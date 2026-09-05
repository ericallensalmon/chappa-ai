//! Subagent nesting in the rail + `close_on_exit`:
//!  - the recorded parent surfaces on `RailRow` + the `/processes` row +
//!    `term://created` (Part A wire facts);
//!  - `close_on_exit` closes the entry through the ORDINARY close path after
//!    the child exits, broadcasting `term://closed {reason: "close_on_exit"}`;
//!  - an ordinary close broadcasts `reason: "closed"` exactly once;
//!  - the debug-harness (8323) close (`close_silent`) emits neither.
//!
//! Any fixture touching OS process semantics is cfg-gated (`sh` on unix,
//! `cmd` on Windows).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::agents::{AgentMeta, Bridge, NsPid, SpawnAgentRequest, SpawnContext};
use app_lib::projects::Projects;
use app_lib::registry::{
    ClosedBroadcast, ClosedEvent, EventSink, JsonEvent, Registry,
};
use app_lib::settings::SettingsState;
use term_core::actor::ActorConfig;
use term_core::pty::PtySpec;

/// A sink that records whatever the pump pushes (records nothing specific
/// here — closes need no binary/frames assertions).
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

/// The test's `term://closed` broadcast — mirrors what a real AppHandle emits.
#[derive(Default)]
struct FakeClosed {
    events: Mutex<Vec<ClosedEvent>>,
}

impl ClosedBroadcast for FakeClosed {
    fn closed(&self, event: &ClosedEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

/// A short-lived child (prints nothing, exits quickly) — the substrate for
/// `close_on_exit`: the pump observes `Exited` and (with the flag) closes.
#[cfg(not(windows))]
fn exiting_spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "exit 0".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

#[cfg(windows)]
fn exiting_spec() -> PtySpec {
    PtySpec {
        command: "cmd.exe".into(),
        args: vec!["/c".into(), "exit".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

/// A longer-lived child (stays alive for the duration of a spawn test).
#[cfg(not(windows))]
fn lingering_spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "exec sleep 30".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

#[cfg(windows)]
fn lingering_spec() -> PtySpec {
    PtySpec {
        command: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 30".into(),
        ],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

fn cfg(spec: PtySpec) -> ActorConfig {
    ActorConfig {
        spec,
        scrollback_lines: 100,
        ..ActorConfig::default()
    }
}

/// Wait until a predicate over shared state holds.
fn wait_until<T>(mut pred: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = pred() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the pump / close thread"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A non-docker agent meta: project is optional, parent is the fixture knob.
fn agent_meta(project_id: Option<u32>, parent_process_id: Option<u32>) -> AgentMeta {
    AgentMeta {
        tool_id: 3,
        tool_type: ToolType::Custom,
        model: Some("m".into()),
        runtime: Runtime::Host,
        project_id,
        parent_process_id,
        spawned_at_ms: 0,
        spawn_uuid: "u48".into(),
        nspid: NsPid::Unresolved,
        container: None,
        bridge: Bridge::Ok,
        transport: app_lib::agent_tools::Transport::Tty,
        awaiting_input: false,
    }
}

/// A plain terminal carrying the parent's role (any kind is a valid parent).
fn parent_entry(registry: &Registry) -> u32 {
    registry
        .create_terminal(cfg(lingering_spec()), Arc::new(MockSink::default()), None, "parent-claude".into())
        .expect("parent spawn")
}

/// The recorded parent rides on `RailRow` (`list_rows` /
/// `attach_terminal`) AND the `/processes` (`list_control`) row — the wire
/// facts the frontend and MCP consumers rely on.
#[test]
fn parent_surfaces_on_rail_row_and_control_row() {
    let registry = Registry::default();
    let parent = parent_entry(&registry);
    let child = registry
        .reserve_id();
    registry
        .create_agent_terminal(
            cfg(lingering_spec()),
            Arc::new(MockSink::default()),
            None,
            "child".into(),
            child,
            agent_meta(None, Some(parent)),
            false,
        )
        .expect("child agent spawn");

    // RailRow (the reconcile/adopt source).
    let row = registry
        .list_rows()
        .into_iter()
        .find(|r| r.base.id == child)
        .expect("child rail row");
    assert_eq!(row.parent_process_id, Some(parent), "rail_row carries the parent");

    // /processes row: `parent_process_id` via the agent block.
    let proc = registry
        .control_snapshot(child)
        .expect("child control row");
    assert_eq!(
        proc.agent.as_ref().and_then(|m| m.parent_process_id),
        Some(parent),
        "control /processes agent block carries the parent"
    );
    // A root has `null` on both surfaces.
    let root = registry.reserve_id();
    registry
        .create_agent_terminal(
            cfg(lingering_spec()),
            Arc::new(MockSink::default()),
            None,
            "root".into(),
            root,
            agent_meta(None, None),
            false,
        )
        .expect("root agent spawn");
    let root_row = registry.list_rows().into_iter().find(|r| r.base.id == root).unwrap();
    assert_eq!(root_row.parent_process_id, None, "root is null on the rail");
    let root_proc = registry.control_snapshot(root).unwrap();
    assert_eq!(root_proc.agent.as_ref().and_then(|m| m.parent_process_id), None);
}

/// `close_on_exit` = the entry closes through the SAME close
/// path after the child exits, and the broadcast reason is `close_on_exit`.
#[test]
fn close_on_exit_closes_after_exit_with_reason_close_on_exit() {
    let registry = Registry::default();
    let closed = Arc::new(FakeClosed::default());
    registry.set_closed_broadcast(closed.clone());
    let id = registry
        .reserve_id();
    registry
        .create_agent_terminal(
            cfg(exiting_spec()),
            Arc::new(MockSink::default()),
            None,
            "short-lived".into(),
            id,
            agent_meta(None, None),
            true,
        )
        .expect("agent spawn");
    // The child exits → the pump's exit branch closes the entry (off-thread)
    // and broadcasts `term://closed {reason: close_on_exit}`.
    let closed_event = wait_until(|| {
        let e = closed.events.lock().unwrap();
        (e.len() >= 1).then(|| e[0].clone())
    });
    assert_eq!(closed_event.term_id, id);
    assert_eq!(closed_event.reason, "close_on_exit");
    // The entry is really gone (list/control agree).
    assert!(registry.control_snapshot(id).is_none(), "entry closed");
    assert!(registry.list_rows().iter().all(|r| r.base.id != id));
    assert_eq!(closed.events.lock().unwrap().len(), 1, "exactly one close_on_exit");
}

/// An ordinary close (the frontend × / MCP close_terminal
/// path) broadcasts `reason: "closed"` — and exactly once.
#[test]
fn ordinary_close_broadcasts_closed_exactly_once() {
    let registry = Registry::default();
    let closed = Arc::new(FakeClosed::default());
    registry.set_closed_broadcast(closed.clone());
    let id = parent_entry(&registry);
    assert!(registry.close(id).is_some());
    let events = closed.events.lock().unwrap();
    assert_eq!(events.len(), 1, "the frontend-× / MCP close path emits exactly one");
    assert_eq!(events[0].term_id, id);
    assert_eq!(events[0].reason, "closed");
}

/// The debug-harness (8323) close is SILENT — a
/// terminal created through the harness path emits neither created nor
/// closed, mirroring the created exclusion.
#[test]
fn debug_harness_close_emits_neither() {
    let registry = Registry::default();
    let closed = Arc::new(FakeClosed::default());
    registry.set_closed_broadcast(closed.clone());
    // A debug-harness terminal: created via the plain registry path (never
    // broadcast `term://created`) and closed through `close_silent`.
    let id = parent_entry(&registry);
    assert!(registry.close_silent(id).is_some());
    assert!(
        closed.events.lock().unwrap().is_empty(),
        "debug-harness close must not broadcast term://closed"
    );
}

/// Part A, end to end: `spawn_agent` (the control/MCP headless path)
/// records the SPAWNING ACTOR as the parent, the explicit request field wins
/// over the actor, and a `user`/absent actor produces a root.
#[test]
fn spawn_records_parent_from_explicit_then_actor_else_none() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let tool = host_tool(&tools, "spawn probe");
    let registry = Registry::default();
    let settings = SettingsState::default();
    let projects = Projects::default();
    let parent = parent_entry(&registry);

    let spawn = |req_parent: Option<u32>, actor: Option<u32>| {
        let ctx = SpawnContext {
            registry: &registry,
            tools: &tools,
            settings: &settings,
            projects: &projects,
            actor,
        };
        app_lib::agents::spawn_agent_headless(
            ctx,
            SpawnAgentRequest {
                agent_tool_id: tool.id,
                parent_process_id: req_parent,
                ..SpawnAgentRequest::default()
            },
        )
        .expect("spawn")
    };

    // Actor (the X-Chappa-Actor parses to a numeric id) → the parent.
    let by_actor = spawn(None, Some(parent));
    assert_eq!(by_actor.agent.parent_process_id, Some(parent), "actor → parent");

    // Explicit field beats the actor.
    let explicit = spawn(Some(parent), Some(999_999));
    assert_eq!(explicit.agent.parent_process_id, Some(parent), "explicit wins");

    // Stale actor (a live id is required) → None.
    let stale = spawn(Some(999_999), Some(parent));
    assert_eq!(stale.agent.parent_process_id, None, "stale explicit → None");

    // `user`/absent actor → a root.
    let root = spawn(None, None);
    assert_eq!(root.agent.parent_process_id, None, "no actor → root");
}

/// A host-runtime agent tool whose command line exists on both platforms.
fn host_tool(tools: &AgentToolsState, name: &str) -> app_lib::agent_tools::AgentTool {
    let (program, args) = echo_tool();
    tools
        .upsert(AgentTool {
            id: 0,
            name: name.into(),
            tool_type: ToolType::Custom,
            program,
            // `probe` is the tool's own last arg; the screen echoes it back.
            args: [args, vec!["probe".into()]].concat(),
            model: Some("fake-model".into()),
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: app_lib::agent_tools::Transport::Tty,
        })
        .unwrap()
}

#[cfg(not(windows))]
fn echo_tool() -> (String, Vec<String>) {
    (
        "sh".into(),
        vec!["-c".into(), "echo \"$1\"; exec sh".into(), "sh".into()],
    )
}

#[cfg(windows)]
fn echo_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/K".into(), "echo".into()])
}
