//! Durable row records: the service over the stores, the run ledger, and
//! the control / command surface for `start`, `remove` and the run history.
//!
//! The model lives in `project_model::records`; where a record is stored
//! depends on what owns it — a project's records sit on its projects.json
//! row, project-less agents and workspace shells in workspace.json, and a
//! command's record state on the command's own override entry in either
//! file. This service hides that: every operation takes a `record_id` and
//! finds the record wherever it is.
//!
//! Lifecycle, in one place:
//! - **spawn**: the caller writes the record (if new) and its fresh run
//!   (`begin_run` — store head + ledger row) BEFORE the terminal exists, and
//!   passes the binding into the registry spawn, so even an instant exit
//!   lands on the right run;
//! - **exit**: the pump captures the tail and reports `run_exited` on its own
//!   thread before the exit handler (which may respawn the same terminal id);
//! - **close**: when a pump ends without an exit the run is `stopped`,
//!   reported from a detached thread (the closer may hold the store lock
//!   while it joins the pump);
//! - **launch**: every run still open was cut off by a crash and is
//!   reconciled to `exited` with no exit code; **quit** stops every open run
//!   synchronously first so a clean quit never looks like a crash.
//!
//! Every late fact goes through the run-uuid rule
//! (`project_model::records::update_run`, and `status IN (running, starting)`
//! on the ledger UPDATE): the first writer for a run wins, and a fact for a
//! run the record has moved past touches only that run's ledger row.
//!
//! **Session + resume.** A run learns the provider's id for its conversation
//! (the json adapter, a configured regex, an `OSC 777;session` announcement,
//! `set_session` over the control surface) as one more late fact under the
//! same rule. `resume` starts the record again with the PROVIDER continuing
//! that id — the agent comes back with its own complete history, nothing is
//! replayed here — and watches the start: a child that exits before it is
//! ready was refused by the provider, and the caller gets that refusal (with
//! the provider's own words) as a typed failure, never a fresh session
//! dressed up as a resume.
//!
//! **Summaries.** A process says what its run was about — `OSC
//! 777;summary;<text>` printed by the process, or `set_summary` over the
//! control surface — and that text lands on the CURRENT run as one more late
//! fact (last writer wins per run). Only a live run takes one: a finished
//! run's record is not edited after the fact.
//!
//! Lock discipline: the service holds clones of the store handles and never
//! locks two of them at once; the ledger (coordination.db) has its own
//! connection mutex. A command's record state is written by `projects.rs` /
//! `workspace.rs` directly while they hold their own lock (they cannot call
//! back in here), and they hold a [`RunLedger`] for the ledger half.

use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};

use project_model::records::{
    AgentRecord, AgentRecordPatch, RecordKind, Run, RunHead, RunUpdate, Session, SessionSource, StoredRecord,
    SummarySource, TailKind, TerminalRecord, RUNS_RETAINED,
};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, State};
use term_core::actor::epoch_ms;
use term_core::status::ProcessStatus;

use crate::agent_json::machine_mode;
use crate::agent_tools::{AgentToolsState, Transport};
use crate::agents::{readiness, spawn_agent_with, IoView, ReadyGate, Readiness, ResumeSpec, SpawnAgentRequest, SpawnContext};
use crate::control_http::{json, ControlState, Params, Resp};
use crate::coordination::{CoordError, Coordination};
use crate::coordination_http::Args;
use crate::projects::{ControlError, Projects};
use crate::registry::{
    emit_created, emit_session, emit_summary, AppCreatedBroadcast, ChannelSink, CreatedEvent, EventSink, NullSink,
    Registry, RunExit, RunRecorder, SummaryOutcome, TermId,
};
use crate::settings::SettingsState;
use crate::workspace::Workspace;

/// Where a record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project(u32),
    Workspace,
}

impl Scope {
    pub fn project_id(self) -> Option<u32> {
        match self {
            Scope::Project(id) => Some(id),
            Scope::Workspace => None,
        }
    }
}

/// Why a record operation failed; the HTTP layer maps variants to statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// No record with that id (or its project is not open).
    NotFound(String),
    /// A bad argument.
    Invalid(String),
    /// The operation needs something this caller cannot supply.
    Unavailable(String),
    Internal(String),
}

impl RecordError {
    pub fn status(&self) -> u16 {
        match self {
            RecordError::NotFound(_) => 404,
            RecordError::Invalid(_) => 400,
            RecordError::Unavailable(_) => 409,
            RecordError::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            RecordError::NotFound(m)
            | RecordError::Invalid(m)
            | RecordError::Unavailable(m)
            | RecordError::Internal(m) => m,
        }
    }
}

impl From<String> for RecordError {
    fn from(message: String) -> Self {
        RecordError::Internal(message)
    }
}

impl From<ControlError> for RecordError {
    fn from(err: ControlError) -> Self {
        match err {
            ControlError::NotFound(m) | ControlError::NotOpen(m) => RecordError::NotFound(m),
            ControlError::Internal(m) => RecordError::Internal(m),
        }
    }
}

impl From<RecordError> for String {
    fn from(err: RecordError) -> String {
        err.message().to_owned()
    }
}

// ---- the ledger -----------------------------------------------------------

/// The `runs` table in coordination.db. Shares the coordination store's one
/// connection; an uninitialized store (tests, the harness) answers `Err`,
/// which callers log — the record head in the JSON store is what the rail
/// reads, the ledger is history.
#[derive(Clone, Default)]
pub struct RunLedger {
    coord: Coordination,
}

impl RunLedger {
    pub fn new(coord: Coordination) -> Self {
        Self { coord }
    }

    /// Insert a run's row and prune the record to its newest
    /// [`RUNS_RETAINED`] runs.
    pub fn insert(&self, record_id: &str, project_id: Option<u32>, kind: RecordKind, run: &Run) -> Result<(), CoordError> {
        self.coord.with_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO runs (run_uuid, record_id, project_id, kind, status, started_at_ms, ended_at_ms,
                    exit_code, success, tail_kind, tail, summary, summary_source, session)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                rusqlite::params![
                    run.run_uuid,
                    record_id,
                    project_id.map(i64::from),
                    kind.as_str(),
                    run.status.as_str(),
                    run.started_at_ms as i64,
                    run.ended_at_ms.map(|v| v as i64),
                    run.exit_code,
                    run.success.map(i64::from),
                    run.tail_kind.as_str(),
                    Value::Array(run.tail.clone()).to_string(),
                    run.summary,
                    run.summary_source.map(SummarySource::as_str),
                    run.session.as_ref().and_then(|s| serde_json::to_string(s).ok()),
                ],
            )?;
            tx.execute(
                "DELETE FROM runs WHERE record_id = ?1 AND run_uuid NOT IN (
                    SELECT run_uuid FROM runs WHERE record_id = ?1
                    ORDER BY started_at_ms DESC, rowid DESC LIMIT ?2)",
                rusqlite::params![record_id, RUNS_RETAINED as i64],
            )?;
            Ok(())
        })
    }

    /// Record how a run ended. Applies only while the row is still open
    /// (first writer wins, the same rule the store head follows); returns
    /// whether a row changed.
    pub fn finish(&self, run_uuid: &str, run: &Run) -> Result<bool, CoordError> {
        self.coord.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE runs SET status=?2, ended_at_ms=?3, exit_code=?4, success=?5, tail_kind=?6, tail=?7
                 WHERE run_uuid=?1 AND status IN ('running','starting')",
                rusqlite::params![
                    run_uuid,
                    run.status.as_str(),
                    run.ended_at_ms.map(|v| v as i64),
                    run.exit_code,
                    run.success.map(i64::from),
                    run.tail_kind.as_str(),
                    Value::Array(run.tail.clone()).to_string(),
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// The row was closed before any exit: `stopped`, still first-writer-wins.
    pub fn stop(&self, run_uuid: &str, ended_at_ms: u64) -> Result<bool, CoordError> {
        self.coord.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE runs SET status='stopped', ended_at_ms=?2
                 WHERE run_uuid=?1 AND status IN ('running','starting')",
                rusqlite::params![run_uuid, ended_at_ms as i64],
            )?;
            Ok(changed > 0)
        })
    }

    /// The run learned its provider session. The row is that run's own
    /// history, so it takes the id whether or not the run is still open (an
    /// id printed on the way out lands after the exit is recorded).
    pub fn set_session(&self, run_uuid: &str, session: &Session) -> Result<bool, CoordError> {
        let text = serde_json::to_string(session).map_err(|e| CoordError::Internal(e.to_string()))?;
        self.coord.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE runs SET session=?2 WHERE run_uuid=?1",
                rusqlite::params![run_uuid, text],
            )?;
            Ok(changed > 0)
        })
    }

    /// The process posted a summary of the run (the text as stored, plain —
    /// not JSON); a later post replaces it. Unlike a session id, only while
    /// the row is still open: a finished run is not edited after the fact,
    /// so the row and the record head always agree. (A summary printed on
    /// the way out is handled before the exit, so it still finds it open.)
    pub fn set_summary(&self, run_uuid: &str, text: &str, source: SummarySource) -> Result<bool, CoordError> {
        self.coord.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE runs SET summary=?2, summary_source=?3 WHERE run_uuid=?1 AND status IN ('running','starting')",
                rusqlite::params![run_uuid, text, source.as_str()],
            )?;
            Ok(changed > 0)
        })
    }

    /// Launch-time reconcile: every row still open was cut off by a crash.
    pub fn reconcile_open_runs(&self) -> Result<usize, CoordError> {
        self.coord.with_tx(|tx| {
            Ok(tx.execute(
                "UPDATE runs SET status='exited', exit_code=NULL, success=NULL WHERE status IN ('running','starting')",
                [],
            )?)
        })
    }

    /// The record's runs, newest first, at most `limit`.
    pub fn runs(&self, record_id: &str, limit: usize) -> Result<Vec<Run>, CoordError> {
        self.coord.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT run_uuid, status, started_at_ms, ended_at_ms, exit_code, success, tail_kind, tail, summary, session,
                    summary_source
                 FROM runs WHERE record_id = ?1 ORDER BY started_at_ms DESC, rowid DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![record_id, limit as i64], |r| {
                let status: String = r.get(1)?;
                let tail_kind: String = r.get(6)?;
                let tail: String = r.get(7)?;
                let summary: Option<String> = r.get(8)?;
                let session: Option<String> = r.get(9)?;
                let summary_source: Option<String> = r.get(10)?;
                Ok(Run {
                    run_uuid: r.get(0)?,
                    status: parse_status(&status),
                    started_at_ms: r.get::<_, i64>(2)? as u64,
                    ended_at_ms: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    exit_code: r.get(4)?,
                    success: r.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                    tail_kind: TailKind::parse(&tail_kind),
                    tail: serde_json::from_str::<Vec<Value>>(&tail).unwrap_or_default(),
                    summary,
                    summary_source: summary_source.as_deref().and_then(SummarySource::parse),
                    session: session.and_then(|s| serde_json::from_str(&s).ok()),
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    /// Delete every run of the given records; returns how many rows went.
    pub fn delete_records(&self, record_ids: &[String]) -> Result<usize, CoordError> {
        if record_ids.is_empty() {
            return Ok(0);
        }
        self.coord.with_tx(|tx| {
            let mut total = 0;
            for id in record_ids {
                total += tx.execute("DELETE FROM runs WHERE record_id = ?1", [id])?;
            }
            Ok(total)
        })
    }
}

fn parse_status(s: &str) -> ProcessStatus {
    match s {
        "starting" => ProcessStatus::Starting,
        "running" => ProcessStatus::Running,
        "exited" => ProcessStatus::Exited,
        "failed" => ProcessStatus::Failed,
        _ => ProcessStatus::Stopped,
    }
}

// ---- the service ----------------------------------------------------------

#[derive(Clone, Default)]
struct Parts {
    projects: Projects,
    workspace: Workspace,
    ledger: RunLedger,
}

/// Managed state: the record service over the project store, the workspace
/// store and the run ledger. `Default` is inert (no stores) so tests and
/// the harness can construct a `ControlState` without one; every store op
/// then answers a clear error and spawns proceed without a record.
#[derive(Clone, Default)]
pub struct Records {
    parts: Arc<Mutex<Parts>>,
}

/// Everything a record start reaches for beyond the stores.
pub struct StartDeps<'a> {
    pub registry: &'a Registry,
    pub tools: &'a AgentToolsState,
    pub settings: &'a SettingsState,
    /// The app handle: the frontend's frames channel is bound to it, and a
    /// headless start broadcasts `term://created` through it. `None` in tests.
    pub app: Option<AppHandle>,
}

/// The reply to `start_record`.
#[derive(Debug, Clone, Serialize)]
pub struct StartDto {
    pub record_id: String,
    pub kind: RecordKind,
    pub term_id: Option<TermId>,
    pub run_uuid: Option<String>,
    /// A project command started from the control surface: the frontend
    /// performs the spawn, and it had not registered before the wait ran out.
    pub pending: bool,
}

/// The reply to `resume_record`: the new run, the id it continues, and how
/// far the provider was watched before the resume counted as accepted.
#[derive(Debug, Clone, Serialize)]
pub struct ResumeDto {
    pub record_id: String,
    pub kind: RecordKind,
    pub term_id: TermId,
    pub run_uuid: String,
    /// The provider session this run continues.
    pub session_id: String,
    pub check: ResumeCheck,
}

/// What "the provider accepted the resume" rested on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeCheck {
    /// The child produced output and went quiet (the ready gate opened).
    Quiet,
    /// The child kept printing for the whole max wait — a TUI that never
    /// goes quiet — and is still alive.
    ReadyByTimeout,
    /// The child printed nothing within the max wait but is still alive.
    Silent,
    /// A per-turn CLI (opencode json): no child runs until the first
    /// message, so the provider can only refuse then — as that send's
    /// error, in the transcript.
    Deferred,
}

/// Why a resume did not happen. The control surface answers it as
/// `{"error": <kind>, ...}`; the Tauri command rejects with the same object
/// (`kind` tagged), so the pane can show "Resume failed: <provider text>"
/// and offer a fresh start — and fall through to a plain start when there
/// was simply nothing to resume.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResumeFailure {
    /// The record's last run (or the named run) has no saved session.
    NoSession { record_id: String, message: String },
    /// The provider refused: the child exited before it was ready.
    /// `provider_text` is what it printed (the last lines of the run's
    /// tail), `run_uuid` the failed run — readable through the ledger.
    Rejected {
        record_id: String,
        run_uuid: String,
        term_id: TermId,
        exit_code: Option<i32>,
        provider_text: String,
        message: String,
    },
    /// Not an agent, no tool, no resume command, already running.
    Unavailable { record_id: String, message: String },
    NotFound { record_id: String, message: String },
    Internal { message: String },
}

impl ResumeFailure {
    pub fn status(&self) -> u16 {
        match self {
            ResumeFailure::NoSession { .. } | ResumeFailure::Rejected { .. } | ResumeFailure::Unavailable { .. } => 409,
            ResumeFailure::NotFound { .. } => 404,
            ResumeFailure::Internal { .. } => 500,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ResumeFailure::NoSession { .. } => "no_session",
            ResumeFailure::Rejected { .. } => "resume_rejected",
            ResumeFailure::Unavailable { .. } => "resume_unavailable",
            ResumeFailure::NotFound { .. } => "not_found",
            ResumeFailure::Internal { .. } => "internal",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ResumeFailure::NoSession { message, .. }
            | ResumeFailure::Rejected { message, .. }
            | ResumeFailure::Unavailable { message, .. }
            | ResumeFailure::NotFound { message, .. }
            | ResumeFailure::Internal { message } => message,
        }
    }

    /// The HTTP body: the tagged object with `kind` spelled `error`, the
    /// control surface's error word.
    pub fn to_json(&self) -> Value {
        let mut body = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Some(map) = body.as_object_mut() {
            map.remove("kind");
            map.insert("error".into(), json!(self.kind()));
        }
        body
    }
}

impl From<RecordError> for ResumeFailure {
    fn from(err: RecordError) -> Self {
        match err {
            RecordError::NotFound(m) => ResumeFailure::NotFound { record_id: String::new(), message: m },
            RecordError::Invalid(m) | RecordError::Unavailable(m) => {
                ResumeFailure::Unavailable { record_id: String::new(), message: m }
            }
            RecordError::Internal(m) => ResumeFailure::Internal { message: m },
        }
    }
}

/// How long a resume watches a child that prints nothing at all before
/// counting it as accepted ([`ResumeCheck::Silent`]).
const RESUME_SILENT_MS: u64 = 5_000;
/// How long a rejected resume waits for the run's exit (and tail) to be
/// recorded before answering with whatever text it has.
const RESUME_TAIL_WAIT: Duration = Duration::from_secs(3);
/// How many lines of the failed run's tail travel as the provider's text.
const REJECTION_LINES: usize = 4;

/// The reply to `remove_record`.
#[derive(Debug, Clone, Serialize)]
pub struct RemovedDto {
    pub record_id: String,
    pub kind: RecordKind,
    pub removed: bool,
    pub runs_deleted: usize,
}

/// One record as the frontend / dashboard lists it: the stored record plus
/// its live binding (if a terminal is attached right now).
#[derive(Debug, Clone, Serialize)]
pub struct RecordDto {
    pub record_id: String,
    pub kind: RecordKind,
    pub name: String,
    pub project_id: Option<u32>,
    /// The live terminal's status when one is attached; otherwise the last
    /// run's outcome (`exited` / `failed`) or `stopped`.
    pub status: ProcessStatus,
    pub term_id: Option<TermId>,
    pub last_run: Option<RunHead>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentFacts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalFacts>,
}

/// The agent record minus what the row already carries.
#[derive(Debug, Clone, Serialize)]
pub struct AgentFacts {
    pub tool_id: u32,
    pub parent_record_id: Option<String>,
    pub favorite: bool,
    pub disable_auto_rename: bool,
    pub notification_level: Option<String>,
    pub extra_args: Vec<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalFacts {
    pub shell_profile: Option<String>,
    pub cwd: Option<String>,
}

impl Records {
    /// A service over explicit stores (tests, and the app's setup).
    pub fn new(projects: Projects, workspace: Workspace, coordination: Coordination) -> Self {
        let this = Self::default();
        this.init(projects, workspace, coordination);
        this
    }

    /// Point the service at the real stores (once, at setup, after the
    /// stores themselves loaded) and reconcile runs a crash left open.
    pub fn init(&self, projects: Projects, workspace: Workspace, coordination: Coordination) {
        *self.parts.lock().unwrap_or_else(|e| e.into_inner()) = Parts {
            projects,
            workspace,
            ledger: RunLedger::new(coordination),
        };
        self.reconcile_crashes();
    }

    fn parts(&self) -> Parts {
        self.parts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The ledger handle the project and workspace runtimes write through
    /// while holding their own locks.
    pub fn ledger(&self) -> RunLedger {
        self.parts().ledger
    }

    /// Launch: a run still `running` in either store (or the ledger) was
    /// cut off by a crash or a hard kill — it ends as `exited` with no exit
    /// code and whatever tail it had.
    fn reconcile_crashes(&self) {
        let parts = self.parts();
        if let Err(e) = parts.projects.store_access(|s| s.reconcile_crashed_runs()).and_then(|r| r) {
            log::warn!("[records] projects.json crash reconcile: {e}");
        }
        if let Ok(store) = parts.workspace.store() {
            if let Err(e) = store.reconcile_crashed_runs() {
                log::warn!("[records] workspace.json crash reconcile: {e}");
            }
        }
        if let Err(e) = parts.ledger.reconcile_open_runs() {
            log::warn!("[records] ledger crash reconcile: {e}");
        }
    }

    /// Where `record_id` lives and what it is.
    pub fn locate(&self, record_id: &str) -> Option<(Scope, RecordKind, String)> {
        let parts = self.parts();
        if let Ok(Some((project_id, r))) = parts.projects.store_access(|s| s.find_record(record_id)) {
            return Some((Scope::Project(project_id), r.kind, r.name));
        }
        parts
            .workspace
            .store()
            .ok()
            .and_then(|s| s.find_record(record_id))
            .map(|r| (Scope::Workspace, r.kind, r.name))
    }

    /// The stored record with its scope.
    pub fn get(&self, record_id: &str) -> Option<(Scope, StoredRecord)> {
        let parts = self.parts();
        if let Ok(Some((project_id, record))) = parts.projects.store_access(|s| s.record(record_id)) {
            return Some((Scope::Project(project_id), record));
        }
        parts
            .workspace
            .store()
            .ok()
            .and_then(|s| s.record(record_id))
            .map(|r| (Scope::Workspace, r))
    }

    /// Every stored record, project ones first.
    pub fn all(&self) -> Vec<(Scope, StoredRecord)> {
        let parts = self.parts();
        let mut out: Vec<(Scope, StoredRecord)> = parts
            .projects
            .store_access(|s| s.all_records())
            .unwrap_or_default()
            .into_iter()
            .map(|(pid, r)| (Scope::Project(pid), r))
            .collect();
        if let Ok(store) = parts.workspace.store() {
            out.extend(store.records().into_iter().map(|r| (Scope::Workspace, r)));
        }
        out
    }

    /// The frontend's record list: every record with its live binding.
    pub fn list(&self, registry: &Registry) -> Vec<RecordDto> {
        self.all()
            .into_iter()
            .map(|(scope, record)| record_dto(registry, scope, record))
            .collect()
    }

    /// Store a new agent record in its scope.
    pub fn add_agent_record(&self, scope: Scope, record: AgentRecord) -> Result<(), String> {
        let parts = self.parts();
        match scope {
            Scope::Project(id) => parts.projects.store_access(|s| s.add_agent_record(id, record))?,
            Scope::Workspace => parts.workspace.store()?.add_agent_record(record),
        }
    }

    /// Store a new terminal record in its scope.
    pub fn add_terminal_record(&self, scope: Scope, record: TerminalRecord) -> Result<(), String> {
        let parts = self.parts();
        match scope {
            Scope::Project(id) => parts.projects.store_access(|s| s.add_terminal_record(id, record))?,
            Scope::Workspace => parts.workspace.store()?.add_terminal_record(record),
        }
    }

    /// A new run begins on `record_id`: the store head and a ledger row.
    /// Written BEFORE the terminal spawns, so the binding the registry gets
    /// already names a run the store knows.
    pub fn begin_run(&self, scope: Scope, kind: RecordKind, record_id: &str, run: &Run) -> Result<(), String> {
        let parts = self.parts();
        let found = match scope {
            Scope::Project(_) => parts.projects.store_access(|s| s.begin_run(record_id, run.clone()))??,
            Scope::Workspace => parts.workspace.store()?.begin_run(record_id, run.clone())?,
        };
        if !found {
            return Err(format!("no such record: {record_id}"));
        }
        if let Err(e) = parts.ledger.insert(record_id, scope.project_id(), kind, run) {
            log::warn!("[records] ledger insert for run {} failed: {e}", run.run_uuid);
        }
        Ok(())
    }

    /// Apply a late fact to a record's current run (store head, under the
    /// run-uuid rule) and to the run's own ledger row (first writer wins).
    /// The ledger row is the run's own history: it takes the fact even when
    /// the head has moved on, but never twice.
    fn apply_late_fact(&self, record_id: &str, run_uuid: &str, fact: &LateFact) {
        let parts = self.parts();
        let edit = |run: &mut Run| fact.apply(run);
        let outcome = match parts.projects.store_access(|s| s.update_run(record_id, run_uuid, edit)) {
            Ok(Ok(RunUpdate::NotFound)) | Err(_) => match parts.workspace.store() {
                Ok(store) => store.update_run(record_id, run_uuid, edit),
                Err(e) => Err(e),
            },
            Ok(other) => other,
        };
        match outcome {
            Ok(RunUpdate::Applied) => {}
            Ok(RunUpdate::Stale) => {
                log::info!("[records] late fact for run {run_uuid} of {record_id} dropped: the record moved on")
            }
            Ok(RunUpdate::NotFound) => {
                log::info!("[records] late fact for run {run_uuid}: record {record_id} is gone")
            }
            Err(e) => log::warn!("[records] late fact for run {run_uuid} of {record_id} not stored: {e}"),
        }
        let ledger = match fact {
            LateFact::Exited(exit) => {
                let mut row = Run::begin(run_uuid, 0, exit.tail_kind);
                row.finish(exit.ended_at_ms, exit.exit_code, exit.success, exit.tail.clone());
                parts.ledger.finish(run_uuid, &row)
            }
            LateFact::Stopped(ended_at_ms) => parts.ledger.stop(run_uuid, *ended_at_ms),
            LateFact::Session(session) => parts.ledger.set_session(run_uuid, session),
            LateFact::Summary(text, source) => parts.ledger.set_summary(run_uuid, text, *source),
        };
        if let Err(e) = ledger {
            log::warn!("[records] ledger update for run {run_uuid} failed: {e}");
        }
    }

    /// App exit: every open run becomes `stopped` NOW, synchronously, before
    /// the actors are killed — the pumps' own detached stop reports may not
    /// get to run before the process ends.
    pub fn stop_open_runs(&self, registry: &Registry) {
        let now = epoch_ms();
        for (record_id, run_uuid) in registry.open_runs() {
            self.run_stopped(&record_id, &run_uuid, now);
        }
    }

    /// The run history of a record, newest first.
    pub fn runs(&self, record_id: &str, limit: usize) -> Result<Vec<Run>, RecordError> {
        if self.locate(record_id).is_none() {
            return Err(RecordError::NotFound(format!("no such record: {record_id}")));
        }
        self.parts()
            .ledger
            .runs(record_id, limit.clamp(1, RUNS_RETAINED))
            .map_err(|e| RecordError::Internal(e.to_string()))
    }

    /// Remove a record and its history. An attached terminal is closed
    /// first. A command record is its definition, so removing it deletes
    /// the command (through the same path the editor's delete takes).
    pub fn remove(&self, registry: &Registry, app: Option<&AppHandle>, record_id: &str) -> Result<RemovedDto, RecordError> {
        let (scope, kind, name) = self
            .locate(record_id)
            .ok_or_else(|| RecordError::NotFound(format!("no such record: {record_id}")))?;
        let parts = self.parts();
        match (kind, scope) {
            (RecordKind::Command, Scope::Project(project_id)) => {
                let app = app
                    .cloned()
                    .or_else(|| parts.projects.app_handle().ok())
                    .ok_or_else(|| RecordError::Internal("app handle not initialized".into()))?;
                // The project runtime prunes the ledger itself (it holds the
                // record id while it deletes the override).
                parts.projects.delete_process(&app, registry, project_id, &name)?;
                Ok(RemovedDto { record_id: record_id.to_owned(), kind, removed: true, runs_deleted: 0 })
            }
            (RecordKind::Command, Scope::Workspace) => {
                let app = app
                    .cloned()
                    .or_else(|| parts.workspace.app_handle().ok())
                    .ok_or_else(|| RecordError::Internal("app handle not initialized".into()))?;
                parts.workspace.delete_command(&app, registry, &name)?;
                Ok(RemovedDto { record_id: record_id.to_owned(), kind, removed: true, runs_deleted: 0 })
            }
            (_, scope) => {
                if let Some((term_id, _)) = registry.term_of_record(record_id) {
                    registry.close(term_id);
                }
                match scope {
                    Scope::Project(_) => {
                        parts.projects.store_access(|s| s.remove_record(record_id))??;
                    }
                    Scope::Workspace => {
                        parts.workspace.store()?.remove_record(record_id)?;
                    }
                }
                let runs_deleted = parts
                    .ledger
                    .delete_records(&[record_id.to_owned()])
                    .map_err(|e| RecordError::Internal(e.to_string()))?;
                Ok(RemovedDto { record_id: record_id.to_owned(), kind, removed: true, runs_deleted })
            }
        }
    }

    /// Edit an agent record's row state (name, favorite, automatic
    /// renaming, notification level) wherever it is stored. A rename also
    /// renames the attached terminal, so a live row and its record never
    /// disagree. Answers the record as the list shows it.
    pub fn update_agent(
        &self,
        registry: &Registry,
        record_id: &str,
        patch: &AgentRecordPatch,
    ) -> Result<RecordDto, RecordError> {
        let (scope, kind, _) = self
            .locate(record_id)
            .ok_or_else(|| RecordError::NotFound(format!("no such record: {record_id}")))?;
        if kind != RecordKind::Agent {
            return Err(RecordError::Invalid(format!("not an agent record: {record_id}")));
        }
        patch.validate().map_err(RecordError::Invalid)?;
        let parts = self.parts();
        let updated = match scope {
            Scope::Project(_) => parts
                .projects
                .store_access(|s| s.update_agent_record(record_id, patch))??
                .map(|(_, agent)| agent),
            Scope::Workspace => parts.workspace.store()?.update_agent_record(record_id, patch)?,
        };
        let agent = updated.ok_or_else(|| RecordError::NotFound(format!("no such record: {record_id}")))?;
        if patch.name.is_some() {
            if let Some((term_id, _)) = registry.term_of_record(record_id) {
                registry.set_name(term_id, &agent.name);
            }
        }
        let (scope, record) = self
            .get(record_id)
            .ok_or_else(|| RecordError::NotFound(format!("no such record: {record_id}")))?;
        Ok(record_dto(registry, scope, record))
    }

    /// Start a fresh run of a stopped record: a new terminal, a new run
    /// uuid, the previous run left to the ledger. `frames` is the webview's
    /// channel when the frontend asked; `None` is a headless (control) start
    /// that reads back through `/output` and broadcasts `term://created` so
    /// the open webview adopts it. An agent starts a NEW session: its stored
    /// prompt is kept for display and never replayed.
    pub fn start(
        &self,
        deps: StartDeps<'_>,
        record_id: &str,
        frames: Option<Channel<InvokeResponseBody>>,
        wait_ms: u64,
    ) -> Result<StartDto, RecordError> {
        let (scope, record) = self
            .get(record_id)
            .ok_or_else(|| RecordError::NotFound(format!("no such record: {record_id}")))?;
        if let Some((term_id, head)) = deps.registry.term_of_record(record_id) {
            if matches!(head.status, ProcessStatus::Starting | ProcessStatus::Running) {
                // Already live: idempotent, like every other start.
                return Ok(StartDto {
                    record_id: record_id.to_owned(),
                    kind: record.kind,
                    term_id: Some(term_id),
                    run_uuid: Some(head.run_uuid),
                    pending: false,
                });
            }
            // An exited terminal still attached (its pane is readable) goes
            // when its record starts again: one terminal per record.
            deps.registry.close(term_id);
        }
        let parts = self.parts();
        let kind = record.kind;
        let (term_id, pending) = match (kind, scope) {
            (RecordKind::Agent, scope) => {
                let agent = record.agent.ok_or_else(|| RecordError::Internal("agent record without its facts".into()))?;
                let req = SpawnAgentRequest {
                    agent_tool_id: agent.tool_id,
                    project_id: scope.project_id(),
                    name: Some(agent.name),
                    extra_args: agent.extra_args,
                    record_id: Some(record_id.to_owned()),
                    ..SpawnAgentRequest::default()
                };
                let ctx = SpawnContext {
                    registry: deps.registry,
                    tools: deps.tools,
                    settings: deps.settings,
                    projects: &parts.projects,
                    records: self,
                    actor: None,
                };
                let (sink, on_notify): (Arc<dyn EventSink>, Option<AppHandle>) = match (frames, deps.app.clone()) {
                    (Some(frames), Some(app)) => (Arc::new(ChannelSink::new(frames, app.clone())), Some(app)),
                    _ => (Arc::new(NullSink), None),
                };
                let headless = on_notify.is_none();
                let resp = spawn_agent_with(ctx, req, sink, on_notify).map_err(RecordError::Internal)?;
                if headless {
                    if let Some(app) = deps.app.as_ref() {
                        emit_created(
                            &AppCreatedBroadcast(app.clone()),
                            CreatedEvent {
                                term_id: resp.process_id,
                                name: resp.name.clone(),
                                kind: "agent",
                                project_id: resp.agent.project_id,
                                agent_tool_id: Some(resp.agent.tool_id),
                                parent_process_id: resp.agent.parent_process_id,
                                record_id: Some(record_id.to_owned()),
                                last_run: deps.registry.run_head(resp.process_id),
                            },
                        );
                    }
                }
                (Some(resp.process_id), false)
            }
            (RecordKind::Terminal, scope) => {
                let terminal = record
                    .terminal
                    .ok_or_else(|| RecordError::Internal("terminal record without its facts".into()))?;
                let id = crate::commands::spawn_terminal_record(
                    deps.registry,
                    &deps.settings.snapshot(),
                    self,
                    scope,
                    &terminal,
                    frames,
                    deps.app.as_ref(),
                )?;
                (Some(id), false)
            }
            (RecordKind::Command, Scope::Project(project_id)) => match frames {
                Some(frames) => {
                    let app = deps
                        .app
                        .clone()
                        .ok_or_else(|| RecordError::Internal("app handle not initialized".into()))?;
                    let id = parts
                        .projects
                        .start_with_frames(&app, deps.registry, project_id, record.name, frames)?;
                    (Some(id), false)
                }
                None => {
                    let lifecycle = parts.projects.control_start(project_id, &record.name, wait_ms)?;
                    (lifecycle.term_id, lifecycle.pending)
                }
            },
            (RecordKind::Command, Scope::Workspace) => match frames {
                Some(frames) => {
                    let app = deps
                        .app
                        .clone()
                        .ok_or_else(|| RecordError::Internal("app handle not initialized".into()))?;
                    let id = parts.workspace.start_with_frames(&app, deps.registry, record.name, frames)?;
                    (Some(id), false)
                }
                None => {
                    return Err(RecordError::Unavailable(format!(
                        "workspace command `{}` starts from the app (its terminal needs the window's frames channel)",
                        record.name
                    )))
                }
            },
        };
        Ok(StartDto {
            record_id: record_id.to_owned(),
            kind,
            term_id,
            run_uuid: term_id.and_then(|id| deps.registry.uuid_of(id)),
            pending,
        })
    }
}

/// The late facts a run can receive.
enum LateFact {
    Exited(RunExit),
    Stopped(u64),
    Session(Session),
    Summary(String, SummarySource),
}

impl LateFact {
    fn apply(&self, run: &mut Run) {
        match self {
            LateFact::Exited(exit) => {
                run.tail_kind = exit.tail_kind;
                run.finish(exit.ended_at_ms, exit.exit_code, exit.success, exit.tail.clone());
            }
            LateFact::Stopped(ended_at_ms) => run.stop(*ended_at_ms),
            LateFact::Session(session) => run.set_session(session.clone()),
            LateFact::Summary(text, source) => run.set_summary(text.clone(), *source),
        }
    }
}

impl RunRecorder for Records {
    fn run_exited(&self, record_id: &str, run_uuid: &str, exit: RunExit) {
        self.apply_late_fact(record_id, run_uuid, &LateFact::Exited(exit));
    }

    fn run_stopped(&self, record_id: &str, run_uuid: &str, ended_at_ms: u64) {
        self.apply_late_fact(record_id, run_uuid, &LateFact::Stopped(ended_at_ms));
    }

    fn run_session(&self, record_id: &str, run_uuid: &str, session: Session) {
        self.apply_late_fact(record_id, run_uuid, &LateFact::Session(session));
    }

    fn run_summary(&self, record_id: &str, run_uuid: &str, text: &str, source: SummarySource) {
        self.apply_late_fact(record_id, run_uuid, &LateFact::Summary(text.to_owned(), source));
    }
}

// ---- resume ---------------------------------------------------------------------

impl Records {
    /// Resume an agent record: start it again with the PROVIDER continuing
    /// `session_id` (default: the last run's saved session), then watch the
    /// start. The agent comes back with its complete prior conversation —
    /// nothing is replayed from here. A record with nothing to resume
    /// answers [`ResumeFailure::NoSession`] so the caller can fall through to
    /// a plain start; a child that exits before it is ready was refused by
    /// the provider and comes back as [`ResumeFailure::Rejected`] with the
    /// provider's own text — never a silent fresh session.
    pub fn resume(
        &self,
        deps: StartDeps<'_>,
        record_id: &str,
        session_id: Option<String>,
        frames: Option<Channel<InvokeResponseBody>>,
    ) -> Result<ResumeDto, ResumeFailure> {
        let rid = || record_id.to_owned();
        let (scope, record) = self.get(record_id).ok_or_else(|| ResumeFailure::NotFound {
            record_id: rid(),
            message: format!("no such record: {record_id}"),
        })?;
        if record.kind != RecordKind::Agent {
            return Err(ResumeFailure::Unavailable {
                record_id: rid(),
                message: format!("record {record_id} is a {}, only agents have a session to resume", record.kind.as_str()),
            });
        }
        let agent = record.agent.ok_or_else(|| ResumeFailure::Internal {
            message: "agent record without its facts".into(),
        })?;
        let session_id = match session_id.filter(|id| !id.trim().is_empty()) {
            Some(id) => id,
            None => match record.last_run.as_ref().and_then(|run| run.session.as_ref()) {
                Some(session) => session.session_id.clone(),
                None => {
                    return Err(ResumeFailure::NoSession {
                        record_id: rid(),
                        message: format!("record {record_id} has no saved session to resume"),
                    })
                }
            },
        };
        let tool = deps.tools.get(agent.tool_id).ok_or_else(|| ResumeFailure::Unavailable {
            record_id: rid(),
            message: format!("agent tool {} of record {record_id} no longer exists", agent.tool_id),
        })?;
        let json_mode = match tool.transport {
            Transport::Json => machine_mode(tool.tool_type),
            Transport::Tty => None,
        };
        if let Some((term_id, head)) = deps.registry.term_of_record(record_id) {
            if matches!(head.status, ProcessStatus::Starting | ProcessStatus::Running) {
                return Err(ResumeFailure::Unavailable {
                    record_id: rid(),
                    message: format!("record {record_id} is running as terminal {term_id}; stop it before resuming"),
                });
            }
            deps.registry.close(term_id);
        }
        let parts = self.parts();
        let req = SpawnAgentRequest {
            agent_tool_id: agent.tool_id,
            project_id: scope.project_id(),
            name: Some(agent.name),
            extra_args: agent.extra_args,
            record_id: Some(rid()),
            resume: Some(ResumeSpec { session_id: session_id.clone() }),
            ..SpawnAgentRequest::default()
        };
        let ctx = SpawnContext {
            registry: deps.registry,
            tools: deps.tools,
            settings: deps.settings,
            projects: &parts.projects,
            records: self,
            actor: None,
        };
        let (sink, on_notify): (Arc<dyn EventSink>, Option<AppHandle>) = match (frames, deps.app.clone()) {
            (Some(frames), Some(app)) => (Arc::new(ChannelSink::new(frames, app.clone())), Some(app)),
            _ => (Arc::new(NullSink), None),
        };
        let headless = on_notify.is_none();
        let resp = spawn_agent_with(ctx, req, sink, on_notify).map_err(|e| {
            if e.contains("no resume command") {
                ResumeFailure::Unavailable { record_id: rid(), message: e }
            } else {
                ResumeFailure::Internal { message: e }
            }
        })?;
        let term_id = resp.process_id;
        let run_uuid = resp.agent.spawn_uuid.clone();
        if headless {
            if let Some(app) = deps.app.as_ref() {
                emit_created(
                    &AppCreatedBroadcast(app.clone()),
                    CreatedEvent {
                        term_id,
                        name: resp.name.clone(),
                        kind: "agent",
                        project_id: resp.agent.project_id,
                        agent_tool_id: Some(resp.agent.tool_id),
                        parent_process_id: resp.agent.parent_process_id,
                        record_id: Some(rid()),
                        last_run: deps.registry.run_head(term_id),
                    },
                );
            }
        }
        let settings = deps.settings.snapshot();
        let check = match json_mode {
            Some(mode) if !mode.persistent => Ok(ResumeCheck::Deferred),
            _ => self.watch_resume(
                deps.registry,
                term_id,
                u64::from(settings.agent_ready_quiet_ms),
                u64::from(settings.agent_ready_max_wait_ms),
            ),
        };
        let check = match check {
            Ok(check) => check,
            Err(()) => {
                // Refused: wait for the run's end (and its tail) to be
                // recorded, then hand the caller the provider's words.
                let deadline = Instant::now() + RESUME_TAIL_WAIT;
                let run = loop {
                    let run = self.get(record_id).and_then(|(_, r)| r.last_run).filter(|r| r.run_uuid == run_uuid);
                    match run {
                        Some(run) if !run.is_open() => break Some(run),
                        _ if Instant::now() >= deadline => break run,
                        _ => std::thread::sleep(Duration::from_millis(25)),
                    }
                };
                let exit_code = run.as_ref().and_then(|r| r.exit_code);
                let provider_text = run.as_ref().map(|r| rejection_text(&r.tail)).unwrap_or_default();
                let message = if provider_text.is_empty() {
                    format!("the provider exited before it was ready (exit code {exit_code:?}) and printed nothing")
                } else {
                    format!("the provider refused the resume: {provider_text}")
                };
                return Err(ResumeFailure::Rejected {
                    record_id: rid(),
                    run_uuid,
                    term_id,
                    exit_code,
                    provider_text,
                    message,
                });
            }
        };
        Ok(ResumeDto {
            record_id: rid(),
            kind: RecordKind::Agent,
            term_id,
            run_uuid,
            session_id,
            check,
        })
    }

    /// Watch a resumed child through the ready gate: output then quiet (or
    /// the max wait) means the provider took the session; an exit first
    /// means it refused (`Err`). A child that prints nothing for
    /// [`RESUME_SILENT_MS`] but is alive is accepted as silent.
    fn watch_resume(&self, registry: &Registry, term_id: TermId, quiet_ms: u64, max_wait_ms: u64) -> Result<ResumeCheck, ()> {
        let handle = registry.handle(term_id).ok_or(())?;
        let started = Instant::now();
        let mut first_output_ms: Option<u64> = None;
        loop {
            let view = IoView::of(&handle);
            let now = epoch_ms();
            if first_output_ms.is_none() && view.has_output {
                first_output_ms = Some(now);
            }
            let gate = ReadyGate { quiet_ms, max_wait_ms, first_output_ms };
            match readiness(view, now, gate) {
                Readiness::Exited => return Err(()),
                Readiness::Ready => return Ok(ResumeCheck::Quiet),
                Readiness::ReadyByTimeout => return Ok(ResumeCheck::ReadyByTimeout),
                Readiness::Pending if started.elapsed() >= Duration::from_millis(RESUME_SILENT_MS) => {
                    return Ok(ResumeCheck::Silent)
                }
                Readiness::Pending => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

/// The provider's own words out of a failed run's tail: the last few
/// non-empty text lines, or a json agent's error / stderr / stray lines.
fn rejection_text(tail: &[Value]) -> String {
    let mut lines: Vec<String> = tail
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            Value::Object(event) => {
                let payload = event.get("payload")?;
                match event.get("kind").and_then(Value::as_str) {
                    Some("error") => payload.get("message").map(|m| match m {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    }),
                    Some("raw") => payload
                        .get("stderr")
                        .or_else(|| payload.get("line"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                }
            }
            _ => None,
        })
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let skip = lines.len().saturating_sub(REJECTION_LINES);
    lines.drain(..skip);
    lines.join("\n")
}

/// One record's DTO: the live binding wins over the stored head while a
/// terminal is attached.
fn record_dto(registry: &Registry, scope: Scope, record: StoredRecord) -> RecordDto {
    let live = registry.term_of_record(&record.record_id);
    let (status, term_id, last_run) = match live {
        Some((term_id, head)) => (head.status, Some(term_id), Some(head)),
        None => {
            let head = record.last_run.as_ref().map(Run::head);
            let status = match head.as_ref().map(|h| h.status) {
                Some(ProcessStatus::Exited) => ProcessStatus::Exited,
                Some(ProcessStatus::Failed) => ProcessStatus::Failed,
                _ => ProcessStatus::Stopped,
            };
            (status, None, head)
        }
    };
    RecordDto {
        record_id: record.record_id,
        kind: record.kind,
        name: record.name,
        project_id: scope.project_id(),
        status,
        term_id,
        last_run,
        agent: record.agent.map(|a| AgentFacts {
            tool_id: a.tool_id,
            parent_record_id: a.parent_record_id,
            favorite: a.favorite,
            disable_auto_rename: a.disable_auto_rename,
            notification_level: a.notification_level,
            extra_args: a.extra_args,
            prompt: a.prompt,
        }),
        terminal: record.terminal.map(|t| TerminalFacts {
            shell_profile: t.shell_profile,
            cwd: t.cwd,
        }),
    }
}

// ---- control routes -------------------------------------------------------

fn record_id_of(params: &Params) -> Result<String, RecordError> {
    params
        .record_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| RecordError::Invalid("missing record id".into()))
}

fn fail(err: RecordError) -> Resp {
    json(err.status(), json!({"error": err.message()}))
}

fn args(query: &str, body: &str) -> Result<Args, Resp> {
    Args::from(query, body)
}

/// Default/maximum wait for a control-surface start of a project command.
const START_WAIT_DEFAULT_MS: u64 = 3_000;
const START_WAIT_MAX_MS: u64 = 15_000;

/// `GET /records/{record_id}/runs?limit=N`.
pub fn route_record_runs(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let result = record_id_of(params).and_then(|record_id| {
        let limit = a
            .usize("limit")
            .map_err(|e| RecordError::Invalid(e.to_string()))?
            .unwrap_or(RUNS_RETAINED);
        let runs = state.records.runs(&record_id, limit)?;
        Ok(json!({"record_id": record_id, "runs": runs}))
    });
    match result {
        Ok(v) => json(200, v),
        Err(err) => fail(err),
    }
}

/// `POST /records/{record_id}/start`.
pub fn route_start_record(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let result = record_id_of(params).and_then(|record_id| {
        let wait_ms = a
            .i64("wait_ms")
            .map_err(|e| RecordError::Invalid(e.to_string()))?
            .map(|w| u64::try_from(w).unwrap_or(0))
            .unwrap_or(START_WAIT_DEFAULT_MS)
            .min(START_WAIT_MAX_MS);
        let deps = StartDeps {
            registry: &state.registry,
            tools: &state.agent_tools,
            settings: &state.settings,
            app: state.app.clone(),
        };
        state.records.start(deps, &record_id, None, wait_ms)
    });
    match result {
        Ok(v) => json(200, json!(v)),
        Err(err) => fail(err),
    }
}

/// `POST /records/{record_id}/resume` (`session_id?`).
pub fn route_resume_record(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let result = record_id_of(params).map_err(ResumeFailure::from).and_then(|record_id| {
        let session_id = a.str("session_id").map(str::to_owned);
        let deps = StartDeps {
            registry: &state.registry,
            tools: &state.agent_tools,
            settings: &state.settings,
            app: state.app.clone(),
        };
        state.records.resume(deps, &record_id, session_id, None)
    });
    match result {
        Ok(v) => json(200, json!(v)),
        Err(err) => json(err.status(), err.to_json()),
    }
}

/// `POST /processes/{id}/session` (`session_id`): the process names the
/// conversation it is running — the self-targeting half of the announce
/// path (`set_session` over MCP, the actor header naming the caller).
pub fn route_set_session(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let id = params.id.unwrap_or_default();
    let session_id = match a.str("session_id").map(str::trim) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => return json(400, json!({"error": "invalid", "message": "session_id is required"})),
    };
    let Some(provider) = state.registry.session_provider(id) else {
        return json(404, json!({"error": "not_found", "message": format!("no such terminal: {id}")}));
    };
    let session = Session::new(provider, session_id, SessionSource::Mcp, epoch_ms());
    match state.registry.record_session(id, session.clone()) {
        Some((binding, changed)) => {
            if changed {
                if let Some(app) = state.app.as_ref() {
                    emit_session(app, id, binding.as_ref(), &session);
                }
            }
            json(
                200,
                json!({
                    "id": id,
                    "record_id": binding.as_ref().map(|b| b.record_id.clone()),
                    "run_uuid": binding.as_ref().map(|b| b.run_uuid.clone()),
                    "session": session,
                    "durable": binding.is_some(),
                }),
            )
        }
        None => json(404, json!({"error": "not_found", "message": format!("no such terminal: {id}")})),
    }
}

/// `POST /processes/{id}/summary` (`text`): the process posts what its run
/// was about — the control-surface twin of `OSC 777;summary` (`set_summary`
/// over MCP, the actor header naming the caller). The text is normalized
/// and capped exactly as the OSC path does; only a LIVE run takes it.
pub fn route_set_summary(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let id = params.id.unwrap_or_default();
    let raw = a.str("text").unwrap_or_default();
    let Some(text) = term_core::scanner::cap_summary(raw) else {
        return json(400, json!({"error": "invalid", "message": "text is required"}));
    };
    let truncated = raw.len() > term_core::scanner::SUMMARY_MAX_BYTES && text.ends_with(term_core::scanner::SUMMARY_TRUNCATED_MARKER);
    match state.registry.record_summary(id, text.clone(), SummarySource::Mcp) {
        SummaryOutcome::Stored { binding, changed } => {
            if changed {
                if let Some(app) = state.app.as_ref() {
                    emit_summary(app, id, binding.as_ref(), &text, SummarySource::Mcp);
                }
            }
            json(
                200,
                json!({
                    "id": id,
                    "record_id": binding.as_ref().map(|b| b.record_id.clone()),
                    "run_uuid": binding.as_ref().map(|b| b.run_uuid.clone()),
                    "summary": text,
                    "summary_source": SummarySource::Mcp,
                    "truncated": truncated,
                    "durable": binding.is_some(),
                }),
            )
        }
        SummaryOutcome::Ended => json(
            409,
            json!({"error": "not_running", "message": format!("terminal {id}'s run has ended; a finished run's summary cannot be changed")}),
        ),
        SummaryOutcome::Unknown => json(404, json!({"error": "not_found", "message": format!("no such terminal: {id}")})),
    }
}

/// `POST /records/{record_id}/remove` (confirm-gated: the history goes too).
pub fn route_remove_record(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    let a = match args(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let result = record_id_of(params).and_then(|record_id| {
        if a.bool("confirm").map_err(|e| RecordError::Invalid(e.to_string()))? != Some(true) {
            return Err(RecordError::Invalid("remove requires confirm=true".into()));
        }
        state.records.remove(&state.registry, state.app.as_ref(), &record_id)
    });
    match result {
        Ok(v) => json(200, json!(v)),
        Err(err) => fail(err),
    }
}

/// The `## Records` section of the control API doc.
pub const DOC: &str = "Every rail row that outlives its terminal has a RECORD: a stable `record_id` (a 32-hex uuid, \
never a terminal id — those restart at 1 every launch) that points at its CURRENT terminal only while \
one is live. Kinds: `command` (a project or workspace process definition), `agent` (the durable half \
of a spawn: tool, project, name, extra_args, the prompt it was spawned with — never replayed), \
`terminal` (a shell: profile + cwd). Project records live on the project's row in `projects.json`, \
project-less agents and shells in `workspace.json`; every row surface (`/processes`, `term://created`, \
the rail rows, project and workspace command rows) carries `record_id` and `last_run`.\n\n\
`last_run` is the head of the record's run ledger minus the tail: `{run_uuid, status: running | \
exited | failed | stopped, started_at_ms, ended_at_ms, exit_code, success, tail_kind: text | events, \
tail_len, summary?, summary_source?, session?}`. When a run's child exits the app captures an automatic tail — the \
last 200 screen lines (`text`), or for a json-transport agent the last 200 typed events (`events`) — \
and the row stays as `exited` / `failed`. Closing the row (×, `close_terminal`, project stop, app \
quit) leaves the record `stopped`; a run still `running` at launch was cut off by a crash and \
reconciles to `exited` with no exit code. Rows spawned with `close_on_exit` or `ephemeral: true` \
(and `spawn_terminal` / debug-harness terminals) have no record and vanish on close as before.\n\n\
The ledger (`runs` in coordination.db, schema version 4) keeps the last 20 runs per record. Every \
late fact about a run (its exit, its tail, later a summary or session id) carries the run uuid it \
was captured under and is dropped when the record has moved on to a newer run, so a respawn into \
the same terminal id or a close racing an exit can never clobber the current run.\n\n\
`GET /records/{record_id}/runs` answers `{record_id, runs: [Run…]}` newest first (`limit`, max 20), \
each run WITH its tail. `POST /records/{record_id}/start` spawns a fresh run (new terminal, new \
run uuid; an agent gets a NEW session with no prompt replay; a project command goes through the \
frontend like `start_process`, with `wait_ms` / `pending`; a workspace command answers 409 — it \
starts from the app) and answers `{record_id, kind, term_id, run_uuid, pending}`. `POST \
/records/{record_id}/remove` (confirm=true) closes any attached terminal, deletes the record and its \
ledger rows — for a command record that deletes the command definition — and answers `{record_id, \
kind, removed, runs_deleted}`. Removing a project removes its records' ledger rows the same way.\n\n\
**Sessions and resume.** `last_run.session` is the provider's own id for the conversation a run is \
(or was) having: `{provider, session_id, captured_at_ms, source: json | regex | announced | mcp | \
resume, resumed_from?}`. It is learned per agent tool according to its `session_capture` setting \
(`json` = the machine-mode adapter's typed id, copied onto the run on every turn_started and at exit; \
`regex` = a one-group pattern over every plain-text line the process prints, latest match wins, so an \
id printed at exit is the one kept; `announced` = only what the process says itself; `none`), and \
ALWAYS from the two announce paths any process can use: print `OSC 777;session;<id>` — \
`printf '\\033]777;session;%s\\007' \"$ID\"` — or call `set_session` over MCP (`POST \
/processes/{id}/session`, `{session_id}`; the MCP tool targets the caller's own process). Every \
sighting goes through the run-uuid rule; a change broadcasts `term://session {term_id, record_id, \
run_uuid, session}`. Only the id is stored, never a transcript — whether the conversation can be \
picked up again depends on the provider's own history.\n\n\
`POST /records/{record_id}/resume` (`session_id?`, default the last run's saved session) starts an \
agent record again with the PROVIDER continuing that session, so the agent comes back with its \
complete prior conversation; nothing is replayed from here. The tool's `resume_argv` (`{session_id}` \
substituted; claude `--resume <id>`, opencode `-s <id>`) goes before the tool's own args — after \
the machine-mode flags for a json tool; a per-turn json CLI is continued through its adapter instead. \
The start is then watched through the ready gate: output then quiet (or the max wait) = accepted, \
answering `{record_id, kind, term_id, run_uuid, session_id, check: quiet | ready_by_timeout | silent \
| deferred}` (`deferred` = a per-turn CLI, which can only refuse on the first message). A child that \
exits before it is ready was REFUSED by the provider: 409 `{error: resume_rejected, record_id, \
run_uuid, term_id, exit_code, provider_text, message}` with the provider's own words (the last lines \
of the failed run's tail, e.g. claude's `No conversation found with session ID: <id>`), and the \
failed run stays readable in the ledger — never a fresh session passed off as a resume. A record \
with nothing to resume answers 409 `{error: no_session}`; a running record, a non-agent, a missing \
tool or an empty `resume_argv` answer 409 `{error: resume_unavailable}`.\n\n\
**Run summaries.** `last_run.summary` is what the process itself said its run was about — \
markdown, shown on the row's record card above the tail and as a one-line strip above its exited \
pane — with `summary_source: osc | mcp` saying how it arrived. Two writers, both landing on the \
CURRENT run (last writer wins; the run-uuid rule applies): print `OSC 777;summary;<text>` — \
`printf '\\033]777;summary;%s\\007' \"build ok, 3 warnings\"`, literal newlines allowed in the \
text — or call `set_summary` over MCP (`POST /processes/{id}/summary`, `{text}`; the MCP tool \
targets the caller's own process — the path for json-transport agents, which have no terminal to \
print into). Both normalize the text the same way (CRLF to LF, surrounding blank lines trimmed) and \
cap it at 16 KiB, cutting a longer one with a `… (summary truncated)` marker. A summary printed just \
before the process exits still lands; once the run has ended it is no longer editable (the route \
answers 409 `{error: not_running}`). A change broadcasts `term://summary {term_id, record_id, \
run_uuid, summary, summary_source}`. The ledger keeps each run's summary with the run.\n";

// ---- Tauri commands -------------------------------------------------------

/// Every stored record with its live binding — the rail's stopped rows.
#[tauri::command]
pub fn list_records(records: State<'_, Records>, registry: State<'_, Registry>) -> Vec<RecordDto> {
    records.list(&registry)
}

/// A record's run history, newest first (each run with its tail).
#[tauri::command]
pub fn get_record_runs(records: State<'_, Records>, record_id: String, limit: Option<usize>) -> Result<Vec<Run>, String> {
    Ok(records.runs(&record_id, limit.unwrap_or(RUNS_RETAINED))?)
}

/// Start a stopped record into this webview (frames channel attached).
/// Async + `spawn_blocking`: an agent start can talk to docker and wait on
/// the ready gate.
#[tauri::command]
pub async fn start_record(
    app: AppHandle,
    registry: State<'_, Registry>,
    tools: State<'_, AgentToolsState>,
    settings: State<'_, SettingsState>,
    records: State<'_, Records>,
    record_id: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<StartDto, String> {
    let (registry, tools, settings, records) = (
        registry.inner().clone(),
        tools.inner().clone(),
        settings.inner().clone(),
        records.inner().clone(),
    );
    tauri::async_runtime::spawn_blocking(move || {
        let deps = StartDeps {
            registry: &registry,
            tools: &tools,
            settings: &settings,
            app: Some(app),
        };
        records
            .start(deps, &record_id, Some(frames), START_WAIT_DEFAULT_MS)
            .map_err(String::from)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Resume an agent record into this webview (frames channel attached):
/// the provider continues the saved session (or `session_id`). Rejects with
/// a [`ResumeFailure`] object — `kind: no_session` means fall through to a
/// plain start, `kind: resume_rejected` carries the provider's text. Async +
/// `spawn_blocking`: the start is watched through the ready gate.
#[tauri::command]
pub async fn resume_record(
    app: AppHandle,
    registry: State<'_, Registry>,
    tools: State<'_, AgentToolsState>,
    settings: State<'_, SettingsState>,
    records: State<'_, Records>,
    record_id: String,
    session_id: Option<String>,
    frames: Channel<InvokeResponseBody>,
) -> Result<ResumeDto, ResumeFailure> {
    let (registry, tools, settings, records) = (
        registry.inner().clone(),
        tools.inner().clone(),
        settings.inner().clone(),
        records.inner().clone(),
    );
    tauri::async_runtime::spawn_blocking(move || {
        let deps = StartDeps {
            registry: &registry,
            tools: &tools,
            settings: &settings,
            app: Some(app),
        };
        records.resume(deps, &record_id, session_id, Some(frames))
    })
    .await
    .map_err(|e| ResumeFailure::Internal { message: e.to_string() })?
}

/// Edit an agent record's row state. Every field of `patch` is optional;
/// `notification_level: null` clears the level (the row inherits).
#[tauri::command]
pub fn update_agent_record(
    registry: State<'_, Registry>,
    records: State<'_, Records>,
    record_id: String,
    patch: AgentRecordPatch,
) -> Result<RecordDto, String> {
    Ok(records.update_agent(&registry, &record_id, &patch)?)
}

/// Remove a record and its history (closing its terminal first). Async: a
/// docker-exec agent's close blocks on the container-side verification.
#[tauri::command]
pub async fn remove_record(
    app: AppHandle,
    registry: State<'_, Registry>,
    records: State<'_, Records>,
    record_id: String,
) -> Result<RemovedDto, String> {
    let (registry, records) = (registry.inner().clone(), records.inner().clone());
    tauri::async_runtime::spawn_blocking(move || {
        records
            .remove(&registry, Some(&app), &record_id)
            .map_err(String::from)
    })
    .await
    .map_err(|e| e.to_string())?
}
