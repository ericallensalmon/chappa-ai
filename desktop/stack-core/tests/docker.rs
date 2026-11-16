//! One real engine round trip: a single `alpine` container that sleeps,
//! brought up and torn down through the runner against the docker CLI.
//!
//! Runs only with `CHAPPA_TEST_DOCKER=1` and a reachable docker daemon;
//! otherwise it prints why it skipped and passes. Everything else in this
//! crate is tested through fakes, so this is the one place that proves the
//! argv the drivers build is argv docker accepts.

use indexmap::IndexMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use stack_core::cli::{CliRunner, EngineCli, Invocation};
use stack_core::compose::CliGatewayResolver;
use stack_core::definition::StacksDocument;
use stack_core::driver::ComponentState;
use stack_core::jobs::JobStep;
use stack_core::planner::{Change, Selection};
use stack_core::probe::StdHttp;
use stack_core::process::CommandTable;
use stack_core::provenance::GitCli;
use stack_core::runner::{ApplyOptions, StackRunner, Tools};

const STACK: &str = r#"
stacks:
  smoke:
    slots: { count: 1, portBase: 14900 }
    components:
      sleeper:
        driver: compose
        file: compose.yml
        service: sleeper
        modes: { prebuilt: { image: alpine, tag: "3.20" } }
        ports: 0
"#;

const COMPOSE: &str = r#"services:
  sleeper:
    image: alpine:3.20
    command: ["sleep", "300"]
"#;

/// Pulls and container starts can be slow on a cold machine.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

fn skip(reason: &str) {
    eprintln!("skipping the real-docker round trip: {reason}");
}

/// Execute an applied plan the way the host would: local steps in
/// process, runs one after another, stopping at the first failure.
fn execute(steps: &[JobStep], cli: &CliRunner) {
    for step in steps {
        match step {
            JobStep::Local { step } => step.perform().unwrap(),
            JobStep::Run { run, component } => {
                let inv = Invocation::from_argv(&run.argv)
                    .cwd(&run.cwd)
                    .timeout(RUN_TIMEOUT);
                let started = std::time::Instant::now();
                let out = cli.run(&inv).unwrap();
                eprintln!(
                    "{component}: {} ({:.1}s)",
                    run.title.as_deref().unwrap_or_default(),
                    started.elapsed().as_secs_f32()
                );
                assert!(
                    out.ok(),
                    "{component}: `{}` failed: {}",
                    run.command_line(),
                    out.summary()
                );
            }
            JobStep::StopProcess { component, .. } => {
                panic!("{component}: no process components in this stack")
            }
            JobStep::Script { component, .. } => {
                panic!("{component}: no script components in this stack")
            }
        }
    }
}

/// Tears the project down even when an assertion fails halfway.
struct Cleanup<'a> {
    project: String,
    dir: &'a Path,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let inv = Invocation::new(
            "docker",
            &["compose", "-p", &self.project, "down", "--timeout", "1"],
        )
        .cwd(self.dir)
        .timeout(Duration::from_secs(60));
        let _ = CliRunner.run(&inv);
    }
}

#[test]
fn an_alpine_container_goes_up_and_down_through_the_docker_cli() {
    if std::env::var("CHAPPA_TEST_DOCKER").as_deref() != Ok("1") {
        return skip("set CHAPPA_TEST_DOCKER=1 to run it");
    }
    let cli = CliRunner;
    match cli.run(&Invocation::new(
        "docker",
        &["info", "--format", "{{.OSType}}"],
    )) {
        Ok(out) if out.ok() && out.stdout.trim() == "linux" => {}
        Ok(out) if out.ok() => {
            return skip(&format!(
                "docker runs {} containers; this test needs linux",
                out.stdout.trim()
            ))
        }
        Ok(out) => return skip(&format!("docker is not reachable: {}", out.summary())),
        Err(e) => return skip(&format!("docker is not installed: {e}")),
    }

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("compose.yml"), COMPOSE).unwrap();
    let stack = StacksDocument::parse(STACK)
        .unwrap()
        .stacks
        .shift_remove("smoke")
        .unwrap();
    let git = GitCli::default();
    let commands = CommandTable::default();
    let gateways = CliGatewayResolver { cli: &cli };
    let r = StackRunner::new(
        "smoke",
        &stack,
        dir,
        Tools {
            cli: &cli,
            git: &git,
            commands: &commands,
            gateways: &gateways,
        },
        "2026-11-13T12:00:00Z",
    );
    let (inst, _) = r.instance(0).unwrap();
    let project = r.projects(&inst).values().next().unwrap().project.clone();
    let _cleanup = Cleanup {
        project: project.clone(),
        dir,
    };
    let processes = IndexMap::new();

    // up
    let obs = r.observe(&inst, &processes).unwrap();
    assert!(!obs.actual["sleeper"].exists, "{project} already exists");
    let plan = r
        .plan(
            &obs,
            &Change::Up {
                selection: Selection::Default,
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    execute(&applied.steps, &cli);

    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &processes).unwrap();
    let status = r.status(&inst, &obs, &StdHttp);
    assert_eq!(status[0].status.state, ComponentState::Up, "{status:#?}");
    let prov = status[0].status.provenance.as_ref().unwrap();
    assert!(
        prov.image
            .as_deref()
            .is_some_and(|d| d.starts_with("sha256:")),
        "{prov:?}"
    );
    assert!(
        obs.containers["sleeper"]
            .image_ref
            .starts_with("alpine@sha256:"),
        "pinned by digest: {}",
        obs.containers["sleeper"].image_ref
    );

    // down
    let plan = r
        .plan(
            &obs,
            &Change::Down {
                keep_groups: vec![],
            },
        )
        .unwrap();
    let applied = r
        .apply(&inst, &obs, &plan, &ApplyOptions::default())
        .unwrap();
    execute(&applied.steps, &cli);
    let (inst, _) = r.instance(0).unwrap();
    let obs = r.observe(&inst, &processes).unwrap();
    let status = r.status(&inst, &obs, &StdHttp);
    assert_eq!(
        status[0].status.state,
        ComponentState::NotDeployed,
        "{status:#?}"
    );
}
