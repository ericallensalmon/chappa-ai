//! The example stack served for real against docker: the database is
//! started through the protocol as a headless job, observed up, and the
//! stack taken down again, all through the source's routes.
//!
//! Runs only with `CHAPPA_TEST_DOCKER=1` and a docker daemon in Linux
//! mode; otherwise it prints why it skipped and passes. The `api` component
//! of the example runs on podman and `web` from a checkout that is not part
//! of the example, so only `db` (docker, `postgres:16`) is brought up.
//!
//! Both tests run the example's slot 0, which is one compose project
//! (`local-0-core`) with fixed container, network and volume names, so
//! they take turns.

mod common;

use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chappa_stack::world::{ThreadSpawner, World};
use common::*;
use stack_core::cli::{CliRunner, EngineCli, Invocation};

fn skip(reason: &str) {
    eprintln!("skipping the real-docker round trip: {reason}");
}

static SLOT_ZERO: Mutex<()> = Mutex::new(());

/// The example's slot 0, held for the whole test. A test that failed
/// while holding it leaves nothing behind (its [`Cleanup`] ran), so a
/// poisoned lock is still a free slot.
fn take_slot_zero() -> MutexGuard<'static, ()> {
    SLOT_ZERO.lock().unwrap_or_else(|e| e.into_inner())
}

/// `compose down` of the example's docker project, before the test (what
/// an interrupted earlier run left) and after it, whatever happened.
struct Cleanup(std::path::PathBuf);

impl Cleanup {
    fn new(dir: &std::path::Path) -> Cleanup {
        let c = Cleanup(dir.to_path_buf());
        c.down();
        c
    }

    fn down(&self) {
        let _ = std::process::Command::new("docker")
            .args(["compose", "-p", "local-0-core", "down", "--volumes"])
            .current_dir(&self.0)
            .output();
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.down();
    }
}

/// The example keeps its database password out of its files: the runner
/// generated one for slot 0, and the scan of the instance env, which
/// reads the resolved URLs that carry it, found nothing.
fn the_password_was_generated_and_not_flagged(p: &Project, env: &Value) {
    let password = std::fs::read_to_string(p.work().join("instances/local-0/secrets/db")).unwrap();
    assert_eq!(password.len(), 48);
    let findings = env["secretScan"]["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .all(|f| !f["file"].as_str().unwrap().starts_with("instance env")),
        "{findings:?}"
    );
}

fn wait(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("timed out waiting for {what}");
}

fn component<'a>(env: &'a Value, id: &str) -> &'a Value {
    env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

fn run(src: &chappa_stack::server::Source, action: &Value, key: Option<&str>) -> String {
    let body = json!({
        "id": action["id"],
        "target": {"view": "environment", "key": key},
        "params": action["params"],
        "confirmed": true,
    });
    let (status, reply) = post(src, "/api/actions", &body, false);
    assert_eq!(status, 202, "{reply}");
    assert!(reply.get("run").is_none());
    let id = reply["jobId"].as_str().unwrap().to_string();
    wait("the job", Duration::from_secs(600), || {
        get(src, &format!("/api/jobs/{id}")).1["state"] != "running"
    });
    let (_, state) = get(src, &format!("/api/jobs/{id}"));
    assert_eq!(
        state["state"],
        "done",
        "{}",
        state["log"].as_str().unwrap_or("")
    );
    id
}

#[test]
fn the_example_database_comes_up_and_goes_down_through_the_protocol() {
    if std::env::var("CHAPPA_TEST_DOCKER").as_deref() != Ok("1") {
        return skip("CHAPPA_TEST_DOCKER is not 1");
    }
    let info = CliRunner.run(&Invocation::new(
        "docker",
        &["info", "--format", "{{.OSType}}"],
    ));
    match info {
        Ok(out) if out.ok() && out.stdout.trim() == "linux" => {}
        Ok(out) => return skip(&format!("docker is not in Linux mode ({})", out.summary())),
        Err(e) => return skip(&format!("docker is not reachable ({e})")),
    }
    let _slot = take_slot_zero();
    let p = Project::example();
    let _cleanup = Cleanup::new(&p.dir);
    let mut world = World::real();
    world.spawner = Arc::new(ThreadSpawner);
    world.hosts_text = Some("127.0.0.1 localhost dev.example.test\n".into());
    let src = source(world, &p.dir);

    let (_, env) = get(&src, "/api/environment");
    assert_eq!(component(&env, "db")["state"], "not-deployed");
    let start_db = component(&env, "db")["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "start")
        .unwrap()
        .clone();
    assert!(start_db["confirm"].as_str().unwrap().contains("start db"));
    let job = run(&src, &start_db, Some("db"));
    let log = std::fs::read_to_string(p.work().join("jobs").join(format!("{job}.log"))).unwrap();
    assert!(log.contains("docker pull postgres@sha256:"), "{log}");

    wait("db to be up", Duration::from_secs(120), || {
        component(&get(&src, "/api/environment").1, "db")["state"] == "up"
    });
    let (_, env) = get(&src, "/api/environment");
    the_password_was_generated_and_not_flagged(&p, &env);
    let db = component(&env, "db");
    assert_eq!(db["source"]["mode"], "prebuilt");
    assert!(db["source"]["image"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(db["uptime"].as_str().is_some());
    let logs = db["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "logs")
        .unwrap();
    assert_eq!(logs["command"][0], "docker");

    let down = env["facts"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "down")
        .unwrap()
        .clone();
    run(&src, &down, None);
    let (_, env) = get(&src, "/api/environment");
    assert_eq!(component(&env, "db")["state"], "not-deployed");
}

/// An actions call on the Work view, run headless, waited for.
fn run_work(src: &chappa_stack::server::Source, action: &Value, key: &str) -> String {
    let body = json!({
        "id": action["id"],
        "target": {"view": "work", "key": key},
        "params": action["params"],
        "confirmed": true,
    });
    let (status, reply) = post(src, "/api/actions", &body, false);
    assert_eq!(status, 202, "{reply}");
    let id = reply["jobId"].as_str().unwrap().to_string();
    wait("the job", Duration::from_secs(600), || {
        get(src, &format!("/api/jobs/{id}")).1["state"] != "running"
    });
    let (_, state) = get(src, &format!("/api/jobs/{id}"));
    assert_eq!(
        state["state"],
        "done",
        "{}",
        state["log"].as_str().unwrap_or("")
    );
    state["log"].as_str().unwrap_or("").to_string()
}

fn docker_ready() -> bool {
    if std::env::var("CHAPPA_TEST_DOCKER").as_deref() != Ok("1") {
        skip("CHAPPA_TEST_DOCKER is not 1");
        return false;
    }
    match CliRunner.run(&Invocation::new(
        "docker",
        &["info", "--format", "{{.OSType}}"],
    )) {
        Ok(out) if out.ok() && out.stdout.trim() == "linux" => true,
        Ok(out) => {
            skip(&format!("docker is not in Linux mode ({})", out.summary()));
            false
        }
        Err(e) => {
            skip(&format!("docker is not reachable ({e})"));
            false
        }
    }
}

#[test]
fn the_example_saves_its_database_goes_down_and_resumes_it() {
    if !docker_ready() {
        return;
    }
    let _slot = take_slot_zero();
    let p = Project::example();
    let _cleanup = Cleanup::new(&p.dir);
    // A work item with no repos, bound to slot 0 (the example builds
    // nothing from a clone, so binding changes no component).
    let item = p.work().join("ABC-1");
    std::fs::create_dir_all(&item).unwrap();
    stack_core::workspace::ItemRecord::new(
        stack_core::workspace::ItemRef {
            source: "tracker".into(),
            key: "ABC-1".into(),
        },
        "ABC-1",
        "2026-11-14T12:00:00Z",
    )
    .save(&item)
    .unwrap();
    let mut active = stack_core::intent::ActiveRecord::default();
    active.set(0, "ABC-1", "local-0", "2026-11-14T12:00:00Z");
    active.save(&p.work()).unwrap();
    let mut world = World::real();
    world.spawner = Arc::new(ThreadSpawner);
    world.hosts_text = Some("127.0.0.1 localhost dev.example.test\n".into());
    let src = source(world, &p.dir);

    let (_, env) = get(&src, "/api/environment");
    let start_db = component(&env, "db")["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "start")
        .unwrap()
        .clone();
    run(&src, &start_db, Some("db"));
    wait("db to be up", Duration::from_secs(120), || {
        component(&get(&src, "/api/environment").1, "db")["state"] == "up"
    });

    let row = |src: &chappa_stack::server::Source| {
        get(src, "/api/work").1["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == "ABC-1")
            .unwrap()
            .clone()
    };
    let save = row(&src)["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "save-state")
        .unwrap()
        .clone();
    let log = run_work(&src, &save, "ABC-1");
    assert!(log.contains("✓ db: snapshot ABC-1-db-"), "{log}");
    let states = stack_core::states::list_states(&item);
    assert_eq!(states.len(), 1);
    let first = states[0].id.clone();

    // Down saves first, keeps the volume, and the snapshot in it.
    let (_, env) = get(&src, "/api/environment");
    let down = env["facts"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "down")
        .unwrap()
        .clone();
    run(&src, &down, None);
    assert_eq!(stack_core::states::list_states(&item).len(), 2);
    assert_eq!(
        component(&get(&src, "/api/environment").1, "db")["state"],
        "not-deployed"
    );

    // Resume the first state: db comes back from its digest and the
    // snapshot is restored once it runs.
    let r = row(&src);
    let resume = r["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "saved-states")
        .unwrap()["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == format!("resume:{first}"))
        .unwrap()
        .clone();
    assert!(resume["confirm"]
        .as_str()
        .unwrap()
        .contains("this overwrites the database db"));
    let log = run_work(&src, &resume, "ABC-1");
    assert!(log.contains("docker pull postgres@sha256:"), "{log}");
    assert!(log.contains("✓ db: restored ABC-1-db-"), "{log}");
    wait("db to be up again", Duration::from_secs(120), || {
        component(&get(&src, "/api/environment").1, "db")["state"] == "up"
    });
}
