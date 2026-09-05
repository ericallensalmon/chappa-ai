//! Pump test: the event pump forwards actor events through
//! the 2-method `EventSink` trait, so a mock sink can observe what a real
//! Tauri Channel would receive. This runs a real actor (real pty, `sh -c
//! 'printf x'`) and asserts the encoded frame bytes and the exit JSON event
//! both come out of the sink — the half that runs headless; the webview
//! half is host-verified.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::registry::{EventSink, JsonEvent, Registry};
use term_core::actor::ActorConfig;
use term_core::frame::FRAME_MAGIC;
use term_core::pty::PtySpec;

/// Records whatever the pump pushes, the way a real Channel sink would
/// receive it.
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

/// A child that prints `x` and exits 0 — `sh` in the container/CI, `cmd`
/// on a Windows host (no `sh` on PATH there).
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

/// Wait until a predicate over the mock's recorded payloads holds.
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

#[test]
fn pump_forwards_encoded_frames_and_exit_event() {
    let registry = Registry::default();
    let sink = Arc::new(MockSink::default());
    let id = registry
        .create_terminal(
            ActorConfig {
                spec: spec(),
                scrollback_lines: 10_000,
                ..ActorConfig::default()
            },
            sink.clone(),
            None,
            "pump test".into(),
        )
        .expect("pty spawn");

    // The terminal is registered and listed with the spawn-time geometry.
    let info = wait_until(|| {
        let list = registry.list();
        list.iter().find(|info| info.id == id).cloned()
    });
    assert_eq!(info.name, "pump test");
    assert_eq!((info.cols, info.rows), (20, 5));

    // The actor opens with a forced Full frame; the pump encodes it. Ack it
    // so the 'x' damage frame is not gated behind the outstanding frame.
    let first = wait_until(|| sink.binary.lock().unwrap().first().cloned());
    let seq = frame_seq(&first);
    registry.handle(id).expect("actor handle").ack(seq);

    // The pump must forward the damage frame as encoded binary carrying the
    // 'x' cell (wire cell char = u32 LE, 'x' = 0x78).
    let frame = wait_until(|| {
        let bufs = sink.binary.lock().unwrap().clone();
        bufs.into_iter()
            .find(|buf| buf.windows(4).any(|w| w == [0x78, 0, 0, 0]))
    });
    assert_eq!(frame[0], FRAME_MAGIC, "encoded frame magic");

    // The child exits 0; the pump reports it as a JSON event carrying the
    // term_id and a monotonic seq, and the same event lands in the debug
    // event ring the /events route reads from.
    let exited = wait_until(|| {
        let events = sink.events.lock().unwrap().clone();
        events
            .iter()
            .find(|event| event.event == "term://exited")
            .cloned()
    });
    assert_eq!(exited.term_id, id);
    assert_eq!(
        exited.data.get("code").and_then(|v| v.as_i64()),
        Some(0),
        "the print-x child exits 0"
    );
    assert!(exited.seq > 0, "events carry a monotonic seq");

    let ring = wait_until(|| {
        let events = registry.events_since(id, 0);
        (!events.is_empty()).then_some(events)
    });
    assert!(
        ring.iter().any(|event| event.event == "term://exited"),
        "ring holds the exited event for /events?since=0"
    );
    assert!(
        registry.events_since(id, exited.seq).is_empty(),
        "since cursor past the exit drops it"
    );

    registry.close(id);
}
