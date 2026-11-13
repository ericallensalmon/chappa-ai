//! Session capture and resume end to end:
//!  - a process that announces its session (`OSC 777;session;<id>`) lands
//!    it on the record head, the ledger row and the row surfaces, a later
//!    announcement replaces it, and a late one for a stale run is dropped;
//!  - a tool with a regex capture keeps the LATEST printed id (one printed
//!    on the way out wins);
//!  - a json-transport agent's own session id is copied off its event stream;
//!  - `resume` on a record with nothing saved is the distinct no-session
//!    outcome, a provider that exits before it is ready is a REJECTION that
//!    carries its words, an accepted resume runs the substituted resume argv
//!    and the run says what it continued, a running record cannot be resumed;
//!  - the control routes: `set_session` self-targeting through the actor
//!    header, and the resume route's typed answers.
//!
//! Real terminals: `cmd` / `powershell` on Windows, `sh` elsewhere.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, SessionCapture, ToolType, Transport};
use app_lib::agents::{spawn_agent_headless, AgentMeta, Bridge, NsPid, SpawnAgentRequest, SpawnContext};
use app_lib::control_http::{self, ControlState};
use app_lib::coordination::Coordination;
use app_lib::projects::Projects;
use app_lib::records::{Records, ResumeCheck, ResumeFailure, Scope, StartDeps};
use app_lib::registry::{EventSink, JsonEvent, Registry, RunRecorder, TERM_SESSION};
use app_lib::settings::SettingsState;
use app_lib::workspace::Workspace;
use project_model::projects::ProjectStore;
use project_model::records::{Run, Session, SessionSource, TailKind};
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

/// A tool program that prints its last argument and exits.
#[cfg(windows)]
fn exiting_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/C".into(), "echo".into()])
}

#[cfg(not(windows))]
fn exiting_tool() -> (String, Vec<String>) {
    ("sh".into(), vec!["-c".into(), "echo \"$1\"".into(), "sh".into()])
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

/// A program whose whole command line is the resume template: the shell,
/// with the template supplying what it runs.
fn shell_program() -> String {
    if cfg!(windows) { "cmd".into() } else { "sh".into() }
}

/// A resume template that prints the provider's refusal and exits.
fn rejecting_resume_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["/C".into(), "echo No conversation found with session ID: {session_id}".into()]
    } else {
        vec!["-c".into(), "echo \"No conversation found with session ID: {session_id}\"".into()]
    }
}

/// A resume template that prints what it was asked to continue and stays
/// alive — a provider that took the session.
fn accepting_resume_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["/K".into(), "echo resumed {session_id}".into()]
    } else {
        vec!["-c".into(), "echo \"resumed {session_id}\"; exec sh".into()]
    }
}

/// A program that prints two session lines and exits: the second is the
/// one printed on the way out.
fn two_ids_program() -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".into(), vec!["/C".into(), "echo session: ses-first & echo session: ses-last".into()])
    } else {
        ("sh".into(), vec!["-c".into(), "echo 'session: ses-first'; echo 'session: ses-last'".into()])
    }
}

/// A persistent machine-mode stand-in: announces its session in its init
/// line, answers each stdin line, exits at EOF.
fn fake_json_cli(dir: &std::path::Path) -> (String, Vec<String>) {
    let init = r#"{"type":"system","subtype":"init","session_id":"ses-json","model":"fake-model"}"#;
    let result = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"usage":{"input_tokens":5,"output_tokens":2},"result":"pong"}"#;
    if cfg!(windows) {
        let script = dir.join("fake_json_cli.cmd");
        let body = [
            "@echo off",
            ":loop",
            "set \"line=\"",
            "set /p line=",
            "if not defined line exit /b 0",
            &format!("echo {init}"),
            &format!("echo {result}"),
            "goto loop",
            "",
        ]
        .join("\r\n");
        std::fs::write(&script, body).unwrap();
        ("cmd".into(), vec!["/C".into(), script.to_string_lossy().into_owned()])
    } else {
        let script = dir.join("fake_json_cli.sh");
        let body = [
            "#!/bin/sh",
            "while IFS= read -r line; do",
            &format!("  echo '{init}'"),
            &format!("  echo '{result}'"),
            "done",
            "",
        ]
        .join("\n");
        std::fs::write(&script, body).unwrap();
        ("sh".into(), vec![script.to_string_lossy().into_owned()])
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

struct World {
    dir: tempfile::TempDir,
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
    World { dir, registry, records, projects, workspace, tools, settings: SettingsState::default() }
}

impl World {
    fn tool(&self, name: &str, program: String, args: Vec<String>, capture: SessionCapture, resume_argv: Vec<String>) -> AgentTool {
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
                resume_argv,
                session_capture: capture,
            })
            .unwrap()
    }

    fn ctx(&self) -> SpawnContext<'_> {
        SpawnContext {
            registry: &self.registry,
            tools: &self.tools,
            settings: &self.settings,
            projects: &self.projects,
            records: &self.records,
            actor: None,
        }
    }

    fn deps(&self) -> StartDeps<'_> {
        StartDeps { registry: &self.registry, tools: &self.tools, settings: &self.settings, app: None }
    }

    fn last_run(&self, record_id: &str) -> Option<Run> {
        self.records.get(record_id).and_then(|(_, r)| r.last_run)
    }

    fn session(&self, record_id: &str) -> Option<Session> {
        self.last_run(record_id).and_then(|run| run.session)
    }

    fn screen(&self, id: u32) -> String {
        self.registry.handle(id).map(|h| h.dump_text(50).join("\n")).unwrap_or_default()
    }
}

fn host_meta(tool_id: u32, tool_type: ToolType, uuid: &str) -> AgentMeta {
    AgentMeta {
        tool_id,
        tool_type,
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

/// A no-pty agent entry bound to an agent record, driven by hand.
fn announcing_entry(w: &World, record_id: &str, run_uuid: &str, sink: Arc<MockSink>) -> (u32, term_core::actor::TermHandle) {
    w.records
        .add_agent_record(
            Scope::Workspace,
            project_model::records::AgentRecord {
                record_id: record_id.into(),
                name: "announcer".into(),
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
    w.records
        .begin_run(Scope::Workspace, project_model::records::RecordKind::Agent, record_id, &Run::begin(run_uuid, 1, TailKind::Text))
        .unwrap();
    let id = w.registry.reserve_id();
    let cfg = ActorConfig {
        spec: PtySpec { command: String::new(), args: vec![], cwd: None, env: vec![], cols: 40, rows: 6 },
        scrollback_lines: 100,
        ..ActorConfig::default()
    };
    let handle = w
        .registry
        .create_json_agent_terminal(cfg, sink, None, "announcer".into(), id, host_meta(1, ToolType::Custom, run_uuid), false, Some(record_id.into()))
        .expect("entry");
    (id, handle)
}

/// The announce path: `OSC 777;session;<id>` lands on the live row, the
/// record head and the ledger row with `term://session` broadcast; a later
/// announcement replaces it; a late fact for a run the record left behind
/// is dropped from the head but kept on that run's own ledger row.
#[test]
fn an_announced_session_lands_on_the_run_and_a_later_one_replaces_it() {
    let w = world();
    let sink = Arc::new(MockSink::default());
    let (id, handle) = announcing_entry(&w, "a-announce", "run-a", sink.clone());
    handle.feed(b"booting\r\n\x1b]777;session;ses-one\x07\r\n");
    let session = wait_until(|| w.session("a-announce"));
    assert_eq!(session.session_id, "ses-one");
    assert_eq!(session.source, SessionSource::Announced);
    assert_eq!(session.provider, "custom", "filed under the agent's tool type");
    assert_eq!(session.resumed_from, None);
    // The live row's head and the control row carry it.
    let row = w.registry.control_snapshot(id).unwrap();
    assert_eq!(row.last_run.as_ref().unwrap().session.as_ref().unwrap().session_id, "ses-one");
    let listed = w.records.list(&w.registry).into_iter().find(|r| r.record_id == "a-announce").unwrap();
    assert_eq!(listed.last_run.unwrap().session.unwrap().session_id, "ses-one");
    // The ledger row too.
    let runs = w.records.runs("a-announce", 20).unwrap();
    assert_eq!(runs[0].session.as_ref().unwrap().session_id, "ses-one");
    // The broadcast names the record and the run.
    let event = sink
        .events
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.event == TERM_SESSION)
        .cloned()
        .expect("term://session");
    assert_eq!(event.term_id, id);
    assert_eq!(event.data["record_id"], json!("a-announce"));
    assert_eq!(event.data["run_uuid"], json!("run-a"));
    assert_eq!(event.data["session"]["session_id"], json!("ses-one"));
    assert_eq!(event.data["session"]["source"], json!("announced"));

    // A later announcement wins; a repeat of the same id is not news.
    handle.feed(b"\x1b]777;session;ses-two\x1b\\");
    wait_until(|| w.session("a-announce").filter(|s| s.session_id == "ses-two"));
    handle.feed(b"\x1b]777;session;ses-two\x07");
    std::thread::sleep(Duration::from_millis(100));
    let announcements = sink.events.lock().unwrap().iter().filter(|e| e.event == TERM_SESSION).count();
    assert_eq!(announcements, 2, "the repeat emitted nothing");

    // The run-uuid rule: a fact for an older run never touches the head.
    w.records.run_session("a-announce", "run-stale", Session::new("custom", "ses-stale", SessionSource::Mcp, 9));
    assert_eq!(w.session("a-announce").unwrap().session_id, "ses-two");
    w.registry.close(id);
    wait_until(|| (w.last_run("a-announce").map(|r| r.status) == Some(ProcessStatus::Stopped)).then_some(()));
    // After the run ended its head is closed to late session facts too;
    // the ledger row still takes the id (it is that run's own history).
    w.records.run_session("a-announce", "run-a", Session::new("custom", "ses-late", SessionSource::Mcp, 10));
    assert_eq!(w.session("a-announce").unwrap().session_id, "ses-two");
    assert_eq!(w.records.runs("a-announce", 20).unwrap()[0].session.as_ref().unwrap().session_id, "ses-late");
}

/// A tty tool with a regex capture: the id is the LAST match in the output,
/// so the one printed as the child exits is the one kept; the ledger row
/// agrees; the record is resumable only if the tool has a resume command.
#[test]
fn a_regex_capture_keeps_the_latest_printed_id() {
    let w = world();
    let (program, args) = two_ids_program();
    let tool = w.tool("two-ids", program, args, SessionCapture::Regex { pattern: r"session: (\S+)".into() }, vec![]);
    let spawned = spawn_agent_headless(w.ctx(), SpawnAgentRequest { agent_tool_id: tool.id, ..SpawnAgentRequest::default() }).unwrap();
    let record_id = spawned.record_id.clone().unwrap();
    let run = wait_until(|| w.last_run(&record_id).filter(|r| !r.is_open()));
    let session = run.session.expect("captured");
    assert_eq!(session.session_id, "ses-last", "the later match wins");
    assert_eq!(session.source, SessionSource::Regex);
    assert_eq!(w.records.runs(&record_id, 20).unwrap()[0].session.as_ref().unwrap().session_id, "ses-last");
    // The tool has no resume command: a resume is unavailable, not a fresh start.
    let err = w.records.resume(w.deps(), &record_id, None, None).unwrap_err();
    assert!(matches!(&err, ResumeFailure::Unavailable { message, .. } if message.contains("no resume command")), "{err:?}");
    assert_eq!(err.status(), 409);
    assert_eq!(err.to_json()["error"], json!("resume_unavailable"));
    w.registry.close(spawned.process_id);
}

/// A json-transport agent names its session in its own event stream: the
/// id is copied onto the run (source `json`) and survives the exit.
#[test]
fn a_json_agent_copies_its_session_from_the_event_stream() {
    let w = world();
    let (program, args) = fake_json_cli(w.dir.path());
    let tool = w
        .tools
        .upsert(AgentTool {
            id: 0,
            name: "fake claude".into(),
            tool_type: ToolType::Claude,
            program,
            args,
            model: None,
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: Transport::Json,
            resume_argv: vec!["--resume".into(), "{session_id}".into()],
            session_capture: SessionCapture::Json,
        })
        .unwrap();
    let spawned = spawn_agent_headless(
        w.ctx(),
        SpawnAgentRequest { agent_tool_id: tool.id, prompt: Some("ping".into()), ..SpawnAgentRequest::default() },
    )
    .expect("json spawn");
    let record_id = spawned.record_id.clone().unwrap();
    let session = wait_until(|| w.session(&record_id));
    assert_eq!((session.session_id.as_str(), session.source, session.provider.as_str()), ("ses-json", SessionSource::Json, "claude"));
    let head = w.registry.run_head(spawned.process_id).unwrap();
    assert_eq!(head.session.unwrap().session_id, "ses-json");
    w.registry.close(spawned.process_id);
    let run = wait_until(|| w.last_run(&record_id).filter(|r| !r.is_open()));
    assert_eq!(run.session.unwrap().session_id, "ses-json", "the id outlives the run");
}

/// The resume outcomes, in order: nothing saved → the distinct no-session
/// answer; a provider that exits before it is ready → a rejection carrying
/// its words, the failed run readable; a provider that takes the session →
/// the substituted resume argv ran and the run says what it continued; a
/// running record cannot be resumed.
#[test]
fn resume_distinguishes_no_session_rejection_and_acceptance() {
    let w = world();
    // A record whose runs never learned a session.
    let (program, args) = exiting_tool();
    let plain = w.tool("plain", program, [args, vec!["probe".into()]].concat(), SessionCapture::None, vec![]);
    let spawned = spawn_agent_headless(w.ctx(), SpawnAgentRequest { agent_tool_id: plain.id, ..SpawnAgentRequest::default() }).unwrap();
    let plain_record = spawned.record_id.clone().unwrap();
    wait_until(|| w.last_run(&plain_record).filter(|r| !r.is_open()));
    let err = w.records.resume(w.deps(), &plain_record, None, None).unwrap_err();
    assert!(matches!(err, ResumeFailure::NoSession { .. }), "{err:?}");
    assert_eq!(err.to_json()["error"], json!("no_session"));
    assert!(matches!(w.records.resume(w.deps(), "no-such", None, None).unwrap_err(), ResumeFailure::NotFound { .. }));

    // A record whose run learned a session while it was live (the announce
    // path through the registry), then was closed.
    let (program, args) = lingering_tool();
    let mut tool = w.tool("resumable", program, [args, vec!["probe".into()]].concat(), SessionCapture::Announced, vec![]);
    let spawned = spawn_agent_headless(w.ctx(), SpawnAgentRequest { agent_tool_id: tool.id, ..SpawnAgentRequest::default() }).unwrap();
    let record_id = spawned.record_id.clone().unwrap();
    wait_until(|| w.screen(spawned.process_id).contains("probe").then_some(()));
    w.registry.record_session(spawned.process_id, Session::new("custom", "ses-saved", SessionSource::Mcp, 5)).unwrap();
    assert_eq!(w.session(&record_id).unwrap().session_id, "ses-saved");
    // Running: not resumable.
    let err = w.records.resume(w.deps(), &record_id, None, None).unwrap_err();
    assert!(matches!(&err, ResumeFailure::Unavailable { message, .. } if message.contains("running")), "{err:?}");
    w.registry.close(spawned.process_id);
    wait_until(|| (w.last_run(&record_id).map(|r| r.status) == Some(ProcessStatus::Stopped)).then_some(()));

    // The provider refuses: the child prints its refusal and exits before
    // it is ready. The record's tool now runs the template as its whole
    // command line (no args of its own).
    tool.program = shell_program();
    tool.args = vec![];
    tool.resume_argv = rejecting_resume_argv();
    w.tools.upsert(tool.clone()).unwrap();
    let err = w.records.resume(w.deps(), &record_id, None, None).unwrap_err();
    let ResumeFailure::Rejected { record_id: rid, run_uuid, term_id, exit_code, provider_text, message } = &err else {
        panic!("expected a rejection: {err:?}");
    };
    assert_eq!(rid, &record_id);
    assert!(provider_text.contains("No conversation found with session ID: ses-saved"), "{provider_text}");
    assert!(message.contains(provider_text), "{message}");
    assert_eq!(*exit_code, Some(0));
    assert_eq!(err.status(), 409);
    let body = err.to_json();
    assert_eq!(body["error"], json!("resume_rejected"));
    assert_eq!(body["provider_text"], json!(provider_text));
    assert!(body.get("kind").is_none(), "the tag is spelled `error` on the wire");
    // The failed run is the record's head, still asking for the id it was
    // given, and its output stays readable; the terminal stayed attached
    // like any exited run (a fresh start replaces it).
    let run = w.last_run(&record_id).unwrap();
    assert_eq!(run.run_uuid, *run_uuid);
    assert_eq!(run.status, ProcessStatus::Exited);
    let session = run.session.unwrap();
    assert_eq!((session.source, session.resumed_from.as_deref()), (SessionSource::Resume, Some("ses-saved")));
    assert_eq!(w.registry.term_of_record(&record_id).map(|(t, _)| t), Some(*term_id));
    let runs = w.records.runs(&record_id, 20).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].tail.iter().any(|l| l.as_str().is_some_and(|s| s.contains("No conversation found"))), "{:?}", runs[0].tail);

    // The provider takes the session: the resume words ran (substituted,
    // on the command line), the run names what it continued.
    tool.resume_argv = accepting_resume_argv();
    w.tools.upsert(tool.clone()).unwrap();
    let ok = w.records.resume(w.deps(), &record_id, None, None).expect("accepted");
    assert_eq!(ok.session_id, "ses-saved");
    assert_eq!(ok.check, ResumeCheck::Quiet);
    assert_ne!(ok.term_id, *term_id, "the rejected terminal was replaced");
    assert!(w.registry.handle(*term_id).is_none());
    assert!(w.screen(ok.term_id).contains("resumed ses-saved"), "{}", w.screen(ok.term_id));
    let head = w.registry.run_head(ok.term_id).unwrap();
    let session = head.session.unwrap();
    assert_eq!((session.session_id.as_str(), session.source, session.resumed_from.as_deref()), ("ses-saved", SessionSource::Resume, Some("ses-saved")));
    assert_eq!(w.session(&record_id).unwrap().resumed_from.as_deref(), Some("ses-saved"));
    assert_eq!(w.records.runs(&record_id, 20).unwrap()[0].run_uuid, ok.run_uuid);
    assert!(w.registry.input_journal(ok.term_id).unwrap().is_empty(), "nothing was typed into the resumed agent");
    // An explicit id from an earlier run is honoured over the saved one.
    w.registry.close(ok.term_id);
    wait_until(|| (w.last_run(&record_id).map(|r| r.status) == Some(ProcessStatus::Stopped)).then_some(()));
    let chosen = w.records.resume(w.deps(), &record_id, Some("ses-older".into()), None).expect("accepted");
    assert_eq!(chosen.session_id, "ses-older");
    assert!(w.screen(chosen.term_id).contains("resumed ses-older"));
    w.registry.close(chosen.term_id);
    let _ = (&w.workspace, &w.projects);
}

// ---- control routes ----------------------------------------------------------

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

/// `POST /processes/{id}/session` lands the caller's id on its own row
/// (the MCP tool fills `{id}` from the actor identity); the resume route
/// answers the typed outcomes.
#[test]
fn session_and_resume_routes_answer_typed_outcomes() {
    let w = world();
    let (program, args) = lingering_tool();
    let tool = w.tool("routed", program, [args, vec!["probe".into()]].concat(), SessionCapture::Announced, vec![]);
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

    let (status, spawned) = http(&addr, "POST", "/agents", &token, None, &json!({"agent_tool_id": tool.id, "name": "routed"}).to_string());
    assert_eq!(status, 200, "{spawned}");
    let record_id = spawned["record_id"].as_str().unwrap().to_owned();
    let term = spawned["process_id"].as_u64().unwrap();
    let actor = term.to_string();

    // Nothing saved yet: the resume route says so.
    let (status, body) = http(&addr, "POST", &format!("/records/{record_id}/resume"), &token, None, "{}");
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], json!("no_session"));

    // set_session, self-targeted: the actor header names the row.
    let (status, body) = http(&addr, "POST", &format!("/processes/{term}/session"), &token, Some(&actor), r#"{"session_id": " ses-mcp "}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], json!(term));
    assert_eq!(body["record_id"], json!(record_id));
    assert_eq!(body["durable"], json!(true));
    assert_eq!(body["session"]["session_id"], json!("ses-mcp"), "trimmed");
    assert_eq!(body["session"]["source"], json!("mcp"));
    let (status, row) = http(&addr, "GET", &format!("/processes/{term}"), &token, None, "");
    assert_eq!(status, 200);
    assert_eq!(row["last_run"]["session"]["session_id"], json!("ses-mcp"));
    assert_eq!(w.session(&record_id).unwrap().session_id, "ses-mcp");
    let (status, body) = http(&addr, "POST", &format!("/processes/{term}/session"), &token, Some(&actor), "{}");
    assert_eq!(status, 400, "{body}");
    let (status, _) = http(&addr, "POST", "/processes/999999/session", &token, None, r#"{"session_id": "x"}"#);
    assert_eq!(status, 404);
    // A running record is not resumable.
    let (status, body) = http(&addr, "POST", &format!("/records/{record_id}/resume"), &token, None, "{}");
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], json!("resume_unavailable"));

    // Stop it, make its tool refuse, resume: the rejection travels whole.
    let (status, _) = http(&addr, "POST", &format!("/processes/{term}/close"), &token, None, r#"{"confirm": true}"#);
    assert_eq!(status, 200);
    wait_until(|| (w.last_run(&record_id).map(|r| r.status) == Some(ProcessStatus::Stopped)).then_some(()));
    let mut refusing = tool.clone();
    refusing.program = shell_program();
    refusing.args = vec![];
    refusing.resume_argv = rejecting_resume_argv();
    w.tools.upsert(refusing).unwrap();
    let (status, body) = http(&addr, "POST", &format!("/records/{record_id}/resume"), &token, None, "{}");
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], json!("resume_rejected"));
    assert_eq!(body["record_id"], json!(record_id));
    assert!(body["provider_text"].as_str().unwrap().contains("No conversation found with session ID: ses-mcp"), "{body}");
    assert!(body["run_uuid"].is_string() && body["term_id"].is_number(), "{body}");
    let (status, _) = http(&addr, "POST", "/records/no-such-record/resume", &token, None, "{}");
    assert_eq!(status, 404);
    // A session named explicitly on the route.
    let mut accepting = tool.clone();
    accepting.program = shell_program();
    accepting.args = vec![];
    accepting.resume_argv = accepting_resume_argv();
    w.tools.upsert(accepting).unwrap();
    let (status, body) = http(&addr, "POST", &format!("/records/{record_id}/resume"), &token, None, r#"{"session_id": "ses-named"}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session_id"], json!("ses-named"));
    assert_eq!(body["check"], json!("quiet"));
    assert_eq!(body["kind"], json!("agent"));
    let resumed = body["term_id"].as_u64().unwrap() as u32;
    assert!(w.screen(resumed).contains("resumed ses-named"));
    w.registry.close(resumed);
}
