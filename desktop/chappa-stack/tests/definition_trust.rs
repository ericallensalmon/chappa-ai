//! The stack definition runs only as trusted: a source started with the
//! hash the person trusted refuses every action with `409
//! untrusted-definition` once the definition hashes differently, shows
//! why on the Environment view, keeps the read-only views, runs no status
//! script in its background pool, and a job accepted under one definition
//! does not run under another.

mod common;

use serde_json::{json, Value};
use std::fs;

use chappa_stack::runjob::{run_job, EXIT_REFUSED};
use chappa_stack::server::{DefinitionPin, UNTRUSTED_KIND, UNTRUSTED_TEXT};
use common::*;
use stack_core::cli::FakeCli;
use stack_core::probe::FakeHttp;

const ENV: &str = "/api/environment";

const YML: &str = r#"
name: trust-example
stacks:
  scripted:
    components:
      app:
        driver: script
        up: [node, scripts/up.js]
        status: [node, scripts/status.js]
"#;

const STATUS: &str = r#"{"components":[{"state":"up"}]}"#;

fn hash_of(dir: &std::path::Path) -> String {
    stack_core::trust::definition_trust(dir)
        .unwrap()
        .unwrap()
        .hash
}

fn status_runs(cli: &FakeCli) -> usize {
    cli.calls_matching(&["node", "scripts/status.js"]).len()
}

fn banner_ids(env: &Value) -> Vec<String> {
    env["banners"]
        .as_array()
        .map(|b| {
            b.iter()
                .filter_map(|x| x["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_changed_definition_is_refused_shown_and_runs_nothing_in_the_background() {
    let project = Project::with(YML, &[]);
    let cli = FakeCli::new().on(&["node", "scripts/status.js"], STATUS);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &project.dir);
    src.set_definition_pin(DefinitionPin::Trusted(hash_of(&project.dir)));

    // Trusted: the status script is queued and run by the pool.
    let env = get(&src, ENV).1;
    assert!(
        !banner_ids(&env).contains(&UNTRUSTED_KIND.to_string()),
        "{env}"
    );
    src.background.refresh_once(&f.world);
    assert_eq!(f.cli.with(status_runs), 1);

    // Someone edits the status script's argv: nothing runs any more.
    fs::write(
        project.dir.join("chappa.yml"),
        YML.replace("scripts/status.js", "scripts/other.js"),
    )
    .unwrap();
    f.cli.with(|c| c.clear_calls());
    src.background.invalidate_facts();
    let env = get(&src, ENV).1;
    assert_eq!(
        banner_ids(&env).first().map(String::as_str),
        Some(UNTRUSTED_KIND),
        "{env}"
    );
    assert_eq!(env["banners"][0]["title"], UNTRUSTED_TEXT);
    src.background.refresh_once(&f.world);
    assert!(
        f.cli
            .with(|c| c.lines())
            .iter()
            .all(|l| !l.starts_with("node")),
        "no script ran: {:?}",
        f.cli.with(|c| c.lines())
    );
    // The read-only views keep answering.
    assert_eq!(get(&src, "/chappa/v1/manifest").0, 200);
    assert_eq!(get(&src, "/api/work").0, 200);
    // Every action is refused before anything runs.
    let (code, body) = post(
        &src,
        "/api/actions",
        &json!({"id": "up", "target": {"view": "environment", "key": "app"}, "confirmed": true}),
        true,
    );
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["kind"], UNTRUSTED_KIND);
    let (code, body) = post(
        &src,
        "/api/actions",
        &json!({"id": "start-work", "target": {"view": "work", "key": "ABC-1"}, "confirmed": true}),
        true,
    );
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["kind"], UNTRUSTED_KIND);
    assert!(!src.secret_scan_pass(), "no rescan while untrusted");
    assert!(src.data_safety_tick().is_empty());

    // Trusting the new definition (the app relaunches with its hash) runs it.
    src.set_definition_pin(DefinitionPin::Trusted(hash_of(&project.dir)));
    let env = get(&src, ENV).1;
    assert!(
        !banner_ids(&env).contains(&UNTRUSTED_KIND.to_string()),
        "{env}"
    );
    src.background.refresh_once(&f.world);
    assert_eq!(
        f.cli
            .with(|c| c.calls_matching(&["node", "scripts/other.js"]).len()),
        1
    );
}

#[test]
fn nothing_trusted_means_read_only() {
    let project = Project::with(YML, &[]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &project.dir);
    src.set_definition_pin(DefinitionPin::parse("none").unwrap());
    let env = get(&src, ENV).1;
    assert_eq!(
        banner_ids(&env).first().map(String::as_str),
        Some(UNTRUSTED_KIND)
    );
    src.background.refresh_once(&f.world);
    assert!(f
        .cli
        .with(|c| c.lines())
        .iter()
        .all(|l| !l.starts_with("node")));
    assert!(DefinitionPin::parse("not-a-hash").is_err());
    assert_eq!(
        DefinitionPin::parse(&"AB".repeat(32)).unwrap(),
        DefinitionPin::Trusted("ab".repeat(32))
    );
}

#[test]
fn a_job_accepted_under_one_definition_does_not_run_under_another() {
    let project = Project::with(YML, &[]);
    let cli = FakeCli::new().on(&["node", "scripts/status.js"], STATUS);
    let f = fakes(cli, FakeHttp::default());
    let src = source(f.world.clone(), &project.dir);
    src.set_definition_pin(DefinitionPin::Trusted(hash_of(&project.dir)));
    let env = get(&src, ENV).1;
    let offered = env["components"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "up")
        .cloned()
        .unwrap_or_else(|| panic!("no up action in {env}"));
    let mut body =
        json!({"id": "up", "target": {"view": "environment", "key": "app"}, "confirmed": true});
    if let Some(p) = offered.get("params") {
        body["params"] = p.clone();
    }
    let (code, accepted) = post(&src, "/api/actions", &body, true);
    assert_eq!(code, 202, "{accepted}");
    let job = accepted["jobId"].as_str().unwrap().to_string();
    let record = fs::read_to_string(project.work().join("jobs").join(format!("{job}.json")))
        .unwrap_or_default();
    assert!(
        record.contains(&hash_of(&project.dir)) || record.is_empty(),
        "the record keeps the trusted hash"
    );
    // The definition changes between accept and run.
    fs::write(
        project.dir.join("chappa.yml"),
        YML.replace("scripts/up.js", "scripts/evil.js"),
    )
    .unwrap();
    let mut pane = Vec::new();
    let code = run_job(&f.world, &project.dir, &job, &mut pane);
    assert_eq!(code, EXIT_REFUSED, "{}", String::from_utf8_lossy(&pane));
    assert!(String::from_utf8_lossy(&pane).contains("changed after this job was accepted"));
    assert!(f
        .cli
        .with(|c| c.calls_matching(&["node", "scripts/evil.js"]).is_empty()));
}

#[test]
fn the_work_folder_is_git_ignored_as_soon_as_the_definition_is_read() {
    let project = Project::with(YML, &[]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &project.dir);
    let _ = get(&src, ENV);
    assert_eq!(
        fs::read_to_string(project.work().join(".gitignore")).unwrap(),
        "*\n"
    );
}
