//! The stack verbs as source actions.
//!
//! Every mutating verb is planned when the Environment payload is built,
//! so its `confirm` text is the plan itself ("Pull db (postgres:16);
//! start db, migrate, api"), and an action whose plan has nothing to do is
//! not offered at all. The plan's fingerprint travels in the action's
//! `params`; when the action comes back, the instance is observed again
//! and a different fingerprint means the user confirmed something that is
//! no longer what would happen: the call is refused (409) and the view
//! shows the new plan on its next poll.
//!
//! | key | id | verb |
//! |---|---|---|
//! | none (the stack) | `up` | bring the default selection up |
//! | | `start` | start what exists, recreating what baked in a stale gateway |
//! | | `stop` | stop everything, in stop order, with grace periods |
//! | | `down` | stop and remove every container (volumes kept) |
//! | | `rebuild` | pull or build every image again and recreate |
//! | | `gc` | remove unreferenced images, prune build caches |
//! | component | `start` | bring it (and what it depends on) up |
//! | | `stop` | stop it alone |
//! | | `restart` | `compose restart` |
//! | | `rebuild` | pull or build its image again and recreate it (and what pins it) |
//! | | `mode-prebuilt` / `mode-source` / `mode-dev` | switch its mode |
//! | | `watch` | dev mode with the watch command |
//! | | `logs` | a terminal following its output (not a job) |
//! | | `open-<output>` | a link to one of its URLs |
//! | script component | `up` / `start` / `stop` / `restart` / `rebuild` / `down` | run that script; only the verbs the definition names a script for |
//! | reported row (`<script>/<name>`) | a verb whose argv names `{row}` | run that script with `{row}` = the row's reported name; only on rows the last status report listed |
//!
//! Where the Environment draws a script component's verbs (its row, its
//! group's header, or the stack's own actions) is [`crate::env_view`]'s
//! business; a verb drawn on a group or with the stack is sent back as
//! `<script>:<verb>` ([`script_action_id`]).
//!
//! A plan that runs scripts says so per script: "Run the `stop` script for
//! stack" and what its `plan` script printed (else the resolved argv). Its
//! fingerprint also covers that text, so a plan preview that changed (or
//! arrived) after the confirm was shown refuses the call like a moved
//! stack does, and the next view shows the new one.

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use stack_core::cli::engine_argv;
use stack_core::definition::{DriverKind, Mode};
use stack_core::driver::ResolvedInstance;
use stack_core::planner::{Change, Plan, Selection, Step, Verb};
use stack_core::runner::{Observation, ScriptConfirm, StackRunner};
use stack_core::script::{self, ScriptVerb};

use crate::records::JobWork;

/// One endpoint action as offered right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Offered {
    pub id: String,
    /// The component; `None` for the whole stack.
    pub key: Option<String>,
    pub label: String,
    pub confirm: String,
    pub work: JobWork,
    /// Why the action is offered but cannot be accepted yet (a script's
    /// plan preview still being worked out); the app draws it disabled with
    /// this as its tooltip, and a call to it is refused.
    pub disabled: Option<String>,
}

impl Offered {
    pub fn to_json(&self, fingerprint: &str) -> Value {
        let mut v = json!({
            "kind": "endpoint",
            "id": self.id,
            "label": self.label,
            "mutates": true,
            "confirm": self.confirm,
            "params": { "fingerprint": self.fingerprint_for(fingerprint) },
        });
        if let Some(why) = &self.disabled {
            v["disabled"] = why.clone().into();
        }
        v
    }

    /// The fingerprint a call must bring back, given the state's: the
    /// state's own, or for a plan that runs scripts the state and the
    /// confirm together (a script's plan preview is not part of the state).
    pub fn fingerprint_for(&self, state: &str) -> String {
        let runs_scripts = matches!(&self.work, JobWork::Plan { plan, .. }
            if plan.steps.iter().any(|s| matches!(s, Step::Script { .. })));
        if runs_scripts {
            crate::workspaces::fingerprint(state, &self.confirm)
        } else {
            state.to_string()
        }
    }
}

/// The paragraph each script step of `plan` adds to its confirm, each with
/// whether its plan preview is still pending.
pub fn script_lines(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    plan: &Plan,
) -> Vec<ScriptConfirm> {
    plan.steps
        .iter()
        .filter_map(|s| match s {
            Step::Script {
                component,
                verb,
                row: None,
                rows,
            } if !rows.is_empty() => Some(ScriptConfirm {
                text: runner.script_rows_confirm(inst, obs, component, *verb, rows),
                pending: false,
            }),
            Step::Script {
                component,
                verb,
                row: None,
                ..
            } => Some(runner.script_confirm_of(inst, obs, component, *verb)),
            Step::Script {
                component,
                verb,
                row: Some(row),
                ..
            } => Some(ScriptConfirm {
                text: runner.script_row_confirm(inst, obs, component, *verb, row),
                pending: false,
            }),
            _ => None,
        })
        .collect()
}

/// `Pull db (postgres:16); start db, migrate, api; recreate web (mode
/// prebuilt → source).` plus the plan's notes and what a verb does to data.
pub fn confirm_text(plan: &Plan) -> String {
    let mut groups: IndexMap<&'static str, Vec<String>> = IndexMap::new();
    for step in &plan.steps {
        let item = match (step, step.detail()) {
            (Step::Stop { .. }, _) | (_, None) => step.component().to_string(),
            // The pull's tag template, with the channel this plan uses.
            (_, Some(d)) => match &plan.channel {
                Some(c) => format!("{} ({})", step.component(), d.replace("{channel}", c)),
                None => format!("{} ({d})", step.component()),
            },
        };
        let list = groups.entry(step.verb_label()).or_default();
        if !list.contains(&item) {
            list.push(item);
        }
    }
    let mut parts: Vec<String> = groups
        .into_iter()
        .map(|(verb, items)| format!("{} {}", verb.to_lowercase(), items.join(", ")))
        .collect();
    if let Some(first) = parts.first_mut() {
        *first = capitalize(first);
    }
    let mut text = parts.join("; ");
    if !text.is_empty() {
        text.push('.');
    }
    if plan.verb == Verb::Down && plan.steps.iter().any(|s| matches!(s, Step::Remove { .. })) {
        text.push_str(" Containers are removed; volumes are kept.");
    }
    for note in &plan.notes {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&capitalize(note));
        if !note.ends_with('.') {
            text.push('.');
        }
    }
    text
}

/// Why an action is disabled while any of its scripts' plan previews has
/// not arrived: accepting it would run something nobody has been shown.
fn pending_reason(scripts: &[ScriptConfirm]) -> Option<String> {
    scripts
        .iter()
        .any(|s| s.pending)
        .then(|| script::PLAN_PENDING.to_string())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The channel an `up` resolves `{channel}` with: the one the instance
/// already recorded, else the configured default.
pub fn channel_for(obs: &Observation, default: &str) -> String {
    obs.intent
        .components
        .values()
        .find_map(|c| c.channel.clone())
        .unwrap_or_else(|| default.to_string())
}

fn planned(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    id: &str,
    key: Option<&str>,
    label: &str,
    change: Change,
) -> Option<Offered> {
    let plan = runner.plan(obs, &change).ok()?;
    let records_only = plan.steps.is_empty() && !plan.desired.is_empty();
    if plan.steps.is_empty() && !records_only {
        return None;
    }
    let scripts = script_lines(runner, inst, obs, &plan);
    let disabled = pending_reason(&scripts);
    Some(Offered {
        id: id.to_string(),
        key: key.map(str::to_string),
        label: label.to_string(),
        confirm: {
            // A plan without its own channel pulls with the one each
            // component recorded; show that one.
            let mut shown = plan.clone();
            if shown.channel.is_none() {
                shown.channel = obs
                    .intent
                    .components
                    .get(key.unwrap_or_default())
                    .and_then(|c| c.channel.clone())
                    .or_else(|| {
                        obs.intent
                            .components
                            .values()
                            .find_map(|c| c.channel.clone())
                    });
            }
            let mut text = confirm_text(&shown);
            for s in scripts {
                text.push_str("\n\n");
                text.push_str(&s.text);
            }
            text
        },
        work: JobWork::Plan {
            change,
            plan: Box::new(plan),
            save: None,
            remove_volumes: false,
        },
        disabled,
    })
}

/// The stack-wide actions.
pub fn stack_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    channel: &str,
) -> Vec<Offered> {
    let mut out = Vec::new();
    let up = Change::Up {
        selection: Selection::Default,
        modes: IndexMap::new(),
        channel: Some(channel.to_string()),
    };
    out.extend(planned(runner, inst, obs, "up", None, "Bring up", up));
    out.extend(planned(
        runner,
        inst,
        obs,
        "start",
        None,
        "Start (after a reboot)",
        StackRunner::start_change(obs),
    ));
    out.extend(planned(
        runner,
        inst,
        obs,
        "stop",
        None,
        "Stop",
        Change::Stop {
            keep_groups: Vec::new(),
        },
    ));
    out.extend(planned(
        runner,
        inst,
        obs,
        "down",
        None,
        "Down",
        Change::Down {
            keep_groups: Vec::new(),
        },
    ));
    out.extend(planned(
        runner,
        inst,
        obs,
        "rebuild",
        None,
        "Rebuild all",
        Change::RebuildAll,
    ));
    let engines: Vec<String> = inst
        .engines()
        .into_iter()
        .filter(|e| e.kind.is_container_engine())
        .map(|e| e.name)
        .collect();
    if !engines.is_empty() {
        out.push(Offered {
            id: "gc".into(),
            key: None,
            label: "Clean up images".into(),
            confirm: format!(
                "Remove images that no running instance, kept state or pinned channel uses, and prune the build cache on {}. The job lists every image before it removes anything.",
                engines.join(", ")
            ),
            work: JobWork::Gc,
            disabled: None,
        });
    }
    out
}

/// The current mode of a component: what the observation says, else the
/// definition's default.
pub fn mode_now(runner: &StackRunner<'_>, obs: &Observation, component: &str) -> Option<Mode> {
    obs.state
        .components
        .get(component)
        .and_then(|c| c.mode)
        .or_else(|| {
            runner
                .stack
                .components
                .get(component)
                .and_then(|c| c.default_mode())
        })
}

/// The endpoint actions on one component.
pub fn component_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    channel: &str,
) -> Vec<Offered> {
    let mut out = Vec::new();
    let Some(def) = runner.stack.components.get(component) else {
        return out;
    };
    if def.driver == DriverKind::Script {
        return script_actions(runner, inst, obs, component);
    }
    let one = vec![component.to_string()];
    out.extend(planned(
        runner,
        inst,
        obs,
        "start",
        Some(component),
        "Start",
        Change::Up {
            selection: Selection::Components(one.clone()),
            modes: IndexMap::new(),
            channel: Some(channel.to_string()),
        },
    ));
    out.extend(planned(
        runner,
        inst,
        obs,
        "stop",
        Some(component),
        "Stop",
        Change::StopComponents { components: one },
    ));
    let current = mode_now(runner, obs, component);
    let running = obs.state.component(component).running;
    let as_process = StackRunner::runs_as_process(inst, component, current);
    if running && !as_process && def.driver == DriverKind::Compose {
        let grace = runner.stack.grace_seconds(component);
        out.push(Offered {
            id: "restart".into(),
            key: Some(component.to_string()),
            label: "Restart".into(),
            confirm: format!("Restart {component} (stop with a {grace} s grace, then start)."),
            work: JobWork::Restart {
                component: component.to_string(),
            },
            disabled: None,
        });
    }
    if matches!(current, Some(Mode::Prebuilt | Mode::Source))
        && obs.state.component(component).exists
    {
        out.extend(planned(
            runner,
            inst,
            obs,
            "rebuild",
            Some(component),
            "Rebuild",
            Change::Rebuild {
                component: component.to_string(),
            },
        ));
    }
    if def.modes.len() > 1 {
        for mode in [Mode::Prebuilt, Mode::Source, Mode::Dev] {
            if !def.has_mode(mode) || Some(mode) == current {
                continue;
            }
            out.extend(planned(
                runner,
                inst,
                obs,
                &format!("mode-{mode}"),
                Some(component),
                &format!("Switch to {mode}"),
                Change::SwitchMode {
                    component: component.to_string(),
                    mode,
                    git_ref: None,
                },
            ));
        }
    }
    if def.has_mode(Mode::Dev) && def.watch_command().is_some() {
        out.extend(planned(
            runner,
            inst,
            obs,
            "watch",
            Some(component),
            "Watch",
            Change::Watch {
                component: component.to_string(),
                git_ref: None,
            },
        ));
    }
    out
}

/// A script component's verbs: one action per script the definition
/// names, its confirm the script's own (see [`script_lines`]).
fn script_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
) -> Vec<Offered> {
    let mut out = Vec::new();
    let def = &runner.stack.components[component];
    for verb in ScriptVerb::MUTATING {
        if !script::has(def, verb) {
            continue;
        }
        if script::takes_row(def, verb) {
            out.extend(row_actions(runner, inst, obs, component, verb));
            continue;
        }
        if verb == ScriptVerb::Up && script::takes_rows(def, verb) {
            out.extend(row_ups(runner, inst, obs, component));
        }
        let change = Change::Script {
            component: component.to_string(),
            verb,
            row: None,
            rows: Vec::new(),
        };
        let Ok(plan) = runner.plan(obs, &change) else {
            continue;
        };
        let scripts = script_lines(runner, inst, obs, &plan);
        let disabled = pending_reason(&scripts);
        let mut confirm = scripts
            .into_iter()
            .map(|s| s.text)
            .collect::<Vec<_>>()
            .join("\n\n");
        for note in &plan.notes {
            confirm.push(' ');
            confirm.push_str(&capitalize(note));
        }
        out.push(Offered {
            id: verb.as_str().to_string(),
            key: Some(component.to_string()),
            label: script_label(verb).to_string(),
            confirm,
            work: JobWork::Plan {
                change,
                plan: Box::new(plan),
                save: None,
                remove_volumes: false,
            },
            disabled,
        });
    }
    out
}

/// A per-row verb on every row the last status report listed (a row the
/// script did not report, or one only assumed from the intent, gets
/// none): `{row}` is the row's reported name.
fn row_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    verb: ScriptVerb,
) -> Vec<Offered> {
    let Some(view) = obs
        .scripts
        .get(component)
        .filter(|v| v.from == script::ViewFrom::Status)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in &view.rows {
        if script::row_name_problem(&row.name).is_some() {
            continue;
        }
        let change = Change::Script {
            component: component.to_string(),
            verb,
            row: Some(row.name.clone()),
            rows: Vec::new(),
        };
        let Ok(plan) = runner.plan(obs, &change) else {
            continue;
        };
        out.push(Offered {
            id: verb.as_str().to_string(),
            key: Some(row.id.clone()),
            label: script_label(verb).to_string(),
            confirm: runner.script_row_confirm(inst, obs, component, verb, &row.name),
            work: JobWork::Plan {
                change,
                plan: Box::new(plan),
                save: None,
                remove_volumes: false,
            },
            disabled: None,
        });
    }
    out
}

/// "Up this" on every reported row that is not running, for a script whose
/// `up` takes `{rows}`: its `up` with just that row's name.
fn row_ups(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
) -> Vec<Offered> {
    let Some(view) = obs
        .scripts
        .get(component)
        .filter(|v| v.from == script::ViewFrom::Status && v.multi)
    else {
        return Vec::new();
    };
    view.rows
        .iter()
        .filter(|r| !r.state.is_running() && script::row_name_problem(&r.name).is_none())
        .filter_map(|r| {
            planned(
                runner,
                inst,
                obs,
                "up",
                Some(&r.id),
                "Up this",
                Change::Script {
                    component: component.to_string(),
                    verb: ScriptVerb::Up,
                    row: None,
                    rows: vec![r.name.clone()],
                },
            )
        })
        .collect()
}

/// The name of the picker input that chooses how the stack comes up, and
/// of the one that holds what was ticked.
pub const SELECTION_INPUT: &str = "selection";
pub const PICK_INPUT: &str = "pick";
/// What a script's Up sends the ticked rows (and groups) in.
pub const ROWS_INPUT: &str = "rows";
/// A ticked group, in a pick list.
const GROUP_PREFIX: &str = "group:";
/// A named selection, as a value of the selection input.
const NAMED_PREFIX: &str = "named:";
/// Most options one picker list carries (the app refuses more).
const PICK_OPTIONS_MAX: usize = 200;

fn option(value: impl Into<String>, label: impl Into<String>) -> Value {
    json!({ "value": value.into(), "label": label.into() })
}

/// The picker on the stack's Bring up: the default selection (everything
/// not normally off), each named selection of the definition, or what is
/// ticked among the definition's groups and components.
pub fn up_inputs(stack: &stack_core::definition::StackDef) -> Value {
    let mut choices = vec![option("default", "Default: everything not normally off")];
    for (name, members) in &stack.selections {
        choices.push(option(
            format!("{NAMED_PREFIX}{name}"),
            format!("{name}: {}", members.join(", ")),
        ));
    }
    choices.push(option("picked", "Only what is ticked below"));
    let mut picks: Vec<Value> = Vec::new();
    for (id, g) in &stack.groups {
        if stack
            .components
            .values()
            .any(|c| c.group.as_deref() == Some(id.as_str()))
        {
            let label = g.label.clone().unwrap_or_else(|| id.clone());
            picks.push(option(
                format!("{GROUP_PREFIX}{id}"),
                format!("{label} (group)"),
            ));
        }
    }
    for (id, c) in &stack.components {
        let label = c.label.clone().unwrap_or_else(|| id.clone());
        let off = if c.normally_off {
            " (normally off)"
        } else {
            ""
        };
        picks.push(option(id.clone(), format!("{label}{off}")));
    }
    picks.truncate(PICK_OPTIONS_MAX);
    json!([
        { "name": SELECTION_INPUT, "label": "Bring up", "kind": "choice", "options": choices, "required": true, "default": "default" },
        { "name": PICK_INPUT, "label": "Ticked", "kind": "multi", "options": picks },
    ])
}

/// The picker on a script's Up whose argv takes `{rows}`: the reported
/// rows and the groups that hold them; nothing ticked = everything.
pub fn rows_inputs(
    view: &script::ScriptView,
    group_of: &dyn Fn(&script::ScriptRow) -> String,
) -> Value {
    let mut picks: Vec<Value> = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    for r in &view.rows {
        let g = group_of(r);
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    if groups.len() > 1 {
        for g in &groups {
            let label = view
                .groups
                .iter()
                .find(|x| &x.id == g)
                .map(|x| x.label.clone())
                .unwrap_or_else(|| g.clone());
            picks.push(option(
                format!("{GROUP_PREFIX}{g}"),
                format!("{label} (group)"),
            ));
        }
    }
    for r in &view.rows {
        picks.push(option(r.id.clone(), r.name.clone()));
    }
    picks.truncate(PICK_OPTIONS_MAX);
    json!([
        { "name": ROWS_INPUT, "label": "Only these (nothing ticked brings up everything)", "kind": "multi", "options": picks },
    ])
}

fn strings(v: Option<&Value>) -> Result<Vec<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "the ticked entries must be text".to_string())
            })
            .collect(),
        Some(_) => Err("the ticked entries must be a list".into()),
    }
}

/// What the stack's Bring up was asked to bring up: `None` for the default
/// selection. Checked against the definition: a named selection must
/// exist, and every ticked component or group must be one of its own.
pub fn picked_selection(
    stack: &stack_core::definition::StackDef,
    params: &Value,
) -> Result<Option<Selection>, String> {
    let choice = params
        .get(SELECTION_INPUT)
        .and_then(Value::as_str)
        .unwrap_or("default");
    if choice == "default" {
        return Ok(None);
    }
    if let Some(name) = choice.strip_prefix(NAMED_PREFIX) {
        return if stack.selections.contains_key(name) {
            Ok(Some(Selection::Named(name.to_string())))
        } else {
            Err(format!("the definition has no selection `{name}`"))
        };
    }
    if choice != "picked" {
        return Err(format!("`{choice}` is not a way to bring the stack up"));
    }
    let ticked = strings(params.get(PICK_INPUT))?;
    if ticked.is_empty() {
        return Err("nothing is ticked".into());
    }
    let mut wanted: Vec<&str> = Vec::new();
    for t in &ticked {
        if let Some(g) = t.strip_prefix(GROUP_PREFIX) {
            let members: Vec<&str> = stack
                .components
                .iter()
                .filter(|(_, c)| c.group.as_deref() == Some(g))
                .map(|(n, _)| n.as_str())
                .collect();
            if members.is_empty() {
                return Err(format!("the definition has no group `{g}`"));
            }
            wanted.extend(members);
        } else if stack.components.contains_key(t) {
            wanted.push(t);
        } else {
            return Err(format!("the definition has no component `{t}`"));
        }
    }
    // In the definition's order, each once.
    let list: Vec<String> = stack
        .components
        .keys()
        .filter(|n| wanted.contains(&n.as_str()))
        .cloned()
        .collect();
    Ok(Some(Selection::Components(list)))
}

/// The rows a script's Up was asked to bring up, as the names its `{rows}`
/// gets, in the order the status reported them: empty for everything.
/// Each ticked row must be one the last status report listed, and each
/// ticked group one that holds such rows.
pub fn picked_rows(
    view: &script::ScriptView,
    group_of: &dyn Fn(&script::ScriptRow) -> String,
    params: &Value,
) -> Result<Vec<String>, String> {
    let ticked = strings(params.get(ROWS_INPUT))?;
    if ticked.is_empty() {
        return Ok(Vec::new());
    }
    if view.from != script::ViewFrom::Status {
        return Err("its status has not reported any rows yet".into());
    }
    let mut wanted: Vec<&str> = Vec::new();
    for t in &ticked {
        if let Some(g) = t.strip_prefix(GROUP_PREFIX) {
            let members: Vec<&str> = view
                .rows
                .iter()
                .filter(|r| group_of(r) == g)
                .map(|r| r.id.as_str())
                .collect();
            if members.is_empty() {
                return Err(format!("no reported row is in group `{g}`"));
            }
            wanted.extend(members);
        } else if view.rows.iter().any(|r| &r.id == t) {
            wanted.push(t);
        } else {
            return Err(format!("`{t}` is not a row its status reported"));
        }
    }
    Ok(view
        .rows
        .iter()
        .filter(|r| wanted.contains(&r.id.as_str()))
        .map(|r| r.name.clone())
        .collect())
}

/// `chosen` as asked for by its params: the stack's Bring up with a picked
/// selection, or a script's Up with picked rows. `Ok(None)` when the
/// params ask for nothing but what was offered.
pub fn with_picks(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    chosen: &Offered,
    params: &Value,
    group_of: &dyn Fn(&str, &script::ScriptRow) -> String,
    channel: &str,
) -> Result<Option<Offered>, String> {
    let change = match chosen.key.as_deref() {
        None if chosen.id == "up" => match picked_selection(runner.stack, params)? {
            None => return Ok(None),
            Some(selection) => Change::Up {
                selection,
                modes: IndexMap::new(),
                channel: Some(channel.to_string()),
            },
        },
        Some(c)
            if chosen.id == "up"
                && runner.stack.components.get(c).is_some_and(|d| {
                    script::has(d, ScriptVerb::Up) && script::takes_rows(d, ScriptVerb::Up)
                }) =>
        {
            if params.get(ROWS_INPUT).is_none() {
                return Ok(None);
            }
            let Some(view) = obs.scripts.get(c) else {
                return Err("its status has not reported any rows yet".into());
            };
            let rows = picked_rows(view, &|r| group_of(c, r), params)?;
            if rows.is_empty() {
                return Ok(None);
            }
            Change::Script {
                component: c.to_string(),
                verb: ScriptVerb::Up,
                row: None,
                rows,
            }
        }
        _ => return Ok(None),
    };
    planned(
        runner,
        inst,
        obs,
        &chosen.id,
        chosen.key.as_deref(),
        &chosen.label,
        change,
    )
    .map(Some)
    .ok_or_else(|| "there is nothing to bring up in what was picked".to_string())
}

/// The id a script component's verb is sent back with when it is drawn
/// once for the whole component (on its group's header or with the
/// stack's actions) rather than on its row: `<component>:<verb>`.
/// Component names never contain `:`.
pub fn script_action_id(component: &str, verb: &str) -> String {
    format!("{component}:{verb}")
}

/// The component and verb of a [`script_action_id`], if `id` is one.
pub fn parse_script_action_id(id: &str) -> Option<(&str, &str)> {
    let (component, verb) = id.split_once(':')?;
    (!component.is_empty() && ScriptVerb::parse(verb).is_some()).then_some((component, verb))
}

fn script_label(verb: ScriptVerb) -> &'static str {
    match verb {
        ScriptVerb::Up => "Up",
        ScriptVerb::Start => "Start",
        ScriptVerb::Stop => "Stop",
        ScriptVerb::Restart => "Restart",
        ScriptVerb::Rebuild => "Rebuild",
        ScriptVerb::Down => "Down",
        ScriptVerb::Status => "Status",
        ScriptVerb::Plan => "Plan",
    }
}

/// Every endpoint action offered now, the stack's first.
pub fn all_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    channel: &str,
) -> Vec<Offered> {
    let mut out = stack_actions(runner, inst, obs, channel);
    for name in inst.components.keys() {
        out.extend(component_actions(runner, inst, obs, name, channel));
    }
    out
}

/// The flag `follow` and `follow-command` take the work folder in: the
/// values generated for its instances are shown as `••••` in what they
/// print.
pub const HIDE_GENERATED_ARG: &str = "--hide-generated";

/// The `logs` terminal action: `<engine> logs -f <container>` for a
/// container, or this program following a process component's log file.
///
/// When the instance has generated secrets (`work_dir` names where they
/// live), a component's output may print them (a connection string at
/// startup), so the engine's command runs under this program's
/// `follow-command`, and `follow` is told the same folder: both show each
/// generated value as `••••`, like every other view.
pub fn logs_action(
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    project_dir: &Path,
    process_log: Option<&PathBuf>,
    exe: &Path,
    work_dir: &Path,
) -> Option<Value> {
    let c = inst.components.get(component)?;
    let hide = !inst.secrets.is_empty();
    let hide_args = || {
        vec![
            HIDE_GENERATED_ARG.to_string(),
            work_dir.display().to_string(),
        ]
    };
    let command: Vec<String> = match (process_log, obs.ps.get(component)) {
        (Some(log), _) => {
            let mut argv = vec![
                exe.display().to_string(),
                "follow".into(),
                log.display().to_string(),
            ];
            if hide {
                argv.extend(hide_args());
            }
            argv
        }
        (None, Some(ps)) if !ps.name.is_empty() => {
            let mut engine = engine_argv(&c.engine);
            engine.extend(["logs".to_string(), "-f".to_string(), ps.name.clone()]);
            if hide {
                let mut argv = vec![exe.display().to_string(), "follow-command".into()];
                argv.extend(hide_args());
                argv.push("--".into());
                argv.extend(engine);
                argv
            } else {
                engine
            }
        }
        _ => return None,
    };
    Some(json!({
        "kind": "terminal",
        "id": "logs",
        "label": "Logs",
        "cwd": project_dir.display().to_string(),
        "command": command,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stack_core::planner::RecreateReason;

    #[test]
    fn confirm_text_names_each_step_kind_once_in_order() {
        let mut plan = Plan::empty("app-0", Verb::Up, "f");
        plan.steps = vec![
            Step::Pull {
                component: "db".into(),
                image: "postgres".into(),
                tag: Some("16".into()),
            },
            Step::Start {
                component: "api".into(),
            },
            Step::Start {
                component: "db".into(),
            },
            Step::Recreate {
                component: "web".into(),
                reason: RecreateReason::Rebuilt,
            },
        ];
        assert_eq!(
            confirm_text(&plan),
            "Pull db (postgres:16); start api, db; recreate web (rebuilt)."
        );
        let mut down = Plan::empty("app-0", Verb::Down, "f");
        down.steps = vec![
            Step::Stop {
                component: "db".into(),
                grace_seconds: 30,
            },
            Step::Remove {
                component: "db".into(),
            },
        ];
        down.notes.push("api was already stopped".into());
        assert_eq!(
            confirm_text(&down),
            "Stop db; remove db. Containers are removed; volumes are kept. Api was already stopped."
        );
    }
}
