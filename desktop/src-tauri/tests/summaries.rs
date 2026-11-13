//! Run summaries end to end:
//!  - a process that prints `OSC 777;summary;<text>` lands it on the record
//!    head, the ledger row and the live row, with `term://summary`
//!    broadcast; a later post replaces it, a repeat is not news, and a
//!    late one for a run the record left behind is dropped from the head;
//!  - a summary printed just before the child exits still lands (it is
//!    handled before the exit in the same flush); once the run has ended the
//!    row takes no more;
//!  - the control route: `set_summary` self-targeting through the actor
//!    header, normalization and the cap, and the typed refusals (blank,
//!    unknown terminal, a run that already ended).
//!
//! Real terminals: `cmd` on Windows, `sh` elsewhere.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, SessionCapture, ToolType, Transport};
use app_lib::agents::{AgentMeta, Bridge, NsPid};
use app_lib::control_http::{self, ControlState};
use app_lib::coordination::Coordination;
use app_lib::projects::Projects;
use app_lib::records::{Records, Scope};
use app_lib::registry::{EventSink, JsonEvent, Registry, RunRecorder, SummaryOutcome, TERM_SUMMARY};
use app_lib::settings::SettingsState;
use app_lib::workspace::Workspace;
use project_model::projects::ProjectStore;
use project_model::records::{AgentRecord, RecordKind, Run, SummarySource, TailKind};
use project_model::workspace::WorkspaceStore;
use serde_json::{json, Value};
use term_core::actor::ActorConfig;
use term_core::pty::{ExitStatus, PtySpec};
use term_core::scanner::{SUMMARY_MAX_BYTES, SUMMARY_TRUNCATED_MARKER};
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

impl MockSink {
    fn summaries(&self) -> Vec<JsonEvent> {
        self.events.lock().unwrap().iter().filter(|e| e.event == TERM_SUMMARY).cloned().collect()
    }
}

/// A tool program that prints its last argument and stays alive.
#[cfg(windows)]
fn lingering_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/K".into(), "echo".into()])
}

#[cfg(not(windows))]
fn lingering_tool() -> (String, Vec<String>) {
    ("sh".into(), vec!["-c".into(), "echo \"$1\"; exec sh".into(), "sh".into()])
}

/// A tool program that prints its last argument and exits.
#[cfg(windows)]
fn exiting_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/C".into(), "echo".into()])
}

#[cfg(not(windows))]
fn exiting_tool() -> (String, Vec<String>) {
    ("sh".into(), vec!["-c".into(), "echo \"$1\"".into(), "sh".into()])
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

struct World {
    registry: Registry,
    records: Records,
    projects: Projects,
    tools: AgentToolsState,
    settings: SettingsState,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    store.add(dir.path().join("proj"), "proj".into(), None).unwrap();
    let projects = Projects::with_store(store);
    let workspace = Workspace::with_store(WorkspaceStore::new(dir.path().join("workspace.json")));
    let records = Records::new(projects.clone(), workspace, Coordination::in_memory().unwrap());
    let registry = Registry::default();
    registry.set_run_recorder(Arc::new(records.clone()));
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    World { registry, records, projects, tools, settings: SettingsState::default(), _dir: dir }
}

impl World {
    fn last_run(&self, record_id: &str) -> Option<Run> {
        self.records.get(record_id).and_then(|(_, r)| r.last_run)
    }

    fn summary(&self, record_id: &str) -> Option<(String, Option<SummarySource>)> {
        self.last_run(record_id).and_then(|run| run.summary.map(|text| (text, run.summary_source)))
    }

    fn tool(&self, name: &str, program: String, args: Vec<String>) -> AgentTool {
        self.tools
            .upsert(AgentTool {
                id: 0,
                name: name.into(),
                tool_type: ToolType::Custom,
                program,
                args,
                model: None,
                runtime: Runtime::Host,
                env: Default::default(),
                enabled: true,
                max_busy: None,
                transport: Transport::Tty,
                resume_argv: vec![],
                session_capture: SessionCapture::None,
            })
            .unwrap()
    }
}

fn host_meta(uuid: &str) -> AgentMeta {
    AgentMeta {
        tool_id: 1,
        tool_type: ToolType::Custom,
        model: None,
        runtime: Runtime::Host,
        project_id: None,
        parent_process_id: None,
        spawned_at_ms: 0,
        spawn_uuid: uuid.into(),
        nspid: NsPid::Unresolved,
        container: None,
        bridge: Bridge::Ok,
        transport: Transport::Json,
        awaiting_input: false,
    }
}

/// A no-pty entry bound to an agent record, its byte stream and its exit
/// driven by hand — the exact order a child's last output and its exit
/// reach the app in.
fn posting_entry(w: &World, record_id: &str, run_uuid: &str, sink: Arc<MockSink>) -> (u32, term_core::actor::TermHandle) {
    w.records
        .add_agent_record(
            Scope::Workspace,
            AgentRecord {
                record_id: record_id.into(),
                name: "poster".into(),
                tool_id: 1,
                favorite: false,
                disable_auto_rename: false,
                notification_level: None,
                parent_record_id: None,
                extra_args: vec![],
                prompt: None,
                last_run: None,
            },
        )
        .unwrap();
    w.records.begin_run(Scope::Workspace, RecordKind::Agent, record_id, &Run::begin(run_uuid, 1, TailKind::Text)).unwrap();
    let id = w.registry.reserve_id();
    let cfg = ActorConfig {
        spec: PtySpec { command: String::new(), args: vec![], cwd: None, env: vec![], cols: 40, rows: 6 },
        scrollback_lines: 100,
        ..ActorConfig::default()
    };
    let handle = w
        .registry
        .create_json_agent_terminal(cfg, sink, None, "poster".into(), id, host_meta(run_uuid), false, Some(record_id.into()))
        .expect("entry");
    (id, handle)
}

/// The OSC path: the summary lands on the record head, the live row and
/// the ledger row with `term://summary` broadcast; a later post replaces
/// it; a repeat emits nothing; a fact for a run the record left behind
/// never touches the head.
#[test]
fn a_printed_summary_lands_on_the_current_run_and_a_later_one_replaces_it() {
    let w = world();
    let sink = Arc::new(MockSink::default());
    let (id, handle) = posting_entry(&w, "s-osc", "run-s", sink.clone());
    handle.feed(b"building\r\n\x1b]777;summary;first draft\x07\r\n");
    let (text, source) = wait_until(|| w.summary("s-osc"));
    assert_eq!((text.as_str(), source), ("first draft", Some(SummarySource::Osc)));
    let row = w.registry.control_snapshot(id).unwrap();
    let head = row.last_run.as_ref().unwrap();
    assert_eq!((head.summary.as_deref(), head.summary_source), (Some("first draft"), Some(SummarySource::Osc)));
    let listed = w.records.list(&w.registry).into_iter().find(|r| r.record_id == "s-osc").unwrap();
    assert_eq!(listed.last_run.unwrap().summary.as_deref(), Some("first draft"));
    let runs = w.records.runs("s-osc", 20).unwrap();
    assert_eq!((runs[0].summary.as_deref(), runs[0].summary_source), (Some("first draft"), Some(SummarySource::Osc)));
    let event = sink.summaries().into_iter().next().expect("term://summary");
    assert_eq!(event.term_id, id);
    assert_eq!(event.data["record_id"], json!("s-osc"));
    assert_eq!(event.data["run_uuid"], json!("run-s"));
    assert_eq!(event.data["summary"], json!("first draft"));
    assert_eq!(event.data["summary_source"], json!("osc"));

    // A later post (markdown, CRLF from the pty) replaces it; a repeat of
    // the same text is not news.
    handle.feed(b"\x1b]777;summary;## Build\r\n- ok\x1b\\");
    wait_until(|| w.summary("s-osc").filter(|(t, _)| t == "## Build\n- ok"));
    handle.feed(b"\x1b]777;summary;## Build\n- ok\x07");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(sink.summaries().len(), 2, "the repeat emitted nothing");
    assert_eq!(w.records.runs("s-osc", 20).unwrap()[0].summary.as_deref(), Some("## Build\n- ok"));

    // The run-uuid rule: a fact for another run never touches the head.
    w.records.run_summary("s-osc", "run-stale", "stale", SummarySource::Mcp);
    assert_eq!(w.summary("s-osc").unwrap().0, "## Build\n- ok");
    w.registry.close(id);
}

/// A summary printed as the child's last words, with the exit right behind
/// it, still lands on the run — and the run is closed by the exit, so
/// nothing changes it afterwards.
#[test]
fn a_summary_printed_on_the_way_out_still_lands() {
    let w = world();
    let sink = Arc::new(MockSink::default());
    let (id, handle) = posting_entry(&w, "s-exit", "run-x", sink.clone());
    handle.feed(b"done\r\n\x1b]777;summary;build ok, 3 warnings\x07");
    handle.note_child_exit(ExitStatus { code: Some(0), success: true });
    let run = wait_until(|| w.last_run("s-exit").filter(|r| !r.is_open()));
    assert_eq!(run.status, ProcessStatus::Exited);
    assert_eq!((run.summary.as_deref(), run.summary_source), (Some("build ok, 3 warnings"), Some(SummarySource::Osc)));
    assert!(run.tail.iter().any(|l| l.as_str() == Some("done")), "{:?}", run.tail);
    let ledger = &w.records.runs("s-exit", 20).unwrap()[0];
    assert_eq!((ledger.status, ledger.summary.as_deref()), (ProcessStatus::Exited, Some("build ok, 3 warnings")));
    assert_eq!(sink.summaries().len(), 1);

    // The ended run takes no more summaries, from either path.
    assert_eq!(w.registry.record_summary(id, "rewritten".into(), SummarySource::Mcp), SummaryOutcome::Ended);
    w.records.run_summary("s-exit", "run-x", "late", SummarySource::Mcp);
    assert_eq!(w.summary("s-exit").unwrap().0, "build ok, 3 warnings");
    assert_eq!(w.records.runs("s-exit", 20).unwrap()[0].summary.as_deref(), Some("build ok, 3 warnings"), "the ledger row agrees");
    assert_eq!(w.registry.record_summary(9999, "x".into(), SummarySource::Mcp), SummaryOutcome::Unknown);
    w.registry.close(id);
}

// ---- control route -----------------------------------------------------------

fn http(addr: &str, method: &str, path: &str, token: &str, actor: Option<&str>, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let actor = actor.map(|a| format!("X-Chappa-Actor: {a}\r\n")).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n{actor}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// `POST /processes/{id}/summary` lands the caller's text on its own live
/// run (the MCP tool fills `{id}` from the actor identity), normalized and
/// capped like the OSC path; a blank text, an unknown terminal and a run
/// that already ended are refused, and the ended run keeps what it had.
#[test]
fn the_summary_route_stores_on_a_live_run_and_refuses_an_ended_one() {
    let w = world();
    let (program, args) = lingering_tool();
    let lingering = w.tool("lingering", program, [args, vec!["probe".into()]].concat());
    let (program, args) = exiting_tool();
    let exiting = w.tool("exiting", program, [args, vec!["bye".into()]].concat());
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let token = "t0ken-for-tests".to_owned();
    let state = ControlState {
        registry: w.registry.clone(),
        records: w.records.clone(),
        agent_tools: w.tools.clone(),
        settings: w.settings.clone(),
        projects: w.projects.clone(),
        ..ControlState::default()
    };
    let t = token.clone();
    std::thread::spawn(move || control_http::serve(server, state, t));

    let (status, spawned) = http(&addr, "POST", "/agents", &token, None, &json!({"agent_tool_id": lingering.id}).to_string());
    assert_eq!(status, 200, "{spawned}");
    let record_id = spawned["record_id"].as_str().unwrap().to_owned();
    let term = spawned["process_id"].as_u64().unwrap();
    let actor = term.to_string();
    let path = format!("/processes/{term}/summary");

    let (status, body) = http(&addr, "POST", &path, &token, Some(&actor), &json!({"text": "\r\n# Review\r\n- 2 findings\r\n\r\n"}).to_string());
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], json!(term));
    assert_eq!(body["record_id"], json!(record_id));
    assert_eq!(body["summary"], json!("# Review\n- 2 findings"), "normalized");
    assert_eq!(body["summary_source"], json!("mcp"));
    assert_eq!(body["truncated"], json!(false));
    assert_eq!(body["durable"], json!(true));
    let (status, row) = http(&addr, "GET", &format!("/processes/{term}"), &token, None, "");
    assert_eq!(status, 200);
    assert_eq!(row["last_run"]["summary"], json!("# Review\n- 2 findings"));
    assert_eq!(row["last_run"]["summary_source"], json!("mcp"));
    assert_eq!(w.summary(&record_id), Some(("# Review\n- 2 findings".into(), Some(SummarySource::Mcp))));

    // Over the cap: cut and marked, and the reply says so.
    let long = "word ".repeat(SUMMARY_MAX_BYTES / 4);
    let (status, body) = http(&addr, "POST", &path, &token, Some(&actor), &json!({"text": long}).to_string());
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["truncated"], json!(true));
    let stored = w.summary(&record_id).unwrap().0;
    assert!(stored.len() <= SUMMARY_MAX_BYTES && stored.ends_with(SUMMARY_TRUNCATED_MARKER), "{}", stored.len());

    // Refusals.
    let (status, body) = http(&addr, "POST", &path, &token, Some(&actor), r#"{"text": "  \n "}"#);
    assert_eq!((status, body["error"].as_str()), (400, Some("invalid")), "{body}");
    let (status, body) = http(&addr, "POST", &path, &token, Some(&actor), "{}");
    assert_eq!(status, 400, "{body}");
    let (status, body) = http(&addr, "POST", "/processes/999999/summary", &token, None, r#"{"text": "x"}"#);
    assert_eq!((status, body["error"].as_str()), (404, Some("not_found")), "{body}");
    let (status, _) = http(&addr, "POST", &format!("/processes/{term}/close"), &token, None, r#"{"confirm": true}"#);
    assert_eq!(status, 200);

    // A run that already ended (its pane still attached): not editable.
    let (status, spawned) = http(&addr, "POST", "/agents", &token, None, &json!({"agent_tool_id": exiting.id}).to_string());
    assert_eq!(status, 200, "{spawned}");
    let ended_record = spawned["record_id"].as_str().unwrap().to_owned();
    let ended_term = spawned["process_id"].as_u64().unwrap();
    wait_until(|| w.last_run(&ended_record).filter(|r| !r.is_open()));
    let (status, body) =
        http(&addr, "POST", &format!("/processes/{ended_term}/summary"), &token, Some(&ended_term.to_string()), r#"{"text": "after the fact"}"#);
    assert_eq!((status, body["error"].as_str()), (409, Some("not_running")), "{body}");
    assert_eq!(w.summary(&ended_record), None, "the ended run kept what it had");
    assert_eq!(w.records.runs(&ended_record, 20).unwrap()[0].summary, None);
    w.registry.close(ended_term as u32);
}
