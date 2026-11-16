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

fn stack_actions(env: &Value) -> Value {
    env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "stack")
        .unwrap()["actions"]
        .clone()
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

    // status: both services, not deployed, in app's group.
    let e = look(&src, &world);
    assert_eq!(ids(&e), ["app/api", "app/web"]);
    for id in ["app/api", "app/web"] {
        assert_eq!(row(&e, id)["state"], "not-deployed", "{id}");
        assert!(row(&e, id)["hint"]
            .as_str()
            .unwrap()
            .contains("its actions run App's scripts"));
    }
    let facts = e["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "script-app")
        .expect("the status script's facts");
    assert_eq!(facts["rows"][0]["value"], "scripted-0");

    // up, from a reported row: the confirm is the plan script's output.
    let api = row(&e, "app/api");
    assert_eq!(
        action_ids(api),
        ["up", "start", "stop", "down"],
        "restart and rebuild have no script"
    );
    let up = action(&api["actions"], "up").unwrap();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for App.\napi: start on port 15000\nweb: start on port 15001"
    );
    let (status, reply) = act(&src, "up", Some("app/api"), &up["params"]);
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

    // stop, stack-wide: the script's own confirm under the plan line.
    let stop = action(&stack_actions(&e), "stop").unwrap().clone();
    let confirm = stop["confirm"].as_str().unwrap();
    assert!(
        confirm.starts_with(
            "Run app (stop script).\n\nRun the `stop` script for App.\nweb: stop\napi: stop"
        ),
        "{confirm}"
    );
    let (status, reply) = act(&src, "stop", None, &stop["params"]);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&world, &dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    src.background.invalidate_facts();
    let e = look(&src, &world);
    assert_eq!(row(&e, "app/api")["state"], "stopped-idle");
    assert_eq!(row(&e, "app/web")["state"], "stopped-idle");

    // A service that stops while the stack wants it running is red.
    let start = action(&stack_actions(&e), "start").map(|a| a["id"].clone());
    assert!(start.is_none(), "nothing is wanted running: {start:?}");

    // down: not deployed again, the pretend state removed.
    let down = action(&stack_actions(&e), "down").unwrap().clone();
    let (status, reply) = act(&src, "down", None, &down["params"]);
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
    f.world.cli = Arc::new(SlowCli(Duration::from_secs(3)));
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
        "Run the `up` script for app. Its plan is still being worked out; look again in a moment to see what it will do."
    );
    let e = look(&src, &f.world);
    let app = row(&e, "app");
    let up = action(&app["actions"], "up").unwrap().clone();
    assert_eq!(
        up["confirm"],
        "Run the `up` script for app.\napi: start\nweb: start"
    );
    assert_eq!(
        action(&app["actions"], "stop").unwrap()["confirm"],
        "Run the `stop` script for app. Its plan script failed (it exited with code 2), so what it will do is not shown."
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
    assert_eq!(
        action(&row(&e, "app")["actions"], "up").unwrap()["confirm"],
        "Run the `up` script for app: node up.js."
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
fn a_script_that_needs_a_bound_item_says_so_and_is_refused_before_it_runs() {
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        repos: [../api]\n        up: [node, up.js, \"{worktree:api}\"]\n",
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
    let (status, body) = act(&src, "up", Some("app"), &up["params"]);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("script")),
        "{body}"
    );
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
