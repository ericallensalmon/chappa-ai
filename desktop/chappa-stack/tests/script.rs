//! Script components through the source: the example stack end to end
//! with real `node` (status, up, status, stop, down), status that never
//! waits on a slow script, verbs offered only where a script exists and
//! the stack-wide confirm that says what it skips, confirms built from a
//! plan script (and when it fails, and without one), a confirm that moved
//! refusing the call, a placeholder that needs an item refused before
//! anything runs, real jobs with an exit code and with a timeout that
//! stops the whole process tree, and a saved state that resumes a script
//! component by running its `up` again.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_FAILED};
use chappa_stack::server::Source;
use chappa_stack::world::{ChildSteps, ManualClock, ThreadSpawner, World};
use common::*;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::intent::ActiveRecord;
use stack_core::probe::FakeHttp;
use stack_core::script::{ScriptRuns, ScriptVerb};
use stack_core::workspace::{ItemRecord, ItemRef};

const ENV: &str = "/api/environment";
const NOW: &str = "2026-11-15T12:00:00Z";

fn has_node() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn env(src: &Source) -> Value {
    get(src, ENV).1
}

/// The Environment after the background has answered what the last look
/// asked for.
fn look(src: &Source, world: &World) -> Value {
    src.background.refresh_once(world);
    env(src)
}

fn row<'a>(env: &'a Value, id: &str) -> &'a Value {
    env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {}", env["components"]))
}

fn ids(env: &Value) -> Vec<String> {
    env["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

fn action<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"] == id)
}

fn action_ids(row: &Value) -> Vec<String> {
    row["actions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["kind"] == "endpoint")
                .map(|x| x["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The rows of the runner-metadata section that say who reports a
/// script's rows, as `label=value`.
fn meta_rows(env: &Value) -> Vec<String> {
    let meta = env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "meta")
        .expect("a runner-metadata section");
    meta["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["label"] == "Scripts")
        .map(|r| {
            format!(
                "{}={}",
                r["label"].as_str().unwrap(),
                r["value"].as_str().unwrap()
            )
        })
        .collect()
}

fn stack_actions(env: &Value) -> Value {
    env["actions"].clone()
}

fn act(src: &Source, id: &str, key: Option<&str>, params: &Value) -> (u16, Value) {
    post(
        src,
        "/api/actions",
        &json!({ "id": id, "target": { "view": "environment", "key": key }, "params": params, "confirmed": true }),
        true,
    )
}

fn run(world: &World, dir: &Path, reply: &Value) -> (i32, String) {
    let id = reply["jobId"]
        .as_str()
        .unwrap_or_else(|| panic!("no job: {reply}"));
    let mut pane = Vec::new();
    let code = run_job(world, dir, id, &mut pane);
    (code, String::from_utf8(pane).unwrap())
}

fn badge_texts(row: &Value) -> Vec<String> {
    row["badges"]
        .as_array()
        .map(|b| {
            b.iter()
                .map(|x| x["text"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------ the example

#[test]
fn the_example_stack_runs_status_up_stop_and_down_with_real_node() {
    if !has_node() {
        eprintln!("skipping the script example: node is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("script");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../stack/examples/script"),
        &dir,
    );
    let mut world = World::real();
    world.spawner = Arc::new(ThreadSpawner);
    let src = source(world.clone(), &dir);

    // The definition loads clean.
    let first = env(&src);
    assert!(
        first["banners"].as_array().unwrap().is_empty(),
        "{}",
        first["banners"]
    );
    // Before the status script answered: one row, said so.
    assert_eq!(ids(&first), ["app"]);
    assert_eq!(badge_texts(row(&first, "app")), ["status not read yet"]);

    // status: both services, not deployed, each in the group the status
    // names; who reports them is said once, not on every row.
    let e = look(&src, &world);
    assert_eq!(ids(&e), ["app/api", "app/web"]);
    for id in ["app/api", "app/web"] {
        assert_eq!(row(&e, id)["state"], "not-deployed", "{id}");
        assert!(row(&e, id).get("hint").is_none(), "{}", row(&e, id));
    }
    assert_eq!(row(&e, "app/api")["group"], "backend");
    assert_eq!(row(&e, "app/web")["group"], "frontend");
    let groups: Vec<String> = e["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            format!(
                "{}={}",
                g["id"].as_str().unwrap(),
                g["label"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(groups, ["backend=Back end", "frontend=Front end"]);
    assert_eq!(
        e["freshness"]["notices"],
        json!(null),
        "who reports the rows is runner metadata, not a notice"
    );
    assert_eq!(
        meta_rows(&e),
        ["Scripts=App reports 2 components; their actions run App's scripts"]
    );
    let facts = e["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "script-app")
        .expect("the status script's facts");
    assert_eq!(facts["rows"][0]["value"], "scripted-0");

    // App stands for the whole stack: its verbs are the stack's, drawn
    // once, and the stack's own plans that would do the same are not
    // offered beside them. Only the `{row}` verb is on the rows.
    assert_eq!(
        action_ids(&json!({ "actions": stack_actions(&e) })),
        ["app:up", "app:start", "app:stop", "app:down"],
        "rebuild has no script; restart is per row"
    );
    assert_eq!(action_ids(row(&e, "app/api")), ["restart"]);
    // up: the confirm is the plan script's output.
    let up = action(&stack_actions(&e), "app:up").unwrap().clone();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for App.\napi: start on port 15000\nweb: start on port 15001"
    );
    let (status, reply) = act(&src, "app:up", None, &up["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&world, &dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(pane.contains("api: started on port 15000"), "{pane}");
    assert!(pane.contains("no work item bound"), "{pane}");

    // status: up, with uptime and the reported link (loopback: the stack
    // has no hostname).
    src.background.invalidate_facts();
    let e = look(&src, &world);
    let api = row(&e, "app/api");
    assert_eq!(api["state"], "up", "{api}");
    assert!(api["uptime"].is_string(), "{api}");
    assert_eq!(api["links"][0]["url"], "http://localhost:15000");
    assert_eq!(row(&e, "app/web")["state"], "up");

    // restart, on one row: `{row}` is that row's name, one argument.
    let restart = action(&api["actions"], "restart").unwrap().clone();
    assert_eq!(
        restart["confirm"],
        "Run the `restart` script for api (reported by App): node scripts/restart.js api."
    );
    let (status, reply) = act(&src, "restart", Some("app/api"), &restart["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&world, &dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(pane.contains("api: restarted on port 15000"), "{pane}");
    // A row the last status did not report is refused.
    let (status, body) = act(&src, "restart", Some("app/db"), &restart["params"]);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("not-offered")),
        "{body}"
    );
    src.background.invalidate_facts();
    let e = look(&src, &world);

    // stop, for the whole stack: the script's own confirm.
    let stop = action(&stack_actions(&e), "app:stop").unwrap().clone();
    let confirm = stop["confirm"].as_str().unwrap();
    assert!(
        confirm.starts_with("Run the `stop` script for App.\nweb: stop\napi: stop"),
        "{confirm}"
    );
    let (status, reply) = act(&src, "app:stop", None, &stop["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&world, &dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    src.background.invalidate_facts();
    let e = look(&src, &world);
    assert_eq!(row(&e, "app/api")["state"], "stopped-idle");
    assert_eq!(row(&e, "app/web")["state"], "stopped-idle");

    // down: not deployed again, the pretend state removed.
    let down = action(&stack_actions(&e), "app:down").unwrap().clone();
    let (status, reply) = act(&src, "app:down", None, &down["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&world, &dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    src.background.invalidate_facts();
    let e = look(&src, &world);
    assert_eq!(row(&e, "app/api")["state"], "not-deployed");
    assert!(!dir.join(".work/fake/scripted-0.json").exists());
    let runs = ScriptRuns::load(&dir.join(".work"), "scripted-0");
    assert_eq!(runs.components["app"].verb, ScriptVerb::Down);
    assert!(runs.components["app"].ok);
}

// ------------------------------------------------------ off the hot path

/// A status script that takes its time.
struct SlowCli(Duration);

impl EngineCli for SlowCli {
    fn run(&self, _: &Invocation) -> Result<CliOutput, CliError> {
        std::thread::sleep(self.0);
        Ok(CliOutput::success(r#"{"components":[{"state":"up"}]}"#))
    }
}

const ONE: &str = "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        up: [node, up.js]\n        status: [node, status.js]\n";

#[test]
fn the_environment_never_waits_for_a_slow_status_script() {
    let p = Project::with(ONE, &[]);
    let mut f = fakes(FakeCli::new(), FakeHttp::default());
    let slow = Arc::new(SlowCli(Duration::from_secs(3)));
    f.world.cli = slow.clone();
    f.world.scripts = slow;
    let src = source(f.world.clone(), &p.dir);
    let t = Instant::now();
    let e = env(&src);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(row(&e, "app")["state"], "not-deployed");
    assert_eq!(badge_texts(row(&e, "app")), ["status not read yet"]);
    // The refresher pays for the script; the next look reads its answer.
    src.background.refresh_once(&f.world);
    let t = Instant::now();
    let e = env(&src);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(row(&e, "app")["state"], "up");
    assert!(badge_texts(row(&e, "app")).is_empty());
}

// -------------------------------------------------------- verbs offered

fn wanted_running(p: &Project, components: &[&str]) {
    let comps: serde_json::Map<String, Value> = components
        .iter()
        .map(|c| (c.to_string(), json!({ "desired": "running" })))
        .collect();
    let dir = p.work().join("instances");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("app-0.json"),
        json!({ "instance": "app-0", "stack": "app", "slot": 0, "components": comps, "updatedUtc": NOW })
            .to_string(),
    )
    .unwrap();
}

#[test]
fn a_verb_is_offered_only_where_a_script_exists_and_stack_wide_ones_say_what_they_skip() {
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      api: { driver: script, up: [node, api-up.js], stop: [node, api-stop.js] }\n      web: { driver: script, up: [node, web-up.js], depends_on: [api] }\n",
        &[],
    );
    wanted_running(&p, &["api", "web"]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    assert_eq!(action_ids(row(&e, "api")), ["up", "stop"]);
    assert_eq!(action_ids(row(&e, "web")), ["up"]);
    let stop = action(&stack_actions(&e), "stop").unwrap().clone();
    assert_eq!(
        stop["confirm"],
        "Run api (stop script). Web has no stop script, so stop leaves it as it is.\n\nRun the `stop` script for api: node api-stop.js."
    );
    // No start script anywhere: no stack-wide start.
    assert!(action(&stack_actions(&e), "start").is_none());
    // A verb that is not offered is refused.
    let (status, body) = act(&src, "stop", Some("web"), &json!({}));
    assert_eq!((status, body["kind"].as_str()), (409, Some("not-offered")));
    // Without a status script the state is what the stack asked for.
    assert_eq!(row(&e, "web")["state"], "up");
    assert_eq!(badge_texts(row(&e, "web")), ["no status script"]);
    // The stack-wide stop runs api's script only, and records web stays.
    let (status, reply) = act(&src, "stop", None, &stop["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let lines = f.cli.with(|c| c.lines());
    assert_eq!(lines, ["node api-stop.js"]);
    let e = env(&src);
    assert_eq!(row(&e, "api")["state"], "stopped-idle");
    assert_eq!(row(&e, "web")["state"], "up");
}

// --------------------------------------------------------------- confirms

const PLANNED: &str = "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        up: [node, up.js]\n        stop: [node, stop.js]\n        plan: [node, plan.js]\n";

#[test]
fn confirms_come_from_the_plan_script_and_a_changed_preview_refuses_the_call() {
    let p = Project::with(PLANNED, &[]);
    let cli = FakeCli::new()
        .on(&["node", "plan.js", "up"], "api: start\nweb: start\n")
        .failing(&["node", "plan.js", "stop"], 2, "boom");
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    let up = action(&row(&e, "app")["actions"], "up").unwrap().clone();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for app. Its plan is still being worked out."
    );
    // Offered, but disabled until the preview is in: nobody confirms a
    // script run blind, and the source refuses the call meanwhile.
    assert_eq!(up["disabled"], "Its plan is still being worked out");
    let stack_up = action(&stack_actions(&e), "up").unwrap().clone();
    assert_eq!(stack_up["disabled"], "Its plan is still being worked out");
    let (status, body) = act(&src, "up", Some("app"), &up["params"]);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("plan-pending")),
        "{body}"
    );
    let (status, body) = act(&src, "up", None, &stack_up["params"]);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("plan-pending")),
        "{body}"
    );
    // Once the preview lands the next look enables it.
    let e = look(&src, &f.world);
    assert!(action(&stack_actions(&e), "up")
        .unwrap()
        .get("disabled")
        .is_none());
    let app = row(&e, "app");
    let up = action(&app["actions"], "up").unwrap().clone();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for app.\napi: start\nweb: start"
    );
    assert!(up.get("disabled").is_none(), "{up}");
    let stop = action(&app["actions"], "stop").unwrap();
    assert_eq!(
        stop["confirm"],
        "Run the `stop` script for app. Its plan script failed (it exited with code 2), so what it will do is not shown."
    );
    assert!(
        stop.get("disabled").is_none(),
        "a failed plan is shown, not waited for"
    );
    // The confirm shown is what the fingerprint stands for: a preview that
    // changed since refuses the call with the new one.
    let stale = up["params"].clone();
    f.cli.0.lock().unwrap().add(
        &["node", "plan.js", "up"],
        CliOutput::success("api: start\nweb: start\ndb: migrate\n"),
    );
    src.background.invalidate_facts();
    src.background.refresh_once(&f.world);
    let (status, body) = act(&src, "up", Some("app"), &stale);
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    assert!(
        body["message"].as_str().unwrap().contains("db: migrate"),
        "{body}"
    );
    let up = action(&row(&env(&src), "app")["actions"], "up")
        .unwrap()
        .clone();
    let (status, reply) = act(&src, "up", Some("app"), &up["params"]);
    assert_eq!(status, 202, "{reply}");

    // Without a plan script: the resolved argv.
    let p = Project::with(ONE, &[]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    let up = action(&row(&e, "app")["actions"], "up").unwrap();
    assert_eq!(up["confirm"], "Run the `up` script for app: node up.js.");
    assert!(
        up.get("disabled").is_none(),
        "without a plan script nothing is pending"
    );
}

#[test]
fn a_script_component_sits_beside_a_compose_one_in_dependency_order() {
    let p = Project::with(
        "stacks:\n  app:\n    groups:\n      data: { engine: docker }\n    components:\n      db:\n        group: data\n        driver: compose\n        file: stack/data.yml\n        modes: { prebuilt: { image: postgres, tag: \"16\" } }\n      app:\n        driver: script\n        depends_on: [db]\n        up: [node, up.js]\n        down: [node, down.js]\n        env: { DB: \"{host}:{port:db}\" }\n",
        &[(
            "stack/data.yml",
            "services:\n  db: { image: x, expose: [\"5432\"] }\n",
        )],
    );
    let cli = docker_facts(FakeCli::new()).on(
        &["docker", "manifest", "inspect"],
        r#"{"Descriptor":{"digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"}}"#,
    );
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    let up = action(&stack_actions(&e), "up").unwrap().clone();
    assert!(
        up["confirm"]
            .as_str()
            .unwrap()
            .starts_with("Pull db (postgres:16); start db; run app (up script).\n\nRun the `up` script for app: node up.js."),
        "{}",
        up["confirm"]
    );
    let (status, reply) = act(&src, "up", None, &up["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let db = pane.find("▶ db: up db").expect(&pane);
    let app = pane.find("▶ app: up script").expect(&pane);
    assert!(db < app, "{pane}");
    let call = f.cli.with(|c| c.calls_matching(&["node", "up.js"]));
    assert_eq!(call[0].env["DB"], "localhost:14000");
}

#[test]
fn a_script_that_needs_a_bound_item_is_offered_disabled_with_why_and_refused_before_it_runs() {
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        repos: [../api]\n        up: [node, up.js, \"{worktree:api}\"]\n      web:\n        driver: script\n        up: [node, web.js]\n",
        &[],
    );
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    let up = action(&row(&e, "app")["actions"], "up").unwrap().clone();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for app. It cannot run now: the `up` script of `app` cannot run: `{worktree:api}` needs a bound work item; activate one in this slot first."
    );
    // The button says what to do first, rather than a confirm that fails.
    assert_eq!(
        up["disabled"],
        "Bind a work item first: the script needs it for `{worktree:api}`"
    );
    // A verb whose scripts all resolve stays enabled.
    let web_up = action(&row(&e, "web")["actions"], "up").unwrap().clone();
    assert!(web_up.get("disabled").is_none(), "{web_up}");
    // The stack-wide bring up runs app's script too: disabled the same way.
    let stack_up = action(&stack_actions(&e), "up").unwrap().clone();
    assert_eq!(stack_up["disabled"], up["disabled"]);
    for (key, a) in [(Some("app"), &up), (None, &stack_up)] {
        let (status, body) = act(&src, "up", key, &a["params"]);
        assert_eq!(
            (status, body["kind"].as_str()),
            (409, Some("cannot-run")),
            "{body}"
        );
        assert_eq!(body["message"], a["disabled"], "{body}");
    }
    assert!(f.cli.with(|c| c.lines()).is_empty());
}

// ------------------------------------------------------------------- jobs

#[test]
fn a_script_job_fails_on_its_exit_code_and_a_timeout_stops_its_whole_tree() {
    if !has_node() {
        eprintln!("skipping the real script jobs: node is not on PATH");
        return;
    }
    let slow = r#""use strict";
const { spawn } = require("child_process");
const path = require("path");
const late = path.join(process.cwd(), "late.txt");
// A child that would leave a file behind if it outlived the timeout.
spawn(process.execPath, ["-e", `setTimeout(() => require("fs").writeFileSync(${JSON.stringify(late)}, "x"), 4000)`], { stdio: "inherit" });
console.log("working...");
setTimeout(() => {}, 60000);
"#;
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      bad:\n        driver: script\n        up: [node, -e, \"process.exit(3)\"]\n      slow:\n        driver: script\n        up: [node, scripts/slow.js]\n        timeouts: { up: 2s }\n",
        &[("scripts/slow.js", slow)],
    );
    let mut f = fakes(FakeCli::new(), FakeHttp::default());
    f.world.steps = Arc::new(ChildSteps);
    let src = source(f.world.clone(), &p.dir);

    let e = env(&src);
    let up = action(&row(&e, "bad")["actions"], "up").unwrap().clone();
    let (status, reply) = act(&src, "up", Some("bad"), &up["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(pane.contains("the up script exited with code 3"), "{pane}");
    let runs = ScriptRuns::load(&p.work(), "app-0");
    assert_eq!(runs.components["bad"].exit_code, Some(3));
    assert!(!runs.components["bad"].ok);
    let bad = row(&env(&src), "bad").clone();
    assert_eq!(bad["state"], "stopped-problem", "{bad}");
    assert_eq!(bad["exitCode"], 3);
    assert!(bad["hint"]
        .as_str()
        .unwrap()
        .contains("its `up` script failed (exit 3)"));

    let up = action(&row(&env(&src), "slow")["actions"], "up")
        .unwrap()
        .clone();
    let (status, reply) = act(&src, "up", Some("slow"), &up["params"]);
    assert_eq!(status, 202, "{reply}");
    let t = Instant::now();
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert!(pane.contains("working..."), "{pane}");
    assert!(
        pane.contains("timed out after 2 s; it and every process it started was stopped"),
        "{pane}"
    );
    let runs = ScriptRuns::load(&p.work(), "app-0");
    assert!(runs.components["slow"]
        .error
        .as_deref()
        .unwrap()
        .contains("timed out"));
    // The child would have written its file 4 s after it started.
    while t.elapsed() < Duration::from_secs(6) {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !p.dir.join("late.txt").exists(),
        "the script's child outlived the timeout"
    );
}

// ----------------------------------------------------------- saved states

fn work_row(src: &Source) -> Value {
    src.work.invalidate();
    src.refresh_work();
    get(src, "/api/work").1["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == "ABC-1")
        .cloned()
        .expect("the item row")
}

fn work_act(src: &Source, id: &str, params: &Value) -> (u16, Value) {
    post(
        src,
        "/api/actions",
        &json!({ "id": id, "target": { "view": "work", "key": "ABC-1" }, "params": params, "confirmed": true }),
        true,
    )
}

#[test]
fn a_saved_state_resumes_a_script_component_by_running_up_again() {
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        up: [node, up.js]\n",
        &[],
    );
    let item_dir: PathBuf = p.work().join("ABC-1");
    fs::create_dir_all(&item_dir).unwrap();
    ItemRecord::new(
        ItemRef {
            source: "tracker".into(),
            key: "ABC-1".into(),
        },
        "ABC-1",
        NOW,
    )
    .save(&item_dir)
    .unwrap();
    let mut a = ActiveRecord::load(&p.work()).unwrap();
    a.set(0, "ABC-1", "app-0", NOW);
    a.save(&p.work()).unwrap();
    wanted_running(&p, &["app"]);
    let mut f = fakes(FakeCli::new(), FakeHttp::default());
    f.world.clock = ManualClock::at(NOW);
    let src = source(f.world.clone(), &p.dir);

    let r = work_row(&src);
    let save = action(&r["actions"], "save-state")
        .expect("Save state on the active item")
        .clone();
    assert!(
        save["confirm"]
            .as_str()
            .unwrap()
            .contains("app script (not restorable beyond re-running `up`)"),
        "{}",
        save["confirm"]
    );
    let (status, reply) = work_act(&src, "save-state", &save["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let state: Value = serde_json::from_str(
        &fs::read_to_string(item_dir.join("states/20261115T120000Z.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["components"]["app"]["script"], true);
    assert_eq!(state["components"]["app"]["desired"], "running");
    assert!(state["components"]["app"].get("image").is_none());

    let r = work_row(&src);
    let sec = r["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "saved-states")
        .expect("the saved states")
        .clone();
    let resume = action(&sec["actions"], "resume:20261115T120000Z")
        .expect("resume offered")
        .clone();
    let confirm = resume["confirm"].as_str().unwrap();
    for part in [
        "Changes: app: its `up` script runs again.",
        "app not restorable beyond re-running `up`: its `up` script runs again, with the environment of ABC-1's worktrees.",
        "Run app (up script).",
    ] {
        assert!(confirm.contains(part), "`{part}` not in: {confirm}");
    }
    let (status, reply) = work_act(&src, "resume:20261115T120000Z", &resume["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let calls = f.cli.with(|c| c.calls_matching(&["node", "up.js"]));
    assert_eq!(calls.len(), 1, "{pane}");
    assert_eq!(calls[0].env["CHAPPA_STACK_ITEM"], "ABC-1");
}

// ------------------------------------------------- groups and per-row verbs

const GROUPED: &str = "stacks:\n  app:\n    components:\n      core:\n        driver: script\n        up: [node, up.js]\n        stop: [node, stop.js]\n        restart: [node, restart.js, \"{row}\"]\n        status: [node, status.js]\n      db:\n        driver: script\n        group: data\n        up: [node, db-up.js]\n        status: [node, db-status.js]\n";

const CORE_STATUS: &str = r#"{"groups": [{"id": "apis", "label": "APIs"}, {"id": "web", "label": "Web"}],
  "components": [
    {"name": "api", "group": "apis", "state": "up", "detail": "app-0-api (podman)"},
    {"name": "auth", "group": "apis", "state": "up"},
    {"name": "portal", "group": "web", "state": "up"},
    {"name": "tools", "state": "up"}]}"#;

const DB_STATUS: &str =
    r#"{"components": [{"name": "main", "state": "up"}, {"name": "replica", "state": "up"}]}"#;

fn group_rows(env: &Value) -> Vec<String> {
    env["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let ids: Vec<String> = g["actions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| x["id"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "{}={} [{}]",
                g["id"].as_str().unwrap(),
                g["label"].as_str().unwrap(),
                ids.join(",")
            )
        })
        .collect()
}

#[test]
fn reported_groups_pass_through_in_order_and_verbs_are_drawn_once() {
    let p = Project::with(GROUPED, &[]);
    wanted_running(&p, &["core", "db"]);
    let cli = FakeCli::new()
        .on(&["node", "status.js"], CORE_STATUS)
        .on(&["node", "db-status.js"], DB_STATUS);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    env(&src);
    let e = look(&src, &f.world);
    // The status's groups in its order; a row with no group goes to one
    // named after the script component; db's rows to db's own group, which
    // they fill alone, so db's verb sits on that group's header.
    assert_eq!(
        group_rows(&e),
        [
            "apis=APIs []",
            "web=Web []",
            "core=core []",
            "data=Data [db:up]"
        ],
        "{}",
        e["components"]
    );
    let group_of = |id: &str| row(&e, id)["group"].as_str().unwrap().to_string();
    assert_eq!(group_of("core/api"), "apis");
    assert_eq!(group_of("core/portal"), "web");
    assert_eq!(group_of("core/tools"), "core");
    assert_eq!(group_of("db/main"), "data");
    // core spans three groups: its verbs once, with the stack's own (which
    // stay, since core is not the only component); none on its rows but
    // the `{row}` one.
    let top: Vec<String> = stack_actions(&e)
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect();
    assert!(top.contains(&"up".to_string()), "{top:?}");
    assert_eq!(
        top.iter()
            .filter(|a| a.starts_with("core:"))
            .collect::<Vec<_>>(),
        ["core:up", "core:stop"]
    );
    for id in ["core/api", "core/auth", "core/portal", "core/tools"] {
        assert_eq!(action_ids(row(&e, id)), ["restart"], "{id}");
    }
    assert!(action_ids(row(&e, "db/main")).is_empty());
    // The detail is the row's second line; who reports the rows is said
    // once for the view.
    assert_eq!(row(&e, "core/api")["hint"], "app-0-api (podman)");
    assert_eq!(
        meta_rows(&e),
        [
            "Scripts=core reports 4 components; their actions run core's scripts",
            "Scripts=db reports 2 components; their actions run db's scripts"
        ]
    );

    // A verb drawn once comes back by its `<component>:<verb>` id, from the
    // stack's actions or from a group's header (sent with the group's id).
    let stop = action(&stack_actions(&e), "core:stop").unwrap().clone();
    assert_eq!(
        stop["confirm"],
        "Run the `stop` script for core: node stop.js."
    );
    let (status, reply) = act(&src, "core:stop", None, &stop["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let e = look(&src, &f.world);
    let db_up = e["groups"][3]["actions"][0].clone();
    let (status, reply) = act(&src, "db:up", Some("data"), &db_up["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(
        f.cli
            .with(|c| c.calls_matching(&["node", "db-up.js"]))
            .len(),
        1
    );
}

#[test]
fn a_row_verb_runs_with_the_reported_name_and_refuses_a_name_not_reported() {
    let p = Project::with(GROUPED, &[]);
    wanted_running(&p, &["core", "db"]);
    let cli = FakeCli::new()
        .on(&["node", "status.js"], CORE_STATUS)
        .on(&["node", "db-status.js"], DB_STATUS);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    // Before the status answered, no row is known: no per-row verb.
    let e = env(&src);
    assert!(
        action_ids(row(&e, "core")).iter().all(|a| a != "restart"),
        "{}",
        row(&e, "core")
    );

    let e = look(&src, &f.world);
    let restart = action(&row(&e, "core/auth")["actions"], "restart")
        .unwrap()
        .clone();
    assert_eq!(
        restart["confirm"],
        "Run the `restart` script for auth (reported by core): node restart.js auth."
    );
    // A name the last status did not report, or the component itself, is
    // refused before anything is stored.
    for key in ["core/billing", "core"] {
        let (status, body) = act(&src, "restart", Some(key), &restart["params"]);
        assert_eq!(
            (status, body["kind"].as_str()),
            (409, Some("not-offered")),
            "{key}: {body}"
        );
    }
    let (status, reply) = act(&src, "restart", Some("core/auth"), &restart["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let calls = f.cli.with(|c| c.calls_matching(&["node", "restart.js"]));
    assert_eq!(calls.len(), 1, "{pane}");
    assert_eq!(calls[0].argv(), ["node", "restart.js", "auth"]);
    // A verb on one row leaves what the stack wants of core as it was: the
    // other rows stay wanted running.
    let e = look(&src, &f.world);
    assert_eq!(row(&e, "core/api")["state"], "up");
}

// ------------------------------------------------------ picking what comes up

const PICKED: &str = "stacks:\n  app:\n    selections: { core-only: [core] }\n    components:\n      core:\n        driver: script\n        up: [node, up.js, \"{rows}\"]\n        status: [node, status.js]\n      db:\n        driver: script\n        group: data\n        up: [node, db-up.js]\n        status: [node, db-status.js]\n";

const CORE_PARTLY_UP: &str = r#"{"groups": [{"id": "apis", "label": "APIs"}, {"id": "web", "label": "Web"}],
  "components": [
    {"name": "api", "group": "apis", "state": "up"},
    {"name": "auth", "group": "apis", "state": "up"},
    {"name": "portal", "group": "web", "state": "not-deployed"}]}"#;

fn picked_source() -> (Project, Fakes, Arc<Source>) {
    let p = Project::with(PICKED, &[]);
    let cli = FakeCli::new()
        .on(&["node", "status.js"], CORE_PARTLY_UP)
        .on(&["node", "db-status.js"], DB_STATUS);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    // The first look asks for the status scripts' answers.
    env(&src);
    (p, f, src)
}

/// Run `id` with `extra` merged into its params; the reply.
fn act_with(
    src: &Source,
    id: &str,
    key: Option<&str>,
    action: &Value,
    extra: Value,
) -> (u16, Value) {
    let mut params = action["params"].clone();
    for (k, v) in extra.as_object().unwrap() {
        params[k] = v.clone();
    }
    act(src, id, key, &params)
}

fn ups(f: &Fakes) -> Vec<String> {
    f.cli
        .with(|c| c.lines())
        .into_iter()
        .filter(|l| l.contains("up.js"))
        .collect()
}

#[test]
fn bring_up_picks_a_named_selection_or_ticked_groups_and_refuses_what_the_definition_lacks() {
    let (p, f, src) = picked_source();
    let e = look(&src, &f.world);
    let up = action(&stack_actions(&e), "up").unwrap().clone();
    // The picker: the default, each named selection, or what is ticked.
    let inputs = &up["inputs"];
    let choices: Vec<&str> = inputs[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(choices, ["default", "named:core-only", "picked"]);
    assert_eq!(inputs[0]["default"], "default");
    let picks: Vec<&str> = inputs[1]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(picks, ["core", "db"]);

    // Refused before anything is stored: an unknown selection, an unknown
    // component or group, nothing ticked, a choice that is no choice.
    for extra in [
        json!({ "selection": "named:nope" }),
        json!({ "selection": "picked", "pick": ["ghost"] }),
        json!({ "selection": "picked", "pick": ["group:nowhere"] }),
        json!({ "selection": "picked", "pick": [] }),
        json!({ "selection": "everything" }),
    ] {
        let (status, body) = act_with(&src, "up", None, &up, extra.clone());
        assert_eq!(
            (status, body["kind"].as_str()),
            (409, Some("bad-selection")),
            "{extra}: {body}"
        );
    }
    assert!(ups(&f).is_empty());

    // A named selection brings up its members only.
    let (status, reply) = act_with(
        &src,
        "up",
        None,
        &up,
        json!({ "selection": "named:core-only" }),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(ups(&f), ["node up.js"]);

    // A ticked component brings up just that one.
    let e = look(&src, &f.world);
    let up = action(&stack_actions(&e), "up").unwrap().clone();
    let (status, reply) = act_with(
        &src,
        "up",
        None,
        &up,
        json!({ "selection": "picked", "pick": ["db"] }),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(ups(&f), ["node up.js", "node db-up.js"]);
}

#[test]
fn a_scripts_up_takes_the_picked_rows_through_rows_and_each_idle_row_comes_up_alone() {
    let (p, f, src) = picked_source();
    let e = look(&src, &f.world);
    // core spans three groups, so its Up is with the stack's, with a
    // picker of its reported rows and their groups.
    let up = action(&stack_actions(&e), "core:up").unwrap().clone();
    let picks: Vec<&str> = up["inputs"][0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        picks,
        [
            "group:apis",
            "group:web",
            "core/api",
            "core/auth",
            "core/portal"
        ]
    );
    // Rows the last status did not report are refused.
    for rows in [
        json!(["core/ghost"]),
        json!(["group:nowhere"]),
        json!("core/api"),
    ] {
        let (status, body) = act_with(&src, "core:up", None, &up, json!({ "rows": rows }));
        assert_eq!(
            (status, body["kind"].as_str()),
            (409, Some("bad-selection")),
            "{rows}: {body}"
        );
    }
    // Picked rows: each name one argument, in the reported order.
    let (status, reply) = act_with(
        &src,
        "core:up",
        None,
        &up,
        json!({ "rows": ["group:web", "core/api"] }),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let calls = f.cli.with(|c| c.calls_matching(&["node", "up.js"]));
    assert_eq!(
        calls.last().unwrap().argv(),
        ["node", "up.js", "api", "portal"]
    );
    // Nothing picked: the whole stack, `{rows}` gone.
    let e = look(&src, &f.world);
    let up = action(&stack_actions(&e), "core:up").unwrap().clone();
    let (status, reply) = act(&src, "core:up", None, &up["params"]);
    assert_eq!(status, 202, "{reply}");
    run(&f.world, &p.dir, &reply);
    let calls = f.cli.with(|c| c.calls_matching(&["node", "up.js"]));
    assert_eq!(calls.last().unwrap().argv(), ["node", "up.js"]);

    // The row that is not running has its own Up; running rows do not.
    let e = look(&src, &f.world);
    assert_eq!(action_ids(row(&e, "core/portal")), ["up"]);
    assert!(action_ids(row(&e, "core/api")).is_empty());
    let one = action(&row(&e, "core/portal")["actions"], "up")
        .unwrap()
        .clone();
    assert_eq!(one["label"], "Up this");
    let (status, reply) = act(&src, "up", Some("core/portal"), &one["params"]);
    assert_eq!(status, 202, "{reply}");
    run(&f.world, &p.dir, &reply);
    let calls = f.cli.with(|c| c.calls_matching(&["node", "up.js"]));
    assert_eq!(calls.last().unwrap().argv(), ["node", "up.js", "portal"]);
}

// ------------------------------------------- every column, and per-row intent

const ROW_STOP: &str = "stacks:\n  app:\n    components:\n      core:\n        driver: script\n        up: [node, up.js, \"{rows}\"]\n        stop: [node, stop.js, \"{row}\"]\n        status: [node, status.js]\n";

const FULL_STATUS: &str = r#"{"components": [
    {"name": "api", "state": "up", "uptime": "2d 7h", "restarts": 2, "detail": "app-0-api (podman)",
     "source": {"mode": "source", "ref": "main", "commit": "9b10002fdeadbeef"},
     "probe": {"url": "http://localhost:14090/health", "rule": "health", "status": 503},
     "links": [{"label": "API", "url": "http://localhost:14090"}, {"label": "Docs", "url": "https://docs.example.test/api"}]},
    {"name": "auth", "state": "stopped-idle"},
    {"name": "portal", "state": "stopped-idle"}]}"#;

fn write_intent(p: &Project, desired: &str) {
    let dir = p.work().join("instances");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("app-0.json"),
        json!({ "instance": "app-0", "stack": "app", "slot": 0, "components": { "core": { "desired": desired } }, "updatedUtc": NOW })
            .to_string(),
    )
    .unwrap();
}

fn row_source(cli: FakeCli) -> (Project, Fakes, Arc<Source>) {
    let p = Project::with(ROW_STOP, &[]);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    env(&src);
    (p, f, src)
}

#[test]
fn a_status_script_fills_every_column_with_the_same_checks_as_any_component() {
    let (p, f, src) = row_source(FakeCli::new().on(&["node", "status.js"], FULL_STATUS));
    write_intent(&p, "running");
    let e = look(&src, &f.world);
    let api = row(&e, "core/api");
    assert_eq!(api["uptime"], "2d 7h");
    assert_eq!(api["restarts"], 2);
    assert_eq!(api["hint"], "app-0-api (podman)");
    assert_eq!(
        api["source"],
        json!({ "mode": "source", "ref": "main", "commit": "9b10002fdeadbeef" })
    );
    // The probe is judged by its rule here: a 503 fails `health`.
    assert_eq!(
        api["probe"],
        json!({ "url": "http://localhost:14090/health", "rule": "health", "up": false, "status": 503 })
    );
    // Links still only to the stack's own hosts; the rest is counted once.
    assert_eq!(
        api["links"],
        json!([{ "label": "API", "url": "http://localhost:14090" }])
    );
    let notices = e["freshness"]["notices"].as_array().unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n == "1 reported link to a host other than the stack's own was left out"),
        "{notices:?}"
    );
    // Not running: no uptime shown even if one was reported.
    assert!(row(&e, "core/auth")["uptime"].is_null());
}

#[test]
fn a_row_stopped_on_request_is_gray_until_the_whole_stack_is_brought_up_again() {
    let (p, f, src) = row_source(FakeCli::new().on(&["node", "status.js"], FULL_STATUS));
    write_intent(&p, "running");
    let e = look(&src, &f.world);
    // Wanted running and not running: red.
    assert_eq!(row(&e, "core/auth")["state"], "stopped-problem");
    // Stop on the auth row records that auth was asked to stop.
    let stop = action(&row(&e, "core/auth")["actions"], "stop")
        .unwrap()
        .clone();
    let (status, reply) = act(&src, "stop", Some("core/auth"), &stop["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let e = look(&src, &f.world);
    assert_eq!(row(&e, "core/auth")["state"], "stopped-idle");
    assert_eq!(
        row(&e, "core/portal")["state"],
        "stopped-problem",
        "only the row asked for"
    );
    let intent: Value =
        serde_json::from_str(&fs::read_to_string(p.work().join("instances/app-0.json")).unwrap())
            .unwrap();
    assert_eq!(intent["components"]["core"]["desired"], "running");
    assert_eq!(
        intent["components"]["core"]["rows"],
        json!({ "auth": "stopped" })
    );
    // A later Up of the whole stack clears it: auth is wanted running again.
    let up = action(&stack_actions(&e), "core:up").unwrap().clone();
    let (status, reply) = act(&src, "core:up", None, &up["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let e = look(&src, &f.world);
    assert_eq!(row(&e, "core/auth")["state"], "stopped-problem");
    let intent: Value =
        serde_json::from_str(&fs::read_to_string(p.work().join("instances/app-0.json")).unwrap())
            .unwrap();
    assert!(
        intent["components"]["core"].get("rows").is_none(),
        "{intent}"
    );
}

#[test]
fn a_row_brought_up_on_its_own_is_wanted_running_while_the_rest_stays_stopped() {
    let (p, f, src) = row_source(FakeCli::new().on(&["node", "status.js"], FULL_STATUS));
    write_intent(&p, "stopped");
    let e = look(&src, &f.world);
    assert_eq!(row(&e, "core/auth")["state"], "stopped-idle");
    let one = action(&row(&e, "core/auth")["actions"], "up")
        .unwrap()
        .clone();
    let (status, reply) = act(&src, "up", Some("core/auth"), &one["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f.world, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    // The status still says auth is stopped: now that is a problem for
    // auth only; portal stays as the stack left it.
    let e = look(&src, &f.world);
    assert_eq!(row(&e, "core/auth")["state"], "stopped-problem");
    assert_eq!(row(&e, "core/portal")["state"], "stopped-idle");
}
