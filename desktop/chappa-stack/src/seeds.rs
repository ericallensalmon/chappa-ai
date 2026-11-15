//! Which state seeds a work item asks for, chosen on its overlay.
//!
//! A component's `data.stateSeeds` are named scenarios (argv hooks, some
//! with `{param:x}` placeholders). An item records the ones it wants in
//! `item.json` `dataSeeds`; they are replayed after scrub and seed when
//! the component's data is rebuilt from its recipe, and every saved state
//! records them.
//!
//! | action | where | does |
//! |---|---|---|
//! | `set-seeds` | overlay, when a component of the stack defines state seeds | an endpoint with inputs: a `multi` listing every `<component>/<seed>`, and one `text` input per `{param:x}` of each seed; the call writes `dataSeeds` (nothing runs) |
//!
//! Input names: `seeds` for the choice; `param:<component>/<seed>/<name>`
//! for a parameter. Every value is checked here again (the app checks it
//! against the inputs first): a choice must be one offered, a chosen
//! seed's parameter must be given, printable, at most [`PARAM_CAP`]
//! characters. Parameters only ever fill one argv word; no shell sees them.

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::path::Path;

use stack_core::placeholders::{references, Placeholder};
use stack_core::states::SeedChoice;
use stack_core::workspace::ItemRecord;

use crate::project::Loaded;
use crate::workspaces::{refused, Accepted};

pub const SET_SEEDS_ID: &str = "set-seeds";
/// The `multi` input's name.
pub const SEEDS_INPUT: &str = "seeds";
/// The prefix of a parameter input's name.
pub const PARAM_INPUT_PREFIX: &str = "param:";
/// The longest parameter value kept.
pub const PARAM_CAP: usize = 200;

/// One state seed a component defines, with the parameters its argv uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedDef {
    pub component: String,
    pub name: String,
    pub params: Vec<String>,
}

impl SeedDef {
    /// `<component>/<seed>`: the option value.
    pub fn value(&self) -> String {
        format!("{}/{}", self.component, self.name)
    }

    fn param_input(&self, param: &str) -> String {
        format!("{PARAM_INPUT_PREFIX}{}/{param}", self.value())
    }
}

/// Every state seed of the stack, in definition order.
pub fn seed_defs(loaded: &Loaded) -> Vec<SeedDef> {
    let mut out = Vec::new();
    for (component, def) in &loaded.stack.components {
        let Some(d) = &def.data else {
            continue;
        };
        for (name, argv) in &d.state_seeds {
            let mut params: Vec<String> = Vec::new();
            for word in argv {
                for p in references(word).unwrap_or_default() {
                    if let Placeholder::Param(x) = p {
                        if !params.contains(&x) {
                            params.push(x);
                        }
                    }
                }
            }
            out.push(SeedDef {
                component: component.clone(),
                name: name.clone(),
                params,
            });
        }
    }
    out
}

/// "db: three-categories (count 3)", per chosen seed; "none".
pub fn describe(rec: &ItemRecord) -> String {
    let parts: Vec<String> = rec
        .data_seeds
        .iter()
        .flat_map(|(c, list)| {
            list.iter().map(move |s| {
                if s.params.is_empty() {
                    format!("{c}: {}", s.name)
                } else {
                    format!(
                        "{c}: {} ({})",
                        s.name,
                        s.params
                            .iter()
                            .map(|(k, v)| format!("{k} {v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })
        })
        .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join("; ")
    }
}

/// The `set-seeds` action of `rec`'s overlay, when the stack has any state
/// seeds. Its defaults are what the item asks for now.
pub fn overlay_action(loaded: &Loaded, rec: &ItemRecord) -> Option<Value> {
    let defs = seed_defs(loaded);
    if defs.is_empty() {
        return None;
    }
    let chosen = |d: &SeedDef| {
        rec.data_seeds
            .get(&d.component)
            .and_then(|l| l.iter().find(|s| s.name == d.name))
    };
    let mut inputs = vec![json!({
        "name": SEEDS_INPUT,
        "label": "State seeds",
        "kind": "multi",
        "options": defs
            .iter()
            .map(|d| json!({ "value": d.value(), "label": format!("{}: {}", d.component, d.name) }))
            .collect::<Vec<_>>(),
        "default": defs
            .iter()
            .filter(|d| chosen(d).is_some())
            .map(SeedDef::value)
            .collect::<Vec<_>>(),
    })];
    for d in &defs {
        for p in &d.params {
            let mut input = json!({
                "name": d.param_input(p),
                "label": format!("{}: {} — {p}", d.component, d.name),
                "kind": "text",
                "max": PARAM_CAP,
            });
            if let Some(v) = chosen(d).and_then(|s| s.params.get(p)) {
                input["default"] = Value::String(v.clone());
            }
            inputs.push(input);
        }
    }
    Some(json!({
        "kind": "endpoint",
        "id": SET_SEEDS_ID,
        "label": "State seeds",
        "mutates": true,
        "confirm": format!(
            "Set the state seeds {} asks for (now: {}). They run after scrub and seed whenever a component's data is rebuilt from its recipe (a resume, or an up while the item is active), and every saved state records them. A parameter is needed only for a seed that is chosen.",
            rec.item_ref.key,
            describe(rec)
        ),
        "params": {},
        "inputs": inputs,
    }))
}

fn printable(v: &str) -> bool {
    !v.chars().any(char::is_control)
}

/// Accept (or refuse) `set-seeds`: check the values against the stack's
/// seeds and write them to `item.json`.
pub fn accept(
    loaded: &Loaded,
    dir: &Path,
    rec: &mut ItemRecord,
    params: &Value,
    confirmed: bool,
    now_utc: &str,
) -> Accepted {
    let defs = seed_defs(loaded);
    if defs.is_empty() {
        return refused(409, "not-offered", "this stack defines no state seeds");
    }
    if !confirmed {
        return refused(
            409,
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    let picked: Vec<&str> = match params.get(SEEDS_INPUT) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => {
            let mut out = Vec::new();
            for v in list {
                let Some(s) = v.as_str() else {
                    return refused(400, "bad-input", "`seeds` must be a list of strings");
                };
                out.push(s);
            }
            out
        }
        Some(_) => return refused(400, "bad-input", "`seeds` must be a list of strings"),
    };
    let mut seeds: IndexMap<String, Vec<SeedChoice>> = IndexMap::new();
    for value in picked {
        let Some(d) = defs.iter().find(|d| d.value() == value) else {
            return refused(
                400,
                "bad-input",
                format!("`{value}` is not a state seed of this stack"),
            );
        };
        let list = seeds.entry(d.component.clone()).or_default();
        if list.iter().any(|s| s.name == d.name) {
            continue;
        }
        let mut chosen = SeedChoice {
            name: d.name.clone(),
            params: IndexMap::new(),
        };
        for p in &d.params {
            let raw = params.get(d.param_input(p)).and_then(Value::as_str);
            let v = raw.map(str::trim).unwrap_or("");
            if v.is_empty() {
                return refused(
                    400,
                    "bad-input",
                    format!("{}: {} needs a value for {p}", d.component, d.name),
                );
            }
            if v.chars().count() > PARAM_CAP || !printable(v) {
                return refused(
                    400,
                    "bad-input",
                    format!(
                        "{}: {} — {p} must be printable and at most {PARAM_CAP} characters",
                        d.component, d.name
                    ),
                );
            }
            chosen.params.insert(p.clone(), v.to_string());
        }
        list.push(chosen);
    }
    rec.data_seeds = seeds;
    rec.updated_utc = now_utc.to_string();
    if let Err(e) = rec.save(dir) {
        return refused(500, "workspace", e);
    }
    Accepted::Done(format!(
        "{} asks for these state seeds: {}",
        rec.item_ref.key,
        describe(rec)
    ))
}
