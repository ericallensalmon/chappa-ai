//! An agent CLI with a permission system of its own is launched with deny
//! rules for the app's folders, through the real spawn path: the command
//! line names one settings file made for the launch, that file holds the
//! tool's own settings plus the rules (never a rule for the agent's own
//! project), and it is gone once the row exits.
//!
//! The "CLI" is a shell script that records its arguments and copies the
//! settings file it was given, so the test reads exactly what a real CLI
//! would have read.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::control_http::{self, ControlState};
use app_lib::coordination::Coordination;
use app_lib::launch_deny::LAUNCH_SUBDIR;
use app_lib::projects::Projects;
use app_lib::registry::Registry;
use app_lib::settings::SettingsState;
use app_lib::timers::TimerService;
use project_model::projects::ProjectStore;
use serde_json::{json, Value};

const PERSON: &str = "the-persons-token-for-tests";

fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {PERSON}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// `//` + the folder's resolved path: how the rules name it.
fn rule_root(p: &Path) -> String {
    format!("/{}", std::fs::canonicalize(p).unwrap().display())
}

#[test]
fn a_claude_launch_carries_deny_rules_for_the_app_folders_and_not_its_own_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let config = dir.path().join("config");
    let data = dir.path().join("data");
    let seen = dir.path().join("seen");
    for d in [&root, &config, &data, &seen] {
        std::fs::create_dir_all(d).unwrap();
    }
    // The fake CLI: records its argv and the settings file it was handed
    // (the word after --settings), then stays interactive until told to exit.
    let program = dir.path().join("fake-cli");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{s}/args'\ncp \"$2\" '{s}/settings.json'\nexec sh\n",
            s = seen.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The person's own settings for the tool, with a rule of their own.
    let own = dir.path().join("own-settings.json");
    std::fs::write(
        &own,
        r#"{"permissions": {"deny": ["Bash(curl *)"], "allow": ["Read"]}}"#,
    )
    .unwrap();

    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    let project_id = store.add(root.clone(), "app".into(), None).unwrap().id;
    let projects = Projects::with_store(store);
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let tool_id = tools
        .upsert(AgentTool {
            id: 0,
            name: "claude".into(),
            tool_type: ToolType::Claude,
            program: program.display().to_string(),
            args: vec!["--settings".into(), own.display().to_string()],
            model: None,
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: Default::default(),
            resume_argv: Vec::new(),
            prompt_argv: Vec::new(),
            session_capture: Default::default(),
            hooks_opt_out: true,
            agent_args_allowed: Vec::new(),
            agent_args_any: false,
        })
        .unwrap()
        .id;
    let registry = Registry::default();
    registry
        .launch_files()
        .configure(vec![config.clone(), data.clone()], data.join(LAUNCH_SUBDIR));
    let settings = SettingsState::default();
    std::fs::write(
        config.join("settings.json"),
        json!({"secretScanBeforeAgents": false}).to_string(),
    )
    .unwrap();
    settings.init(config.clone());
    let coordination = Coordination::in_memory().unwrap();
    let timers = TimerService::start(registry.clone(), settings.clone(), coordination.clone());
    registry.set_spawn_gate(std::sync::Arc::new(app_lib::spawn_gate::AppSpawnGate {
        projects: projects.clone(),
        settings: settings.clone(),
    }));
    let state = ControlState {
        registry: registry.clone(),
        projects,
        settings,
        agent_tools: tools,
        coordination,
        timers,
        ..ControlState::default()
    };
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    std::thread::spawn(move || control_http::serve(server, state, PERSON.to_owned()));

    let (status, body) = http(
        &addr,
        "POST",
        "/agents",
        &json!({"agent_tool_id": tool_id, "project_id": project_id}).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let id = body["process_id"].as_u64().unwrap();
    let copied = seen.join("settings.json");
    wait_for(
        "the CLI's copy of its settings",
        Duration::from_secs(15),
        || {
            std::fs::read_to_string(&copied)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        },
    );

    // One --settings, naming a launch file in the app's data folder.
    let args: Vec<String> = std::fs::read_to_string(seen.join("args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        args.iter().filter(|a| *a == "--settings").count(),
        1,
        "{args:?}"
    );
    assert_eq!(args[0], "--settings");
    let launch_file = std::path::PathBuf::from(&args[1]);
    assert!(
        launch_file.starts_with(data.join(LAUNCH_SUBDIR)),
        "{}",
        launch_file.display()
    );
    assert!(launch_file.exists());

    let written: Value = serde_json::from_str(&std::fs::read_to_string(&copied).unwrap()).unwrap();
    let deny: Vec<&str> = written["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // The person's own settings are kept.
    assert!(deny.contains(&"Bash(curl *)"), "{deny:?}");
    assert_eq!(written["permissions"]["allow"], json!(["Read"]));
    // The app's folders (the launch file's among them) are denied to its
    // file tools, both reading and changing.
    for folder in [&config, &data] {
        let r = rule_root(folder);
        for tool in ["Read", "Edit"] {
            let rule = format!("{tool}({r}/**)");
            assert!(deny.contains(&rule.as_str()), "{rule} missing: {deny:?}");
        }
    }
    // Its own project is not.
    let own_root = rule_root(&root);
    assert!(
        !deny.iter().any(|r| r.contains(&own_root)),
        "its own project is denied: {deny:?}"
    );

    // The row's exit removes the launch file.
    let (status, _) = http(
        &addr,
        "POST",
        &format!("/processes/{id}/input"),
        &json!({"text": "exit", "submit": true}).to_string(),
    );
    assert_eq!(status, 200);
    wait_for("the launch file's removal", Duration::from_secs(15), || {
        (!launch_file.exists()).then_some(())
    });
    registry.close(id as u32);
}
