//! Source actions end to end against the example source in
//! `desktop/sources/fixture`: the judged action list (a mutating endpoint
//! without a confirm is not offered; a folder outside every root disables
//! with its reason), every endpoint outcome (200, 202 polled, 202 with a
//! `run` hosted in a pane and reported back on exit, 503, the note rule,
//! the confirm rule), the link and panel rules, the agent launch with its
//! prompt on the command line and the source as the journal actor, the
//! prompt-shown policy, and `working_dir` on a plain spawn.
//!
//! The fixture is a Node script; the tests skip themselves (and say so)
//! when `node` is not on PATH. Its data is copied to a temp dir with the
//! placeholder folder `/srv/example/abc-1` rewritten to a real folder inside
//! a temp project, so the cwd roots can be exercised for real.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::agent_tools::{AgentTool, AgentToolsState, Runtime, ToolType};
use app_lib::agents::{spawn_agent_headless, FakeDocker, SpawnAgentRequest, SpawnContext, PROMPT_VIA_ARGV};
use app_lib::projects::Projects;
use app_lib::records::Records;
use app_lib::registry::{EventSink, JsonEvent, Registry};
use term_core::actor::ActorConfig;
use term_core::pty::PtySpec;
use app_lib::settings::SettingsState;
use app_lib::source_actions::{run_action, ActionDeps, ActionOutcome};
use app_lib::sources::identity::{ADDR_ENV, TOKEN_ENV};
use app_lib::sources::{PromptPolicy, SourceStatus, Sources, SourcesEmitter};
use project_model::projects::ProjectStore;
use serde_json::Value;

struct Collector(Mutex<Vec<Value>>);

struct Sink;
impl EventSink for Sink {
    fn send_binary(&self, _: Vec<u8>) {}
    fn emit_json(&self, _: JsonEvent) {}
}

impl SourcesEmitter for Collector {
    fn emit(&self, payload: Value) {
        self.0.lock().unwrap().push(payload);
    }
}

fn node_available() -> bool {
    Command::new("node").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().map_or(false, |s| s.success())
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

/// An interactive child that echoes its LAST argument then stays alive —
/// so a prompt passed on the command line shows up on screen.
#[cfg(windows)]
fn echo_tool() -> (String, Vec<String>) {
    ("cmd".into(), vec!["/K".into(), "echo".into()])
}

#[cfg(not(windows))]
fn echo_tool() -> (String, Vec<String>) {
    ("sh".into(), vec!["-c".into(), "echo \"$1\"; exec sh".into(), "sh".into()])
}

fn screen_contains(registry: &Registry, id: u32, needle: &str) -> bool {
    registry.handle(id).map(|h| h.dump_text(50).iter().any(|r| r.contains(needle))).unwrap_or(false)
}

/// The whole stage: a temp project with a `wt` folder, the fixture's data
/// rewritten to point there, a tool named `claude` (the action's tool), the
/// registries, and the source launched and trusted.
struct Stage {
    _dir: tempfile::TempDir,
    _fixture: Reaper,
    root: PathBuf,
    wt: PathBuf,
    project_id: u32,
    source_id: String,
    /// The source's own process row (where its endpoint calls are journaled).
    source_row: u32,
    tool: AgentTool,
    deps: ActionDeps,
}

fn stage() -> Option<Stage> {
    if !node_available() {
        eprintln!("skipping: `node` is not on PATH, the fixture source cannot run");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    // The fixture's data, with its placeholder folder made real.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture/data");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let real = wt.display().to_string().replace('\\', "/");
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap().replace("/srv/example/abc-1", &real);
        std::fs::write(data.join(entry.file_name()), text).unwrap();
    }

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
        })
        .unwrap();
    // The source's process row: a plain shell standing in for the launched
    // command, so endpoint calls have a row to be journaled on.
    let registry = Registry::with_docker(FakeDocker::new());
    let (shell, shell_args) = echo_tool();
    let source_row = registry
        .create_terminal(
            ActorConfig {
                spec: PtySpec { command: shell, args: [shell_args, vec!["source".into()]].concat(), cwd: None, env: Vec::new(), cols: 80, rows: 24 },
                scrollback_lines: 1000,
                ..ActorConfig::default()
            },
            Arc::new(Sink),
            None,
            "fixture-source".into(),
        )
        .unwrap();
    let sources = Sources::default();
    sources.init_with(app_lib::sources::store::SourceStore::new(dir.path().join("sources.json")), Arc::new(Collector(Mutex::new(Vec::new()))));
    let launch = sources.prepare_launch(project_id, &root, "fixture-source").unwrap();
    let child = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture/index.mjs"))
        .arg("--data")
        .arg(&data)
        .env(ADDR_ENV, &launch.addr)
        .env(TOKEN_ENV, &launch.token)
        // A polled job finishes quickly, so the poller is exercised, not waited on.
        .env("CHAPPA_FIXTURE_JOB_MS", "300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the fixture source");
    sources.launched(&root, "fixture-source", source_row);
    let (source_id, hash) = wait_for("needs review", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::NeedsReview).then(|| (s.id.clone(), s.trust.clone().unwrap().hash))
    });
    sources.trust_answer(&source_id, true, &hash).unwrap();
    wait_for("every view polled", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::Up && s.views.iter().all(|v| v.has_payload)).then_some(())
    });
    let deps = ActionDeps {
        registry,
        tools,
        settings: SettingsState::default(),
        projects,
        records: Records::default(),
        sources,
        app: None,
    };
    Some(Stage { _dir: dir, _fixture: Reaper(Some(child)), root, wt, project_id, source_id, source_row, tool, deps })
}

#[test]
fn the_fixture_actions_are_judged_called_run_and_journaled() {
    let Some(st) = stage() else { return };
    let (deps, sid) = (&st.deps, st.source_id.as_str());
    let run = |view: &str, key: Option<&str>, id: &str, confirmed: bool, note: Option<&str>| run_action(deps, sid, view, key, id, confirmed, note);

    // The judged list of ABC-1: everything offered except nothing hidden
    // here; the agent action resolves the tool named `claude` and the real
    // folder, and needs its prompt shown (first time).
    let roots = deps.projects.cwd_roots(Some(st.project_id));
    let listed = deps.sources.actions(sid, "work", Some("ABC-1"), &deps.tools.list(), &roots).unwrap();
    let ids: Vec<&str> = listed.iter().map(|a| a.action.id()).collect();
    // The item's own actions first, then the one under its section.
    assert_eq!(ids, ["agent-here", "terminal-here", "approve", "start-build", "run-seed", "flaky-deploy", "ping", "open-item", "validate", "open-docs"]);
    let agent = listed.iter().find(|a| a.action.id() == "agent-here").unwrap();
    assert!(agent.verdict.enabled, "{:?}", agent.verdict);
    assert_eq!(agent.verdict.tool, Some((st.tool.id, "claude".into())));
    assert!(agent.needs_prompt);
    assert!(listed.iter().all(|a| a.verdict.enabled), "{listed:?}");
    // With no roots at all the folder actions are disabled with the reason.
    let bare = deps.sources.actions(sid, "work", Some("ABC-1"), &deps.tools.list(), &[]).unwrap();
    let agent = bare.iter().find(|a| a.action.id() == "agent-here").unwrap();
    assert!(!agent.verdict.enabled);
    assert!(agent.verdict.reason.as_deref().unwrap().contains("no project is open"), "{:?}", agent.verdict);
    // A mutating endpoint without a confirm is not in the list and cannot run.
    let pr = deps.sources.actions(sid, "prs", Some("github:owner/repo:#1"), &deps.tools.list(), &roots).unwrap();
    assert_eq!(pr.iter().map(|a| a.action.id()).collect::<Vec<_>>(), ["open"]);
    assert!(run("prs", Some("github:owner/repo:#1"), "unsafe", true, None).unwrap_err().contains("not offered"));
    assert!(run("work", Some("ABC-1"), "nope", true, None).unwrap_err().contains("offers no action"));

    // 200: done, with the message.
    assert_eq!(run("work", Some("ABC-1"), "ping", false, None).unwrap(), ActionOutcome::Done { message: Some("pong".into()) });
    // A mutating action needs the confirm; the fixture refuses an
    // unconfirmed one too (defence in depth), which the client sees as 409.
    assert!(run("work", Some("ABC-1"), "approve", false, None).unwrap_err().contains("must be confirmed"));
    let refused = deps.sources.call_endpoint(sid, &serde_json::json!({"id": "approve", "target": {"view": "work", "key": "ABC-1"}})).unwrap();
    assert_eq!(refused.status, 409);
    assert_eq!(run("work", Some("ABC-1"), "approve", true, Some("looks good")).unwrap(), ActionOutcome::Done { message: Some("Approved".into()) });
    // The call was journaled on the source's own row under its actor.
    let journal = deps.registry.input_journal(st.source_row).unwrap();
    assert!(journal.iter().any(|r| r.actor.as_deref() == Some("source:fixture") && r.text.contains("action `approve` on work/ABC-1 → 200")), "{journal:?}");

    // 503: uncertain, never retried; the note rule first.
    assert!(run("work", Some("ABC-1"), "flaky-deploy", true, None).unwrap_err().contains("needs a note"));
    match run("work", Some("ABC-1"), "flaky-deploy", true, Some("go")).unwrap() {
        ActionOutcome::Uncertain { message } => assert!(message.contains("may or may not have happened") && message.contains("did not answer"), "{message}"),
        other => panic!("{other:?}"),
    }

    // 202 without a run: polled until the source says done.
    let ActionOutcome::Job { job_id } = run("work", Some("ABC-1"), "start-build", true, None).unwrap() else { panic!() };
    let job = wait_for("the polled job to finish", Duration::from_secs(20), || {
        deps.sources.job(sid, &job_id).filter(|j| j.state != "running")
    });
    assert_eq!((job.state.as_str(), job.run, job.log.as_deref()), ("done", false, Some("finished")));
    assert_eq!(job.action_id, "start-build");

    // 202 with a run: the job's argv runs in a pane of the project, and the
    // exit report (pane text included) reaches the source.
    let ActionOutcome::Run { job_id, term_id } = run("work", Some("ABC-1"), "run-seed", true, None).unwrap() else { panic!() };
    let job = wait_for("the run to exit and report", Duration::from_secs(20), || {
        deps.sources.job(sid, &job_id).filter(|j| j.state != "running")
    });
    assert_eq!((job.state.as_str(), job.run, job.term_id, job.exit_code), ("done", true, Some(term_id), Some(0)), "{job:?}");
    assert!(job.log.is_none(), "a delivered report leaves no complaint: {job:?}");
    let remote = deps.sources.job_get(sid, &job_id).unwrap();
    assert_eq!(remote["state"], "done");
    assert!(remote["log"].as_str().unwrap().contains("seeded"), "{remote}");
    let row = deps.registry.control_snapshot(term_id).unwrap();
    assert_eq!(row.kind, "shell", "a run pane is an ordinary terminal row");
    let journal = deps.registry.input_journal(term_id).unwrap();
    assert_eq!(journal[0].actor.as_deref(), Some("source:fixture"));
    assert_eq!(journal[0].via.as_deref(), Some("launch"));
    assert!(journal[0].text.contains("node"), "{journal:?}");

    // Links: the host rule, exact. Panels: only payload-named paths.
    deps.sources.link_check(sid, "https://registry.example/tracker/ABC-1").unwrap();
    assert!(deps.sources.link_check(sid, "https://evil.example/").unwrap_err().contains("not one of the hosts"));
    assert!(deps.sources.link_check(sid, "http://registry.example/").unwrap_err().contains("only https"));
    assert!(deps.sources.link_check(sid, "https://api.registry.example/").is_err());
    match run("work", Some("ABC-1"), "validate", false, None).unwrap() {
        ActionOutcome::Panel { payload } => assert_eq!(payload["title"], "Validate"),
        other => panic!("{other:?}"),
    }
    assert!(deps.sources.panel(sid, "/api/work").unwrap_err().contains("names no panel"));
    assert!(deps.sources.panel(sid, "/api/panel/../work").is_err());

    // A panel page's own actions: listed, judged and run as the
    // `panel:<path>` view once the page was fetched, and only then.
    let checks = "panel:/api/panel/checks";
    assert!(deps.sources.actions(sid, checks, None, &deps.tools.list(), &roots).unwrap_err().contains("open it first"));
    assert!(run(checks, None, "rerun-checks", false, None).unwrap_err().contains("open it first"));
    match run("prs", Some("github:owner/repo:#2"), "checks", false, None).unwrap() {
        ActionOutcome::Panel { payload } => assert_eq!(payload["title"], "Checks for #2"),
        other => panic!("{other:?}"),
    }
    let page = deps.sources.actions(sid, checks, None, &deps.tools.list(), &roots).unwrap();
    // The page's own two, then its banner's (a banner belongs to the page).
    assert_eq!(page.iter().map(|a| a.action.id()).collect::<Vec<_>>(), ["rerun-checks", "validate", "rerun-checks"]);
    assert!(page.iter().all(|a| a.verdict.enabled), "{page:?}");
    assert_eq!(run(checks, None, "rerun-checks", false, None).unwrap(), ActionOutcome::Done { message: Some("Checks restarted".into()) });
    let journal = deps.registry.input_journal(st.source_row).unwrap();
    assert!(journal.iter().any(|r| r.text.contains("action `rerun-checks` on panel:/api/panel/checks → 200")), "{journal:?}");
    // A panel opened from a panel: its path is one a view names (the work
    // item's), so it opens; one no view names is refused even when a page
    // links it, and a path never named cannot be addressed at all.
    match run(checks, None, "validate", false, None).unwrap() {
        ActionOutcome::Panel { payload } => assert_eq!(payload["title"], "Validate"),
        other => panic!("{other:?}"),
    }
    assert!(deps.sources.actions(sid, "panel:/api/panel/nowhere", None, &deps.tools.list(), &roots).unwrap_err().contains("names no panel"));
    assert!(deps.sources.panel(sid, "/api/panel/nowhere").unwrap_err().contains("names no panel"));
    // View-level banners: their actions are the view's own (no key).
    let prs_own = deps.sources.actions(sid, "prs", None, &deps.tools.list(), &roots).unwrap();
    assert_eq!(prs_own.iter().map(|a| a.action.id()).collect::<Vec<_>>(), ["resync"]);
    assert_eq!(run("prs", None, "resync", false, None).unwrap(), ActionOutcome::Done { message: Some("Synced".into()) });
    let env_own = deps.sources.actions(sid, "environment", None, &deps.tools.list(), &roots).unwrap();
    assert_eq!(env_own.iter().map(|a| a.action.id()).collect::<Vec<_>>(), ["check-images"]);
    assert!(deps.sources.actions(sid, "environment", Some("stale-images"), &deps.tools.list(), &roots).unwrap().is_empty(), "a banner id is not a key");

    // A terminal action: the argv in the checked folder, journaled.
    let ActionOutcome::Launched { term_id, what } = run("work", Some("ABC-1"), "terminal-here", false, None).unwrap() else { panic!() };
    assert_eq!(what, "terminal");
    let journal = deps.registry.input_journal(term_id).unwrap();
    assert!(journal[0].text.contains("git status"), "{journal:?}");
    assert_eq!(journal[0].actor.as_deref(), Some("source:fixture"));

    // The agent action: the prompt must be shown first; once accepted the
    // prompt rides the command line (nothing typed) and the row is
    // journaled under the source. Afterwards the prompt counts as shown.
    assert!(run("work", Some("ABC-1"), "agent-here", false, None).unwrap_err().contains("must be shown"));
    let ActionOutcome::Launched { term_id, what } = run("work", Some("ABC-1"), "agent-here", true, None).unwrap() else { panic!() };
    assert_eq!(what, "agent");
    wait_for("the argv prompt on screen", Duration::from_secs(10), || screen_contains(&deps.registry, term_id, "summarise what is left").then_some(()));
    let journal = deps.registry.input_journal(term_id).unwrap();
    assert_eq!(journal.len(), 2, "{journal:?}");
    assert_eq!((journal[0].via.as_deref(), journal[0].actor.as_deref()), (Some("launch"), Some("source:fixture")));
    assert_eq!((journal[1].via.as_deref(), journal[1].submit, journal[1].bytes_len), (Some(PROMPT_VIA_ARGV), true, 0));
    assert_eq!(journal[1].text, "Review ABC-1 and summarise what is left.");
    let meta = deps.registry.agent_meta(term_id).unwrap();
    assert_eq!(meta.project_id, Some(st.project_id));
    assert!(!deps.sources.prompt_needs_dialog(sid, "agent-here").unwrap(), "shown once");
    assert!(deps.sources.listing().sources[0].prompts_shown.contains(&"agent-here".to_owned()));
    assert!(run("work", Some("ABC-1"), "agent-here", false, None).is_ok(), "no dialog needed the second time");
    deps.sources.set_prompt_policy(sid, PromptPolicy::Always).unwrap();
    assert!(deps.sources.prompt_needs_dialog(sid, "agent-here").unwrap());
    assert!(run("work", Some("ABC-1"), "agent-here", false, None).unwrap_err().contains("must be shown"));

    for row in deps.registry.agent_rows() {
        deps.registry.close(row.id);
    }
    deps.registry.close(st.source_row);
    deps.sources.shutdown();
    let _ = (&st.root, &st.wt);
}

/// `working_dir` on a plain spawn: a folder inside the project (or a
/// worktree of it) is accepted and remembered on the record, anything
/// else is refused with the reason before a process exists.
#[test]
fn spawn_working_dir_is_checked_against_the_roots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let inside = root.join("sub");
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    let project_id = store.add(root.clone(), "app".into(), None).unwrap().id;
    let projects = Projects::with_store(store);
    let tools = AgentToolsState::with_path(dir.path().join("agent_tools.json"));
    let (program, args) = echo_tool();
    let tool = tools
        .upsert(AgentTool {
            id: 0,
            name: "echo".into(),
            tool_type: ToolType::Custom,
            program,
            args: [args, vec!["probe".into()]].concat(),
            model: None,
            runtime: Runtime::Host,
            env: Default::default(),
            enabled: true,
            max_busy: None,
            transport: Default::default(),
            resume_argv: Vec::new(),
            prompt_argv: Vec::new(),
            session_capture: Default::default(),
            hooks_opt_out: false,
        })
        .unwrap();
    let registry = Registry::with_docker(FakeDocker::new());
    let settings = SettingsState::default();
    let records = Records::default();
    let ctx = || SpawnContext { registry: &registry, tools: &tools, settings: &settings, projects: &projects, records: &records, actor: None };
    let req = |working_dir: &Path| SpawnAgentRequest {
        agent_tool_id: tool.id,
        project_id: Some(project_id),
        working_dir: Some(working_dir.display().to_string()),
        ..SpawnAgentRequest::default()
    };
    let err = spawn_agent_headless(ctx(), req(&outside)).unwrap_err();
    assert!(err.contains("not inside an open project"), "{err}");
    assert!(registry.agent_rows().is_empty(), "nothing was spawned");
    let err = spawn_agent_headless(ctx(), req(&root.join("sub").join("..").join("..").join("elsewhere"))).unwrap_err();
    assert!(err.contains("`..`"), "{err}");
    let err = spawn_agent_headless(ctx(), SpawnAgentRequest { env: [("lower".to_owned(), "x".to_owned())].into_iter().collect(), ..req(&inside) }).unwrap_err();
    assert!(err.contains("env name"), "{err}");
    let resp = spawn_agent_headless(ctx(), req(&inside)).expect("inside the project");
    let meta = registry.agent_meta(resp.process_id).unwrap();
    assert_eq!(meta.project_id, Some(project_id));
    registry.close(resp.process_id);
}
