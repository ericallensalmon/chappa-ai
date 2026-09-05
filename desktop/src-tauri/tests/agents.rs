//! Integration tests: the registry's verified close against
//! a fake docker CLI (container-side kill FIRST, host client LAST, pid file
//! removed, `docker top` never consulted, container-down short-circuit), the
//! bounded `shutdown_all` under a hung docker, and the real spawn path
//! (extra_args verbatim, identity env, ready-gated prompt, busy guards, the
//! control routes). Real ptys: `cmd` on Windows, `sh` elsewhere.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::agents::{
    reap_orphans, AgentMeta, Bridge, CloseVerification, FakeDocker, NsPid, SpawnAgentRequest,
    SpawnContext, ORPHAN_SCAN_SCRIPT,
};
use app_lib::control_http::{self, ControlState};
use app_lib::projects::Projects;
use app_lib::registry::{EventSink, JsonEvent, Registry};
use app_lib::settings::SettingsState;
use serde_json::{json, Value};
use term_core::actor::ActorConfig;
use term_core::pty::PtySpec;

struct Sink;
impl EventSink for Sink {
    fn send_binary(&self, _: Vec<u8>) {}
    fn emit_json(&self, _: JsonEvent) {}
}

/// An interactive child that echoes its LAST argument then stays alive.
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

fn spec() -> PtySpec {
    let (command, args) = echo_tool();
    PtySpec {
        command,
        args: [args, vec!["probe".into()]].concat(),
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }
}

fn docker_meta(nspid: NsPid) -> AgentMeta {
    AgentMeta {
        tool_id: 1,
        tool_type: ToolType::Opencode,
        model: Some("model-fast".into()),
        runtime: Runtime::DockerExec {
            container: "dev-worker".into(),
            user: Some("dev".into()),
            workdir: Some("/workspace/app".into()),
            tty: true,
            max_busy_in_container: None,
        },
        project_id: None,
        parent_process_id: None,
        spawned_at_ms: 0,
        spawn_uuid: "u1".into(),
        nspid,
        container: None,
        bridge: Bridge::Ok,
        transport: Default::default(),
        awaiting_input: false,
    }
}

fn agent_entry(registry: &Registry, meta: AgentMeta) -> u32 {
    let id = registry.reserve_id();
    registry
        .create_agent_terminal(
            ActorConfig {
                spec: spec(),
                scrollback_lines: 1000,
                ..ActorConfig::default()
            },
            Arc::new(Sink),
            None,
            "worker · build".into(),
            id,
            meta,
            false,
        )
        .expect("pty spawn")
}

/// The docker call "kind" the close-order assertions read: an `exec … sh -c
/// "kill …"` is the signal step (the shell builtin — worker images have no
/// `kill` binary, see `agents::signal_args`), any other exec is its program,
/// everything else is the docker verb.
fn call_kind(c: &[String]) -> &str {
    if c[0] != "exec" {
        &c[0]
    } else if c[2] == "sh" && c.get(4).is_some_and(|s| s.starts_with("kill ")) {
        "kill"
    } else {
        &c[2]
    }
}

fn wait_until<T>(mut pred: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = pred() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// TERM → poll → KILL → verify inside the container, the host client
/// killed LAST, the pid file removed after that, `docker top` never invoked.
#[test]
fn close_kills_the_container_side_first_and_the_client_last() {
    let docker = FakeDocker::new();
    docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
    docker.ok(&["exec", "dev-worker", "cat", "/tmp/.chappa-ai/u1.pid"], "4242\n");
    docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\"", "kill", "4242"], "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/4242"], ""); // gone after TERM
    docker.ok(&["exec", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/u1.pid"], "");
    let registry = Registry::with_docker(docker.clone());
    let id = agent_entry(&registry, docker_meta(NsPid::PidFile));
    let handle = registry.handle(id).unwrap();
    wait_until(|| handle.io().has_output().then_some(()));

    // Every docker call except the final `rm` must find the host client
    // still alive — that IS "client killed last".
    let probe = handle.clone();
    docker.set_on_call(move |args| {
        let is_rm = args.get(2).map(String::as_str) == Some("rm");
        assert_eq!(
            probe.io().child_alive(),
            !is_rm,
            "host client alive during {args:?}? expected {}",
            !is_rm
        );
    });

    let rows = registry.list_control();
    assert_eq!(rows[0].kind, "agent");
    assert_eq!(rows[0].agent.as_ref().unwrap().tool_id, 1);
    assert_eq!(registry.agent_rows()[0].container.as_deref(), Some("dev-worker"));

    let outcome = registry.close(id).expect("closed");
    assert_eq!(outcome.verification, Some(CloseVerification::Gone));
    assert!(!handle.io().child_alive(), "host client is dead after close");
    let calls = docker.calls();
    assert!(calls.iter().all(|c| c[0] != "top"), "docker top must never be consulted: {calls:?}");
    assert_eq!(calls.last().unwrap()[2..], ["rm", "-f", "/tmp/.chappa-ai/u1.pid"].map(String::from));
    let order: Vec<&str> = calls
        .iter()
        .map(|c| call_kind(c))
        .collect();
    assert_eq!(order, vec!["inspect", "cat", "kill", "test", "rm"]);
    assert!(registry.handle(id).is_none());
}

/// Container already down → no in-container steps at all, the client is
/// killed, the answer is `container-down`. A plain terminal has no
/// verification.
#[test]
fn close_short_circuits_on_a_down_container() {
    let docker = FakeDocker::new();
    docker.ok(&["inspect"], "exited 2026-01-01T00:00:00Z");
    let registry = Registry::with_docker(docker.clone());
    let id = agent_entry(&registry, docker_meta(NsPid::PidFile));
    let handle = registry.handle(id).unwrap();
    let outcome = registry.close(id).unwrap();
    assert_eq!(outcome.verification, Some(CloseVerification::ContainerDown));
    assert!(!handle.io().child_alive());
    assert!(docker.calls_with(&["exec"]).is_empty(), "{:?}", docker.calls());

    let plain = registry
        .create_terminal(
            ActorConfig {
                spec: spec(),
                scrollback_lines: 100,
                ..ActorConfig::default()
            },
            Arc::new(Sink),
            None,
            "shell".into(),
        )
        .unwrap();
    assert_eq!(registry.close(plain).unwrap().verification, None);
}

/// A hung docker must not hold the app open: `shutdown_all` returns within
/// 5 s, the host clients are dead, the verification is `unresolved`.
#[test]
fn shutdown_all_is_bounded_when_docker_hangs() {
    let docker = FakeDocker::new();
    docker.set_hang(true);
    let registry = Registry::with_docker(docker.clone());
    let a = agent_entry(&registry, docker_meta(NsPid::PidFile));
    let mut other = docker_meta(NsPid::PidFile);
    if let Runtime::DockerExec { container, .. } = &mut other.runtime {
        *container = "other-box".into();
    }
    let b = agent_entry(&registry, other);
    let handles = [registry.handle(a).unwrap(), registry.handle(b).unwrap()];
    let started = Instant::now();
    let results = registry.shutdown_all();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "shutdown_all took {elapsed:?}");
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, v)| *v == CloseVerification::Unresolved), "{results:?}");
    assert!(handles.iter().all(|h| !h.io().child_alive()), "clients killed regardless");
    assert!(registry.list().is_empty());
}

fn host_tool(tools: &AgentToolsState, name: &str, max_busy: Option<u32>) -> AgentTool {
    let (program, args) = echo_tool();
    tools
        .upsert(AgentTool {
            id: 0,
            name: name.into(),
            tool_type: ToolType::Custom,
            program,
            // `probe` is the tool's own last arg; extra_args land after it
            // (and become the echoed word when present).
            args: [args, vec!["probe".into()]].concat(),
            model: Some("fake-model".into()),
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy,
            transport: Default::default(),
        })
        .unwrap()
}

fn screen_contains(registry: &Registry, id: u32, needle: &str) -> bool {
    registry
        .handle(id)
        .map(|h| h.dump_text(50).iter().any(|r| r.contains(needle)))
        .unwrap_or(false)
}

/// REQUIRED: an `extra_args` element with spaces reaches the child as
/// ONE argv entry — the fake echo program prints its last argument and the
/// screen shows it intact. Plus the ready-gated prompt: delivered exactly
/// once, journaled, with a receipt.
#[test]
fn spawn_agent_passes_extra_args_verbatim_and_delivers_the_prompt_when_ready() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let tool = host_tool(&tools, "echo tool", None);
    let registry = Registry::with_docker(FakeDocker::new());
    let settings = SettingsState::default();
    let projects = Projects::default();
    let ctx = SpawnContext {
        registry: &registry,
        tools: &tools,
        settings: &settings,
        projects: &projects,
        actor: None,
    };
    let resp = app_lib::agents::spawn_agent_headless(
        ctx,
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            extra_args: vec!["a b c".into()],
            prompt: Some("echo hi-from-prompt".into()),
            name: Some("argv probe".into()),
            ..SpawnAgentRequest::default()
        },
    )
    .expect("spawn");
    let id = resp.process_id;
    assert_eq!(resp.term_id, id);
    assert_eq!(resp.name, "argv probe");
    assert!(resp.agent_instructions.contains(&format!("process {id}")));
    assert!(resp.agent_instructions.contains("chappa-ai MCP tools: list_processes"));
    assert_eq!(resp.agent.tool_id, tool.id);
    assert_eq!(resp.agent.model.as_deref(), Some("fake-model"));
    assert!(resp.container.is_none(), "host runtime: no container block");
    // Windows quotes a spaced argument on the command line (`"a b c"`),
    // sh prints it bare; either way the three words arrive TOGETHER.
    wait_until(|| screen_contains(&registry, id, "a b c").then_some(()));

    // The prompt landed once the gate opened: receipt + one journal record
    // with submit, and the echo shows on screen.
    let receipt = resp.prompt_receipt.expect("prompt receipt");
    assert!(receipt.delivered, "{receipt:?}");
    assert!(receipt.waited_ms.unwrap() >= 750, "waited for the quiet window: {receipt:?}");
    wait_until(|| screen_contains(&registry, id, "hi-from-prompt").then_some(()));
    let journal = registry.input_journal(id).unwrap();
    assert_eq!(journal.len(), 1, "exactly one delivery");
    assert_eq!(journal[0].text, "echo hi-from-prompt");
    assert!(journal[0].submit);

    // The process record is an agent row.
    let row = registry.control_snapshot(id).unwrap();
    assert_eq!(row.kind, "agent");
    assert_eq!(row.agent.unwrap().tool_type, ToolType::Custom);
    registry.close(id);
}

/// Busy guard end-to-end through the control surface: per-tool limit refuses
/// naming the busy process, `force` spawns it; unknown/disabled tools answer
/// 404/409; the tool listing carries the parity `command` plus the fields.
#[test]
fn control_routes_list_tools_and_guard_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let tool = host_tool(&tools, "guarded", Some(1));
    let registry = Registry::with_docker(FakeDocker::new());
    let state = ControlState {
        registry: registry.clone(),
        agent_tools: tools.clone(),
        ..ControlState::default()
    };
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_string();
    std::thread::spawn(move || control_http::serve(server, state, "tok".into()));

    let (status, list) = http(&addr, "GET", "/agent_tools", "");
    assert_eq!(status, 200);
    assert_eq!(list[0]["id"], tool.id);
    assert_eq!(list[0]["tool_type"], "custom");
    assert_eq!(list[0]["enabled"], true);
    assert_eq!(list[0]["model"], "fake-model");
    assert_eq!(list[0]["runtime"]["kind"], "host");
    assert_eq!(list[0]["container"], Value::Null);
    assert_eq!(list[0]["command"], tool.command_line());

    let (status, first) = http(&addr, "POST", "/agents", &json!({"agent_tool_id": tool.id, "name": "first"}).to_string());
    assert_eq!(status, 200, "{first}");
    let first_id = first["process_id"].as_u64().unwrap() as u32;
    wait_until(|| screen_contains(&registry, first_id, "probe").then_some(()));

    // Busy (output within 120 s) + max_busy 1 → refused, naming "first".
    let (status, refused) = http(&addr, "POST", "/agents", &json!({"agent_tool_id": tool.id}).to_string());
    assert_eq!(status, 409, "{refused}");
    let err = refused["error"].as_str().unwrap();
    assert!(err.contains("first (process"), "{err}");
    assert!(err.contains("last_output_at"), "{err}");
    assert_eq!(registry.list().len(), 1, "nothing spawned");

    // force → spawns, and says it was forced.
    let (status, forced) = http(&addr, "POST", "/agents", &json!({"agent_tool_id": tool.id, "force": true}).to_string());
    assert_eq!(status, 200, "{forced}");
    assert!(forced["forced"].as_str().unwrap().contains("forced past busy guard"));
    assert_eq!(registry.list().len(), 2);

    // Unknown tool / disabled tool.
    let (status, _) = http(&addr, "POST", "/agents", r#"{"agent_tool_id": 999}"#);
    assert_eq!(status, 404);
    let mut disabled = tool.clone();
    disabled.enabled = false;
    tools.upsert(disabled).unwrap();
    let (status, body) = http(&addr, "POST", "/agents", &json!({"agent_tool_id": tool.id, "force": true}).to_string());
    assert_eq!(status, 409, "{body}");

    // Delete refusal names the live process; close, then it goes.
    let err = app_lib::agent_tools::delete_refusal(tool.id, &registry.agent_rows()).unwrap();
    assert!(err.contains("first (process"), "{err}");
    let (status, closed) = http(&addr, "POST", &format!("/processes/{first_id}/close"), r#"{"confirm": true}"#);
    assert_eq!(status, 200);
    assert_eq!(closed["verification"], Value::Null, "host agent: nothing to verify");
    for row in registry.list() {
        registry.close(row.id);
    }
    assert!(app_lib::agent_tools::delete_refusal(tool.id, &registry.agent_rows()).is_none());
}

fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer tok\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

// ---- the json transport end to end ------------------------------

/// A fake PERSISTENT machine-mode CLI: per stdin line it prints a claude
/// `system/init`, an `assistant` text echoing the line, and a `result` (as
/// claude does per user message), and exits on EOF. A batch file on
/// Windows, a `sh` script elsewhere.
fn fake_json_cli(dir: &std::path::Path) -> (String, Vec<String>) {
    let init = r#"{"type":"system","subtype":"init","session_id":"fake-session","model":"fake-model"}"#;
    let result = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"usage":{"input_tokens":5,"output_tokens":2},"modelUsage":{"fake-model":{"contextWindow":1000}},"result":"pong"}"#;
    if cfg!(windows) {
        let script = dir.join("fake_json_cli.cmd");
        let body = [
            "@echo off",
            ":loop",
            "set \"line=\"",
            "set /p line=",
            "if not defined line exit /b 0",
            &format!("echo {init}"),
            "echo {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"got\"}]}}",
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
            "  echo '{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"got\"}]}}'",
            &format!("  echo '{result}'"),
            "done",
            "",
        ]
        .join("\n");
        std::fs::write(&script, body).unwrap();
        ("sh".into(), vec![script.to_string_lossy().into_owned()])
    }
}

/// Spawn a json-transport tool (claude type, fake program): machine-mode
/// flags prepended, the prompt delivered as a stream-json line with the
/// turn receipt, `awaiting_input` set after the result and cleared by the
/// next send, events in the ring, the control routes, and a close that
/// takes the child down.
#[test]
fn json_transport_spawns_over_pipes_and_flips_awaiting_input() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let (program, args) = fake_json_cli(dir.path());
    let tool = tools
        .upsert(AgentTool {
            id: 0,
            name: "fake claude".into(),
            tool_type: ToolType::Claude,
            program,
            args: args.clone(),
            model: Some("fake-model".into()),
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: app_lib::agent_tools::Transport::Json,
        })
        .unwrap();
    // A json transport on a type without a machine mode is refused at upsert.
    let mut bad = tool.clone();
    bad.id = 0;
    bad.tool_type = ToolType::Codex;
    let err = tools.upsert(bad).unwrap_err();
    assert!(err.contains("no machine mode"), "{err}");

    let registry = Registry::with_docker(FakeDocker::new());
    let state = ControlState {
        registry: registry.clone(),
        agent_tools: tools.clone(),
        ..ControlState::default()
    };
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_string();
    std::thread::spawn(move || control_http::serve(server, state, "tok".into()));

    let (status, resp) = http(
        &addr,
        "POST",
        "/agents",
        &json!({"agent_tool_id": tool.id, "prompt": "ping", "name": "json probe"}).to_string(),
    );
    assert_eq!(status, 200, "{resp}");
    let id = resp["process_id"].as_u64().unwrap() as u32;
    assert_eq!(resp["agent"]["transport"], "json");
    let receipt = &resp["prompt_receipt"];
    assert_eq!(receipt["delivered"], true, "{receipt}");
    assert!(receipt["seq_after"].as_u64().is_some(), "turn_started seq: {receipt}");

    // The child ran with the machine-mode flags FIRST, then the tool's args.
    let agent = registry.json_agent(id).expect("json agent attached");
    let recipe = agent.recipe();
    let flags: Vec<&str> = recipe.args.iter().map(String::as_str).collect();
    let first = flags.iter().position(|a| *a == "-p").unwrap();
    assert_eq!(&flags[first..first + 6], &["-p", "--output-format", "stream-json", "--input-format", "stream-json", "--verbose"]);
    assert!(flags.ends_with(&args.iter().map(String::as_str).collect::<Vec<_>>()), "{flags:?}");

    // The fake answered: text + result → awaiting_input on the record.
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));
    let row = registry.control_snapshot(id).unwrap();
    assert_eq!(row.kind, "agent");
    assert!(row.has_output, "bytes were noted on the no-pty actor");
    assert!(row.child_alive);
    let (status, events) = http(&addr, "GET", &format!("/processes/{id}/agent_events"), "");
    assert_eq!(status, 200);
    let kinds: Vec<&str> = events["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["turn_started", "text", "usage", "turn_ended", "awaiting_input"], "{events}");
    assert_eq!(events["events"][1]["payload"]["text"], "got", "the stream-json user line reached the child");
    assert_eq!(events["events"][2]["payload"]["context_pct"], 0.5);
    let last_seq = events["events"][4]["seq"].as_u64().unwrap();
    // Journaled once, as a submit.
    let journal = registry.input_journal(id).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].text, "ping");

    // send_input over the control route: json receipt, awaiting cleared.
    let (status, sent) = http(&addr, "POST", &format!("/processes/{id}/input"), r#"{"text": "again", "wait_ms": 5000}"#);
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["transport"], "json");
    assert_eq!(sent["delivered"], true, "{sent}");
    assert_eq!(sent["written"], true);
    assert_eq!(sent["seq_before"], last_seq);
    assert!(sent["turn_started_seq"].as_u64().unwrap() > last_seq);
    let (status, since) = http(&addr, "GET", &format!("/processes/{id}/agent_events?since={last_seq}"), "");
    assert_eq!(status, 200);
    assert_eq!(since["events"][0]["kind"], "turn_started");
    assert_eq!(sent["wait_ms"], 5000, "the receipt carries the effective wait");
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));
    assert_eq!(registry.input_journal(id).unwrap().len(), 2);
    // A json agent takes whole messages: `submit: false` and empty text are
    // caller errors (400), and neither reaches the journal.
    let (status, refused) = http(&addr, "POST", &format!("/processes/{id}/input"), r#"{"text": "x", "submit": false}"#);
    assert_eq!(status, 400, "{refused}");
    let (status, refused) = http(&addr, "POST", &format!("/processes/{id}/input"), r#"{"text": "   "}"#);
    assert_eq!(status, 400, "{refused}");
    assert_eq!(registry.input_journal(id).unwrap().len(), 2);
    // Raw bytes are refused on a json agent; an unknown id is 404 on the
    // events route.
    let (status, _) = http(&addr, "POST", &format!("/processes/{id}/bytes"), r#"{"bytes": [3]}"#);
    assert_eq!(status, 409);
    let (status, _) = http(&addr, "GET", "/processes/9999/agent_events", "");
    assert_eq!(status, 404);

    // Close: the child is gone, the agent with it.
    let (status, closed) = http(&addr, "POST", &format!("/processes/{id}/close"), r#"{"confirm": true}"#);
    assert_eq!(status, 200, "{closed}");
    assert!(registry.json_agent(id).is_none());
    assert!(registry.handle(id).is_none());
    let r = agent.send_input("after close", Duration::from_millis(50));
    assert_eq!(r.reason.as_deref(), Some("exited"));
}

/// A json spawn whose child cannot start leaves NO entry behind, and a
/// per-turn (opencode-shaped) tool spawned without a prompt is
/// `awaiting_input` from the start.
#[test]
fn json_transport_spawn_failure_and_per_turn_idle() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let registry = Registry::with_docker(FakeDocker::new());
    let settings = SettingsState::default();
    let projects = Projects::default();
    let missing = tools
        .upsert(AgentTool {
            id: 0,
            name: "missing".into(),
            tool_type: ToolType::Claude,
            program: dir.path().join("no-such-program").to_string_lossy().into_owned(),
            transport: app_lib::agent_tools::Transport::Json,
            enabled: true,
            ..AgentTool::default()
        })
        .unwrap();
    let ctx = || SpawnContext {
        registry: &registry,
        tools: &tools,
        settings: &settings,
        projects: &projects,
        actor: None,
    };
    let err = app_lib::agents::spawn_agent_headless(
        ctx(),
        SpawnAgentRequest {
            agent_tool_id: missing.id,
            ..SpawnAgentRequest::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("cannot spawn"), "{err}");
    assert!(registry.list().is_empty(), "no entry for a child that never started");

    let per_turn = tools
        .upsert(AgentTool {
            id: 0,
            name: "opencode-shaped".into(),
            tool_type: ToolType::Opencode,
            program: dir.path().join("no-such-program").to_string_lossy().into_owned(),
            transport: app_lib::agent_tools::Transport::Json,
            enabled: true,
            ..AgentTool::default()
        })
        .unwrap();
    let resp = app_lib::agents::spawn_agent_headless(
        ctx(),
        SpawnAgentRequest {
            agent_tool_id: per_turn.id,
            ..SpawnAgentRequest::default()
        },
    )
    .expect("no prompt = nothing runs yet");
    assert!(resp.prompt_receipt.is_none());
    let meta = registry.agent_meta(resp.process_id).unwrap();
    assert!(meta.awaiting_input, "idle from the start");
    let events = app_lib::agents::agent_events(&registry, resp.process_id, 0).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, app_lib::agent_json::AgentEventKind::AwaitingInput);
    let row = registry.control_snapshot(resp.process_id).unwrap();
    assert!(row.has_output, "review fix: a ring event ends 'booting'");
    assert!(row.child_alive, "alive between turns");
    // The first message would launch `run --format json <message>`; the
    // program is missing so the receipt says so — and (review fix) the
    // entry is RETIRED: not waiting, not alive, the reason in the ring;
    // (review fix) a refused send is NOT journaled.
    let receipt = app_lib::agents::json_send_input(&registry, resp.process_id, "hello", Some(100)).unwrap();
    assert!(!receipt.delivered);
    assert!(receipt.reason.as_deref().unwrap().starts_with("exited: cannot spawn"), "{receipt:?}");
    assert_eq!(receipt.wait_ms, 100);
    assert!(registry.input_journal(resp.process_id).unwrap().is_empty(), "refused send: no submit record");
    let meta = registry.agent_meta(resp.process_id).unwrap();
    assert!(!meta.awaiting_input, "a retired agent is never waiting");
    wait_until(|| (!registry.control_snapshot(resp.process_id).unwrap().child_alive).then_some(()));
    assert!(registry.agent_rows().iter().all(|r| !r.child_alive), "busy guard sees it retired");
    let events = app_lib::agents::agent_events(&registry, resp.process_id, 0).unwrap();
    assert_eq!(events.last().unwrap().kind, app_lib::agent_json::AgentEventKind::Error);
    let agent = registry.json_agent(resp.process_id).unwrap();
    assert_eq!(agent.child_args(None, Some("hello")), ["run", "--format", "json", "hello"]);
    assert_eq!(agent.child_args(Some("ses"), Some("hello")), ["run", "--format", "json", "-s", "ses", "hello"]);
    let again = app_lib::agents::json_send_input(&registry, resp.process_id, "again", Some(100)).unwrap();
    assert_eq!(again.reason.as_deref(), Some("exited"));
    assert!(registry.input_journal(resp.process_id).unwrap().is_empty());
    registry.close(resp.process_id);
}

/// A fake persistent CLI that announces itself (claude's `system/init`)
/// BEFORE any input, then behaves like [`fake_json_cli`].
fn fake_json_cli_ready(dir: &std::path::Path) -> (String, Vec<String>) {
    let (program, args) = fake_json_cli(dir);
    let init = r#"{"type":"system","subtype":"init","session_id":"fake-session","model":"fake-model"}"#;
    let script = std::path::PathBuf::from(args.last().unwrap());
    let body = std::fs::read_to_string(&script).unwrap();
    let body = if cfg!(windows) {
        body.replacen("@echo off\r\n", &format!("@echo off\r\necho {init}\r\n"), 1)
    } else {
        body.replacen("#!/bin/sh\n", &format!("#!/bin/sh\necho '{init}'\n"), 1)
    };
    std::fs::write(&script, body).unwrap();
    (program, args)
}

/// Review fix: a persistent json tool spawned WITHOUT a prompt — the
/// CLI's ready signal is `awaiting_input` (not a turn), `has_output` flips
/// with it, and the first send is the first turn. Review fix: the journal
/// records that send once, after it was written.
#[test]
fn json_persistent_no_prompt_spawn_is_awaiting_after_the_ready_signal() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let (program, args) = fake_json_cli_ready(dir.path());
    let tool = tools
        .upsert(AgentTool {
            id: 0,
            name: "ready claude".into(),
            tool_type: ToolType::Claude,
            program,
            args,
            transport: app_lib::agent_tools::Transport::Json,
            enabled: true,
            ..AgentTool::default()
        })
        .unwrap();
    let registry = Registry::with_docker(FakeDocker::new());
    let settings = SettingsState::default();
    let projects = Projects::default();
    let resp = app_lib::agents::spawn_agent_headless(
        SpawnContext {
            registry: &registry,
            tools: &tools,
            settings: &settings,
            projects: &projects,
            actor: None,
        },
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            ..SpawnAgentRequest::default()
        },
    )
    .unwrap();
    let id = resp.process_id;
    assert!(resp.prompt_receipt.is_none());
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));
    let row = registry.control_snapshot(id).unwrap();
    assert!(row.has_output, "ready = not booting");
    assert!(row.child_alive);
    assert!(row.agent.as_ref().unwrap().awaiting_input, "the control row derives it too");
    let events = app_lib::agents::agent_events(&registry, id, 0).unwrap();
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [app_lib::agent_json::AgentEventKind::AwaitingInput], "{events:?}");
    assert_eq!(events[0].payload["after"], "init");
    assert!(registry.input_journal(id).unwrap().is_empty(), "nothing sent yet");
    // The first send: the CLI's next init is turn 1, journaled once.
    let receipt = app_lib::agents::json_send_input(&registry, id, "ping", Some(5000)).unwrap();
    assert!(receipt.delivered, "{receipt:?}");
    assert_eq!(receipt.wait_ms, 5000);
    assert!(!registry.agent_meta(id).unwrap().awaiting_input);
    let journal = registry.input_journal(id).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].text, "ping");
    assert!(journal[0].submit);
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));
    let kinds: Vec<_> = app_lib::agents::agent_events(&registry, id, 0)
        .unwrap()
        .iter()
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(kinds, ["awaiting_input", "turn_started", "text", "usage", "turn_ended", "awaiting_input"]);
    // Close: the child dies with the entry; the agent is gone.
    registry.close(id);
    assert!(registry.json_agent(id).is_none());
    assert!(registry.agent_meta(id).is_none());
}

/// Review fix (registry side): the persistent child's exit clears
/// `awaiting_input` on every read surface — a dead agent is never waiting.
#[test]
fn json_persistent_child_exit_clears_awaiting_on_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let (program, args) = fake_json_cli(dir.path());
    let tool = tools
        .upsert(AgentTool {
            id: 0,
            name: "fake claude".into(),
            tool_type: ToolType::Claude,
            program,
            args,
            transport: app_lib::agent_tools::Transport::Json,
            enabled: true,
            ..AgentTool::default()
        })
        .unwrap();
    let registry = Registry::with_docker(FakeDocker::new());
    let settings = SettingsState::default();
    let projects = Projects::default();
    let resp = app_lib::agents::spawn_agent_headless(
        SpawnContext {
            registry: &registry,
            tools: &tools,
            settings: &settings,
            projects: &projects,
            actor: None,
        },
        SpawnAgentRequest {
            agent_tool_id: tool.id,
            prompt: Some("ping".into()),
            ..SpawnAgentRequest::default()
        },
    )
    .unwrap();
    let id = resp.process_id;
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));
    // Kill the child from outside (the CLI crashes): EOF on the reader.
    let agent = registry.json_agent(id).unwrap();
    agent.kill_child();
    wait_until(|| (!registry.control_snapshot(id).unwrap().child_alive).then_some(()));
    let meta = registry.agent_meta(id).unwrap();
    assert!(!meta.awaiting_input, "dead agent: not waiting");
    assert!(agent.exited());
    let rows = registry.list_control();
    assert!(!rows[0].agent.as_ref().unwrap().awaiting_input);
    let receipt = app_lib::agents::json_send_input(&registry, id, "late", Some(100)).unwrap();
    assert_eq!(receipt.reason.as_deref(), Some("exited"));
    assert_eq!(registry.input_journal(id).unwrap().len(), 1, "the refused send was not journaled");
    registry.close(id);
}

/// Review fix: a docker-exec JSON agent closes in the documented order —
/// container side (cat pid, TERM, verify) FIRST with the host `docker exec
/// -i` client (the piped child) still alive, THEN the client, THEN the pid
/// file. `docker top` never.
#[test]
fn json_docker_agent_close_kills_the_container_side_before_the_piped_client() {
    let docker = FakeDocker::new();
    docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
    docker.ok(&["exec", "dev-worker", "cat", "/tmp/.chappa-ai/u1.pid"], "4242\n");
    docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\"", "kill", "4242"], "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/4242"], "");
    docker.ok(&["exec", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/u1.pid"], "");
    let registry = Registry::with_docker(docker.clone());
    let dir = tempfile::tempdir().unwrap();
    let (command, args) = fake_json_cli(dir.path());
    let mut meta = docker_meta(NsPid::PidFile);
    meta.transport = app_lib::agent_tools::Transport::Json;
    let id = registry.reserve_id();
    let handle = registry
        .create_json_agent_terminal(
            ActorConfig {
                spec: spec(),
                scrollback_lines: 100,
                ..ActorConfig::default()
            },
            Arc::new(Sink),
            None,
            "json worker".into(),
            id,
            meta,
            false,
        )
        .unwrap();
    let agent = app_lib::agent_json::JsonAgent::new(
        id,
        handle.clone(),
        app_lib::agent_json::SpawnRecipe {
            command,
            args,
            cwd: None,
            env: Default::default(),
        },
        app_lib::agent_json::machine_mode(ToolType::Claude).unwrap(),
        app_lib::agent_json::JsonHooks::none(),
    );
    registry.attach_json_agent(id, agent.clone());
    agent.spawn_child(None).unwrap();
    // The fake stays up on its open stdin: the "host client" is alive.
    assert!(agent.child_alive());
    let receipt = app_lib::agents::json_send_input(&registry, id, "ping", Some(5000)).unwrap();
    assert!(receipt.delivered, "{receipt:?}");
    wait_until(|| registry.agent_meta(id).filter(|m| m.awaiting_input).map(|_| ()));

    // Every docker call except the final `rm` must find the piped client
    // still alive — that IS "client killed last".
    let probe = agent.clone();
    docker.set_on_call(move |args| {
        let is_rm = args.get(2).map(String::as_str) == Some("rm");
        assert_eq!(probe.child_alive(), !is_rm, "piped client alive during {args:?}? expected {}", !is_rm);
    });
    let outcome = registry.close(id).expect("closed");
    assert_eq!(outcome.verification, Some(CloseVerification::Gone));
    assert!(!agent.child_alive(), "piped client dead after close");
    assert!(agent.exited());
    assert!(registry.json_agent(id).is_none());
    assert!(registry.handle(id).is_none());
    let calls = docker.calls();
    assert!(calls.iter().all(|c| c[0] != "top"), "{calls:?}");
    let order: Vec<&str> = calls
        .iter()
        .map(|c| call_kind(c))
        .collect();
    assert_eq!(order, vec!["inspect", "cat", "kill", "test", "rm"]);
    // The close was quiet: no `awaiting_input` / exit events after it began.
    let kinds: Vec<&str> = agent.events_since(0).iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds.last(), Some(&"awaiting_input"));
    assert_eq!(kinds.iter().filter(|k| **k == "error").count(), 0, "{kinds:?}");
}

// ---- the docker orphan reaper ----------------------------------------

/// A docker tool store with one enabled dev-worker tool (plus an optional
/// enabled down-box tool, to prove a down container is skipped).
fn docker_tools(dir: &std::path::Path, extra_container: Option<&str>) -> AgentToolsState {
    let tools = AgentToolsState::with_path(dir.join("agent_tools.json"));
    let mut tool = AgentTool {
        id: 0,
        name: "worker".into(),
        tool_type: ToolType::Opencode,
        program: "opencode".into(),
        args: Vec::new(),
        model: None,
        runtime: Runtime::DockerExec {
            container: "dev-worker".into(),
            user: Some("dev".into()),
            workdir: None,
            tty: true,
            max_busy_in_container: None,
        },
        env: Default::default(),
        enabled: true,
        max_busy: None,
        transport: Default::default(),
    };
    tools.upsert(tool.clone()).unwrap();
    if let Some(container) = extra_container {
        tool.id = 0;
        tool.name = "down".into();
        tool.runtime = Runtime::DockerExec {
            container: container.into(),
            user: Some("dev".into()),
            workdir: None,
            tty: true,
            max_busy_in_container: None,
        };
        tools.upsert(tool).unwrap();
    }
    tools
}

/// One scan call's argv for dev-worker as the tool user.
fn scan_call() -> Vec<String> {
    ["exec", "-u", "dev", "dev-worker", "sh", "-c", ORPHAN_SCAN_SCRIPT]
        .map(String::from)
        .to_vec()
}

/// `FakeDocker` matches prefixes as `&[&str]`, but the argv helpers below
/// must own their strings (they interpolate a pid and a signal name), so
/// borrow the owned argv once at the call site.
fn strs(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

/// A reap signal argv as the tool user.
fn signal_call(pid: u32, signal: &str) -> Vec<String> {
    ["exec", "-u", "dev", "dev-worker", "sh", "-c", &format!("kill -{signal} \"$1\""), "kill", &pid.to_string()]
        .map(String::from)
        .to_vec()
}

/// The full sweep against FakeDocker: EXACTLY ONE scan per container, the
/// shared TERM/poll/KILL sequence per orphan (rooted at the SMALLEST pid per
/// uuid), each reaped uuid's pid file removed, `docker top` never consulted,
/// and a down container skipped silently.
#[test]
fn sweep_reaps_orphans_one_scan_per_container_and_skips_a_down_one() {
    let dir = tempfile::tempdir().unwrap();
    let tools = docker_tools(dir.path(), Some("down-box"));
    let docker = FakeDocker::new();
    // dev-worker is running; down-box is exited — a down container is skipped
    // without a scan.
    docker.ok(
        &["inspect", "-f", "{{.State.Status}} {{.State.StartedAt}}", "dev-worker"],
        "running 2026-01-01T00:00:00Z",
    );
    docker.ok(
        &["inspect", "-f", "{{.State.Status}} {{.State.StartedAt}}", "down-box"],
        "exited 2026-01-01T00:00:00Z",
    );
    let registry = Registry::with_docker(docker.clone());

    // The scan sees orphan orph1 as two pids (532 root + 6927 child) and
    // orph2 as one; all spawned long ago (outside the busy window).
    docker.ok(&strs(&scan_call()), "532 orph1 0\n6927 orph1 0\n8875 orph2 0\n");
    // 532 obeys TERM: poll gone → Gone, pid file removed.
    docker.ok(&strs(&signal_call(532, "TERM")), "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/532"], "");
    docker.ok(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph1.pid"], "");
    // 8875 too.
    docker.ok(&strs(&signal_call(8875, "TERM")), "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/8875"], "");
    docker.ok(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph2.pid"], "");

    let reaped_notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
    let emit: Arc<dyn Fn(&str, usize) + Send + Sync> = {
        let log = reaped_notifications.clone();
        Arc::new(move |container: &str, count: usize| log.lock().unwrap().push((container.to_owned(), count)))
    };
    let results = reap_orphans(&registry, &tools, None, Some(&*emit));
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0].uuid, "orph1");
    assert_eq!(results[0].pid, 532, "the SMALLEST pid per uuid is the root");
    assert_eq!(results[0].verdict, CloseVerification::Gone);
    assert_eq!(results[1].uuid, "orph2");
    assert_eq!(results[1].verdict, CloseVerification::Gone);
    // ONE notification per container that reaped anything.
    assert_eq!(
        reaped_notifications.lock().unwrap().clone(),
        vec![("dev-worker".to_owned(), 2)]
    );

    // Exactly one scan per container, as the tool user.
    assert_eq!(docker.calls_with(&["exec", "-u", "dev", "dev-worker", "sh", "-c", ORPHAN_SCAN_SCRIPT]).len(), 1);
    // The root's sibling (6927) is never signalled.
    assert!(docker.calls_with(&strs(&signal_call(6927, "TERM"))).is_empty(), "{:?}", docker.calls());
    // Each reaped uuid's pid file is removed.
    assert_eq!(docker.calls_with(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph1.pid"]).len(), 1);
    assert_eq!(docker.calls_with(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph2.pid"]).len(), 1);
    // Down container: inspected but never scanned.
    assert!(docker.calls_with(&["exec", "-u", "dev", "down-box"]).is_empty(), "{:?}", docker.calls());
    // docker top is never consulted.
    assert!(docker.calls().iter().all(|c| c[0] != "top"), "{:?}", docker.calls());
}

/// A sweep with the setting OFF still works through the EXPLICIT path — the
/// `agentReapOrphans` setting gates only the automatic startup/probe sweeps.
#[test]
fn reap_route_is_not_gated_by_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    let tools = docker_tools(dir.path(), None);
    let docker = FakeDocker::new();
    docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
    docker.ok(&strs(&scan_call()), "532 orph1 0\n");
    docker.ok(&strs(&signal_call(532, "TERM")), "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/532"], "");
    docker.ok(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph1.pid"], "");
    let registry = Registry::with_docker(docker.clone());

    // The control route calls reap_orphans unconditionally (no perishable
    // setting gate), exactly as `POST /agents/reap` does.
    let results = reap_orphans(&registry, &tools, None, None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].verdict, CloseVerification::Gone);
    let expected = serde_json::to_value(&results).unwrap();
    // The ReapResult serializes as the route's per-orphan verdict object.
    assert_eq!(expected[0]["container"], "dev-worker");
    assert_eq!(expected[0]["pid"], 532);
    assert_eq!(expected[0]["uuid"], "orph1");
    assert_eq!(expected[0]["verdict"], "gone");
    assert_eq!(expected[0]["user"], "dev");
}

/// `POST /agents/reap` through the real control surface returns the
/// per-orphan verdicts.
#[test]
fn control_reap_route_returns_per_orphan_verdicts() {
    let dir = tempfile::tempdir().unwrap();
    let tools = docker_tools(dir.path(), None);
    let docker = FakeDocker::new();
    docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
    docker.ok(&strs(&scan_call()), "532 orph1 0\n8875 orph2 0\n");
    docker.ok(&strs(&signal_call(532, "TERM")), "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/532"], "");
    docker.ok(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph1.pid"], "");
    docker.ok(&strs(&signal_call(8875, "TERM")), "");
    docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/8875"], "");
    docker.ok(&["exec", "-u", "dev", "dev-worker", "rm", "-f", "/tmp/.chappa-ai/orph2.pid"], "");
    let registry = Registry::with_docker(docker.clone());
    let state = ControlState {
        registry: registry.clone(),
        agent_tools: tools.clone(),
        ..ControlState::default()
    };
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_string();
    std::thread::spawn(move || control_http::serve(server, state, "tok".into()));

    let (status, body) = http(&addr, "POST", "/agents/reap", "{}");
    assert_eq!(status, 200, "{body}");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    let verdicts: Vec<&str> = results.iter().map(|r| r["verdict"].as_str().unwrap()).collect();
    assert_eq!(verdicts, ["gone", "gone"]);
    assert_eq!(results[0]["container"], "dev-worker");
    assert_eq!(results[0]["uuid"], "orph1");
    assert_eq!(results[0]["pid"], 532);
}

