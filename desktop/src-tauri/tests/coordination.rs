//! Coordination-surface tests:
//! migrations idempotent, revision guards, append semantics, section
//! resolution, line-range bounds, leading-H1 title, list matched_fields,
//! archive, transfer, todo locks/leases, blocker cycles, derived is_blocked,
//! transfer semantics, and actor attribution from the `X-Chappa-Actor`
//! header over the real control server.
//!
//! Everything runs against a tempdir `coordination.db` (the store the app
//! opens) and a control server on an ephemeral port — never 8324.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use app_lib::control_http::{self, ControlState, ACTOR_HEADER};
use app_lib::coordination::{
    CoordError, Coordination, EditTarget, ReadMode, ScratchpadListQuery, TodoListQuery, TodoPatch, DB_FILE,
    HISTORY_CAP, SCHEMA_VERSION, USER_ACTOR,
};
use serde_json::{json, Value};

const TOKEN: &str = "t0ken-for-tests";

fn store() -> (tempfile::TempDir, Coordination) {
    let dir = tempfile::tempdir().unwrap();
    let store = Coordination::default();
    store.open(dir.path().join("cfg").join(DB_FILE)).unwrap();
    (dir, store)
}

/// One request with an optional actor header.
fn http(addr: &str, method: &str, path: &str, actor: Option<&str>, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let actor = actor.map(|a| format!("{ACTOR_HEADER}: {a}\r\n")).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n{actor}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("status");
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

fn start_control(coordination: Coordination) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let state = ControlState { coordination, ..ControlState::default() };
    std::thread::spawn(move || control_http::serve(server, state, TOKEN.to_owned()));
    addr
}

const DOC: &str = "# Plan\n\nintro\n\n## Alpha\n\na1\n\n### Alpha.child\n\nac\n\n## Beta\n\nb1\n\n## Alpha\n\ndup\n";

#[test]
fn migrations_are_idempotent_and_versioned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let first = Coordination::default();
    first.open(path.clone()).unwrap();
    assert_eq!(first.schema_version().unwrap(), SCHEMA_VERSION);
    let pad = first.scratchpad_create(USER_ACTOR, Some(1), "keep", "x\n", vec![]).unwrap();
    drop(first);
    // Open the SAME file again: version unchanged, data intact, no error.
    let second = Coordination::default();
    second.open(path).unwrap();
    assert_eq!(second.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(second.scratchpad_get(pad.scratchpad_id).unwrap().name, "keep");
}

/// The timers schema bumped `user_version` 1 → 2, then 2 → 3. The
/// migration is ADDITIVE: a file written by a build keeps every row
/// and simply gains the timer tables and columns.
#[test]
fn a_version_1_file_upgrades_additively_to_version_3() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    // A v1 file: the schema, stamped 1, with a pad and a todo in it.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE scratchpads (id INTEGER PRIMARY KEY, project_id INTEGER NULL, name TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '', revision INTEGER NOT NULL DEFAULT 1, tags TEXT NOT NULL DEFAULT '[]',
                archived INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                updated_by TEXT NOT NULL);
             CREATE TABLE scratchpad_history (id INTEGER PRIMARY KEY, scratchpad_id INTEGER NOT NULL,
                revision INTEGER NOT NULL, actor TEXT NOT NULL, op TEXT NOT NULL, at INTEGER NOT NULL);
             CREATE TABLE todos (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '', priority TEXT NOT NULL DEFAULT 'normal', status TEXT NOT NULL DEFAULT 'open',
                completed INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 1,
                locked_by TEXT NULL, lock_expires_at INTEGER NULL, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, updated_by TEXT NOT NULL);
             CREATE TABLE todo_blockers (todo_id INTEGER NOT NULL, blocked_by_todo_id INTEGER NOT NULL,
                PRIMARY KEY (todo_id, blocked_by_todo_id));
             CREATE TABLE todo_comments (id INTEGER PRIMARY KEY, todo_id INTEGER NOT NULL, actor TEXT NOT NULL,
                body TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             INSERT INTO scratchpads (name, content, created_at, updated_at, updated_by)
                VALUES ('old pad', '# old pad\nkeep me\n', 1, 1, 'user');
             INSERT INTO todos (project_id, title, created_at, updated_at, updated_by)
                VALUES (1, 'old todo', 1, 1, 'user');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let store = Coordination::default();
    store.open(path.clone()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.scratchpad_get(1).unwrap().content, "# old pad\nkeep me\n", "rows survive");
    assert_eq!(store.todo_get(1).unwrap().title, "old todo");
    // …and the timer tables now exist and take rows.
    let timers = app_lib::timers::TimerStore::new(store);
    assert!(timers.load_all().unwrap().is_empty());
    let mut timer = app_lib::timers::Timer::delay("user", 1, "go".into(), 1_000, 0).pinned("spawn-1");
    timer.id = timers.save(&timer).unwrap();
    let back = timers.load_all().unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].delivery_uuid.as_deref(), Some("spawn-1"));
}

/// A v2 file gains the uuid columns and the firing
/// index; its timer rows survive UNPINNED (`delivery_uuid: null`), which the
/// service treats as "never deliver" rather than "deliver to whoever has
/// that id now".
#[test]
fn a_version_2_file_upgrades_additively_to_version_3() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE timers (id INTEGER PRIMARY KEY, name TEXT NULL, kind TEXT NOT NULL, owner TEXT NOT NULL,
                project_id INTEGER NULL, delivery_process_id INTEGER NOT NULL, body TEXT NOT NULL,
                watch TEXT NOT NULL DEFAULT '[]', ignored TEXT NOT NULL DEFAULT '[]', delay_ms INTEGER NOT NULL DEFAULT 0,
                repeat_every_ms INTEGER NULL, next_fire_at INTEGER NULL, deadline_at INTEGER NULL,
                idle_ms INTEGER NOT NULL DEFAULT 0, confirm_ms INTEGER NOT NULL DEFAULT 0, rearm INTEGER NOT NULL DEFAULT 1,
                armed INTEGER NOT NULL DEFAULT 1, status TEXT NOT NULL, fired_count INTEGER NOT NULL DEFAULT 0,
                remaining_ms INTEGER NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE timer_firings (id INTEGER PRIMARY KEY, timer_id INTEGER NOT NULL, at INTEGER NOT NULL,
                reason TEXT NOT NULL, process_id INTEGER NOT NULL, delivered INTEGER NOT NULL, detail TEXT NULL,
                coalesced_with INTEGER NULL, receipt TEXT NULL);
             INSERT INTO timers (kind, owner, delivery_process_id, body, next_fire_at, status, created_at, updated_at)
                VALUES ('delay', '7', 7, 'old body', 5000, 'pending', 1, 1);
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let store = Coordination::default();
    store.open(path.clone()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let timers = app_lib::timers::TimerStore::new(store);
    let rows = timers.load_all().unwrap();
    assert_eq!(rows.len(), 1, "the v2 row survives");
    assert_eq!(rows[0].body, "old body");
    assert_eq!(rows[0].delivery_uuid, None, "unpinned, never re-bound to a reused id");
    assert!(rows[0].watch_uuids.is_empty());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='timer_firings_process'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "the (process_id, at) index exists");
}

#[test]
fn revision_guard_rejects_a_stale_write_and_reports_the_current_revision() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("7", Some(1), "pad", "one\n", vec![]).unwrap();
    assert_eq!(pad.revision, 1);
    let pad = store.scratchpad_append("7", pad.scratchpad_id, "two", None).unwrap();
    assert_eq!(pad.revision, 2, "append without a guard still advances the revision");
    // A write at the OLD revision is a typed conflict carrying the truth.
    let err = store
        .scratchpad_overwrite("8", pad.scratchpad_id, "pad", "clobber\n", None, Some(1))
        .unwrap_err();
    assert_eq!(
        err,
        CoordError::RevisionConflict { current_revision: 2, current_content: Some("one\ntwo\n".into()), current_content_truncated: false }
    );
    assert_eq!(store.scratchpad_get(pad.scratchpad_id).unwrap().content, "one\ntwo\n", "nothing overwritten");
    // write/edit/rename/clear REQUIRE the guard.
    assert!(matches!(
        store.scratchpad_overwrite("8", pad.scratchpad_id, "pad", "x", None, None).unwrap_err(),
        CoordError::Invalid(_)
    ));
    assert!(matches!(
        store.scratchpad_rename("8", pad.scratchpad_id, "y", None).unwrap_err(),
        CoordError::Invalid(_)
    ));
    // The right revision goes through and attributes the write.
    let pad = store
        .scratchpad_overwrite("8", pad.scratchpad_id, "pad", "fresh\n", None, Some(2))
        .unwrap();
    assert_eq!((pad.revision, pad.content.as_str(), pad.updated_by.as_str()), (3, "fresh\n", "8"));
}

#[test]
fn append_section_under_a_missing_heading_errors_and_changes_nothing() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "pad", DOC, vec![]).unwrap();
    let err = store
        .scratchpad_append_section("u", pad.scratchpad_id, "Gamma", "nope", None)
        .unwrap_err();
    assert!(matches!(err, CoordError::Invalid(_)), "{err:?}");
    let same = store.scratchpad_get(pad.scratchpad_id).unwrap();
    assert_eq!((same.revision, same.content.as_str()), (1, DOC));
    // Under an existing heading: lands at the end of THAT section (before
    // Beta), not at the end of the document; the first duplicate wins.
    let pad = store
        .scratchpad_append_section("u", pad.scratchpad_id, "Alpha", "a2", None)
        .unwrap();
    assert!(pad.content.contains("ac\na2\n\n## Beta"), "{}", pad.content);
    assert!(pad.content.ends_with("## Alpha\n\ndup\n"), "second Alpha untouched");
    assert_eq!(pad.revision, 2);
}

#[test]
fn edit_by_heading_replaces_exactly_that_section() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "pad", DOC, vec![]).unwrap();
    let pad = store
        .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Section("Alpha".into()), "new\n", Some(1))
        .unwrap();
    assert_eq!(
        pad.content,
        "# Plan\n\nintro\n\n## Alpha\nnew\n## Beta\n\nb1\n\n## Alpha\n\ndup\n",
        "nested child gone with its parent, Beta and the duplicate Alpha intact"
    );
    // Stale guard on edit is a conflict; a missing target heading is invalid.
    assert!(matches!(
        store
            .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Section("Beta".into()), "x", Some(1))
            .unwrap_err(),
        CoordError::RevisionConflict { current_revision: 2, .. }
    ));
    assert!(matches!(
        store
            .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Section("Nope".into()), "x", Some(2))
            .unwrap_err(),
        CoordError::Invalid(_)
    ));
    // Section read resolves the same way.
    let read = store
        .scratchpad_read(pad.scratchpad_id, &ReadMode::Section("Beta".into()), 0, None)
        .unwrap();
    assert_eq!(read["content"], "## Beta\n\nb1\n\n");
    let outline = store.scratchpad_read(pad.scratchpad_id, &ReadMode::Outline, 0, None).unwrap();
    let texts: Vec<&str> = outline["headings"].as_array().unwrap().iter().map(|h| h["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["Plan", "Alpha", "Beta", "Alpha"]);
    assert!(outline.get("content").is_none(), "outline carries no content");
}

#[test]
fn edit_by_line_range_is_bounds_checked() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "pad", "a\nb\nc\n", vec![]).unwrap();
    for (s, e) in [(0, 1), (2, 1), (1, 4), (4, 4)] {
        let err = store
            .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Lines { start_line: s, end_line: e }, "x", Some(1))
            .unwrap_err();
        assert!(matches!(err, CoordError::Invalid(_)), "{s}..={e}: {err:?}");
    }
    assert_eq!(store.scratchpad_get(pad.scratchpad_id).unwrap().revision, 1, "nothing changed");
    let pad = store
        .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Lines { start_line: 2, end_line: 3 }, "B\n", Some(1))
        .unwrap();
    assert_eq!(pad.content, "a\nB\n");
}

#[test]
fn a_leading_h1_overrides_the_name() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", None, "given", "# Real title\nbody\n", vec![]).unwrap();
    assert_eq!(pad.name, "Real title");
    assert_eq!(pad.project_id, None, "global pads are allowed");
    let pad = store.scratchpad_overwrite("u", pad.scratchpad_id, "given", "no heading\n", None, Some(1)).unwrap();
    assert_eq!(pad.name, "given", "no H1 → the given name");
    let pad = store.scratchpad_overwrite("u", pad.scratchpad_id, "given", "## h2 only\n", None, Some(2)).unwrap();
    assert_eq!(pad.name, "given", "only an H1 overrides");
}

#[test]
fn list_query_returns_matched_fields_and_archive_hides_by_default() {
    let (_dir, store) = store();
    let a = store.scratchpad_create("u", Some(1), "alpha notes", "nothing here\n", vec!["x".into()]).unwrap();
    let b = store.scratchpad_create("u", Some(1), "other", "the alpha protocol is long enough to snip around\n", vec![]).unwrap();
    let _other_project = store.scratchpad_create("u", Some(2), "alpha elsewhere", "", vec![]).unwrap();
    let listing = store
        .scratchpad_list(&ScratchpadListQuery {
            project_id: Some(Some(1)),
            query: Some("ALPHA".into()),
            ..Default::default()
        })
        .unwrap();
    let rows = listing["scratchpads"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let by_id = |id: i64| rows.iter().find(|r| r["scratchpad_id"] == id).unwrap();
    assert_eq!(by_id(a.scratchpad_id)["matched_fields"], json!(["name"]));
    assert_eq!(by_id(b.scratchpad_id)["matched_fields"], json!(["content"]));
    assert!(by_id(b.scratchpad_id)["snippet"].as_str().unwrap().contains("alpha protocol"));
    assert!(by_id(a.scratchpad_id).get("content").is_none(), "list rows carry no content");
    // Tags filter, then archive.
    let tagged = store.scratchpad_list(&ScratchpadListQuery { tags: vec!["x".into()], ..Default::default() }).unwrap();
    assert_eq!(tagged["total"], 1);
    store.scratchpad_archive("u", a.scratchpad_id, true).unwrap();
    let default = store.scratchpad_list(&ScratchpadListQuery { project_id: Some(Some(1)), ..Default::default() }).unwrap();
    assert_eq!(default["scratchpads"].as_array().unwrap().len(), 1);
    assert_eq!(default["scratchpads"][0]["scratchpad_id"], b.scratchpad_id);
    let all = store
        .scratchpad_list(&ScratchpadListQuery { project_id: Some(Some(1)), include_archived: true, ..Default::default() })
        .unwrap();
    assert_eq!(all["scratchpads"].as_array().unwrap().len(), 2);
    assert_eq!(store.scratchpad_tags(Some(Some(1))).unwrap(), vec!["x".to_owned()]);
}

// ---- hardening ----------------------------------------------

/// `current_content` rides along only on the content-bearing ops
/// (write/edit/clear), capped at 8 KB with the truncated flag; a rename /
/// tags / delete conflict carries the revision alone.
#[test]
fn conflict_payload_carries_content_only_for_content_ops_and_caps_it() {
    let (_dir, store) = store();
    let big = format!("# Big\n{}", "x".repeat(40 * 1024));
    let pad = store.scratchpad_create("u", Some(1), "big", &big, vec![]).unwrap();
    let pad = store.scratchpad_append("u", pad.scratchpad_id, "more", None).unwrap();
    assert_eq!(pad.revision, 2);
    let stale = Some(1);
    // Meta ops: no content.
    for err in [
        store.scratchpad_rename("u", pad.scratchpad_id, "renamed", stale).unwrap_err(),
        store.scratchpad_add_tags("u", pad.scratchpad_id, vec!["t".into()], stale).unwrap_err(),
        store.scratchpad_remove_tags("u", pad.scratchpad_id, vec!["t".into()], stale).unwrap_err(),
        store.scratchpad_delete("u", pad.scratchpad_id, stale).unwrap_err(),
        store.scratchpad_transfer("u", pad.scratchpad_id, Some(2), stale).unwrap_err(),
        store.scratchpad_append("u", pad.scratchpad_id, "x", stale).unwrap_err(),
    ] {
        assert!(
            matches!(err, CoordError::RevisionConflict { current_revision: 2, current_content: None, .. }),
            "{err:?}"
        );
        assert!(err.to_json().get("current_content").is_none(), "{}", err.to_json());
    }
    // Content ops: capped content + the flag.
    let target = EditTarget::Section("Big".into());
    for err in [
        store.scratchpad_overwrite("u", pad.scratchpad_id, "big", "new", None, stale).unwrap_err(),
        store.scratchpad_edit("u", pad.scratchpad_id, &target, "new", stale).unwrap_err(),
        store.scratchpad_clear("u", pad.scratchpad_id, stale).unwrap_err(),
    ] {
        match &err {
            CoordError::RevisionConflict { current_revision: 2, current_content: Some(c), current_content_truncated: true } => {
                assert_eq!(c.len(), 8 * 1024);
                assert!(pad.content.starts_with(c.as_str()));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(err.to_json()["current_content_truncated"], true);
    }
    // A small pad comes back whole, untruncated.
    let small = store.scratchpad_create("u", Some(1), "small", "hello\n", vec![]).unwrap();
    let err = store.scratchpad_clear("u", small.scratchpad_id, Some(7)).unwrap_err();
    assert_eq!(
        err,
        CoordError::RevisionConflict { current_revision: 1, current_content: Some("hello\n".into()), current_content_truncated: false }
    );
    assert_eq!(store.scratchpad_get(pad.scratchpad_id).unwrap().content, pad.content, "nothing changed");
}

/// The leading-H1 title override applies on create and full overwrite ONLY:
/// an explicit rename survives a later section edit.
#[test]
fn edit_never_reverts_an_explicit_rename() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "given", "# Auto title\n\n## Log\n\nold\n", vec![]).unwrap();
    assert_eq!(pad.name, "Auto title");
    let pad = store.scratchpad_rename("u", pad.scratchpad_id, "Chosen name", Some(1)).unwrap();
    assert_eq!(pad.name, "Chosen name");
    let pad = store
        .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Section("Log".into()), "new\n", Some(2))
        .unwrap();
    assert_eq!(pad.name, "Chosen name", "edit must not re-apply the H1 override");
    assert!(pad.content.contains("## Log\nnew\n"), "{}", pad.content);
    // Even editing the H1 line itself leaves the name alone …
    let pad = store
        .scratchpad_edit("u", pad.scratchpad_id, &EditTarget::Lines { start_line: 1, end_line: 1 }, "# Another H1\n", Some(3))
        .unwrap();
    assert_eq!(pad.name, "Chosen name");
    // … while a full overwrite applies it again (the documented rule).
    let pad = store.scratchpad_overwrite("u", pad.scratchpad_id, "ignored", "# Overwritten\n", None, Some(4)).unwrap();
    assert_eq!(pad.name, "Overwritten");
}

/// Archive flips the flag without bumping the revision (a concurrent guarded
/// writer is not bounced by it); delete logs history at the same convention
/// as every other op; history is capped per pad.
#[test]
fn archive_keeps_the_revision_and_history_is_capped() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "pad", "x\n", vec![]).unwrap();
    let archived = store.scratchpad_archive("9", pad.scratchpad_id, true).unwrap();
    assert_eq!((archived.revision, archived.archived, archived.updated_by.as_str()), (1, true, "9"));
    // A writer holding revision 1 is still fine after the archive.
    let pad = store.scratchpad_append("u", pad.scratchpad_id, "y", Some(1)).unwrap();
    assert_eq!((pad.revision, pad.archived), (2, true));
    let back = store.scratchpad_archive("u", pad.scratchpad_id, false).unwrap();
    assert_eq!((back.revision, back.archived), (2, false));

    let history = |id: i64| -> Vec<(i64, String)> {
        let conn = rusqlite::Connection::open(_dir.path().join("cfg").join(DB_FILE)).unwrap();
        let mut stmt = conn.prepare("SELECT revision, op FROM scratchpad_history WHERE scratchpad_id = ?1 ORDER BY id").unwrap();
        stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(
        history(pad.scratchpad_id),
        vec![(1, "create".to_owned()), (1, "archive".into()), (2, "append".into()), (2, "unarchive".into())]
    );
    store.scratchpad_delete("u", pad.scratchpad_id, Some(2)).unwrap();
    assert_eq!(history(pad.scratchpad_id).last().cloned(), Some((3, "delete".into())), "delete = revision the op produced");

    // Cap: HISTORY_CAP rows per pad, oldest pruned.
    let pad = store.scratchpad_create("u", Some(1), "busy", "", vec![]).unwrap();
    for _ in 0..(HISTORY_CAP as usize + 20) {
        store.scratchpad_append("u", pad.scratchpad_id, "l", None).unwrap();
    }
    let rows = history(pad.scratchpad_id);
    assert_eq!(rows.len(), HISTORY_CAP as usize);
    assert_eq!(rows.last().unwrap().0, HISTORY_CAP + 21, "the newest row survives");
    assert_ne!(rows[0].1, "create", "the oldest rows were pruned");
}

/// Global pads (`project_id: null`) show up beside a project's own pads when
/// asked for (the rail asks; a plain Claude Code session creates them).
#[test]
fn list_include_global_adds_the_global_pads_to_a_project_scope() {
    let (_dir, store) = store();
    let own = store.scratchpad_create("u", Some(1), "own", "", vec![]).unwrap();
    let global = store.scratchpad_create("user", None, "eval · worker models", "", vec![]).unwrap();
    let _other = store.scratchpad_create("u", Some(2), "elsewhere", "", vec![]).unwrap();
    let ids = |v: Value| -> Vec<i64> {
        v["scratchpads"].as_array().unwrap().iter().map(|r| r["scratchpad_id"].as_i64().unwrap()).collect()
    };
    let plain = store.scratchpad_list(&ScratchpadListQuery { project_id: Some(Some(1)), ..Default::default() }).unwrap();
    assert_eq!(ids(plain), vec![own.scratchpad_id]);
    let with_global = store
        .scratchpad_list(&ScratchpadListQuery { project_id: Some(Some(1)), include_global: true, ..Default::default() })
        .unwrap();
    let mut got = ids(with_global);
    got.sort();
    assert_eq!(got, vec![own.scratchpad_id, global.scratchpad_id]);
    let only_global = store.scratchpad_list(&ScratchpadListQuery { project_id: Some(None), include_global: true, ..Default::default() }).unwrap();
    assert_eq!(ids(only_global), vec![global.scratchpad_id]);
    // Over HTTP too.
    let addr = start_control(store);
    let (status, r) = http(&addr, "GET", "/scratchpads?project_id=1&include_global=true", None, "");
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["total"], 2);
    let row = r["scratchpads"].as_array().unwrap().iter().find(|r| r["scratchpad_id"] == global.scratchpad_id).unwrap();
    assert!(row["project_id"].is_null());
    assert_eq!(row["line_count"], 0);
}

/// The SQL-side summary listing computes line_count/bytes exactly like the
/// Rust `lines_of` did, and todo_list's SQL filters + sorts agree with the
/// full-row path.
#[test]
fn sql_side_listings_match_the_row_semantics() {
    let (_dir, store) = store();
    let cases = [("", 0), ("a", 1), ("a\n", 1), ("a\nb", 2), ("a\nb\n", 2), ("é\n\n", 2)];
    for (content, lines) in cases {
        let pad = store.scratchpad_create("u", Some(3), "p", content, vec![]).unwrap();
        let r = store.scratchpad_list(&ScratchpadListQuery { project_id: Some(Some(3)), ..Default::default() }).unwrap();
        let row = r["scratchpads"].as_array().unwrap().iter().find(|r| r["scratchpad_id"] == pad.scratchpad_id).unwrap();
        assert_eq!((row["line_count"].as_i64(), row["bytes"].as_i64()), (Some(lines), Some(content.len() as i64)), "{content:?}");
    }
    let a = store.todo_create("u", 1, "Bravo", "", Some("low"), vec!["x".into()]).unwrap();
    let b = store.todo_create("u", 1, "alpha", "", Some("urgent"), vec![]).unwrap();
    let c = store.todo_create("u", 2, "other project", "", Some("high"), vec![]).unwrap();
    store.todo_complete("u", a.todo_id, true, true).unwrap();
    let ids = |v: Value| -> Vec<i64> { v["todos"].as_array().unwrap().iter().map(|r| r["todo_id"].as_i64().unwrap()).collect() };
    let q = |f: TodoListQuery| store.todo_list(&f).unwrap();
    assert_eq!(ids(q(TodoListQuery { project_id: Some(1), sort: Some("title".into()), ..Default::default() })), vec![b.todo_id, a.todo_id]);
    assert_eq!(ids(q(TodoListQuery { sort: Some("priority_desc".into()), ..Default::default() })), vec![b.todo_id, c.todo_id, a.todo_id]);
    assert_eq!(ids(q(TodoListQuery { completed: Some(true), ..Default::default() })), vec![a.todo_id]);
    assert_eq!(ids(q(TodoListQuery { status: Some("open".into()), priority: Some("urgent".into()), ..Default::default() })), vec![b.todo_id]);
    assert_eq!(ids(q(TodoListQuery { tags: vec!["x".into()], ..Default::default() })), vec![a.todo_id]);
    assert_eq!(ids(q(TodoListQuery { query: Some("ALPHA".into()), ..Default::default() })), vec![b.todo_id]);
    let err = store.todo_list(&TodoListQuery { sort: Some("bogus".into()), ..Default::default() }).unwrap_err();
    assert!(matches!(err, CoordError::Invalid(_)), "{err:?}");
    let page = q(TodoListQuery { project_id: Some(1), sort: Some("title".into()), ..Default::default() });
    assert!(page["todos"][0].get("body").is_none(), "summaries carry no body");
    assert_eq!(page["todos"][0]["comment_count"], 0);
    assert_eq!(store.todo_tags(Some(1)).unwrap(), vec!["x".to_owned()]);
    assert_eq!(store.todo_tags(Some(2)).unwrap(), Vec::<String>::new());
}

#[test]
fn scratchpad_transfer_changes_the_project_only() {
    let (_dir, store) = store();
    let pad = store.scratchpad_create("u", Some(1), "pad", "body\n", vec!["t".into()]).unwrap();
    let moved = store.scratchpad_transfer("9", pad.scratchpad_id, Some(2), Some(1)).unwrap();
    assert_eq!(moved.project_id, Some(2));
    assert_eq!((moved.name.as_str(), moved.content.as_str(), &moved.tags), ("pad", "body\n", &pad.tags));
    assert_eq!((moved.revision, moved.updated_by.as_str()), (2, "9"));
    let global = store.scratchpad_transfer("u", pad.scratchpad_id, None, None).unwrap();
    assert_eq!(global.project_id, None);
}

#[test]
fn todo_lock_leases_by_actor() {
    let (_dir, store) = store();
    let t = store.todo_create("a", 1, "task", "", None, vec![]).unwrap();
    assert_eq!((t.priority.as_str(), t.status.as_str(), t.completed), ("normal", "open", false));
    // Actor b takes the lock; a's update is refused naming b.
    let locked = store.todo_lock("b", t.todo_id, Some(60_000)).unwrap();
    assert_eq!(locked.locked_by.as_deref(), Some("b"));
    let err = store
        .todo_update("a", t.todo_id, &TodoPatch { title: Some("x".into()), ..Default::default() })
        .unwrap_err();
    assert!(matches!(&err, CoordError::Locked { locked_by, .. } if locked_by == "b"), "{err:?}");
    assert_eq!(store.todo_get(t.todo_id).unwrap().title, "task");
    // The holder edits freely; a foreign unlock is refused too.
    store.todo_update("b", t.todo_id, &TodoPatch { title: Some("by b".into()), ..Default::default() }).unwrap();
    assert!(matches!(store.todo_unlock("a", t.todo_id).unwrap_err(), CoordError::Locked { .. }));
    // An EXPIRED lock does not block: lease of 1 ms, then wait.
    let short = store.todo_lock("b", t.todo_id, Some(1)).unwrap();
    assert!(short.lock_expires_at.is_some());
    std::thread::sleep(Duration::from_millis(20));
    let row = store.todo_get(t.todo_id).unwrap();
    assert_eq!(row.locked_by, None, "an expired lease reads as no lock");
    store.todo_update("a", t.todo_id, &TodoPatch { body: Some("a again".into()), ..Default::default() }).unwrap();
    // complete releases the caller's own lock unless release_lock=false.
    store.todo_lock("a", t.todo_id, None).unwrap();
    let (done, _) = store.todo_complete("a", t.todo_id, true, false).unwrap();
    assert_eq!((done.completed, done.status.as_str(), done.locked_by.as_deref()), (true, "done", Some("a")));
    let (undone, _) = store.todo_complete("a", t.todo_id, false, true).unwrap();
    assert_eq!((undone.completed, undone.status.as_str(), undone.locked_by), (false, "open", None));
    // Revision guard on update is optional but honoured.
    let err = store
        .todo_update("a", t.todo_id, &TodoPatch { expected_revision: Some(1), ..Default::default() })
        .unwrap_err();
    assert!(matches!(err, CoordError::RevisionConflict { current_content: None, .. }), "{err:?}");
}

#[test]
fn blocker_cycles_are_rejected_and_is_blocked_is_derived_from_open_blockers() {
    let (_dir, store) = store();
    let a = store.todo_create("u", 1, "a", "", None, vec![]).unwrap().todo_id;
    let b = store.todo_create("u", 1, "b", "", None, vec![]).unwrap().todo_id;
    let c = store.todo_create("u", 1, "c", "", None, vec![]).unwrap().todo_id;
    let other = store.todo_create("u", 2, "other project", "", None, vec![]).unwrap().todo_id;
    store.todo_add_blocker("u", a, b).unwrap(); // a blocked by b
    store.todo_add_blocker("u", b, c).unwrap(); // b blocked by c
    assert!(matches!(store.todo_add_blocker("u", c, a).unwrap_err(), CoordError::Cycle(_)), "c→a closes the loop");
    assert!(matches!(store.todo_add_blocker("u", a, a).unwrap_err(), CoordError::Cycle(_)), "self");
    assert!(matches!(store.todo_set_blockers("u", c, &[b]).unwrap_err(), CoordError::Cycle(_)));
    assert!(matches!(store.todo_add_blocker("u", a, other).unwrap_err(), CoordError::Invalid(_)), "cross-project");
    assert_eq!(store.todo_get(c).unwrap().blocker_ids, Vec::<i64>::new(), "a failed set changes nothing");
    let row = store.todo_get(a).unwrap();
    assert_eq!((row.is_blocked, row.blocker_ids.clone()), (true, vec![b]));
    // Completing b unblocks a (derived, not stored) and names a as affected.
    let (_, affected) = store.todo_complete("u", b, true, true).unwrap();
    assert_eq!(affected, vec![a]);
    let row = store.todo_get(a).unwrap();
    assert_eq!((row.is_blocked, row.blocker_ids.clone()), (false, vec![b]), "blocker kept, no longer blocking");
    let listing = store
        .todo_list(&app_lib::coordination::TodoListQuery { project_id: Some(1), is_blocked: Some(true), ..Default::default() })
        .unwrap();
    assert_eq!(listing["todos"].as_array().unwrap().len(), 1, "only b (blocked by open c)");
    assert_eq!(listing["todos"][0]["todo_id"], b);
    assert!(listing["todos"][0].get("body").is_none(), "summaries carry no body");
}

#[test]
fn todo_transfer_clears_blockers_and_lock_and_keeps_comments() {
    let (_dir, store) = store();
    let a = store.todo_create("u", 1, "a", "", None, vec![]).unwrap().todo_id;
    let b = store.todo_create("u", 1, "b", "", None, vec![]).unwrap().todo_id;
    let d = store.todo_create("u", 1, "d", "", None, vec![]).unwrap().todo_id;
    store.todo_add_blocker("u", a, b).unwrap();
    store.todo_add_blocker("u", d, a).unwrap();
    store.todo_lock("u", a, None).unwrap();
    let comment = store.todo_comment_create("u", a, "note").unwrap();
    let (moved, affected) = store.todo_transfer("u", a, 2).unwrap();
    assert_eq!(moved.project_id, 2);
    assert_eq!((moved.locked_by, moved.blocker_ids.clone(), moved.is_blocked), (None, vec![], false));
    assert_eq!(affected, vec![d]);
    assert_eq!(store.todo_get(d).unwrap().blocker_ids, Vec::<i64>::new(), "reverse edge cleared too");
    let comments = store.todo_comments(a, 0, None).unwrap();
    assert_eq!(comments["comments"][0]["comment_id"], comment.comment_id);
    assert_eq!(moved.comment_count, 1);
    // A foreign lock refuses a transfer like any other edit.
    store.todo_lock("x", b, None).unwrap();
    assert!(matches!(store.todo_transfer("u", b, 2).unwrap_err(), CoordError::Locked { .. }));
}

/// The whole loop over HTTP: the actor header attributes writes, the typed
/// errors reach the wire with their status and fields, confirm gates clear
/// and delete, and a bare request is "user".
#[test]
fn control_routes_attribute_the_actor_header_and_surface_typed_errors() {
    let (_dir, store) = store();
    let addr = start_control(store.clone());

    let (status, pad) = http(
        &addr,
        "POST",
        "/scratchpads",
        Some("12"),
        &json!({"name": "ignored", "content": "# Field notes\n\n## Log\n\nfirst\n", "project_id": 6, "tags": ["eval"]}).to_string(),
    );
    assert_eq!(status, 200, "{pad}");
    assert_eq!(pad["name"], "Field notes", "leading H1 overrides name");
    assert_eq!(pad["revision"], 1);
    let id = pad["scratchpad_id"].as_i64().unwrap();
    assert_eq!(store.scratchpad_get(id).unwrap().updated_by, "12", "X-Chappa-Actor is the actor");

    // No header = "user".
    let (status, r) = http(&addr, "POST", &format!("/scratchpads/{id}/append"), None, r#"{"content": "second"}"#);
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["revision"], 2);
    assert_eq!(store.scratchpad_get(id).unwrap().updated_by, USER_ACTOR);

    // append_section under an existing heading; missing heading = 400.
    let (status, r) = http(&addr, "POST", &format!("/scratchpads/{id}/append_section"), Some("12"), r#"{"heading": "Log", "content": "third"}"#);
    assert_eq!(status, 200, "{r}");
    let (status, r) = http(&addr, "POST", &format!("/scratchpads/{id}/append_section"), Some("12"), r#"{"heading": "Nope", "content": "x"}"#);
    assert_eq!((status, r["error"].as_str()), (400, Some("invalid")));

    // Stale revision → 409 revision_conflict with the current state.
    let (status, r) = http(
        &addr,
        "POST",
        &format!("/scratchpads/{id}/edit"),
        Some("12"),
        &json!({"target": {"heading": "Log"}, "content": "replaced\n", "expected_revision": 1}).to_string(),
    );
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["error"], "revision_conflict");
    assert_eq!(r["current_revision"], 3);
    assert!(r["current_content"].as_str().unwrap().contains("third"));
    let (status, r) = http(
        &addr,
        "POST",
        &format!("/scratchpads/{id}/edit"),
        Some("12"),
        &json!({"target": {"type": "section", "section_heading": "Log"}, "content": "replaced\n", "expected_revision": 3}).to_string(),
    );
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["revision"], 4);

    // Reads: outline, section, find, tail, list with matched_fields.
    let (_, r) = http(&addr, "GET", &format!("/scratchpads/{id}?mode=outline"), None, "");
    assert_eq!(r["headings"].as_array().unwrap().len(), 2);
    let (_, r) = http(&addr, "GET", &format!("/scratchpads/{id}?mode=section&section_heading=Log"), None, "");
    assert_eq!(r["content"], "## Log\nreplaced\n");
    let (_, r) = http(&addr, "GET", &format!("/scratchpads/{id}/find?query=REPLACED"), None, "");
    assert_eq!(r["total_matches"], 1);
    let (_, r) = http(&addr, "GET", &format!("/scratchpads/{id}/find?query=REPLACED&case_sensitive=true"), None, "");
    assert_eq!(r["total_matches"], 0);
    let (_, r) = http(&addr, "GET", &format!("/scratchpads/{id}/tail?lines=1"), None, "");
    assert_eq!(r["content"], "replaced\n");
    let (_, r) = http(&addr, "GET", "/scratchpads?project_id=6&query=replaced&tags=eval", None, "");
    assert_eq!(r["scratchpads"][0]["matched_fields"], json!(["content"]));
    assert_eq!(r["scratchpads"][0]["updated_by"], "12");
    let (_, r) = http(&addr, "GET", "/scratchpads/tags?project_id=6", None, "");
    assert_eq!(r["tags"], json!(["eval"]));

    // rich response mode returns the full row.
    let (_, r) = http(&addr, "POST", &format!("/scratchpads/{id}/tags/add"), Some("12"), r#"{"tags": ["x"], "expected_revision": 4, "response_mode": "rich"}"#);
    assert_eq!(r["tags"], json!(["eval", "x"]));
    assert!(r["content"].is_string());

    // clear/delete need confirm.
    let (status, r) = http(&addr, "POST", &format!("/scratchpads/{id}/clear"), Some("12"), r#"{"expected_revision": 5}"#);
    assert_eq!(status, 400, "{r}");
    assert!(r["message"].as_str().unwrap().contains("confirm"));
    let (status, _) = http(&addr, "POST", &format!("/scratchpads/{id}/clear"), Some("12"), r#"{"expected_revision": 5, "confirm": true}"#);
    assert_eq!(status, 200);
    assert_eq!(store.scratchpad_get(id).unwrap().content, "");
    let (status, r) = http(&addr, "POST", &format!("/scratchpads/{id}/delete"), Some("12"), r#"{"expected_revision": 6, "confirm": true}"#);
    assert_eq!((status, r["deleted"].as_bool()), (200, Some(true)));
    let (status, r) = http(&addr, "GET", &format!("/scratchpads/{id}"), None, "");
    assert_eq!((status, r["error"].as_str()), (404, Some("not_found")));

    // Todos over the wire: lock by one actor, update by another → 409 locked.
    let (status, t) = http(&addr, "POST", "/todos", Some("1"), r#"{"project_id": 6, "title": "first", "priority": "medium"}"#);
    assert_eq!(status, 200, "{t}");
    let tid = t["todo_id"].as_i64().unwrap();
    let (status, u) = http(&addr, "POST", "/todos", Some("1"), r#"{"project_id": 6, "title": "second"}"#);
    assert_eq!(status, 200, "{u}");
    let uid = u["todo_id"].as_i64().unwrap();
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}/blockers/add"), Some("1"), &json!({"blocker_id": uid}).to_string());
    assert_eq!((status, r["is_blocked"].as_bool()), (200, Some(true)));
    let (status, r) = http(&addr, "POST", &format!("/todos/{uid}/blockers/add"), Some("1"), &json!({"blocker_id": tid}).to_string());
    assert_eq!((status, r["error"].as_str()), (409, Some("cycle")));
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}/lock"), Some("2"), r#"{"lease_ttl_seconds": 60}"#);
    assert_eq!((status, r["locked_by"].as_str()), (200, Some("2")));
    // Review fix: an absurd lease_ttl_seconds saturates and clamps to 24 h
    // instead of overflowing the multiply (renewal by the same actor).
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}/lock"), Some("2"), r#"{"lease_ttl_seconds": 9223372036854775807}"#);
    assert_eq!(status, 200, "{r}");
    let exp = r["lock_expires_at"].as_i64().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(exp > now && exp <= now + 24 * 3_600_000 + 5_000, "clamped to MAX_LEASE_MS: {exp} vs now {now}");
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}"), Some("1"), r#"{"title": "stolen"}"#);
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["error"], "locked");
    assert_eq!(r["locked_by"], "2");
    assert!(r["lock_expires_at"].as_i64().unwrap() > 0);
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}"), Some("2"), r#"{"title": "mine", "status": "in_progress"}"#);
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["title"], "mine");
    assert_eq!(r["status"], "in_progress");
    assert!(r.get("body").is_none(), "slim receipt = changed fields only");
    let (_, r) = http(&addr, "GET", &format!("/todos/{tid}"), None, "");
    assert_eq!(r["priority"], "normal", "`medium` maps to normal");
    assert_eq!(r["updated_by"], "2");
    let (status, c) = http(&addr, "POST", &format!("/todos/{tid}/comments"), Some("2"), r#"{"body": "done soon"}"#);
    assert_eq!(status, 200, "{c}");
    let (_, r) = http(&addr, "GET", &format!("/todos/{tid}?include_comments=true"), None, "");
    assert_eq!(r["comments"][0]["actor"], "2");
    let (_, r) = http(&addr, "GET", "/todos?project_id=6&query=soon", None, "");
    assert_eq!(r["total"], 1, "query searches comments too");
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}/delete"), Some("2"), "{}");
    assert_eq!(status, 400, "{r}");
    let (status, r) = http(&addr, "POST", &format!("/todos/{tid}/delete"), Some("2"), r#"{"confirm": true}"#);
    assert_eq!((status, r["deleted"].as_bool()), (200, Some(true)));
}

/// An uninitialized store (no Tauri runtime) is a clean 500, never a panic.
#[test]
fn an_uninitialized_store_answers_500() {
    let addr = start_control(Coordination::default());
    let (status, r) = http(&addr, "GET", "/scratchpads", None, "");
    assert_eq!(status, 500);
    assert_eq!(r["error"], "internal");
}
