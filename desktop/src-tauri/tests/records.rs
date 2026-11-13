//! Durable row records end to end:
//!  - a run's exit lands on its record (status, exit code, the automatic
//!    tail) and in the ledger, before anything else sees the exit;
//!  - a late fact for a stale run is dropped from the record head (and never
//!    overwrites a run that already ended) while the ledger keeps history;
//!  - a close leaves the record `stopped`; a remove deletes it with its runs;
//!  - a start assigns a new run uuid and a new terminal id, idempotent on a
//!    live one;
//!  - a run still open at launch is reconciled as a crash;
//!  - the ledger keeps the newest twenty runs per record;
//!  - agent spawns create records (parent by RECORD id), `ephemeral` opts
//!    out, and the three control routes answer as documented.
//!
//! Real terminals: `cmd` / `powershell` on Windows, `sh` elsewhere (the
//! split every pty-touching test in this crate uses).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::agents::{spawn_agent_headless, SpawnAgentRequest, SpawnContext};
use app_lib::control_http::{self, ControlState};
use app_lib::coordination::Coordination;
use app_lib::projects::Projects;
use app_lib::records::{Records, Scope, StartDeps};
use app_lib::registry::{
    EventSink, JsonEvent, RecordBinding, Registry, RunExit, RunRecorder,
};
use app_lib::settings::SettingsState;
use app_lib::workspace::Workspace;
use project_model::projects::ProjectStore;
use project_model::records::{
    AgentRecord, RecordKind, Run, TailKind, TerminalRecord, RUNS_RETAINED, TAIL_LINES,
};
use project_model::workspace::WorkspaceStore;
use serde_json::{json, Value};
use term_core::actor::ActorConfig;
use term_core::pty::PtySpec;
use term_core::status::ProcessStatus;

#[derive(Default)]
struct MockSink {
    events: Mutex<Vec<JsonEvent>>,
}

impl EventSink for MockSink {
    fn send_binary(&self, _: Vec<u8>) {}
    fn emit_json(&self, event: JsonEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A child that prints `hello` and exits 0.
#[cfg(windows)]
fn echo_exit_spec() -> PtySpec {
    PtySpec {
        command: "cmd.exe".into(),
        args: vec!["/c".into(), "echo hello".into()],
        cwd: None,
        env: Vec::new(),
        cols: 40,
        rows: 6,
    }
}

#[cfg(not(windows))]
fn echo_exit_spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "echo hello".into()],
        cwd: None,
        env: Vec::new(),
        cols: 40,
        rows: 6,
    }
}

/// A child that stays alive until killed.
#[cfg(windows)]
fn lingering_spec() -> PtySpec {
    PtySpec {
        command: "powershell.exe".into(),
        args: vec!["-NoProfile".into(), "-Command".into(), "Start-Sleep -Seconds 30".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

#[cfg(not(windows))]
fn lingering_spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "sleep 30".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

/// An agent program that prints its last argument and exits.
#[cfg(windows)]
fn exiting_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/C".into(), "echo".into()])
}

#[cfg(not(windows))]
fn exiting_tool() -> (String, Vec<String>) {
    ("sh".into(), vec!["-c".into(), "echo \"$1\"".into(), "sh".into()])
}

fn cfg(spec: PtySpec) -> ActorConfig {
    ActorConfig {
        spec,
        scrollback_lines: 1000,
        ..ActorConfig::default()
    }
}

fn wait_until<T>(mut pred: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = pred() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Stores in a tempdir, an in-memory ledger, and a registry that reports its
/// runs to them.
struct World {
    _dir: tempfile::TempDir,
    registry: Registry,
    records: Records,
    projects: Projects,
    workspace: Workspace,
    tools: AgentToolsState,
    settings: SettingsState,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    store.add(dir.path().join("proj"), "proj".into(), None).unwrap();
    let projects = Projects::with_store(store);
    let workspace = Workspace::with_store(WorkspaceStore::new(dir.path().join("workspace.json")));
    let records = Records::new(projects.clone(), workspace.clone(), Coordination::in_memory().unwrap());
    let registry = Registry::default();
    registry.set_run_recorder(Arc::new(records.clone()));
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    World {
        _dir: dir,
        registry,
        records,
        projects,
        workspace,
        tools,
        settings: SettingsState::default(),
    }
}

fn terminal_record(id: &str) -> TerminalRecord {
    TerminalRecord {
        record_id: id.into(),
        name: "shell".into(),
        shell_profile: None,
        cwd: None,
        last_run: None,
    }
}

/// Spawn a terminal for an existing record the way the app does: run begun
/// in the store first, binding passed into the spawn.
fn spawn_for(w: &World, record_id: &str, run_uuid: &str, spec: PtySpec) -> u32 {
    w.records
        .begin_run(Scope::Workspace, RecordKind::Terminal, record_id, &Run::begin(run_uuid, 1, TailKind::Text))
        .unwrap();
    w.registry
        .create_recorded_terminal(
            cfg(spec),
            Arc::new(MockSink::default()),
            None,
            "shell".into(),
            RecordBinding { record_id: record_id.into(), run_uuid: run_uuid.into() },
        )
        .expect("spawn")
}

fn head_status(w: &World, record_id: &str) -> Option<ProcessStatus> {
    w.records.get(record_id).and_then(|(_, r)| r.last_run.map(|run| run.status))
}

fn host_tool(tools: &AgentToolsState, name: &str) -> AgentTool {
    let (program, args) = exiting_tool();
    tools
        .upsert(AgentTool {
            id: 0,
            name: name.into(),
            tool_type: ToolType::Custom,
            program,
            args: [args, vec!["probe".into()]].concat(),
            model: None,
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: Default::default(),
        })
        .unwrap()
}

fn spawn_ctx(w: &World) -> SpawnContext<'_> {
    SpawnContext {
        registry: &w.registry,
        tools: &w.tools,
        settings: &w.settings,
        projects: &w.projects,
        records: &w.records,
        actor: None,
    }
}

/// The child exits: the record head carries exited / code / success and the
/// automatic text tail, the ledger row agrees, and every row surface names
/// the record. The exited terminal is still attached (readable), so the live
/// head wins on the rows.
#[test]
fn exit_writes_the_tail_onto_the_record_and_the_ledger() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-exit"))
        .unwrap();
    let id = spawn_for(&w, "t-exit", "run-1", echo_exit_spec());

    let run = wait_until(|| {
        w.records
            .get("t-exit")
            .and_then(|(_, r)| r.last_run)
            .filter(|run| !run.is_open())
    });
    assert_eq!(run.status, ProcessStatus::Exited);
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.success, Some(true));
    assert!(run.ended_at_ms.is_some());
    assert_eq!(run.tail_kind, TailKind::Text);
    assert!(
        run.tail.iter().any(|line| line.as_str().is_some_and(|s| s.contains("hello"))),
        "tail must hold the output: {:?}",
        run.tail
    );
    assert!(run.tail.len() <= TAIL_LINES);
    assert!(
        !run.tail.last().unwrap().as_str().unwrap().trim().is_empty(),
        "trailing blank screen rows are trimmed"
    );

    let runs = w.records.runs("t-exit", 20).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_uuid, "run-1");
    assert_eq!(runs[0].status, ProcessStatus::Exited);
    assert_eq!(runs[0].tail, run.tail, "the ledger row holds the same tail");

    // Row surfaces: the rail row and the control row both name the record.
    let rows = w.registry.list_rows();
    let row = rows.iter().find(|r| r.base.id == id).unwrap();
    assert_eq!(row.record_id.as_deref(), Some("t-exit"));
    let head = row.last_run.as_ref().unwrap();
    assert_eq!(head.run_uuid, "run-1");
    assert_eq!(head.exit_code, Some(0));
    let control = w.registry.control_snapshot(id).unwrap();
    assert_eq!(control.record_id.as_deref(), Some("t-exit"));
    assert_eq!(control.last_run.as_ref().unwrap().status, ProcessStatus::Exited);
    // The record list shows the exited terminal still attached.
    let listed = w.records.list(&w.registry);
    let entry = listed.iter().find(|r| r.record_id == "t-exit").unwrap();
    assert_eq!(entry.term_id, Some(id));
    assert_eq!(entry.status, ProcessStatus::Exited);
    assert_eq!(entry.kind, RecordKind::Terminal);
    w.registry.close(id);
}

/// The run-uuid rule, exercised through the recorder: a fact for an older
/// run never touches the current head (its own ledger row still takes it),
/// and a second ending for the same run is dropped — first writer wins.
#[test]
fn a_late_fact_for_a_stale_run_is_dropped_from_the_head() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-stale"))
        .unwrap();
    w.records
        .begin_run(Scope::Workspace, RecordKind::Terminal, "t-stale", &Run::begin("old", 1, TailKind::Text))
        .unwrap();
    w.records
        .begin_run(Scope::Workspace, RecordKind::Terminal, "t-stale", &Run::begin("new", 2, TailKind::Text))
        .unwrap();
    assert_eq!(head_status(&w, "t-stale"), Some(ProcessStatus::Running));

    // A late exit for the OLD run: head untouched, ledger row for `old` ends.
    w.records.run_exited(
        "t-stale",
        "old",
        RunExit { ended_at_ms: 3, exit_code: Some(1), success: false, tail_kind: TailKind::Text, tail: vec![json!("late")] },
    );
    let (_, record) = w.records.get("t-stale").unwrap();
    let head = record.last_run.unwrap();
    assert_eq!((head.run_uuid.as_str(), head.status), ("new", ProcessStatus::Running));
    let runs = w.records.runs("t-stale", 20).unwrap();
    assert_eq!(runs.iter().map(|r| r.run_uuid.as_str()).collect::<Vec<_>>(), vec!["new", "old"], "newest first");
    assert_eq!(runs[1].status, ProcessStatus::Failed);
    assert_eq!(runs[1].tail, vec![json!("late")]);

    // The current run stops; a later exit for the same run is stale.
    w.records.run_stopped("t-stale", "new", 4);
    assert_eq!(head_status(&w, "t-stale"), Some(ProcessStatus::Stopped));
    w.records.run_exited(
        "t-stale",
        "new",
        RunExit { ended_at_ms: 5, exit_code: Some(0), success: true, tail_kind: TailKind::Text, tail: vec![] },
    );
    assert_eq!(head_status(&w, "t-stale"), Some(ProcessStatus::Stopped));
    let runs = w.records.runs("t-stale", 20).unwrap();
    assert_eq!(runs[0].status, ProcessStatus::Stopped, "the ledger row keeps the first ending too");
    assert_eq!(runs[0].ended_at_ms, Some(4));
    // Facts for an unknown record are ignored, never an error.
    w.records.run_stopped("nobody", "x", 1);
}

/// Closing a live terminal leaves its record `stopped` (reported after the
/// pump ends); removing the record deletes it and its ledger rows.
#[test]
fn close_leaves_the_record_stopped_and_remove_deletes_its_history() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-close"))
        .unwrap();
    let id = spawn_for(&w, "t-close", "run-1", lingering_spec());
    assert_eq!(w.registry.term_of_record("t-close").map(|(t, _)| t), Some(id));
    w.registry.close(id);
    let run = wait_until(|| {
        w.records
            .get("t-close")
            .and_then(|(_, r)| r.last_run)
            .filter(|run| !run.is_open())
    });
    assert_eq!(run.status, ProcessStatus::Stopped);
    assert_eq!((run.exit_code, run.success), (None, None));
    assert!(run.ended_at_ms.is_some());
    assert!(w.registry.term_of_record("t-close").is_none());
    let listed = w.records.list(&w.registry);
    let entry = listed.iter().find(|r| r.record_id == "t-close").unwrap();
    assert_eq!((entry.status, entry.term_id), (ProcessStatus::Stopped, None));
    assert_eq!(w.records.runs("t-close", 20).unwrap()[0].status, ProcessStatus::Stopped);

    let removed = w.records.remove(&w.registry, None, "t-close").unwrap();
    assert_eq!((removed.removed, removed.runs_deleted, removed.kind), (true, 1, RecordKind::Terminal));
    assert!(w.records.get("t-close").is_none());
    assert!(w.records.runs("t-close", 20).is_err(), "no record, no history");
    assert!(w.records.remove(&w.registry, None, "t-close").is_err());
    // Removing a record whose terminal is LIVE closes it first.
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-live"))
        .unwrap();
    let live = spawn_for(&w, "t-live", "run-l", lingering_spec());
    w.records.remove(&w.registry, None, "t-live").unwrap();
    assert!(w.registry.handle(live).is_none(), "the live terminal was closed");
}

/// `start` on a stopped record spawns a new terminal under a NEW run uuid
/// (the previous run stays in the ledger), is idempotent on a live one, and
/// replaces an exited-but-attached terminal.
#[test]
fn start_assigns_a_new_run_uuid_and_a_new_terminal() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-start"))
        .unwrap();
    let deps = || StartDeps {
        registry: &w.registry,
        tools: &w.tools,
        settings: &w.settings,
        app: None,
    };
    let first = w.records.start(deps(), "t-start", None, 0).unwrap();
    let (t1, u1) = (first.term_id.unwrap(), first.run_uuid.clone().unwrap());
    assert_eq!(first.kind, RecordKind::Terminal);
    assert!(!first.pending);
    assert_eq!(w.registry.term_of_record("t-start").map(|(t, _)| t), Some(t1));
    assert_eq!(w.records.get("t-start").unwrap().1.last_run.unwrap().run_uuid, u1);

    // Live: idempotent, the same terminal and run.
    let again = w.records.start(deps(), "t-start", None, 0).unwrap();
    assert_eq!((again.term_id, again.run_uuid.as_deref()), (Some(t1), Some(u1.as_str())));

    w.registry.close(t1);
    wait_until(|| (head_status(&w, "t-start") == Some(ProcessStatus::Stopped)).then_some(()));
    let second = w.records.start(deps(), "t-start", None, 0).unwrap();
    let (t2, u2) = (second.term_id.unwrap(), second.run_uuid.unwrap());
    assert_ne!(t2, t1, "a fresh terminal id");
    assert_ne!(u2, u1, "a fresh run uuid");
    let runs = w.records.runs("t-start", 20).unwrap();
    assert_eq!(runs.iter().map(|r| r.run_uuid.clone()).collect::<Vec<_>>(), vec![u2.clone(), u1.clone()]);
    assert_eq!(runs[1].status, ProcessStatus::Stopped, "the previous run is history, untouched");
    w.registry.close(t2);
    assert!(w.records.start(deps(), "no-such", None, 0).is_err());
}

/// A shell opened under a project is filed in that project's terminals
/// bucket, and the live terminal it starts is attributed to the project, so
/// its row sits under the project both while running and after a restart.
#[test]
fn a_project_shell_record_lives_with_its_project() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Project(1), terminal_record("t-proj"))
        .unwrap();
    let (scope, stored) = w.records.get("t-proj").unwrap();
    assert_eq!((scope, stored.kind), (Scope::Project(1), RecordKind::Terminal));
    let projects_file = std::fs::read_to_string(w._dir.path().join("projects.json")).unwrap();
    assert!(projects_file.contains("t-proj"), "{projects_file}");
    let workspace_file = std::fs::read_to_string(w._dir.path().join("workspace.json")).unwrap_or_default();
    assert!(!workspace_file.contains("t-proj"), "{workspace_file}");

    let started = w
        .records
        .start(
            StartDeps { registry: &w.registry, tools: &w.tools, settings: &w.settings, app: None },
            "t-proj",
            None,
            0,
        )
        .unwrap();
    let term_id = started.term_id.unwrap();
    let row = w.registry.list_rows().into_iter().find(|r| r.base.id == term_id).expect("a live row");
    assert_eq!((row.kind, row.project_id), ("terminal", Some(1)));
    let listed = w.records.list(&w.registry).into_iter().find(|r| r.record_id == "t-proj").unwrap();
    assert_eq!((listed.project_id, listed.term_id), (Some(1), Some(term_id)));
    w.registry.close(term_id);
}

/// A run still `running` in the stores and the ledger when the service
/// starts was cut off by a crash: it ends as `exited` with no exit code and
/// its tail intact, on the record and in the ledger.
#[test]
fn a_run_left_open_by_a_crash_is_reconciled_at_launch() {
    let dir = tempfile::tempdir().unwrap();
    let coordination = Coordination::in_memory().unwrap();
    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    store.add(dir.path().join("p"), "p".into(), None).unwrap();
    let mut open = Run::begin("crashed", 1, TailKind::Events);
    open.tail = vec![json!({"kind": "text"})];
    store
        .add_agent_record(
            1,
            AgentRecord {
                record_id: "a-crash".into(),
                name: "worker".into(),
                tool_id: 1,
                favorite: false,
                disable_auto_rename: false,
                notification_level: None,
                parent_record_id: None,
                extra_args: vec![],
                prompt: None,
                last_run: Some(open.clone()),
            },
        )
        .unwrap();
    let ws = WorkspaceStore::new(dir.path().join("workspace.json"));
    ws.add_terminal_record(TerminalRecord {
        record_id: "t-crash".into(),
        name: "shell".into(),
        shell_profile: None,
        cwd: None,
        last_run: Some(Run::begin("crashed-too", 1, TailKind::Text)),
    })
    .unwrap();
    // The ledger rows a previous session left behind.
    {
        let first = Records::new(Projects::default(), Workspace::default(), coordination.clone());
        first.ledger().insert("a-crash", Some(1), RecordKind::Agent, &open).unwrap();
        first
            .ledger()
            .insert("t-crash", None, RecordKind::Terminal, &Run::begin("crashed-too", 1, TailKind::Text))
            .unwrap();
    }

    let records = Records::new(Projects::with_store(store), Workspace::with_store(ws), coordination);
    let (_, agent) = records.get("a-crash").unwrap();
    let run = agent.last_run.unwrap();
    assert_eq!(run.status, ProcessStatus::Exited);
    assert_eq!((run.exit_code, run.success), (None, None));
    assert_eq!(run.tail.len(), 1, "whatever tail it had");
    let (_, terminal) = records.get("t-crash").unwrap();
    assert_eq!(terminal.last_run.unwrap().status, ProcessStatus::Exited);
    for id in ["a-crash", "t-crash"] {
        let runs = records.runs(id, 20).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ProcessStatus::Exited, "{id}");
        assert_eq!(runs[0].exit_code, None, "{id}");
    }
    // The listing shows them as exited rows with no terminal.
    let registry = Registry::default();
    let listed = records.list(&registry);
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|r| r.status == ProcessStatus::Exited && r.term_id.is_none()));
}

/// The ledger keeps the newest [`RUNS_RETAINED`] runs per record.
#[test]
fn the_ledger_keeps_the_newest_twenty_runs() {
    let w = world();
    w.records
        .add_terminal_record(Scope::Workspace, terminal_record("t-prune"))
        .unwrap();
    for i in 0..(RUNS_RETAINED + 5) {
        w.records
            .begin_run(
                Scope::Workspace,
                RecordKind::Terminal,
                "t-prune",
                &Run::begin(format!("run-{i:02}"), 100 + i as u64, TailKind::Text),
            )
            .unwrap();
    }
    let runs = w.records.runs("t-prune", 50).unwrap();
    assert_eq!(runs.len(), RUNS_RETAINED, "the limit is clamped to the retention");
    assert_eq!(runs[0].run_uuid, format!("run-{:02}", RUNS_RETAINED + 4), "newest first");
    assert_eq!(runs[RUNS_RETAINED - 1].run_uuid, "run-05", "the five oldest are gone");
    assert_eq!(w.records.runs("t-prune", 3).unwrap().len(), 3);
}

/// An agent spawn creates a record in its scope (project row or workspace
/// bucket) with the parent recorded by RECORD id; `ephemeral` and
/// `close_on_exit` opt out; the exit lands on the record with the tail; a
/// start of the stopped record spawns a new session without the prompt.
#[test]
fn agent_spawns_create_records_and_resolve_parents_by_record_id() {
    let w = world();
    let tool = host_tool(&w.tools, "echoer");
    let parent = spawn_agent_headless(
        spawn_ctx(&w),
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            name: Some("parent".into()),
            prompt: Some("remember me".into()),
            ..SpawnAgentRequest::default()
        },
    )
    .expect("parent spawn");
    let parent_record = parent.record_id.clone().expect("a record");
    let (scope, stored) = w.records.get(&parent_record).unwrap();
    assert_eq!(scope, Scope::Workspace, "project-less agents live in the workspace bucket");
    let facts = stored.agent.clone().unwrap();
    assert_eq!(facts.prompt.as_deref(), Some("remember me"));
    assert_eq!(facts.tool_id, tool.id);
    assert_eq!(stored.last_run.as_ref().unwrap().run_uuid, parent.agent.spawn_uuid);
    let workspace_file = std::fs::read_to_string(w._dir.path().join("workspace.json")).unwrap();
    assert!(workspace_file.contains("\"agents\""), "{workspace_file}");

    // A child spawned under the parent's TERMINAL id records the parent's RECORD id.
    let child = spawn_agent_headless(
        spawn_ctx(&w),
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            project_id: Some(1),
            parent_process_id: Some(parent.process_id),
            ..SpawnAgentRequest::default()
        },
    )
    .expect("child spawn");
    let child_record = child.record_id.clone().unwrap();
    let (scope, stored) = w.records.get(&child_record).unwrap();
    assert_eq!(scope, Scope::Project(1), "a project-bound agent lives on its project row");
    assert_eq!(stored.agent.unwrap().parent_record_id.as_deref(), Some(parent_record.as_str()));
    // The row surfaces carry the record.
    let row = w.registry.control_snapshot(child.process_id).unwrap();
    assert_eq!(row.record_id.as_deref(), Some(child_record.as_str()));
    assert_eq!(row.last_run.as_ref().unwrap().run_uuid, child.agent.spawn_uuid);

    // Opt-outs leave no record; a child of a record-less parent has no parent record.
    let ephemeral = spawn_agent_headless(
        spawn_ctx(&w),
        SpawnAgentRequest { agent_tool_id: tool.id, ephemeral: true, ..SpawnAgentRequest::default() },
    )
    .unwrap();
    assert_eq!(ephemeral.record_id, None);
    assert_eq!(w.registry.record_of(ephemeral.process_id), None);
    let self_reaping = spawn_agent_headless(
        spawn_ctx(&w),
        SpawnAgentRequest { agent_tool_id: tool.id, close_on_exit: true, ..SpawnAgentRequest::default() },
    )
    .unwrap();
    assert_eq!(self_reaping.record_id, None, "close_on_exit implies ephemeral");
    let orphan = spawn_agent_headless(
        spawn_ctx(&w),
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            parent_process_id: Some(ephemeral.process_id),
            ..SpawnAgentRequest::default()
        },
    )
    .unwrap();
    let (_, stored) = w.records.get(orphan.record_id.as_deref().unwrap()).unwrap();
    assert_eq!(stored.agent.unwrap().parent_record_id, None);

    // The echo tool exits: the parent's record ends with the echoed tail.
    let run = wait_until(|| {
        w.records
            .get(&parent_record)
            .and_then(|(_, r)| r.last_run)
            .filter(|run| !run.is_open())
    });
    assert_eq!(run.status, ProcessStatus::Exited);
    assert!(run.tail.iter().any(|l| l.as_str().is_some_and(|s| s.contains("probe"))), "{:?}", run.tail);

    // Start the stopped record again: a fresh session (new run, new
    // terminal), the stored prompt kept for display and NOT replayed.
    let started = w
        .records
        .start(
            StartDeps { registry: &w.registry, tools: &w.tools, settings: &w.settings, app: None },
            &parent_record,
            None,
            0,
        )
        .unwrap();
    assert_eq!(started.kind, RecordKind::Agent);
    let new_term = started.term_id.unwrap();
    assert_ne!(new_term, parent.process_id);
    assert_ne!(started.run_uuid.as_deref(), Some(parent.agent.spawn_uuid.as_str()));
    assert_eq!(w.registry.record_of(new_term).as_deref(), Some(parent_record.as_str()));
    assert!(
        w.registry.input_journal(new_term).unwrap().is_empty(),
        "nothing was typed into the new session"
    );
    let (_, stored) = w.records.get(&parent_record).unwrap();
    assert_eq!(stored.agent.unwrap().prompt.as_deref(), Some("remember me"), "kept for display");
    wait_until(|| (head_status(&w, &parent_record) == Some(ProcessStatus::Exited)).then_some(()));
    assert_eq!(w.records.runs(&parent_record, 20).unwrap().len(), 2);
    // The exited-but-attached terminal goes when the record starts again.
    let third = w
        .records
        .start(
            StartDeps { registry: &w.registry, tools: &w.tools, settings: &w.settings, app: None },
            &parent_record,
            None,
            0,
        )
        .unwrap();
    assert!(w.registry.handle(new_term).is_none(), "one terminal per record");
    wait_until(|| (head_status(&w, &parent_record) == Some(ProcessStatus::Exited)).then_some(()));
    for id in [child.process_id, ephemeral.process_id, orphan.process_id, third.term_id.unwrap()] {
        w.registry.close(id);
    }
}

// ---- control routes ----------------------------------------------------------

fn http(addr: &str, method: &str, path: &str, token: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("status line");
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

fn start_control(state: ControlState) -> (String, String) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let token = "t0ken-for-tests".to_owned();
    let t = token.clone();
    std::thread::spawn(move || control_http::serve(server, state, t));
    (addr, token)
}

/// The three routes: history (with the tail, newest first, 404 unknown),
/// start (a new run and terminal; 404 unknown), remove (confirm-gated, the
/// history goes with the record). Spawning through `/agents` creates the
/// record the routes then act on, and the `/processes` row names it.
#[test]
fn record_routes_answer_history_start_and_remove() {
    let w = world();
    let tool = host_tool(&w.tools, "echoer");
    let (addr, token) = start_control(ControlState {
        registry: w.registry.clone(),
        records: w.records.clone(),
        agent_tools: w.tools.clone(),
        settings: w.settings.clone(),
        projects: w.projects.clone(),
        ..ControlState::default()
    });

    let (status, spawned) = http(&addr, "POST", "/agents", &token, &json!({"agent_tool_id": tool.id, "name": "routed"}).to_string());
    assert_eq!(status, 200, "{spawned}");
    let record_id = spawned["record_id"].as_str().expect("record_id on the spawn reply").to_owned();
    let term = spawned["process_id"].as_u64().unwrap();
    let (status, row) = http(&addr, "GET", &format!("/processes/{term}"), &token, "");
    assert_eq!(status, 200);
    assert_eq!(row["record_id"], json!(record_id));
    assert_eq!(row["last_run"]["run_uuid"], row["uuid"], "the run uuid IS the spawn uuid");
    assert!(row["last_run"].get("tail").is_none(), "rows carry the head, never the tail");

    // The echo tool exits; the history route answers the finished run with its tail.
    let runs = wait_until(|| {
        let (status, body) = http(&addr, "GET", &format!("/records/{record_id}/runs"), &token, "");
        assert_eq!(status, 200, "{body}");
        let runs = body["runs"].as_array().unwrap().clone();
        (runs[0]["status"] != "running").then_some(runs)
    });
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "exited");
    assert_eq!(runs[0]["tail_kind"], "text");
    assert!(runs[0]["tail"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("probe")), "{}", runs[0]);
    let (status, limited) = http(&addr, "GET", &format!("/records/{record_id}/runs?limit=1"), &token, "");
    assert_eq!(status, 200);
    assert_eq!(limited["record_id"], json!(record_id));
    assert_eq!(limited["runs"].as_array().unwrap().len(), 1);
    let (status, _) = http(&addr, "GET", "/records/no-such-record/runs", &token, "");
    assert_eq!(status, 404);

    // Start: a fresh run on a new terminal; the previous run stays.
    let (status, started) = http(&addr, "POST", &format!("/records/{record_id}/start"), &token, "{}");
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["kind"], "agent");
    assert_eq!(started["pending"], false);
    let new_term = started["term_id"].as_u64().unwrap();
    assert_ne!(new_term, term);
    assert_ne!(started["run_uuid"], row["uuid"]);
    wait_until(|| {
        let (_, body) = http(&addr, "GET", &format!("/records/{record_id}/runs"), &token, "");
        let runs = body["runs"].as_array().unwrap();
        (runs.len() == 2 && runs[0]["status"] == "exited").then_some(())
    });
    let (status, _) = http(&addr, "POST", "/records/no-such-record/start", &token, "{}");
    assert_eq!(status, 404);

    // Remove: confirm-gated; the record and its runs are gone afterwards.
    let (status, refused) = http(&addr, "POST", &format!("/records/{record_id}/remove"), &token, "{}");
    assert_eq!(status, 400, "{refused}");
    let (status, removed) = http(&addr, "POST", &format!("/records/{record_id}/remove"), &token, r#"{"confirm": true}"#);
    assert_eq!(status, 200, "{removed}");
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["runs_deleted"], 2);
    assert!(w.registry.handle(new_term as u32).is_none(), "the attached terminal was closed");
    let (status, _) = http(&addr, "GET", &format!("/records/{record_id}/runs"), &token, "");
    assert_eq!(status, 404);
    let (status, _) = http(&addr, "POST", "/records/no-such-record/remove", &token, r#"{"confirm": true}"#);
    assert_eq!(status, 404);
    assert!(w.records.list(&w.registry).is_empty());
    let _ = &w.workspace;
}
