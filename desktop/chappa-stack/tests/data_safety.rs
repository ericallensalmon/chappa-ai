//! Data-safety checks run by the source, through fakes: hooks run through
//! the engine CLI (scripted), verdicts recorded on the instance, the
//! `dataSafety` block in every state (pinned as a snapshot,
//! `tests/snapshots/data_safety_states.json`; `UPDATE_SNAPSHOTS=1` rewrites
//! it), `blocksAgents` over every policy and state, the acknowledge action,
//! the `every:` schedule on a manual clock, `block-up` and `stop` enforced
//! by the job, and no sampled value anywhere it could be kept.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chappa_stack::background::Background;
use chappa_stack::data_safety::{self, CheckEntry, CheckStore, SafetyState};
use chappa_stack::identity::Token;
use chappa_stack::project::{Loaded, ProjectConfig};
use chappa_stack::records::{self, JobRecord, JobResult, JobStatus};
use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_FAILED};
use chappa_stack::server::{Caller, Source};
use chappa_stack::world::{CliSteps, Locked, ManualClock, World};
use common::*;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::intent::{Desired, IntentRecord};
use stack_core::probe::FakeHttp;
use stack_core::safety::data::{acknowledge, evaluate, DataRules, HookRun};

const NOW: &str = "2026-11-14T12:00:00Z";
const PROJECT: &str = "app-0-data";
const CONTAINER: &str = "app-0-data-db-1";

const RULES: &str = r#"rules:
  - id: email-real-domain
    pattern: '(?i)[^@\s]+@[^@\s]+\.[a-z]{2,}'
    allow: ['(?i)@example\.(com|org|net)$']
  - id: card-number
    pattern: '\b(?:\d[ -]?){13,19}\b'
    validate: luhn
"#;

// Published card-number test vectors and addresses on a reserved TLD: the
// shapes the rules look for, none of them real.
const CARD_1: &str = "4111 1111 1111 1111";
const CARD_2: &str = "5500 0000 0000 0004";
const EMAIL_1: &str = "person@customers.invalid";
const EMAIL_2: &str = "second@customers.invalid";
const FIXTURE_1: &str = "user1@example.com";
const FIXTURE_2: &str = "user2@example.org";

const MARKER_OK: &str = r#"{"scrubbed":true,"version":"2026-11","at":"2026-11-01T00:00:00Z"}"#;

fn sample(emails: &[&str], cards: &[&str]) -> String {
    json!([
        { "table": "customers", "column": "email", "values": emails },
        { "table": "payments", "column": "card", "values": cards },
    ])
    .to_string()
}

fn clean_sample() -> String {
    sample(&[FIXTURE_1, FIXTURE_2], &["TEST-1"])
}

fn dirty_sample(cards: &[&str]) -> String {
    sample(&[FIXTURE_1, EMAIL_1], cards)
}

const API: &str = r#"
      api:
        group: data
        driver: compose
        file: stack/data.yml
        service: api
        modes: { prebuilt: { image: registry.example/api, tag: "1" } }
        depends_on: [db]"#;

const CACHE: &str = r#"
      cache:
        group: data
        driver: compose
        file: stack/data.yml
        service: cache
        modes: { prebuilt: { image: registry.example/cache, tag: "7" } }
        persistence: { volume: cache-data }"#;

fn yml(on_finding: &str, unscanned: &str, extra: &str) -> String {
    format!(
        r#"name: safety
stacks:
  app:
    hostname: dev.example.test
    groups:
      data: {{ engine: docker }}
    dataSafety: {{ rules: stack/rules.yml, onFinding: {on_finding}, unscanned: {unscanned} }}
    components:
      db:
        group: data
        driver: compose
        file: stack/data.yml
        service: db
        modes: {{ prebuilt: {{ image: postgres, tag: "16" }} }}
        persistence: {{ volume: db-data }}
        dataSafety:
          marker: [docker, exec, "{{container}}", /scripts/marker.sh]
          sample: [docker, exec, "{{container}}", /scripts/sample.sh]
          when: [after-up, every: 6h]{extra}
"#
    )
}

fn project(on_finding: &str, unscanned: &str, extra: &str) -> Project {
    Project::with(
        &yml(on_finding, unscanned, extra),
        &[
            (
                "stack/data.yml",
                "services:\n  db: { image: x, expose: [\"5432\"] }\n  api: { image: x }\n  cache: { image: x }\n",
            ),
            ("stack/rules.yml", RULES),
        ],
    )
}

/// The engine with db running and the hooks answering as given (`Err`:
/// the marker hook fails with that error output).
fn running_cli(marker: Result<&str, &str>, sample: &str) -> FakeCli {
    let cli = with_docker_project(docker_facts(FakeCli::new()), PROJECT, &[Ctr::running("db")]);
    let cli = match marker {
        Ok(m) => cli.on(&["exec", CONTAINER, "/scripts/marker.sh"], m),
        Err(stderr) => cli.failing(&["exec", CONTAINER, "/scripts/marker.sh"], 1, stderr),
    };
    cli.on(&["exec", CONTAINER, "/scripts/sample.sh"], sample)
}

fn set_hooks(f: &Fakes, marker: &str, sample: &str) {
    let mut cli = f.cli.0.lock().unwrap();
    cli.add(
        &["exec", CONTAINER, "/scripts/marker.sh"],
        CliOutput::success(marker),
    );
    cli.add(
        &["exec", CONTAINER, "/scripts/sample.sh"],
        CliOutput::success(sample),
    );
}

fn world_at(cli: FakeCli) -> Fakes {
    let mut f = fakes(cli, FakeHttp::default());
    f.world.clock = ManualClock::at(NOW);
    f
}

fn env(src: &Source) -> Value {
    get(src, "/api/environment").1
}

fn actor(src: &Source, body: &Value, who: Option<&str>) -> (u16, Value) {
    src.handle_as(
        "POST",
        "/api/actions",
        Some(&bearer()),
        &Caller {
            runs_jobs: true,
            actor: who.map(str::to_string),
        },
        &body.to_string(),
    )
}

/// Run the check action for db in a "pane"; the exit code and the pane.
fn check(src: &Source, world: &World, dir: &Path) -> (i32, String) {
    let body = json!({
        "id": "data-safety-check",
        "target": { "view": "environment", "key": "db" },
        "params": null,
        "note": null,
        "confirmed": false,
    });
    let (status, reply) = post(src, "/api/actions", &body, true);
    assert_eq!(status, 202, "{reply}");
    let id = reply["jobId"].as_str().unwrap().to_string();
    let rec = JobRecord::load(&dir.join(".work"), &id).unwrap().unwrap();
    assert!(!rec.mutates, "a check changes nothing");
    let mut pane = Vec::new();
    let code = run_job(world, dir, &id, &mut pane);
    (code, String::from_utf8(pane).unwrap())
}

fn result_of(dir: &Path, id: &str) -> JobResult {
    let text = fs::read_to_string(JobRecord::result_path(&dir.join(".work"), id)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn find_action<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"] == id)
}

fn facts_section<'a>(env: &'a Value, id: &str) -> &'a Value {
    env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no facts section {id}"))
}

fn ack_action(env: &Value) -> Option<Value> {
    find_action(
        &facts_section(env, "data-safety")["actions"],
        "data-safety-ack",
    )
    .cloned()
}

fn ack_body(action: &Value, note: Option<&str>, confirmed: bool) -> Value {
    json!({
        "id": "data-safety-ack",
        "target": { "view": "environment", "key": null },
        "params": action["params"],
        "note": note,
        "confirmed": confirmed,
    })
}

fn rules_hash() -> String {
    DataRules::parse(RULES, "rules.yml").unwrap().hash
}

// ----------------------------------------------------------- the hooks

#[test]
fn a_missing_marker_is_not_clean_whatever_the_sample_says() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(
        Err(&format!("ERROR: no marker table; last row {CARD_1}")),
        &clean_sample(),
    ));
    let src = source(f.world.clone(), &p.dir);
    let (code, pane) = check(&src, &f.world, &p.dir);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(pane.contains("data safety: checking db (manual)"), "{pane}");
    assert!(pane.contains("  marker hook exited with code 1"), "{pane}");
    assert!(pane.contains("✗ db: not clean: no scrub marker"), "{pane}");
    assert!(!pane.contains(CARD_1), "hook error output is never shown");
    // The hooks ran inside the container, by name.
    let calls = f
        .cli
        .with(|c| c.calls_matching(&["docker", "exec", CONTAINER]));
    assert_eq!(calls.len(), 2);
    let ds = &env(&src)["dataSafety"];
    assert_eq!(ds["state"], "not-clean");
    assert_eq!(ds["marker"], json!({ "scrubbed": false }));
    assert_eq!(ds["findings"], json!([]));
    assert_eq!(ds["blocksAgents"], true);
}

#[test]
fn a_sample_with_findings_reports_counts_per_place() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1, CARD_2])));
    let src = source(f.world.clone(), &p.dir);
    let (code, pane) = check(&src, &f.world, &p.dir);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(
        pane.contains("✗ db: not clean: 3 findings (email-real-domain ×1, card-number ×2)"),
        "{pane}"
    );
    let e = env(&src);
    let ds = &e["dataSafety"];
    assert_eq!(
        ds["findings"],
        json!([
            { "rule": "email-real-domain", "table": "customers", "column": "email", "count": 1, "severity": "bad" },
            { "rule": "card-number", "table": "payments", "column": "card", "count": 2, "severity": "bad" },
        ])
    );
    // The alert, the badge, the banner with the acknowledge action.
    let alert = e["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["componentId"] == "db")
        .unwrap();
    assert_eq!(alert["severity"], "bad");
    assert_eq!(
        alert["text"],
        "db: 3 findings (email-real-domain ×1, card-number ×2) — agents blocked"
    );
    let db = e["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "db")
        .unwrap();
    assert_eq!(db["badges"][0]["text"], "data not clean");
    assert_eq!(
        db["badges"][0]["title"],
        "3 findings (email-real-domain in customers.email ×1, card-number in payments.card ×2)"
    );
    assert!(find_action(&db["actions"], "data-safety-check").is_some());
    let banner = e["banners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "data-safety")
        .expect("agents-blocked banner");
    assert_eq!(banner["tone"], "bad");
    assert_eq!(
        banner["title"],
        "Agents are blocked on app/0: its data is not known to be scrubbed"
    );
    assert_eq!(banner["actions"][0]["id"], "data-safety-ack");
    // The job's result keeps the verdict, counts only.
    let id = fs::read_dir(records::jobs_dir(&p.work()))
        .unwrap()
        .flatten()
        .find_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".result.json").map(str::to_string)
        })
        .unwrap();
    let result = result_of(&p.dir, &id);
    assert_eq!(result.data_safety["db"]["state"], "not-clean");
    assert_eq!(
        result.data_safety["db"]["verdict"]["findings"][1]["count"],
        2
    );
}

#[test]
fn a_clean_check_shows_the_marker_and_lifts_nothing_it_did_not_need_to() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let (code, pane) = check(&src, &f.world, &p.dir);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(
        pane.contains("✓ db: clean (3 values checked), scrub marker 2026-11"),
        "{pane}"
    );
    let e = env(&src);
    assert_eq!(e["dataSafety"]["state"], "clean");
    assert_eq!(e["dataSafety"]["blocksAgents"], false);
    assert!(ack_action(&e).is_none(), "nothing to acknowledge");
    assert!(e["banners"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b["id"] != "data-safety"));
    let rows: Vec<String> = facts_section(&e, "data-safety")["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}={}",
                r["label"].as_str().unwrap(),
                r["value"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            "State=clean".to_string(),
            "Policy=on findings: block-agents · unscanned: warn".into(),
            format!("Last scan={NOW}"),
            "Marker=scrubbed · version 2026-11 · at 2026-11-01T00:00:00Z".into(),
            format!("Rules=hash {}", &rules_hash()[..12]),
            "db=clean (3 values checked), scrub marker 2026-11".into(),
        ]
    );
    let store = CheckStore::load(&p.work(), "app-0").unwrap();
    let entry = &store.components["db"];
    assert_eq!(entry.trigger, "manual");
    assert_eq!(entry.scanned_utc, NOW);
}

// ------------------------------------------------------- the wire block

#[test]
fn the_wire_block_in_every_state() {
    let mut states = serde_json::Map::new();
    // unscanned: db runs and has never been checked
    let p = project("block-agents", "warn", "");
    // The temp folder is masked wherever it shows (the project, and the
    // paths an error reason names), and with it the separator after it.
    let dir = p.dir.display().to_string();
    let mask = |mut v: Value| {
        assert!(v["project"].is_string(), "{v}");
        v["project"] = "<project>".into();
        if v["rulesHash"].is_string() {
            assert_eq!(v["rulesHash"], rules_hash());
            v["rulesHash"] = "<rules hash>".into();
        }
        for r in v["reasons"].as_array_mut().unwrap() {
            let text = r["text"]
                .as_str()
                .unwrap()
                .replace(&dir, "<project>")
                .replace("<project>\\", "<project>/");
            r["text"] = text.into();
        }
        v
    };
    let f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    states.insert("unscanned".into(), mask(env(&src)["dataSafety"].clone()));

    // clean
    check(&src, &f.world, &p.dir);
    states.insert("clean".into(), mask(env(&src)["dataSafety"].clone()));

    // not clean
    set_hooks(&f, MARKER_OK, &dirty_sample(&[CARD_1]));
    check(&src, &f.world, &p.dir);
    let e = env(&src);
    states.insert("not-clean".into(), mask(e["dataSafety"].clone()));

    // acknowledged
    let a = ack_action(&e).unwrap();
    let (status, body) = actor(
        &src,
        &ack_body(&a, Some("fixture rows, known"), true),
        Some("dev-1"),
    );
    assert_eq!(status, 200, "{body}");
    states.insert("acknowledged".into(), mask(env(&src)["dataSafety"].clone()));

    // error: the rules file no longer loads
    fs::write(p.dir.join("stack/rules.yml"), "rules:\n  - id: broken\n").unwrap();
    check(&src, &f.world, &p.dir);
    states.insert("error".into(), mask(env(&src)["dataSafety"].clone()));

    // the definition cannot be read at all
    fs::write(p.dir.join("chappa.yml"), "stacks: [").unwrap();
    states.insert("unreadable".into(), mask(env(&src)["dataSafety"].clone()));

    let got = Value::Object(states);
    let snap =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/data_safety_states.json");
    let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        fs::write(&snap, &text).unwrap();
    }
    let want = fs::read_to_string(&snap)
        .expect("snapshot missing; run with UPDATE_SNAPSHOTS=1")
        .replace("\r\n", "\n");
    assert_eq!(
        text, want,
        "the block moved; rerun with UPDATE_SNAPSHOTS=1 and review"
    );
}

/// `bound` names the slot's active item and the folders agents would work
/// in for it (the item folder, then each worktree, absolute); with no
/// active item it is null and `project` is what the block covers.
#[test]
fn the_block_names_the_bound_item_and_its_folders() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(ds["bound"], Value::Null);
    let project_dir = PathBuf::from(ds["project"].as_str().unwrap());
    assert!(project_dir.is_absolute(), "{project_dir:?}");
    assert_eq!(
        fs::canonicalize(&project_dir).unwrap(),
        fs::canonicalize(&p.dir).unwrap()
    );
    assert_eq!(
        ds["reasons"],
        json!([{ "component": "db", "text": "db has a data-safety check that has not run since it started" }])
    );

    let work = p.work();
    let item = work.join("ABC-1");
    fs::create_dir_all(item.join("src/api-1a2b3c4d")).unwrap();
    fs::write(
        item.join("item.json"),
        json!({
            "ref": { "source": "tracker", "key": "ABC-1" },
            "key": "ABC-1",
            "repos": { "api": { "branch": "task/ABC-1-fix", "path": "src/api-1a2b3c4d", "c8": "1a2b3c4d", "base": "main", "createdUtc": NOW } },
            "createdUtc": NOW,
            "updatedUtc": NOW,
        })
        .to_string(),
    )
    .unwrap();
    bind(&p.dir, 0, "ABC-1", "app-0");
    check(&src, &f.world, &p.dir);
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(ds["blocksAgents"], true);
    assert_eq!(
        (ds["bound"]["source"].as_str(), ds["bound"]["key"].as_str()),
        (Some("tracker"), Some("ABC-1"))
    );
    let folders: Vec<PathBuf> = ds["bound"]["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| PathBuf::from(f.as_str().unwrap()))
        .collect();
    assert_eq!(folders.len(), 2, "{ds}");
    assert!(folders.iter().all(|f| f.is_absolute()), "{folders:?}");
    assert_eq!(
        fs::canonicalize(&folders[0]).unwrap(),
        fs::canonicalize(&item).unwrap()
    );
    assert_eq!(
        fs::canonicalize(&folders[1]).unwrap(),
        fs::canonicalize(item.join("src/api-1a2b3c4d")).unwrap()
    );
    assert_eq!(ds["reasons"][0]["component"], "db");
    assert_eq!(
        ds["reasons"],
        json!([{ "component": "db", "text": "db: 2 findings (email-real-domain ×1, card-number ×1)" }])
    );

    // An item whose record is gone is still named, by its folder.
    fs::remove_file(item.join("item.json")).unwrap();
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(ds["bound"]["source"], Value::Null);
    assert_eq!(ds["bound"]["key"], "ABC-1");
    assert_eq!(ds["bound"]["folders"].as_array().unwrap().len(), 1);
}

/// A binding read from the work-folder file an older version kept (which
/// anything working in the project can write) is shown as such, and the
/// blocks do not narrow to the folders it names: they cover the project.
#[test]
fn a_binding_from_the_work_folder_file_is_shown_and_narrows_no_block() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    let item = p.work().join("ABC-1");
    fs::create_dir_all(item.join("src/api-1a2b3c4d")).unwrap();
    fs::write(
        p.work().join("active.json"),
        json!({ "slots": { "0": { "item": "ABC-1", "instance": "app-0", "sinceUtc": NOW } } })
            .to_string(),
    )
    .unwrap();
    check(&src, &f.world, &p.dir);
    let e = env(&src);
    let ds = &e["dataSafety"];
    assert_eq!(ds["blocksAgents"], true, "{ds}");
    assert_eq!(ds["bound"], Value::Null, "covers the project: {ds}");
    let banner = e["banners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "binding-from-work-folder")
        .unwrap_or_else(|| panic!("no banner: {}", e["banners"]));
    assert!(
        banner["title"].as_str().unwrap().contains("bound to ABC-1"),
        "{banner}"
    );
    // Recorded in the app's storage, it is the binding, and the file is
    // never written.
    bind(&p.dir, 0, "ABC-1", "app-0");
    let e = env(&src);
    assert_eq!(e["dataSafety"]["bound"]["key"], "ABC-1");
    assert!(!e["banners"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "binding-from-work-folder"));
}

#[test]
fn blocks_agents_over_every_policy_and_state() {
    let rules = DataRules::parse(RULES, "rules.yml").unwrap();
    let clean_sample = clean_sample();
    let dirty_sample = dirty_sample(&[CARD_1]);
    let clean = evaluate(
        Some(&rules),
        HookRun::Stdout(MARKER_OK),
        HookRun::Stdout(&clean_sample),
    )
    .unwrap();
    let dirty = evaluate(
        Some(&rules),
        HookRun::Stdout(MARKER_OK),
        HookRun::Stdout(&dirty_sample),
    )
    .unwrap();
    assert!(clean.clean && !dirty.clean);
    let entry = |verdict: Option<_>, error: Option<&str>, acked: bool| CheckEntry {
        scanned_utc: NOW.into(),
        trigger: "manual".into(),
        job_id: None,
        ack: match (&verdict, acked) {
            (Some(v), true) => Some(acknowledge(v, "dev-1", "known", NOW)),
            _ => None,
        },
        verdict,
        error: error.map(str::to_string),
    };
    let states = [
        SafetyState::Clean,
        SafetyState::NotClean,
        SafetyState::Acknowledged,
        SafetyState::Unscanned,
        SafetyState::Error,
    ];
    let mut rows = 0;
    for on_finding in ["warn", "block-agents", "stop", "block-up"] {
        for unscanned in ["warn", "block-agents"] {
            let p = project(on_finding, unscanned, "");
            let loaded = Loaded::load(&p.dir, None, 0).unwrap();
            for state in states {
                let mut store = CheckStore::default();
                match state {
                    SafetyState::Clean => {
                        store
                            .components
                            .insert("db".into(), entry(Some(clean.clone()), None, false));
                    }
                    SafetyState::NotClean => {
                        store
                            .components
                            .insert("db".into(), entry(Some(dirty.clone()), None, false));
                    }
                    SafetyState::Acknowledged => {
                        store
                            .components
                            .insert("db".into(), entry(Some(dirty.clone()), None, true));
                    }
                    SafetyState::Error => {
                        store
                            .components
                            .insert("db".into(), entry(None, Some("rules"), false));
                    }
                    SafetyState::Unscanned => {}
                }
                let s = data_safety::summarize(&loaded, &|c| c == "db", &Ok(store), None);
                let want = match state {
                    SafetyState::Clean | SafetyState::Acknowledged => false,
                    SafetyState::NotClean | SafetyState::Error => on_finding != "warn",
                    SafetyState::Unscanned => unscanned == "block-agents",
                };
                let want_policy = match state {
                    SafetyState::Clean | SafetyState::Acknowledged => None,
                    SafetyState::NotClean | SafetyState::Error => Some(on_finding),
                    SafetyState::Unscanned => Some(unscanned),
                };
                let w = s.wire();
                assert_eq!(s.state, state, "{on_finding}/{unscanned}/{state:?}");
                assert_eq!(w["state"], state.as_str());
                assert_eq!(
                    w["blocksAgents"], want,
                    "{on_finding}/{unscanned}/{state:?}"
                );
                assert_eq!(
                    w["policy"],
                    json!(want_policy),
                    "{on_finding}/{unscanned}/{state:?}"
                );
                rows += 1;
            }
        }
    }
    assert_eq!(rows, 40);

    // Across components the strongest policy wins: a not-clean db under
    // `warn` and an unscanned cache under `unscanned: block-agents`.
    let p = project("warn", "block-agents", CACHE);
    let loaded = Loaded::load(&p.dir, None, 0).unwrap();
    let mut store = CheckStore::default();
    store
        .components
        .insert("db".into(), entry(Some(dirty.clone()), None, false));
    let s = data_safety::summarize(&loaded, &|_| true, &Ok(store), None);
    let w = s.wire();
    assert_eq!(w["state"], "not-clean");
    assert_eq!(w["policy"], "block-agents");
    assert_eq!(w["blocksAgents"], true);
    assert_eq!(w["unscanned"], json!(["cache"]));

    // A store that cannot be read is an error, not clean.
    let p = project("block-agents", "warn", "");
    let loaded = Loaded::load(&p.dir, None, 0).unwrap();
    let s = data_safety::summarize(&loaded, &|_| true, &Err("bad".into()), None);
    assert_eq!(s.state, SafetyState::Error);
    assert!(s.blocks_agents());
    // A checked component that is not running and was never checked has
    // nothing to say.
    let s = data_safety::summarize(&loaded, &|_| false, &Ok(CheckStore::default()), None);
    assert_eq!(s.state, SafetyState::Clean);
    assert!(s.components.is_empty());
}

// ---------------------------------------------------------- acknowledge

/// Verdicts and acknowledgements are read from the app's own storage
/// only: a store written into the work folder (by anything working in the
/// project, or kept by an older version) counts for nothing. An old
/// acknowledgement that matches today's verdict is shown, to be confirmed
/// again with one acknowledgement.
#[test]
fn a_verdict_or_acknowledgement_written_into_the_work_folder_opens_no_gate() {
    let p = project("block-agents", "block-agents", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    check(&src, &f.world, &p.dir);
    let a = ack_action(&env(&src)).expect("offered while not clean");
    let (status, body) = actor(&src, &ack_body(&a, Some("known"), true), Some("human:alex"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(env(&src)["dataSafety"]["state"], "acknowledged");
    // The acknowledged store, as if it were in the work folder (where an
    // older version kept it, and where anything can write), and none in
    // the app's storage.
    let stored = data_safety::store_path(&p.work(), "app-0");
    let legacy = data_safety::legacy_store_path(&p.work(), "app-0");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::copy(&stored, &legacy).unwrap();
    fs::remove_file(&stored).unwrap();
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(
        (ds["state"].as_str(), ds["blocksAgents"].as_bool()),
        (Some("unscanned"), Some(true)),
        "{ds}"
    );
    // Checked again: the same findings, not acknowledged, the old
    // acknowledgement named.
    check(&src, &f.world, &p.dir);
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(
        (ds["state"].as_str(), ds["blocksAgents"].as_bool()),
        (Some("not-clean"), Some(true)),
        "{ds}"
    );
    assert_eq!(ds["ackedBefore"], json!(["db"]), "{ds}");
    assert!(
        ds["reasons"]
            .to_string()
            .contains("db: acknowledged before this update — confirm again"),
        "{ds}"
    );
    let a = ack_action(&env(&src)).expect("offered while not clean");
    let (status, body) = actor(&src, &ack_body(&a, Some("again"), true), Some("human:alex"));
    assert_eq!(status, 200, "{body}");
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(ds["state"], "acknowledged", "{ds}");
    assert!(ds.get("ackedBefore").is_none(), "{ds}");
}

#[test]
fn acknowledging_needs_a_confirm_and_a_note_and_ends_on_a_count_change() {
    let p = project("block-agents", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    check(&src, &f.world, &p.dir);
    // Clean: not offered, and refused if sent anyway.
    assert!(ack_action(&env(&src)).is_none());
    let fake = json!({ "params": { "fingerprint": "x" } });
    let (status, body) = actor(&src, &ack_body(&fake, Some("why"), true), None);
    assert_eq!((status, body["kind"].as_str()), (409, Some("not-offered")));

    set_hooks(&f, MARKER_OK, &dirty_sample(&[CARD_1]));
    check(&src, &f.world, &p.dir);
    let e = env(&src);
    let a = ack_action(&e).expect("offered while not clean");
    assert_eq!(a["kind"], "endpoint");
    assert_eq!(a["mutates"], true);
    assert_eq!(a["note"], "required");
    assert_eq!(
        a["confirm"],
        "Acknowledge what the data-safety check found on app/0: db: 2 findings \
         (email-real-domain in customers.email ×1, card-number in payments.card ×1). \
         Agents may be pointed at it again until a check finds anything different."
    );

    // Unconfirmed, without a note, with a blank note, or stale: refused.
    let (status, body) = actor(&src, &ack_body(&a, Some("known"), false), None);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("confirmation-required"))
    );
    let (status, body) = actor(&src, &ack_body(&a, None, true), None);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("note-required"))
    );
    let (status, body) = actor(&src, &ack_body(&a, Some("  \n "), true), None);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("note-required"))
    );
    let mut stale = a.clone();
    stale["params"]["fingerprint"] = "0000".into();
    let (status, body) = actor(&src, &ack_body(&stale, Some("known"), true), None);
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    assert!(body["message"].as_str().unwrap().contains("card-number"));
    assert!(
        CheckStore::load(&p.work(), "app-0").unwrap().components["db"]
            .ack
            .is_none()
    );

    // Recorded: who (the actor header), the note, the time.
    let (status, body) = actor(
        &src,
        &ack_body(&a, Some("fixture rows, known"), true),
        Some("dev-1"),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["message"],
        "Acknowledged the findings of 1 component on app/0"
    );
    let e = env(&src);
    let ds = &e["dataSafety"];
    assert_eq!(ds["state"], "acknowledged");
    assert_eq!(ds["blocksAgents"], false);
    assert_eq!(ds["policy"], Value::Null);
    assert_eq!(
        ds["ack"],
        json!({ "by": "dev-1", "note": "fixture rows, known", "atUtc": NOW })
    );
    assert!(ack_action(&e).is_none(), "nothing left to acknowledge");
    assert!(e["banners"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b["id"] != "data-safety"));

    // The same verdict again keeps it.
    let (_, pane) = check(&src, &f.world, &p.dir);
    assert!(pane.contains("(still acknowledged by dev-1)"), "{pane}");
    assert_eq!(env(&src)["dataSafety"]["state"], "acknowledged");

    // One more matching card: a different count ends it.
    set_hooks(&f, MARKER_OK, &dirty_sample(&[CARD_1, CARD_2]));
    check(&src, &f.world, &p.dir);
    let ds = env(&src)["dataSafety"].clone();
    assert_eq!(ds["state"], "not-clean");
    assert_eq!(ds["ack"], Value::Null);
    assert_eq!(ds["blocksAgents"], true);
    assert!(
        CheckStore::load(&p.work(), "app-0").unwrap().components["db"]
            .ack
            .is_none()
    );

    // Without an actor header the app is named.
    let a = ack_action(&env(&src)).unwrap();
    let (status, _) = actor(&src, &ack_body(&a, Some("known"), true), None);
    assert_eq!(status, 200);
    assert_eq!(env(&src)["dataSafety"]["ack"]["by"], "the app");
}

// ------------------------------------------------------------- schedule

fn check_jobs(work: &Path) -> Vec<JobRecord> {
    let mut out: Vec<JobRecord> = records::record_ids(work)
        .into_iter()
        .filter_map(|id| JobRecord::load(work, &id).ok().flatten())
        .filter(|r| r.action == "data-safety-check")
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[test]
fn every_interval_checks_run_on_the_background_thread_by_the_clock() {
    let p = project("block-agents", "warn", "");
    let mut f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let clock = ManualClock::at(NOW);
    f.world.clock = clock.clone();
    let bg = Background::new();
    let src = Source::with_background(
        f.world.clone(),
        ProjectConfig::new(&p.dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
        bg.clone(),
    );
    let settled = |n: usize| {
        eventually(|| {
            let jobs = check_jobs(&p.work());
            jobs.len() == n && jobs.iter().all(|j| j.status.finished())
        })
    };

    // Never checked and running: due at once, run headless as a job.
    bg.refresh_once(&f.world);
    assert!(settled(1), "{:?}", check_jobs(&p.work()));
    let first = &check_jobs(&p.work())[0];
    assert!(first.headless && !first.mutates);
    assert_eq!(first.status, JobStatus::Done);
    let store = CheckStore::load(&p.work(), "app-0").unwrap();
    assert_eq!(store.components["db"].trigger, "every");
    assert_eq!(store.components["db"].scanned_utc, NOW);

    // Within the interval: nothing new, however often the thread looks.
    for _ in 0..3 {
        bg.refresh_once(&f.world);
    }
    clock.advance(Duration::from_secs(3600));
    bg.refresh_once(&f.world);
    clock.advance(Duration::from_secs(5 * 3600 - 1));
    bg.refresh_once(&f.world);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(check_jobs(&p.work()).len(), 1);

    // Six hours after the last check: due again.
    clock.advance(Duration::from_secs(61));
    bg.refresh_once(&f.world);
    assert!(settled(2), "{:?}", check_jobs(&p.work()));
    let store = CheckStore::load(&p.work(), "app-0").unwrap();
    assert_eq!(store.components["db"].scanned_utc, "2026-11-14T18:01:00Z");
    let marker_calls = f
        .cli
        .with(|c| c.calls_matching(&["exec", "/scripts/marker.sh"]).len());
    assert_eq!(marker_calls, 2);

    // Not running: nothing is started, even when due.
    let p2 = project("block-agents", "warn", "");
    let f2 = world_at(docker_facts(FakeCli::new()).on(&["compose", "ps"], ""));
    let bg2 = Background::new();
    let _src2 = Source::with_background(
        f2.world.clone(),
        ProjectConfig::new(&p2.dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
        bg2.clone(),
    );
    bg2.refresh_once(&f2.world);
    std::thread::sleep(Duration::from_millis(100));
    assert!(check_jobs(&p2.work()).is_empty());
    drop(src);
}

// ------------------------------------------------- enforcement in jobs

/// An engine whose answers change once a `compose up` has run: before,
/// nothing exists; after, db runs. Steps and quick calls share it.
struct Switching {
    before: Arc<Locked<FakeCli>>,
    after: Arc<Locked<FakeCli>>,
    up: AtomicBool,
}

impl EngineCli for Switching {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        if inv.args.iter().any(|a| a == "up") {
            self.up.store(true, Ordering::SeqCst);
        }
        if self.up.load(Ordering::SeqCst) {
            self.after.run(inv)
        } else {
            self.before.run(inv)
        }
    }
}

fn switching(after: FakeCli) -> (Fakes, Arc<Switching>) {
    let before = docker_facts(FakeCli::new()).on(&["compose", "ps"], "").on(
        &["docker", "manifest", "inspect", "--verbose", "postgres:16"],
        &format!(r#"{{"Descriptor":{{"digest":"{PG_DIGEST}","platform":{{"os":"linux"}}}}}}"#),
    );
    let sw = Arc::new(Switching {
        before: Locked::new(before),
        after: Locked::new(after),
        up: AtomicBool::new(false),
    });
    let mut f = world_at(FakeCli::new());
    f.world.cli = sw.clone();
    f.world.registry = sw.clone();
    f.world.steps = Arc::new(CliSteps(sw.clone()));
    (f, sw)
}

fn stack_up(src: &Source) -> String {
    let e = env(src);
    let up = e["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "up")
        .unwrap()
        .clone();
    let (status, body) = post(
        src,
        "/api/actions",
        &json!({ "id": "up", "target": { "view": "environment", "key": null },
                 "params": up["params"], "note": null, "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{body}");
    body["jobId"].as_str().unwrap().to_string()
}

#[test]
fn block_up_checks_before_the_instance_is_ready_and_fails_the_job() {
    let p = project("block-up", "warn", API);
    let (f, sw) = switching(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    let id = stack_up(&src);
    let mut pane = Vec::new();
    let code = run_job(&f.world, &p.dir, &id, &mut pane);
    let pane = String::from_utf8(pane).unwrap();
    assert_eq!(code, EXIT_FAILED, "{pane}");
    let order = [
        "▶ db: up db",
        "data safety: checking db (after-up)",
        "✗ db: not clean: 2 findings",
        "✗ data safety: db is not clean, so up stops before the instance is ready (policy block-up)",
        "data safety: stopping db (the volume is kept)",
        "✗ data safety: db is not clean, so up stops",
    ];
    let mut at = 0;
    for needle in order {
        let found = pane[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not after {at} in:\n{pane}"));
        at += found + needle.len();
    }
    assert!(!pane.contains("▶ api: up api"), "api never starts:\n{pane}");
    let result = result_of(&p.dir, &id);
    let api_up = result
        .steps
        .iter()
        .find(|s| s.step.contains("api") && s.step.contains("up"))
        .unwrap();
    assert_eq!(api_up.status, "skipped");
    assert_eq!(result.data_safety["db"]["state"], "not-clean");
    // db was stopped through the planner, its volume kept.
    let stops = sw.after.with(|c| c.calls_matching(&["compose", "stop"]));
    assert_eq!(stops.len(), 1, "{:?}", sw.after.with(|c| c.lines()));
    assert!(stops[0].args.iter().any(|a| a == "db"));
    let all = sw.after.with(|c| c.lines()).join("\n");
    assert!(
        !all.contains("down") && !all.contains("--volumes") && !all.contains(" -v"),
        "{all}"
    );
    let intent = IntentRecord::load(&p.work(), "app-0").unwrap().unwrap();
    assert_eq!(intent.components["db"].desired, Desired::Stopped);
    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    assert_eq!(rec.status, JobStatus::Failed);

    // A clean db comes up and the rest follows.
    let p = project("block-up", "warn", API);
    let (f, _) = switching(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let id = stack_up(&src);
    let mut pane = Vec::new();
    let code = run_job(&f.world, &p.dir, &id, &mut pane);
    let pane = String::from_utf8(pane).unwrap();
    assert_eq!(code, EXIT_DONE, "{pane}");
    let check = pane.find("✓ db: clean").expect("checked");
    let api = pane.find("▶ api: up api").expect("api started");
    assert!(check < api, "the check gates what follows:\n{pane}");
}

#[test]
fn after_up_checks_once_the_steps_are_done_and_warn_only_says_so() {
    let p = project("warn", "warn", API);
    let (f, _) = switching(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    let id = stack_up(&src);
    let mut pane = Vec::new();
    let code = run_job(&f.world, &p.dir, &id, &mut pane);
    let pane = String::from_utf8(pane).unwrap();
    assert_eq!(code, EXIT_DONE, "warn never fails the up:\n{pane}");
    let api = pane.find("▶ api: up api").unwrap();
    let check = pane.find("data safety: checking db (after-up)").unwrap();
    assert!(api < check, "{pane}");
    let ds = &env(&src)["dataSafety"];
    assert_eq!(ds["state"], "not-clean");
    assert_eq!(ds["policy"], "warn");
    assert_eq!(ds["blocksAgents"], false);
}

#[test]
fn stop_stops_a_component_whose_data_is_not_clean() {
    let p = project("stop", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &dirty_sample(&[CARD_1])));
    let src = source(f.world.clone(), &p.dir);
    let (code, pane) = check(&src, &f.world, &p.dir);
    assert_eq!(code, EXIT_FAILED, "{pane}");
    assert!(
        pane.contains("data safety: stopping db (the volume is kept)"),
        "{pane}"
    );
    let stops = f
        .cli
        .with(|c| c.calls_matching(&["compose", "-p", PROJECT, "stop"]));
    assert_eq!(stops.len(), 1, "{:?}", f.cli.with(|c| c.lines()));
    assert!(stops[0].args.iter().any(|a| a == "db"));
    let all = f.cli.with(|c| c.lines()).join("\n");
    assert!(
        !all.contains(" down") && !all.contains("--volumes"),
        "{all}"
    );
    let ds = &env(&src)["dataSafety"];
    assert_eq!(ds["policy"], "stop");
    assert_eq!(ds["blocksAgents"], true);
    let alert = env(&src)["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["componentId"] == "db" && a["severity"] == "bad")
        .unwrap()["text"]
        .clone();
    assert!(
        alert
            .as_str()
            .unwrap()
            .ends_with("— stopped, agents blocked"),
        "{alert}"
    );
    // A clean check stops nothing.
    let p = project("stop", "warn", "");
    let f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let (code, _) = check(&src, &f.world, &p.dir);
    assert_eq!(code, EXIT_DONE);
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["compose", "stop"]))
        .is_empty());
}

#[test]
fn the_check_action_is_offered_only_for_running_checked_components() {
    let p = project("block-agents", "warn", CACHE);
    let f = world_at(running_cli(Ok(MARKER_OK), &clean_sample()));
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    assert!(find_action(
        &facts_section(&e, "data-safety")["actions"],
        "data-safety-check"
    )
    .is_some());
    let comp = |id: &str| {
        e["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
            .clone()
    };
    let db = comp("db");
    let check = find_action(&db["actions"], "data-safety-check").unwrap();
    assert_eq!(check["kind"], "endpoint");
    assert_eq!(check["mutates"], false);
    assert!(check.get("confirm").is_none());
    let cache = comp("cache");
    assert!(find_action(&cache["actions"], "data-safety-check").is_none());
    assert_eq!(cache["badges"][0]["text"], "no data-safety check");
    let body = |key: &str| {
        json!({ "id": "data-safety-check", "target": { "view": "environment", "key": key },
                "params": null, "note": null, "confirmed": false })
    };
    let (status, reply) = post(&src, "/api/actions", &body("cache"), true);
    assert_eq!((status, reply["kind"].as_str()), (409, Some("not-offered")));
    // db not running: not offered either.
    let p = project("block-agents", "warn", "");
    let f = world_at(docker_facts(FakeCli::new()).on(&["compose", "ps"], ""));
    let src = source(f.world.clone(), &p.dir);
    let e = env(&src);
    assert!(facts_section(&e, "data-safety").get("actions").is_none());
    let (status, _) = post(&src, "/api/actions", &body("db"), true);
    assert_eq!(status, 409);
}

// --------------------------------------------------------------- no leak

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                files_under(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}

#[test]
fn no_sampled_value_reaches_the_payload_the_logs_or_any_file() {
    let values = [
        CARD_1, CARD_2, EMAIL_1, EMAIL_2, FIXTURE_1, FIXTURE_2, "TEST-1",
    ];
    let p = project("stop", "warn", API);
    let leaky_stderr = format!("ERROR: row {EMAIL_2} / {CARD_2} failed");
    let after = running_cli(Err(&leaky_stderr), &dirty_sample(&[CARD_1, CARD_2])).failing(
        &["exec", "/scripts/other.sh"],
        1,
        &leaky_stderr,
    );
    let (f, sw) = switching(after);
    let src = source(f.world.clone(), &p.dir);
    let mut panes = String::new();
    // up (after-up check, stop policy), then manual checks with an
    // acknowledgement in between.
    let id = stack_up(&src);
    let mut pane = Vec::new();
    run_job(&f.world, &p.dir, &id, &mut pane);
    panes.push_str(&String::from_utf8(pane).unwrap());
    sw.after.0.lock().unwrap().add(
        &["exec", CONTAINER, "/scripts/marker.sh"],
        CliOutput::success(MARKER_OK),
    );
    let e = env(&src);
    if let Some(a) = ack_action(&e) {
        let (status, _) = actor(&src, &ack_body(&a, Some("reviewed"), true), Some("dev-1"));
        assert_eq!(status, 200);
    }
    let (_, pane) = check(&src, &f.world, &p.dir);
    panes.push_str(&pane);
    sw.after.0.lock().unwrap().add(
        &["exec", CONTAINER, "/scripts/sample.sh"],
        CliOutput::success(&sample(&[EMAIL_1, EMAIL_2, FIXTURE_2], &[CARD_2])),
    );
    let (_, pane) = check(&src, &f.world, &p.dir);
    panes.push_str(&pane);
    let payload = env(&src).to_string();

    let mut files = Vec::new();
    files_under(&p.work(), &mut files);
    files_under(gate(&p.work()).dir(), &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("data-safety.json")),
        "{files:?}"
    );
    assert!(
        files
            .iter()
            .filter(|f| f.to_string_lossy().ends_with(".log"))
            .count()
            >= 3
    );
    let mut texts = vec![
        ("payload".to_string(), payload),
        ("panes".to_string(), panes),
    ];
    for path in &files {
        texts.push((
            path.display().to_string(),
            String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned(),
        ));
    }
    for (name, text) in &texts {
        for v in values {
            assert!(!text.contains(v), "`{v}` leaked into {name}:\n{text}");
        }
        for piece in ["4111 1111", "5500 0000", "customers.invalid", "@example."] {
            assert!(!text.contains(piece), "`{piece}` leaked into {name}");
        }
    }
    // It did find them: counts made it through.
    let store = CheckStore::load(&p.work(), "app-0").unwrap();
    let v = store.components["db"].verdict.as_ref().unwrap();
    assert!(!v.clean && !v.findings.is_empty(), "{v:?}");
}
