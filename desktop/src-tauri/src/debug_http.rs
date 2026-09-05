//! Debug-build parity-harness HTTP server.
//! Bound to 127.0.0.1:8323, JSON in/out, no auth. An external harness drives
//! chappa-ai desktop over this surface and checks process semantics and
//! terminal text.
//!
//! Routes:
//! - `GET    /debug/terminals`                     → `[{id, name, status, cols, rows, seq}]`
//! - `POST   /debug/terminals` `{command, args?, cwd?, cols?, rows?}` → `{id}`
//! - `GET    /debug/terminals/<id>/text?scrollback=N` → `{rows: [String]}` — grid via
//!   `TermHandle::dump_text` (viewport + last N history lines).
//! - `GET    /debug/terminals/<id>/events?since=<seq>` → last 100 JSON events with
//!   `seq > since` (title/bell/notify/links/exited, each with a monotonic seq).
//! - `POST   /debug/terminals/<id>/write` `{text}` → raw bytes to the PTY
//!   (no key encoding — the harness sends exact bytes).
//! - `POST   /debug/terminals/<id>/key` `{kind, shift?}` → a real `WriteKey`
//!   through the actor's key path (mode-aware encoding, synthetic prompt
//!   marks — everything `/write` bypasses). `kind` currently: "enter".
//! - `GET    /debug/terminals/<id>/modes` → `{alt_screen, kitty_flags}` —
//!   the mode bits that gate input-side behavior (marks forensics).
//! - `POST   /debug/terminals/<id>/resize` `{cols, rows}`
//! - `DELETE /debug/terminals/<id>`                → close.
//!
//! tiny_http, not an async framework: seven routes, and the harness is the
//! only client. Each request is handled on its own thread so a slow
//! `dump_text` round-trip can never block the listing routes. The whole
//! module is `#[cfg(debug_assertions)]` — release builds never reference it
//! (kept a plain dependency because Cargo target tables cannot key on
//! `debug_assertions`).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use term_core::pty::PtySpec;

// The HTTP plumbing (JSON responses, body reading with its size cap, query
// parsing) is the control surface's — the control surface grew out of this module's routes,
// so the two servers share one implementation instead of two copies that
// drift. The ROUTES table and the bearer token stay control-only.
use crate::control_http::{json, query_u64, read_body};
use crate::registry::{actor_config, NullSink, Registry, TermId};

const BIND: &str = "127.0.0.1:8323";

#[derive(Debug, Deserialize)]
struct CreateBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

#[derive(Debug, Deserialize)]
struct WriteBody {
    text: String,
}

#[derive(Debug, Deserialize)]
struct KeyBody {
    kind: String,
    #[serde(default)]
    shift: bool,
}

#[derive(Debug, Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// Spawn the harness server on its own thread. Called once from `setup` in
/// debug builds; a bind failure is loud but not fatal — the app still runs.
pub fn start(registry: Registry) {
    std::thread::spawn(move || run(registry));
}

fn run(registry: Registry) {
    let server = match tiny_http::Server::http(BIND) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("[debug_http] failed to bind {BIND}: {err}");
            return;
        }
    };
    eprintln!("[debug_http] parity harness on http://{BIND}");
    serve(server, registry);
}

/// Serve forever on an already-bound server. Split out of `run` so the
/// tokenless-debug-routes test can bind an ephemeral port; the
/// routes themselves are unchanged.
pub fn serve(server: tiny_http::Server, registry: Registry) {
    for request in server.incoming_requests() {
        let registry = registry.clone();
        std::thread::spawn(move || respond(registry, request));
    }
}

fn respond(registry: Registry, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_owned();
    let body = read_body(&mut request.as_reader());
    let response = route(&registry, &method, &url, body);
    let _ = request.respond(response);
}

fn route(
    registry: &Registry,
    method: &tiny_http::Method,
    url: &str,
    body: String,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url, ""),
    };
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // Every route hangs off /debug/terminals; anything else is a 404.
    if segs.len() < 2 || segs[0] != "debug" || segs[1] != "terminals" {
        return json(404, json!({"error": "not found"}));
    }

    let id: Option<TermId> = segs.get(2).and_then(|s| s.parse().ok());

    match (method.as_str(), segs.len(), id) {
        ("GET", 2, _) => json(200, json!(registry.list())),
        ("POST", 2, _) => create(registry, &body),
        ("DELETE", 3, Some(id)) => close(registry, id),
        ("GET", 4, Some(id)) => match segs[3] {
            "text" => text(registry, id, query),
            "events" => events(registry, id, query),
            "modes" => modes(registry, id),
            _ => json(404, json!({"error": "not found"})),
        },
        ("POST", 4, Some(id)) => match segs[3] {
            "write" => write(registry, id, &body),
            "key" => key(registry, id, &body),
            "resize" => resize(registry, id, &body),
            _ => json(404, json!({"error": "not found"})),
        },
        _ => json(400, json!({"error": "bad request"})),
    }
}

fn create(registry: &Registry, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let spec: CreateBody = match serde_json::from_str(body) {
        Ok(spec) => spec,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    if spec.command.is_empty() {
        return json(400, json!({"error": "command required"}));
    }
    // `settings: None` = the built-in defaults: the parity harness
    // asserts behavior at them, so it never reads settings.json.
    let cfg = actor_config(
        PtySpec {
            command: spec.command,
            args: spec.args,
            cwd: spec.cwd.map(Into::into),
            env: Vec::new(),
            cols: spec.cols,
            rows: spec.rows,
        },
        None,
        None,
    );
    let name = cfg.spec.command.clone();
    // Harness terminals have no webview channel: the harness reads text back
    // via /text, so the sink is a no-op. They still show up in /terminals.
    match registry.create_terminal(cfg, Arc::new(NullSink), None, name) {
        Ok(id) => json(200, json!({"id": id})),
        Err(err) => json(500, json!({"error": format!("spawn failed: {err}")})),
    }
}

fn text(
    registry: &Registry,
    id: TermId,
    query: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let handle = match registry.handle(id) {
        Some(handle) => handle,
        None => return json(404, json!({"error": format!("no such terminal: {id}")})),
    };
    // scrollback=N → the last N history lines; missing/bad → 0.
    let lines = query_u64(query, "scrollback").unwrap_or(0) as usize;
    let rows = handle.dump_text(lines);
    json(200, json!({"rows": rows}))
}

fn events(
    registry: &Registry,
    id: TermId,
    query: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let since = query_u64(query, "since").unwrap_or(0);
    json(200, json!(registry.events_since(id, since)))
}

fn write(
    registry: &Registry,
    id: TermId,
    body: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let handle = match registry.handle(id) {
        Some(handle) => handle,
        None => return json(404, json!({"error": format!("no such terminal: {id}")})),
    };
    let body: WriteBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    handle.write_input(body.text.as_bytes());
    json(200, json!({"ok": true}))
}

/// A real `WriteKey` through the actor (mode-aware encoding + the
/// synthetic-marks hook). `/write` is a raw paste-path write and can never
/// exercise those.
fn key(
    registry: &Registry,
    id: TermId,
    body: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    use term_core::keys::{Key, KeyEvent, Mods};
    let handle = match registry.handle(id) {
        Some(handle) => handle,
        None => return json(404, json!({"error": format!("no such terminal: {id}")})),
    };
    let body: KeyBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    let key = match body.kind.as_str() {
        "enter" => Key::Enter,
        other => return json(400, json!({"error": format!("unsupported kind: {other}")})),
    };
    let mods = if body.shift { Mods::SHIFT } else { Mods::EMPTY };
    handle.write_key(KeyEvent { key, mods });
    json(200, json!({"ok": true}))
}

fn modes(registry: &Registry, id: TermId) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let handle = match registry.handle(id) {
        Some(handle) => handle,
        None => return json(404, json!({"error": format!("no such terminal: {id}")})),
    };
    match handle.modes() {
        Some(m) => json(
            200,
            json!({"alt_screen": m.alt_screen, "kitty_flags": m.kitty_flags}),
        ),
        None => json(500, json!({"error": "actor did not reply"})),
    }
}

fn resize(
    registry: &Registry,
    id: TermId,
    body: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let handle = match registry.handle(id) {
        Some(handle) => handle,
        None => return json(404, json!({"error": format!("no such terminal: {id}")})),
    };
    let body: ResizeBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    handle.resize(body.cols, body.rows);
    registry.set_size(id, body.cols, body.rows);
    json(200, json!({"ok": true}))
}

fn close(registry: &Registry, id: TermId) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    // `close_silent` — debug-harness terminals are test scaffolding
    // and must emit neither `term://created` (never adopted) nor
    // `term://closed` (the exclusion, mirrored).
    match registry.close_silent(id) {
        Some(_) => json(200, json!({"ok": true})),
        None => json(404, json!({"error": format!("no such terminal: {id}")})),
    }
}

