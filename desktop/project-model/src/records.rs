//! Durable row records: what a rail row is when no terminal is attached.
//!
//! A rail row's identity used to be its terminal id, so an exit left a dead
//! pane, a close dropped the row and a restart forgot every agent. A RECORD
//! separates the two: it has a stable `record_id` (a random 128-bit hex
//! string, never a terminal id — terminal ids restart at 1 every launch and
//! recycle) and points at its CURRENT terminal only while one is live.
//!
//! Three kinds of row get a record:
//! - a **command** — the process definition the store already owns; its
//!   record state (`record_id`, `last_run`) rides the per-command
//!   `ProcessOverride` so an untouched file stays byte-identical;
//! - an **agent** — the durable half of a spawn request ([`AgentRecord`]);
//! - a **terminal** — a shell ([`TerminalRecord`]).
//!
//! Every record keeps `last_run`: the denormalized head of the run ledger
//! (`runs` in coordination.db, last [`RUNS_RETAINED`] per record), so the
//! rail opens without touching the database. A run is tagged by its
//! `run_uuid` (the spawn uuid of the terminal that ran it) and every LATE
//! fact about a run — exit, tail, and later a summary or a session id —
//! carries that uuid and is dropped when the record has moved on to a newer
//! run ([`RunUpdate::Stale`]). That rule is what makes a respawn into the
//! same terminal id, a close racing an exit, or a slow writer harmless.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use term_core::status::ProcessStatus;

/// How many output lines (or, for a json-transport agent, typed events) the
/// automatic tail keeps when a run exits. Enough to read why something
/// stopped; small enough that projects.json stays a quick read.
pub const TAIL_LINES: usize = 200;

/// Runs kept per record in the ledger; older rows are pruned on insert.
pub const RUNS_RETAINED: usize = 20;

/// A fresh record id: 16 random bytes as lowercase hex, the same form as a
/// spawn uuid. An RNG failure is an error, never a weaker fallback — two
/// records sharing an id would merge their histories.
pub fn new_record_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot generate a record id: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The record kinds, as they appear on the wire and in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Command,
    Agent,
    Terminal,
}

impl RecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::Command => "command",
            RecordKind::Agent => "agent",
            RecordKind::Terminal => "terminal",
        }
    }
}

/// What the automatic tail holds: terminal text lines, or — for a
/// json-transport agent, whose output is typed events rather than a screen —
/// the serialized events themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TailKind {
    #[default]
    Text,
    Events,
}

impl TailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TailKind::Text => "text",
            TailKind::Events => "events",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "events" => TailKind::Events,
            _ => TailKind::Text,
        }
    }
}

/// One run of a record. Serialized the same way in projects.json /
/// workspace.json (`last_run`) and on the ledger route; `summary` and
/// `session` are RESERVED slots (a run summary; a provider session identity)
/// kept as opaque JSON so a file written by a later build round-trips here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub run_uuid: String,
    /// `running` while the terminal is live; `exited` / `failed` once the
    /// child's exit was observed; `stopped` when the row was closed before
    /// that (the user closed it, the app quit, or a crash — reconciled to
    /// `exited` with no exit code at the next launch).
    pub status: ProcessStatus,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(default)]
    pub tail_kind: TailKind,
    /// The last [`TAIL_LINES`] lines (`tail_kind: text`, strings) or events
    /// (`tail_kind: events`, objects) at exit; empty while running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tail: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Value>,
}

impl Run {
    /// A run that just started.
    pub fn begin(run_uuid: impl Into<String>, started_at_ms: u64, tail_kind: TailKind) -> Self {
        Self {
            run_uuid: run_uuid.into(),
            status: ProcessStatus::Running,
            started_at_ms,
            ended_at_ms: None,
            exit_code: None,
            success: None,
            tail_kind,
            tail: Vec::new(),
            summary: None,
            session: None,
        }
    }

    /// The child exited: record the outcome and the automatic tail.
    pub fn finish(&mut self, ended_at_ms: u64, exit_code: Option<i32>, success: bool, tail: Vec<Value>) {
        self.status = if success {
            ProcessStatus::Exited
        } else {
            ProcessStatus::Failed
        };
        self.ended_at_ms = Some(ended_at_ms);
        self.exit_code = exit_code;
        self.success = Some(success);
        self.tail = tail;
    }

    /// The row was closed while the child was still running.
    pub fn stop(&mut self, ended_at_ms: u64) {
        self.status = ProcessStatus::Stopped;
        self.ended_at_ms = Some(ended_at_ms);
    }

    /// True while no end has been recorded — the only state a late fact may
    /// still land on.
    pub fn is_open(&self) -> bool {
        matches!(self.status, ProcessStatus::Starting | ProcessStatus::Running)
    }

    /// A run still `running` when the app starts was interrupted by a crash
    /// or a hard kill: nothing observed its exit, so it ends as `exited`
    /// with no exit code, no success and whatever tail it had. Returns true
    /// when something changed.
    pub fn reconcile_crash(&mut self) -> bool {
        if !self.is_open() {
            return false;
        }
        self.status = ProcessStatus::Exited;
        self.exit_code = None;
        self.success = None;
        true
    }

    /// The row-surface projection: everything but the tail itself.
    pub fn head(&self) -> RunHead {
        RunHead {
            run_uuid: self.run_uuid.clone(),
            status: self.status,
            started_at_ms: self.started_at_ms,
            ended_at_ms: self.ended_at_ms,
            exit_code: self.exit_code,
            success: self.success,
            tail_kind: self.tail_kind,
            tail_len: self.tail.len(),
            summary: self.summary.clone(),
            session: self.session.clone(),
        }
    }
}

/// `last_run` minus the tail — what every row surface broadcasts (the tail is
/// fetched through the ledger route, never pushed with a row). `tail_len`
/// says whether there is anything to fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunHead {
    pub run_uuid: String,
    pub status: ProcessStatus,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub success: Option<bool>,
    pub tail_kind: TailKind,
    pub tail_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Value>,
}

/// The outcome of applying a late fact to a record's current run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUpdate {
    /// The record's current run carried that uuid and was still open.
    Applied,
    /// The record has a different (newer) run, or that run already ended:
    /// the fact is dropped. The ledger row for the run may still take it.
    Stale,
    /// No record with that id.
    NotFound,
}

/// Apply `edit` to `last_run` when it is the run `run_uuid` names and still
/// open — the one rule every late fact goes through.
pub fn update_run(last_run: Option<&mut Run>, run_uuid: &str, edit: impl FnOnce(&mut Run)) -> RunUpdate {
    match last_run {
        Some(run) if run.run_uuid == run_uuid && run.is_open() => {
            edit(run);
            RunUpdate::Applied
        }
        Some(_) => RunUpdate::Stale,
        None => RunUpdate::Stale,
    }
}

/// The durable half of an agent spawn: enough to start the same agent again
/// (the tool, the project it belongs to by its location in the store, the
/// extra args) plus the row's own state (name, favorite, renaming toggle,
/// notification level). `prompt` is kept for display and for a later
/// resume feature; starting the record again never replays it — a fresh
/// session that silently re-sends an old prompt would be a surprise.
///
/// `parent_record_id` is the RECORD id of the agent that spawned this one
/// (resolved from the spawner's terminal at spawn time; none when the
/// spawner had no record). Stored, not interpreted here: the stopped-row
/// forest nests by it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub record_id: String,
    pub name: String,
    pub tool_id: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_auto_rename: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Run>,
}

/// A shell row. `shell_profile` names a settings profile (none = the
/// platform default shell) and `cwd` is where the shell was opened, so
/// starting the record again resumes in the same place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalRecord {
    pub record_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Run>,
}

/// Where a record was found: its kind and, for a command, the name it is
/// keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    pub kind: RecordKind,
    pub name: String,
}

/// The ONE list every store answers for a record id (the location —
/// project or workspace — is the store's own business). Used by the
/// services above the stores to dispatch start / remove / list.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRecord {
    pub record_id: String,
    pub kind: RecordKind,
    pub name: String,
    pub last_run: Option<Run>,
    pub agent: Option<AgentRecord>,
    pub terminal: Option<TerminalRecord>,
}

/// Reconcile every run in `runs` that is still open (see
/// [`Run::reconcile_crash`]); returns the uuids that changed so the ledger
/// rows can follow.
pub fn reconcile_runs<'a>(runs: impl Iterator<Item = &'a mut Run>) -> Vec<String> {
    runs.filter_map(|run| run.reconcile_crash().then(|| run.run_uuid.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_ids_are_32_hex_and_distinct() {
        let a = new_record_id().unwrap();
        let b = new_record_id().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// A run written by this build round-trips through JSON, and a run
    /// written with ONLY the required keys (an older or sparser writer)
    /// loads with every optional field at its default — never an error.
    #[test]
    fn runs_round_trip_and_missing_fields_default() {
        let mut run = Run::begin("r1", 10, TailKind::Text);
        run.finish(20, Some(3), false, vec![json!("line a"), json!("line b")]);
        let text = serde_json::to_string(&run).unwrap();
        let back: Run = serde_json::from_str(&text).unwrap();
        assert_eq!(back, run);
        assert_eq!(back.status, ProcessStatus::Failed);
        assert_eq!(back.head().tail_len, 2);

        let sparse: Run =
            serde_json::from_str(r#"{"run_uuid":"r2","status":"running","started_at_ms":5}"#).unwrap();
        assert_eq!(sparse.tail_kind, TailKind::Text);
        assert!(sparse.tail.is_empty());
        assert_eq!(sparse.summary, None);
        assert_eq!(sparse.session, None);
        assert!(sparse.is_open());

        // Reserved slots pass through untouched (a later build's data).
        let future: Run = serde_json::from_str(
            r#"{"run_uuid":"r3","status":"exited","started_at_ms":1,"summary":{"text":"ok","source":"mcp"},"session":{"provider":"x","id":"y"}}"#,
        )
        .unwrap();
        assert_eq!(future.summary.as_ref().unwrap()["source"], "mcp");
        assert_eq!(future.session.as_ref().unwrap()["id"], "y");
        let again: Run = serde_json::from_str(&serde_json::to_string(&future).unwrap()).unwrap();
        assert_eq!(again, future);
    }

    /// The late-fact rule: a fact for the current, open run applies; a fact
    /// for an older run (or for a run that already ended) is dropped; a
    /// record without a run has nothing to apply it to.
    #[test]
    fn late_facts_apply_only_to_the_current_open_run() {
        let mut head = Some(Run::begin("new", 2, TailKind::Text));
        assert_eq!(
            update_run(head.as_mut(), "old", |r| r.stop(9)),
            RunUpdate::Stale
        );
        assert!(head.as_ref().unwrap().is_open(), "the stale stop touched nothing");
        assert_eq!(
            update_run(head.as_mut(), "new", |r| r.finish(5, Some(0), true, vec![])),
            RunUpdate::Applied
        );
        assert_eq!(head.as_ref().unwrap().status, ProcessStatus::Exited);
        // A second fact for the SAME run after it ended is stale too: first
        // writer wins (a close racing an exit cannot turn exited into stopped).
        assert_eq!(update_run(head.as_mut(), "new", |r| r.stop(6)), RunUpdate::Stale);
        assert_eq!(head.as_ref().unwrap().status, ProcessStatus::Exited);
        assert_eq!(update_run(None, "new", |_| {}), RunUpdate::Stale);
    }

    /// Crash reconcile: an open run becomes `exited` with no code and no
    /// success, keeping its tail; ended runs are untouched.
    #[test]
    fn crash_reconcile_closes_open_runs_only() {
        let mut open = Run::begin("a", 1, TailKind::Events);
        open.tail = vec![json!({"kind": "text"})];
        let mut stopped = Run::begin("b", 1, TailKind::Text);
        stopped.stop(2);
        let changed = reconcile_runs([&mut open, &mut stopped].into_iter());
        assert_eq!(changed, vec!["a".to_owned()]);
        assert_eq!(open.status, ProcessStatus::Exited);
        assert_eq!((open.exit_code, open.success, open.ended_at_ms), (None, None, None));
        assert_eq!(open.tail.len(), 1, "whatever tail it had stays");
        assert_eq!(stopped.status, ProcessStatus::Stopped);
    }

    /// Agent and terminal records serialize only what is set, and load with
    /// defaults for everything absent.
    #[test]
    fn agent_and_terminal_records_round_trip_sparsely() {
        let agent = AgentRecord {
            record_id: "id".into(),
            name: "claude".into(),
            tool_id: 3,
            favorite: false,
            disable_auto_rename: false,
            notification_level: None,
            parent_record_id: None,
            extra_args: vec![],
            prompt: None,
            last_run: None,
        };
        let text = serde_json::to_string(&agent).unwrap();
        assert_eq!(text, r#"{"record_id":"id","name":"claude","tool_id":3}"#);
        let back: AgentRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, agent);

        let terminal: TerminalRecord =
            serde_json::from_str(r#"{"record_id":"t","name":"shell"}"#).unwrap();
        assert_eq!(terminal.shell_profile, None);
        assert_eq!(terminal.cwd, None);
        assert_eq!(terminal.last_run, None);
    }
}
