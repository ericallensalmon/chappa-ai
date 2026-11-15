//! The actions a source attaches to its data (`actions?: Action[]` on an
//! item, a component, an attention item, a section, a banner, a panel or
//! the payload itself), parsed out of the cached JSON and judged before a
//! view may draw a button for them.
//!
//! Five kinds, and the app enforces what each may do:
//! - `agent`: launch an agent tool in a folder with a prompt — the tool is
//!   resolved by NAME against the configured tools, the folder must pass
//!   the cwd roots, env names must be plain identifiers, and the prompt is
//!   shown to the user before the first run;
//! - `terminal`: a shell or an argv in a folder, same folder and env rules;
//! - `endpoint`: a POST back to the source; one that `mutates` without a
//!   `confirm` text is not rendered at all (and logged once per source);
//! - `link`: opens `https://` on a trusted host, or plain `http://` when that
//!   trusted host is this machine (`links.rs`);
//! - `panel`: a drill-in page fetched from a path the payload named.
//!
//! Nothing here performs an action. [`action_verdict`] answers "may the
//! button be enabled, and if not, why" as data; the runner in `sources.rs`
//! applies the same rules again before acting.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::agent_tools::AgentTool;
use crate::roots::{check_cwd, Root, RootsVerdict};

use super::links::source_link_shape_check;
use super::manifest::validate_endpoint_path;

/// Whether an endpoint action wants a note from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteRule {
    Optional,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceAction {
    Agent {
        id: String,
        label: String,
        cwd: String,
        prompt: String,
        /// Agent tool NAME; absent = the first enabled tool.
        tool: Option<String>,
        title: Option<String>,
        env: BTreeMap<String, String>,
    },
    Terminal {
        id: String,
        label: String,
        cwd: String,
        /// argv, never a shell string; empty = the default shell.
        command: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Endpoint {
        id: String,
        label: String,
        mutates: bool,
        confirm: Option<String>,
        params: Option<Value>,
        note: Option<NoteRule>,
    },
    Link {
        id: String,
        label: String,
        url: String,
    },
    Panel {
        id: String,
        label: String,
        endpoint: String,
    },
}

impl SourceAction {
    pub fn id(&self) -> &str {
        match self {
            SourceAction::Agent { id, .. }
            | SourceAction::Terminal { id, .. }
            | SourceAction::Endpoint { id, .. }
            | SourceAction::Link { id, .. }
            | SourceAction::Panel { id, .. } => id,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            SourceAction::Agent { label, .. }
            | SourceAction::Terminal { label, .. }
            | SourceAction::Endpoint { label, .. }
            | SourceAction::Link { label, .. }
            | SourceAction::Panel { label, .. } => label,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            SourceAction::Agent { .. } => "agent",
            SourceAction::Terminal { .. } => "terminal",
            SourceAction::Endpoint { .. } => "endpoint",
            SourceAction::Link { .. } => "link",
            SourceAction::Panel { .. } => "panel",
        }
    }

    /// An `endpoint` that changes something but carries no confirm text.
    /// Such an action is never rendered: the user would have nothing to
    /// accept, and `confirmed: true` is only ever set by the dialog.
    pub fn is_unconfirmable_mutation(&self) -> bool {
        matches!(self, SourceAction::Endpoint { mutates: true, confirm: None, .. })
    }
}

/// Env names a source may set on a launched agent or terminal:
/// `[A-Z_][A-Z0-9_]*`. Anything else (lowercase, `=`, spaces, an empty
/// name) is refused with the offending name.
pub fn validate_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    for name in env.keys() {
        let mut chars = name.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_');
        if !head_ok || !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
            return Err(format!("env name `{name}` is not of the form [A-Z_][A-Z0-9_]*"));
        }
    }
    Ok(())
}

fn text(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

fn optional_text(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn env_of(obj: &serde_json::Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    match obj.get("env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (k, v) in map {
                let value = v.as_str().ok_or_else(|| format!("env `{k}` must be a string"))?;
                env.insert(k.clone(), value.to_owned());
            }
        }
        Some(_) => return Err("`env` must be an object of strings".into()),
    }
    validate_env(&env)?;
    Ok(env)
}

/// Parse one `Action` object. The error names the field, for the log.
pub fn parse_action(value: &Value) -> Result<SourceAction, String> {
    let obj = value.as_object().ok_or("an action must be an object")?;
    let kind = text(obj, "kind")?;
    let id = text(obj, "id")?;
    let label = text(obj, "label")?;
    match kind.as_str() {
        "agent" => Ok(SourceAction::Agent {
            id,
            label,
            cwd: text(obj, "cwd")?,
            prompt: text(obj, "prompt")?,
            tool: optional_text(obj, "tool")?,
            title: optional_text(obj, "title")?,
            env: env_of(obj)?,
        }),
        "terminal" => {
            let command = match obj.get("command") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(words)) => words
                    .iter()
                    .map(|w| w.as_str().map(str::to_owned).ok_or_else(|| "`command` must be a list of strings".to_owned()))
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err("`command` must be a list of strings (argv), never a shell line".into()),
            };
            Ok(SourceAction::Terminal { id, label, cwd: text(obj, "cwd")?, command, env: env_of(obj)? })
        }
        "endpoint" => {
            let mutates = obj.get("mutates").and_then(Value::as_bool).ok_or("`mutates` must be a boolean")?;
            let note = match obj.get("note") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s == "optional" => Some(NoteRule::Optional),
                Some(Value::String(s)) if s == "required" => Some(NoteRule::Required),
                Some(_) => return Err("`note` must be \"optional\" or \"required\"".into()),
            };
            let params = match obj.get("params") {
                None | Some(Value::Null) => None,
                Some(p @ Value::Object(_)) => Some(p.clone()),
                Some(_) => return Err("`params` must be an object".into()),
            };
            Ok(SourceAction::Endpoint { id, label, mutates, confirm: optional_text(obj, "confirm")?, params, note })
        }
        "link" => Ok(SourceAction::Link { id, label, url: text(obj, "url")? }),
        "panel" => Ok(SourceAction::Panel { id, label, endpoint: text(obj, "endpoint")? }),
        other => Err(format!("unknown action kind `{other}`")),
    }
}

/// One action found in a payload, with the key of the OUTERMOST keyed
/// object it sits under: the work item's `key`, a component's or
/// attention item's `id`, a pull request's `forge:repo:number`. `None`
/// for the payload's own actions (a panel's, a response's), and for the
/// actions of the payload's own banners and sections: a banner or section
/// is part of whatever holds it, never an item of its own (see
/// [`PART_LISTS`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundAction {
    pub key: Option<String>,
    pub action: SourceAction,
    /// Where it was: `items[2].sections[0].actions[1]`, for the log.
    pub path: String,
}

/// Every action anywhere in `value`. Unparseable entries are skipped and
/// reported in the second list (path + reason), so a source's one broken
/// action does not hide its others.
pub fn actions_in(value: &Value) -> (Vec<FoundAction>, Vec<(String, String)>) {
    let mut found = Vec::new();
    let mut broken = Vec::new();
    walk(value, None, "", &mut found, &mut broken);
    (found, broken)
}

/// The key an item's actions are addressed by: its `key` or `id`. A pull
/// request has neither, and its number repeats across repositories (two
/// repos of one source both have a `#12`), so it is keyed by its whole
/// identity, `forge:repo:number`, which is also what a view dedupes it by.
/// A bare `number` is the last resort. A work entry that adds to another
/// source's item and names no key of its own is that item's (`ref.key`).
fn item_key(obj: &serde_json::Map<String, Value>) -> Option<String> {
    let text = |k: &str| obj.get(k).and_then(Value::as_str);
    if let Some(k) = text("key").or_else(|| text("id")) {
        return Some(k.to_owned());
    }
    if let Some(k) = obj.get("ref").and_then(|r| r.get("key")).and_then(Value::as_str) {
        return Some(k.to_owned());
    }
    if let (Some(forge), Some(repo), Some(number)) = (text("forge"), text("repo"), text("number")) {
        return Some(pr_key(forge, repo, number));
    }
    text("number").map(str::to_owned)
}

/// The action key of a pull request: `forge:repo:number`, the fields as
/// the source sent them.
pub fn pr_key(forge: &str, repo: &str, number: &str) -> String {
    format!("{forge}:{repo}:{number}")
}

/// Lists whose entries are parts of their holder, not items: their `id`
/// names the part (for fold state), so it must not become an action key.
/// A view's banner is addressed as the view itself, a work item's section
/// as that item, a panel's section as the panel.
pub const PART_LISTS: [&str; 3] = ["banners", "sections", "facts"];

fn walk(value: &Value, key: Option<&str>, path: &str, found: &mut Vec<FoundAction>, broken: &mut Vec<(String, String)>) {
    walk_in(value, key, path, false, found, broken);
}

/// `part`: `value` sits in one of the [`PART_LISTS`], so it cannot own a key.
fn walk_in(value: &Value, key: Option<&str>, path: &str, part: bool, found: &mut Vec<FoundAction>, broken: &mut Vec<(String, String)>) {
    match value {
        Value::Object(obj) => {
            // The outermost keyed object on the path owns what is below it.
            let own_key = key.map(str::to_owned).or_else(|| if path.is_empty() || part { None } else { item_key(obj) });
            for (k, v) in obj {
                let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                if k == "actions" {
                    if let Value::Array(list) = v {
                        for (i, raw) in list.iter().enumerate() {
                            let at = format!("{child_path}[{i}]");
                            match parse_action(raw) {
                                Ok(action) => found.push(FoundAction { key: own_key.clone(), action, path: at }),
                                Err(why) => broken.push((at, why)),
                            }
                        }
                    } else {
                        broken.push((child_path, "`actions` must be a list".into()));
                    }
                    continue;
                }
                walk_in(v, own_key.as_deref(), &child_path, PART_LISTS.contains(&k.as_str()), found, broken);
            }
        }
        Value::Array(list) => {
            for (i, v) in list.iter().enumerate() {
                walk_in(v, key, &format!("{path}[{i}]"), part, found, broken);
            }
        }
        _ => {}
    }
}

/// The actions under `key` (`None` = the payload's own).
pub fn actions_for(value: &Value, key: Option<&str>) -> Vec<FoundAction> {
    actions_in(value).0.into_iter().filter(|f| f.key.as_deref() == key).collect()
}

/// The action `action_id` under `key`, if the payload has one.
pub fn find_action(value: &Value, key: Option<&str>, action_id: &str) -> Option<FoundAction> {
    actions_for(value, key).into_iter().find(|f| f.action.id() == action_id)
}

/// Where a work entry that adds to another source's item points:
/// `ref: {source, key}`, the owning source's manifest id and the item's key
/// there. `None` for an entry the source owns itself.
pub fn overlay_ref(item: &Value) -> Option<(&str, &str)> {
    let r = item.get("ref")?;
    let source = r.get("source")?.as_str().filter(|s| !s.trim().is_empty())?;
    let key = r.get("key")?.as_str().filter(|s| !s.trim().is_empty())?;
    Some((source, key))
}

/// The entries of a list payload.
fn items(payload: &Value) -> &[Value] {
    payload.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// The first `items` entry of a work payload addressed by `key` (the key
/// its actions are filed under, see [`actions_in`]).
pub fn work_entry<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    items(payload).iter().find(|item| item.as_object().and_then(item_key).as_deref() == Some(key))
}

/// The row a source owns under `key` in its `view` payload, exactly as it
/// sent it: a work item that adds to nobody else's, or the pull request
/// whose `forge:repo:number` is `key`.
pub fn owned_row<'a>(payload: &'a Value, view: &str, key: &str) -> Option<&'a Value> {
    items(payload).iter().find(|item| {
        let Some(obj) = item.as_object() else { return false };
        match view {
            "work" => overlay_ref(item).is_none() && obj.get("key").and_then(Value::as_str) == Some(key),
            "prs" => {
                let text = |k: &str| obj.get(k).and_then(Value::as_str);
                matches!((text("forge"), text("repo"), text("number")), (Some(f), Some(r), Some(n)) if pr_key(f, r, n) == key)
            }
            _ => false,
        }
    })
}

/// Whether a work payload carries an entry adding to `key` of the source
/// whose manifest id is `owner`.
pub fn has_overlay(payload: &Value, owner: &str, key: &str) -> bool {
    items(payload).iter().any(|item| overlay_ref(item) == Some((owner, key)))
}

/// How much of a row's title a filled-in confirm text carries.
pub const CONFIRM_TITLE_MAX: usize = 80;

/// A title as one line (control characters and runs of whitespace become
/// one space), cut to `max` characters with a trailing `…` when longer.
pub fn one_line_title(title: &str, max: usize) -> String {
    let flat = title.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// An item action's confirm text with `{key}`, `{title}` and `{source}`
/// filled from the row it runs on, in one pass: text a placeholder brings
/// in is never read again, so a title cannot smuggle in another
/// placeholder. The title is one line of at most [`CONFIRM_TITLE_MAX`]
/// characters; an item without one reads "(no title)".
pub fn fill_confirm(template: &str, key: &str, title: Option<&str>, source: &str) -> String {
    let title = title.map(|t| one_line_title(t, CONFIRM_TITLE_MAX)).filter(|t| !t.is_empty()).unwrap_or_else(|| "(no title)".to_owned());
    let mut out = String::with_capacity(template.len() + title.len());
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let filled = [("{key}", key), ("{title}", title.as_str()), ("{source}", source)]
            .into_iter()
            .find(|(slot, _)| tail.starts_with(slot));
        match filled {
            Some((slot, value)) => {
                out.push_str(value);
                rest = &tail[slot.len()..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What the verdict judges against: the configured agent tools, the cwd
/// roots in force, and the source's trusted link hosts.
pub struct VerdictContext<'a> {
    pub tools: &'a [AgentTool],
    pub roots: &'a [Root],
    pub link_hosts: &'a [String],
}

/// The judgement on one action, as the button needs it: enabled or not,
/// the reason when not, and what the runner resolved (so the dialog can
/// show the real tool and folder).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionVerdict {
    pub enabled: bool,
    /// Why the button is disabled; shown as its tooltip.
    pub reason: Option<String>,
    /// Not drawn at all (a mutating endpoint without a confirm).
    pub hidden: bool,
    /// The resolved working directory of an agent / terminal action.
    pub cwd: Option<PathBuf>,
    /// The resolved agent tool `(id, name)` of an agent action.
    pub tool: Option<(u32, String)>,
}

impl ActionVerdict {
    fn ok() -> Self {
        Self { enabled: true, reason: None, hidden: false, cwd: None, tool: None }
    }

    fn disabled(reason: String) -> Self {
        Self { enabled: false, reason: Some(reason), hidden: false, cwd: None, tool: None }
    }
}

/// The agent tool an `agent` action names: by name among the configured
/// tools, or the first enabled one when it names none. Disabled tools and
/// unknown names are errors with the fix in the text.
pub fn resolve_tool<'a>(tools: &'a [AgentTool], name: Option<&str>) -> Result<&'a AgentTool, String> {
    match name {
        Some(name) => {
            let tool = tools
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| format!("no agent tool is named `{name}`; add one under Settings ▸ Agents or rename an existing one"))?;
            if !tool.enabled {
                return Err(format!("the agent tool `{}` is disabled", tool.name));
            }
            Ok(tool)
        }
        None => tools
            .iter()
            .find(|t| t.enabled)
            .ok_or_else(|| "no agent tool is enabled; add one under Settings ▸ Agents".to_owned()),
    }
}

/// Judge `action` against `ctx`.
pub fn action_verdict(action: &SourceAction, ctx: &VerdictContext<'_>) -> ActionVerdict {
    match action {
        SourceAction::Agent { cwd, tool, env, prompt, .. } => {
            if prompt.trim().is_empty() {
                return ActionVerdict::disabled("the action carries no prompt".into());
            }
            let tool = match resolve_tool(ctx.tools, tool.as_deref()) {
                Ok(t) => t,
                Err(why) => return ActionVerdict::disabled(why),
            };
            if let Err(why) = validate_env(env) {
                return ActionVerdict::disabled(why);
            }
            match check_cwd(ctx.roots, cwd) {
                RootsVerdict::Allowed { cwd, .. } => ActionVerdict { cwd: Some(cwd), tool: Some((tool.id, tool.name.clone())), ..ActionVerdict::ok() },
                RootsVerdict::Denied { reason } => ActionVerdict::disabled(reason),
            }
        }
        SourceAction::Terminal { cwd, env, command, .. } => {
            if command.first().is_some_and(|w| w.trim().is_empty()) {
                return ActionVerdict::disabled("the command's program name is empty".into());
            }
            if let Err(why) = validate_env(env) {
                return ActionVerdict::disabled(why);
            }
            match check_cwd(ctx.roots, cwd) {
                RootsVerdict::Allowed { cwd, .. } => ActionVerdict { cwd: Some(cwd), ..ActionVerdict::ok() },
                RootsVerdict::Denied { reason } => ActionVerdict::disabled(reason),
            }
        }
        SourceAction::Endpoint { .. } if action.is_unconfirmable_mutation() => ActionVerdict {
            enabled: false,
            reason: Some("a mutating action without a confirm text is not offered".into()),
            hidden: true,
            cwd: None,
            tool: None,
        },
        SourceAction::Endpoint { .. } => ActionVerdict::ok(),
        // Listing never waits on a name lookup; opening does the full check.
        SourceAction::Link { url, .. } => match source_link_shape_check(url, ctx.link_hosts) {
            Ok(_) => ActionVerdict::ok(),
            Err(why) => ActionVerdict::disabled(why),
        },
        SourceAction::Panel { endpoint, .. } => match validate_endpoint_path(endpoint) {
            Ok(()) => ActionVerdict::ok(),
            Err(why) => ActionVerdict::disabled(format!("the panel's endpoint is unusable: {why}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_tools::ToolType;
    use crate::roots::RootKind;
    use serde_json::json;

    fn tool(id: u32, name: &str, enabled: bool) -> AgentTool {
        let mut t = AgentTool::template(ToolType::Custom);
        t.id = id;
        t.name = name.into();
        t.program = "x".into();
        t.enabled = enabled;
        t
    }

    #[test]
    fn every_kind_parses_and_bad_shapes_name_the_field() {
        let agent = parse_action(&json!({"kind": "agent", "id": "a", "label": "Agent here", "cwd": "/srv/x", "prompt": "Fix it",
            "tool": "claude", "title": "fixer", "env": {"CHAPPA_STACK_SLOT": "1"}})).unwrap();
        assert_eq!(agent.kind(), "agent");
        assert_eq!(agent.id(), "a");
        match &agent {
            SourceAction::Agent { env, tool, title, .. } => {
                assert_eq!(env["CHAPPA_STACK_SLOT"], "1");
                assert_eq!(tool.as_deref(), Some("claude"));
                assert_eq!(title.as_deref(), Some("fixer"));
            }
            other => panic!("{other:?}"),
        }
        let term = parse_action(&json!({"kind": "terminal", "id": "t", "label": "Shell", "cwd": "/srv/x"})).unwrap();
        assert!(matches!(term, SourceAction::Terminal { ref command, .. } if command.is_empty()));
        let ep = parse_action(&json!({"kind": "endpoint", "id": "e", "label": "Approve", "mutates": true, "confirm": "Approve?", "note": "required", "params": {"to": "done"}})).unwrap();
        assert!(matches!(ep, SourceAction::Endpoint { note: Some(NoteRule::Required), mutates: true, .. }));
        assert!(!ep.is_unconfirmable_mutation());
        let unsafe_ep = parse_action(&json!({"kind": "endpoint", "id": "u", "label": "Nuke", "mutates": true})).unwrap();
        assert!(unsafe_ep.is_unconfirmable_mutation());
        assert!(matches!(parse_action(&json!({"kind": "link", "id": "l", "label": "Open", "url": "https://x"})).unwrap(), SourceAction::Link { .. }));
        assert!(matches!(parse_action(&json!({"kind": "panel", "id": "p", "label": "Validate", "endpoint": "/api/panel"})).unwrap(), SourceAction::Panel { .. }));

        let err = |v: Value| parse_action(&v).unwrap_err();
        assert!(err(json!({"kind": "agent", "id": "a", "label": "x", "cwd": "/y"})).contains("`prompt`"));
        assert!(err(json!({"kind": "terminal", "id": "t", "label": "x", "cwd": "/y", "command": "rm -rf /"})).contains("argv"));
        assert!(err(json!({"kind": "endpoint", "id": "e", "label": "x"})).contains("`mutates`"));
        assert!(err(json!({"kind": "endpoint", "id": "e", "label": "x", "mutates": false, "note": "maybe"})).contains("`note`"));
        assert!(err(json!({"kind": "agent", "id": "a", "label": "x", "cwd": "/y", "prompt": "p", "env": {"lower": "1"}})).contains("env name `lower`"));
        assert!(err(json!({"kind": "agent", "id": "a", "label": "x", "cwd": "/y", "prompt": "p", "env": {"A=B": "1"}})).contains("A=B"));
        assert!(err(json!({"kind": "wat", "id": "a", "label": "x"})).contains("unknown action kind"));
        assert!(err(json!("nope")).contains("object"));
    }

    #[test]
    fn env_names_follow_the_identifier_rule() {
        let ok: BTreeMap<String, String> = [("CHAPPA_STACK_SLOT", "1"), ("_X", ""), ("A9", "z")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert!(validate_env(&ok).is_ok());
        for bad in ["", "9A", "a", "A-B", "A B", "Ä"] {
            let env: BTreeMap<String, String> = [(bad.to_string(), "v".to_string())].into_iter().collect();
            assert!(validate_env(&env).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_walker_finds_actions_everywhere_and_keys_them_by_the_outermost_item() {
        let payload = json!({
            "freshness": {"asOf": null},
            "items": [
                {"key": "ABC-1", "title": "t", "actions": [{"kind": "link", "id": "open", "label": "Open", "url": "https://x"}],
                 "sections": [{"id": "s1", "title": "S", "rows": [], "actions": [{"kind": "endpoint", "id": "approve", "label": "A", "mutates": true, "confirm": "ok?"}]}]},
                {"key": "ABC-2", "actions": [{"kind": "endpoint", "id": "approve", "label": "A", "mutates": true}]}
            ],
            "components": [{"id": "api", "group": "g", "state": "up", "actions": [{"kind": "terminal", "id": "logs", "label": "Logs", "cwd": "/x"}]}],
            "banners": [{"id": "drift", "tone": "warn", "title": "D", "actions": [{"kind": "panel", "id": "validate", "label": "V", "endpoint": "/api/panel/validate"}]}],
            "actions": [{"kind": "endpoint", "id": "ping", "label": "Ping", "mutates": false}, {"kind": "bogus"}]
        });
        let (found, broken) = actions_in(&payload);
        let summary: Vec<(Option<&str>, &str, &str)> = found.iter().map(|f| (f.key.as_deref(), f.action.id(), f.path.as_str())).collect();
        assert_eq!(
            summary,
            vec![
                (None, "ping", "actions[0]"),
                (None, "validate", "banners[0].actions[0]"),
                (Some("api"), "logs", "components[0].actions[0]"),
                (Some("ABC-1"), "open", "items[0].actions[0]"),
                (Some("ABC-1"), "approve", "items[0].sections[0].actions[0]"),
                (Some("ABC-2"), "approve", "items[1].actions[0]"),
            ]
        );
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "actions[1]");
        assert_eq!(actions_for(&payload, Some("ABC-1")).len(), 2);
        assert_eq!(actions_for(&payload, None).len(), 2, "the payload's own and its banner's");
        assert!(find_action(&payload, Some("drift"), "validate").is_none(), "a banner's id is not a key");
        assert_eq!(find_action(&payload, Some("ABC-2"), "approve").unwrap().path, "items[1].actions[0]");
        assert!(find_action(&payload, Some("ABC-1"), "approve").unwrap().action.is_unconfirmable_mutation() == false);
        assert!(find_action(&payload, Some("ABC-2"), "approve").unwrap().action.is_unconfirmable_mutation());
        assert!(find_action(&payload, Some("ABC-9"), "open").is_none());
        assert!(find_action(&payload, None, "open").is_none(), "an item's action is not a top-level one");
    }

    #[test]
    fn sections_and_banners_belong_to_what_holds_them() {
        let payload = json!({
            "facts": [{"id": "runtimes", "title": "R", "rows": [], "actions": [{"kind": "endpoint", "id": "refresh", "label": "R", "mutates": false}]}],
            "sections": [{"id": "checks", "title": "C", "rows": [], "actions": [{"kind": "endpoint", "id": "rerun", "label": "R", "mutates": false}]}],
            "components": [{"id": "api", "sections": [{"id": "conf", "title": "C", "rows": [], "actions": [{"kind": "endpoint", "id": "reload", "label": "R", "mutates": false}]}]}]
        });
        let found = actions_in(&payload).0;
        let mut keys: Vec<(Option<&str>, &str)> = found.iter().map(|f| (f.key.as_deref(), f.action.id())).collect();
        keys.sort();
        assert_eq!(keys, vec![(None, "refresh"), (None, "rerun"), (Some("api"), "reload")]);
    }

    #[test]
    fn pull_requests_are_keyed_by_forge_repo_and_number_so_equal_numbers_do_not_collide() {
        let payload = json!({
            "freshness": {"asOf": null},
            "items": [
                {"forge": "github", "repo": "owner/repo", "number": "#12", "actions": [{"kind": "link", "id": "open", "label": "Open", "url": "https://x/a/12"}]},
                {"forge": "github", "repo": "owner/other", "number": "#12", "actions": [{"kind": "link", "id": "open", "label": "Open", "url": "https://x/b/12"}]},
                {"number": "#3", "actions": [{"kind": "link", "id": "open", "label": "Open", "url": "https://x/c/3"}]}
            ]
        });
        let url = |key: &str| match find_action(&payload, Some(key), "open").map(|f| f.action) {
            Some(SourceAction::Link { url, .. }) => Some(url),
            _ => None,
        };
        assert_eq!(pr_key("github", "owner/repo", "#12"), "github:owner/repo:#12");
        assert_eq!(url("github:owner/repo:#12").as_deref(), Some("https://x/a/12"));
        assert_eq!(url("github:owner/other:#12").as_deref(), Some("https://x/b/12"));
        assert_eq!(url("#12"), None, "the bare number no longer names either");
        assert_eq!(url("#3").as_deref(), Some("https://x/c/3"), "without forge and repo the number still keys");
        assert_eq!(actions_for(&payload, Some("github:owner/repo:#12")).len(), 1);
    }

    #[test]
    fn verdicts_resolve_tools_and_folders_and_hide_unconfirmable_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        std::fs::create_dir_all(root.join("wt")).unwrap();
        let tools = [tool(1, "claude", true), tool(2, "codex", false)];
        let roots = [Root::new(&root, RootKind::Project { id: 1, name: "app".into() })];
        let hosts = vec!["github.com".to_owned()];
        let ctx = VerdictContext { tools: &tools, roots: &roots, link_hosts: &hosts };
        let inside = root.join("wt").display().to_string();
        let agent = |tool: Option<&str>, cwd: &str| SourceAction::Agent {
            id: "a".into(),
            label: "A".into(),
            cwd: cwd.into(),
            prompt: "do".into(),
            tool: tool.map(str::to_owned),
            title: None,
            env: BTreeMap::new(),
        };
        let v = action_verdict(&agent(Some("Claude"), &inside), &ctx);
        assert!(v.enabled, "{v:?}");
        assert_eq!(v.tool, Some((1, "claude".into())));
        assert!(v.cwd.is_some());
        let v = action_verdict(&agent(None, &inside), &ctx);
        assert_eq!(v.tool, Some((1, "claude".into())), "no name = the first enabled tool");
        assert!(action_verdict(&agent(Some("codex"), &inside), &ctx).reason.unwrap().contains("disabled"));
        assert!(action_verdict(&agent(Some("gemini"), &inside), &ctx).reason.unwrap().contains("no agent tool is named `gemini`"));
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let v = action_verdict(&agent(Some("claude"), &outside.display().to_string()), &ctx);
        assert!(!v.enabled);
        assert!(v.reason.unwrap().contains("not inside an open project"));
        assert!(action_verdict(&agent(Some("claude"), "relative/dir"), &ctx).reason.unwrap().contains("relative"));

        let term = SourceAction::Terminal { id: "t".into(), label: "T".into(), cwd: inside.clone(), command: vec![], env: BTreeMap::new() };
        assert!(action_verdict(&term, &ctx).enabled);
        let mut bad_env = BTreeMap::new();
        bad_env.insert("path".to_owned(), "x".to_owned());
        let term = SourceAction::Terminal { id: "t".into(), label: "T".into(), cwd: inside.clone(), command: vec![], env: bad_env };
        assert!(action_verdict(&term, &ctx).reason.unwrap().contains("env name"));

        let hidden = SourceAction::Endpoint { id: "u".into(), label: "U".into(), mutates: true, confirm: None, params: None, note: None };
        let v = action_verdict(&hidden, &ctx);
        assert!(v.hidden && !v.enabled);
        let shown = SourceAction::Endpoint { id: "u".into(), label: "U".into(), mutates: true, confirm: Some("Sure?".into()), params: None, note: None };
        assert!(action_verdict(&shown, &ctx).enabled);
        let read_only = SourceAction::Endpoint { id: "p".into(), label: "P".into(), mutates: false, confirm: None, params: None, note: None };
        assert!(action_verdict(&read_only, &ctx).enabled, "a non-mutating action needs no confirm");

        let link = |url: &str| SourceAction::Link { id: "l".into(), label: "L".into(), url: url.into() };
        assert!(action_verdict(&link("https://github.com/x"), &ctx).enabled);
        // Listing never looks a name up: plain http on a trusted name is
        // offered, and opening it decides by the lookup (`links.rs`).
        assert!(action_verdict(&link("http://github.com/x"), &ctx).enabled);
        assert!(action_verdict(&link("ftp://github.com/x"), &ctx).reason.unwrap().contains("only https"));
        assert!(action_verdict(&link("https://gist.github.com/x"), &ctx).reason.unwrap().contains("not by subdomain"));

        let panel = |ep: &str| SourceAction::Panel { id: "p".into(), label: "P".into(), endpoint: ep.into() };
        assert!(action_verdict(&panel("/api/panel/validate"), &ctx).enabled);
        assert!(action_verdict(&panel("http://10.0.0.5/steal"), &ctx).reason.unwrap().contains("unusable"));
    }

    #[test]
    fn overlay_entries_are_found_by_key_and_owned_rows_never_are_overlays() {
        let payload = json!({"items": [
            {"ref": {"source": "tracker", "key": "ABC-1"}, "key": "ABC-1", "actions": [{"kind": "endpoint", "id": "archive", "label": "A", "mutates": false}]},
            {"ref": {"source": "tracker", "key": "ABC-2"}, "actions": [{"kind": "endpoint", "id": "archive", "label": "A", "mutates": false}]},
            {"key": "ABC-3", "title": "own"},
            {"ref": {"source": "", "key": "ABC-4"}, "key": "ABC-4"}
        ]});
        assert_eq!(overlay_ref(&payload["items"][0]), Some(("tracker", "ABC-1")));
        assert_eq!(overlay_ref(&payload["items"][2]), None);
        assert_eq!(overlay_ref(&payload["items"][3]), None, "a ref without a source is no overlay");
        // An overlay that names no key of its own is addressed by `ref.key`.
        assert!(find_action(&payload, Some("ABC-2"), "archive").is_some());
        assert_eq!(work_entry(&payload, "ABC-2").unwrap()["ref"]["key"], "ABC-2");
        assert!(owned_row(&payload, "work", "ABC-1").is_none(), "an overlay is not the source's own row");
        assert_eq!(owned_row(&payload, "work", "ABC-3").unwrap()["title"], "own");
        assert!(has_overlay(&payload, "tracker", "ABC-1"));
        assert!(!has_overlay(&payload, "tracker", "ABC-3"));
        assert!(!has_overlay(&payload, "other", "ABC-1"));
        let prs = json!({"items": [{"forge": "github", "repo": "owner/repo", "number": "#1", "title": "t"}]});
        assert_eq!(owned_row(&prs, "prs", "github:owner/repo:#1").unwrap()["title"], "t");
        assert!(owned_row(&prs, "prs", "github:owner/repo:#12").is_none());
        assert!(owned_row(&prs, "environment", "x").is_none());
    }

    #[test]
    fn confirm_texts_are_filled_in_one_pass_with_a_short_one_line_title() {
        let t = "Create a workspace for {key} — {title} (from {source})";
        assert_eq!(fill_confirm(t, "ABC-1", Some("Fix login"), "Tracker"), "Create a workspace for ABC-1 — Fix login (from Tracker)");
        assert_eq!(fill_confirm(t, "ABC-1", None, "Tracker"), "Create a workspace for ABC-1 — (no title) (from Tracker)");
        // A title cannot bring in another placeholder; unknown braces stay.
        assert_eq!(fill_confirm("{title} / {key} {nope} {", "K", Some("{source}"), "S"), "{source} / K {nope} {");
        // Line breaks and control characters flatten; exactly 80 stays, 81 is cut to 79 + an ellipsis.
        assert_eq!(one_line_title("a\n\tb\u{7}  c", 80), "a b c");
        let eighty = "x".repeat(80);
        assert_eq!(one_line_title(&eighty, CONFIRM_TITLE_MAX), eighty);
        let long = format!("{eighty}é");
        let cut = one_line_title(&long, CONFIRM_TITLE_MAX);
        assert_eq!(cut.chars().count(), 80);
        assert!(cut.ends_with('…') && cut.starts_with(&"x".repeat(79)));
        let filled = fill_confirm("{title}", "K", Some(&"é".repeat(200)), "S");
        assert_eq!(filled.chars().count(), 80, "counted in characters, not bytes");
    }

    #[test]
    fn the_fixture_payloads_parse_with_their_actions() {
        for (name, text) in [
            ("work", include_str!("../../../sources/fixture/data/work.json")),
            ("prs", include_str!("../../../sources/fixture/data/prs.json")),
            ("attention", include_str!("../../../sources/fixture/data/attention.json")),
            ("environment", include_str!("../../../sources/fixture/data/environment.json")),
            ("panel", include_str!("../../../sources/fixture/data/panel.json")),
            ("panel-checks", include_str!("../../../sources/fixture/data/panel-checks.json")),
            ("overlay work", include_str!("../../../sources/fixture/data-overlay/work.json")),
        ] {
            let value: Value = serde_json::from_str(text).unwrap();
            let (found, broken) = actions_in(&value);
            assert!(broken.is_empty(), "{name}: {broken:?}");
            assert!(!found.is_empty(), "{name} has actions to exercise");
        }
    }
}
