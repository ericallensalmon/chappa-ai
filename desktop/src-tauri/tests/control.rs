//! Control-surface tests: token
//! file created once with restrictive permissions, control routes reject a
//! bad/missing token, debug routes stay tokenless, `GET /` matches the
//! registered handlers (walking the shared const), argv survives unsplit,
//! submit is one atomic write, receipts + journal + confirm-gated close.
//!
//! Servers bind ephemeral ports (never 8323/8324) so a running app on the
//! host can't collide with the suite. Real terminals: `cmd` on Windows, `sh`
//! elsewhere — same split as tests/pump.rs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use app_lib::control_http::{
    self, load_or_create_token, route_table_json, ControlState, ROUTES, TOKEN_FILE,
};
use app_lib::registry::Registry;
use serde_json::{json, Value};

/// Hand-rolled HTTP/1.1 client: one request per connection, no crate.
fn http(addr: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let value = serde_json::from_str(payload).unwrap_or(Value::Null);
    (status, value)
}

fn start_control(state: ControlState) -> (String, String) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let token = "t0ken-for-tests".to_owned();
    let t = token.clone();
    std::thread::spawn(move || control_http::serve(server, state, t));
    (addr, token)
}

#[cfg(windows)]
fn shell_echoing_argv() -> (String, Vec<String>) {
    // `cmd /K echo a b c` prints the argument then stays interactive.
    ("cmd".into(), vec!["/K".into(), "echo".into(), "a b c".into()])
}

#[cfg(not(windows))]
fn shell_echoing_argv() -> (String, Vec<String>) {
    (
        "sh".into(),
        vec![
            "-c".into(),
            "echo \"$1\"; exec sh".into(),
            "sh".into(),
            "a b c".into(),
        ],
    )
}

/// Poll `/output` until a row contains `needle` (or panic after 10s).
fn wait_for_text(addr: &str, token: &str, id: u64, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = http(addr, "GET", &format!("/processes/{id}/output"), Some(token), "");
        assert_eq!(status, 200);
        let rows: Vec<String> = body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        if rows.iter().any(|r| r.contains(needle)) {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {needle:?} in output; last rows: {rows:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn token_file_is_created_once_with_restrictive_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg"); // does not exist yet: created
    let first = load_or_create_token(&cfg).expect("create token");
    assert_eq!(first.len(), 64, "32 random bytes, hex");
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    let second = load_or_create_token(&cfg).expect("reload token");
    assert_eq!(first, second, "an existing token is never regenerated");
    let on_disk = std::fs::read_to_string(cfg.join(TOKEN_FILE)).unwrap();
    assert_eq!(on_disk.trim(), first);
    assert_restrictive(&cfg.join(TOKEN_FILE));
}

#[cfg(unix)]
fn assert_restrictive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token must be owner-only (got {mode:o})");
}

#[cfg(windows)]
fn assert_restrictive(path: &Path) {
    let out = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .expect("icacls");
    let text = String::from_utf8_lossy(&out.stdout);
    let aces: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(":(") || l.contains(":F") || l.contains("(F)"))
        .collect();
    assert!(
        !text.contains("(I)"),
        "inherited ACEs must be stripped: {text}"
    );
    assert_eq!(aces.len(), 1, "exactly one ACE (the owner): {text}");
    let user = std::env::var("USERNAME").unwrap();
    assert!(
        aces[0].to_lowercase().contains(&user.to_lowercase()),
        "the single ACE must be the current user: {text}"
    );
}

#[test]
fn control_routes_reject_bad_or_missing_token() {
    let (addr, token) = start_control(ControlState::default());
    let (status, body) = http(&addr, "GET", "/processes", None, "");
    assert_eq!(status, 401);
    assert!(body["error"].as_str().unwrap().contains("control_token"));
    let (status, _) = http(&addr, "GET", "/processes", Some("wrong"), "");
    assert_eq!(status, 401);
    // Even the discovery route needs it — "every request must carry it".
    let (status, _) = http(&addr, "GET", "/", None, "");
    assert_eq!(status, 401);
    let (status, body) = http(&addr, "GET", "/processes", Some(&token), "");
    assert_eq!(status, 200);
    assert_eq!(body, json!([]));
}

/// `GET /` is rendered from ROUTES, and ROUTES is what dispatch walks: every
/// listed (method, path) reaches a handler (never the unknown-route 404),
/// and nothing outside the table does.
#[test]
fn route_list_matches_the_registered_handlers() {
    let (addr, token) = start_control(ControlState::default());
    let (status, body) = http(&addr, "GET", "/", Some(&token), "");
    assert_eq!(status, 200);
    assert_eq!(body["routes"], json!(route_table_json()));
    assert_eq!(body["routes"].as_array().unwrap().len(), ROUTES.len());

    for route in ROUTES {
        let concrete = route
            .path
            .replace("{id}", "999999")
            .replace("{project}", "999999")
            .replace("{name}", "nope")
            .replace("{rid}", "999999");
        let body = if route.method == "POST" { "{}" } else { "" };
        let (status, reply) = http(&addr, route.method, &concrete, Some(&token), body);
        let err = reply["error"].as_str().unwrap_or("");
        assert!(
            !err.contains("unknown route"),
            "{} {} is listed but not routed ({status}: {err})",
            route.method,
            route.path
        );
        // A wrong method on a known path is 405, proving the path matched.
        let other = if route.method == "GET" { "POST" } else { "GET" };
        let has_sibling = ROUTES
            .iter()
            .any(|r| r.path == route.path && r.method == other);
        if !has_sibling {
            let (status, _) = http(&addr, other, &concrete, Some(&token), body);
            assert_eq!(status, 405, "{other} {concrete}");
        }
    }
    let (status, reply) = http(&addr, "GET", "/nope", Some(&token), "");
    assert_eq!(status, 404);
    assert!(reply["error"].as_str().unwrap().contains("unknown route"));
}

/// The end-to-end loop the host checklist runs, minus the MCP hop: spawn
/// with an argument containing spaces, read the whole screen
///, send_input with submit as ONE write and get
/// a receipt off the byte counter, see it in the journal, see
/// liveness flip, send raw bytes, close only
/// with confirm.
#[test]
fn spawn_input_receipt_journal_close_round_trip() {
    let registry = Registry::default();
    let (addr, token) = start_control(ControlState {
        registry: registry.clone(),
        ..ControlState::default()
    });
    let (command, args) = shell_echoing_argv();
    let (status, body) = http(
        &addr,
        "POST",
        "/processes",
        Some(&token),
        &json!({"command": command, "args": args, "name": "argv probe", "cols": 100, "rows": 10}).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let id = body["id"].as_u64().unwrap();
    assert_eq!(body["name"], "argv probe");

    // THE SPACED ARGUMENT REACHED the child intact.
    wait_for_text(&addr, &token, id, "a b c");

    // LIVENESS IS BYTE FLOW. has_output flipped, child alive.
    let (_, row) = http(&addr, "GET", &format!("/processes/{id}"), Some(&token), "");
    assert_eq!(row["has_output"], true);
    assert_eq!(row["child_alive"], true);
    assert!(row["output_bytes"].as_u64().unwrap() > 0);
    assert!(row["last_output_at"].as_u64().unwrap() > 0);
    assert_eq!(row["cols"], 100);
    // The listing carries the same row.
    let (_, list) = http(&addr, "GET", "/processes", Some(&token), "");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);

    // SUBMIT = TEXT + \r in one write; receipt sees the echo.
    let (status, receipt) = http(
        &addr,
        "POST",
        &format!("/processes/{id}/input"),
        Some(&token),
        &json!({"text": "echo hi-from-control", "submit": true, "wait_ms": 2000}).to_string(),
    );
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["written"], true);
    assert_eq!(receipt["bytes_len"], "echo hi-from-control\r".len());
    assert_eq!(receipt["had_output_within_ms"], true, "{receipt}");
    assert!(receipt["seq_after"].as_u64().unwrap() > receipt["seq_before"].as_u64().unwrap());
    wait_for_text(&addr, &token, id, "hi-from-control");

    // RAW BYTES TAKE the same door (a lone CR here).
    let (status, receipt) = http(
        &addr,
        "POST",
        &format!("/processes/{id}/bytes"),
        Some(&token),
        &json!({"bytes": [13], "wait_ms": 500}).to_string(),
    );
    assert_eq!(status, 200);
    assert_eq!(receipt["bytes_len"], 1);

    // BOTH DELIVERIES ARE in the journal, in order, with the
    // exact payload sizes.
    let (_, journal) = http(&addr, "GET", &format!("/processes/{id}/input_journal"), Some(&token), "");
    let records = journal["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["text"], "echo hi-from-control");
    assert_eq!(records[0]["submit"], true);
    assert_eq!(records[0]["bytes_len"], "echo hi-from-control\r".len());
    assert_eq!(records[1]["submit"], false);
    assert_eq!(records[1]["bytes_len"], 1);
    assert!(records[0]["ts"].as_u64().unwrap() <= records[1]["ts"].as_u64().unwrap());

    // Journal is a ring of 100: 110 no-op writes (empty text) keep it capped.
    for _ in 0..110 {
        registry.write_journaled(id as u32, b"", String::new(), false);
    }
    let (_, journal) = http(&addr, "GET", &format!("/processes/{id}/input_journal"), Some(&token), "");
    assert_eq!(journal["records"].as_array().unwrap().len(), 100);

    // Explicit history window: offset>0 returns history only (no screen).
    let (status, out) = http(
        &addr,
        "GET",
        &format!("/processes/{id}/output?lines=5&offset=1"),
        Some(&token),
        "",
    );
    assert_eq!(status, 200);
    assert_eq!(out["offset"], 1);
    assert!(out["rows"].as_array().unwrap().len() <= 5);

    // Close requires confirm.
    let (status, body) = http(&addr, "POST", &format!("/processes/{id}/close"), Some(&token), "{}");
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("confirm"));
    assert!(registry.handle(id as u32).is_some(), "not closed without confirm");
    let (status, _) = http(
        &addr,
        "POST",
        &format!("/processes/{id}/close"),
        Some(&token),
        r#"{"confirm": true}"#,
    );
    assert_eq!(status, 200);
    let (status, _) = http(&addr, "GET", &format!("/processes/{id}"), Some(&token), "");
    assert_eq!(status, 404);
}

/// Review: caller-controlled sizes are bounded with a 400, never a
/// panic (zero-row grid) or a wrapped-around window (lines + offset).
#[test]
fn spawn_and_output_reject_out_of_range_sizes() {
    let registry = Registry::default();
    let (addr, token) = start_control(ControlState {
        registry: registry.clone(),
        ..ControlState::default()
    });
    let (status, body) = http(&addr, "POST", "/processes", Some(&token), r#"{"rows": 0}"#);
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("rows"));
    let (status, _) = http(&addr, "POST", "/processes", Some(&token), r#"{"cols": 70000}"#);
    assert_eq!(status, 400, "u16 overflow is a bad body, not a spawn");
    assert!(registry.list().is_empty(), "nothing spawned");

    let (command, args) = shell_echoing_argv();
    let (status, body) = http(
        &addr,
        "POST",
        "/processes",
        Some(&token),
        &json!({"command": command, "args": args}).to_string(),
    );
    assert_eq!(status, 200);
    let id = body["id"].as_u64().unwrap();
    let (status, _) = http(
        &addr,
        "GET",
        &format!("/processes/{id}/output?lines=18446744073709551615&offset=1"),
        Some(&token),
        "",
    );
    assert_eq!(status, 400, "overflowing window is refused, not wrapped");
    let (status, out) = http(&addr, "GET", &format!("/processes/{id}/output?lines=5&offset=2"), Some(&token), "");
    assert_eq!(status, 200);
    assert!(out["rows"].as_array().unwrap().len() <= 5);
    registry.close(id as u32);
}

/// Cleanup: `GET /` also says WHICH instance answered. 8324 is a
/// single fixed port, so when two chappa-ai builds are open one silently
/// serves every agent tool call — pid + config dir make that diagnosable.
#[test]
fn the_route_table_identifies_the_instance() {
    let (addr, token) = start_control(ControlState {
        config_dir: std::path::PathBuf::from("C:/tmp/chappa-cfg"),
        ..ControlState::default()
    });
    let (status, body) = http(&addr, "GET", "/", Some(&token), "");
    assert_eq!(status, 200);
    assert_eq!(body["instance"]["pid"], std::process::id());
    assert_eq!(body["instance"]["config_dir"], "C:/tmp/chappa-cfg");
    // Still the same table alongside it.
    assert_eq!(body["routes"], json!(route_table_json()));
}

/// The control row is now `TerminalSnapshot` + liveness (one shared `Entry`
/// conversion instead of two hand-copied structs). The WIRE SHAPE is the
/// contract — `CONTROL_API.md`, the MCP tool descriptions and any curl script
/// read these exact keys — so pin the whole key set and the shared values
/// against the debug listing's row for the same terminal.
#[test]
fn the_control_row_keeps_its_exact_wire_shape() {
    let registry = Registry::default();
    let (addr, token) = start_control(ControlState {
        registry: registry.clone(),
        ..ControlState::default()
    });
    let (command, args) = shell_echoing_argv();
    let (status, spawned) = http(
        &addr,
        "POST",
        "/processes",
        Some(&token),
        &json!({"command": command, "args": args, "name": "shape probe", "cols": 90, "rows": 12})
            .to_string(),
    );
    assert_eq!(status, 200, "{spawned}");
    let id = spawned["id"].as_u64().unwrap();
    wait_for_text(&addr, &token, id, "a b c");

    let (_, row) = http(&addr, "GET", &format!("/processes/{id}"), Some(&token), "");
    let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "agent",
            "child_alive",
            "close_on_exit",
            "cols",
            "exit_code",
            "has_output",
            "id",
            "kind",
            "last_output_at",
            "name",
            "output_bytes",
            "project_id",
            "rows",
            "seq",
            "status",
            "uuid",
        ],
        "the flattened base must contribute exactly the debug row's fields"
    );
    // The stable identity is a 32-hex uuid, and a bare
    // terminal belongs to no project.
    assert_eq!(row["uuid"].as_str().unwrap().len(), 32, "{}", row["uuid"]);
    assert_eq!(row["project_id"], Value::Null);
    assert_eq!(row["id"], id);
    assert_eq!(row["name"], "shape probe");
    assert_eq!(row["cols"], 90);
    assert_eq!(row["rows"], 12);
    // The status string is the status vocabulary, exactly as the rail listing
    // reports it for the same terminal (same `Entry` → snapshot conversion).
    let rail = registry.list();
    let rail_row = rail.iter().find(|r| r.id as u64 == id).expect("rail row");
    assert_eq!(row["status"], rail_row.status_str());
    assert_eq!(row["exit_code"], json!(rail_row.exit_code));
    // And the listing carries the identical object.
    let (_, list) = http(&addr, "GET", "/processes", Some(&token), "");
    assert_eq!(list[0], row);
    registry.close(id as u32);
}

/// The server runs a FIXED pool of worker threads (no thread per request):
/// a burst still gets answered, and the pool keeps serving afterwards.
#[test]
fn a_burst_of_requests_is_served_by_the_worker_pool() {
    let (addr, token) = start_control(ControlState::default());
    let mut clients = Vec::new();
    for _ in 0..6 {
        let (addr, token) = (addr.clone(), token.clone());
        clients.push(std::thread::spawn(move || {
            for _ in 0..4 {
                let (status, body) = http(&addr, "GET", "/processes", Some(&token), "");
                assert_eq!(status, 200);
                assert_eq!(body, json!([]));
            }
        }));
    }
    for client in clients {
        client.join().expect("every request answered");
    }
    // Still alive after the burst.
    let (status, _) = http(&addr, "GET", "/processes", Some(&token), "");
    assert_eq!(status, 200);
}

/// 2026-08-30: a command with embedded double quotes must survive `cmd /C`
/// on Windows. The plain form is broken by CreateProcessW's `\"` escaping;
/// the env-var indirection (`cmd /C call %CHAPPA_CMD%`) is what the project
/// runner uses — prove it end to end with a real child.
#[cfg(windows)]
#[test]
fn cmd_indirection_preserves_embedded_quotes() {
    let registry = Registry::default();
    let (addr, token) = start_control(ControlState {
        registry: registry.clone(),
        ..ControlState::default()
    });
    let body = json!({
        "command": "cmd",
        "args": ["/K", "call", "%CHAPPA_CMD%"],
        "env": {"CHAPPA_CMD": "py -c \"print('quoted' + ' ok')\""},
    });
    let (status, spawned) = http(&addr, "POST", "/processes", Some(&token), &body.to_string());
    assert_eq!(status, 200, "{spawned}");
    let id = spawned["id"].as_u64().unwrap();
    let rows = wait_for_text(&addr, &token, id, "quoted ok");
    assert!(
        rows.iter().any(|r| r.contains("quoted ok")),
        "python never printed through cmd indirection: {rows:?}"
    );
    registry.close(id as u32);
}

/// Idempotent project routes without an initialized store answer a clear
/// 500, never a hang; an unknown project is a 404.
#[test]
fn project_routes_degrade_cleanly_without_a_store() {
    let (addr, token) = start_control(ControlState::default());
    let (status, body) = http(&addr, "GET", "/projects", Some(&token), "");
    assert_eq!(status, 500);
    assert!(body["error"].as_str().unwrap().contains("not initialized"));
    let (status, _) = http(
        &addr,
        "POST",
        "/projects/1/processes/x/stop",
        Some(&token),
        "",
    );
    assert_eq!(status, 500);
}

/// The debug surface is UNTOUCHED: same routes, no token, on its own server.
#[cfg(debug_assertions)]
#[test]
fn debug_routes_stay_tokenless() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let registry = Registry::default();
    std::thread::spawn(move || app_lib::debug_http::serve(server, registry));
    let (status, body) = http(&addr, "GET", "/debug/terminals", None, "");
    assert_eq!(status, 200);
    assert_eq!(body, json!([]));
    // And the control routes are NOT served there.
    let (status, _) = http(&addr, "GET", "/processes", None, "");
    assert_eq!(status, 404);
}

/// The checked-in doc is the emitter's output. `CONTROL_API_WRITE=1`
/// regenerates it (also mirrored as a unit test in control_http.rs).
#[test]
fn control_api_doc_is_in_sync() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../CONTROL_API.md");
    let rendered = control_http::render_api_doc();
    if std::env::var_os("CONTROL_API_WRITE").is_some() {
        std::fs::write(&path, &rendered).expect("write CONTROL_API.md");
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "desktop/CONTROL_API.md is stale — run `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop --test control control_api_doc`"
    );
}
