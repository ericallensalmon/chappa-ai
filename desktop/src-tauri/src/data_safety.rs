//! Data safety, enforced by the app: agents are kept away from a database
//! that may hold real data.
//!
//! A stack source checks its databases and says what it found in the
//! `dataSafety` block of its Environment payload (the block is documented
//! in `desktop/sources/README.md`). The source owns detection; the app owns
//! enforcement, here:
//!
//! - **What blocks.** A block blocks agents when it says `blocksAgents`,
//!   and also when its `state` is `error` whatever it says (a check that
//!   cannot be judged fails closed). A source that sent a block and is now
//!   down, unreachable, untrusted or failing its Environment poll keeps its
//!   LAST block in force ("last known: …, source unavailable") until it
//!   answers again.
//! - **Across restarts.** Every answer's block is saved
//!   ([`crate::sources::verdicts`]); until a source answers after a start,
//!   its saved verdict counts (a blocking one as the last known one). A
//!   trusted stack source (`stack:…`) with no saved verdict keeps agents
//!   out of its folder until it first answers ([`UNHEARD_STATE`]); any
//!   other source that has never sent a block enforces nothing.
//! - **Where.** In the folders the block names under `bound` (the active
//!   item's folder and each of its worktrees), or, when no item is bound,
//!   the stack project's folder (`project`, else the folder of the project
//!   that launched the source). Containment is decided on resolved paths:
//!   `..` is folded first, symlinks are followed, and on Windows case is
//!   ignored. A worktree outside the stack project counts like any other
//!   named folder.
//! - **New agents** whose working folder is inside a blocked folder are
//!   refused at the one spawn path every launch takes (a source's `agent`
//!   action, the app's own spawn over IPC, the control surface and MCP,
//!   starting a stopped record and resuming one), with the reason.
//!   Terminals are not agents and are never refused.
//! - **Running agents** are not stopped. Each one inside a blocked folder
//!   is listed (the webview raises a Needs you entry for it), and what the
//!   control surface, MCP and timers deliver to it is held
//!   ([`crate::delivery_hold`]) until the block clears, then delivered in
//!   order. What the person types is never held.
//!
//! The webview reads all of it through [`data_safety_state`] and the
//! [`EVENT`] broadcast, sent when anything in it changes.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::delivery_hold::HoldGate;
use crate::registry::{Registry, TermId};
use crate::roots::{display, resolve_best_effort, same_or_inside, CASE_INSENSITIVE};
use crate::sources::{main_window_only, Sources};

/// The broadcast the webview listens to: the whole [`DataSafetyState`].
pub const EVENT: &str = "term://data-safety";
/// How often held deliveries are looked at and the state compared with the
/// last one sent.
pub const WATCH_EVERY: Duration = Duration::from_secs(1);
/// The view whose payload carries the block.
pub const ENVIRONMENT_VIEW: &str = "environment";

/// One finding as the block counts it (never a value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFinding {
    pub rule: String,
    pub count: u64,
}

/// One component's reason line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireReason {
    pub component: Option<String>,
    pub text: String,
}

/// The item a block is bound to and the folders agents work in for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireBound {
    pub source: Option<String>,
    pub key: String,
    pub folders: Vec<PathBuf>,
}

/// A `dataSafety` block as a source sent it, read leniently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireBlock {
    pub instance: String,
    pub state: String,
    pub blocks_agents: bool,
    pub findings: Vec<WireFinding>,
    pub unscanned: Vec<String>,
    pub reasons: Vec<WireReason>,
    pub bound: Option<WireBound>,
    pub project: Option<PathBuf>,
    pub ack_action_id: Option<String>,
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Absolute paths only: a relative one would be read against whatever the
/// app's own folder happens to be.
fn abs_path(v: &Value) -> Option<PathBuf> {
    v.as_str().map(PathBuf::from).filter(|p| p.is_absolute())
}

/// The block in an Environment payload, or `None` when the payload has
/// none (an older source, or a source that is not a stack): nothing to
/// enforce. A block whose `state` is missing or not a string reads as
/// `error`, so a garbled block fails closed.
pub fn parse_block(payload: &Value) -> Option<WireBlock> {
    let b = payload.get("dataSafety").filter(|b| b.is_object())?;
    let list = |key: &str| b.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let bound = b.get("bound").filter(|v| v.is_object()).and_then(|v| {
        let key = str_of(v, "key")?;
        let folders = v.get("folders").and_then(Value::as_array).map(|f| f.iter().filter_map(abs_path).collect()).unwrap_or_default();
        Some(WireBound { source: str_of(v, "source"), key, folders })
    });
    Some(WireBlock {
        instance: str_of(b, "instance").unwrap_or_else(|| "the stack".to_owned()),
        state: str_of(b, "state").unwrap_or_else(|| "error".to_owned()),
        blocks_agents: b.get("blocksAgents").and_then(Value::as_bool).unwrap_or(false),
        findings: list("findings")
            .iter()
            .filter_map(|f| Some(WireFinding { rule: str_of(f, "rule")?, count: f.get("count").and_then(Value::as_u64).unwrap_or(1) }))
            .collect(),
        unscanned: list("unscanned").iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        reasons: list("reasons")
            .iter()
            .filter_map(|r| Some(WireReason { component: str_of(r, "component"), text: str_of(r, "text")? }))
            .collect(),
        bound,
        project: b.get("project").and_then(abs_path),
        ack_action_id: str_of(b, "ackActionId"),
    })
}

impl WireBlock {
    /// Whether this block keeps agents out: the source says so, or its
    /// check cannot be judged (failing closed).
    pub fn enforces(&self) -> bool {
        self.blocks_agents || self.state == "error"
    }

    /// What the block says, in one phrase: the source's reason lines, else
    /// the findings counted per rule, else the state in words.
    pub fn summary(&self) -> String {
        if !self.reasons.is_empty() {
            return self.reasons.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("; ");
        }
        if !self.findings.is_empty() {
            let mut per_rule: Vec<(String, u64)> = Vec::new();
            for f in &self.findings {
                match per_rule.iter_mut().find(|(r, _)| *r == f.rule) {
                    Some((_, n)) => *n += f.count,
                    None => per_rule.push((f.rule.clone(), f.count)),
                }
            }
            let total: u64 = per_rule.iter().map(|(_, n)| n).sum();
            let items: Vec<String> = per_rule.iter().map(|(r, n)| format!("{r} ×{n}")).collect();
            return format!("{total} finding{} ({})", if total == 1 { "" } else { "s" }, items.join(", "));
        }
        match self.state.as_str() {
            "error" => "the data-safety check could not be judged".to_owned(),
            "unscanned" if !self.unscanned.is_empty() => format!("data not checked yet ({})", self.unscanned.join(", ")),
            "unscanned" => "data not checked yet".to_owned(),
            "not-clean" => "the data is not known to be scrubbed".to_owned(),
            other => format!("data safety is `{other}`"),
        }
    }
}

/// The text a blocked launch, a disabled button and a Needs you entry
/// show: what was found and where to deal with it, and, when the source is
/// not answering, that this is the last thing it said.
pub fn reason_text(block: &WireBlock, current: bool) -> String {
    let text = what_and_where(block);
    if current {
        text
    } else {
        format!("last known: {text}, source unavailable")
    }
}

/// The reason of a kept block whose launched source the project's
/// `chappa.yml` no longer declares: it stays until that content is trusted.
pub fn removed_reason(block: &WireBlock) -> String {
    format!("last known: {}, source removed — trust the new chappa.yml to lift", what_and_where(block))
}

fn what_and_where(block: &WireBlock) -> String {
    let next = if block.state == "not-clean" { "acknowledge in Environment" } else { "see Environment" };
    format!("{} — {next}", block.summary())
}

/// The `state` of the block a stack source is held to before its first
/// answer (it has said nothing yet, so its data may be real).
pub const UNHEARD_STATE: &str = "unknown";

/// The reason of that block.
pub fn unheard_reason(source_label: &str) -> String {
    format!("{source_label} has not said yet whether its data is safe — agents wait until it answers")
}

/// A block in force, as the webview and the gates see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveBlock {
    /// The source's registry id (what the views address it by).
    pub source_id: String,
    pub source_label: String,
    /// `<stack>/<slot>`.
    pub instance: String,
    pub state: String,
    /// The source answers now; false = the last block it sent before it
    /// went down, lost trust or stopped answering.
    pub current: bool,
    pub reason: String,
    /// The folders it covers, for people.
    pub folders: Vec<String>,
    /// The bound item: its owner's manifest id and key.
    pub bound: Option<BoundItem>,
    /// The component the first reason is about (the Environment row to
    /// open), when the source names one.
    pub component: Option<String>,
    pub ack_action_id: Option<String>,
    /// The same folders, as compared (not sent).
    #[serde(skip)]
    pub covered: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundItem {
    pub source: Option<String>,
    pub key: String,
}

/// The block in force for one source, or `None` when it does not keep
/// agents out (or names no folder at all, which cannot happen for a
/// launched source). `fallback` is the folder of the project that launched
/// the source, used when the block names no project.
pub fn active_block(source_id: &str, source_label: &str, block: &WireBlock, current: bool, fallback: Option<&Path>) -> Option<ActiveBlock> {
    if !block.enforces() {
        return None;
    }
    let covered: Vec<PathBuf> = match block.bound.as_ref().filter(|b| !b.folders.is_empty()) {
        Some(b) => b.folders.clone(),
        None => block.project.clone().or_else(|| fallback.map(Path::to_path_buf)).into_iter().collect(),
    };
    if covered.is_empty() {
        log::warn!("[data-safety] {source_label}: a block for {} names no folder; nothing can be enforced", block.instance);
        return None;
    }
    Some(ActiveBlock {
        source_id: source_id.to_owned(),
        source_label: source_label.to_owned(),
        instance: block.instance.clone(),
        state: block.state.clone(),
        current,
        reason: reason_text(block, current),
        folders: covered.iter().map(|p| display(p)).collect(),
        bound: block.bound.as_ref().map(|b| BoundItem { source: b.source.clone(), key: b.key.clone() }),
        component: block.reasons.iter().find_map(|r| r.component.clone()).or_else(|| block.unscanned.first().cloned()),
        ack_action_id: block.ack_action_id.clone(),
        covered,
    })
}

/// `path` with `.` dropped and each `..` taking off the part before it, so
/// a path that climbs back into a blocked folder is seen to be in it. On
/// the spelled path, before symlinks are followed.
fn fold_dots(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn resolved(path: &Path) -> PathBuf {
    resolve_best_effort(&fold_dots(path))
}

/// Whether `cwd` is one of `folders` or inside one, on resolved paths.
pub fn covers(folders: &[PathBuf], cwd: &Path) -> bool {
    let cwd = resolved(cwd);
    folders.iter().any(|f| same_or_inside(&resolved(f), &cwd, CASE_INSENSITIVE))
}

/// The first block covering `cwd`.
pub fn blocking<'a>(blocks: &'a [ActiveBlock], cwd: &Path) -> Option<&'a ActiveBlock> {
    blocks.iter().find(|b| covers(&b.covered, cwd))
}

/// How every launch refusal begins, so a caller can tell it from a failure.
pub const REFUSAL_LEAD: &str = "agents cannot start in `";

/// Whether `error` is a data-safety launch refusal.
pub fn is_refusal(error: &str) -> bool {
    error.starts_with(REFUSAL_LEAD)
}

/// The refusal of an agent launch in `cwd`.
pub fn launch_refusal(block: &ActiveBlock, cwd: &Path) -> String {
    format!(
        "{REFUSAL_LEAD}{}`: the data on {} ({}) may be real; {}",
        display(cwd),
        block.instance,
        block.source_label,
        block.reason
    )
}

/// What a held delivery says about why.
pub fn hold_text(block: &ActiveBlock) -> String {
    format!("held until data safety on {} clears: {}", block.instance, block.reason)
}

/// The spawn path's check: `Err` with the reason when `cwd` is blocked.
pub fn check_launch(sources: &Sources, cwd: &Path) -> Result<(), String> {
    let blocks = sources.data_safety_blocks();
    match blocking(&blocks, cwd) {
        Some(block) => Err(launch_refusal(block, cwd)),
        None => Ok(()),
    }
}

/// Deliveries to an agent in a blocked folder wait.
impl HoldGate for Sources {
    fn hold_reason(&self, cwd: &Path) -> Option<String> {
        let blocks = self.data_safety_blocks();
        blocking(&blocks, cwd).map(hold_text)
    }
}

/// A running agent inside a blocked folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedAgent {
    pub term_id: TermId,
    pub name: String,
    pub source_id: String,
    pub instance: String,
    pub reason: String,
    pub cwd: String,
}

/// Deliveries held for one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldCount {
    pub term_id: TermId,
    pub count: usize,
}

/// Everything the webview shows about data safety.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSafetyState {
    pub blocks: Vec<ActiveBlock>,
    pub agents: Vec<BlockedAgent>,
    pub held: Vec<HeldCount>,
}

/// The state now: the blocks in force, the running agents inside them,
/// and what is held for each pane.
pub fn state_of(sources: &Sources, registry: &Registry) -> DataSafetyState {
    let blocks = sources.data_safety_blocks();
    let agents = registry
        .live_agent_cwds()
        .into_iter()
        .filter_map(|(term_id, name, cwd)| {
            let b = blocking(&blocks, &cwd)?;
            Some(BlockedAgent {
                term_id,
                name,
                source_id: b.source_id.clone(),
                instance: b.instance.clone(),
                reason: b.reason.clone(),
                cwd: display(&cwd),
            })
        })
        .collect();
    let held = registry.held_counts().into_iter().map(|(term_id, count)| HeldCount { term_id, count }).collect();
    DataSafetyState { blocks, agents, held }
}

/// One pass of the watcher: release what the blocks no longer hold, then
/// the state, which the caller sends when it differs from the last.
pub fn tick(sources: &Sources, registry: &Registry) -> DataSafetyState {
    registry.release_held();
    state_of(sources, registry)
}

/// The app's watcher: every [`WATCH_EVERY`], release and broadcast
/// [`EVENT`] when the state moved.
pub fn spawn_watch(sources: Sources, registry: Registry, app: AppHandle) {
    let emit: Arc<dyn Fn(&DataSafetyState) + Send + Sync> = Arc::new(move |s| {
        let _ = app.emit(EVENT, s);
    });
    let spawned = std::thread::Builder::new().name("data safety".into()).spawn(move || {
        let mut last = DataSafetyState::default();
        loop {
            if sources.is_shutting_down() {
                return;
            }
            let now = tick(&sources, &registry);
            if now != last {
                emit(&now);
                last = now;
            }
            std::thread::sleep(WATCH_EVERY);
        }
    });
    if let Err(e) = spawned {
        log::error!("[data-safety] cannot start the watcher: {e}");
    }
}

/// `data_safety_state`: the blocks, the agents inside them, the held counts.
#[tauri::command]
pub fn data_safety_state(
    window: tauri::Window,
    sources: State<'_, Sources>,
    registry: State<'_, Registry>,
) -> Result<DataSafetyState, String> {
    main_window_only(&window)?;
    Ok(state_of(&sources, &registry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(v: Value) -> WireBlock {
        parse_block(&json!({ "dataSafety": v })).expect("a block")
    }

    fn base(state: &str, blocks: bool) -> Value {
        json!({
            "instance": "app/0", "state": state, "policy": if blocks { "block-agents" } else { "warn" },
            "blocksAgents": blocks, "findings": [], "unscanned": [], "ackActionId": "data-safety-ack",
            "bound": null, "project": abs("srv/example"), "reasons": [],
        })
    }

    fn abs(rel: &str) -> String {
        if cfg!(windows) { format!("C:\\{}", rel.replace('/', "\\")) } else { format!("/{rel}") }
    }

    #[test]
    fn every_state_reads_and_only_blocking_or_error_enforces() {
        for (state, blocks, enforces) in [
            ("clean", false, false),
            ("acknowledged", false, false),
            ("unscanned", false, false),
            ("unscanned", true, true),
            ("not-clean", false, false),
            ("not-clean", true, true),
            ("error", false, true),
            ("error", true, true),
        ] {
            let b = block(base(state, blocks));
            assert_eq!(b.state, state);
            assert_eq!(b.enforces(), enforces, "{state} blocksAgents={blocks}");
            assert_eq!(active_block("s1", "Stack", &b, true, None).is_some(), enforces, "{state}");
        }
        // No block at all: an older source, or not a stack.
        assert_eq!(parse_block(&json!({ "components": [] })), None);
        assert_eq!(parse_block(&json!({ "dataSafety": null })), None);
        // A garbled block fails closed.
        let garbled = parse_block(&json!({ "dataSafety": { "state": 7 } })).unwrap();
        assert!(garbled.enforces());
    }

    #[test]
    fn the_reason_names_what_was_found_and_where_to_deal_with_it() {
        let mut v = base("not-clean", true);
        v["findings"] = json!([
            { "rule": "email-real-domain", "table": "customers", "column": "email", "count": 2, "severity": "bad" },
            { "rule": "card-number", "table": "payments", "column": "card", "count": 1, "severity": "bad" },
        ]);
        let b = block(v.clone());
        assert_eq!(reason_text(&b, true), "3 findings (email-real-domain ×2, card-number ×1) — acknowledge in Environment");
        v["reasons"] = json!([{ "component": "db", "text": "db: 3 findings (email-real-domain ×2, card-number ×1)" }]);
        let b = block(v);
        assert_eq!(reason_text(&b, true), "db: 3 findings (email-real-domain ×2, card-number ×1) — acknowledge in Environment");
        assert_eq!(
            reason_text(&b, false),
            "last known: db: 3 findings (email-real-domain ×2, card-number ×1) — acknowledge in Environment, source unavailable"
        );
        let a = active_block("s1", "Stack", &b, true, None).unwrap();
        assert_eq!(a.component.as_deref(), Some("db"));
        let err = block(base("error", false));
        assert_eq!(reason_text(&err, true), "the data-safety check could not be judged — see Environment");
    }

    #[test]
    fn a_block_covers_its_bound_folders_else_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("stack");
        let item = project.join(".work").join("ABC-1");
        // A worktree outside the stack project is covered when it is named.
        let outside = dir.path().join("elsewhere").join("api-1a2b3c4d");
        let other = dir.path().join("other");
        for d in [&item, &outside, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut v = base("not-clean", true);
        v["project"] = json!(project.display().to_string());
        let unbound = active_block("s1", "Stack", &block(v.clone()), true, None).unwrap();
        assert!(covers(&unbound.covered, &project));
        assert!(covers(&unbound.covered, &item));
        assert!(!covers(&unbound.covered, &other));
        v["bound"] = json!({ "source": "tracker", "key": "ABC-1", "folders": [item.display().to_string(), outside.display().to_string(), "relative/ignored"] });
        let bound = active_block("s1", "Stack", &block(v), true, None).unwrap();
        assert_eq!(bound.bound, Some(BoundItem { source: Some("tracker".into()), key: "ABC-1".into() }));
        assert_eq!(bound.covered.len(), 2, "a relative folder is ignored");
        assert!(covers(&bound.covered, &item));
        assert!(covers(&bound.covered, &item.join("src").join("deep")), "a folder not created yet inside it");
        assert!(covers(&bound.covered, &outside));
        assert!(!covers(&bound.covered, &project), "bound: the project itself is not the item");
        assert!(!covers(&bound.covered, &other));
        // `..` is folded before comparing, both ways.
        assert!(covers(&bound.covered, &other.join("..").join("stack").join(".work").join("ABC-1")));
        assert!(!covers(&bound.covered, &item.join("..").join("ABC-2")));
        // A sibling whose name merely starts the same is not inside.
        assert!(!covers(&bound.covered, &project.join(".work").join("ABC-10")));
        // Case: Windows ignores it, so does the check there.
        let shouted = PathBuf::from(item.display().to_string().to_uppercase());
        assert_eq!(covers(&bound.covered, &shouted), cfg!(windows));
        // A launched source with no `project` in its block falls back to its project's folder.
        let mut bare = base("error", false);
        bare.as_object_mut().unwrap().remove("project");
        let b = active_block("s1", "Stack", &block(bare.clone()), true, Some(&project)).unwrap();
        assert!(covers(&b.covered, &item));
        assert_eq!(active_block("s1", "Stack", &block(bare), true, None), None, "no folder named: nothing to enforce");
    }

    #[test]
    fn the_refusal_says_where_and_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = base("not-clean", true);
        v["project"] = json!(dir.path().display().to_string());
        v["reasons"] = json!([{ "component": "db", "text": "db: 1 finding (card-number ×1)" }]);
        let a = active_block("s1", "Stack", &block(v), true, None).unwrap();
        let text = launch_refusal(&a, dir.path());
        assert!(text.contains("agents cannot start in"), "{text}");
        assert!(text.contains("on app/0 (Stack)"), "{text}");
        assert!(text.ends_with("may be real; db: 1 finding (card-number ×1) — acknowledge in Environment"), "{text}");
        assert_eq!(hold_text(&a), "held until data safety on app/0 clears: db: 1 finding (card-number ×1) — acknowledge in Environment");
    }
}
