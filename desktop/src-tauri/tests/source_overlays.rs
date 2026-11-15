//! One work item fed by two sources, end to end against the example source
//! in `desktop/sources/fixture` run twice: once as the tracker that owns the
//! items (`data/`), once as a second source that adds to them and offers
//! item actions on the first one's rows (`data-overlay/`).
//!
//! What is pinned: the second source's item actions are part of what its
//! trust covers; they are offered on the owner's rows (with the confirm
//! text filled from the row) except where the second source already adds
//! to the row; the webview cannot run an id the manifest does not declare
//! for that row; and every call's `target` names the owner and carries the
//! owner's row exactly as received (the fixture echoes the call body), or
//! leaves the row out when the owner does not list it.
//!
//! The fixture is a Node script; the test skips itself (and says so) when
//! `node` is not on PATH.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_lib::agent_tools::AgentToolsState;
use app_lib::agents::FakeDocker;
use app_lib::coordination::Coordination;
use app_lib::projects::Projects;
use app_lib::records::Records;
use app_lib::registry::Registry;
use app_lib::settings::SettingsState;
use app_lib::source_actions::{run_action, run_item_action, ActionDeps, ActionOutcome};
use app_lib::sources::identity::{ADDR_ENV, TOKEN_ENV};
use app_lib::sources::{SourceStatus, Sources, SourcesEmitter};
use app_lib::workspace::Workspace;
use project_model::projects::ProjectStore;
use project_model::workspace::WorkspaceStore;
use serde_json::{json, Value};

struct Quiet;

impl SourcesEmitter for Quiet {
    fn emit(&self, _: Value) {}
}

fn node_available() -> bool {
    Command::new("node").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

struct Reaper(Vec<Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture")
}

fn data(dir: &str, name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture_dir().join(dir).join(format!("{name}.json"))).unwrap()).unwrap()
}

/// The fixture's own row `key`, exactly as the file has it.
fn tracker_row(key: &str) -> Value {
    data("data", "work")["items"].as_array().unwrap().iter().find(|i| i["key"] == key).unwrap().clone()
}

/// What the echoing fixture answered: the call's target, params and note.
fn echoed(outcome: ActionOutcome) -> Value {
    match outcome {
        ActionOutcome::Done { message: Some(m) } => serde_json::from_str(&m).unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_second_source_adds_to_the_first_ones_items_and_acts_on_its_rows() {
    if !node_available() {
        eprintln!("skipping: `node` is not on PATH, the fixture source cannot run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let mut store = ProjectStore::new(dir.path().join("projects.json"));
    let project_id = store.add(root.clone(), "app".into(), None).unwrap().id;
    let projects = Projects::with_store(store);
    let registry = Registry::with_docker(FakeDocker::new());
    let workspace = Workspace::with_store(WorkspaceStore::new(dir.path().join("workspace.json")));
    let records = Records::new(projects.clone(), workspace, Coordination::in_memory().unwrap());
    let sources = Sources::default();
    sources.init_with(app_lib::sources::store::SourceStore::new(dir.path().join("sources.json")), Arc::new(Quiet));

    // Two launches of the same script with different data, in listing
    // order: the tracker first, the workspaces source second.
    let mut children = Vec::new();
    for (command, data_dir) in [("fixture-source", "data"), ("workspaces-source", "data-overlay")] {
        let launch = sources.prepare_launch(project_id, &root, command).unwrap();
        children.push(
            Command::new("node")
                .arg(fixture_dir().join("index.mjs"))
                .arg("--data")
                .arg(fixture_dir().join(data_dir))
                .env(ADDR_ENV, &launch.addr)
                .env(TOKEN_ENV, &launch.token)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn the fixture source"),
        );
        // No process row stands behind these launches (the journal is not
        // what this test is about), so the id names none.
        sources.launched(&root, command, u32::MAX);
    }
    let _reaper = Reaper(children);
    let listed = wait_for("both need review", Duration::from_secs(20), || {
        let all = sources.listing().sources;
        (all.len() == 2 && all.iter().all(|s| s.status == SourceStatus::NeedsReview)).then_some(all)
    });
    let (tracker, overlay) = (listed[0].id.clone(), listed[1].id.clone());
    assert_eq!(listed[0].manifest_id.as_deref(), Some("fixture"));
    assert_eq!(listed[1].manifest_id.as_deref(), Some("workspaces"));
    // The trust dialog lists the item actions; the hash covers them.
    let trust = listed[1].trust.clone().unwrap();
    assert_eq!(trust.item_actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["review-later", "start-work"]);
    assert_eq!(trust.item_actions[1].sources, ["fixture"]);
    // Before the tracker is trusted, nothing is offered on its rows.
    sources.trust_answer(&overlay, true, &trust.hash).unwrap();
    wait_for("the overlay polled", Duration::from_secs(20), || {
        let s = sources.listing().sources[1].clone();
        (s.status == SourceStatus::Up && s.views.iter().all(|v| v.has_payload)).then_some(())
    });
    assert!(sources.item_actions(&overlay, "work", &tracker, "ABC-3", &[], &[]).unwrap_err().contains("not trusted"));
    let deps = ActionDeps {
        registry,
        tools: AgentToolsState::with_path(dir.path().join("agent_tools.json")),
        settings: SettingsState::default(),
        projects,
        records,
        sources: sources.clone(),
        app: None,
    };
    // The overlay's own actions on a row whose owner is not trusted: the
    // call names the owner but carries no row of an untrusted source.
    let early = echoed(run_action(&deps, &overlay, "work", Some("ABC-2"), "archive", true, None).unwrap());
    assert_eq!(early["target"], json!({"view": "work", "key": "ABC-2", "source": "fixture"}));

    let hash = listed[0].trust.clone().unwrap().hash;
    sources.trust_answer(&tracker, true, &hash).unwrap();
    wait_for("the tracker polled", Duration::from_secs(20), || {
        let s = sources.listing().sources[0].clone();
        (s.status == SourceStatus::Up && s.views.iter().all(|v| v.has_payload)).then_some(())
    });

    // Offered on the tracker's rows the second source does not add to yet,
    // with the confirm filled from the row; not on the ones it adds to.
    let offered = sources.item_actions(&overlay, "work", &tracker, "ABC-3", &[], &[]).unwrap();
    assert_eq!(offered.len(), 1);
    match &offered[0].action {
        app_lib::sources::actions::SourceAction::Endpoint { id, mutates, confirm, .. } => {
            assert_eq!((id.as_str(), *mutates), ("start-work", true));
            assert_eq!(confirm.as_deref(), Some("Create a workspace for ABC-3 — Retire the old queue"));
        }
        other => panic!("{other:?}"),
    }
    assert!(offered[0].verdict.enabled);
    for key in ["ABC-1", "ABC-2"] {
        assert!(sources.item_actions(&overlay, "work", &tracker, key, &[], &[]).unwrap().is_empty(), "{key} already has the overlay's own actions");
    }
    // The tracker's untitled row reads "(no title)" in the confirm.
    let untitled = sources.item_actions(&overlay, "work", &tracker, "owner/repo#2", &[], &[]).unwrap();
    assert!(matches!(&untitled[0].action, app_lib::sources::actions::SourceAction::Endpoint { confirm: Some(c), .. } if c.ends_with("— (no title)")));
    assert!(sources.item_actions(&overlay, "work", &tracker, "ABC-404", &[], &[]).unwrap_err().contains("lists no `ABC-404`"));
    assert!(sources.item_actions(&overlay, "work", &overlay, "ABC-1", &[], &[]).unwrap_err().contains("other sources' rows only"));
    assert!(sources.item_actions(&tracker, "work", &overlay, "ABC-9", &[], &[]).unwrap_err().contains("lists no"), "an overlay is not a row its source owns");
    assert!(sources.item_actions(&tracker, "work", &overlay, "ABC-3", &[], &[]).is_err());
    let on_pr = sources.item_actions(&overlay, "prs", &tracker, "github:owner/repo:#1", &[], &[]).unwrap();
    assert_eq!(on_pr.iter().map(|a| a.action.id()).collect::<Vec<_>>(), ["review-later"]);
    assert!(sources.item_actions(&overlay, "prs", &tracker, "ABC-3", &[], &[]).unwrap_err().contains("lists no"));

    // Running one: the confirm rule, the closed list, then the call body.
    let item = |id: &str, key: &str, confirmed: bool, note: Option<&str>| run_item_action(&deps, &overlay, "work", &tracker, key, id, confirmed, note);
    assert!(item("start-work", "ABC-3", false, None).unwrap_err().contains("must be confirmed"));
    assert!(item("nuke", "ABC-3", true, None).unwrap_err().contains("offers no item action `nuke`"));
    assert!(item("start-work", "ABC-1", true, None).unwrap_err().contains("offers no item action"), "hidden where the overlay is");
    assert!(item("review-later", "ABC-3", false, None).is_err(), "declared for PR rows only");
    let sent = echoed(item("start-work", "ABC-3", true, Some("for the demo")).unwrap());
    assert_eq!(sent["target"], json!({"view": "work", "key": "ABC-3", "source": "fixture", "item": tracker_row("ABC-3")}));
    assert_eq!(sent["params"], json!({"from": "tracker", "slot": 0}));
    assert_eq!(sent["note"], "for the demo");
    let pr1 = data("data", "prs")["items"].as_array().unwrap().iter().find(|p| p["number"] == "#1").unwrap().clone();
    let sent = echoed(run_item_action(&deps, &overlay, "prs", &tracker, "github:owner/repo:#1", "review-later", false, None).unwrap());
    assert_eq!(sent["target"], json!({"view": "prs", "key": "github:owner/repo:#1", "source": "fixture", "item": pr1}));

    // The overlay's own actions (an item's and its banner's) carry the
    // owner and the owner's row; with the owner not listing the key, the
    // row is left out.
    let sent = echoed(run_action(&deps, &overlay, "work", Some("ABC-2"), "archive", true, None).unwrap());
    assert_eq!(sent["target"], json!({"view": "work", "key": "ABC-2", "source": "fixture", "item": tracker_row("ABC-2")}));
    let sent = echoed(run_action(&deps, &overlay, "work", Some("ABC-2"), "rebuild", true, None).unwrap());
    assert_eq!(sent["target"]["item"], tracker_row("ABC-2"), "a row's banner actions are the row's");
    let sent = echoed(run_action(&deps, &overlay, "work", Some("ABC-9"), "archive", true, None).unwrap());
    assert_eq!(sent["target"], json!({"view": "work", "key": "ABC-9", "source": "fixture"}));
    // The tracker's own actions on its own rows stay plain.
    assert_eq!(run_action(&deps, &tracker, "work", Some("ABC-1"), "ping", false, None).unwrap(), ActionOutcome::Done { message: Some("pong".into()) });

    sources.shutdown();
}
