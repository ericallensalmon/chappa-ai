use super::*;
use crate::cli::{CliOutput, FakeCli};
use crate::compose::FakeGateways;
use crate::definition::{validate, Mode, StacksDocument, ValidationContext};
use crate::driver::resolve;
use crate::intent::IntentRecord;
use crate::jobs::JobStep;
use crate::planner::{plan, Change, InstanceState, PlanError, Selection, Step, Verb};
use crate::process::CommandTable;
use crate::provenance::{GitError, GitFacts};
use crate::runner::{ApplyOptions, StackRunner, Tools};

const NOW: &str = "2026-11-15T12:00:00Z";

/// A mixed stack: a compose database, and a script component wrapping the
/// rest, with every verb but `rebuild`.
const MIXED: &str = r#"
stacks:
  app:
    slots: { count: 2, portBase: 14000, portStride: 100 }
    components:
      db:
        driver: compose
        file: stack/db.compose.yml
        modes: { prebuilt: { image: postgres, tag: "16" } }
      stack:
        driver: script
        depends_on: [db]
        up:      [node, scripts/up.js, "{slot}"]
        start:   [node, scripts/start.js]
        stop:    [node, scripts/stop.js, "{instance}"]
        restart: [node, scripts/restart.js]
        down:    [node, scripts/down.js]
        status:  [node, scripts/status.js]
        plan:    [node, scripts/plan.js]
        cwd: tools
        ports: 2
        repos: [../api]
        env:
          API_URL: "http://localhost:{port:stack}"
          DB_ADDR: "{host}:{port:db}"
        timeouts: { up: 30m, status: 5s }
"#;

fn stack_of(text: &str) -> StackDef {
    let mut doc = StacksDocument::parse(text).unwrap();
    doc.stacks.shift_remove("app").unwrap()
}

fn diags(text: &str) -> Vec<Diagnostic> {
    validate(
        &StacksDocument::parse(text).unwrap(),
        &ValidationContext::default(),
    )
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

// ------------------------------------------------------------- definition

#[test]
fn a_script_component_parses_every_verb_and_its_settings() {
    let stack = stack_of(MIXED);
    let c = &stack.components["stack"];
    assert_eq!(c.driver, DriverKind::Script);
    assert_eq!(
        argv(c, ScriptVerb::Up).unwrap(),
        words(&["node", "scripts/up.js", "{slot}"])
    );
    assert_eq!(argv(c, ScriptVerb::Stop).unwrap()[1], "scripts/stop.js");
    assert_eq!(argv(c, ScriptVerb::Plan).unwrap()[1], "scripts/plan.js");
    assert!(argv(c, ScriptVerb::Rebuild).is_none());
    assert!(has(c, ScriptVerb::Restart) && !has(c, ScriptVerb::Rebuild));
    assert!(!has(&stack.components["db"], ScriptVerb::Up));
    assert_eq!(c.cwd.as_deref(), Some(Path::new("tools")));
    assert_eq!(c.env["DB_ADDR"], "{host}:{port:db}");
    assert_eq!(timeout(c, ScriptVerb::Up), Duration::from_secs(30 * 60));
    assert_eq!(timeout(c, ScriptVerb::Status), Duration::from_secs(5));
    // The defaults where the definition says nothing.
    assert_eq!(timeout(c, ScriptVerb::Stop), Duration::from_secs(5 * 60));
    assert_eq!(timeout(c, ScriptVerb::Start), Duration::from_secs(15 * 60));
    assert_eq!(
        timeout(c, ScriptVerb::Rebuild),
        Duration::from_secs(60 * 60)
    );
    assert_eq!(timeout(c, ScriptVerb::Plan), Duration::from_secs(60));
    assert_eq!(
        ScriptVerb::Status.default_timeout(),
        Duration::from_secs(20)
    );
    // `repos:` names a clone a work item can have a worktree of.
    let repos = stack.repos();
    assert_eq!(repos["api"].uses[0].component, "stack");
    assert_eq!(repos["api"].uses[0].mode, None);
    // `stop:` is still the grace setting on other drivers.
    let compose = stack_of(
        "stacks:\n  app:\n    components:\n      db: { driver: compose, file: a.yml, stop: { graceSeconds: 40 } }\n",
    );
    assert_eq!(compose.grace_seconds("db"), 40);
    assert!(argv(&compose.components["db"], ScriptVerb::Stop).is_none());
    let errors: Vec<Diagnostic> = diags(MIXED)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn validation_covers_verbs_cwd_timeouts_and_the_powershell_rule() {
    let base = "stacks:\n  app:\n    components:\n      s:\n        driver: script\n";
    let find = |text: &str, path: &str| -> Option<Diagnostic> {
        diags(text).into_iter().find(|d| d.path == path)
    };
    // `up` is required.
    let d = find(
        &format!("{base}        down: [x]\n"),
        "stacks.app.components.s.up",
    )
    .unwrap();
    assert_eq!((d.severity, d.rule), (Severity::Error, Rule::MissingField));
    // An empty argv.
    let d = find(
        &format!("{base}        up: []\n"),
        "stacks.app.components.s.up",
    )
    .unwrap();
    assert_eq!((d.severity, d.rule), (Severity::Error, Rule::ScriptArgv));
    // A PowerShell file named directly: warned, with the fix.
    let d = find(
        &format!("{base}        up: [scripts/up.ps1, -Slot, \"{{slot}}\"]\n"),
        "stacks.app.components.s.up",
    )
    .unwrap();
    assert_eq!((d.severity, d.rule), (Severity::Warning, Rule::ScriptArgv));
    assert!(
        d.message
            .contains("[powershell, -NoProfile, -File, scripts/up.ps1]"),
        "{}",
        d.message
    );
    // ... and fine with the interpreter named.
    assert!(find(
        &format!("{base}        up: [powershell, -NoProfile, -File, scripts/UP.PS1]\n"),
        "stacks.app.components.s.up"
    )
    .is_none());
    // cwd must stay inside the project.
    for bad in ["../elsewhere", "/srv/example", "a/../../b"] {
        let d = find(
            &format!("{base}        up: [x]\n        cwd: \"{bad}\"\n"),
            "stacks.app.components.s.cwd",
        )
        .unwrap_or_else(|| panic!("{bad}"));
        assert_eq!(d.rule, Rule::PathOutsideProject, "{bad}");
    }
    assert!(find(
        &format!("{base}        up: [x]\n        cwd: tools/stack\n"),
        "stacks.app.components.s.cwd"
    )
    .is_none());
    // timeouts: a verb, a duration, not zero, for a script that exists.
    let t = |entry: &str| {
        find(
            &format!("{base}        up: [x]\n        timeouts: {{ {entry} }}\n"),
            &format!(
                "stacks.app.components.s.timeouts.{}",
                entry.split(':').next().unwrap()
            ),
        )
    };
    assert_eq!(t("deploy: 5m").unwrap().rule, Rule::UnknownKey);
    assert_eq!(t("up: 5 minutes").unwrap().rule, Rule::InvalidDuration);
    assert_eq!(t("up: 0s").unwrap().rule, Rule::OutOfRange);
    let unused = t("stop: 1m").unwrap();
    assert_eq!(
        (unused.severity, unused.rule),
        (Severity::Warning, Rule::UnknownKey)
    );
    assert!(t("up: 90s").is_none());
    // env values are templates like any other: an unknown placeholder fails.
    let d = find(
        &format!("{base}        up: [x]\n        env: {{ A: \"{{prot:s}}\" }}\n"),
        "stacks.app.components.s.env.A",
    )
    .unwrap();
    assert_eq!(d.rule, Rule::Placeholder);
    // Placeholders in a verb's argv are checked, the item ones included.
    assert!(find(
        &format!("{base}        up: [x, \"{{worktree:api}}\", \"{{branch:api}}\", \"{{item}}\", \"{{instance}}\"]\n"),
        "stacks.app.components.s.up"
    )
    .is_none());
    // Script settings on another driver are reported as unused.
    let d = find(
        "stacks:\n  app:\n    components:\n      db: { driver: compose, file: a.yml, up: [x], cwd: tools }\n",
        "stacks.app.components.db.up",
    )
    .unwrap();
    assert_eq!(
        (d.severity, d.rule),
        (Severity::Warning, Rule::NotForDriver)
    );
}

// --------------------------------------------------- placeholders and env

fn instance(stack: &StackDef, slot: u32, item: Option<&str>) -> ResolvedInstance {
    let mut intent = IntentRecord::new("app", slot, NOW);
    intent.item = item.map(str::to_string);
    resolve("app", stack, slot, Some(&intent)).unwrap()
}

fn bound() -> ScriptBinding {
    let mut b = ScriptBinding {
        item: Some("ABC-1".into()),
        ..ScriptBinding::default()
    };
    b.branches
        .insert("api".into(), "task/ABC-1-fix-login".into());
    b.worktrees.insert(
        "api".into(),
        PathBuf::from("/srv/example/.work/ABC-1/src/api-1a2b3c4d"),
    );
    b.worktrees.insert(
        "web-ui".into(),
        PathBuf::from("/srv/example/.work/ABC-1/src/web-ui-5e6f7a8b"),
    );
    b
}

#[test]
fn a_run_resolves_argv_cwd_and_the_item_environment() {
    let mut stack = stack_of(MIXED);
    stack.components.get_mut("stack").unwrap().up = Some(words(&[
        "node",
        "scripts/up.js",
        "{slot}",
        "{instance}",
        "{port:stack:1}",
        "{worktree:api}",
        "{branch:api}",
        "{item}",
    ]));
    let inst = instance(&stack, 1, None);
    let project = Path::new("/srv/example");
    let run = prepare(
        &inst,
        &stack,
        project,
        "stack",
        ScriptVerb::Up,
        &bound(),
        &IndexMap::new(),
        &[],
    )
    .unwrap();
    assert_eq!(
        run.argv,
        words(&[
            "node",
            "scripts/up.js",
            "1",
            "app-1",
            "14102",
            "/srv/example/.work/ABC-1/src/api-1a2b3c4d",
            "task/ABC-1-fix-login",
            "ABC-1",
        ])
    );
    assert_eq!(run.cwd, project.join("tools"));
    assert_eq!(run.timeout_secs, 30 * 60);
    assert_eq!(run.env["CHAPPA_STACK_INSTANCE"], "app-1");
    assert_eq!(run.env["CHAPPA_STACK_SLOT"], "1");
    assert_eq!(run.env["CHAPPA_STACK_ITEM"], "ABC-1");
    assert_eq!(
        run.env["CHAPPA_STACK_WORKTREE_API"],
        "/srv/example/.work/ABC-1/src/api-1a2b3c4d"
    );
    // Repo names become `[A-Z0-9_]`.
    assert_eq!(
        run.env["CHAPPA_STACK_WORKTREE_WEB_UI"],
        "/srv/example/.work/ABC-1/src/web-ui-5e6f7a8b"
    );
    assert_eq!(run.env["API_URL"], "http://localhost:14101");
    assert_eq!(run.env["DB_ADDR"], "localhost:14100");
    // The run block takes a relative program path from the cwd.
    let block = run.run_block();
    assert_eq!(block.argv[0], "node");
    assert_eq!(block.title.as_deref(), Some("stack: up script"));
    // `plan` gets the verb appended.
    let p = prepare(
        &inst,
        &stack,
        project,
        "stack",
        ScriptVerb::Plan,
        &bound(),
        &IndexMap::new(),
        &["stop".into()],
    )
    .unwrap();
    assert_eq!(p.argv, words(&["node", "scripts/plan.js", "stop"]));
    assert_eq!(p.timeout_secs, 60);
    assert_ne!(p.key(), run.key());
}

#[test]
fn without_a_bound_item_the_item_placeholders_fail_and_the_env_is_empty() {
    let mut stack = stack_of(MIXED);
    let inst = instance(&stack, 0, None);
    let none = ScriptBinding::default();
    let run = prepare(
        &inst,
        &stack,
        Path::new("/srv/example"),
        "stack",
        ScriptVerb::Stop,
        &none,
        &IndexMap::new(),
        &[],
    )
    .unwrap();
    assert_eq!(run.env["CHAPPA_STACK_ITEM"], "");
    assert!(!run.env.keys().any(|k| k.starts_with(WORKTREE_ENV_PREFIX)));
    stack.components.get_mut("stack").unwrap().up =
        Some(words(&["node", "up.js", "{worktree:api}"]));
    let e = prepare(
        &inst,
        &stack,
        Path::new("/srv/example"),
        "stack",
        ScriptVerb::Up,
        &none,
        &IndexMap::new(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(
        &e,
        ScriptError::Placeholder {
            source: PlaceholderError::NoItem { .. },
            ..
        }
    ));
    assert_eq!(
        e.to_string(),
        "the `up` script of `stack` cannot run: `{worktree:api}` needs a bound work item; activate one in this slot first"
    );
    // The intent's item counts as bound when the runner names none.
    let with_item = instance(&stack, 0, Some("ABC-1"));
    let mut b = ScriptBinding::default();
    b.worktrees.insert("api".into(), PathBuf::from("/w/api"));
    let ok = prepare(
        &with_item,
        &stack,
        Path::new("/srv/example"),
        "stack",
        ScriptVerb::Up,
        &b,
        &IndexMap::new(),
        &[],
    )
    .unwrap();
    assert_eq!(ok.env["CHAPPA_STACK_ITEM"], "ABC-1");
    // A PowerShell file named directly is refused before it would run.
    stack.components.get_mut("stack").unwrap().stop =
        Some(ComponentStop::Script(words(&["scripts\\stop.ps1"])));
    let e = prepare(
        &inst,
        &stack,
        Path::new("/srv/example"),
        "stack",
        ScriptVerb::Stop,
        &none,
        &IndexMap::new(),
        &[],
    )
    .unwrap_err();
    assert!(e.to_string().contains("name the interpreter first"), "{e}");
    // No such verb, no such component.
    assert!(matches!(
        prepare(
            &inst,
            &stack,
            Path::new("/p"),
            "stack",
            ScriptVerb::Rebuild,
            &none,
            &IndexMap::new(),
            &[]
        ),
        Err(ScriptError::NoScript { .. })
    ));
    assert!(matches!(
        prepare(
            &inst,
            &stack,
            Path::new("/p"),
            "db",
            ScriptVerb::Up,
            &none,
            &IndexMap::new(),
            &[]
        ),
        Err(ScriptError::NotScript(_))
    ));
}

#[test]
fn a_relative_program_path_with_a_folder_runs_from_the_cwd() {
    let run = ScriptRun {
        component: "stack".into(),
        verb: ScriptVerb::Up,
        argv: words(&["scripts/up.cmd", "two words"]),
        cwd: PathBuf::from("/srv/example"),
        env: IndexMap::new(),
        timeout_secs: 5,
    };
    assert_eq!(
        run.spawn_argv()[0],
        Path::new("/srv/example")
            .join("scripts/up.cmd")
            .display()
            .to_string()
    );
    assert_eq!(run.command_line(), "scripts/up.cmd \"two words\"");
    let bare = ScriptRun {
        argv: words(&["node", "up.js"]),
        ..run.clone()
    };
    assert_eq!(bare.spawn_argv(), words(&["node", "up.js"]));
    let inv = run.invocation();
    assert_eq!(inv.timeout_secs, 5);
    assert_eq!(inv.cwd.as_deref(), Some(Path::new("/srv/example")));
}

// -------------------------------------------------------------- status JSON

#[test]
fn status_json_table() {
    let ok = |text: &str| parse_status(text, "stack").unwrap();
    let bad = |text: &str| parse_status(text, "stack").unwrap_err();

    // single, named after itself
    let s = ok(
        r#"{"components":[{"name":"stack","state":"up","since":"2026-11-15T11:00:00Z","exitCode":0,"detail":"fine","links":[{"label":"API","url":"http://localhost:14000"}]}]}"#,
    );
    assert_eq!(s.components.len(), 1);
    assert_eq!(s.components[0].state, ComponentState::Up);
    assert_eq!(s.components[0].links[0].label, "API");
    // single without a name
    let s = ok(r#"{"components":[{"state":"stopped-idle"}]}"#);
    assert_eq!(s.components[0].name, "stack");
    // multi, with facts and alerts of every tone
    let s = ok(
        r#"{"components":[{"name":"api","state":"crashloop","exitCode":137},{"name":"web","state":"oneshot-ok"}],
        "facts":[{"label":"Built from","value":"main"},{"label":"Count","value":3}],
        "alerts":[{"tone":"warn","text":"slow"},{"tone":"bad","text":"broken"},{"tone":"info","text":"fyi"}]}"#,
    );
    assert_eq!(s.components[1].name, "web");
    assert_eq!(s.components[0].exit_code, Some(137));
    assert_eq!(s.facts[1].value, "3");
    assert_eq!(
        s.alerts.iter().map(|a| a.tone).collect::<Vec<_>>(),
        [AlertTone::Warn, AlertTone::Bad, AlertTone::Info]
    );
    // every one of the nine states is accepted by its name
    for st in ComponentState::ALL {
        let s = ok(&format!(
            r#"{{"components":[{{"state":"{}"}}]}}"#,
            st.as_str()
        ));
        assert_eq!(s.components[0].state, st);
    }
    // nothing reported at all is a valid (empty) report
    assert!(ok("{}").components.is_empty());

    // missing name among several
    assert_eq!(
        bad(r#"{"components":[{"name":"api","state":"up"},{"state":"up"}]}"#),
        "components[1] has no `name`, and there is more than one entry"
    );
    // a state outside the nine, said without quoting it
    let e = bad(r#"{"components":[{"name":"api","state":"running-ish"}]}"#);
    assert!(
        e.starts_with("components[0] (`api`): `state` is not one of up, starting"),
        "{e}"
    );
    assert!(!e.contains("running-ish"), "{e}");
    // a name that is not a key
    assert!(bad(r#"{"components":[{"name":"a b","state":"up"}]}"#).contains("`name` must match"));
    // twice the same name
    assert!(
        bad(r#"{"components":[{"name":"a","state":"up"},{"name":"a","state":"up"}]}"#)
            .contains("reported twice")
    );
    // malformed: not JSON, the wrong shape, cut short, a bad tone
    let e = bad("Starting api... done\n{\"components\": []}");
    assert!(e.starts_with("not valid JSON (line 1, column"), "{e}");
    assert!(!e.contains("Starting"), "{e}");
    assert_eq!(bad("[1, 2]"), "the output is not a JSON object");
    assert!(bad(r#"{"components": [{"state": "up"}"#).starts_with("JSON that ends too early"));
    assert_eq!(
        bad(r#"{"components": "api"}"#),
        "`components` is not a list"
    );
    assert_eq!(
        bad(r#"{"components":[],"alerts":[{"tone":"loud","text":"x"}]}"#),
        "alerts[0]: `tone` is not warn, bad or info"
    );
    assert!(
        bad(r#"{"components":[{"name":"a","state":"up","exitCode":"zero"}]}"#)
            .contains("`exitCode` is not a whole number")
    );
    // oversized: refused before parsing
    let big = format!(
        r#"{{"components":[{{"state":"up","detail":"{}"}}]}}"#,
        "x".repeat(STATUS_CAP)
    );
    assert_eq!(bad(&big), "the output is over 1024 KiB");
    // control characters never reach the screen
    let s = ok("{\"components\":[{\"state\":\"up\",\"detail\":\"a\\u001b[31mb\"}]}");
    assert_eq!(s.components[0].detail.as_deref(), Some("a [31mb"));
}

fn run_of(component: &str) -> ScriptRun {
    ScriptRun {
        component: component.into(),
        verb: ScriptVerb::Status,
        argv: words(&["node", "scripts/status.js"]),
        cwd: PathBuf::from("/srv/example"),
        env: IndexMap::new(),
        timeout_secs: 20,
    }
}

#[test]
fn status_runs_through_the_cli_and_every_failure_has_a_reason() {
    let run = run_of("stack");
    let good = FakeCli::new().on(
        &["node", "scripts/status.js"],
        r#"{"components":[{"name":"api","state":"up"},{"name":"web","state":"starting"}]}"#,
    );
    match run_status(&good, &run) {
        ScriptReport::Status { status } => assert_eq!(status.components.len(), 2),
        other => panic!("{other:?}"),
    }
    let calls = good.calls.borrow();
    assert_eq!(calls[0].timeout_secs, 20);
    assert_eq!(calls[0].cwd.as_deref(), Some(Path::new("/srv/example")));
    drop(calls);
    let reason = |cli: FakeCli| match run_status(&cli, &run) {
        ScriptReport::Failed { reason } => reason,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        reason(FakeCli::new().timing_out(&["node"])),
        "the status script did not finish within 20 s"
    );
    assert_eq!(
        reason(FakeCli::new().failing(&["node"], 3, "secret token xyz")),
        "the status script exited with code 3"
    );
    let r = reason(FakeCli::new().without("node"));
    assert!(r.starts_with("the status script could not run"), "{r}");
    let r = reason(FakeCli::new().on(&["node"], "password=hunter2"));
    assert!(
        r.starts_with("its status output cannot be read: not valid JSON"),
        "{r}"
    );
    assert!(!r.contains("hunter2"));
    let mut big = FakeCli::new();
    big.add(&["node"], CliOutput::success(&" ".repeat(STATUS_CAP + 1)));
    assert_eq!(
        reason(big),
        "its status output cannot be read: the output is over 1024 KiB"
    );
}

// ------------------------------------------------------------------ views

fn status_of(json: &str) -> ScriptReport {
    ScriptReport::Status {
        status: parse_status(json, "stack").unwrap(),
    }
}

fn last(verb: ScriptVerb, ok: bool, code: Option<i32>) -> LastRun {
    LastRun {
        verb,
        ok,
        exit_code: code,
        ended_utc: NOW.into(),
        error: None,
    }
}

#[test]
fn reported_states_follow_the_desired_state_rule() {
    let report = status_of(
        r#"{"components":[{"name":"api","state":"stopped-idle"},{"name":"web","state":"stopped-problem"},{"name":"db","state":"up"}]}"#,
    );
    let wanted = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        Some(&report),
        None,
    );
    assert!(wanted.multi);
    assert_eq!(wanted.from, ViewFrom::Status);
    assert_eq!(
        wanted
            .rows
            .iter()
            .map(|r| (r.id.as_str(), r.state))
            .collect::<Vec<_>>(),
        [
            ("stack/api", ComponentState::StoppedProblem),
            ("stack/web", ComponentState::StoppedProblem),
            ("stack/db", ComponentState::Up),
        ]
    );
    assert_eq!(wanted.state(), ComponentState::StoppedProblem);
    assert!(wanted.running() && wanted.exists());
    let stopped = view(
        "stack",
        ComponentKind::Service,
        Desired::Stopped,
        true,
        Some(&report),
        None,
    );
    assert_eq!(stopped.rows[0].state, ComponentState::StoppedIdle);
    assert_eq!(stopped.rows[1].state, ComponentState::StoppedIdle);
    assert_eq!(stopped.state(), ComponentState::Up);
    // A single report named after the component is the component's row.
    let single = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        Some(&status_of(r#"{"components":[{"state":"unhealthy"}]}"#)),
        None,
    );
    assert!(!single.multi);
    assert_eq!(single.rows[0].id, "stack");
    assert_eq!(component_of_row("stack/api"), "stack");
    assert_eq!(component_of_row("stack"), "stack");
}

#[test]
fn an_unreadable_report_is_unhealthy_when_wanted_with_a_bad_alert() {
    let failed = ScriptReport::Failed {
        reason: "the status script did not finish within 20 s".into(),
    };
    let v = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        Some(&failed),
        None,
    );
    assert_eq!(v.from, ViewFrom::Unreadable);
    assert_eq!(v.rows[0].state, ComponentState::Unhealthy);
    assert_eq!(v.alerts[0].tone, AlertTone::Bad);
    assert_eq!(
        v.alerts[0].text,
        "stack: the status script did not finish within 20 s; shown as unhealthy until its status script answers"
    );
    let idle = view(
        "stack",
        ComponentKind::Service,
        Desired::Stopped,
        true,
        Some(&failed),
        None,
    );
    assert_eq!(idle.rows[0].state, ComponentState::StoppedIdle);
    let gone = view(
        "stack",
        ComponentKind::Service,
        Desired::Absent,
        true,
        Some(&failed),
        None,
    );
    assert_eq!(gone.rows[0].state, ComponentState::NotDeployed);
    // Not answered yet: the intent's state, said so.
    let pending = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        None,
        None,
    );
    assert_eq!(pending.from, ViewFrom::Pending);
    assert_eq!(pending.rows[0].state, ComponentState::Up);
    assert!(pending.alerts.is_empty());
}

#[test]
fn without_a_status_script_the_intent_and_last_run_decide() {
    let table = [
        (Desired::Absent, None, ComponentState::NotDeployed),
        (
            Desired::Running,
            Some(last(ScriptVerb::Up, true, Some(0))),
            ComponentState::Up,
        ),
        (
            Desired::Stopped,
            Some(last(ScriptVerb::Stop, true, Some(0))),
            ComponentState::StoppedIdle,
        ),
        (
            Desired::Absent,
            Some(last(ScriptVerb::Down, true, Some(0))),
            ComponentState::NotDeployed,
        ),
        (
            Desired::Running,
            Some(last(ScriptVerb::Up, false, Some(2))),
            ComponentState::StoppedProblem,
        ),
        (
            Desired::Running,
            Some(last(ScriptVerb::Restart, false, None)),
            ComponentState::StoppedProblem,
        ),
        (
            Desired::Stopped,
            Some(last(ScriptVerb::Stop, false, Some(1))),
            ComponentState::Unhealthy,
        ),
        (
            Desired::Absent,
            Some(last(ScriptVerb::Down, false, Some(1))),
            ComponentState::Unhealthy,
        ),
    ];
    for (desired, run, want) in table {
        let v = view(
            "stack",
            ComponentKind::Service,
            desired,
            false,
            None,
            run.as_ref(),
        );
        assert_eq!(v.from, ViewFrom::Assumed);
        assert_eq!(v.rows[0].state, want, "{desired:?} {run:?}");
        assert!(v.rows[0]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("no status script; "));
    }
    let failed = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        false,
        None,
        Some(&last(ScriptVerb::Up, false, Some(2))),
    );
    assert_eq!(failed.rows[0].exit_code, Some(2));
    assert_eq!(
        failed.rows[0].detail.as_deref(),
        Some("no status script; its `up` script failed (exit 2)")
    );
    let oneshot = view(
        "seed",
        ComponentKind::Oneshot,
        Desired::Running,
        false,
        None,
        Some(&last(ScriptVerb::Up, false, Some(1))),
    );
    assert_eq!(oneshot.rows[0].state, ComponentState::OneshotFailed);
}

#[test]
fn last_runs_are_recorded_per_component() {
    let dir = tempfile::tempdir().unwrap();
    assert!(ScriptRuns::load(dir.path(), "app-0").components.is_empty());
    ScriptRuns::record(
        dir.path(),
        "app-0",
        "stack",
        last(ScriptVerb::Up, true, Some(0)),
    )
    .unwrap();
    ScriptRuns::record(
        dir.path(),
        "app-0",
        "seed",
        last(ScriptVerb::Up, false, Some(1)),
    )
    .unwrap();
    ScriptRuns::record(
        dir.path(),
        "app-0",
        "stack",
        last(ScriptVerb::Stop, true, Some(0)),
    )
    .unwrap();
    let runs = ScriptRuns::load(dir.path(), "app-0");
    assert_eq!(runs.components["stack"].verb, ScriptVerb::Stop);
    assert!(!runs.components["seed"].ok);
    assert!(dir
        .path()
        .join("instances")
        .join("app-0")
        .join("scripts.json")
        .is_file());
}

// --------------------------------------------------------- plan + confirm

#[test]
fn plan_output_is_cut_to_twenty_lines_and_two_kilobytes_without_control_characters() {
    let many: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    let body = plan_body(&many);
    assert_eq!(body.lines().count(), PLAN_LINES);
    assert!(body.ends_with("line 20"));
    let wide = format!("{}\n{}\n", "é".repeat(1500), "b".repeat(10));
    let body = plan_body(&wide);
    assert!(body.len() <= PLAN_BYTES, "{}", body.len());
    assert!(body.chars().all(|c| c == 'é'));
    assert_eq!(plan_body("a\u{1b}[1mb\tc\r\n\n\n"), "a[1mb c");
}

#[test]
fn confirms_with_a_plan_without_one_and_when_the_plan_fails() {
    let run = ScriptRun {
        component: "stack".into(),
        verb: ScriptVerb::Up,
        argv: words(&["node", "scripts/up.js", "a b"]),
        cwd: PathBuf::from("/srv/example"),
        env: IndexMap::new(),
        timeout_secs: 900,
    };
    assert_eq!(
        confirm("stack", ScriptVerb::Up, Ok(&run), false, None),
        "Run the `up` script for stack: node scripts/up.js \"a b\"."
    );
    let ready = PlanPreview::Ready {
        text: "api: start\nweb: start".into(),
    };
    assert_eq!(
        confirm("stack", ScriptVerb::Up, Ok(&run), true, Some(&ready)),
        "Run the `up` script for stack.\napi: start\nweb: start"
    );
    let failed = PlanPreview::Failed {
        reason: "it exited with code 2".into(),
    };
    assert_eq!(
        confirm("stack", ScriptVerb::Up, Ok(&run), true, Some(&failed)),
        "Run the `up` script for stack. Its plan script failed (it exited with code 2), so what it will do is not shown."
    );
    assert!(
        confirm("stack", ScriptVerb::Up, Ok(&run), true, None).contains("still being worked out")
    );
    let e = ScriptError::NoScript {
        component: "stack".into(),
        verb: ScriptVerb::Up,
    };
    assert_eq!(
        confirm("stack", ScriptVerb::Up, Err(&e), true, None),
        "Run the `up` script for stack. It cannot run now: `stack` has no `up` script."
    );
    // The plan runs through the CLI with its own limit.
    let mut p = run.clone();
    p.verb = ScriptVerb::Plan;
    p.timeout_secs = 60;
    assert_eq!(
        run_plan(&FakeCli::new().on(&["node"], "x\ny\n"), &p),
        PlanPreview::Ready {
            text: "x\ny".into()
        }
    );
    assert_eq!(
        run_plan(&FakeCli::new().timing_out(&["node"]), &p),
        PlanPreview::Failed {
            reason: "it did not finish within 1 min".into()
        }
    );
}

// ---------------------------------------------------------------- planner

fn state_with(desired: &[(&str, Desired)]) -> InstanceState {
    let mut st = InstanceState::new("app", 0);
    for (name, d) in desired {
        st.components.insert(
            name.to_string(),
            crate::planner::ComponentCurrent {
                desired: *d,
                exists: *d != Desired::Absent,
                running: *d == Desired::Running,
                ..Default::default()
            },
        );
    }
    st
}

fn steps(p: &crate::planner::Plan) -> Vec<String> {
    p.steps
        .iter()
        .map(|s| match s {
            Step::Script {
                component,
                verb,
                row: None,
                ..
            } => format!("{component}:{verb}"),
            Step::Script {
                component,
                verb,
                row: Some(r),
                ..
            } => format!("{component}:{verb}:{r}"),
            other => format!(
                "{}:{}",
                other.component(),
                other.verb_label().to_lowercase()
            ),
        })
        .collect()
}

#[test]
fn the_planner_runs_scripts_in_dependency_order_and_skips_missing_verbs() {
    let mut stack = stack_of(MIXED);
    let fresh = InstanceState::new("app", 0);
    let up = plan(
        &stack,
        &fresh,
        &Change::Up {
            selection: Selection::Default,
            modes: IndexMap::new(),
            channel: Some("main".into()),
        },
    )
    .unwrap();
    assert_eq!(steps(&up), ["db:pull", "db:start", "stack:up"]);
    assert_eq!(up.desired["stack"], Desired::Running);
    // `up` runs the script again even when it is wanted running already.
    let running = state_with(&[("db", Desired::Running), ("stack", Desired::Running)]);
    let again = plan(
        &stack,
        &running,
        &Change::Up {
            selection: Selection::Components(vec!["stack".into()]),
            modes: IndexMap::new(),
            channel: None,
        },
    )
    .unwrap();
    assert_eq!(steps(&again).last().map(String::as_str), Some("stack:up"));
    assert_eq!(
        steps(&again)
            .iter()
            .filter(|s| s.starts_with("stack"))
            .count(),
        1
    );

    let stop = plan(
        &stack,
        &running,
        &Change::Stop {
            keep_groups: vec![],
        },
    )
    .unwrap();
    assert_eq!(steps(&stop), ["stack:stop", "db:stop"]);
    assert_eq!(stop.desired["stack"], Desired::Stopped);
    let down = plan(
        &stack,
        &running,
        &Change::Down {
            keep_groups: vec![],
        },
    )
    .unwrap();
    assert_eq!(steps(&down), ["db:stop", "stack:down", "db:remove"]);
    assert_eq!(down.desired["stack"], Desired::Absent);
    let start = plan(
        &stack,
        &running,
        &Change::Start {
            gateways: IndexMap::new(),
        },
    )
    .unwrap();
    assert_eq!(steps(&start), ["stack:start"]);

    // A stack-wide verb with no script for a component skips it and says so.
    let rebuild = plan(&stack, &running, &Change::RebuildAll).unwrap();
    assert!(!steps(&rebuild).iter().any(|s| s.starts_with("stack")));
    assert!(rebuild
        .notes
        .contains(&"stack has no rebuild script, so rebuild leaves it as it is".to_string()));
    let c = stack.components.get_mut("stack").unwrap();
    c.stop = None;
    c.start = None;
    let stop = plan(
        &stack,
        &running,
        &Change::Stop {
            keep_groups: vec![],
        },
    )
    .unwrap();
    assert_eq!(steps(&stop), ["db:stop"]);
    assert_eq!(
        stop.notes,
        ["stack has no stop script, so stop leaves it as it is"]
    );
    assert!(!stop.desired.contains_key("stack"));
    let start = plan(
        &stack,
        &running,
        &Change::Start {
            gateways: IndexMap::new(),
        },
    )
    .unwrap();
    assert!(start.steps.is_empty());
    assert_eq!(
        start.notes,
        ["stack has no start script, so start leaves it as it is"]
    );
    // A stopped script component is not stopped again.
    let stopped = state_with(&[("db", Desired::Running), ("stack", Desired::Stopped)]);
    let stop = plan(
        &stack_of(MIXED),
        &stopped,
        &Change::Stop {
            keep_groups: vec![],
        },
    )
    .unwrap();
    assert_eq!(steps(&stop), ["db:stop"]);
}

#[test]
fn one_script_verb_is_its_own_change_and_a_missing_one_is_refused() {
    let stack = stack_of(MIXED);
    let st = state_with(&[("stack", Desired::Stopped)]);
    let restart = plan(
        &stack,
        &st,
        &Change::Script {
            component: "stack".into(),
            verb: ScriptVerb::Restart,
            row: None,
            rows: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(restart.verb, Verb::Restart);
    assert_eq!(steps(&restart), ["stack:restart"]);
    assert_eq!(restart.desired["stack"], Desired::Running);
    for verb in [ScriptVerb::Rebuild, ScriptVerb::Status, ScriptVerb::Plan] {
        assert_eq!(
            plan(
                &stack,
                &st,
                &Change::Script {
                    component: "stack".into(),
                    verb,
                    row: None,
                    rows: Vec::new(),
                }
            )
            .unwrap_err(),
            PlanError::NoScript {
                component: "stack".into(),
                verb
            }
        );
    }
    assert!(matches!(
        plan(
            &stack,
            &st,
            &Change::Rebuild {
                component: "stack".into()
            }
        ),
        Err(PlanError::NoScript { .. })
    ));
    assert!(plan(
        &stack,
        &st,
        &Change::SwitchMode {
            component: "stack".into(),
            mode: Mode::Source,
            git_ref: None
        }
    )
    .is_err());
    // A resume runs `up` for the script components it names.
    let resume = plan(
        &stack,
        &InstanceState::new("app", 0),
        &Change::Resume {
            item: Some("ABC-1".into()),
            targets: IndexMap::new(),
            scripts: vec!["stack".into()],
        },
    )
    .unwrap();
    assert_eq!(steps(&resume), ["stack:up"]);
}

// ----------------------------------------------------------------- runner

#[derive(Default)]
struct NoGit;

impl GitFacts for NoGit {
    fn head_commit(&self, _: &Path) -> Result<String, GitError> {
        Err(GitError::Failed {
            args: vec!["rev-parse".into()],
            status: Some(128),
            stderr: "not a repository".into(),
        })
    }
    fn current_branch(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(None)
    }
    fn count_between(&self, _: &Path, _: &str, _: &str) -> Result<u32, GitError> {
        Ok(0)
    }
    fn dirty_hash(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(None)
    }
    fn commit_exists(&self, _: &Path, _: &str) -> Result<bool, GitError> {
        Ok(false)
    }
}

const ONLY_SCRIPT: &str = r#"
stacks:
  app:
    components:
      stack:
        driver: script
        up:     [node, up.js]
        stop:   [node, stop.js]
        status: [node, status.js]
"#;

#[test]
fn the_runner_observes_through_script_outputs_and_applies_script_steps() {
    let dir = tempfile::tempdir().unwrap();
    let stack = stack_of(ONLY_SCRIPT);
    let cli = FakeCli::new().on(
        &["node", "status.js"],
        r#"{"components":[{"name":"api","state":"stopped-idle"},{"name":"web","state":"up"}]}"#,
    );
    let git = NoGit;
    let commands = CommandTable::default();
    let gateways = FakeGateways::default();
    let tools = Tools {
        cli: &cli,
        git: &git,
        commands: &commands,
        gateways: &gateways,
    };
    // Nothing known yet: the job's view, which never runs a status script.
    let quiet = StackRunner::new("app", &stack, dir.path(), tools, NOW);
    let (inst, _) = quiet.instance(0).unwrap();
    let obs = quiet.observe(&inst, &IndexMap::new()).unwrap();
    assert_eq!(obs.scripts["stack"].from, ViewFrom::Pending);
    assert!(cli.calls.borrow().is_empty());
    // Through live outputs the status script runs.
    let live = LiveScripts(&cli);
    let runner = StackRunner::new("app", &stack, dir.path(), tools, NOW).with_scripts(&live);
    let plan_up = runner
        .plan(
            &obs,
            &Change::Up {
                selection: Selection::Default,
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
    let applied = runner
        .apply(&inst, &obs, &plan_up, &ApplyOptions::default())
        .unwrap();
    let script_steps: Vec<&JobStep> = applied
        .steps
        .iter()
        .filter(|s| matches!(s, JobStep::Script { .. }))
        .collect();
    assert_eq!(script_steps.len(), 1);
    match script_steps[0] {
        JobStep::Script {
            run,
            component,
            verb,
            timeout_seconds,
        } => {
            assert_eq!((component.as_str(), *verb), ("stack", ScriptVerb::Up));
            assert_eq!(run.argv, words(&["node", "up.js"]));
            assert_eq!(*timeout_seconds, 15 * 60);
            assert_eq!(run.env["CHAPPA_STACK_ITEM"], "");
        }
        _ => unreachable!(),
    }
    for s in &applied.steps {
        if let JobStep::Local { step } = s {
            step.perform().unwrap();
        }
    }
    let (inst, _) = runner.instance(0).unwrap();
    let obs2 = runner.observe(&inst, &IndexMap::new()).unwrap();
    let v = &obs2.scripts["stack"];
    assert!(v.multi);
    // Wanted running now, so the reported idle api is a problem.
    assert_eq!(v.rows[0].state, ComponentState::StoppedProblem);
    assert_eq!(v.rows[1].state, ComponentState::Up);
    // The fingerprint follows the intent only: another report, same plan.
    let other = FakeCli::new().on(&["node", "status.js"], r#"{"components":[]}"#);
    let other_live = LiveScripts(&other);
    let other_tools = Tools {
        cli: &other,
        ..tools
    };
    let r2 =
        StackRunner::new("app", &stack, dir.path(), other_tools, NOW).with_scripts(&other_live);
    let obs3 = r2.observe(&inst, &IndexMap::new()).unwrap();
    assert_eq!(obs3.state.fingerprint(), obs2.state.fingerprint());
    assert_ne!(obs3.scripts, obs2.scripts);
    // Status reports the worst row for the component, with the view.
    let reports = runner.status(&inst, &obs2, &crate::probe::FakeHttp::default());
    assert_eq!(reports[0].status.state, ComponentState::StoppedProblem);
    assert!(reports[0].script.as_ref().unwrap().multi);
    // A confirm without a plan script shows the argv.
    assert_eq!(
        runner.script_confirm(&inst, &obs2, "stack", ScriptVerb::Stop),
        "Run the `stop` script for stack: node stop.js."
    );
}

// ------------------------------------------- groups, rows and picked rows

#[test]
fn status_groups_pass_through_in_order_with_each_components_group() {
    let s = parse_status(
        r#"{"groups": [{"id": "apis", "label": "APIs"}, {"id": "web"}],
            "components": [{"name": "api", "group": "apis", "state": "up"},
                           {"name": "portal", "group": "web", "state": "up"},
                           {"name": "tools", "state": "up"}]}"#,
        "stack",
    )
    .unwrap();
    assert_eq!(
        s.groups,
        vec![
            ReportedGroup {
                id: "apis".into(),
                label: "APIs".into()
            },
            ReportedGroup {
                id: "web".into(),
                label: "web".into()
            },
        ]
    );
    let groups: Vec<Option<&str>> = s.components.iter().map(|c| c.group.as_deref()).collect();
    assert_eq!(groups, [Some("apis"), Some("web"), None]);
    // The view keeps both.
    let report = ScriptReport::Status { status: s };
    let v = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        Some(&report),
        None,
    );
    assert_eq!(v.groups.len(), 2);
    assert_eq!(v.rows[0].group.as_deref(), Some("apis"));
    assert_eq!(v.rows[2].group, None);
    // Bad shapes are refused, without quoting the output.
    for (text, says) in [
        (
            r#"{"groups": [{"id": "a b"}], "components": []}"#,
            "`id` must match",
        ),
        (
            r#"{"groups": [{"id": "a"}, {"id": "a"}], "components": []}"#,
            "listed twice",
        ),
        (
            r#"{"groups": [{"label": "x"}], "components": []}"#,
            "has no `id`",
        ),
        (
            r#"{"components": [{"name": "a", "group": "x/y", "state": "up"}]}"#,
            "`group` must match",
        ),
    ] {
        let e = parse_status(text, "stack").unwrap_err();
        assert!(e.contains(says), "{text}: {e}");
    }
}

#[test]
fn row_placeholders_are_checked_where_they_are_written() {
    let base = "stacks:\n  app:\n    components:\n      s:\n        driver: script\n        up: [node, up.js, \"{rows}\"]\n";
    let find = |extra: &str, path: &str| -> Option<Diagnostic> {
        diags(&format!("{base}{extra}"))
            .into_iter()
            .find(|d| d.path == path && d.rule == Rule::ScriptArgv)
    };
    assert!(
        find("", "stacks.app.components.s.up").is_none(),
        "{:?}",
        diags(base)
    );
    assert!(find(
        "        restart: [node, r.js, \"{row}\"]\n",
        "stacks.app.components.s.restart"
    )
    .is_none());
    assert!(find(
        "        status: [node, s.js, \"{row}\"]\n",
        "stacks.app.components.s.status"
    )
    .is_some());
    assert!(find(
        "        stop: [node, s.js, \"{rows}\"]\n",
        "stacks.app.components.s.stop"
    )
    .is_some());
    assert!(find(
        "        env: { R: \"{row}\" }\n",
        "stacks.app.components.s.env.R"
    )
    .is_some());
    let inside = "stacks:\n  app:\n    components:\n      s:\n        driver: script\n        up: [node, up.js, \"--only={rows}\"]\n";
    assert!(diags(inside)
        .iter()
        .any(|d| d.path == "stacks.app.components.s.up" && d.message.contains("whole word")));
    let stack = stack_of(&format!(
        "{base}        restart: [node, r.js, \"--name={{row}}\"]\n"
    ));
    let def = &stack.components["s"];
    assert!(takes_row(def, ScriptVerb::Restart));
    assert!(!takes_row(def, ScriptVerb::Up));
    assert!(takes_rows(def, ScriptVerb::Up));
    assert!(!takes_rows(def, ScriptVerb::Restart));
}

#[test]
fn row_and_rows_become_argv_elements_and_unsafe_names_are_refused() {
    let mut stack = stack_of(MIXED);
    let s = stack.components.get_mut("stack").unwrap();
    s.restart = Some(words(&[
        "node",
        "scripts/restart.js",
        "--name={row}",
        "{row}",
    ]));
    s.up = Some(words(&["node", "scripts/up.js", "{rows}", "{slot}"]));
    let inst = instance(&stack, 1, None);
    let project = Path::new("/srv/example");
    let run = |verb: ScriptVerb, row: Option<&str>, rows: &[&str]| {
        let mut b = bound();
        b.row = row.map(str::to_string);
        b.rows = words(rows);
        prepare(
            &inst,
            &stack,
            project,
            "stack",
            verb,
            &b,
            &IndexMap::new(),
            &[],
        )
    };
    assert_eq!(
        run(ScriptVerb::Restart, Some("api"), &[]).unwrap().argv,
        words(&["node", "scripts/restart.js", "--name=api", "api"])
    );
    // Each picked row one word; none picked, no words.
    assert_eq!(
        run(ScriptVerb::Up, None, &["api", "web.ui"]).unwrap().argv,
        words(&["node", "scripts/up.js", "api", "web.ui", "1"])
    );
    assert_eq!(
        run(ScriptVerb::Up, None, &[]).unwrap().argv,
        words(&["node", "scripts/up.js", "1"])
    );
    // A row verb with no row cannot run.
    let e = run(ScriptVerb::Restart, None, &[]).unwrap_err();
    assert!(e.to_string().contains("names one reported row"), "{e}");
    // A name a program would read as an option, or that is no name.
    for bad in ["-rf", "a b", "x;y"] {
        assert!(run(ScriptVerb::Restart, Some(bad), &[]).is_err(), "{bad}");
        assert!(run(ScriptVerb::Up, None, &["api", bad]).is_err(), "{bad}");
    }
}

#[test]
fn a_reported_component_fills_every_column_and_its_probe_is_judged_by_the_rules() {
    let s = parse_status(
        r#"{"components": [
            {"name": "api", "state": "up", "uptime": "2d 7h", "restarts": 3, "exitCode": 0,
             "source": {"mode": "source", "ref": "main", "commit": "9B10002F"},
             "probe": {"url": "https://dev.example.test:14090/health", "rule": "health", "status": 200}},
            {"name": "web", "state": "up", "probe": {"url": "http://localhost:14091/", "rule": "web", "status": 302, "up": false}},
            {"name": "jobs", "state": "up", "probe": {"url": "http://localhost:14092/health", "status": 404, "up": true}},
            {"name": "mail", "state": "up", "probe": {"url": "http://localhost:14093/", "rule": "any"}}]}"#,
        "stack",
    )
    .unwrap();
    let api = &s.components[0];
    assert_eq!(api.uptime.as_deref(), Some("2d 7h"));
    assert_eq!(api.restarts, Some(3));
    assert_eq!(
        api.source,
        Some(ReportedSource {
            mode: Mode::Source,
            git_ref: Some("main".into()),
            commit: Some("9b10002f".into())
        })
    );
    // The rule decides, never the script's own `up`; no status is down.
    let ups: Vec<bool> = s
        .components
        .iter()
        .map(|c| c.probe.as_ref().unwrap().up())
        .collect();
    assert_eq!(ups, [true, true, false, false]);
    assert_eq!(
        s.components[2].probe.as_ref().unwrap().rule,
        ProbeRule::Health
    );
    // The view carries it all.
    let report = ScriptReport::Status { status: s };
    let v = view(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        true,
        Some(&report),
        None,
    );
    assert_eq!(v.rows[0].restarts, Some(3));
    assert_eq!(v.rows[0].source.as_ref().unwrap().mode, Mode::Source);
    assert!(v.rows[0].probe.as_ref().unwrap().up());
    for (text, says) in [
        (
            r#"{"components": [{"name": "a", "state": "up", "restarts": -1}]}"#,
            "`restarts`",
        ),
        (
            r#"{"components": [{"name": "a", "state": "up", "source": {"mode": "built"}}]}"#,
            "`mode`",
        ),
        (
            r#"{"components": [{"name": "a", "state": "up", "source": {"mode": "dev", "commit": "not a sha"}}]}"#,
            "commit hash",
        ),
        (
            r#"{"components": [{"name": "a", "state": "up", "probe": {"url": "file:///etc"}}]}"#,
            "http(s)",
        ),
        (
            r#"{"components": [{"name": "a", "state": "up", "probe": {"url": "http://x.test", "rule": "ok"}}]}"#,
            "`rule`",
        ),
        (
            r#"{"components": [{"name": "a", "state": "up", "probe": {"url": "http://x.test", "status": 99}}]}"#,
            "HTTP status",
        ),
    ] {
        let e = parse_status(text, "stack").unwrap_err();
        assert!(e.contains(says), "{text}: {e}");
    }
}

#[test]
fn a_rows_own_desired_state_judges_it_until_the_whole_component_is_asked_again() {
    let report = ScriptReport::Status {
        status: parse_status(
            r#"{"components": [{"name": "api", "state": "stopped-idle"}, {"name": "web", "state": "stopped-idle"}]}"#,
            "stack",
        )
        .unwrap(),
    };
    let mut rows = IndexMap::new();
    rows.insert("api".to_string(), Desired::Stopped);
    let v = view_with_rows(
        "stack",
        ComponentKind::Service,
        Desired::Running,
        &rows,
        true,
        Some(&report),
        None,
    );
    let states: Vec<ComponentState> = v.rows.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [ComponentState::StoppedIdle, ComponentState::StoppedProblem]
    );
    // The other way: the stack stopped, one row brought up on its own.
    rows.insert("api".to_string(), Desired::Running);
    let v = view_with_rows(
        "stack",
        ComponentKind::Service,
        Desired::Stopped,
        &rows,
        true,
        Some(&report),
        None,
    );
    let states: Vec<ComponentState> = v.rows.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [ComponentState::StoppedProblem, ComponentState::StoppedIdle]
    );

    // The plan records it per row, and a verb on the whole component
    // clears it.
    let mut stack = stack_of(MIXED);
    stack.components.get_mut("stack").unwrap().stop =
        Some(ComponentStop::Script(words(&["node", "stop.js", "{row}"])));
    let st = InstanceState::new("app", 0);
    let one = plan(
        &stack,
        &st,
        &Change::Script {
            component: "stack".into(),
            verb: ScriptVerb::Stop,
            row: Some("api".into()),
            rows: Vec::new(),
        },
    )
    .unwrap();
    assert!(one.desired.is_empty());
    assert_eq!(one.row_desired["stack"]["api"], Desired::Stopped);
    let mut intent = IntentRecord::new("app", 0, NOW);
    intent.components.entry("stack".into()).or_default().desired = Desired::Running;
    intent.record_plan(&one, NOW);
    assert_eq!(intent.components["stack"].desired, Desired::Running);
    assert_eq!(intent.components["stack"].rows["api"], Desired::Stopped);
    let picked = plan(
        &stack,
        &st,
        &Change::Script {
            component: "stack".into(),
            verb: ScriptVerb::Up,
            row: None,
            rows: words(&["web"]),
        },
    )
    .unwrap();
    intent.record_plan(&picked, NOW);
    assert_eq!(
        intent.components["stack"].rows.len(),
        2,
        "picked rows add to what was asked"
    );
    let whole = plan(
        &stack,
        &st,
        &Change::Script {
            component: "stack".into(),
            verb: ScriptVerb::Up,
            row: None,
            rows: Vec::new(),
        },
    )
    .unwrap();
    intent.record_plan(&whole, NOW);
    assert_eq!(intent.components["stack"].desired, Desired::Running);
    assert!(intent.components["stack"].rows.is_empty());
}
