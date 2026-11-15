//! An item's local workflow through the source: Start work picks the
//! first state or the one the tracker's status maps to, moves follow the
//! workflow's list and note rules, `human` and `human-exit` gates judge
//! the `X-Chappa-Actor` header (an agent entering a `human` state waits
//! for a person's approval), rounds and history are kept in `item.json`,
//! and a tracker status that means another state puts a banner with the
//! move on the item. The status blocks are pinned as
//! `tests/snapshots/local_status.json` (`UPDATE_SNAPSHOTS=1` rewrites it).
//! git and the engines are fakes: none of this asks either.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chappa_stack::server::{Caller, Source};
use chappa_stack::world::{Locked, ManualClock};
use common::*;
use stack_core::cli::FakeCli;
use stack_core::probe::FakeHttp;
use stack_core::workspace::ItemRecord;

const WORKFLOW: &str = r#"
    workflow:
      states:
        - { id: todo,   label: To do }
        - { id: dev,    label: In development }
        - { id: review, label: In review, gate: human-exit }
        - { id: done,   label: Done,      gate: human }
      moves: { todo: [dev], dev: [review, done], review: [dev, done], done: [dev] }
      requireNote: [ "review->dev" ]
      trackers:
        tracker:
          map: { "To Do": todo, "In Progress": dev, "Code Review": review, "Done": done }
        "*":
          map: { "Doing": dev }
"#;

const STACK: &str = r#"
    groups:
      core: { engine: docker }
    components:
      db:
        group: core
        driver: compose
        file: stack/core.yml
        service: db
        modes: { prebuilt: { image: postgres, tag: "16" } }
"#;

const COMPOSE: &str = "services:\n  db:\n    image: postgres:16\n    expose: [\"5432\"]\n";
const NOW: &str = "2026-11-14T12:00:00Z";
const HUMAN: &str = "human:alex";
const AGENT: &str = "agent:a1";

struct Ws {
    p: Project,
    src: Arc<Source>,
    clock: Arc<ManualClock>,
}

fn setup_with(workflow: &str) -> Ws {
    let yml = format!("name: ws\nstacks:\n  app:{workflow}{STACK}");
    let p = Project::with(&yml, &[("stack/core.yml", COMPOSE)]);
    let mut f = fakes(
        with_docker_project(docker_facts(FakeCli::new()), "app-0-core", &[]),
        FakeHttp::default(),
    );
    // git answers nothing; no test here needs a worktree.
    f.world.git_cli = Locked::new(FakeCli::new());
    let clock = ManualClock::at(NOW);
    f.world.clock = clock.clone();
    let src = source(f.world.clone(), &p.dir);
    Ws { p, src, clock }
}

fn setup() -> Ws {
    setup_with(WORKFLOW)
}

/// The owner's row as the app sends it with every overlay call.
fn owner(key: &str, status: &str) -> Value {
    json!({ "key": key, "title": format!("Item {key}"), "status": { "label": status }, "prs": [] })
}

fn call(ws: &Ws, body: Value, actor: Option<&str>) -> (u16, Value) {
    ws.src.handle_as(
        "POST",
        "/api/actions",
        Some(&bearer()),
        &Caller {
            runs_jobs: true,
            actor: actor.map(str::to_string),
        },
        &body.to_string(),
    )
}

fn start_from(ws: &Ws, source: &str, key: &str, status: &str, actor: Option<&str>) {
    let (code, body) = call(
        ws,
        json!({
            "id": "start-work",
            "target": { "view": "work", "key": key, "source": source, "item": owner(key, status) },
            "params": {},
            "confirmed": true,
        }),
        actor,
    );
    assert_eq!(code, 202, "{body}");
}

fn start(ws: &Ws, key: &str, status: &str) {
    start_from(ws, "tracker", key, status, Some(HUMAN));
}

/// An overlay action on `key`, carrying the owner's row with `status`.
#[allow(clippy::too_many_arguments)]
fn act(
    ws: &Ws,
    key: &str,
    status: &str,
    id: &str,
    params: Value,
    note: Option<&str>,
    actor: Option<&str>,
    confirmed: bool,
) -> (u16, Value) {
    call(
        ws,
        json!({
            "id": id,
            "target": { "view": "work", "key": key, "source": "tracker", "item": owner(key, status) },
            "params": params,
            "note": note,
            "confirmed": confirmed,
        }),
        actor,
    )
}

/// Run the row's own action `id` as offered (its params), as `actor`.
fn run(ws: &Ws, key: &str, id: &str, note: Option<&str>, actor: &str) -> (u16, Value) {
    let a = action(&row(ws, key), id)
        .unwrap_or_else(|| panic!("{key} offers no {id}"))
        .clone();
    let status = rec(ws, key).status.unwrap().label;
    act(
        ws,
        key,
        &status,
        id,
        a["params"].clone(),
        note,
        Some(actor),
        true,
    )
}

fn row(ws: &Ws, key: &str) -> Value {
    get(&ws.src, "/api/work").1["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == key)
        .cloned()
        .unwrap_or_else(|| panic!("no row {key}"))
}

fn rec(ws: &Ws, key: &str) -> ItemRecord {
    ItemRecord::load(&ws.p.work().join(key)).unwrap().unwrap()
}

fn action<'a>(row: &'a Value, id: &str) -> Option<&'a Value> {
    row["actions"].as_array()?.iter().find(|a| a["id"] == id)
}

fn state(ws: &Ws, key: &str) -> String {
    rec(ws, key).workflow.unwrap().state
}

fn kind(reply: &(u16, Value)) -> (u16, &str) {
    (reply.0, reply.1["kind"].as_str().unwrap_or(""))
}

#[test]
fn no_workflow_means_no_status_block_and_no_moves() {
    let ws = setup_with("");
    start(&ws, "ABC-1", "To Do");
    let r = row(&ws, "ABC-1");
    assert!(r.get("localStatus").is_none(), "{r}");
    assert!(!r["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"].as_str().unwrap().starts_with("move:")));
    assert!(rec(&ws, "ABC-1").workflow.is_none());
    let reply = act(
        &ws,
        "ABC-1",
        "To Do",
        "move:dev",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "not-offered"));
    assert_eq!(reply.1["message"], "this stack has no workflow");
}

#[test]
fn start_work_picks_the_first_state_or_the_one_the_tracker_maps_to() {
    let ws = setup();
    start(&ws, "ABC-1", "Brand new");
    let w = rec(&ws, "ABC-1").workflow.unwrap();
    assert_eq!(
        (
            w.state.as_str(),
            w.actor.as_str(),
            w.round,
            w.since_utc.as_str()
        ),
        ("todo", HUMAN, 1, NOW)
    );
    assert_eq!(w.history.len(), 1);
    start(&ws, "ABC-2", "Code Review");
    let w = rec(&ws, "ABC-2").workflow.unwrap();
    assert_eq!((w.state.as_str(), w.actor.as_str()), ("review", "tracker"));
    // Another source's status falls back to the "*" map.
    start_from(&ws, "other", "XYZ-3", "Doing", Some(AGENT));
    assert_eq!(state(&ws, "XYZ-3"), "dev");
    // No actor header: the first state, by "unknown".
    start_from(&ws, "tracker", "ABC-4", "Elsewhere", None);
    assert_eq!(rec(&ws, "ABC-4").workflow.unwrap().actor, "unknown");
    // The block on the row says the same.
    let ls = &row(&ws, "ABC-2")["localStatus"];
    assert_eq!(
        (&ls["id"], &ls["label"], &ls["actor"], &ls["gate"]),
        (
            &json!("review"),
            &json!("In review"),
            &json!("tracker"),
            &json!("human-exit")
        )
    );
}

#[test]
fn an_item_started_before_the_workflow_existed_stands_where_it_would_have_started() {
    let ws = setup_with("");
    start(&ws, "ABC-1", "In Progress");
    let yml = fs::read_to_string(ws.p.dir.join("chappa.yml")).unwrap();
    fs::write(
        ws.p.dir.join("chappa.yml"),
        yml.replace("  app:\n", &format!("  app:{WORKFLOW}")),
    )
    .unwrap();
    let ls = &row(&ws, "ABC-1")["localStatus"];
    assert_eq!(
        (&ls["id"], &ls["actor"]),
        (&json!("dev"), &json!("tracker"))
    );
    assert!(
        rec(&ws, "ABC-1").workflow.is_none(),
        "nothing written by a read"
    );
    assert_eq!(run(&ws, "ABC-1", "move:review", None, AGENT).0, 200);
    let w = rec(&ws, "ABC-1").workflow.unwrap();
    assert_eq!(w.state, "review");
    assert_eq!(w.history.len(), 2);
}

#[test]
fn moves_follow_the_list_need_their_notes_and_must_be_confirmed() {
    let ws = setup();
    start(&ws, "ABC-1", "To Do");
    let r = row(&ws, "ABC-1");
    let mv = action(&r, "move:dev").unwrap();
    assert_eq!(
        mv,
        &json!({
            "kind": "endpoint", "id": "move:dev", "label": "Move to In development",
            "mutates": true, "confirm": "Move ABC-1 from To do to In development",
            "params": { "from": "todo", "to": "dev" }, "note": "optional"
        })
    );
    // Unconfirmed: refused, nothing moves.
    let reply = act(
        &ws,
        "ABC-1",
        "To Do",
        "move:dev",
        mv["params"].clone(),
        None,
        Some(HUMAN),
        false,
    );
    assert_eq!(kind(&reply), (409, "confirmation-required"));
    assert_eq!(state(&ws, "ABC-1"), "todo");
    // Not one of the moves.
    let reply = act(
        &ws,
        "ABC-1",
        "To Do",
        "move:review",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "not-allowed"));
    assert_eq!(
        reply.1["message"],
        "ABC-1 cannot move from To do to In review; from To do it can move to In development"
    );
    let reply = act(
        &ws,
        "ABC-1",
        "To Do",
        "move:qa",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "not-offered"));
    // Allowed.
    let reply = run(&ws, "ABC-1", "move:dev", None, AGENT);
    assert_eq!(
        reply,
        (200, json!({ "message": "ABC-1: To do → In development" }))
    );
    // The action shown before the move is stale now.
    let reply = act(
        &ws,
        "ABC-1",
        "To Do",
        "move:dev",
        mv["params"].clone(),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "stale-plan"));
    assert_eq!(run(&ws, "ABC-1", "move:review", None, AGENT).0, 200);
    // review -> dev needs a note (the action says so too).
    let back = action(&row(&ws, "ABC-1"), "move:dev").unwrap().clone();
    assert_eq!(back["note"], "required");
    let reply = run(&ws, "ABC-1", "move:dev", None, HUMAN);
    assert_eq!(kind(&reply), (409, "note-required"));
    let reply = run(&ws, "ABC-1", "move:dev", Some("   "), HUMAN);
    assert_eq!(kind(&reply), (409, "note-required"), "blank is no note");
    ws.clock.advance(std::time::Duration::from_secs(3600));
    assert_eq!(
        run(&ws, "ABC-1", "move:dev", Some("tests fail"), HUMAN).0,
        200
    );
    let w = rec(&ws, "ABC-1").workflow.unwrap();
    let last = w.history.last().unwrap();
    assert_eq!(
        (
            last.from.as_deref(),
            last.to.as_deref(),
            last.actor.as_str(),
            last.note.as_deref(),
            last.at_utc.as_str()
        ),
        (
            Some("review"),
            Some("dev"),
            HUMAN,
            Some("tests fail"),
            "2026-11-14T13:00:00Z"
        )
    );
    assert_eq!(w.since_utc, "2026-11-14T13:00:00Z");
    // A bare `move` with `params.to` works too.
    let reply = act(
        &ws,
        "ABC-1",
        "In Progress",
        "move",
        json!({ "to": "review" }),
        None,
        Some(AGENT),
        true,
    );
    assert_eq!(reply.0, 200, "{}", reply.1);
    assert_eq!(state(&ws, "ABC-1"), "review");
}

#[test]
fn only_a_person_leaves_a_human_exit_state() {
    let ws = setup();
    start(&ws, "ABC-1", "Code Review");
    let ls = &row(&ws, "ABC-1")["localStatus"];
    assert_eq!(
        ls["allowed"],
        json!([
            { "id": "move:dev", "label": "Move to In development", "humanOnly": true, "requireNote": true },
            { "id": "move:done", "label": "Move to Done", "humanOnly": true }
        ])
    );
    for who in [Some(AGENT), None, Some("source:tracker"), Some("human:")] {
        let reply = run_as(&ws, "ABC-1", "move:done", None, who);
        assert_eq!(kind(&reply), (409, "human-only"), "{who:?}");
    }
    assert_eq!(state(&ws, "ABC-1"), "review");
    assert!(rec(&ws, "ABC-1").workflow.unwrap().pending.is_none());
    // A person: straight into done, gate and all.
    assert_eq!(run(&ws, "ABC-1", "move:done", None, HUMAN).0, 200);
    assert_eq!(state(&ws, "ABC-1"), "done");
}

fn run_as(ws: &Ws, key: &str, id: &str, note: Option<&str>, actor: Option<&str>) -> (u16, Value) {
    let a = action(&row(ws, key), id).unwrap().clone();
    let status = rec(ws, key).status.unwrap().label;
    act(ws, key, &status, id, a["params"].clone(), note, actor, true)
}

#[test]
fn an_agent_entering_a_human_state_waits_for_a_person_to_approve_or_send_back() {
    let ws = setup();
    start(&ws, "ABC-1", "In Progress");
    let reply = run(&ws, "ABC-1", "move:done", Some("all green"), AGENT);
    assert_eq!(
        reply,
        (
            200,
            json!({ "message": "ABC-1: the move to Done waits for a person's approval" })
        )
    );
    assert_eq!(state(&ws, "ABC-1"), "dev");
    let r = row(&ws, "ABC-1");
    assert_eq!(
        r["localStatus"]["pendingApproval"],
        json!({ "to": "Done", "toId": "done", "by": AGENT, "note": "all green" })
    );
    let approve = action(&r, "approve").unwrap();
    assert_eq!(
        approve["confirm"],
        "Approve moving ABC-1 from In development to Done (asked by agent:a1)"
    );
    assert_eq!(approve["params"], json!({ "to": "done" }));
    let back = action(&r, "send-back").unwrap();
    assert_eq!(
        (back["note"].as_str(), back["confirm"].as_str()),
        (
            Some("required"),
            Some(
                "Send back the move of ABC-1 to Done (asked by agent:a1); it stays In development"
            )
        )
    );
    // Not for agents, nor for a caller that names nobody.
    for id in ["approve", "send-back"] {
        for who in [Some(AGENT), None] {
            let reply = run_as(&ws, "ABC-1", id, Some("ok"), who);
            assert_eq!(kind(&reply), (409, "human-only"), "{id} {who:?}");
        }
    }
    // Send back: a note, the item stays, the decline is in the history.
    assert_eq!(
        kind(&run(&ws, "ABC-1", "send-back", None, HUMAN)),
        (409, "note-required")
    );
    assert_eq!(
        run(&ws, "ABC-1", "send-back", Some("docs missing"), HUMAN).0,
        200
    );
    let w = rec(&ws, "ABC-1").workflow.unwrap();
    assert_eq!((w.state.as_str(), w.pending.is_none()), ("dev", true));
    let ls = &row(&ws, "ABC-1")["localStatus"];
    assert_eq!(
        ls["history"].as_array().unwrap().last().unwrap(),
        &json!({ "at": NOW, "actor": HUMAN, "note": "sent back the move to Done asked by agent:a1: docs missing" })
    );
    assert!(action(&row(&ws, "ABC-1"), "approve").is_none());
    let reply = act(
        &ws,
        "ABC-1",
        "In Progress",
        "approve",
        json!({ "to": "done" }),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "not-offered"));

    // Ask again; a person approves.
    assert_eq!(
        run(&ws, "ABC-1", "move:done", Some("docs added"), AGENT).0,
        200
    );
    // An approval shown for another target is stale.
    let reply = act(
        &ws,
        "ABC-1",
        "In Progress",
        "approve",
        json!({ "to": "review" }),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "stale-plan"));
    let reply = run(&ws, "ABC-1", "approve", None, HUMAN);
    assert_eq!(
        reply,
        (
            200,
            json!({ "message": "ABC-1: In development → Done (approved; asked by agent:a1)" })
        )
    );
    let w = rec(&ws, "ABC-1").workflow.unwrap();
    assert_eq!(
        (w.state.as_str(), w.actor.as_str(), w.pending.is_none()),
        ("done", HUMAN, true)
    );
    let last = w.history.last().unwrap();
    assert_eq!(
        (
            last.actor.as_str(),
            last.requested_by.as_deref(),
            last.note.as_deref()
        ),
        (HUMAN, Some(AGENT), Some("docs added"))
    );
}

#[test]
fn rounds_count_how_often_the_item_entered_its_state() {
    let ws = setup();
    start(&ws, "ABC-1", "In Progress");
    let mut rounds = Vec::new();
    for (id, note) in [
        ("move:review", None),
        ("move:dev", Some("again")),
        ("move:review", None),
        ("move:dev", Some("still")),
        ("move:review", None),
    ] {
        assert_eq!(run(&ws, "ABC-1", id, note, HUMAN).0, 200, "{id}");
        let ls = row(&ws, "ABC-1")["localStatus"].clone();
        rounds.push((
            ls["id"].as_str().unwrap().to_string(),
            ls["round"].as_u64().unwrap(),
        ));
    }
    assert_eq!(
        rounds,
        [
            ("review", 1),
            ("dev", 2),
            ("review", 2),
            ("dev", 3),
            ("review", 3)
        ]
        .map(|(s, n)| (s.to_string(), n))
    );
    assert_eq!(rec(&ws, "ABC-1").workflow.unwrap().history.len(), 6);
}

fn banner(ws: &Ws, key: &str) -> Option<Value> {
    row(ws, key)["banners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "tracker-status")
        .cloned()
}

#[test]
fn a_tracker_status_that_means_another_state_puts_the_move_on_the_item() {
    let ws = setup();
    start(&ws, "ABC-1", "To Do");
    assert_eq!(banner(&ws, "ABC-1"), None, "the statuses agree");
    // Any overlay call carries the owner's row, refused or not.
    let reply = act(
        &ws,
        "ABC-1",
        "Code Review",
        "nothing",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(kind(&reply), (409, "not-offered"));
    assert_eq!(rec(&ws, "ABC-1").status.unwrap().label, "Code Review");
    // todo -> review is not a move: said, not offered.
    assert_eq!(
        banner(&ws, "ABC-1").unwrap(),
        json!({
            "id": "tracker-status", "tone": "warn",
            "title": "Tracker: Code Review · here: To do",
            "lines": ["The tracker's status means In review here.", "The workflow has no move from To do to In review."],
            "actions": []
        })
    );
    assert_eq!(run(&ws, "ABC-1", "move:dev", None, AGENT).0, 200);
    let b = banner(&ws, "ABC-1").unwrap();
    assert_eq!(b["title"], "Tracker: Code Review · here: In development");
    assert_eq!(
        b["actions"],
        json!([{
            "kind": "endpoint", "id": "move:review", "label": "Move here to In review",
            "mutates": true, "confirm": "Move ABC-1 from In development to In review",
            "params": { "from": "dev", "to": "review" }, "note": "optional"
        }])
    );
    // It is a normal move.
    let reply = act(
        &ws,
        "ABC-1",
        "Code Review",
        "move:review",
        b["actions"][0]["params"].clone(),
        None,
        Some(AGENT),
        true,
    );
    assert_eq!(reply.0, 200, "{}", reply.1);
    assert_eq!(banner(&ws, "ABC-1"), None);
    // An unmapped status says nothing.
    act(
        &ws,
        "ABC-1",
        "Blocked",
        "nothing",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    assert_eq!(banner(&ws, "ABC-1"), None);
    // The same gates: into done from an agent waits for approval.
    act(
        &ws,
        "ABC-1",
        "Done",
        "nothing",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );
    let b = banner(&ws, "ABC-1").unwrap();
    assert_eq!(b["title"], "Tracker: Done · here: In review");
    let reply = act(
        &ws,
        "ABC-1",
        "Done",
        "move:done",
        b["actions"][0]["params"].clone(),
        None,
        Some(AGENT),
        true,
    );
    assert_eq!(
        kind(&reply),
        (409, "human-only"),
        "review is left by people only"
    );

    // Another source's rows use the "*" map.
    start_from(&ws, "other", "XYZ-1", "Brand new", Some(HUMAN));
    let reply = call(
        &ws,
        json!({
            "id": "nothing",
            "target": { "view": "work", "key": "XYZ-1", "source": "other", "item": owner("XYZ-1", "Doing") },
            "params": {},
        }),
        Some(HUMAN),
    );
    assert_eq!(reply.0, 409);
    let b = banner(&ws, "XYZ-1").unwrap();
    assert_eq!(b["title"], "Tracker: Doing · here: To do");
    assert_eq!(b["actions"][0]["id"], "move:dev");
    // Code Review is in tracker's map only.
    act_on(&ws, "other", "XYZ-1", "Code Review");
    assert_eq!(banner(&ws, "XYZ-1"), None);
}

fn act_on(ws: &Ws, source: &str, key: &str, status: &str) {
    call(
        ws,
        json!({
            "id": "nothing",
            "target": { "view": "work", "key": key, "source": source, "item": owner(key, status) },
            "params": {},
        }),
        Some(HUMAN),
    );
}

#[test]
fn the_status_blocks() {
    let ws = setup();
    // todo (no gate), review (human-exit), done (human), and dev with a
    // move into done waiting, its tracker saying Code Review.
    start(&ws, "ABC-1", "To Do");
    start(&ws, "ABC-2", "Code Review");
    start(&ws, "ABC-3", "Done");
    start(&ws, "ABC-4", "In Progress");
    ws.clock.advance(std::time::Duration::from_secs(600));
    assert_eq!(run(&ws, "ABC-4", "move:review", None, AGENT).0, 200);
    assert_eq!(
        run(&ws, "ABC-4", "move:dev", Some("flaky test"), HUMAN).0,
        200
    );
    assert_eq!(run(&ws, "ABC-4", "move:done", Some("fixed"), AGENT).0, 200);
    act(
        &ws,
        "ABC-4",
        "Code Review",
        "nothing",
        json!({}),
        None,
        Some(HUMAN),
        true,
    );

    let work = get(&ws.src, "/api/work").1;
    let blocks: Vec<Value> = work["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            let workflow_actions: Vec<&Value> = i["actions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| {
                    let id = a["id"].as_str().unwrap();
                    id.starts_with("move:") || id == "approve" || id == "send-back"
                })
                .collect();
            json!({
                "key": i["key"],
                "localStatus": i["localStatus"],
                "actions": workflow_actions,
                "banners": i["banners"],
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&blocks).unwrap() + "\n";
    let snap = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/local_status.json");
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        fs::write(&snap, &text).unwrap();
    }
    let want = fs::read_to_string(&snap)
        .expect("snapshot missing; run with UPDATE_SNAPSHOTS=1")
        .replace("\r\n", "\n");
    assert_eq!(
        text, want,
        "the status blocks moved; rerun with UPDATE_SNAPSHOTS=1 and review the diff"
    );

    let gates: Vec<&Value> = blocks.iter().map(|b| &b["localStatus"]["gate"]).collect();
    assert_eq!(
        gates,
        [
            &json!(null),
            &json!("human-exit"),
            &json!("human"),
            &json!(null)
        ]
    );
    // Every move the block lists is an action of the row.
    for b in &blocks {
        for m in b["localStatus"]["allowed"].as_array().unwrap() {
            assert!(
                b["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["id"] == m["id"]),
                "{} lists {} without an action",
                b["key"],
                m["id"]
            );
        }
    }
}

fn push_owner(ws: &Ws, body: &Value) -> (u16, Value) {
    post(&ws.src, "/api/work/owner", body, false)
}

#[test]
fn an_owner_update_refreshes_the_tracker_status_without_an_action() {
    let ws = setup();
    start(&ws, "ABC-1", "In Progress");
    assert_eq!(state(&ws, "ABC-1"), "dev");
    assert!(banner(&ws, "ABC-1").is_none());

    // The tracker moved the item: the push alone brings the banner.
    let (code, body) = push_owner(
        &ws,
        &json!({ "source": "tracker", "key": "ABC-1", "item": owner("ABC-1", "Code Review") }),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["message"], "ABC-1: the tracker's row was updated");
    assert_eq!(
        banner(&ws, "ABC-1").unwrap()["title"],
        "Tracker: Code Review · here: In development"
    );
    assert_eq!(rec(&ws, "ABC-1").status.unwrap().label, "Code Review");
    // The same row again changes nothing, and writes nothing.
    let before = fs::read_to_string(ws.p.work().join("ABC-1/item.json")).unwrap();
    ws.clock.advance(std::time::Duration::from_secs(60));
    let (code, body) = push_owner(
        &ws,
        &json!({ "source": "tracker", "key": "ABC-1", "item": owner("ABC-1", "Code Review") }),
    );
    assert_eq!(
        (code, body["message"].as_str()),
        (200, Some("ABC-1: nothing changed"))
    );
    assert_eq!(
        fs::read_to_string(ws.p.work().join("ABC-1/item.json")).unwrap(),
        before
    );
    // Done in the tracker: the archive badge.
    let done = json!({ "key": "ABC-1", "title": "Item ABC-1", "status": { "label": "Done", "category": "done" } });
    push_owner(
        &ws,
        &json!({ "source": "tracker", "key": "ABC-1", "item": done }),
    );
    assert!(row(&ws, "ABC-1")["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "done in tracker — archive?"));

    // Rows it has no workspace for are ignored, also the same key from
    // another source; the record is untouched.
    let before = fs::read_to_string(ws.p.work().join("ABC-1/item.json")).unwrap();
    for body in [
        json!({ "source": "tracker", "key": "ABC-2", "item": owner("ABC-2", "To Do") }),
        json!({ "source": "other", "key": "ABC-1", "item": owner("ABC-1", "To Do") }),
    ] {
        let (code, reply) = push_owner(&ws, &body);
        assert_eq!(code, 200, "{reply}");
        assert!(reply["message"].as_str().unwrap().ends_with("; ignored"));
    }
    // A row that names another key, or is not one, changes nothing.
    let (code, _) = push_owner(
        &ws,
        &json!({ "source": "tracker", "key": "ABC-1", "item": owner("ABC-9", "To Do") }),
    );
    assert_eq!(code, 200);
    assert_eq!(
        fs::read_to_string(ws.p.work().join("ABC-1/item.json")).unwrap(),
        before
    );
    for bad in [
        json!({ "key": "ABC-1", "item": {} }),
        json!({ "source": "tracker", "key": "", "item": {} }),
        json!({ "source": "tracker", "key": "ABC-1" }),
        json!({ "source": "tracker", "key": "ABC-1", "item": "Done" }),
    ] {
        assert_eq!(push_owner(&ws, &bad).0, 400, "{bad}");
    }
    let (code, _) = ws.src.handle("POST", "/api/work/owner", None, false, "{}");
    assert_eq!(code, 401);
}
