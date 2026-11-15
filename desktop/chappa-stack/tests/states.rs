//! Saved stack states through the source, with a scripted engine: Save
//! state on the item's overlay, the automatic save before down, a mode
//! switch and a rebind (and how a failed one stops the job, and the
//! "without saving" twin), resume (the differences in its confirm, images
//! by digest, a missing snapshot blocking only its component, a recipe
//! replayed in order with its two data-safety checks), the snapshot hooks
//! and the host copy, the volume guard on `down-volumes`, pruning, job
//! retention, the `sensitive` flag, and no hook output anywhere it could
//! be kept.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chappa_stack::records::{self, JobRecord, JobStatus, JobWork};
use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_FAILED};
use chappa_stack::world::{CliSteps, Locked, ManualClock};
use common::*;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::definition::{JobsDef, SnapshotLocation};
use stack_core::intent::ActiveRecord;
use stack_core::probe::FakeHttp;
use stack_core::states::{list_states, SaveReason, SavedState};
use stack_core::workspace::{ItemRecord, ItemRef};

const NOW: &str = "2026-11-14T12:00:00Z";
const PROJECT: &str = "app-0-data";
const CONTAINER: &str = "app-0-data-db-1";
const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// What the snapshot hook prints: it must never be kept anywhere.
const HOOK_OUTPUT: &str = "dumped row person@customers.invalid 4111 1111 1111 1111";

const RULES: &str = r#"rules:
  - id: email-real-domain
    pattern: '(?i)[^@\s]+@[^@\s]+\.[a-z]{2,}'
    allow: ['(?i)@example\.(com|org|net)$']
"#;
const MARKER_OK: &str = r#"{"scrubbed":true,"version":"2026-11"}"#;

fn clean_sample() -> String {
    json!([{ "table": "customers", "column": "email", "values": ["user1@example.com"] }])
        .to_string()
}

fn dirty_sample() -> String {
    json!([{ "table": "customers", "column": "email", "values": ["person@customers.invalid"] }])
        .to_string()
}

/// A stack whose `db` keeps its data in a volume and snapshots it with
/// hooks (`location` as given), and an `api` beside it.
fn yml(location: &str, extra_work: &str, db_check: bool) -> String {
    let check = if db_check {
        "        dataSafety:\n          marker: [docker, exec, \"{container}\", /scripts/marker.sh]\n          sample: [docker, exec, \"{container}\", /scripts/sample.sh]\n"
    } else {
        ""
    };
    format!(
        r#"name: states
stacks:
  app:
    hostname: dev.example.test
    groups:
      data: {{ engine: docker }}
    work: {{ keepStates: 20{extra_work} }}
    dataSafety: {{ rules: stack/rules.yml, onFinding: block-agents, unscanned: warn }}
    components:
      db:
        group: data
        driver: compose
        file: stack/data.yml
        service: db
        modes: {{ prebuilt: {{ image: postgres, tag: "16" }} }}
        data: {{ strategy: snapshot }}
        persistence:
          volume: db-data
          location: {location}
          snapshot:
            create: [docker, exec, "{{container}}", /scripts/snap.sh, "{{snapshot}}"]
            restore: [docker, exec, "{{container}}", /scripts/restore.sh, "{{snapshot}}"]
            exists: [docker, exec, "{{container}}", test, -e, "/snapshots/{{snapshot}}"]
            delete: [docker, exec, "{{container}}", rm, "/snapshots/{{snapshot}}"]
            path: "/snapshots/{{snapshot}}"
            hint: "pg_dump custom"
{check}      api:
        group: data
        driver: compose
        file: stack/data.yml
        service: api
        modes: {{ prebuilt: {{ image: registry.example/api, tag: "1" }} }}
        depends_on: [db]
"#
    )
}

const COMPOSE: &str = "services:\n  db: { image: x, expose: [\"5432\"], volumes: [\"db-data:/var/lib/data\"] }\n  api: { image: x, expose: [\"8080\"] }\nvolumes:\n  db-data:\n";

fn project(text: &str) -> Project {
    Project::with(
        text,
        &[
            ("stack/data.yml", COMPOSE),
            ("stack/rules.yml", RULES),
            ("stack/scrub.sh", "#!/bin/sh\necho scrubbing\n"),
        ],
    )
}

/// The instance as if it had been brought up: db and api prebuilt from
/// `db` / `api` digests, wanted running, on channel `main`.
fn deploy(p: &Project, db: &str, api: &str) {
    let inst = p.work().join("instances");
    fs::create_dir_all(inst.join("app-0")).unwrap();
    fs::write(
        inst.join("app-0/provenance.json"),
        json!({ "components": {
            "db": { "mode": "prebuilt", "buildId": "16", "image": db },
            "api": { "mode": "prebuilt", "buildId": "1", "image": api },
        } })
        .to_string(),
    )
    .unwrap();
    fs::write(
        inst.join("app-0.json"),
        json!({ "instance": "app-0", "stack": "app", "slot": 0, "item": "ABC-1",
                "components": {
                    "db": { "desired": "running", "mode": "prebuilt", "channel": "main" },
                    "api": { "desired": "running", "mode": "prebuilt", "channel": "main" } },
                "updatedUtc": NOW })
        .to_string(),
    )
    .unwrap();
}

/// Item `key` with a workspace and no repos, active in slot 0 when asked.
fn item(p: &Project, key: &str, active: bool) -> PathBuf {
    let dir = p.work().join(key);
    fs::create_dir_all(&dir).unwrap();
    ItemRecord::new(
        ItemRef {
            source: "tracker".into(),
            key: key.into(),
        },
        key,
        NOW,
    )
    .save(&dir)
    .unwrap();
    if active {
        let mut a = ActiveRecord::load(&p.work()).unwrap();
        a.set(0, key, "app-0", NOW);
        a.save(&p.work()).unwrap();
    }
    dir
}

/// db and api running; the snapshot hooks print [`HOOK_OUTPUT`]; db's
/// data-safety hooks answer with `sample`.
fn running(sample: &str) -> FakeCli {
    with_docker_project(
        docker_facts(FakeCli::new()),
        PROJECT,
        &[Ctr::running("db"), Ctr::running("api")],
    )
    .on(&["exec", CONTAINER, "/scripts/snap.sh"], HOOK_OUTPUT)
    .on(&["exec", CONTAINER, "/scripts/marker.sh"], MARKER_OK)
    .on(&["exec", CONTAINER, "/scripts/sample.sh"], sample)
}

fn world(cli: FakeCli) -> (Fakes, Arc<ManualClock>) {
    let mut f = fakes(cli, FakeHttp::default());
    let clock = ManualClock::at(NOW);
    f.world.clock = clock.clone();
    (f, clock)
}

fn work(src: &chappa_stack::server::Source) -> Value {
    get(src, "/api/work").1
}

fn row(src: &chappa_stack::server::Source, key: &str) -> Value {
    work(src)["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == key)
        .cloned()
        .unwrap_or_else(|| panic!("no row {key}"))
}

fn find<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"] == id)
}

fn section<'a>(row: &'a Value, id: &str) -> Option<&'a Value> {
    row["sections"].as_array()?.iter().find(|s| s["id"] == id)
}

fn act(
    src: &chappa_stack::server::Source,
    view: &str,
    id: &str,
    key: Option<&str>,
    params: Value,
) -> (u16, Value) {
    post(
        src,
        "/api/actions",
        &json!({ "id": id, "target": { "view": view, "key": key }, "params": params, "confirmed": true }),
        true,
    )
}

fn run(f: &Fakes, dir: &Path, reply: &Value) -> (i32, String) {
    let id = reply["jobId"]
        .as_str()
        .unwrap_or_else(|| panic!("no job: {reply}"));
    let mut pane = Vec::new();
    let code = run_job(&f.world, dir, id, &mut pane);
    (code, String::from_utf8(pane).unwrap())
}

fn stack_action(src: &chappa_stack::server::Source, id: &str) -> Option<Value> {
    let env = get(src, "/api/environment").1;
    let stack = env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "stack")
        .unwrap()
        .clone();
    find(&stack["actions"], id).cloned()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn save(src: &chappa_stack::server::Source, f: &Fakes, p: &Project) -> String {
    let save = find(&row(src, "ABC-1")["actions"], "save-state")
        .expect("Save state is offered on the active item")
        .clone();
    let (status, reply) = act(
        src,
        "work",
        "save-state",
        Some("ABC-1"),
        save["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    pane
}

// ------------------------------------------------------------------ save

#[test]
fn a_saved_state_records_the_stack_and_where_its_data_is_never_the_data() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);

    let r = row(&src, "ABC-1");
    let save_action = find(&r["actions"], "save-state").unwrap();
    assert_eq!(save_action["mutates"], true, "it runs a snapshot hook");
    assert_eq!(
        save_action["confirm"],
        "Save the stack state of ABC-1 in slot 0: db prebuilt 16 @sha256:aaaaaaaaaaaa, api prebuilt 1 @sha256:aaaaaaaaaaaa. Snapshots db (pg_dump custom) into its volume db-data with its create hook."
    );
    assert!(section(&r, "saved-states").is_none(), "no states yet");
    // Unconfirmed, a mutating save is refused.
    let (status, body) = post(
        &src,
        "/api/actions",
        &json!({ "id": "save-state", "target": { "view": "work", "key": "ABC-1" }, "params": {}, "confirmed": false }),
        true,
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("confirmation-required"))
    );

    let pane = save(&src, &f, &p);
    assert!(
        pane.contains("▶ db: snapshot ABC-1-db-20261114T120000Z"),
        "{pane}"
    );
    let creates = f.cli.with(|c| {
        c.calls_matching(&[
            "docker",
            "exec",
            CONTAINER,
            "/scripts/snap.sh",
            "ABC-1-db-20261114T120000Z",
        ])
    });
    assert_eq!(creates.len(), 1);

    let states = list_states(&dir);
    assert_eq!(states.len(), 1);
    let s = &states[0];
    assert_eq!(s.id, "20261114T120000Z");
    assert_eq!(s.reason, SaveReason::Manual);
    assert_eq!(
        (s.stack.as_str(), s.slot, s.instance.as_str()),
        ("app", 0, "app-0")
    );
    assert_eq!(
        (s.item.source.as_str(), s.item.key.as_str()),
        ("tracker", "ABC-1")
    );
    assert_eq!(s.chappa_stack, env!("CARGO_PKG_VERSION"));
    assert_eq!(s.channel.as_deref(), Some("main"));
    let db = &s.components["db"];
    assert_eq!(db.image.as_deref(), Some(DIGEST_A));
    assert_eq!(db.build_id.as_deref(), Some("16"));
    assert_eq!(s.overrides["api"].channel.as_deref(), Some("main"));
    let snap = s.data["db"].snapshot.as_ref().unwrap();
    assert_eq!(snap.location, SnapshotLocation::InVolume);
    assert_eq!(snap.volume.as_deref(), Some("app-0-data_db-data"));
    assert_eq!(
        snap.where_,
        "volume app-0-data_db-data at /snapshots/ABC-1-db-20261114T120000Z"
    );
    assert_eq!(snap.engine_hint.as_deref(), Some("pg_dump custom"));
    // Never checked yet: unknown counts as sensitive.
    assert!(s.sensitive);
    assert_eq!(s.sensitive_because, ["db: not checked yet"]);
    assert_eq!(s.jobs.len(), 1);
    assert!(records::is_job_id(&s.jobs[0]));
    // The file round-trips, and nothing a hook printed is anywhere.
    let path = SavedState::path(&dir, &s.id);
    let text = fs::read_to_string(&path).unwrap();
    let back: SavedState = serde_json::from_str(&text).unwrap();
    assert_eq!(&back, s);
    let mut all = Vec::new();
    files_under(&p.work(), &mut all);
    for file in all {
        let t = fs::read_to_string(&file).unwrap_or_default();
        for piece in ["dumped row", "customers.invalid", "4111"] {
            assert!(!t.contains(piece), "`{piece}` in {}", file.display());
        }
    }
    assert!(!pane.contains("dumped row"), "{pane}");

    let r = row(&src, "ABC-1");
    let sec = section(&r, "saved-states").expect("the Saved states section");
    assert_eq!(sec["rows"][0]["label"], "2026-11-14 12:00 UTC");
    assert_eq!(
        sec["rows"][0]["value"],
        "just now · saved by hand · 1 snapshot"
    );
    assert_eq!(sec["rows"][0]["badges"][0]["text"], "sensitive");
    assert!(r["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "1 saved state"));
    assert!(find(&sec["actions"], "resume:20261114T120000Z").is_some());
}

#[test]
fn sensitivity_follows_the_last_verdict_and_no_check_counts_as_sensitive() {
    for (sample, check, sensitive, because) in [
        (clean_sample(), true, false, vec![]),
        (
            dirty_sample(),
            true,
            true,
            vec!["db: last data-safety check not-clean"],
        ),
        (
            clean_sample(),
            false,
            true,
            vec!["db: no data-safety check"],
        ),
    ] {
        let p = project(&yml("in-volume", "", check));
        deploy(&p, DIGEST_A, DIGEST_A);
        let dir = item(&p, "ABC-1", true);
        let (f, _) = world(running(&sample));
        let src = source(f.world.clone(), &p.dir);
        if check {
            let (status, reply) = act(
                &src,
                "environment",
                "data-safety-check",
                Some("db"),
                json!(null),
            );
            assert_eq!(status, 202, "{reply}");
            run(&f, &p.dir, &reply);
        }
        save(&src, &f, &p);
        let s = &list_states(&dir)[0];
        assert_eq!(s.sensitive, sensitive, "{sample}");
        assert_eq!(s.sensitive_because, because);
        if check {
            assert_eq!(s.jobs.len(), 2, "the save and the check it relied on");
        }
    }
}

// --------------------------------------------------- automatic saves

#[test]
fn destructive_verbs_save_first_and_a_failed_save_stops_the_job() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);

    let down = stack_action(&src, "down").unwrap();
    assert!(
        down["confirm"].as_str().unwrap().starts_with(
            "Saves the stack state of ABC-1 first (if that fails, nothing else runs). Stop "
        ),
        "{down}"
    );
    let plain = stack_action(&src, "down-no-save").expect("the twin");
    assert!(plain["confirm"]
        .as_str()
        .unwrap()
        .ends_with(" Without saving the stack state of ABC-1 first."));
    assert_eq!(plain["label"], "Down without saving");

    // A save that fails stops the job before anything changes.
    f.cli.0.lock().unwrap().add(
        &["exec", CONTAINER, "/scripts/snap.sh"],
        CliOutput::failure(3, "disk full"),
    );
    f.cli.with(|c| c.clear_calls());
    let (status, reply) = act(&src, "environment", "down", None, down["params"].clone());
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(
        pane.contains("the stack state of ABC-1 could not be saved first (db: the create hook exited with code 3); nothing else was done"),
        "{pane}"
    );
    assert!(
        !pane.contains("disk full"),
        "hook output is never shown: {pane}"
    );
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["compose", "stop"]).is_empty()));
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["compose", "down"]).is_empty()));
    assert!(list_states(&dir).is_empty());

    // Without saving, it goes ahead.
    let (status, reply) = act(
        &src,
        "environment",
        "down-no-save",
        None,
        plain["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(!f.cli.with(|c| c
        .calls_matching(&["compose", "-p", PROJECT, "down"])
        .is_empty()));
    assert!(list_states(&dir).is_empty());
    let downs = f
        .cli
        .with(|c| c.calls_matching(&["compose", "-p", PROJECT, "down"]));
    assert!(
        !downs[0].args.iter().any(|a| a == "--volumes"),
        "down keeps volumes"
    );

    // With a working hook, down saves first (before-down) and then goes.
    deploy(&p, DIGEST_A, DIGEST_A);
    f.cli.0.lock().unwrap().add(
        &["exec", CONTAINER, "/scripts/snap.sh"],
        CliOutput::success(""),
    );
    let down = stack_action(&src, "down").unwrap();
    let (_, reply) = act(&src, "environment", "down", None, down["params"].clone());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let saved = pane.find("✓ saved state").expect("saved");
    let downed = pane.find("down app-0-data").expect("down ran");
    assert!(saved < downed, "{pane}");
    assert_eq!(list_states(&dir)[0].reason, SaveReason::BeforeDown);
}

#[test]
fn a_mode_switch_saves_first_while_an_item_is_bound() {
    let text = yml("in-volume", "", true).replace(
        "        modes: { prebuilt: { image: registry.example/api, tag: \"1\" } }\n",
        "        modes:\n          prebuilt: { image: registry.example/api, tag: \"1\" }\n          source: { repo: ../api }\n",
    );
    let p = project(&text);
    fs::write(p.dir.join("shared-checkouts-allow.txt"), "api\n").unwrap();
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let env = get(&src, "/api/environment").1;
    let api = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "api")
        .unwrap()
        .clone();
    let switch = find(&api["actions"], "mode-source").unwrap();
    assert!(switch["confirm"]
        .as_str()
        .unwrap()
        .starts_with("Saves the stack state of ABC-1 first"));
    assert!(find(&api["actions"], "mode-source-no-save").is_some());
    let (status, reply) = act(
        &src,
        "environment",
        "mode-source",
        Some("api"),
        switch["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    match JobRecord::load(&p.work(), reply["jobId"].as_str().unwrap())
        .unwrap()
        .unwrap()
        .work
    {
        JobWork::Plan { save: Some(s), .. } => {
            assert_eq!(
                (s.item.as_str(), s.reason),
                ("ABC-1", SaveReason::BeforeModeSwitch)
            )
        }
        other => panic!("{other:?}"),
    }
    let (_, pane) = run(&f, &p.dir, &reply);
    assert!(pane.contains("✓ saved state"), "{pane}");
    assert_eq!(list_states(&dir)[0].reason, SaveReason::BeforeModeSwitch);

    // With no item bound, nothing is saved and there is no twin.
    fs::remove_file(ActiveRecord::path(&p.work())).unwrap();
    deploy(&p, DIGEST_A, DIGEST_A);
    let env = get(&src, "/api/environment").1;
    let api = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "api")
        .unwrap()
        .clone();
    assert!(!find(&api["actions"], "mode-source").unwrap()["confirm"]
        .as_str()
        .unwrap()
        .contains("Saves"));
    assert!(find(&api["actions"], "mode-source-no-save").is_none());
}

// ---------------------------------------------------------------- resume

#[test]
fn resume_shows_the_differences_pins_digests_and_restores() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    item(&p, "ABC-1", true);
    let (f, clock) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    save(&src, &f, &p);
    // Later, api moved on to another build.
    clock.advance(Duration::from_secs(2 * 3600));
    deploy(&p, DIGEST_A, DIGEST_B);

    let r = row(&src, "ABC-1");
    let sec = section(&r, "saved-states").unwrap();
    let resume = find(&sec["actions"], "resume:20261114T120000Z")
        .unwrap()
        .clone();
    assert_eq!(resume["label"], "Resume 2026-11-14 12:00 UTC");
    assert!(
        find(&sec["actions"], "resume-no-save:20261114T120000Z").is_none(),
        "nothing else to save"
    );
    let confirm = resume["confirm"].as_str().unwrap();
    for part in [
        "Resume ABC-1 from the state saved 2026-11-14 12:00 UTC (2 h ago, saved by hand).",
        "What runs now is replaced and not saved; save it first to keep it.",
        "Changes: api: prebuilt 1 @sha256:aaaaaaaaaaaa (now prebuilt @sha256:bbbbbbbbbbbb); unchanged: db.",
        "Restores db from snapshot ABC-1-db-20261114T120000Z (2 h old): this overwrites the database db.",
        "Pull api (registry.example/api:1@sha256:aaaaaaaaaaaa); recreate api (saved state).",
        "The state is sensitive (db: not checked yet).",
    ] {
        assert!(confirm.contains(part), "`{part}` not in: {confirm}");
    }
    // A stale fingerprint is refused with what would run now.
    let (status, body) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        json!({ "fingerprint": "0" }),
    );
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    f.cli.with(|c| c.clear_calls());
    let (status, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let exists = f.cli.with(|c| {
        c.calls_matching(&[
            "exec",
            CONTAINER,
            "test",
            "-e",
            "/snapshots/ABC-1-db-20261114T120000Z",
        ])
    });
    assert_eq!(exists.len(), 1, "exists runs before the plan is accepted");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let lines = f.cli.with(|c| c.lines()).join("\n");
    assert!(
        lines.contains(&format!("docker pull registry.example/api@{DIGEST_A}")),
        "by digest:\n{lines}"
    );
    assert!(
        !lines.contains("manifest inspect"),
        "no tag is resolved:\n{lines}"
    );
    let pull = pane.find("pull api").unwrap();
    let restore = pane
        .find("▶ db: restore ABC-1-db-20261114T120000Z")
        .expect(&pane);
    let checked = pane
        .find("data safety: checking db (after-restore)")
        .expect(&pane);
    assert!(pull < restore && restore < checked, "{pane}");
    let cache: Value = serde_json::from_str(
        &fs::read_to_string(p.work().join("instances/app-0/provenance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["components"]["api"]["image"], DIGEST_A);
    assert_eq!(cache["components"]["api"]["buildId"], "1");
}

#[test]
fn a_missing_snapshot_blocks_only_its_component() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    save(&src, &f, &p);
    deploy(&p, DIGEST_A, DIGEST_B);
    f.cli.0.lock().unwrap().add(
        &["exec", CONTAINER, "test", "-e"],
        CliOutput::failure(1, ""),
    );
    let resume = find(
        &section(&row(&src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T120000Z",
    )
    .unwrap()
    .clone();
    // What was shown assumed the snapshot is there; the call finds it is not.
    let (status, body) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("stale-plan")),
        "{body}"
    );
    let blocked = "Not resumed: db (snapshot ABC-1-db-20261114T120000Z is missing; it was expected in volume app-0-data_db-data at /snapshots/ABC-1-db-20261114T120000Z).";
    assert!(
        body["message"].as_str().unwrap().contains(blocked),
        "{body}"
    );
    // The next view shows it, and that confirm is accepted.
    let resume = find(
        &section(&row(&src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T120000Z",
    )
    .unwrap()
    .clone();
    assert!(resume["confirm"].as_str().unwrap().contains(blocked));
    assert!(!resume["confirm"].as_str().unwrap().contains("Restores db"));
    let (status, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    f.cli.with(|c| c.clear_calls());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(
        pane.contains("· db: not resumed (snapshot ABC-1-db-20261114T120000Z is missing"),
        "{pane}"
    );
    let lines = f.cli.with(|c| c.lines()).join("\n");
    assert!(
        lines.contains(&format!("pull registry.example/api@{DIGEST_A}")),
        "api still resumes:\n{lines}"
    );
    assert!(!lines.contains("/scripts/restore.sh"), "{lines}");
}

const RECIPE: &str = r#"name: recipe
stacks:
  app:
    groups:
      data: { engine: docker }
    dataSafety: { rules: stack/rules.yml, onFinding: block-agents }
    components:
      db:
        group: data
        driver: compose
        file: stack/data.yml
        service: db
        modes: { prebuilt: { image: postgres, tag: "16" } }
        data:
          strategy: recipe
          scrub: [docker, exec, "{container}", sh, stack/scrub.sh]
          seed: [docker, exec, "{container}", /scripts/seed.sh]
          stateSeeds:
            three-categories: [docker, exec, "{container}", /scripts/seeds/three.sh, "{param:count}"]
        dataSafety:
          marker: [docker, exec, "{container}", /scripts/marker.sh]
          sample: [docker, exec, "{container}", /scripts/sample.sh]
      api:
        group: data
        driver: compose
        file: stack/data.yml
        service: api
        modes: { prebuilt: { image: registry.example/api, tag: "1" } }
"#;

#[test]
fn a_recipe_replays_scrub_check_seed_state_seeds_check_in_order() {
    let p = project(RECIPE);
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let mut rec = ItemRecord::load(&dir).unwrap().unwrap();
    rec.data_seeds.insert(
        "db".into(),
        vec![stack_core::states::SeedChoice {
            name: "three-categories".into(),
            params: [("count".to_string(), "3".to_string())]
                .into_iter()
                .collect(),
        }],
    );
    rec.save(&dir).unwrap();
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    // No snapshot hooks: saving changes nothing, so no confirm is needed.
    let save_action = find(&row(&src, "ABC-1")["actions"], "save-state")
        .unwrap()
        .clone();
    assert_eq!(save_action["mutates"], false);
    save(&src, &f, &p);
    let s = &list_states(&dir)[0];
    let recipe = s.data["db"].recipe.as_ref().unwrap();
    assert_eq!(recipe.base_image.as_deref(), Some(DIGEST_A));
    let hooks: Vec<&str> = recipe.scripts.iter().map(|x| x.hook.as_str()).collect();
    assert_eq!(hooks, ["scrub", "seed", "stateSeeds.three-categories"]);
    // A script of the project is pinned by its file's hash; one in the
    // image by its argv (the base digest pins the image).
    assert_eq!(recipe.scripts[0].file.as_deref(), Some("stack/scrub.sh"));
    assert_eq!(recipe.scripts[0].hash.len(), 64);
    assert_eq!(recipe.scripts[1].file, None);
    assert_eq!(recipe.seeds[0].params["count"], "3");
    assert!(s.data["db"].snapshot.is_none());

    let resume = find(
        &section(&row(&src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T120000Z",
    )
    .unwrap()
    .clone();
    let confirm = resume["confirm"].as_str().unwrap();
    assert!(
        confirm.contains("db is rebuilt from its recipe (scrub, seed, state seeds three-categories): data typed in during the session is lost unless a state seed recreates it."),
        "{confirm}"
    );
    assert!(
        confirm.contains("Nothing differs from what runs now.")
            && confirm.contains("ecreate db (saved state)"),
        "a clean container: {confirm}"
    );
    let (status, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    f.cli.with(|c| c.clear_calls());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let order: Vec<String> = f
        .cli
        .with(|c| c.lines())
        .into_iter()
        .filter(|l| l.contains(&format!("exec {CONTAINER}")))
        .map(|l| l.rsplit(CONTAINER).next().unwrap().trim().to_string())
        .collect();
    assert_eq!(
        order,
        [
            "sh stack/scrub.sh",
            "/scripts/marker.sh",
            "/scripts/sample.sh",
            "/scripts/seed.sh",
            "/scripts/seeds/three.sh 3",
            "/scripts/marker.sh",
            "/scripts/sample.sh",
        ],
        "{pane}"
    );
    assert!(
        pane.contains("✓ db: data rebuilt from its recipe"),
        "{pane}"
    );

    // Not clean after scrub: no seed touches the data, and the job fails.
    f.cli.0.lock().unwrap().add(
        &["exec", CONTAINER, "/scripts/sample.sh"],
        CliOutput::success(&dirty_sample()),
    );
    let resume = find(
        &section(&row(&src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T120000Z",
    )
    .unwrap()
    .clone();
    let (_, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    f.cli.with(|c| c.clear_calls());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(
        pane.contains("data safety: db is not clean after scrub, so the recipe stops there"),
        "{pane}"
    );
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["/scripts/seed.sh"]).is_empty()));
}

// ------------------------------------------------------ host copies

/// The engine of [`running`], which also writes the file a `docker cp
/// <container>:<path> <dest>` copies out.
struct Copying(Arc<Locked<FakeCli>>);

impl EngineCli for Copying {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        if inv.args.first().map(String::as_str) == Some("cp")
            && inv
                .args
                .get(1)
                .is_some_and(|a| a.starts_with(&format!("{CONTAINER}:")))
        {
            fs::write(&inv.args[2], "artifact").unwrap();
        }
        self.0.run(inv)
    }
}

#[test]
fn a_host_snapshot_is_copied_out_and_back_and_archive_drops_sensitive_copies() {
    let p = project(&yml("host", "", false));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (mut f, _) = world(running(&clean_sample()));
    let copying = Arc::new(Copying(f.cli.clone()));
    f.world.cli = copying.clone();
    f.world.steps = Arc::new(CliSteps(copying));
    let src = source(f.world.clone(), &p.dir);
    let save_action = find(&row(&src, "ABC-1")["actions"], "save-state")
        .unwrap()
        .clone();
    assert!(save_action["confirm"].as_str().unwrap().ends_with(
        "Snapshots db (pg_dump custom) and copies it out to the host with its create hook."
    ));
    save(&src, &f, &p);
    let s = &list_states(&dir)[0];
    let snap = s.data["db"].snapshot.as_ref().unwrap();
    let host = dir.join("states/data/ABC-1-db-20261114T120000Z");
    assert_eq!(snap.host_path.as_deref(), Some(host.as_path()));
    assert_eq!(snap.size_bytes, Some(8));
    assert!(host.is_file());
    assert!(f.cli.with(|c| !c
        .calls_matching(&[
            "docker",
            "cp",
            &format!("{CONTAINER}:/snapshots/ABC-1-db-20261114T120000Z")
        ])
        .is_empty()));

    // Resume puts it back in before restore runs.
    let resume = find(
        &section(&row(&src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T120000Z",
    )
    .unwrap()
    .clone();
    let (status, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    f.cli.with(|c| c.clear_calls());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let lines = f.cli.with(|c| c.lines());
    let copy_in = lines
        .iter()
        .position(|l| {
            l.contains(" cp ")
                && l.ends_with(&format!("{CONTAINER}:/snapshots/ABC-1-db-20261114T120000Z"))
        })
        .expect("copied back in");
    let restore = lines
        .iter()
        .position(|l| l.contains("/scripts/restore.sh"))
        .unwrap();
    assert!(copy_in < restore, "{lines:?}");
    // A host snapshot is checked on the host, not in the container.
    assert!(!lines.iter().any(|l| l.contains("test -e")), "{lines:?}");

    // The state is sensitive (db has no check): archive keeps only its
    // reference, unless "keep data" is chosen.
    let r = row(&src, "ABC-1");
    let keep =
        find(&r["actions"], "archive-keep-data").expect("offered: there is sensitive host data");
    assert!(keep["confirm"]
        .as_str()
        .unwrap()
        .contains("keep their snapshots' host copies"));
    let archive = find(&r["actions"], "archive").unwrap();
    assert!(archive["confirm"]
        .as_str()
        .unwrap()
        .contains("Sensitive saved states keep only their references"));
    let (status, reply) = act(&src, "work", "archive", Some("ABC-1"), json!({}));
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let archived = p.work().join("_archive/ABC-1");
    let states = list_states(&archived);
    assert_eq!(states.len(), 2, "the automatic save came along");
    for s in &states {
        let snap = s.data["db"].snapshot.as_ref().unwrap();
        assert!(snap.removed_utc.is_some(), "{snap:?}");
        assert_eq!(
            snap.removed_why.as_deref(),
            Some("removed on archive: the state is sensitive")
        );
    }
    let mut files = Vec::new();
    files_under(&archived.join("states/data"), &mut files);
    assert!(files.is_empty(), "{files:?}");
}

// -------------------------------------------------- volumes, pruning

#[test]
fn deleting_the_data_volumes_is_its_own_action_and_names_the_states_it_costs() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let dv = stack_action(&src, "down-volumes").unwrap();
    assert!(dv["confirm"]
        .as_str()
        .unwrap()
        .contains("No saved state keeps a snapshot there."));
    save(&src, &f, &p);
    let down = stack_action(&src, "down").unwrap();
    assert!(down["confirm"]
        .as_str()
        .unwrap()
        .contains("Containers are removed; volumes are kept."));
    let dv = stack_action(&src, "down-volumes").unwrap();
    let confirm = dv["confirm"].as_str().unwrap();
    for part in [
        "Down and DELETE the data volumes app-0-data_db-data.",
        "These saved states keep snapshots there and lose them: ABC-1 20261114T120000Z (db). Down (without this) keeps the volumes.",
        "Containers and these volumes are removed.",
    ] {
        assert!(confirm.contains(part), "`{part}` not in: {confirm}");
    }
    assert_eq!(dv["label"], "Down and delete data");
    f.cli.with(|c| c.clear_calls());
    let (status, reply) = act(
        &src,
        "environment",
        "down-volumes",
        None,
        dv["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let downs = f
        .cli
        .with(|c| c.calls_matching(&["compose", "-p", PROJECT, "down", "--volumes"]));
    assert_eq!(downs.len(), 1, "{pane}");
    // Its automatic save takes no snapshot into the volume it deletes.
    let s = &list_states(&dir)[0];
    assert_eq!(s.reason, SaveReason::BeforeDown);
    assert!(s.data["db"].snapshot.is_none());
    assert_eq!(
        s.data["db"].note.as_deref(),
        Some("not snapshotted: this job deletes the volume it would live in")
    );
    assert_eq!(
        f.cli
            .with(|c| c.calls_matching(&["/scripts/snap.sh"]).len()),
        0
    );
}

#[test]
fn pruning_keeps_the_newest_states_and_deletes_their_snapshots() {
    let p = project(&yml("in-volume", ", keepStates: 2", true).replace("keepStates: 20, ", ""));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, clock) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    for _ in 0..3 {
        save(&src, &f, &p);
        clock.advance(Duration::from_secs(60));
    }
    let ids: Vec<String> = list_states(&dir).into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["20261114T120200Z", "20261114T120100Z"]);
    let deletes = f.cli.with(|c| c.calls_matching(&["exec", CONTAINER, "rm"]));
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].args.last().unwrap(),
        "/snapshots/ABC-1-db-20261114T120000Z"
    );
}

#[test]
fn job_records_a_saved_state_refers_to_are_never_pruned() {
    let p = project(&yml("in-volume", "", true));
    let work = p.work();
    let old = "2020-01-01T00:00:00Z";
    for id in ["j20200101T000000Z-aaaaaa", "j20200101T000000Z-bbbbbb"] {
        let rec = JobRecord {
            id: id.into(),
            action: "up".into(),
            label: "x".into(),
            target: None,
            item: None,
            stack: "app".into(),
            slot: 0,
            instance: "app-0".into(),
            argv: vec![],
            cwd: ".".into(),
            mutates: true,
            confirm: String::new(),
            status: JobStatus::Done,
            exit_code: Some(0),
            created_utc: old.into(),
            started_utc: None,
            ended_utc: None,
            runner_pid: None,
            headless: false,
            log: JobRecord::log_path(&work, id),
            result: JobRecord::result_path(&work, id),
            work: JobWork::Gc,
            report: None,
            error: None,
            actor: None,
        };
        rec.save(&work).unwrap();
    }
    let dir = item(&p, "ABC-2", false);
    let state: SavedState = serde_json::from_value(json!({
        "format": 1, "id": "20200101T000000Z", "savedUtc": old, "reason": "manual",
        "chappaStack": "0.1.0", "item": { "source": "tracker", "key": "ABC-2", "folder": "ABC-2" },
        "stack": "app", "slot": 0, "instance": "app-0", "sensitive": false,
        "jobs": ["j20200101T000000Z-aaaaaa"],
    }))
    .unwrap();
    state.save(&dir).unwrap();
    let keep = JobsDef {
        keep_days: 1,
        keep_count: 0,
    };
    let removed = records::prune(&work, &keep, NOW);
    assert_eq!(removed, ["j20200101T000000Z-bbbbbb"]);
    assert!(JobRecord::load(&work, "j20200101T000000Z-aaaaaa")
        .unwrap()
        .is_some());
}

#[test]
fn a_remote_location_is_refused_until_the_cloud_driver_exists() {
    let p = project(&yml("remote", "", true));
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let env = get(&src, "/api/environment").1;
    let lines = env["banners"][0]["lines"].as_array().unwrap();
    assert!(
        lines.iter().any(|l| l
            .as_str()
            .unwrap()
            .contains("`location: remote` is not supported yet")),
        "{lines:?}"
    );
    let (status, body) = act(&src, "environment", "up", None, json!({}));
    assert_eq!((status, body["kind"].as_str()), (409, Some("definition")));
}

// ------------------------------------------------- worktrees (real git)

const GIT_YML: &str = r#"
name: states-git
stacks:
  app:
    root: ../clones
    groups:
      data: { engine: docker }
    components:
      db:
        group: data
        driver: compose
        file: stack/data.yml
        service: db
        modes: { prebuilt: { image: postgres, tag: "16" } }
      api:
        group: data
        driver: compose
        file: stack/data.yml
        service: api
        modes:
          prebuilt: { image: registry.example/api, tag: "1" }
          source: { repo: api }
        default: prebuilt
        depends_on: [db]
"#;

/// ABC-1 with a worktree of api on `task/ABC-1` (two commits: the saved
/// one, then one more), running api from the newer one, and a state that
/// says api ran from `saved` as `api:<c8>`.
fn git_item(saved_commit: Option<&str>) -> Option<(Project, Fakes, PathBuf, String, String)> {
    if !has_git() {
        return None;
    }
    let p = Project::with(GIT_YML, &[("stack/data.yml", COMPOSE)]);
    let (_, clone) = origin_and_clone(&p.dir, "api");
    let dir = item(&p, "ABC-1", true);
    let first = test_git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
    let wt = dir.join("src").join(format!("api-{}", &first[..8]));
    test_git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/ABC-1",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    commit_file(&wt, "more.txt", "more\n", "more work");
    let second = test_git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    let mut rec = ItemRecord::load(&dir).unwrap().unwrap();
    rec.repos.insert(
        "api".into(),
        stack_core::workspace::ItemRepo {
            branch: "task/ABC-1".into(),
            path: PathBuf::from("src").join(format!("api-{}", &first[..8])),
            c8: first[..8].to_string(),
            base: "main".into(),
            created_utc: NOW.into(),
        },
    );
    rec.save(&dir).unwrap();
    let inst = p.work().join("instances");
    fs::create_dir_all(inst.join("app-0")).unwrap();
    fs::write(
        inst.join("app-0/provenance.json"),
        json!({ "components": {
            "db": { "mode": "prebuilt", "buildId": "16", "image": DIGEST_A },
            "api": { "mode": "source", "ref": "task/ABC-1", "commit": second, "image": format!("api:{}", &second[..8]) },
        } })
        .to_string(),
    )
    .unwrap();
    fs::write(
        inst.join("app-0.json"),
        json!({ "instance": "app-0", "stack": "app", "slot": 0, "item": "ABC-1",
                "components": { "db": { "desired": "running", "mode": "prebuilt" },
                                "api": { "desired": "running", "mode": "source", "ref": "task/ABC-1" } },
                "updatedUtc": NOW })
        .to_string(),
    )
    .unwrap();
    let saved = saved_commit
        .map(str::to_string)
        .unwrap_or_else(|| first.clone());
    let state: SavedState = serde_json::from_value(json!({
        "format": 1, "id": "20261114T110000Z", "savedUtc": "2026-11-14T11:00:00Z", "reason": "manual",
        "chappaStack": "0.1.0", "item": { "source": "tracker", "key": "ABC-1", "folder": "ABC-1" },
        "stack": "app", "slot": 0, "instance": "app-0", "sensitive": false,
        "components": {
            "db": { "mode": "prebuilt", "image": DIGEST_A, "buildId": "16", "desired": "running" },
            "api": { "mode": "source", "ref": "task/ABC-1", "commit": saved,
                     "image": format!("api:{}", &saved[..8]), "desired": "running", "repo": "api" },
        },
    }))
    .unwrap();
    state.save(&dir).unwrap();
    // The saved image is gone, so the resume builds it again.
    let cli = with_docker_project(
        docker_facts(FakeCli::new()),
        PROJECT,
        &[Ctr::running("db"), Ctr::running("api")],
    )
    .failing(
        &["image", "inspect", &format!("api:{}", &saved[..8])],
        1,
        "No such image",
    );
    let (mut f, _) = world(cli);
    f.world.git = Arc::new(stack_core::provenance::GitCli::default());
    Some((p, f, wt, first, second))
}

fn resume_action(src: &chappa_stack::server::Source) -> Value {
    find(
        &section(&row(src, "ABC-1"), "saved-states").unwrap()["actions"],
        "resume:20261114T110000Z",
    )
    .unwrap()
    .clone()
}

fn refreshed_row(src: &chappa_stack::server::Source) -> Value {
    src.work.invalidate();
    src.refresh_work();
    row(src, "ABC-1")
}

fn resume_ids(row: &Value) -> Vec<String> {
    section(row, "saved-states").unwrap()["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect()
}

fn offered_resume(row: &Value, id: &str) -> Value {
    find(&section(row, "saved-states").unwrap()["actions"], id)
        .unwrap_or_else(|| panic!("no {id} in {row}"))
        .clone()
}

#[test]
fn a_moved_branch_offers_resume_at_tips_or_exact_commits_and_a_detached_worktree_says_so() {
    let Some((p, f, wt, first, second)) = git_item(None) else {
        return;
    };
    let (a, b) = (&first[..8], &second[..8]);
    let src = source(f.world.clone(), &p.dir);

    // Before the workspaces were looked at, the view cannot tell; the call
    // finds the moved branch and says which variants there are.
    let plain = resume_action(&src);
    let (status, body) = act(
        &src,
        "work",
        "resume:20261114T110000Z",
        Some("ABC-1"),
        plain["params"].clone(),
    );
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("api's branch task/ABC-1 moved from {a} to {b}"))
            && message.contains("choose Resume at branch tips or Resume exact commits"),
        "{message}"
    );

    let r = refreshed_row(&src);
    assert_eq!(
        resume_ids(&r),
        [
            "resume-tips:20261114T110000Z",
            "resume-exact:20261114T110000Z"
        ]
    );
    let exact = offered_resume(&r, "resume-exact:20261114T110000Z");
    assert_eq!(exact["label"], "Resume 2026-11-14 11:00 UTC exact commits");
    let confirm = exact["confirm"].as_str().unwrap();
    assert!(
        confirm.contains(&format!("Since the save, api's branch task/ABC-1 moved from {a} to {b}."))
            && confirm.contains(&format!(
                "Resumes the exact saved commits: the worktree of api is left detached at {a} (switch it back to task/ABC-1 afterwards)."
            ))
            && confirm.contains("The worktrees of api go to the saved commits."),
        "{confirm}"
    );
    let tips = offered_resume(&r, "resume-tips:20261114T110000Z");
    assert_eq!(tips["label"], "Resume 2026-11-14 11:00 UTC at branch tips");
    let confirm = tips["confirm"].as_str().unwrap();
    assert!(
        confirm.contains(&format!(
            "Resumes at the branch tips: api builds from task/ABC-1 at {b}, not the saved {a}, so its code differs from the save."
        )) && !confirm.contains("go to the saved commits"),
        "{confirm}"
    );

    // Uncommitted work in the worktree refuses the whole resume.
    fs::write(wt.join("notes.txt"), "unsaved\n").unwrap();
    let (status, body) = act(
        &src,
        "work",
        "resume-exact:20261114T110000Z",
        Some("ABC-1"),
        exact["params"].clone(),
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("dirty")),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("api has 1 uncommitted change"),
        "{body}"
    );
    fs::remove_file(wt.join("notes.txt")).unwrap();

    // Exact: the worktree goes to the saved commit, detached, and api is
    // built from it again.
    let (status, reply) = act(
        &src,
        "work",
        "resume-exact:20261114T110000Z",
        Some("ABC-1"),
        exact["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(test_git(&wt, &["rev-parse", "HEAD"]).trim(), first);
    assert_eq!(
        test_git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "HEAD"
    );
    let builds = f.cli.with(|c| c.calls_matching(&["docker", "build"]));
    assert_eq!(builds.len(), 1, "{pane}");
    assert!(builds[0].args.iter().any(|x| x == &format!("api:{a}")));

    // The detached worktree carries a badge and a way back, until HEAD is
    // on a branch again.
    let r = refreshed_row(&src);
    let detached = format!("detached at {a} — switch back to task/ABC-1");
    assert!(
        r["badges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["text"] == format!("api {detached}") && x["tone"] == "warn"),
        "{r}"
    );
    let repos = section(&r, "repos").unwrap();
    assert!(repos["rows"][0]["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["text"] == detached.as_str()));
    let switch = find(&r["actions"], "switch-branch:api").expect("a way back");
    assert_eq!(switch["kind"], "terminal");
    assert_eq!(switch["label"], "Switch api back to task/ABC-1");
    assert_eq!(switch["command"], json!(["git", "switch", "task/ABC-1"]));
    assert_eq!(
        Path::new(switch["cwd"].as_str().unwrap()),
        wt.as_path(),
        "it runs in the worktree"
    );

    // Tips: back on the branch at its tip; the detached badge goes.
    let r = refreshed_row(&src);
    let tips = offered_resume(&r, "resume-tips:20261114T110000Z");
    let (status, reply) = act(
        &src,
        "work",
        "resume-tips:20261114T110000Z",
        Some("ABC-1"),
        tips["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(test_git(&wt, &["rev-parse", "HEAD"]).trim(), second);
    assert_eq!(
        test_git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "task/ABC-1"
    );
    let r = refreshed_row(&src);
    assert!(find(&r["actions"], "switch-branch:api").is_none(), "{r}");
    assert!(
        !r["badges"].to_string().contains("detached at")
            && !section(&r, "repos")
                .unwrap()
                .to_string()
                .contains("detached at"),
        "{r}"
    );

    // A state at the branch tip: one plain Resume, and the variants are
    // refused.
    let dir = p.work().join("ABC-1");
    let mut s = SavedState::load(&dir, "20261114T110000Z").unwrap().unwrap();
    s.components.get_mut("api").unwrap().commit = Some(second.clone());
    s.save(&dir).unwrap();
    let r = refreshed_row(&src);
    assert_eq!(resume_ids(&r), ["resume:20261114T110000Z"]);
    let (status, body) = act(
        &src,
        "work",
        "resume-exact:20261114T110000Z",
        Some("ABC-1"),
        exact["params"].clone(),
    );
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    assert_eq!(
        body["message"],
        "no branch moved since the state was saved; use Resume"
    );
}

#[test]
fn a_commit_missing_from_the_repo_blocks_that_component_with_a_reason() {
    let missing = "1234567890abcdef1234567890abcdef12345678";
    let Some((p, f, _, _, _)) = git_item(Some(missing)) else {
        return;
    };
    let src = source(f.world.clone(), &p.dir);
    let resume = resume_action(&src);
    let (status, body) = act(
        &src,
        "work",
        "resume:20261114T110000Z",
        Some("ABC-1"),
        resume["params"].clone(),
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("stale-plan")),
        "{body}"
    );
    assert!(
        body["message"].as_str().unwrap().contains(
            "Not resumed: api (commit 12345678 is not in its worktree of api; fetch it, or resume a newer state)."
        ),
        "{body}"
    );
}

#[test]
fn resuming_another_items_state_saves_the_bound_one_first_or_not() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    let first = item(&p, "ABC-1", true);
    let (f, clock) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    save(&src, &f, &p);
    // ABC-1's state, copied to ABC-2 as if it had been saved there.
    let other = item(&p, "ABC-2", false);
    let mut s = list_states(&first).remove(0);
    s.item.key = "ABC-2".into();
    s.item.folder = "ABC-2".into();
    s.save(&other).unwrap();
    clock.advance(Duration::from_secs(60));
    let sec = section(&row(&src, "ABC-2"), "saved-states")
        .unwrap()
        .clone();
    let resume = find(&sec["actions"], "resume:20261114T120000Z").unwrap();
    assert!(resume["confirm"]
        .as_str()
        .unwrap()
        .contains("Saves the stack state of ABC-1 first, then binds slot 0 to ABC-2."));
    let plain = find(&sec["actions"], "resume-no-save:20261114T120000Z").expect("the twin");
    assert_eq!(
        plain["label"],
        "Resume 2026-11-14 12:00 UTC without saving ABC-1"
    );
    assert!(plain["confirm"]
        .as_str()
        .unwrap()
        .contains("Binds slot 0 to ABC-2 without saving the stack state of ABC-1 first."));

    let (status, reply) = act(
        &src,
        "work",
        "resume:20261114T120000Z",
        Some("ABC-2"),
        resume["params"].clone(),
    );
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let saved = list_states(&first);
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0].reason, SaveReason::BeforeRebind);
    assert_eq!(
        ActiveRecord::load(&p.work()).unwrap().get(0).unwrap().item,
        "ABC-2"
    );
}

// ------------------------------------------------------------ state seeds

/// The engine of a stack whose db is not up yet: once a `compose up` names
/// db, `ps` and `inspect` show it running.
struct Upping(Arc<Locked<FakeCli>>);

impl EngineCli for Upping {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        let up = inv.args.iter().any(|a| a == "up") && inv.args.iter().any(|a| a == "db");
        let out = self.0.run(inv);
        if up {
            let ctrs = [Ctr::running("db"), Ctr::running("api")];
            let ps: Vec<String> = ctrs.iter().map(|c| c.ps(PROJECT).to_string()).collect();
            let inspect: Vec<Value> = ctrs.iter().map(|c| c.inspect(PROJECT)).collect();
            let mut cli = self.0 .0.lock().unwrap();
            cli.add(
                &["docker", "compose", "-p", PROJECT, "ps"],
                CliOutput::success(&ps.join("\n")),
            );
            cli.add(
                &["docker", "inspect", "--type", "container"],
                CliOutput::success(&Value::from(inspect).to_string()),
            );
        }
        out
    }
}

fn set_seeds(src: &chappa_stack::server::Source, params: Value, confirmed: bool) -> (u16, Value) {
    post(
        src,
        "/api/actions",
        &json!({ "id": "set-seeds", "target": { "view": "work", "key": "ABC-1" }, "params": params, "confirmed": confirmed }),
        true,
    )
}

#[test]
fn an_item_chooses_its_state_seeds_through_inputs_and_up_replays_them() {
    let p = project(RECIPE);
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let cli = with_docker_project(
        docker_facts(FakeCli::new()),
        PROJECT,
        &[Ctr::running("api")],
    )
    .on(&["exec", CONTAINER, "/scripts/marker.sh"], MARKER_OK)
    .on(&["exec", CONTAINER, "/scripts/sample.sh"], &clean_sample());
    let (mut f, _) = world(cli);
    let upping = Arc::new(Upping(f.cli.clone()));
    f.world.cli = upping.clone();
    f.world.steps = Arc::new(CliSteps(upping));
    let src = source(f.world.clone(), &p.dir);

    let action = find(&row(&src, "ABC-1")["actions"], "set-seeds")
        .expect("offered: db defines state seeds")
        .clone();
    assert_eq!(action["kind"], "endpoint");
    assert_eq!(action["mutates"], true);
    assert!(action["confirm"]
        .as_str()
        .unwrap()
        .starts_with("Set the state seeds ABC-1 asks for (now: none)."));
    assert_eq!(
        action["inputs"],
        json!([
            { "name": "seeds", "label": "State seeds", "kind": "multi",
              "options": [{ "value": "db/three-categories", "label": "db: three-categories" }],
              "default": [] },
            { "name": "param:db/three-categories/count", "label": "db: three-categories — count",
              "kind": "text", "max": 200 },
        ])
    );

    // Checked here too: unconfirmed, unknown seeds, a missing or bad parameter.
    let picked =
        json!({ "seeds": ["db/three-categories"], "param:db/three-categories/count": "3" });
    assert_eq!(
        set_seeds(&src, picked.clone(), false).1["kind"],
        "confirmation-required"
    );
    for (params, says) in [
        (
            json!({ "seeds": "db/three-categories" }),
            "`seeds` must be a list of strings",
        ),
        (
            json!({ "seeds": ["db/nope"] }),
            "`db/nope` is not a state seed of this stack",
        ),
        (
            json!({ "seeds": ["db/three-categories"] }),
            "db: three-categories needs a value for count",
        ),
        (
            json!({ "seeds": ["db/three-categories"], "param:db/three-categories/count": "3\n4" }),
            "must be printable",
        ),
        (
            json!({ "seeds": ["db/three-categories"], "param:db/three-categories/count": "9".repeat(201) }),
            "at most 200 characters",
        ),
    ] {
        let (status, body) = set_seeds(&src, params, true);
        assert_eq!((status, body["kind"].as_str()), (400, Some("bad-input")));
        assert!(body["message"].as_str().unwrap().contains(says), "{body}");
    }
    assert!(ItemRecord::load(&dir)
        .unwrap()
        .unwrap()
        .data_seeds
        .is_empty());

    let (status, body) = set_seeds(&src, picked, true);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["message"],
        "ABC-1 asks for these state seeds: db: three-categories (count 3)"
    );
    let rec = ItemRecord::load(&dir).unwrap().unwrap();
    assert_eq!(rec.data_seeds["db"][0].name, "three-categories");
    assert_eq!(rec.data_seeds["db"][0].params["count"], "3");
    // The inputs now default to what the item asks for.
    let action = find(&row(&src, "ABC-1")["actions"], "set-seeds")
        .unwrap()
        .clone();
    assert_eq!(
        action["inputs"][0]["default"],
        json!(["db/three-categories"])
    );
    assert_eq!(action["inputs"][1]["default"], "3");

    // Up creates db: its recipe runs with the item's seeds after the steps.
    let up = stack_action(&src, "up").expect("up is offered while db is not running");
    let (status, reply) = act(&src, "environment", "up", None, up["params"].clone());
    assert_eq!(status, 202, "{reply}");
    f.cli.with(|c| c.clear_calls());
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let order: Vec<String> = f
        .cli
        .with(|c| c.lines())
        .into_iter()
        .filter(|l| l.contains(&format!("exec {CONTAINER}")))
        .map(|l| l.rsplit(CONTAINER).next().unwrap().trim().to_string())
        .collect();
    assert_eq!(
        order,
        [
            "sh stack/scrub.sh",
            "/scripts/marker.sh",
            "/scripts/sample.sh",
            "/scripts/seed.sh",
            "/scripts/seeds/three.sh 3",
            "/scripts/marker.sh",
            "/scripts/sample.sh",
        ],
        "{pane}"
    );

    // Up again with db running: nothing is created, nothing reseeded.
    f.cli.with(|c| c.clear_calls());
    if let Some(up) = stack_action(&src, "up") {
        let (status, reply) = act(&src, "environment", "up", None, up["params"].clone());
        if status == 202 {
            let (code, pane) = run(&f, &p.dir, &reply);
            assert_eq!(code, EXIT_DONE, "{pane}");
        }
    }
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["/scripts/seeds/three.sh"]).is_empty()));

    // Choosing none clears them.
    let (status, _) = set_seeds(&src, json!({ "seeds": [] }), true);
    assert_eq!(status, 200);
    assert!(ItemRecord::load(&dir)
        .unwrap()
        .unwrap()
        .data_seeds
        .is_empty());
}

// ----------------------------------------------------------- who asked

#[test]
fn a_job_records_who_asked_for_it_and_a_state_it_saves_says_so() {
    let p = project(&yml("in-volume", "", true));
    deploy(&p, DIGEST_A, DIGEST_A);
    let dir = item(&p, "ABC-1", true);
    let (f, _) = world(running(&clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let save_action = find(&row(&src, "ABC-1")["actions"], "save-state")
        .unwrap()
        .clone();
    let call = |actor: Option<&str>| {
        src.handle_as(
            "POST",
            "/api/actions",
            Some(&bearer()),
            &chappa_stack::server::Caller {
                runs_jobs: true,
                actor: actor.map(str::to_string),
            },
            &json!({ "id": "save-state", "target": { "view": "work", "key": "ABC-1" },
                     "params": save_action["params"], "confirmed": true })
            .to_string(),
        )
    };
    let (status, reply) = call(Some("human:alex"));
    assert_eq!(status, 202, "{reply}");
    let id = reply["jobId"].as_str().unwrap();
    let rec = JobRecord::load(&p.work(), id).unwrap().unwrap();
    assert_eq!(rec.actor.as_deref(), Some("human:alex"));
    let (code, pane) = run(&f, &p.dir, &reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert_eq!(list_states(&dir)[0].saved_by.as_deref(), Some("human:alex"));
    let shown = section(&row(&src, "ABC-1"), "saved-states").unwrap()["rows"][0]["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(shown.contains("by human:alex"), "{shown}");

    // Kept as the app writes it: printable ASCII, capped.
    let (_, reply) = call(Some("agent:7\u{1b}[31m"));
    let rec = JobRecord::load(&p.work(), reply["jobId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.actor.as_deref(), Some("agent:7_[31m"));
    // No header: no actor, and the state says nothing about who.
    let (_, reply) = call(None);
    let rec = JobRecord::load(&p.work(), reply["jobId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.actor, None);
    assert!(!serde_json::to_string(&rec).unwrap().contains("actor"));
}
