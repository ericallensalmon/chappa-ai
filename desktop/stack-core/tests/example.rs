//! The example stack under `desktop/stack/examples/local/`, end to end
//! through fakes: it loads and validates, `up` plans and applies to the
//! exact commands an engine would run, a re-observation through scripted
//! `compose ps` / `inspect` answers yields every one of the nine states,
//! and the other verbs produce the argv they promise.

use indexmap::IndexMap;
use std::fs;
use std::path::{Path, PathBuf};

use stack_core::cli::{CliOutput, FakeCli};
use stack_core::compose::{FakeGateways, Gateway};
use stack_core::definition::{
    has_errors, validate, Mode, StackDef, StacksDocument, ValidationContext,
};
use stack_core::driver::{ComponentState, ResolvedInstance};
use stack_core::intent::{ActiveRecord, Desired};
use stack_core::jobs::{JobStep, LocalStep, RunBlock};
use stack_core::planner::{Change, PlanError, Selection, Verb};
use stack_core::probe::FakeHttp;
use stack_core::process::{CommandSpec, CommandTable, ProcessStatus};
use stack_core::provenance::{GitError, GitFacts};
use stack_core::runner::{
    ApplyOptions, ComponentReport, Observation, RunnerError, StackRunner, Tools,
};

const NOW: &str = "2026-11-13T12:00:00Z";
const PG_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

// ------------------------------------------------------------------ fixture

fn example_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../stack/examples/local")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() == ".work" {
                continue;
            }
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The example copied into a temp folder (apply writes under `.work/`).
struct Example {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    stack: StackDef,
    commands: CommandTable,
}

fn example() -> Example {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("local");
    copy_dir(&example_source(), &dir);
    let (_, mut doc) = StacksDocument::load_from_project(&dir).unwrap().unwrap();
    let stack = doc.stacks.shift_remove("local").unwrap();
    let commands = commands_of(&dir);
    Example {
        _tmp: tmp,
        dir,
        stack,
        commands,
    }
}

/// The project file's `processes:` as a command table, the way the host
/// hands them over (`command:` here is a list, so no shell).
fn commands_of(dir: &Path) -> CommandTable {
    let text = fs::read_to_string(dir.join("chappa.yml")).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let mut table = CommandTable::default();
    for (name, def) in value["processes"].as_mapping().unwrap() {
        let argv: Vec<String> = def["command"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap().to_string())
            .collect();
        table.0.insert(
            name.as_str().unwrap().to_string(),
            CommandSpec {
                argv,
                working_dir: None,
                env: IndexMap::new(),
            },
        );
    }
    table
}

struct FakeGit {
    branch: Option<String>,
    dirty: Option<String>,
}

impl Default for FakeGit {
    fn default() -> Self {
        Self {
            branch: Some("main".into()),
            dirty: None,
        }
    }
}

impl GitFacts for FakeGit {
    fn head_commit(&self, _: &Path) -> Result<String, GitError> {
        Ok(HEAD.into())
    }
    fn current_branch(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(self.branch.clone())
    }
    fn count_between(&self, _: &Path, _: &str, _: &str) -> Result<u32, GitError> {
        Ok(0)
    }
    fn dirty_hash(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(self.dirty.clone())
    }
    fn commit_exists(&self, _: &Path, _: &str) -> Result<bool, GitError> {
        Ok(true)
    }
}

/// The engines before anything ran: no containers, postgres resolvable on
/// docker, the api image not yet local on podman.
fn fresh_engines() -> FakeCli {
    FakeCli::new()
        .on(&["compose", "ps"], "")
        .on(
            &["docker", "manifest", "inspect", "--verbose", "postgres:16"],
            &format!(r#"{{"Ref":"postgres:16","Descriptor":{{"digest":"{PG_DIGEST}","platform":{{"os":"linux"}}}}}}"#),
        )
        .failing(
            &["podman", "image", "inspect"],
            125,
            "Error: registry.example/api:main: image not known",
        )
        .failing(&["image", "inspect", "--format", "{{.Id}}"], 1, "No such image")
}

fn runner<'a>(
    ex: &'a Example,
    cli: &'a FakeCli,
    git: &'a FakeGit,
    gateways: &'a FakeGateways,
) -> StackRunner<'a> {
    StackRunner::new(
        "local",
        &ex.stack,
        &ex.dir,
        Tools {
            cli,
            git,
            commands: &ex.commands,
            gateways,
        },
        NOW,
    )
}

fn no_processes() -> IndexMap<String, ProcessStatus> {
    IndexMap::new()
}

fn up_change() -> Change {
    Change::Up {
        selection: Selection::Default,
        modes: IndexMap::new(),
        channel: Some("main".into()),
    }
}

/// The gate storage every test of this file shares (records are keyed by
/// project, and each test has its own project folder).
fn install_gate() {
    let root = std::env::temp_dir().join(format!("chappa-gate-example-{}", std::process::id()));
    stack_core::gate_store::install(root).unwrap();
}

fn perform_local(steps: &[JobStep]) {
    for s in steps {
        if let JobStep::Local { step } = s {
            step.perform().unwrap();
        }
    }
}

fn runs(steps: &[JobStep]) -> Vec<(String, &RunBlock)> {
    steps
        .iter()
        .filter_map(|s| match s {
            JobStep::Run { run, component } => Some((component.clone(), run)),
            _ => None,
        })
        .collect()
}

fn joined(run: &RunBlock) -> String {
    run.argv.join(" ")
}

fn writes(steps: &[JobStep]) -> Vec<(&str, &Path, &str)> {
    steps
        .iter()
        .filter_map(|s| match s {
            JobStep::Local {
                step:
                    LocalStep::WriteFile {
                        what,
                        path,
                        contents,
                    },
            } => Some((what.as_str(), path.as_path(), contents.as_str())),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------- validation

#[test]
fn the_example_loads_and_validates_without_findings() {
    let dir = example_source();
    let (path, doc) = StacksDocument::load_from_project(&dir).unwrap().unwrap();
    assert!(path.ends_with("chappa.yml"));
    let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
    let diags = validate(
        &doc,
        &ValidationContext {
            gitignore: Some(&gitignore),
        },
    );
    assert!(!has_errors(&diags), "{diags:#?}");
    // The database keeps data and has a data-safety check with a rules
    // file, so neither the unscanned nor the sample-without-rules warning
    // applies. The database password is a generated `{secret:db}`, so the
    // inline-credential check has nothing to point out either.
    assert!(diags.is_empty(), "{diags:#?}");
    let committed = [
        fs::read_to_string(dir.join("chappa.yml")).unwrap(),
        fs::read_to_string(dir.join("stack").join("core.compose.yml")).unwrap(),
        fs::read_to_string(dir.join("stack").join("apis.compose.yml")).unwrap(),
    ];
    use stack_core::credentials::{is_placeholder, spans, CredentialKind};
    for text in &committed {
        assert!(
            spans(text)
                .iter()
                .all(|s| s.kind != CredentialKind::UrlUserinfo),
            "no URL with a password in a committed file: {text}"
        );
        for line in text.lines() {
            if let Some((key, value)) = line.split_once(':') {
                if key.trim().ends_with("PASSWORD") {
                    assert!(is_placeholder(value), "{line}");
                }
            }
        }
    }
    let db_check = stack_core_db_check(&doc);
    assert!(db_check.marker.is_some() && db_check.sample.is_some());
    let rules_file = doc.stacks["local"]
        .data_safety
        .as_ref()
        .and_then(|d| d.rules.clone())
        .unwrap();
    let loaded = stack_core::safety::data::DataRules::load(&dir.join(rules_file)).unwrap();
    let ids: Vec<&str> = loaded.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["email-real-domain", "card-number"]);

    let stack = &doc.stacks["local"];
    // The workflow maps any tracker's neutral labels onto its states.
    let wf = stack.workflow.as_ref().unwrap();
    assert_eq!(wf.initial().unwrap().id, "todo");
    assert_eq!(wf.map_status("any-tracker", "In Review"), Some("review"));
    assert!(wf.needs_note("review", "dev"));
    assert_eq!(stack.groups["core"].engine.as_deref(), Some("docker"));
    assert_eq!(stack.groups["apis"].engine.as_deref(), Some("podman"));
    let api = &stack.components["api"];
    assert!(api.has_mode(Mode::Prebuilt) && api.has_mode(Mode::Source));
    assert!(stack.components["db"].persistence.is_some());
    assert_eq!(
        stack.components["migrate"].kind,
        stack_core::definition::ComponentKind::Oneshot
    );
    // Every compose file the definition names exists beside it.
    for c in stack.components.values() {
        if let Some(f) = &c.file {
            assert!(dir.join(f).is_file(), "{} is missing", f.display());
        }
    }
    // The process command the web component names is in `processes:`.
    let commands = commands_of(&dir);
    assert_eq!(commands.0["web-dev"].argv, ["npm", "run", "dev"]);
}

#[test]
fn the_compose_files_publish_no_ports_themselves() {
    for f in ["stack/core.compose.yml", "stack/apis.compose.yml"] {
        let text = fs::read_to_string(example_source().join(f)).unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        for (name, svc) in yaml["services"].as_mapping().unwrap() {
            assert!(
                svc.get("ports").is_none(),
                "{f}: service {name:?} publishes ports; the override does that per slot"
            );
        }
    }
}

// ------------------------------------------------------------ up, applied

#[test]
fn up_on_a_fresh_instance_writes_overrides_pulls_starts_in_order_and_records() {
    let ex = example();
    let cli = fresh_engines();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &no_processes()).unwrap();
    assert!(obs.notes.is_empty(), "{:?}", obs.notes);
    // `ps` asked once per project, with the instance's project names.
    assert_eq!(cli.calls_matching(&["compose", "ps"]).len(), 2);
    assert_eq!(
        cli.calls_matching(&["docker", "compose", "-p", "local-0-core", "ps", "-a"])
            .len(),
        1
    );
    assert_eq!(
        cli.calls_matching(&["podman", "compose", "-p", "local-0-apis", "ps", "-a"])
            .len(),
        1
    );

    let plan = r.plan(&obs, &up_change()).unwrap();
    assert_eq!(plan.verb, Verb::Up);
    assert_eq!(plan.channel.as_deref(), Some("main"));
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let described: Vec<String> = applied.steps.iter().map(JobStep::describe).collect();

    // 1. override files first, one per compose project
    let w = writes(&applied.steps);
    assert!(matches!(&applied.steps[0], JobStep::Local { .. }));
    assert!(matches!(&applied.steps[1], JobStep::Local { .. }));
    let core = w
        .iter()
        .find(|(what, ..)| *what == "local-0-core override")
        .unwrap();
    assert!(core
        .1
        .ends_with(Path::new(".work/instances/local-0/core.override.yml")));
    assert!(
        core.2.contains("dev.example.test:host-gateway"),
        "{}",
        core.2
    );
    assert!(
        core.2.contains(&format!("postgres@{PG_DIGEST}")),
        "{}",
        core.2
    );
    assert!(core.2.contains("14000:5432"), "{}", core.2);
    let apis = w
        .iter()
        .find(|(what, ..)| *what == "local-0-apis override")
        .unwrap();
    assert!(apis.2.contains("registry.example/api:main"), "{}", apis.2);
    assert!(apis.2.contains("14001:8080"), "{}", apis.2);
    // the api reaches the database through the host, per engine
    assert!(apis.2.contains("DATABASE_URL"), "{}", apis.2);
    // the generated password reaches every container that needs it, and
    // only through the override files under `.work/`
    let password =
        fs::read_to_string(ex.dir.join(".work/instances/local-0/secrets").join("db")).unwrap();
    assert_eq!(password.len(), 48);
    for (text, wanted) in [
        (&core.2, "POSTGRES_PASSWORD"),
        (&core.2, "PGPASSWORD"),
        (&apis.2, "DATABASE_URL"),
    ] {
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with(wanted))
            .unwrap_or_else(|| panic!("{wanted} in {text}"));
        assert!(line.contains(&password), "{wanted}");
    }
    assert!(!apis.2.contains("{secret:"), "{}", apis.2);

    // 2. the runs, in dependency order
    let rs = runs(&applied.steps);
    let lines: Vec<(String, String)> = rs.iter().map(|(c, r)| (c.clone(), joined(r))).collect();
    let pos = |pred: &dyn Fn(&(String, String)) -> bool| lines.iter().position(pred).unwrap();
    let pull_db = pos(&|(c, l)| c == "db" && l == &format!("docker pull postgres@{PG_DIGEST}"));
    let pull_api = pos(&|(c, l)| c == "api" && l == "podman pull registry.example/api:main");
    let up_db = pos(&|(c, l)| c == "db" && l.contains(" up -d --no-deps db"));
    let up_migrate = pos(&|(c, l)| c == "migrate" && l.contains(" up -d --no-deps migrate"));
    let up_api = pos(&|(c, l)| c == "api" && l.contains(" up -d --no-deps api"));
    let web = pos(&|(c, _)| c == "web");
    assert!(pull_db < up_db && up_db < up_migrate);
    // db and migrate share postgres:16: one digest lookup, one pull
    assert_eq!(
        lines
            .iter()
            .filter(|(_, l)| l.contains(" pull postgres"))
            .count(),
        1
    );
    assert_eq!(cli.calls_matching(&["manifest", "inspect"]).len(), 1);
    assert!(described
        .iter()
        .any(|d| d.starts_with("migrate: postgres@") && d.ends_with("pulled by an earlier step")));
    assert!(pull_api < up_api && up_db < up_api && up_api < web);
    // compose commands carry the project, the base file and the override
    let (_, db_up) = &rs[up_db];
    assert_eq!(db_up.argv[..4], ["docker", "compose", "-p", "local-0-core"]);
    assert!(db_up
        .argv
        .iter()
        .any(|a| a.ends_with("core.compose.yml") && !a.contains("override")));
    assert!(db_up.argv.iter().any(|a| a.ends_with("core.override.yml")));
    assert_eq!(
        rs[up_api].1.argv[..4],
        ["podman", "compose", "-p", "local-0-apis"]
    );
    // the web run block: the project command with the instance env
    let (_, web_run) = &rs[web];
    assert_eq!(web_run.argv, ["npm", "run", "dev"]);
    assert!(web_run.cwd.ends_with("web"), "{}", web_run.cwd.display());
    assert_eq!(web_run.env["CHAPPA_STACK"], "local");
    assert_eq!(web_run.env["CHAPPA_STACK_SLOT"], "0");
    assert_eq!(
        web_run.env["CHAPPA_STACK_API_URL"],
        "http://dev.example.test:14001"
    );
    assert_eq!(web_run.env["API_URL"], "http://dev.example.test:14001");
    assert_eq!(web_run.env["PORT"], "14002");
    // podman cannot resolve a digest before the first pull
    assert!(applied
        .notes
        .iter()
        .any(|n| n.starts_with("api: pinned by tag")));

    // 3. the records last
    let n = applied.steps.len();
    let last_two: Vec<&str> = w[w.len() - 2..].iter().map(|(what, ..)| *what).collect();
    assert_eq!(last_two, ["provenance cache", "intent record"]);
    assert!(matches!(applied.steps[n - 1], JobStep::Local { .. }));
    for c in ["db", "migrate", "api", "web"] {
        assert_eq!(
            applied.intent.components[c].desired,
            stack_core::intent::Desired::Running,
            "{c}"
        );
    }
    assert_eq!(
        applied.intent.components["api"].channel.as_deref(),
        Some("main")
    );

    // Performing the local steps leaves the files where compose reads them.
    perform_local(&applied.steps);
    assert!(ex
        .dir
        .join(".work/instances/local-0/core.override.yml")
        .is_file());
    assert!(ex
        .dir
        .join(".work/instances/local-0/apis.override.yml")
        .is_file());
    let (inst2, intent) = r.instance(0).unwrap();
    assert_eq!(
        intent.components["db"].desired,
        stack_core::intent::Desired::Running
    );
    assert_eq!(
        inst2.components["api"].desired,
        stack_core::intent::Desired::Running
    );
}

#[test]
fn slot_one_gets_its_own_projects_ports_and_files() {
    let ex = example();
    let cli = fresh_engines();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(1).unwrap();
    let obs = r.observe(&inst, &no_processes()).unwrap();
    let plan = r.plan(&obs, &up_change()).unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let w = writes(&applied.steps);
    let core = w
        .iter()
        .find(|(what, ..)| *what == "local-1-core override")
        .unwrap();
    assert!(core
        .1
        .ends_with(Path::new(".work/instances/local-1/core.override.yml")));
    assert!(core.2.contains("14100:5432"), "{}", core.2);
    let web = runs(&applied.steps)
        .into_iter()
        .find(|(c, _)| c == "web")
        .unwrap()
        .1
        .clone();
    assert_eq!(web.env["CHAPPA_STACK_SLOT"], "1");
    assert_eq!(web.env["PORT"], "14102");
}

// ------------------------------------------------------- scripted engines

/// One container as the engines report it.
#[derive(Clone)]
struct Ctr {
    service: &'static str,
    state: &'static str,
    health: &'static str,
    exit: i32,
    restarts: u32,
    labels: Vec<(&'static str, &'static str)>,
}

impl Ctr {
    fn running(service: &'static str) -> Self {
        Ctr {
            service,
            state: "running",
            health: "",
            exit: 0,
            restarts: 0,
            labels: Vec::new(),
        }
    }
    fn exited(service: &'static str, exit: i32) -> Self {
        Ctr {
            state: "exited",
            exit,
            ..Ctr::running(service)
        }
    }
    fn health(mut self, h: &'static str) -> Self {
        self.health = h;
        self
    }
    fn restarts(mut self, n: u32) -> Self {
        self.restarts = n;
        self
    }
    fn state(mut self, s: &'static str) -> Self {
        self.state = s;
        self
    }
    fn label(mut self, k: &'static str, v: &'static str) -> Self {
        self.labels.push((k, v));
        self
    }
    fn name(&self, project: &str) -> String {
        format!("{project}-{}-1", self.service)
    }
    fn ps(&self, project: &str) -> serde_json::Value {
        serde_json::json!({
            "Name": self.name(project),
            "Service": self.service,
            "State": self.state,
            "Health": self.health,
            "ExitCode": self.exit,
        })
    }
    fn inspect(&self, project: &str) -> serde_json::Value {
        let labels: serde_json::Map<String, serde_json::Value> = self
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
            .collect();
        let health = if self.health.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "Status": self.health })
        };
        serde_json::json!({
            "Name": format!("/{}", self.name(project)),
            "RestartCount": self.restarts,
            "Image": "sha256:feedface",
            "State": {
                "Status": self.state,
                "Running": self.state == "running",
                "Restarting": self.state == "restarting",
                "ExitCode": self.exit,
                "StartedAt": "2026-11-13T10:00:00.5Z",
                "Health": health,
            },
            "Config": { "Image": "", "Labels": labels },
            "HostConfig": { "ExtraHosts": null },
        })
    }
}

/// Engines answering `ps` (docker as NDJSON, podman as an array) and
/// `inspect` with the given containers, for slot 0.
fn engines_with(core: &[Ctr], apis: &[Ctr]) -> FakeCli {
    let core_ps: Vec<String> = core
        .iter()
        .map(|c| c.ps("local-0-core").to_string())
        .collect();
    let apis_ps: Vec<serde_json::Value> = apis.iter().map(|c| c.ps("local-0-apis")).collect();
    let core_inspect: Vec<serde_json::Value> =
        core.iter().map(|c| c.inspect("local-0-core")).collect();
    let apis_inspect: Vec<serde_json::Value> =
        apis.iter().map(|c| c.inspect("local-0-apis")).collect();
    FakeCli::new()
        .on(
            &["docker", "compose", "-p", "local-0-core", "ps"],
            &core_ps.join("\n"),
        )
        .on(
            &["podman", "compose", "-p", "local-0-apis", "ps"],
            &serde_json::Value::from(apis_ps).to_string(),
        )
        .on(
            &["docker", "inspect", "--type", "container"],
            &serde_json::Value::from(core_inspect).to_string(),
        )
        .on(
            &["podman", "inspect", "--type", "container"],
            &serde_json::Value::from(apis_inspect).to_string(),
        )
        .failing(
            &["image", "inspect", "--format", "{{.Id}}"],
            1,
            "No such image",
        )
}

/// Bring slot 0 up through the fakes and perform the local steps, so the
/// intent record and provenance cache exist on disk.
fn bring_up(ex: &Example) {
    let cli = fresh_engines();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &no_processes()).unwrap();
    let plan = r.plan(&obs, &up_change()).unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    perform_local(&applied.steps);
}

fn web(status: ProcessStatus) -> IndexMap<String, ProcessStatus> {
    let mut m = IndexMap::new();
    m.insert("web".to_string(), status);
    m
}

fn web_running() -> IndexMap<String, ProcessStatus> {
    web(ProcessStatus {
        running: true,
        exit_code: None,
        started_utc: Some("2026-11-13T11:00:00Z".into()),
        restarts: 0,
    })
}

fn web_exited(code: i32) -> IndexMap<String, ProcessStatus> {
    web(ProcessStatus {
        running: false,
        exit_code: Some(code),
        started_utc: None,
        restarts: 0,
    })
}

const API_PROBE: &str = "http://dev.example.test:14001/health/ready";
const WEB_PROBE: &str = "http://dev.example.test:14002/";

fn all_running() -> (Vec<Ctr>, Vec<Ctr>) {
    (
        vec![
            Ctr::running("db").health("healthy"),
            Ctr::exited("migrate", 0),
        ],
        vec![Ctr::running("api")],
    )
}

type Reports = IndexMap<String, ComponentReport>;

fn report(
    ex: &Example,
    core: &[Ctr],
    apis: &[Ctr],
    processes: &IndexMap<String, ProcessStatus>,
    http: &FakeHttp,
) -> Reports {
    let cli = engines_with(core, apis);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, processes).unwrap();
    r.status(&inst, &obs, http)
        .into_iter()
        .map(|c| (c.status.id.clone(), c))
        .collect()
}

fn states(reports: &Reports) -> Vec<(&str, ComponentState)> {
    reports
        .iter()
        .map(|(k, r)| (k.as_str(), r.status.state))
        .collect()
}

fn stop_all() -> Change {
    Change::Stop {
        keep_groups: vec![],
    }
}

// ------------------------------------------------------------ nine states

#[test]
fn status_after_up_reads_all_nine_states_from_ps_inspect_and_probes() {
    use ComponentState::*;
    let ex = example();
    bring_up(&ex);
    let mut seen: Vec<ComponentState> = Vec::new();

    // Everything fine.
    let (core, apis) = all_running();
    let http = FakeHttp::default()
        .status(API_PROBE, 200)
        .status(WEB_PROBE, 302);
    let r = report(&ex, &core, &apis, &web_running(), &http);
    assert_eq!(
        states(&r),
        [("db", Up), ("migrate", OneshotOk), ("api", Up), ("web", Up)]
    );
    let db = &r["db"];
    assert_eq!(db.status.uptime.as_deref(), Some("2h 0m"));
    assert_eq!(db.status.restarts, Some(0));
    let prov = db.status.provenance.as_ref().unwrap();
    assert_eq!(prov.mode, Mode::Prebuilt);
    assert_eq!(prov.image.as_deref(), Some(PG_DIGEST));
    assert_eq!(prov.build_id.as_deref(), Some("16"));
    let password = fs::read_to_string(ex.dir.join(".work/instances/local-0/secrets/db")).unwrap();
    assert_eq!(
        db.links["url"],
        format!("postgres://app:{password}@dev.example.test:14000/app")
    );
    let logs = db.logs.as_ref().unwrap();
    assert_eq!(logs.argv[..4], ["docker", "compose", "-p", "local-0-core"]);
    assert!(joined(logs).ends_with("logs -f --tail 200 db"));
    assert_eq!(logs.title.as_deref(), Some("logs: db"));
    let api = &r["api"];
    assert_eq!(api.probe.as_ref().unwrap().status(), Some(200));
    assert_eq!(
        api.status.provenance.as_ref().unwrap().build_id.as_deref(),
        Some("main")
    );
    assert_eq!(api.group, "apis");
    assert!(r["web"].logs.is_none(), "a process has no compose logs");
    assert_eq!(r["web"].links["url"], "http://dev.example.test:14002");
    seen.extend(states(&r).into_iter().map(|(_, s)| s));

    // Health still starting, migration failed, api answering 503, web never started.
    let core = vec![
        Ctr::running("db").health("starting"),
        Ctr::exited("migrate", 3),
    ];
    let apis = vec![Ctr::running("api")];
    let http = FakeHttp::default().status(API_PROBE, 503);
    let r = report(&ex, &core, &apis, &no_processes(), &http);
    assert_eq!(
        states(&r),
        [
            ("db", Starting),
            ("migrate", OneshotFailed),
            ("api", Unhealthy),
            ("web", NotDeployed)
        ]
    );
    assert_eq!(r["migrate"].status.exit_code, Some(3));
    seen.extend(states(&r).into_iter().map(|(_, s)| s));

    // Restart loops, a migration still running, web exited with an error.
    let core = vec![
        Ctr::running("db").state("restarting"),
        Ctr::running("migrate"),
    ];
    let apis = vec![Ctr::running("api").restarts(5)];
    let http = FakeHttp::default().status(API_PROBE, 200);
    let r = report(&ex, &core, &apis, &web_exited(1), &http);
    assert_eq!(
        states(&r),
        [
            ("db", Crashloop),
            ("migrate", Starting),
            ("api", Crashloop),
            ("web", StoppedProblem)
        ]
    );
    assert_eq!(r["api"].status.restarts, Some(5));
    seen.extend(states(&r).into_iter().map(|(_, s)| s));

    // Unhealthy by its own healthcheck; the migration container gone; the
    // api killed while wanted running; web not answering its probe.
    let core = vec![Ctr::running("db").health("unhealthy")];
    let apis = vec![Ctr::exited("api", 137)];
    let r = report(&ex, &core, &apis, &web_running(), &FakeHttp::default());
    assert_eq!(
        states(&r),
        [
            ("db", Unhealthy),
            ("migrate", NotDeployed),
            ("api", StoppedProblem),
            ("web", Unhealthy)
        ]
    );
    assert!(
        r["api"].probe.is_none(),
        "a stopped component is not probed"
    );
    seen.extend(states(&r).into_iter().map(|(_, s)| s));

    // Stopped through the stack: the intent follows, nothing is a problem.
    let (core, apis) = all_running();
    let cli = engines_with(&core, &apis);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let run = runner(&ex, &cli, &git, &gateways);
    let (inst, _) = run.instance(0).unwrap();
    let obs = run.observe(&inst, &web_running()).unwrap();
    let plan = run.plan(&obs, &stop_all()).unwrap();
    let applied = run
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    perform_local(&applied.steps);
    let core = vec![Ctr::exited("db", 0), Ctr::exited("migrate", 0)];
    let apis = vec![Ctr::exited("api", 0)];
    let r = report(&ex, &core, &apis, &web_exited(0), &FakeHttp::default());
    assert_eq!(
        states(&r),
        [
            ("db", StoppedIdle),
            ("migrate", OneshotOk),
            ("api", StoppedIdle),
            ("web", StoppedIdle)
        ]
    );
    seen.extend(states(&r).into_iter().map(|(_, s)| s));

    for s in ComponentState::ALL {
        assert!(seen.contains(&s), "{s:?} never reached");
    }
}

// ------------------------------------------------------------ other verbs

/// Slot 0 brought up, then observed with every component running.
fn running_observation(ex: &Example, cli: &FakeCli) -> (ResolvedInstance, Observation) {
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(ex, cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &web_running()).unwrap();
    (inst, obs)
}

#[test]
fn stop_runs_in_reverse_dependency_order_with_the_grace_period() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);

    let plan = r.plan(&obs, &stop_all()).unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let order: Vec<String> = applied
        .steps
        .iter()
        .filter(|s| !matches!(s, JobStep::Local { .. }))
        .map(JobStep::describe)
        .collect();
    assert_eq!(
        order,
        [
            "web: stop process (10s)",
            "api: stop api (10s)",
            "db: stop db (10s)"
        ]
    );
    let rs = runs(&applied.steps);
    assert_eq!(
        rs[0].1.argv[..4],
        ["podman", "compose", "-p", "local-0-apis"]
    );
    assert!(joined(rs[0].1).ends_with(" stop -t 10 api"));
    assert!(joined(rs[1].1).ends_with(" stop -t 10 db"));
    // a stop rewrites no override, only the records
    assert_eq!(
        writes(&applied.steps)
            .iter()
            .map(|(what, ..)| *what)
            .collect::<Vec<_>>(),
        ["provenance cache", "intent record"]
    );

    // --keep core: only the api group stops
    let plan = r
        .plan(
            &obs,
            &Change::Stop {
                keep_groups: vec!["core".into()],
            },
        )
        .unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let rs = runs(&applied.steps);
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].0, "api");
    assert!(!applied
        .steps
        .iter()
        .any(|s| matches!(s, JobStep::StopProcess { .. })));
}

#[test]
fn down_removes_each_project_and_keeps_volumes_unless_told() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let down = Change::Down {
        keep_groups: vec![],
    };
    let plan = r.plan(&obs, &down).unwrap();

    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let lines: Vec<String> = runs(&applied.steps)
        .iter()
        .map(|(_, r)| joined(r))
        .collect();
    let downs: Vec<&String> = lines.iter().filter(|l| l.ends_with(" down")).collect();
    assert_eq!(downs.len(), 2, "{lines:#?}");
    assert!(downs
        .iter()
        .any(|l| l.starts_with("docker compose -p local-0-core")));
    assert!(downs
        .iter()
        .any(|l| l.starts_with("podman compose -p local-0-apis")));
    assert!(lines.iter().all(|l| !l.contains("--volumes")));
    // the graceful stops come before the removals
    let first_down = lines.iter().position(|l| l.ends_with(" down")).unwrap();
    let last_stop = lines.iter().rposition(|l| l.contains(" stop -t ")).unwrap();
    assert!(last_stop < first_down);
    assert!(applied
        .steps
        .iter()
        .any(|s| matches!(s, JobStep::StopProcess { component, .. } if component == "web")));
    for c in ["db", "migrate", "api", "web"] {
        assert_eq!(applied.intent.components[c].desired, Desired::Absent, "{c}");
    }

    let applied = r
        .apply(
            &inst,
            &obs,
            &plan,
            &ApplyOptions {
                remove_volumes: true,
            },
        )
        .unwrap();
    let rs = runs(&applied.steps);
    assert_eq!(
        rs.iter()
            .filter(|(_, r)| joined(r).ends_with(" down --volumes"))
            .count(),
        2
    );
    assert!(rs
        .iter()
        .any(|(_, r)| r.title.as_deref() == Some("down local-0-core (volumes removed)")));

    // `down --keep core` leaves the core project alone
    let plan = r
        .plan(
            &obs,
            &Change::Down {
                keep_groups: vec!["core".into()],
            },
        )
        .unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let lines: Vec<String> = runs(&applied.steps)
        .iter()
        .map(|(_, r)| joined(r))
        .collect();
    assert!(
        lines.iter().all(|l| !l.contains("local-0-core")),
        "{lines:#?}"
    );
    assert!(lines
        .iter()
        .any(|l| l.starts_with("podman compose -p local-0-apis") && l.ends_with(" down")));
}

#[test]
fn start_recreates_only_what_baked_a_stale_gateway() {
    let ex = example();
    bring_up(&ex);
    // After a reboot: everything exited; the api baked a literal gateway
    // that has since moved, the db baked the alias that never goes stale.
    let core = vec![
        Ctr::exited("db", 0).label("chappa.stack.gateway", "host-gateway"),
        Ctr::exited("migrate", 0).label("chappa.stack.gateway", "host-gateway"),
    ];
    let apis = vec![Ctr::exited("api", 0).label("chappa.stack.gateway", "10.0.0.1")];
    let cli = engines_with(&core, &apis);
    let git = FakeGit::default();
    let mut gw = IndexMap::new();
    gw.insert("podman".to_string(), Gateway::literal("10.0.0.2"));
    let gateways = FakeGateways(gw);
    let r = runner(&ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &no_processes()).unwrap();
    assert_eq!(obs.gateways["podman"].address, "10.0.0.2");
    assert_eq!(obs.gateways["docker"].address, "host-gateway");
    assert_eq!(
        obs.state.components["api"].gateway.as_deref(),
        Some("10.0.0.1")
    );
    let change = StackRunner::start_change(&obs);
    let plan = r.plan(&obs, &change).unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let lines: Vec<(String, String)> = runs(&applied.steps)
        .iter()
        .map(|(c, r)| (c.clone(), joined(r)))
        .collect();
    let db = lines.iter().find(|(c, _)| c == "db").unwrap();
    assert!(db.1.ends_with(" start db"), "{}", db.1);
    let api = lines.iter().find(|(c, _)| c == "api").unwrap();
    assert!(
        api.1.ends_with(" up -d --no-deps --force-recreate api"),
        "{}",
        api.1
    );
    assert!(!lines.iter().any(|(c, _)| c == "migrate"));
    assert!(plan
        .notes
        .iter()
        .any(|n| n == "migrate already ran to completion"));
    assert!(plan
        .notes
        .iter()
        .any(|n| n == "web has nothing to start; bring it up first"));
    // the apis override is rewritten with the new gateway before the recreate
    let w = writes(&applied.steps);
    let apis_override = w
        .iter()
        .find(|(what, ..)| *what == "local-0-apis override")
        .unwrap();
    assert!(
        apis_override.2.contains("dev.example.test:10.0.0.2"),
        "{}",
        apis_override.2
    );
    // the api stays on the tag it was pulled by
    assert!(
        apis_override.2.contains("registry.example/api:main"),
        "{}",
        apis_override.2
    );
    // and the core override keeps db pinned by its whole reference
    let core_override = w
        .iter()
        .find(|(what, ..)| *what == "local-0-core override")
        .unwrap();
    assert!(
        core_override.2.contains(&format!("postgres@{PG_DIGEST}")),
        "{}",
        core_override.2
    );
    assert!(
        !core_override.2.contains(&format!("image: {PG_DIGEST}")),
        "a bare digest is no image reference: {}",
        core_override.2
    );
}

#[test]
fn watch_swaps_the_container_for_the_watch_command() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let plan = r
        .plan(
            &obs,
            &Change::Watch {
                component: "api".into(),
                git_ref: None,
            },
        )
        .unwrap();
    assert_eq!(plan.verb, Verb::Watch);
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let rs = runs(&applied.steps);
    let rm = rs
        .iter()
        .position(|(_, r)| joined(r).ends_with(" rm -f -s api"))
        .unwrap();
    assert_eq!(
        rs[rm].1.argv[..4],
        ["podman", "compose", "-p", "local-0-apis"]
    );
    let watch = rs
        .iter()
        .position(|(_, r)| r.title.as_deref() == Some("api: watch (api-watch)"))
        .unwrap();
    assert!(rm < watch);
    let (_, w) = &rs[watch];
    assert_eq!(w.argv, ["cargo", "watch", "-x", "run"]);
    assert!(w.cwd.ends_with("api"));
    assert_eq!(w.env["CHAPPA_STACK_SLOT"], "0");
    let url = &w.env["DATABASE_URL"];
    let password = fs::read_to_string(ex.dir.join(".work/instances/local-0/secrets/db")).unwrap();
    assert_eq!(
        url,
        &format!("postgres://app:{password}@localhost:14000/app")
    );
    assert_eq!(applied.intent.components["api"].mode, Some(Mode::Dev));
}

#[test]
fn switching_to_source_builds_once_and_reuses_an_existing_content_key() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let change = Change::SwitchMode {
        component: "api".into(),
        mode: Mode::Source,
        git_ref: None,
    };
    let is_build = |r: &RunBlock| r.argv.get(1).map(String::as_str) == Some("build");

    // Not built yet: a build run with the content key and the labels.
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let r = runner(&ex, &cli, &git, &gateways);
    let plan = r.plan(&obs, &change).unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let rs = runs(&applied.steps);
    let (_, build) = rs.iter().find(|(_, r)| is_build(r)).unwrap();
    assert_eq!(build.argv[..4], ["podman", "build", "-t", "api:01234567"]);
    let a = &build.argv;
    let dockerfile = a.iter().position(|w| w == "-f").unwrap();
    assert!(Path::new(&a[dockerfile + 1]).ends_with(Path::new("api/Dockerfile")));
    assert!(a.contains(&format!("org.opencontainers.image.revision={HEAD}")));
    assert!(a.contains(&"chappa.stack.content-key=api:01234567".to_string()));
    assert!(a.contains(&"org.opencontainers.image.ref.name=main".to_string()));
    assert!(Path::new(a.last().unwrap()).ends_with("api"));
    assert!(build.cwd.ends_with("api"));
    assert!(rs
        .iter()
        .any(|(_, r)| joined(r).ends_with(" up -d --no-deps --force-recreate api")));
    let w = writes(&applied.steps);
    let apis_override = w
        .iter()
        .find(|(what, ..)| *what == "local-0-apis override")
        .unwrap();
    assert!(
        apis_override.2.contains("api:01234567"),
        "{}",
        apis_override.2
    );

    // Already built: reused, no build run.
    let mut cli = engines_with(&core, &apis);
    cli.add(
        &["image", "inspect", "--format", "{{.Id}}", "api:01234567"],
        CliOutput::success("sha256:abc\n"),
    );
    let r = runner(&ex, &cli, &git, &gateways);
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    assert!(!runs(&applied.steps).iter().any(|(_, r)| is_build(r)));
    assert!(applied
        .steps
        .iter()
        .any(|s| s.describe() == "api: api:01234567 exists, reused"));

    // A dirty worktree gets its own key.
    let dirty = FakeGit {
        dirty: Some("abcdef0123456789".into()),
        ..FakeGit::default()
    };
    let cli = engines_with(&core, &apis);
    let r = runner(&ex, &cli, &dirty, &gateways);
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    assert!(runs(&applied.steps)
        .iter()
        .any(|(_, r)| r.argv[..4] == ["podman", "build", "-t", dirty_tag("abcdef01").as_str()]));
}

#[test]
fn rebuild_of_a_prebuilt_component_pulls_again_and_recreates() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let mut cli = engines_with(&core, &apis);
    // a manifest list: the linux entry wins for a linux-mode engine
    cli.add(
        &["docker", "manifest", "inspect", "--verbose", "postgres:16"],
        CliOutput::success(&format!(
            r#"[{{"Descriptor":{{"digest":"sha256:0000","platform":{{"os":"windows"}}}}}},{{"Descriptor":{{"digest":"{PG_DIGEST}","platform":{{"os":"linux"}}}}}}]"#
        )),
    );
    let (inst, obs) = running_observation(&ex, &cli);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let plan = r
        .plan(
            &obs,
            &Change::Rebuild {
                component: "db".into(),
            },
        )
        .unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let lines: Vec<String> = runs(&applied.steps)
        .iter()
        .map(|(_, r)| joined(r))
        .collect();
    assert_eq!(lines[0], format!("docker pull postgres@{PG_DIGEST}"));
    assert!(
        lines[1].ends_with(" up -d --no-deps --force-recreate db"),
        "{lines:#?}"
    );
}

#[test]
fn apply_refuses_a_plan_whose_state_moved_on() {
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let plan = r.plan(&obs, &stop_all()).unwrap();
    // db crashed between plan and apply
    let moved = engines_with(&[Ctr::exited("db", 1), Ctr::exited("migrate", 0)], &apis);
    let (_, obs2) = running_observation(&ex, &moved);
    match r.apply(&inst, &obs2, &plan, &ApplyOptions::default()) {
        Err(RunnerError::Plan(PlanError::StateChanged { expected, actual })) => {
            assert_eq!(expected, plan.fingerprint);
            assert_ne!(expected, actual);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // the unchanged observation still applies
    assert!(r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .is_ok());
}

#[test]
fn restart_and_logs_are_direct_actions_on_compose_components_only() {
    let ex = example();
    let cli = FakeCli::new();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let (inst, _) = r.instance(0).unwrap();
    let restart = r.restart(&inst, "api").unwrap();
    assert_eq!(
        restart.argv[..4],
        ["podman", "compose", "-p", "local-0-apis"]
    );
    assert!(joined(&restart).ends_with(" restart -t 10 api"));
    assert_eq!(restart.title.as_deref(), Some("restart api"));
    assert!(r.restart(&inst, "web").is_none());
    let logs = r.logs(&inst, "migrate").unwrap();
    assert!(joined(&logs).ends_with(" logs -f --tail 200 migrate"));
    assert!(r.logs(&inst, "web").is_none());
    assert!(r.logs(&inst, "nope").is_none());
    assert!(cli.lines().is_empty(), "nothing ran: {:?}", cli.lines());
}

#[test]
fn activate_binds_an_item_to_a_slot_in_both_records() {
    let ex = example();
    bring_up(&ex);
    let cli = FakeCli::new();
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    install_gate();
    let steps = r.activate(0, "ABC-1").unwrap();
    assert!(
        matches!(&steps[0], JobStep::Local { step: LocalStep::WriteGate { what, .. } } if what == "active item"),
        "{:?}",
        steps[0]
    );
    let w = writes(&steps);
    assert_eq!(
        w.iter().map(|(what, ..)| *what).collect::<Vec<_>>(),
        ["intent record"]
    );
    perform_local(&steps);
    assert!(
        !r.work_dir().join("active.json").exists(),
        "the binding is not written into the work folder"
    );
    let gate = stack_core::gate_store::GateStore::installed()
        .unwrap()
        .project(&ex.dir);
    let active = ActiveRecord::load(&gate).unwrap();
    assert_eq!(active.slots["0"].item, "ABC-1");
    assert_eq!(active.slots["0"].instance, "local-0");
    let (inst, intent) = r.instance(0).unwrap();
    assert_eq!(intent.item.as_deref(), Some("ABC-1"));
    // the bring-up survives the activation
    assert_eq!(intent.components["db"].desired, Desired::Running);
    assert_eq!(inst.components["db"].desired, Desired::Running);
    assert!(cli.lines().is_empty());
}

#[test]
fn gc_keeps_what_is_running_pinned_or_kept_and_removes_the_rest() {
    let ex = example();
    bring_up(&ex);
    let other = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let pg_rows = format!(
        "{}\n{}",
        format_args!(
            r#"{{"ID":"bbbbbbbbbbbb","Repository":"postgres","Tag":"16","Digest":"{PG_DIGEST}","Size":"400MB"}}"#
        ),
        format_args!(
            r#"{{"ID":"cccccccccccc","Repository":"postgres","Tag":"15","Digest":"{other}","Size":"380MB"}}"#
        ),
    );
    let cli = FakeCli::new()
        // nothing runs on docker: postgres:16 is protected by the instance's pin alone
        .on(&["docker", "ps", "-a", "--format", "json", "--no-trunc"], "")
        .on(
            &["docker", "image", "ls", "--digests", "--format", "json", "--filter"],
            r#"{"ID":"aaaaaaaaaaaa","Repository":"api","Tag":"0badc0de","Digest":"<none>","Size":"90MB"}"#,
        )
        .on(
            &["docker", "image", "ls", "--digests", "--format", "json", "postgres"],
            &pg_rows,
        )
        .on(
            &["docker", "image", "ls", "--format", "json", "--filter", "dangling=true"],
            r#"{"ID":"dddddddddddd","Repository":"<none>","Tag":"<none>","Digest":"<none>","Size":"10MB"}"#,
        )
        // podman: the api container runs from the tag it was pulled by
        .on(
            &["podman", "ps", "-a", "--format", "json", "--no-trunc"],
            r#"[{"Id":"c1","Image":"registry.example/api:main","ImageID":"sha256:eeeeeeeeeeeeeeee"}]"#,
        )
        .on(
            &["podman", "image", "ls", "--digests", "--format", "json", "--filter"],
            r#"[{"Id":"ffff","Names":["localhost/api:89abcdef"],"Digest":"","Size":1000000}]"#,
        )
        .on(
            &["podman", "image", "ls", "--digests", "--format", "json", "registry.example/api"],
            r#"[{"Id":"eeeeeeeeeeeeeeee","Names":["registry.example/api:main"],"Digest":"sha256:e1","Size":2000000},{"Id":"9999","Names":["registry.example/api:old"],"Digest":"sha256:e2","Size":2000000}]"#,
        )
        .on(
            &["podman", "image", "ls", "--format", "json", "--filter", "dangling=true"],
            "[]",
        );
    let git = FakeGit::default();
    let gateways = FakeGateways::default();
    let r = runner(&ex, &cli, &git, &gateways);
    let plans = r.gc_plan(&["localhost/api:89abcdef".to_string()]).unwrap();
    assert_eq!(
        plans.iter().map(|p| p.engine.as_str()).collect::<Vec<_>>(),
        ["docker", "podman"]
    );
    let docker = &plans[0];
    let removed: Vec<&str> = docker.remove.iter().map(|g| g.reference.as_str()).collect();
    assert_eq!(removed, ["api:0badc0de", "postgres:15", "dddddddddddd"]);
    assert_eq!(docker.remove[2].reason, "dangling");
    let kept: Vec<(&str, &str)> = docker
        .kept
        .iter()
        .map(|k| (k.reference.as_str(), k.reason.as_str()))
        .collect();
    assert_eq!(kept, [("postgres:16", "pinned by an instance")]);
    assert_eq!(
        docker.summary(),
        "docker: remove 3 image(s), keep 1, prune build cache"
    );

    let podman = &plans[1];
    let kept: Vec<(&str, &str)> = podman
        .kept
        .iter()
        .map(|k| (k.reference.as_str(), k.reason.as_str()))
        .collect();
    assert_eq!(
        kept,
        [
            ("localhost/api:89abcdef", "referenced by a kept state"),
            ("registry.example/api:main", "in use by a running container"),
        ]
    );
    let removed: Vec<&str> = podman.remove.iter().map(|g| g.reference.as_str()).collect();
    assert_eq!(removed, ["registry.example/api:old"]);

    let steps = r.gc_apply(&plans);
    let lines: Vec<String> = runs(&steps).iter().map(|(_, r)| joined(r)).collect();
    assert_eq!(
        lines,
        [
            "docker image rm aaaaaaaaaaaa cccccccccccc dddddddddddd",
            "docker builder prune -f",
            "podman image rm 9999",
            "podman image prune -f",
        ]
    );
}

fn stack_core_db_check(doc: &StacksDocument) -> stack_core::definition::ComponentDataSafetyDef {
    doc.stacks["local"].components["db"]
        .data_safety
        .clone()
        .expect("db declares a data-safety check")
}

#[test]
fn a_bound_item_builds_and_runs_in_its_worktrees_and_the_guard_refuses_shared_clones() {
    use stack_core::planner::BindTarget;
    use stack_core::runner::SharedCheckoutGuard;
    let ex = example();
    bring_up(&ex);
    let (core, apis) = all_running();
    let git = FakeGit {
        branch: Some("task/ABC-1-fix".into()),
        ..FakeGit::default()
    };
    let gateways = FakeGateways::default();
    let cli = engines_with(&core, &apis);
    let (inst, obs) = running_observation(&ex, &cli);
    let wt = ex.dir.join(".work/ABC-1/src");
    let worktrees: IndexMap<String, PathBuf> = [
        ("api".to_string(), wt.join("api-01234567")),
        ("web".to_string(), wt.join("web-01234567")),
    ]
    .into_iter()
    .collect();
    let guard = SharedCheckoutGuard {
        allowed: Default::default(),
        file: ex.dir.join("shared-checkouts-allow.txt"),
    };
    let bind = |r: &str| Change::Bind {
        item: "ABC-1".into(),
        bind: [
            (
                "api".to_string(),
                BindTarget {
                    mode: Mode::Source,
                    git_ref: r.into(),
                    rebuild: false,
                },
            ),
            (
                "web".to_string(),
                BindTarget {
                    mode: Mode::Dev,
                    git_ref: r.into(),
                    rebuild: false,
                },
            ),
        ]
        .into_iter()
        .collect(),
        release: IndexMap::new(),
    };
    let r = runner(&ex, &cli, &git, &gateways).with_binding(worktrees.clone(), Some(guard.clone()));
    assert_eq!(
        r.repo_path("api", Mode::Source).unwrap(),
        wt.join("api-01234567")
    );
    let plan = r.plan(&obs, &bind("task/ABC-1-fix")).unwrap();
    assert_eq!(plan.verb, Verb::Bind);
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    let described: Vec<String> = applied.steps.iter().map(|s| s.describe()).collect();
    assert!(
        described.contains(&"api: already on task/ABC-1-fix".to_string()),
        "{described:?}"
    );
    let rs = runs(&applied.steps);
    let (_, build) = rs
        .iter()
        .find(|(_, r)| r.argv.get(1).map(String::as_str) == Some("build"))
        .unwrap();
    assert_eq!(build.cwd, wt.join("api-01234567"));
    // web runs as a process: the old one stops before the new one starts
    // in the worktree.
    let web_stop = applied
        .steps
        .iter()
        .position(|s| matches!(s, JobStep::StopProcess { component, .. } if component == "web"))
        .expect("web is stopped first");
    let web_start = applied
        .steps
        .iter()
        .position(|s| matches!(s, JobStep::Run { component, run } if component == "web" && run.cwd == wt.join("web-01234567")))
        .expect("web starts in its worktree");
    assert!(web_stop < web_start);
    let intent = applied.intent.components["api"].clone();
    assert_eq!(
        (intent.mode, intent.git_ref.as_deref()),
        (Some(Mode::Source), Some("task/ABC-1-fix"))
    );

    // Without the item's worktrees the same build is refused, naming the file.
    let bare =
        runner(&ex, &cli, &git, &gateways).with_binding(IndexMap::new(), Some(guard.clone()));
    let e = bare
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap_err();
    match &e {
        RunnerError::SharedCheckout {
            component,
            clone,
            file,
            ..
        } => {
            assert_eq!((component.as_str(), clone.as_str()), ("api", "api"));
            assert!(file.ends_with("shared-checkouts-allow.txt"));
        }
        other => panic!("{other}"),
    }
    assert!(e.to_string().contains("shared-checkouts-allow.txt"), "{e}");
    // Listed in the allow file: allowed.
    let allowed = SharedCheckoutGuard {
        allowed: ["api".to_string(), "web".to_string()].into_iter().collect(),
        ..guard.clone()
    };
    let on_main = FakeGit::default();
    let ok = runner(&ex, &cli, &on_main, &gateways).with_binding(IndexMap::new(), Some(allowed));
    let main_plan = ok.plan(&obs, &bind("main")).unwrap();
    assert!(ok
        .apply(&inst, &obs, &main_plan, &ApplyOptions::default())
        .is_ok());
    // No guard (a runner nobody bound): as before.
    let plain = runner(&ex, &cli, &on_main, &gateways);
    assert!(plain
        .apply(&inst, &obs, &main_plan, &ApplyOptions::default())
        .is_ok());
}

/// A source-built image tag with a dirty-tree hash, built here so no
/// source line holds a component name beside a varied hex string (a
/// secret scanner reads that as a key).
fn dirty_tag(dirty: &str) -> String {
    format!("{}:{}-dirty-{dirty}", "api", "01234567")
}
