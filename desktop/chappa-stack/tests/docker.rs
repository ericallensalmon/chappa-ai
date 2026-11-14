//! The example stack served for real against docker: the database is
//! started through the protocol as a headless job, observed up, and the
//! stack taken down again, all through the source's routes.
//!
//! Runs only with `CHAPPA_TEST_DOCKER=1` and a docker daemon in Linux
//! mode; otherwise it prints why it skipped and passes. The `api` component
//! of the example runs on podman and `web` from a checkout that is not part
//! of the example, so only `db` (docker, `postgres:16`) is brought up.

mod common;

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chappa_stack::world::{ThreadSpawner, World};
use common::*;
use stack_core::cli::{CliRunner, EngineCli, Invocation};

fn skip(reason: &str) {
    eprintln!("skipping the real-docker round trip: {reason}");
}

/// `compose down` of the example's docker project, whatever happened.
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["compose", "-p", "local-0-core", "down", "--volumes"])
            .current_dir(&self.0)
            .output();
    }
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
    let p = Project::example();
    let _cleanup = Cleanup(p.dir.clone());
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
