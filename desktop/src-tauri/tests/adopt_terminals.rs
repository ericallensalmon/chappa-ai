//! Adopt backend-spawned terminals:
//!  - the SWAPPABLE sink: replacing the sink delivers subsequent events to the
//!    NEW sink and triggers a FULL (the dock-window primitive);
//!  - attach-to-missing is a clean error; re-attach is idempotent;
//!  - the rich rail rows carry kind/binding facts for each spawn kind;
//!  - the `term://created` broadcast seam records every spawn kind (fake app
//!    handle, the `CreatedBroadcast` trait).
//!
//! Any fixture touching OS process semantics is cfg-gated (`sh` on unix,
//! `cmd` on Windows).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{Runtime, ToolType, Transport};
use app_lib::agents::{AgentMeta, Bridge, NsPid};
use app_lib::registry::{
    emit_created, CreatedBroadcast, CreatedEvent, EventSink, JsonEvent, Registry, TERM_CREATED,
};
use term_core::actor::ActorConfig;
use term_core::frame::FRAME_MAGIC;
use term_core::pty::PtySpec;

/// A sink that records whatever the pump pushes (the pump.rs MockSink).
#[derive(Default)]
struct MockSink {
    binary: Mutex<Vec<Vec<u8>>>,
    events: Mutex<Vec<JsonEvent>>,
}

impl EventSink for MockSink {
    fn send_binary(&self, bytes: Vec<u8>) {
        self.binary.lock().unwrap().push(bytes);
    }
    fn emit_json(&self, event: JsonEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// The test's created-broadcast: records what a real AppHandle would emit.
#[derive(Default)]
struct FakeCreated {
    events: Mutex<Vec<CreatedEvent>>,
}

impl CreatedBroadcast for FakeCreated {
    fn created(&self, event: &CreatedEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

/// A child that prints `a`, pauses ~1s, then prints `b` — `sh` in the
/// container/CI, `cmd` on the Windows host.
#[cfg(not(windows))]
fn pausing_spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "printf a; sleep 1; printf b".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

#[cfg(windows)]
fn pausing_spec() -> PtySpec {
    // The pty CHILD must be the process that pauses. cmd compounds die under
    // the CreateProcessW quoting (3fbcc93 family) and the `py` LAUNCHER exits
    // immediately while real python runs as ITS child — either way the actor
    // finish_exits before the test can attach, which mimics a sink-swap
    // failure (seen 2026-09-01). powershell is one process and
    // waits out its own Start-Sleep.
    PtySpec {
        command: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "'a'; Start-Sleep -Seconds 3; 'b'".into(),
        ],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

/// A short-lived child for plain-shell rows (prints nothing, exits quickly).
#[cfg(not(windows))]
fn spec() -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "printf x".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

#[cfg(windows)]
fn spec() -> PtySpec {
    PtySpec {
        command: "cmd.exe".into(),
        args: vec!["/c".into(), "echo x".into()],
        cwd: None,
        env: Vec::new(),
        cols: 20,
        rows: 5,
    }
}

fn cfg() -> ActorConfig {
    ActorConfig {
        spec: spec(),
        scrollback_lines: 10_000,
        ..ActorConfig::default()
    }
}

/// Wait until a predicate over the recorded payloads holds.
fn wait_until<T>(mut pred: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = pred() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pump output"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Seq number from the wire header: magic u8, version u8, seq u32 LE.
fn frame_seq(frame: &[u8]) -> u32 {
    assert_eq!(frame[0], FRAME_MAGIC, "wire frame magic");
    u32::from_le_bytes(frame[2..6].try_into().unwrap())
}

/// Frame kind byte (offset 6): 0 = FULL, 1 = DELTA.
fn frame_kind(frame: &[u8]) -> u8 {
    frame[6]
}

/// A non-docker agent meta (Runtime::Host needs no docker CLI).
fn agent_meta(project_id: Option<u32>) -> AgentMeta {
    AgentMeta {
        tool_id: 3,
        tool_type: ToolType::Custom,
        model: Some("m".into()),
        runtime: Runtime::Host,
        project_id,
        parent_process_id: None,
        spawned_at_ms: 0,
        spawn_uuid: "u47".into(),
        nspid: NsPid::Unresolved,
        container: None,
        bridge: Bridge::Ok,
        transport: Transport::Tty,
        awaiting_input: false,
    }
}

/// Replacing the sink hands the stream to the NEW sink and the new subscriber
/// starts from a FULL frame (the `attach_terminal` contract / dock
/// primitive): a child that prints `a`, pauses, then prints `b` lets the
/// attach land between the two prompts deterministically.
#[test]
fn attach_swaps_sink_delivers_subsequent_events_and_triggers_a_full() {
    let registry = Registry::default();
    let sink_a = Arc::new(MockSink::default());
    let sink_b = Arc::new(MockSink::default());
    let id = registry
        .create_terminal(
            ActorConfig {
                spec: pausing_spec(),
                scrollback_lines: 10_000,
                ..ActorConfig::default()
            },
            sink_a.clone(),
            None,
            "swap".into(),
        )
        .expect("pty spawn");

    // Ack EVERY frame as it lands while scanning for 'a' — the ack gate
    // otherwise wedges this test: an early non-'a' damage frame left
    // un-acked holds the gate shut, the 'a' output coalesces unsent, and
    // the next frame the test sees is finish_exit's final Full (dead actor
    // by attach time). Seen 2026-09-01: acking only the frames
    // the test cared about was the original sin here.
    wait_until(|| {
        let bufs = sink_a.binary.lock().unwrap().clone();
        for b in &bufs {
            registry.handle(id).expect("handle").ack(frame_seq(b));
        }
        bufs.iter()
            .any(|b| b.windows(4).any(|w| w == [0x61, 0, 0, 0]))
            .then_some(())
    });
    let a_count = sink_a.binary.lock().unwrap().len();

    // Swap sink_b in; the answer carries the terminal's row facts.
    let row = registry.attach_terminal(id, sink_b.clone()).expect("attach");
    assert_eq!(row.kind, "terminal");
    assert_eq!(row.base.name, "swap");
    assert!(row.project_id.is_none() && row.agent_tool_id.is_none());

    // The replacement sink's FIRST frame is a FULL (the forced resync).
    let b_first = wait_until(|| sink_b.binary.lock().unwrap().first().cloned());
    assert_eq!(frame_kind(&b_first), 0, "replacement sink starts from a FULL frame");
    registry.handle(id).expect("handle").ack(frame_seq(&b_first));

    // The later 'b' output reaches the NEW sink, and the OLD one sees nothing
    // new after the swap.
    wait_until(|| {
        let bufs = sink_b.binary.lock().unwrap().clone();
        bufs.into_iter()
            .any(|b| b.windows(4).any(|w| w == [0x62, 0, 0, 0]))
            .then_some(())
    });
    assert_eq!(
        sink_a.binary.lock().unwrap().len(),
        a_count,
        "the old sink stops receiving frames after the swap"
    );

    registry.close(id);
}

/// Attaching to a missing id is a clear error, not a silent no-op; re-attach
/// on a live terminal is legal and idempotent.
#[test]
fn attach_to_a_missing_id_is_a_clean_error_and_re_attach_is_idempotent() {
    let registry = Registry::default();
    let err = registry.attach_terminal(42, Arc::new(MockSink::default())).unwrap_err();
    assert!(err.contains("no such terminal"), "clear error naming the id, got: {err}");

    let id = registry
        .create_terminal(cfg(), Arc::new(MockSink::default()), None, "t".into())
        .expect("pty spawn");
    let row1 = registry
        .attach_terminal(id, Arc::new(MockSink::default()))
        .expect("attach");
    let row2 = registry
        .attach_terminal(id, Arc::new(MockSink::default()))
        .expect("re-attach");
    assert_eq!(row1.base.id, id);
    assert_eq!(row2.base.id, id);
    registry.close(id);
}

/// `list_rows` (the reconcile/adoption source) carries the same kind/binding
/// facts the created broadcast does, for every spawn kind.
#[test]
fn rail_rows_carry_kind_and_binding_facts_for_each_spawn_kind() {
    let registry = Registry::default();
    // plain workspace terminal
    let plain = registry
        .create_terminal(cfg(), Arc::new(MockSink::default()), None, "plain".into())
        .expect("spawn");
    // a project process terminal is a plain spawn claimed by a project
    let proc = registry
        .create_terminal(cfg(), Arc::new(MockSink::default()), None, "proc".into())
        .expect("spawn");
    registry.set_project(proc, Some(9));
    // an agent (non-docker) bound to a project
    let aid = registry.reserve_id();
    registry
        .create_agent_terminal(
            cfg(),
            Arc::new(MockSink::default()),
            None,
            "agent".into(),
            aid,
            agent_meta(Some(4)),
            false,
        )
        .expect("agent spawn");

    let rows = registry.list_rows();
    let plain_row = rows.iter().find(|r| r.base.id == plain).expect("plain row");
    assert_eq!(plain_row.kind, "terminal");
    assert_eq!(plain_row.project_id, None);
    let proc_row = rows.iter().find(|r| r.base.id == proc).expect("proc row");
    assert_eq!(proc_row.kind, "process");
    assert_eq!(proc_row.project_id, Some(9));
    let agent_row = rows.iter().find(|r| r.base.id == aid).expect("agent row");
    assert_eq!(agent_row.kind, "agent");
    assert_eq!(agent_row.project_id, Some(4));
    assert_eq!(agent_row.agent_tool_id, Some(3));

    registry.close(plain);
    registry.close(proc);
    registry.close(aid);
}

/// The created-broadcast seam records every spawn kind with its binding facts
/// (a fake app handle; a real AppHandle cannot exist without Tauri).
#[test]
fn created_broadcast_records_every_spawn_kind() {
    let fake = FakeCreated::default();
    for (kind, project_id, tool_id) in [
        ("terminal", None, None),
        ("agent", Some(1), Some(7)),
        ("process", Some(2), None),
    ] {
        emit_created(
            &fake,
            CreatedEvent {
                term_id: 1,
                name: "x".into(),
                kind,
                parent_process_id: None,
                project_id,
                agent_tool_id: tool_id,
            },
        );
    }
    assert_eq!(TERM_CREATED, "term://created");
    let events = fake.events.lock().unwrap().clone();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].kind, "terminal");
    assert_eq!(events[0].project_id, None);
    assert_eq!(events[1].kind, "agent");
    assert_eq!(events[1].project_id, Some(1));
    assert_eq!(events[1].agent_tool_id, Some(7));
    assert_eq!(events[2].kind, "process");
    assert_eq!(events[2].project_id, Some(2));
    assert_eq!(events[2].agent_tool_id, None);
}
