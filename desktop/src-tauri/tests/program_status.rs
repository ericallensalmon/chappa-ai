//! Program status end to end through the registry:
//!  - a process that prints `OSC 777;status;<state>` sets the row's
//!    `program_status` (control row and rail row), with
//!    `term://program_status` broadcast on a change only; an unknown word
//!    changes nothing; the exit clears it and an ended run takes no more;
//!  - while the row reports `blocked`, a delivery through the ready gate
//!    waits, goes in once the program reports anything else, and gives up
//!    with `blocked` at its cap.
//!
//! No pty: the byte stream and the exit are driven by hand.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{Runtime, ToolType, Transport};
use app_lib::agents::{deliver_prompt_when_ready, AgentMeta, Bridge, DeliveryPolicy, NsPid};
use app_lib::registry::{EventSink, JsonEvent, Registry, TERM_PROGRAM_STATUS};
use serde_json::json;
use term_core::actor::{ActorConfig, TermHandle};
use term_core::pty::{ExitStatus, PtySpec};
use term_core::status::ProgramStatus;

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

impl MockSink {
    fn statuses(&self) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.event == TERM_PROGRAM_STATUS)
            .map(|e| e.data["status"].clone())
            .collect()
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

fn meta() -> AgentMeta {
    AgentMeta {
        tool_id: 1,
        tool_type: ToolType::Custom,
        model: None,
        runtime: Runtime::Host,
        project_id: None,
        parent_process_id: None,
        spawned_at_ms: 0,
        spawn_uuid: "status-run".into(),
        nspid: NsPid::Unresolved,
        container: None,
        bridge: Bridge::Ok,
        transport: Transport::Json,
        awaiting_input: false,
    }
}

fn entry(registry: &Registry, sink: Arc<MockSink>) -> (u32, TermHandle) {
    let id = registry.reserve_id();
    let cfg = ActorConfig {
        spec: PtySpec { command: String::new(), args: vec![], cwd: None, env: vec![], cols: 40, rows: 6 },
        scrollback_lines: 100,
        ..ActorConfig::default()
    };
    let handle = registry
        .create_json_agent_terminal(cfg, sink, None, "reporter".into(), id, meta(), false, None)
        .expect("entry");
    (id, handle)
}

fn row_status(registry: &Registry, id: u32) -> Option<ProgramStatus> {
    registry.control_snapshot(id).and_then(|row| row.program_status)
}

#[test]
fn a_reported_status_lands_on_the_row_and_the_exit_clears_it() {
    let registry = Registry::default();
    let sink = Arc::new(MockSink::default());
    let (id, handle) = entry(&registry, sink.clone());
    assert_eq!(row_status(&registry, id), None, "nothing reported yet");

    handle.feed(b"running tests\r\n\x1b]777;status;working\x07");
    wait_until(|| (row_status(&registry, id) == Some(ProgramStatus::Working)).then_some(()));
    handle.feed(b"\x1b]777;status;blocked\x1b\\");
    wait_until(|| (row_status(&registry, id) == Some(ProgramStatus::Blocked)).then_some(()));
    assert_eq!(registry.program_status(id), Some(ProgramStatus::Blocked));
    let rail = registry.list_rows().into_iter().find(|r| r.base.id == id).unwrap();
    assert_eq!(rail.program_status, Some(ProgramStatus::Blocked), "the rail row carries it too");
    let wire = serde_json::to_value(registry.control_snapshot(id).unwrap()).unwrap();
    assert_eq!(wire["program_status"], json!("blocked"), "lowercase on the wire");

    // A repeat is not news; an unknown word changes nothing.
    handle.feed(b"\x1b]777;status;blocked\x07\x1b]777;status;Done\x07\x1b]777;status;done\x07");
    wait_until(|| (row_status(&registry, id) == Some(ProgramStatus::Done)).then_some(()));
    assert_eq!(sink.statuses(), vec![json!("working"), json!("blocked"), json!("done")]);

    // The exit clears it; an ended run takes no more.
    handle.feed(b"\x1b]777;status;failed\x07");
    handle.note_child_exit(ExitStatus { code: Some(1), success: false });
    wait_until(|| registry.control_snapshot(id).filter(|row| !row.child_alive && row.base.status.as_str() == "failed"));
    assert_eq!(row_status(&registry, id), None, "an ended run reports nothing");
    assert_eq!(registry.record_program_status(id, ProgramStatus::Blocked), None);
    assert_eq!(row_status(&registry, id), None);
    assert_eq!(registry.record_program_status(9999, ProgramStatus::Working), None, "unknown terminal");
    registry.close(id);
}

#[test]
fn a_blocked_row_holds_a_delivery_until_it_reports_anything_else() {
    let registry = Registry::default();
    let sink = Arc::new(MockSink::default());
    let (id, handle) = entry(&registry, sink);
    handle.feed(b"Proceed? [y/N] \x1b]777;status;blocked\x07");
    wait_until(|| (registry.program_status(id) == Some(ProgramStatus::Blocked)).then_some(()));

    let gate = registry.clone();
    let delivery = std::thread::spawn(move || {
        deliver_prompt_when_ready(&gate, id, "next step", DeliveryPolicy::quiet_only(50, Duration::from_secs(10)))
    });
    // Quiet for well past the window, but blocked: nothing is written.
    std::thread::sleep(Duration::from_millis(500));
    assert!(!delivery.is_finished(), "held while blocked");
    assert!(registry.input_journal(id).unwrap().is_empty());

    handle.feed(b"\x1b]777;status;working\x07");
    let receipt = delivery.join().unwrap();
    assert!(receipt.delivered, "{receipt:?}");
    assert_eq!(receipt.reason, None);
    assert_eq!(registry.input_journal(id).unwrap().len(), 1);

    // Blocked through the whole cap: the receipt says why.
    handle.feed(b"\x1b]777;status;blocked\x07");
    wait_until(|| (registry.program_status(id) == Some(ProgramStatus::Blocked)).then_some(()));
    let receipt = deliver_prompt_when_ready(&registry, id, "later", DeliveryPolicy::quiet_only(50, Duration::from_millis(300)));
    assert!(!receipt.delivered);
    assert_eq!(receipt.reason.as_deref(), Some("blocked"));
    assert_eq!(registry.input_journal(id).unwrap().len(), 1, "nothing more was written");
    registry.close(id);
}
