//! Data safety end to end against the fixture source in its data-safety
//! mode (`CHAPPA_FIXTURE_DATA_SAFETY=1`): a block whose bound folder is a
//! real folder inside a temp project. While it blocks, every way to start an
//! agent there is refused (the source's own agent action, the app's spawn,
//! the control route MCP uses, starting a stopped record, resuming one),
//! terminals and agents outside it are not; what the control surface sends a
//! running agent there is held, what the person types is not. An agent may
//! not acknowledge; a person does (the call carries `X-Chappa-Actor:
//! human:<name>`, which the source records), the block lifts, the held
//! messages go out in order, and the launches are allowed again.
//!
//! The fixture is a Node script; the test skips itself (and says so) when
//! `node` is not on PATH.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::agents::{spawn_agent_headless, FakeDocker, SpawnAgentRequest, SpawnContext};
use app_lib::control_http::{self, ControlState};
use app_lib::coordination::Coordination;
use app_lib::data_safety::{state_of, REFUSAL_LEAD};
use app_lib::projects::Projects;
use app_lib::records::{Records, StartDeps};
use app_lib::registry::Registry;
use app_lib::settings::SettingsState;
use app_lib::source_actions::{
    run_action, run_action_as, run_action_with, ActionDeps, ActionOutcome, Answers,
};
use app_lib::sources::identity::{ADDR_ENV, TOKEN_ENV};
use app_lib::sources::{CallActor, SourceStatus, Sources, SourcesEmitter};
use app_lib::timers::TimerHold;
use app_lib::workspace::Workspace;
use project_model::projects::ProjectStore;
use project_model::workspace::WorkspaceStore;
use serde_json::{json, Value};

struct Quiet;
impl SourcesEmitter for Quiet {
    fn emit(&self, _: Value) {}
}

struct Reaper(Option<Child>);
impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn wait_for<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An interactive child that prints its last argument and stays alive.
#[cfg(windows)]
fn echo_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/K".into(), "echo".into()])
}

#[cfg(not(windows))]
fn echo_tool() -> (String, Vec<String>) {
    (
        "sh".into(),
        vec!["-c".into(), "echo \"$1\"; exec sh".into(), "sh".into()],
    )
}

fn screen_contains(registry: &Registry, id: u32, needle: &str) -> bool {
    registry
        .handle(id)
        .map(|h| h.dump_text(50).iter().any(|r| r.contains(needle)))
        .unwrap_or(false)
}

fn http(addr: &str, method: &str, path: &str, token: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

/// The fixture's block file with its state set: `blocking` = as shipped
/// (not clean, blocking agents), else a clean verdict.
fn block_file(data: &Path, blocking: bool) {
    let path = data.join("data-safety.json");
    let mut block: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    if blocking {
        block["state"] = "not-clean".into();
        block["blocksAgents"] = true.into();
        block["policy"] = "block-agents".into();
        block["findings"] = json!([
            { "rule": "email-real-domain", "table": "customers", "column": "email", "count": 2, "severity": "bad" },
            { "rule": "card-number", "table": "payments", "column": "card", "count": 1, "severity": "bad" },
        ]);
        block["reasons"] = json!([{ "component": "db", "text": "db: 3 findings (email-real-domain ×2, card-number ×1)" }]);
    } else {
        block["state"] = "clean".into();
        block["blocksAgents"] = false.into();
        block["policy"] = Value::Null;
        block["findings"] = json!([]);
        block["reasons"] = json!([]);
    }
    std::fs::write(&path, block.to_string()).unwrap();
}

#[test]
fn a_block_refuses_launches_in_its_folder_holds_deliveries_and_lifts_on_a_persons_acknowledgement()
{
    if !node_available() {
        eprintln!("skipping: `node` is not on PATH, the fixture source cannot run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    // The fixture's data, its placeholder folder made real: the block's
    // bound folder is `wt`, inside the project; the project root is not in it.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture/data");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let real = wt.display().to_string().replace('\\', "/");
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        let text = std::fs::read_to_string(entry.path())
            .unwrap()
            .replace("/srv/example/abc-1", &real);
        std::fs::write(data.join(entry.file_name()), text).unwrap();
    }
    block_file(&data, false);

    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    let project_id = store.add(root.clone(), "app".into(), None).unwrap().id;
    let projects = Projects::with_store(store);
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let (program, args) = echo_tool();
    let tool = tools
        .upsert(AgentTool {
            id: 0,
            name: "claude".into(),
            tool_type: ToolType::Custom,
            program,
            args,
            model: None,
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: Default::default(),
            resume_argv: Vec::new(),
            prompt_argv: vec!["{prompt}".into()],
            session_capture: Default::default(),
            hooks_opt_out: false,
            agent_args_allowed: Vec::new(),
            agent_args_any: false,
        })
        .unwrap();
    let registry = Registry::with_docker(FakeDocker::new());
    let workspace = Workspace::with_store(WorkspaceStore::new(dir.path().join("workspace.json")));
    let records = Records::new(
        projects.clone(),
        workspace,
        Coordination::in_memory().unwrap(),
    );
    registry.set_run_recorder(Arc::new(records.clone()));
    let sources = Sources::default();
    sources.init_with(
        app_lib::sources::store::SourceStore::new(dir.path().join("sources.json")),
        Arc::new(Quiet),
    );
    // The app's wiring: spawns check the blocks, deliveries wait on them.
    projects.set_sources(sources.clone());
    registry.set_hold_gate(Arc::new(sources.clone()));

    let launch = sources
        .prepare_launch(project_id, &root, "fixture-source")
        .unwrap();
    let child = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture/index.mjs"))
        .arg("--data")
        .arg(&data)
        .env(ADDR_ENV, &launch.addr)
        .env(TOKEN_ENV, &launch.token)
        .env("CHAPPA_FIXTURE_DATA_SAFETY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the fixture source");
    let _fixture = Reaper(Some(child));
    sources.launched(&root, "fixture-source", 9999);
    let (sid, hash) = wait_for("needs review", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::NeedsReview)
            .then(|| (s.id.clone(), s.trust.clone().unwrap().hash))
    });
    sources.trust_answer(&sid, true, &hash).unwrap();
    wait_for("every view polled", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::Up && s.views.iter().all(|v| v.has_payload)).then_some(())
    });
    let deps = ActionDeps {
        registry: registry.clone(),
        tools: tools.clone(),
        settings: SettingsState::default(),
        projects: projects.clone(),
        records: records.clone(),
        sources: sources.clone(),
        app: None,
    };
    let settings = SettingsState::default();
    let ctx = || SpawnContext {
        registry: &registry,
        tools: &tools,
        settings: &settings,
        projects: &projects,
        records: &records,
        actor: None,
        caller: Default::default(),
    };
    let spawn_in = |folder: Option<&Path>| SpawnAgentRequest {
        agent_tool_id: tool.id,
        project_id: Some(project_id),
        working_dir: folder.map(|f| f.display().to_string()),
        ..SpawnAgentRequest::default()
    };
    let start_deps = || StartDeps {
        registry: &registry,
        tools: &tools,
        settings: &settings,
        app: None,
        caller: Default::default(),
    };

    // Clean: nothing is blocked. One agent keeps running in the bound
    // folder; another leaves a stopped record there.
    assert!(sources.data_safety_blocks().is_empty());
    let running = spawn_agent_headless(ctx(), spawn_in(Some(&wt)))
        .expect("clean: an agent may start in wt")
        .process_id;
    let stopped = spawn_agent_headless(ctx(), spawn_in(Some(&wt))).unwrap();
    let stopped_record = stopped.record_id.clone().expect("a durable record");
    registry.close(stopped.process_id);

    // The check finds real-looking data.
    block_file(&data, true);
    sources.refresh_soon(&sid);
    let blocks = wait_for("the block", Duration::from_secs(20), || {
        Some(sources.data_safety_blocks()).filter(|b| !b.is_empty())
    });
    assert_eq!(blocks[0].instance, "app/0");
    assert!(blocks[0].current);
    assert_eq!(
        blocks[0].reason,
        "db: 3 findings (email-real-domain ×2, card-number ×1) — acknowledge in Environment"
    );
    assert_eq!(
        blocks[0].bound.as_ref().map(|b| b.key.as_str()),
        Some("ABC-1")
    );
    assert_eq!(blocks[0].component.as_deref(), Some("db"));
    // An app started again before the source is back has the block from the
    // start: it was kept, and reads as the last known one.
    let restarted = Sources::default();
    restarted.init_with(
        app_lib::sources::store::SourceStore::new(dir.path().join("sources.json")),
        Arc::new(Quiet),
    );
    let kept = restarted.data_safety_blocks();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].source_id, sid);
    assert!(!kept[0].current);
    assert!(
        kept[0].reason.starts_with("last known: db: 3 findings"),
        "{}",
        kept[0].reason
    );
    assert!(app_lib::data_safety::check_launch(&restarted, &wt)
        .unwrap_err()
        .starts_with(REFUSAL_LEAD));
    restarted.shutdown();
    let state = state_of(&sources, &registry);
    assert_eq!(
        state.agents.iter().map(|a| a.term_id).collect::<Vec<_>>(),
        [running],
        "the running agent in wt is listed"
    );

    // Deliveries to the running agent from the control surface are held, in order.
    let control = ControlState {
        registry: registry.clone(),
        projects: projects.clone(),
        agent_tools: tools.clone(),
        records: records.clone(),
        ..ControlState::default()
    };
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let token = "t0ken-for-tests";
    {
        let control = control.clone();
        std::thread::spawn(move || control_http::serve(server, control, token.to_owned()));
    }
    for (n, text) in ["first-held", "second-held"].iter().enumerate() {
        let (status, body) = http(
            &addr,
            "POST",
            &format!("/processes/{running}/input"),
            token,
            &json!({"text": text, "submit": true}).to_string(),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            (
                body["written"].as_bool(),
                body["held"].as_bool(),
                body["held_count"].as_u64()
            ),
            (Some(false), Some(true), Some(n as u64 + 1)),
            "{body}"
        );
        assert!(
            body["reason"]
                .as_str()
                .unwrap()
                .contains("held until data safety on app/0 clears"),
            "{body}"
        );
    }
    let (status, body) = http(
        &addr,
        "POST",
        &format!("/processes/{running}/bytes"),
        token,
        &json!({"bytes": b"third-held\r".to_vec()}).to_string(),
    );
    assert_eq!(
        (status, body["held_count"].as_u64()),
        (200, Some(3)),
        "{body}"
    );
    assert_eq!(registry.held_counts(), vec![(running, 3)]);
    assert!(
        TimerHold::hold_reason(&registry, running).is_some(),
        "a timer firing would wait too"
    );
    assert!(
        registry.input_journal(running).unwrap().is_empty(),
        "nothing held was written"
    );
    assert_eq!(
        registry.release_held(),
        0,
        "still blocked: nothing is released"
    );
    // What the person types is never held.
    registry.handle(running).unwrap().paste("typed-by-hand");
    wait_for("the typed text on screen", Duration::from_secs(10), || {
        screen_contains(&registry, running, "typed-by-hand").then_some(())
    });
    assert!(!screen_contains(&registry, running, "first-held"));

    // Every way to start an agent in wt is refused, with the reason.
    let refused = |e: &str| {
        e.starts_with(REFUSAL_LEAD)
            && e.contains("db: 3 findings")
            && e.contains("acknowledge in Environment")
    };
    let roots = projects.cwd_roots(Some(project_id), Some(sid.as_str()));
    let listed = sources
        .actions(&sid, "work", Some("ABC-1"), &tools.list(), &roots)
        .unwrap();
    let agent_here = listed
        .iter()
        .find(|a| a.action.id() == "agent-here")
        .unwrap();
    let agent_hash = agent_here.prompt_hash.clone().expect("a launch digest");
    assert!(!agent_here.verdict.enabled);
    assert!(
        agent_here
            .verdict
            .reason
            .as_deref()
            .unwrap()
            .starts_with("agents blocked — data safety on app/0: db: 3 findings"),
        "{:?}",
        agent_here.verdict
    );
    assert!(
        listed
            .iter()
            .find(|a| a.action.id() == "terminal-here")
            .unwrap()
            .verdict
            .enabled,
        "terminals are not blocked"
    );
    let agents_before = registry.agent_rows().len();
    let err = run_action_with(
        &deps,
        &sid,
        "work",
        Some("ABC-1"),
        "agent-here",
        Answers {
            confirmed: true,
            prompt_hash: Some(&agent_hash),
            ..Answers::default()
        },
    )
    .unwrap_err();
    assert!(refused(&err), "source agent action: {err}");
    let err = spawn_agent_headless(ctx(), spawn_in(Some(&wt))).unwrap_err();
    assert!(refused(&err), "app spawn: {err}");
    let (status, body) = http(
        &addr,
        "POST",
        "/agents",
        token,
        &json!({"agent_tool_id": tool.id, "project_id": project_id, "working_dir": wt.display().to_string()}).to_string(),
    );
    assert_eq!(status, 409, "{body}");
    assert!(
        refused(body["error"].as_str().unwrap()),
        "control / MCP spawn: {body}"
    );
    let err = records
        .start(start_deps(), &stopped_record, None, 0)
        .unwrap_err();
    assert_eq!(err.status(), 409);
    assert!(refused(err.message()), "record start: {err:?}");
    let err = records
        .resume(
            start_deps(),
            &stopped_record,
            Some("session-1".into()),
            None,
        )
        .unwrap_err();
    assert_eq!(err.status(), 409);
    assert!(refused(err.message()), "resume: {err:?}");
    assert_eq!(
        registry.agent_rows().len(),
        agents_before,
        "nothing was spawned"
    );
    // A terminal in wt, and an agent outside it, are fine.
    let ActionOutcome::Launched {
        term_id: terminal,
        what,
    } = run_action(
        &deps,
        &sid,
        "work",
        Some("ABC-1"),
        "terminal-here",
        false,
        None,
    )
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(what, "terminal");
    let outside = spawn_agent_headless(ctx(), spawn_in(None))
        .expect("the project root is not the bound folder");
    registry.close(outside.process_id);
    registry.close(terminal);

    // An agent may not acknowledge; a person does, by name.
    let err = run_action_as(
        &deps,
        &CallActor::Agent("7".into()),
        &sid,
        "environment",
        None,
        "data-safety-ack",
        true,
        Some("known rows"),
    )
    .unwrap_err();
    assert!(err.contains("agent:7"), "{err}");
    assert!(!sources.data_safety_blocks().is_empty());
    let done = run_action(
        &deps,
        &sid,
        "environment",
        None,
        "data-safety-ack",
        true,
        Some("fixture rows, known"),
    )
    .unwrap();
    let me = CallActor::human().header_value();
    assert_eq!(
        done,
        ActionOutcome::Done {
            message: Some(format!("Acknowledged by {me}"))
        }
    );
    wait_for("the block to lift", Duration::from_secs(20), || {
        sources.data_safety_blocks().is_empty().then_some(())
    });
    let env = sources.view(&sid, "environment").unwrap().payload.unwrap();
    assert_eq!(env["dataSafety"]["state"], "acknowledged");
    assert_eq!(env["dataSafety"]["ack"]["by"], me.as_str());

    // The held messages go out, in the order they came.
    assert_eq!(registry.release_held(), 3);
    assert!(registry.held_counts().is_empty());
    let texts: Vec<String> = registry
        .input_journal(running)
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect();
    assert_eq!(texts, ["first-held", "second-held", "third-held\r"]);
    wait_for(
        "the released text on screen",
        Duration::from_secs(10),
        || screen_contains(&registry, running, "second-held").then_some(()),
    );
    let (_, body) = http(
        &addr,
        "POST",
        &format!("/processes/{running}/input"),
        token,
        &json!({"text": "fourth", "submit": true}).to_string(),
    );
    assert_eq!(body["written"], true, "not held any more: {body}");
    assert!(state_of(&sources, &registry).agents.is_empty());

    // And the launches are allowed again.
    let ActionOutcome::Launched { what, .. } = run_action_with(
        &deps,
        &sid,
        "work",
        Some("ABC-1"),
        "agent-here",
        Answers {
            confirmed: true,
            prompt_hash: Some(&agent_hash),
            ..Answers::default()
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(what, "agent");
    records
        .start(start_deps(), &stopped_record, None, 0)
        .expect("the stopped record starts again");

    for row in registry.agent_rows() {
        registry.close(row.id);
    }
    sources.shutdown();
}
