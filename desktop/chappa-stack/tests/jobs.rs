//! Actions → plans → jobs on the example stack, through fakes: the confirm
//! text is the plan, a confirmed action becomes a stored job with a `run`
//! block for a pane (or runs itself headless), `run-job` executes the whole
//! stored plan and leaves the three job files, the app's exit report is
//! recorded, stale and unconfirmed calls are refused, and retention keeps
//! what it promises.

mod common;

use serde_json::{json, Value};
use std::fs;

use chappa_stack::records::{self, JobRecord, JobStatus, JobWork};
use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_FAILED, EXIT_REFUSED};
use common::*;
use stack_core::cli::{CliOutput, FakeCli};
use stack_core::definition::JobsDef;
use stack_core::intent::IntentRecord;
use stack_core::probe::FakeHttp;

/// The example's engines before anything ran: no containers, postgres
/// resolvable, the api image not yet local on podman, both engines up.
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

fn stack_action<'a>(env: &'a Value, id: &str) -> &'a Value {
    env["facts"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no stack action {id}"))
}

fn component_action<'a>(env: &'a Value, component: &str, id: &str) -> Option<&'a Value> {
    env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == component)?["actions"]
        .as_array()?
        .iter()
        .find(|a| a["id"] == id)
}

fn call(action: &Value, key: Option<&str>, confirmed: bool) -> Value {
    json!({
        "id": action["id"],
        "target": { "view": "environment", "key": key },
        "params": action["params"],
        "note": null,
        "confirmed": confirmed,
    })
}

#[test]
fn up_is_offered_with_its_plan_and_answers_a_pane_to_run_it_in() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let up = stack_action(&env, "up");
    assert_eq!(up["kind"], "endpoint");
    assert_eq!(up["mutates"], true);
    assert_eq!(
        up["confirm"],
        "Pull db (postgres:16), migrate (postgres:16), api (registry.example/api:main); start db, migrate, api, web."
    );
    // Nothing runs yet, so nothing to stop or take down.
    let ids: Vec<&str> = env["facts"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["up", "gc"]);
    // Per component: start (with what it needs) and the mode switches.
    let start_api = component_action(&env, "api", "start").unwrap();
    assert_eq!(
        start_api["confirm"],
        "Pull db (postgres:16), api (registry.example/api:main); start db, api."
    );
    let to_source = component_action(&env, "api", "mode-source").unwrap();
    assert!(
        to_source["confirm"]
            .as_str()
            .unwrap()
            .starts_with("Build api"),
        "{to_source}"
    );
    assert!(component_action(&env, "api", "mode-dev").is_some());
    assert!(
        component_action(&env, "api", "mode-prebuilt").is_none(),
        "already prebuilt"
    );
    assert!(component_action(&env, "api", "watch").is_some());

    // Unconfirmed, stale, not offered, wrong view: refused, nothing stored.
    let (status, body) = post(&src, "/api/actions", &call(up, None, false), true);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("confirmation-required"))
    );
    let mut stale = call(up, None, true);
    stale["params"]["fingerprint"] = "0000".into();
    let (status, body) = post(&src, "/api/actions", &stale, true);
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    assert!(body["message"].as_str().unwrap().contains("Pull db"));
    let mut down = call(up, None, true);
    down["id"] = "down".into();
    let (status, body) = post(&src, "/api/actions", &down, true);
    assert_eq!((status, body["kind"].as_str()), (409, Some("not-offered")));
    let mut other = call(up, None, true);
    other["target"]["view"] = "work".into();
    assert_eq!(post(&src, "/api/actions", &other, true).0, 404);
    assert!(!records::jobs_dir(&p.work()).exists());

    let (status, body) = post(&src, "/api/actions", &call(up, None, true), true);
    assert_eq!(status, 202, "{body}");
    let id = body["jobId"].as_str().unwrap();
    assert!(records::is_job_id(id), "{id}");
    assert_eq!(
        body["run"]["argv"],
        json!(["/opt/chappa/chappa-stack", "run-job", id])
    );
    let cwd = body["run"]["cwd"].as_str().unwrap();
    assert!(std::path::Path::new(cwd).is_absolute());
    assert_eq!(
        std::path::Path::new(cwd),
        std::path::absolute(&p.dir).unwrap()
    );
    assert_eq!(body["run"]["title"], "local: bring up");
    let rec = JobRecord::load(&p.work(), id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Pending);
    assert_eq!(rec.action, "up");
    assert!(rec.mutates);
    assert!(!rec.headless);
    assert_eq!(rec.argv[1], "run-job");
    assert!(matches!(rec.work, JobWork::Plan { .. }));
    // Until the pane reports, the job is running.
    let (_, state) = get(&src, &format!("/api/jobs/{id}"));
    assert_eq!(state, json!({"state": "running"}));
}

#[test]
fn run_job_executes_the_stored_plan_and_leaves_its_three_files() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();

    let mut pane: Vec<u8> = Vec::new();
    let code = run_job(&f.world, &p.dir, &id, &mut pane);
    let pane = String::from_utf8(pane).unwrap();
    assert_eq!(code, EXIT_DONE, "{pane}");
    // The whole plan in one pane, in order.
    let order = [
        "resolving images…",
        "check reachable:docker: ok",
        "check hostname:dev.example.test: ok",
        "✓ write local-0-core override",
        "▶ db: pull db (16)",
        "  $ docker pull postgres@sha256:",
        "▶ db: up db",
        "▶ migrate: up migrate",
        "▶ api: pull api (main)",
        "  $ podman pull registry.example/api:main",
        "▶ api: up api",
        "▶ web: start web-dev",
        "✓ web: running as process 4000",
        "✓ write intent record",
        "✓ done in",
    ];
    let mut at = 0;
    for needle in order {
        let found = pane[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not after {at} in:\n{pane}"));
        at += found + needle.len();
    }
    // The engine saw the commands, the process host the dev server.
    let lines = f.cli.with(|c| c.lines());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("docker pull postgres@sha256:")),
        "{lines:#?}"
    );
    assert!(lines
        .iter()
        .any(|l| l.contains("podman pull registry.example/api:main")));
    assert!(lines
        .iter()
        .any(|l| l.contains("local-0-core") && l.ends_with("up -d --no-deps db")));
    let spawned = f.procs.spawned.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].argv, ["npm", "run", "dev"]);
    assert_eq!(spawned[0].env["CHAPPA_STACK_SLOT"], "0");

    // The three files.
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Done);
    assert_eq!(rec.exit_code, Some(0));
    assert!(rec.started_utc.is_some() && rec.ended_utc.is_some());
    assert_eq!(rec.runner_pid, Some(std::process::id()));
    let log = fs::read_to_string(&rec.log).unwrap();
    assert_eq!(log.replace("\r\n", "\n"), pane.replace("\r\n", "\n"));
    let result: Value = serde_json::from_str(&fs::read_to_string(&rec.result).unwrap()).unwrap();
    assert_eq!(result["jobId"], id.as_str());
    assert_eq!(result["ok"], true);
    assert!(result["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["status"] == "done"));
    assert_eq!(result["components"]["db"]["desired"], "running");
    assert_eq!(result["provenance"]["db"]["image"], PG_DIGEST);
    // The intent and the process table are on disk.
    let intent = IntentRecord::load(&p.work(), "local-0").unwrap().unwrap();
    assert_eq!(
        intent.components["web"].desired,
        stack_core::intent::Desired::Running
    );
    let table = chappa_stack::procs::ProcessTable::load(&p.work(), "local-0");
    assert_eq!(table.processes["web"].pid, 4000);

    // GET reports done with the log; the pane's exit report is recorded
    // but does not replace the runner's own log.
    let (_, state) = get(&src, &format!("/api/jobs/{id}"));
    assert_eq!(state["state"], "done");
    assert_eq!(state["exitCode"], 0);
    assert!(state["log"].as_str().unwrap().contains("▶ api: up api"));
    let (status, _) = post(
        &src,
        &format!("/api/jobs/{id}"),
        &json!({"exitCode": 0, "endedUtc": "2026-11-14T12:00:09Z", "output": "pane dump"}),
        false,
    );
    assert_eq!(status, 200);
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.report.as_ref().unwrap().exit_code, Some(0));
    assert_eq!(rec.status, JobStatus::Done);
    assert!(!fs::read_to_string(&rec.log).unwrap().contains("pane dump"));
    // A finished job does not run twice.
    assert_eq!(
        run_job(&f.world, &p.dir, &id, &mut Vec::new()),
        EXIT_REFUSED
    );
    // Unknown jobs, and ids that are not ours.
    assert_eq!(get(&src, "/api/jobs/j20000101T000000Z-abcdef").0, 404);
    assert_eq!(get(&src, "/api/jobs/..%2F..%2Fsecret").0, 404);
}

#[test]
fn without_a_pane_the_source_runs_the_job_itself() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (status, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        false,
    );
    assert_eq!(status, 202);
    assert!(body.get("run").is_none(), "{body}");
    let id = body["jobId"].as_str().unwrap().to_string();
    assert!(eventually(|| get(&src, &format!("/api/jobs/{id}")).1
        ["state"]
        == "done"));
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert!(rec.headless);
    assert!(fs::read_to_string(&rec.log).unwrap().contains("✓ done in"));
}

#[test]
fn an_exit_report_settles_a_job_whose_runner_never_finished() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap();
    let (status, _) = post(
        &src,
        &format!("/api/jobs/{id}"),
        &json!({"exitCode": 3, "endedUtc": "2026-11-14T12:00:09Z", "output": "cannot start\n"}),
        false,
    );
    assert_eq!(status, 200);
    let (_, state) = get(&src, &format!("/api/jobs/{id}"));
    assert_eq!(state["state"], "failed");
    assert_eq!(state["exitCode"], 3);
    assert_eq!(state["log"], "cannot start\n");
    assert!(state["error"].as_str().unwrap().contains("code 3"));
    let rec = JobRecord::load(&p.work(), id).unwrap().unwrap();
    assert_eq!(rec.ended_utc.as_deref(), Some("2026-11-14T12:00:09Z"));
    assert_eq!(
        post(
            &src,
            "/api/jobs/j20000101T000000Z-abcdef",
            &json!({}),
            false
        )
        .0,
        404
    );
}

#[test]
fn a_failed_step_fails_the_job_skips_the_rest_and_still_records_the_intent() {
    let p = Project::example();
    let cli = fresh().failing(
        &["local-0-core", "up", "migrate"],
        1,
        "migrate: no such image",
    );
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_FAILED);
    let pane = String::from_utf8(pane).unwrap();
    assert!(pane.contains("migrate: no such image"), "{pane}");
    assert!(
        pane.contains("✗ migrate: up migrate: exited with code 1"),
        "{pane}"
    );
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Failed);
    assert_eq!(rec.exit_code, Some(EXIT_FAILED));
    assert!(rec.error.as_deref().unwrap().contains("up migrate"));
    let result: Value = serde_json::from_str(&fs::read_to_string(&rec.result).unwrap()).unwrap();
    let statuses: Vec<&str> = result["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["status"].as_str().unwrap())
        .collect();
    assert!(
        statuses.contains(&"failed") && statuses.contains(&"skipped"),
        "{statuses:?}"
    );
    assert!(
        f.procs.spawned.lock().unwrap().is_empty(),
        "web is never started"
    );
    // api never ran, but it is wanted: it will show as a problem.
    assert!(IntentRecord::load(&p.work(), "local-0").unwrap().is_some());
    assert!(!stack_core::images::ProvenanceCache::path(&p.work(), "local-0").exists());
}

#[test]
fn a_plan_the_stack_moved_away_from_is_refused_at_run_time() {
    let p = Project::example();
    let f = fakes(fresh(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();
    // Someone started db by hand between the confirm and the pane.
    let ctr = Ctr::running("db");
    f.cli.0.lock().unwrap().add(
        &["docker", "compose", "-p", "local-0-core", "ps"],
        CliOutput::success(&ctr.ps("local-0-core").to_string()),
    );
    f.cli.0.lock().unwrap().add(
        &["docker", "inspect", "--type", "container"],
        CliOutput::success(&Value::from(vec![ctr.inspect("local-0-core")]).to_string()),
    );
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_REFUSED);
    let pane = String::from_utf8(pane).unwrap();
    assert!(
        pane.contains("the stack changed since this plan was confirmed"),
        "{pane}"
    );
    assert!(f.cli.with(|c| c.calls_matching(&["up", "-d"]).is_empty()));
    // And the same move makes the old confirm stale at the endpoint.
    let (status, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
}

#[test]
fn a_failed_check_before_up_stops_the_job_before_any_step() {
    let p = Project::example();
    let cli = fresh().on(
        &["docker", "version", "--format", "json"],
        r#"{"Client":{"Version":"29.0.0"},"Server":null}"#,
    );
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();
    let mut pane = Vec::new();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut pane), EXIT_REFUSED);
    let pane = String::from_utf8(pane).unwrap();
    assert!(pane.contains("check reachable:docker: FAILED"), "{pane}");
    assert!(
        pane.contains("not started, a check before up failed"),
        "{pane}"
    );
    assert!(f.cli.with(|c| c.calls_matching(&["pull"]).is_empty()));
}

/// Bring the example up through a job, then report everything running.
fn brought_up() -> (Project, Fakes) {
    let p = Project::example();
    let http = FakeHttp::default()
        .status("http://dev.example.test:14001/health/ready", 200)
        .status("http://dev.example.test:14002/", 302);
    let f = fakes(fresh(), http);
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(stack_action(&env, "up"), None, true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut Vec::new()), EXIT_DONE);
    let core = [
        Ctr::running("db").health("healthy"),
        Ctr::exited("migrate", 0),
    ];
    let api = Ctr::running("api");
    let mut c = f.cli.0.lock().unwrap();
    let ps: Vec<String> = core
        .iter()
        .map(|x| x.ps("local-0-core").to_string())
        .collect();
    c.add(
        &["docker", "compose", "-p", "local-0-core", "ps"],
        CliOutput::success(&ps.join("\n")),
    );
    c.add(
        &["docker", "inspect", "--type", "container"],
        CliOutput::success(
            &Value::from(
                core.iter()
                    .map(|x| x.inspect("local-0-core"))
                    .collect::<Vec<_>>(),
            )
            .to_string(),
        ),
    );
    c.add(
        &["podman", "compose", "-p", "local-0-apis", "ps"],
        CliOutput::success(&Value::from(vec![api.ps("local-0-apis")]).to_string()),
    );
    c.add(
        &["podman", "inspect", "--type", "container"],
        CliOutput::success(&Value::from(vec![api.inspect("local-0-apis")]).to_string()),
    );
    drop(c);
    (p, f)
}

#[test]
fn per_component_stop_and_stack_down_plan_what_they_say() {
    let (p, f) = brought_up();
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let states: Vec<(String, String)> = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().into(),
                c["state"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(states.contains(&("web".into(), "up".into())), "{states:?}");
    assert_eq!(
        stack_action(&env, "down")["confirm"],
        "Stop web, api, db; remove web, api, migrate, db. Containers are removed; volumes are kept."
    );
    assert_eq!(stack_action(&env, "stop")["confirm"], "Stop web, api, db.");
    let stop_web = component_action(&env, "web", "stop").unwrap();
    assert_eq!(stop_web["confirm"], "Stop web.");
    assert!(
        component_action(&env, "web", "restart").is_none(),
        "processes restart by stop/start"
    );
    assert_eq!(
        component_action(&env, "db", "restart").unwrap()["confirm"],
        "Restart db (stop with a 10 s grace, then start)."
    );
    // Stopping the process component ends the process the stack started.
    let (status, body) = post(
        &src,
        "/api/actions",
        &call(stop_web, Some("web"), true),
        true,
    );
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["run"]["title"], "local web: stop");
    let id = body["jobId"].as_str().unwrap().to_string();
    let mut pane = Vec::new();
    assert_eq!(
        run_job(&f.world, &p.dir, &id, &mut pane),
        EXIT_DONE,
        "{}",
        String::from_utf8_lossy(&pane)
    );
    assert_eq!(*f.procs.stopped.lock().unwrap(), [4000]);
    let table = chappa_stack::procs::ProcessTable::load(&p.work(), "local-0");
    assert!(table.processes["web"].stopped_utc.is_some());
    let (_, env) = get(&src, "/api/environment");
    let web = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "web")
        .unwrap();
    assert_eq!(web["state"], "stopped-idle");
    // Restart is one compose command, as a job.
    let restart = component_action(&env, "db", "restart").unwrap().clone();
    let (_, body) = post(
        &src,
        "/api/actions",
        &call(&restart, Some("db"), true),
        true,
    );
    let id = body["jobId"].as_str().unwrap().to_string();
    assert_eq!(run_job(&f.world, &p.dir, &id, &mut Vec::new()), EXIT_DONE);
    assert!(f.cli.with(|c| !c
        .calls_matching(&["local-0-core", "restart", "-t", "10", "db"])
        .is_empty()));
}

fn record(id: &str, created: &str, status: JobStatus) -> JobRecord {
    let work = std::path::PathBuf::from("w");
    JobRecord {
        id: id.into(),
        action: "up".into(),
        label: "x".into(),
        target: None,
        item: None,
        stack: "local".into(),
        slot: 0,
        instance: "local-0".into(),
        argv: vec![],
        cwd: ".".into(),
        mutates: true,
        confirm: String::new(),
        status,
        exit_code: None,
        created_utc: created.into(),
        started_utc: None,
        ended_utc: None,
        runner_pid: None,
        headless: false,
        log: JobRecord::log_path(&work, id),
        result: JobRecord::result_path(&work, id),
        work: JobWork::Gc,
        report: None,
        error: None,
    }
}

#[test]
fn retention_keeps_the_younger_or_the_newest_whichever_keeps_more() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path();
    let jobs = [
        (
            "j20260101T000000Z-000001",
            "2026-01-01T00:00:00Z",
            JobStatus::Done,
        ),
        (
            "j20260201T000000Z-000002",
            "2026-02-01T00:00:00Z",
            JobStatus::Failed,
        ),
        (
            "j20260301T000000Z-000003",
            "2026-03-01T00:00:00Z",
            JobStatus::Running,
        ),
        (
            "j20261101T000000Z-000004",
            "2026-11-01T00:00:00Z",
            JobStatus::Done,
        ),
        (
            "j20261110T000000Z-000005",
            "2026-11-10T00:00:00Z",
            JobStatus::Done,
        ),
    ];
    for (id, created, status) in jobs {
        record(id, created, status).save(work).unwrap();
        fs::write(JobRecord::log_path(work, id), "log").unwrap();
        fs::write(JobRecord::result_path(work, id), "{}").unwrap();
    }
    let now = "2026-11-14T00:00:00Z";
    // 90 days or the newest 1: the two November jobs are young, the
    // running March one is never pruned, January and February go.
    let removed = records::prune(
        work,
        &JobsDef {
            keep_days: 90,
            keep_count: 1,
        },
        now,
    );
    assert_eq!(
        removed,
        ["j20260201T000000Z-000002", "j20260101T000000Z-000001"]
    );
    for id in &removed {
        assert!(!JobRecord::path(work, id).exists());
        assert!(!JobRecord::log_path(work, id).exists());
        assert!(!JobRecord::result_path(work, id).exists());
    }
    assert!(JobRecord::path(work, "j20260301T000000Z-000003").exists());
    // Count keeps more than age: a 1-day window with the newest 3 kept.
    let removed = records::prune(
        work,
        &JobsDef {
            keep_days: 1,
            keep_count: 3,
        },
        now,
    );
    assert!(removed.is_empty(), "{removed:?}");
    let removed = records::prune(
        work,
        &JobsDef {
            keep_days: 1,
            keep_count: 1,
        },
        now,
    );
    assert_eq!(removed, ["j20261101T000000Z-000004"]);
    // The defaults.
    assert_eq!(
        records::retention(&Default::default()),
        JobsDef {
            keep_days: 90,
            keep_count: 500
        }
    );
}
