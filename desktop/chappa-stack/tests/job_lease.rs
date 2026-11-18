//! One change of an instance at a time: a second changing action is refused
//! while one is pending or running (and the view says which, with its
//! verbs disabled and a cancel), an out-of-band runner is held to the same
//! lease, a job runs once, a cancel stops it and ends it `cancelled`, and a
//! job whose runner went away is settled as `interrupted` (or `failed`,
//! never started) instead of reading `running` forever.

mod common;

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chappa_stack::background::ScriptGate;
use chappa_stack::lease;
use chappa_stack::records::{JobRecord, JobStatus};
use chappa_stack::runjob::{run_job, EXIT_CANCELLED, EXIT_DONE, EXIT_REFUSED};
use chappa_stack::server::Caller;
use chappa_stack::world::{Cancel, StepExec, CANCELLED_STEP};
use common::*;
use stack_core::cli::FakeCli;
use stack_core::intent::IntentRecord;
use stack_core::jobs::RunBlock;
use stack_core::probe::FakeHttp;

fn fresh() -> FakeCli {
    docker_facts(FakeCli::new())
        .on(&["compose", "ps"], "")
        .on(
            &["docker", "manifest", "inspect", "--verbose", "postgres:16"],
            &format!(r#"{{"Descriptor":{{"digest":"{PG_DIGEST}","platform":{{"os":"linux"}}}}}}"#),
        )
        .failing(
            &["podman", "image", "inspect"],
            125,
            "Error: registry.example/api:main: image not known",
        )
        .on(
            &["podman", "info", "--format", "json"],
            r#"{"host":{"os":"linux","memTotal":8589934592,"memFree":4294967296},"version":{"Version":"5.2.0"}}"#,
        )
}

fn action<'a>(env: &'a Value, id: &str) -> &'a Value {
    env["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no stack action {id} in {}", env["actions"]))
}

fn call(action: &Value) -> Value {
    json!({
        "id": action["id"],
        "target": { "view": "environment", "key": null },
        "params": action["params"],
        "confirmed": true,
    })
}

/// Accept `up` and answer its job id.
fn accept_up(src: &chappa_stack::server::Source) -> String {
    let (_, env) = get(src, "/api/environment");
    let (status, body) = post(src, "/api/actions", &call(action(&env, "up")), true);
    assert_eq!(status, 202, "{body}");
    body["jobId"].as_str().unwrap().to_string()
}

/// Engine commands that run until the job is cancelled.
struct UntilCancelled;

impl StepExec for UntilCancelled {
    fn run(
        &self,
        _run: &RunBlock,
        _limit: Duration,
        cancel: &Cancel,
        _out: &mut dyn FnMut(&str),
    ) -> Result<i32, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cancel.is_set() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(CANCELLED_STEP.to_string())
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_second_change_is_refused_while_one_waits_for_its_pane_and_its_verbs_say_why() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let up = action(&env, "up").clone();
    let id = accept_up(&src);

    // The same click again: refused, named, nothing stored.
    let (status, body) = post(&src, "/api/actions", &call(&up), true);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("busy")),
        "{body}"
    );
    let msg = body["message"].as_str().unwrap();
    assert!(
        msg.starts_with("local: bring up is running since "),
        "{msg}"
    );
    assert_eq!(
        std::fs::read_dir(chappa_stack::records::jobs_dir(&p.work()))
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".json"))
            .count(),
        1
    );

    // The view: the job, every changing verb disabled with its name, a
    // cancel and the job's log; the job's own record is the active one.
    let (_, env) = get(&src, "/api/environment");
    assert_eq!(env["job"]["active"]["id"], id.as_str());
    assert_eq!(env["job"]["active"]["status"], "pending");
    assert_eq!(env["job"]["active"]["cancelling"], false);
    let disabled = action(&env, "up")["disabled"].as_str().unwrap();
    assert!(
        disabled.starts_with("local: bring up is running since"),
        "{disabled}"
    );
    for c in env["components"].as_array().unwrap() {
        for a in c["actions"].as_array().into_iter().flatten() {
            if a["mutates"] == true {
                assert!(a["disabled"].is_string(), "{a}");
            }
        }
    }
    let cancel = action(&env, "cancel-job");
    assert_eq!(cancel["params"]["jobId"], id.as_str());
    assert!(cancel.get("disabled").is_none());
    assert_eq!(action(&env, "job-log")["kind"], "terminal");
    // Status and plan scripts wait meanwhile.
    assert!(!src.scripts_allowed());

    // Once it ran, the next change is accepted.
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut Vec::new()), EXIT_DONE);
    let (_, env) = get(&src, "/api/environment");
    assert!(env["job"]["active"].is_null(), "{}", env["job"]);
    assert_eq!(env["job"]["last"]["status"], "done");
    assert!(action(&env, "up").get("disabled").is_none());
    assert!(src.scripts_allowed());
    assert_eq!(
        post(&src, "/api/actions", &call(action(&env, "up")), true).0,
        202
    );
}

#[test]
fn a_runner_started_by_hand_is_held_to_the_lease_and_a_job_runs_once() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let id = accept_up(&src);

    // Another job holds the instance (its runner holds the lock).
    let mut other = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    other.id = "j20261114T120000Z-0a0b0c".into();
    other.label = "local: down".into();
    let held = lease::take_lease(&p.work(), &other, "2026-11-14T12:00:00Z").unwrap();
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_REFUSED);
    let pane = String::from_utf8(pane).unwrap();
    assert!(
        pane.contains("not run: local: down is running since 12:00:00 UTC"),
        "{pane}"
    );
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Failed);
    assert!(f
        .cli
        .with(|c| c.lines())
        .iter()
        .all(|l| !l.contains("pull")));
    drop(held);

    // A job some runner holds is not run by a second one.
    let id = accept_up(&src);
    let lock = lease::try_hold(&lease::job_lock_path(&p.work(), &id))
        .unwrap()
        .unwrap();
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_REFUSED);
    assert!(String::from_utf8(pane)
        .unwrap()
        .contains("already being run"));
    drop(lock);
    // One that started before (and is not waiting) is not run again.
    JobRecord::update(&p.work(), &id, |r| r.status = JobStatus::Running).unwrap();
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_REFUSED);
    assert!(String::from_utf8(pane)
        .unwrap()
        .contains("not waiting to run"));
}

#[test]
fn a_cancel_stops_the_running_step_ends_the_job_cancelled_and_frees_the_instance() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let mut world = f.world.clone();
    world.steps = Arc::new(UntilCancelled);
    let src = source(f.world.clone(), &p.dir);
    let id = accept_up(&src);
    let (w, dir, job) = (world.clone(), p.dir.clone(), id.clone());
    let runner = std::thread::spawn(move || run_job(&w, &dir, &job, &mut Vec::new()));
    wait_until("the job to run", || {
        JobRecord::load(&p.work(), &id).unwrap().unwrap().status == JobStatus::Running
    });
    let (_, env) = get(&src, "/api/environment");
    assert_eq!(env["job"]["active"]["status"], "running");

    let caller = Caller {
        runs_jobs: true,
        actor: Some("human:tester".into()),
    };
    let body = call(action(&env, "cancel-job"));
    let (status, reply) = src.handle_as(
        "POST",
        "/api/actions",
        Some(&bearer()),
        &caller,
        &body.to_string(),
    );
    assert_eq!(status, 200, "{reply}");
    assert!(reply["message"]
        .as_str()
        .unwrap()
        .starts_with("Cancelling local: bring up"));
    let (_, env) = get(&src, "/api/environment");
    assert_eq!(env["job"]["active"]["cancelling"], true);
    assert!(action(&env, "cancel-job")["disabled"].is_string());

    assert_eq!(runner.join().unwrap(), EXIT_CANCELLED);
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Cancelled);
    assert_eq!(rec.exit_code, Some(EXIT_CANCELLED));
    assert!(
        rec.error
            .as_deref()
            .unwrap()
            .starts_with("cancelled by human:tester"),
        "{:?}",
        rec.error
    );
    let log = std::fs::read_to_string(&rec.log).unwrap();
    assert!(log.contains("job cancelled after"), "{log}");
    // The intent is still recorded, as after a failure.
    assert!(IntentRecord::load(&p.work(), "local-0").unwrap().is_some());
    // GET says failed, with the outcome.
    let (_, state) = get(&src, &format!("/api/jobs/{id}"));
    assert_eq!(
        (state["state"].as_str(), state["outcome"].as_str()),
        (Some("failed"), Some("cancelled"))
    );
    // The view: free again, the last job cancelled, as a warning.
    let (_, env) = get(&src, "/api/environment");
    assert!(env["job"]["active"].is_null());
    assert_eq!(env["job"]["last"]["status"], "cancelled");
    let alert = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "job")
        .expect("a job alert");
    assert_eq!(alert["severity"], "warn");
    assert_eq!(action(&env, "job-log")["label"], "Last job's log");
    assert!(action(&env, "up").get("disabled").is_none());
    // Nothing left to cancel.
    let (status, _) = post(
        &src,
        "/api/actions",
        &json!({"id": "cancel-job", "confirmed": true, "params": {"jobId": id}}),
        true,
    );
    assert_eq!(status, 409);
}

#[test]
fn a_job_cancelled_before_its_runner_came_never_runs() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let id = accept_up(&src);
    let (status, reply) = post(
        &src,
        "/api/actions",
        &json!({"id": "cancel-job", "confirmed": true}),
        true,
    );
    assert_eq!(status, 200, "{reply}");
    assert!(reply["message"]
        .as_str()
        .unwrap()
        .contains("before it started"));
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Cancelled);
    assert_eq!(
        run_job(&f.world, &p.dir, &id, &mut Vec::new()),
        EXIT_REFUSED
    );
    // Unconfirmed: refused.
    let id = accept_up(&src);
    let (status, _) = post(
        &src,
        "/api/actions",
        &json!({"id": "cancel-job", "params": {"jobId": id}}),
        true,
    );
    assert_eq!(status, 409);
}

#[test]
fn a_job_whose_runner_died_is_interrupted_and_shows_as_a_bad_alert() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let id = accept_up(&src);
    // Its runner started and died: the record says running, nobody holds
    // its lock.
    JobRecord::update(&p.work(), &id, |r| {
        r.status = JobStatus::Running;
        r.runner_pid = Some(999_999);
        r.started_utc = Some(r.created_utc.clone());
    })
    .unwrap();
    drop(lease::try_hold(&lease::job_lock_path(&p.work(), &id)).unwrap());
    let (_, state) = get(&src, &format!("/api/jobs/{id}"));
    assert_eq!(state["outcome"], "interrupted");
    let (_, env) = get(&src, "/api/environment");
    assert!(env["job"]["active"].is_null(), "{}", env["job"]);
    assert_eq!(env["job"]["last"]["status"], "interrupted");
    let alert = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "job")
        .expect("a job alert");
    assert_eq!(alert["severity"], "bad");
    assert!(
        alert["text"].as_str().unwrap().contains("interrupted"),
        "{alert}"
    );
    assert_eq!(
        JobRecord::load(&p.work(), &id).unwrap().unwrap().status,
        JobStatus::Interrupted
    );
    // The instance is free.
    assert_eq!(
        post(&src, "/api/actions", &call(action(&env, "up")), true).0,
        202
    );
}

#[test]
fn settling_at_start_ends_dead_runs_and_stale_pending_jobs_but_not_live_ones() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let base = JobRecord::load(&p.work(), &accept_up(&src))
        .unwrap()
        .unwrap();
    JobRecord::update(&p.work(), &base.id, |r| r.status = JobStatus::Done).unwrap();
    let make = |id: &str, status: JobStatus, created: &str| {
        let mut r = base.clone();
        r.id = id.into();
        r.status = status;
        r.created_utc = created.into();
        r.save(&p.work()).unwrap();
        id.to_string()
    };
    let now = stack_core::intent::utc_now();
    let stale = make(
        "j20200101T000000Z-000001",
        JobStatus::Pending,
        "2020-01-01T00:00:00Z",
    );
    let waiting = make("j20200101T000000Z-000002", JobStatus::Pending, &now);
    let dead = make("j20200101T000000Z-000003", JobStatus::Running, &now);
    drop(lease::try_hold(&lease::job_lock_path(&p.work(), &dead)).unwrap());
    let live = make("j20200101T000000Z-000004", JobStatus::Running, &now);
    let held = lease::try_hold(&lease::job_lock_path(&p.work(), &live))
        .unwrap()
        .unwrap();
    let mut settled = src.settle_jobs();
    settled.sort();
    assert_eq!(settled, vec![stale.clone(), dead.clone()]);
    let st = |id: &str| JobRecord::load(&p.work(), id).unwrap().unwrap();
    assert_eq!(st(&stale).status, JobStatus::Failed);
    assert!(st(&stale).error.unwrap().starts_with("never started"));
    assert_eq!(st(&dead).status, JobStatus::Interrupted);
    assert_eq!(st(&live).status, JobStatus::Running);
    assert_eq!(st(&waiting).status, JobStatus::Pending);
    drop(held);
}

#[test]
fn a_pane_that_could_not_open_reports_why_and_a_pane_killed_mid_job_reads_interrupted() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let id = accept_up(&src);
    let (status, _) = post(
        &src,
        &format!("/api/jobs/{id}"),
        &json!({"exitCode": null, "endedUtc": "2026-11-14T12:00:09Z", "output": "", "error": "the pane did not start: no such program\nsecond line"}),
        false,
    );
    assert_eq!(status, 200);
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Failed);
    assert_eq!(
        rec.error.as_deref(),
        Some("the pane did not start: no such program second line")
    );
    // The instance is free again at once.
    let id = accept_up(&src);
    JobRecord::update(&p.work(), &id, |r| r.status = JobStatus::Running).unwrap();
    let (status, _) = post(
        &src,
        &format!("/api/jobs/{id}"),
        &json!({"exitCode": -1, "endedUtc": "2026-11-14T12:00:09Z", "output": "half"}),
        false,
    );
    assert_eq!(status, 200);
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Interrupted);
    assert!(rec.error.unwrap().starts_with("interrupted"));
}
