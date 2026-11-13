//! chappa-ai-mcp tests: every
//! tool round-trips against a `FakeControlServer` (canned HTTP), the token is
//! attached, the unreachable-app error names the fix, submit atomicity (ONE
//! request carrying text + submit — the app writes text+\r as one payload),
//! a tool-schema snapshot including annotations, and the identity check: the
//! token goes only to a server that proved it is the app.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use chappa_ai_mcp::{
    call_tool, handle_message, identity_proof, tools, Client, Endpoint, ToolError, TokenSource,
    ENDPOINT_FILE,
};
use serde_json::{json, Value};

/// One recorded request.
#[derive(Debug, Clone, PartialEq)]
struct Req {
    method: String,
    path: String,
    auth: Option<String>,
    /// The `X-Chappa-Actor` header, when sent.
    actor: Option<String>,
    body: Value,
}

/// A canned control surface: records every request, answers 200 with a
/// per-path JSON body (or 404 for unknown paths, 401 without the token).
/// `GET /identity` is answered like the app does (a proof keyed by the
/// accepted token) on a keep-alive connection, and recorded separately in
/// `identity_requests`.
struct FakeControlServer {
    addr: String,
    requests: Arc<Mutex<Vec<Req>>>,
    identity_requests: Arc<Mutex<Vec<Req>>>,
    /// The token this server currently accepts — swappable, so the
    /// token-cache test can make the app "regenerate" it mid-session.
    accepted: Arc<Mutex<String>>,
}

const TOKEN: &str = "fake-token-123";
const NONCE: &str = "fake-nonce";

/// Read one request (headers + Content-Length body) off a connection,
/// keeping any bytes past it in `pending`. `None` = the peer hung up.
fn read_request(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Option<Req> {
    let mut buf = [0u8; 4096];
    let (head_end, len) = loop {
        if let Some(pos) = find(pending, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..pos]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if pending.len() >= pos + 4 + len {
                break (pos, len);
            }
        }
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return None;
        }
        pending.extend_from_slice(&buf[..n]);
    };
    let raw: Vec<u8> = pending.drain(..head_end + 4 + len).collect();
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let body = String::from_utf8_lossy(&raw[head_end + 4..]).to_string();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let header_lines: Vec<&str> = lines.collect();
    let header = |name: &str| {
        header_lines
            .iter()
            .find_map(|l| l.strip_prefix(name))
            .map(str::to_owned)
    };
    Some(Req {
        method,
        path,
        auth: header("Authorization: "),
        actor: header("X-Chappa-Actor: "),
        body: serde_json::from_str(&body).unwrap_or(Value::Null),
    })
}

fn write_reply(stream: &mut TcpStream, status: u16, reply: &Value, close: bool) {
    let payload = reply.to_string();
    let connection = if close { "Connection: close\r\n" } else { "" };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{connection}\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

impl FakeControlServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests: Arc<Mutex<Vec<Req>>> = Arc::default();
        let identity_requests: Arc<Mutex<Vec<Req>>> = Arc::default();
        let (log, id_log) = (requests.clone(), identity_requests.clone());
        let accepted = Arc::new(Mutex::new(TOKEN.to_owned()));
        let expect = accepted.clone();
        let bound = addr.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut pending = Vec::new();
                while let Some(req) = read_request(&mut stream, &mut pending) {
                    if let Some(query) = req.path.strip_prefix("/identity?") {
                        let challenge = query.strip_prefix("challenge=").unwrap_or("").to_owned();
                        id_log.lock().unwrap().push(req);
                        let token = expect.lock().unwrap().clone();
                        let proof = identity_proof(&token, &challenge, &bound, NONCE);
                        write_reply(
                            &mut stream,
                            200,
                            &json!({"addr": bound, "nonce": NONCE, "proof": proof}),
                            false,
                        );
                        continue;
                    }
                    log.lock().unwrap().push(req.clone());
                    let want = format!("Bearer {}", expect.lock().unwrap());
                    let (status, reply) = if req.auth.as_deref() != Some(&want) {
                        (401, json!({"error": "unauthorized"}))
                    } else {
                        canned(&req.method, &req.path)
                    };
                    write_reply(&mut stream, status, &reply, true);
                    break;
                }
            }
        });
        Self {
            addr,
            requests,
            identity_requests,
            accepted,
        }
    }

    fn client(&self) -> Client {
        Client::new(format!("http://{}", self.addr), TokenSource::Literal(TOKEN.into()))
    }

    fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    fn identity_requests(&self) -> Vec<Req> {
        self.identity_requests.lock().unwrap().clone()
    }

    /// The app regenerated its control token.
    fn accept_only(&self, token: &str) {
        *self.accepted.lock().unwrap() = token.to_owned();
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Canned replies keyed on (method, path-without-query).
fn canned(method: &str, path: &str) -> (u16, Value) {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("GET", "/processes") => (200, json!([{"id": 1, "name": "shell", "status": "running", "has_output": true}])),
        ("POST", "/processes") => (200, json!({"id": 9, "name": "probe"})),
        ("GET", "/processes/1") => (200, json!({"id": 1, "status": "running", "child_alive": true})),
        ("GET", "/processes/1/output") => (200, json!({"rows": ["$ echo hi", "hi"]})),
        ("GET", "/processes/1/input_journal") => (200, json!({"records": []})),
        ("POST", "/processes/1/input") => (200, json!({"written": true, "seq_before": 0, "seq_after": 5, "had_output_within_ms": true})),
        ("POST", "/processes/1/bytes") => (200, json!({"written": true})),
        ("POST", "/processes/1/close") => (200, json!({"ok": true, "id": 1, "verification": null})),
        ("GET", "/agent_tools") => (200, json!([{"id": 3, "name": "worker", "command": "docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast", "tool_type": "opencode", "enabled": true, "model": "model-fast", "runtime": {"kind": "docker_exec", "container": "dev-worker"}, "container": "dev-worker"}])),
        ("POST", "/agents") => (200, json!({"process_id": 12, "term_id": 12, "name": "worker", "agent_instructions": "You are running as chappa-ai process 12.", "prompt_receipt": {"delivered": true}})),
        ("POST", "/agents/reap") => (200, json!({"results": [{"container": "dev-worker", "user": "dev", "pid": 42, "uuid": "orphan-uuid", "verdict": "gone"}]})),
        ("GET", "/processes/12/agent_events") => (200, json!({"id": 12, "since": 40, "events": [{"id": 12, "seq": 41, "kind": "awaiting_input", "payload": {"after": "result"}}]})),
        ("GET", "/projects") => (200, json!([{"id": 2, "name": "dev", "open": true}])),
        ("GET", "/projects/2") => (200, json!({"id": 2, "processes": []})),
        ("POST", "/projects/2/processes/import%20%2B%20summarize/start") => (200, json!({"status": "starting", "term_id": 4, "pending": false})),
        ("POST", "/projects/2/processes/import%20%2B%20summarize/stop") => (200, json!({"status": "stopped"})),
        ("POST", "/projects/2/processes/import%20%2B%20summarize/restart") => (200, json!({"status": "starting"})),
        ("GET", "/processes/404") => (404, json!({"error": "no such terminal: 404"})),
        // ---- records ----
        ("GET", "/records/abc123/runs") => (200, json!({"record_id": "abc123", "runs": [{"run_uuid": "r1", "status": "exited", "started_at_ms": 1, "ended_at_ms": 2, "exit_code": 0, "success": true, "tail_kind": "text", "tail": ["done"]}]})),
        ("POST", "/records/abc123/start") => (200, json!({"record_id": "abc123", "kind": "agent", "term_id": 13, "run_uuid": "r2", "pending": false})),
        ("POST", "/records/abc123/remove") => (200, json!({"record_id": "abc123", "kind": "agent", "removed": true, "runs_deleted": 2})),
        ("POST", "/records/abc123/resume") => (200, json!({"record_id": "abc123", "kind": "agent", "term_id": 14, "run_uuid": "r3", "session_id": "ses-1", "check": "quiet"})),
        ("POST", "/processes/12/session") => (200, json!({"id": 12, "record_id": "abc123", "run_uuid": "r3", "durable": true})),
        ("POST", "/processes/7/session") => (200, json!({"id": 7, "record_id": null, "run_uuid": null, "durable": false})),
        // ---- coordination ----
        ("GET", "/scratchpads") => (200, json!({"scratchpads": [{"scratchpad_id": 5, "name": "pad", "revision": 3, "updated_by": "12"}], "total": 1})),
        ("POST", "/scratchpads") => (200, json!({"scratchpad_id": 5, "project_id": 6, "revision": 1, "name": "pad"})),
        ("GET", "/scratchpads/tags") => (200, json!({"tags": ["eval"]})),
        ("GET", "/scratchpads/5") => (200, json!({"scratchpad_id": 5, "revision": 3, "content": "# pad\n"})),
        ("POST", "/scratchpads/5") => (200, json!({"scratchpad_id": 5, "project_id": 6, "revision": 4, "name": "pad"})),
        ("GET", "/scratchpads/5/find") => (200, json!({"matches": [{"line": 1, "text": "# pad"}], "total_matches": 1})),
        ("GET", "/scratchpads/5/tail") => (200, json!({"content": "last\n", "lines": 1})),
        ("POST", "/scratchpads/5/rename") => (200, json!({"scratchpad_id": 5, "revision": 4, "name": "renamed"})),
        ("POST", "/scratchpads/5/append") => (200, json!({"scratchpad_id": 5, "revision": 4, "line_count": 2})),
        ("POST", "/scratchpads/5/append_section") => (200, json!({"scratchpad_id": 5, "revision": 4, "heading": "Log"})),
        ("POST", "/scratchpads/5/edit") => (409, json!({"error": "revision_conflict", "message": "expected_revision does not match the current revision 3; re-read and retry", "current_revision": 3, "current_content": "# pad\n"})),
        ("POST", "/scratchpads/5/tags/add") => (200, json!({"scratchpad_id": 5, "revision": 4, "tags": ["a"]})),
        ("POST", "/scratchpads/5/tags/remove") => (200, json!({"scratchpad_id": 5, "revision": 4, "tags": []})),
        ("POST", "/scratchpads/5/clear") => (200, json!({"scratchpad_id": 5, "revision": 4, "cleared": true})),
        ("POST", "/scratchpads/5/delete") => (200, json!({"scratchpad_id": 5, "project_id": 6, "deleted": true})),
        ("POST", "/scratchpads/5/archive") => (200, json!({"scratchpad_id": 5, "revision": 4, "archived": true})),
        ("POST", "/scratchpads/5/transfer") => (200, json!({"scratchpad_id": 5, "project_id": 9, "revision": 4})),
        ("GET", "/todos") => (200, json!({"todos": [{"todo_id": 7, "title": "t", "is_blocked": false}], "total": 1})),
        ("POST", "/todos") => (200, json!({"todo_id": 7, "project_id": 6, "revision": 1, "title": "t"})),
        ("GET", "/todos/tags") => (200, json!({"tags": ["x"]})),
        ("GET", "/todos/7") => (200, json!({"todo_id": 7, "title": "t", "body": "b", "revision": 2, "comments": []})),
        ("POST", "/todos/7") => (409, json!({"error": "locked", "message": "todo is locked by actor 2 until 1700000000000 (unix ms)", "locked_by": "2", "lock_expires_at": 1700000000000u64})),
        ("POST", "/todos/7/tags/add") => (200, json!({"todo_id": 7, "revision": 3, "tag": "x", "tags": ["x"]})),
        ("POST", "/todos/7/tags/remove") => (200, json!({"todo_id": 7, "revision": 3, "tag": "x", "tags": []})),
        ("POST", "/todos/7/blockers/set") => (200, json!({"todo_id": 7, "revision": 3, "blocker_ids": [8], "is_blocked": true})),
        ("POST", "/todos/7/blockers/add") => (200, json!({"todo_id": 7, "revision": 3, "blocker_ids": [8], "is_blocked": true})),
        ("POST", "/todos/7/blockers/remove") => (200, json!({"todo_id": 7, "revision": 3, "blocker_ids": [], "is_blocked": false})),
        ("POST", "/todos/7/complete") => (200, json!({"todo_id": 7, "revision": 3, "completed": true, "affected_todo_ids": []})),
        ("POST", "/todos/7/lock") => (200, json!({"todo_id": 7, "revision": 3, "locked_by": "12", "lock_expires_at": 1})),
        ("POST", "/todos/7/unlock") => (200, json!({"todo_id": 7, "revision": 3, "locked_by": null})),
        ("POST", "/todos/7/transfer") => (200, json!({"todo_id": 7, "project_id": 9, "revision": 3, "target_project_id": 9, "affected_todo_ids": []})),
        ("POST", "/todos/7/delete") => (200, json!({"todo_id": 7, "project_id": 6, "deleted": true, "affected_todo_ids": []})),
        ("GET", "/todos/7/comments") => (200, json!({"todo_id": 7, "comments": [], "total": 0})),
        ("POST", "/todos/7/comments") => (200, json!({"comment_id": 3, "todo_id": 7})),
        ("POST", "/todo_comments/3") => (200, json!({"comment_id": 3, "todo_id": 7})),
        ("POST", "/todo_comments/3/delete") => (200, json!({"comment_id": 3, "todo_id": 7, "deleted": true})),
        // ---- timers ----
        ("POST", "/timers") => (200, json!({"timer_id": 21, "id": 21, "kind": "delay", "status": "pending", "owner": "12", "delivery_process_id": 12, "next_fire_at": 1700000060000u64, "fired_count": 0})),
        ("POST", "/timers/idle_any") => (200, json!({"timer_id": 22, "status": "scheduled", "already_idle": [4], "waiting_on": [5], "deadline_at": 1700000600000u64, "note": "processes already idle at schedule time are ignored; `any` waits for a NEW idle transition"})),
        ("POST", "/timers/idle_all") => (200, json!({"timer_id": null, "status": "already_satisfied", "already_idle": [4, 5], "waiting_on": [], "note": "every watched process was already idle; no timer was created and no body was delivered"})),
        ("GET", "/timers") => (200, json!({"timers": [{"id": 21, "kind": "delay", "status": "fired", "fired_count": 1, "delivery": {"firing_id": 3, "process_id": 12, "status": "failed", "delivered": false, "reason": "condition", "error": "process gone", "receipt": null, "coalesced_with": null, "at": 1700000060000u64}}], "total": 1, "limit": 50, "offset": 0})),
        ("POST", "/timers/21/cancel") => (200, json!({"timer_id": 21, "project_id": 6, "cancelled": true, "status": "cancelled"})),
        ("POST", "/timers/21/pause") => (200, json!({"timer_id": 21, "project_id": 6, "paused": true, "status": "paused"})),
        ("POST", "/timers/21/resume") => (200, json!({"timer_id": 21, "project_id": 6, "resumed": true, "status": "pending"})),
        _ => (404, json!({"error": "unknown route (GET / lists the surface)"})),
    }
}

fn is_coordination(name: &str) -> bool {
    name.starts_with("scratchpad_") || name.starts_with("todo_")
}

fn is_timer(name: &str) -> bool {
    name.starts_with("timer_")
}

/// Every tool → exactly one request, with the token, at the documented
/// method/path, with the documented body; the reply comes back verbatim.
#[test]
fn every_tool_round_trips_with_the_token_attached() {
    let server = FakeControlServer::start();
    let client = server.client();
    let cases: Vec<(&str, Value, &str, &str, Value, Value)> = vec![
        ("list_processes", json!({}), "GET", "/processes", Value::Null, json!([{"id": 1, "name": "shell", "status": "running", "has_output": true}])),
        ("get_process_status", json!({"id": 1}), "GET", "/processes/1", Value::Null, json!({"id": 1, "status": "running", "child_alive": true})),
        ("get_process_output", json!({"id": 1}), "GET", "/processes/1/output", Value::Null, json!({"rows": ["$ echo hi", "hi"]})),
        ("get_process_output", json!({"id": 1, "lines": 50, "offset": 10}), "GET", "/processes/1/output?lines=50&offset=10", Value::Null, json!({"rows": ["$ echo hi", "hi"]})),
        ("get_input_journal", json!({"id": 1}), "GET", "/processes/1/input_journal", Value::Null, json!({"records": []})),
        ("list_projects", json!({}), "GET", "/projects", Value::Null, json!([{"id": 2, "name": "dev", "open": true}])),
        ("get_project", json!({"id": 2}), "GET", "/projects/2", Value::Null, json!({"id": 2, "processes": []})),
        ("spawn_terminal", json!({"command": "cmd", "args": ["/K", "echo", "a b c"], "name": "probe"}), "POST", "/processes", json!({"command": "cmd", "args": ["/K", "echo", "a b c"], "name": "probe"}), json!({"id": 9, "name": "probe"})),
        ("spawn_terminal", json!({}), "POST", "/processes", json!({}), json!({"id": 9, "name": "probe"})),
        ("send_input", json!({"id": 1, "text": "echo hi", "submit": true}), "POST", "/processes/1/input", json!({"text": "echo hi", "submit": true}), json!({"written": true, "seq_before": 0, "seq_after": 5, "had_output_within_ms": true})),
        ("send_bytes", json!({"id": 1, "bytes": [3], "wait_ms": 100}), "POST", "/processes/1/bytes", json!({"bytes": [3], "wait_ms": 100}), json!({"written": true})),
        ("start_process", json!({"project_id": 2, "name": "import + summarize", "wait_ms": 500}), "POST", "/projects/2/processes/import%20%2B%20summarize/start", json!({"wait_ms": 500}), json!({"status": "starting", "term_id": 4, "pending": false})),
        ("stop_process", json!({"project_id": 2, "name": "import + summarize"}), "POST", "/projects/2/processes/import%20%2B%20summarize/stop", json!({}), json!({"status": "stopped"})),
        ("restart_process", json!({"project_id": 2, "name": "import + summarize"}), "POST", "/projects/2/processes/import%20%2B%20summarize/restart", json!({}), json!({"status": "starting"})),
        ("close_terminal", json!({"id": 1, "confirm": true}), "POST", "/processes/1/close", json!({"confirm": true}), json!({"ok": true, "id": 1, "verification": null})),
        ("list_agent_tools", json!({}), "GET", "/agent_tools", Value::Null, json!([{"id": 3, "name": "worker", "command": "docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast", "tool_type": "opencode", "enabled": true, "model": "model-fast", "runtime": {"kind": "docker_exec", "container": "dev-worker"}, "container": "dev-worker"}])),
        // extra_args travel as an ARRAY — "two words here" is one element on the wire.
        ("spawn_agent", json!({"agent_tool_id": 3, "project_id": 6, "extra_args": ["--prompt", "two words here"], "prompt": "go", "force": false}), "POST", "/agents", json!({"agent_tool_id": 3, "project_id": 6, "extra_args": ["--prompt", "two words here"], "prompt": "go", "force": false}), json!({"process_id": 12, "term_id": 12, "name": "worker", "agent_instructions": "You are running as chappa-ai process 12.", "prompt_receipt": {"delivered": true}})),
        ("spawn_agent", json!({"agent_tool_id": 3}), "POST", "/agents", json!({"agent_tool_id": 3}), json!({"process_id": 12, "term_id": 12, "name": "worker", "agent_instructions": "You are running as chappa-ai process 12.", "prompt_receipt": {"delivered": true}})),
        // parent_process_id + close_on_exit round-trip on the schema
        // (both serde-defaulted, so an absent body still parses — see above).
        ("spawn_agent", json!({"agent_tool_id": 3, "parent_process_id": 7, "close_on_exit": true}), "POST", "/agents", json!({"agent_tool_id": 3, "parent_process_id": 7, "close_on_exit": true}), json!({"process_id": 12, "term_id": 12, "name": "worker", "agent_instructions": "You are running as chappa-ai process 12.", "prompt_receipt": {"delivered": true}})),
        // The event ring since a cursor.
        ("get_agent_events", json!({"id": 12, "since": 40}), "GET", "/processes/12/agent_events?since=40", Value::Null, json!({"id": 12, "since": 40, "events": [{"id": 12, "seq": 41, "kind": "awaiting_input", "payload": {"after": "result"}}]})),
        ("get_agent_events", json!({"id": 12}), "GET", "/processes/12/agent_events?since=0", Value::Null, json!({"id": 12, "since": 40, "events": [{"id": 12, "seq": 41, "kind": "awaiting_input", "payload": {"after": "result"}}]})),
        // The orphan reaper.
        ("reap_orphans", json!({}), "POST", "/agents/reap", json!({}), json!({"results": [{"container": "dev-worker", "user": "dev", "pid": 42, "uuid": "orphan-uuid", "verdict": "gone"}]})),
        // Durable records: history with its tail, a fresh start, a confirmed removal.
        ("get_record_runs", json!({"record_id": "abc123"}), "GET", "/records/abc123/runs", Value::Null, json!({"record_id": "abc123", "runs": [{"run_uuid": "r1", "status": "exited", "started_at_ms": 1, "ended_at_ms": 2, "exit_code": 0, "success": true, "tail_kind": "text", "tail": ["done"]}]})),
        ("get_record_runs", json!({"record_id": "abc123", "limit": 5}), "GET", "/records/abc123/runs?limit=5", Value::Null, json!({"record_id": "abc123", "runs": [{"run_uuid": "r1", "status": "exited", "started_at_ms": 1, "ended_at_ms": 2, "exit_code": 0, "success": true, "tail_kind": "text", "tail": ["done"]}]})),
        ("start_record", json!({"record_id": "abc123", "wait_ms": 500}), "POST", "/records/abc123/start", json!({"wait_ms": 500}), json!({"record_id": "abc123", "kind": "agent", "term_id": 13, "run_uuid": "r2", "pending": false})),
        ("remove_record", json!({"record_id": "abc123", "confirm": true}), "POST", "/records/abc123/remove", json!({"confirm": true}), json!({"record_id": "abc123", "kind": "agent", "removed": true, "runs_deleted": 2})),
        // Resume: the provider continues the saved session (or a named one).
        ("resume_agent", json!({"record_id": "abc123"}), "POST", "/records/abc123/resume", json!({}), json!({"record_id": "abc123", "kind": "agent", "term_id": 14, "run_uuid": "r3", "session_id": "ses-1", "check": "quiet"})),
        ("resume_agent", json!({"record_id": "abc123", "session_id": "ses-0"}), "POST", "/records/abc123/resume", json!({"session_id": "ses-0"}), json!({"record_id": "abc123", "kind": "agent", "term_id": 14, "run_uuid": "r3", "session_id": "ses-1", "check": "quiet"})),
        // set_session with an explicit target (the self-targeting default is
        // exercised with an identity below).
        ("set_session", json!({"session_id": "ses-9", "process_id": 7}), "POST", "/processes/7/session", json!({"session_id": "ses-9"}), json!({"id": 7, "record_id": null, "run_uuid": null, "durable": false})),
    ];
    let tool_names: Vec<String> = tools().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    for (tool, args, method, path, body, expect) in cases {
        assert!(tool_names.contains(&tool.to_owned()), "{tool} is not in tools/list");
        let before = server.requests().len();
        let reply = call_tool(&client, tool, &args).unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert_eq!(reply, expect, "{tool} reply");
        let requests = server.requests();
        assert_eq!(requests.len(), before + 1, "{tool} must issue exactly one request");
        let req = &requests[before];
        assert_eq!(req.method, method, "{tool} method");
        assert_eq!(req.path, path, "{tool} path");
        assert_eq!(req.auth.as_deref(), Some("Bearer fake-token-123"), "{tool} token");
        assert_eq!(req.body, body, "{tool} body");
    }
    // Every listed tool was exercised.
    let exercised: std::collections::BTreeSet<&str> = [
        "list_processes", "get_process_status", "get_process_output", "get_input_journal",
        "list_projects", "get_project", "spawn_terminal", "send_input", "send_bytes",
        "start_process", "stop_process", "restart_process", "close_terminal",
        "list_agent_tools", "spawn_agent", "get_agent_events", "reap_orphans",
        "get_record_runs", "start_record", "remove_record", "resume_agent", "set_session",
    ].into_iter().collect();
    let listed: std::collections::BTreeSet<&str> = tool_names
        .iter()
        .map(String::as_str)
        .filter(|n| !is_coordination(n) && !is_timer(n))
        .collect();
    assert_eq!(listed, exercised);

    // set_session self-targets: without `process_id` the client's own actor
    // id (CHAPPA_AI_PROCESS_ID) names the row; a client with no identity is
    // told to pass one instead of guessing.
    let err = call_tool(&client, "set_session", &json!({"session_id": "ses-9"})).unwrap_err();
    assert!(err.to_string().contains("process_id"), "{err}");
    let identified = server.client().with_identity(Some("12"), None);
    let before = server.requests().len();
    let reply = call_tool(&identified, "set_session", &json!({"session_id": "ses-9"})).unwrap();
    assert_eq!(reply["id"], 12);
    assert_eq!(reply["durable"], true);
    let req = &server.requests()[before];
    assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/processes/12/session"));
    assert_eq!(req.body, json!({"session_id": "ses-9"}));
    assert!(call_tool(&identified, "set_session", &json!({"session_id": "  "})).is_err(), "an empty id is refused client-side");
}

/// Every scratchpad/todo tool → exactly one request at the
/// documented route with the documented body, the documented argument names on the
/// tool side. The client carries identity `{actor: 12, project: 6}`, so the
/// project-scoped calls fill `project_id` from it.
#[test]
fn every_coordination_tool_round_trips() {
    let server = FakeControlServer::start();
    let client = server.client().with_identity(Some("12"), Some(6));
    let cases: Vec<(&str, Value, &str, &str, Value)> = vec![
        ("scratchpad_write", json!({"name": "pad", "content": "# pad\n", "tags": ["eval"]}), "POST", "/scratchpads", json!({"name": "pad", "content": "# pad\n", "tags": ["eval"], "project_id": 6})),
        ("scratchpad_write", json!({"scratchpad_id": 5, "name": "pad", "content": "x", "expected_revision": 3}), "POST", "/scratchpads/5", json!({"name": "pad", "content": "x", "expected_revision": 3})),
        ("scratchpad_read", json!({"scratchpad_id": 5}), "GET", "/scratchpads/5", Value::Null),
        ("scratchpad_read", json!({"scratchpad_id": 5, "mode": "section", "section_heading": "Log", "offset": 2, "limit": 10}), "GET", "/scratchpads/5?mode=section&section_heading=Log&offset=2&limit=10", Value::Null),
        ("scratchpad_list", json!({}), "GET", "/scratchpads?project_id=6", Value::Null),
        ("scratchpad_list", json!({"query": "a b", "tags": ["x", "y"], "include_archived": true, "all_projects": true}), "GET", "/scratchpads?query=a%20b&tags=x%2Cy&include_archived=true", Value::Null),
        ("scratchpad_find", json!({"scratchpad_id": 5, "query": "pad", "case_sensitive": true, "limit": 5, "context_lines": 0, "scope": "headings"}), "GET", "/scratchpads/5/find?query=pad&case_sensitive=true&limit=5&context_lines=0&scope=headings", Value::Null),
        ("scratchpad_tail", json!({"scratchpad_id": 5, "lines": 1}), "GET", "/scratchpads/5/tail?lines=1", Value::Null),
        ("scratchpad_append", json!({"scratchpad_id": 5, "content": "more"}), "POST", "/scratchpads/5/append", json!({"content": "more"})),
        ("scratchpad_append_section", json!({"scratchpad_id": 5, "heading": "Log", "content": "more", "expected_revision": 3}), "POST", "/scratchpads/5/append_section", json!({"heading": "Log", "content": "more", "expected_revision": 3})),
        ("scratchpad_rename", json!({"scratchpad_id": 5, "name": "renamed", "expected_revision": 3}), "POST", "/scratchpads/5/rename", json!({"name": "renamed", "expected_revision": 3})),
        ("scratchpad_add_tags", json!({"scratchpad_id": 5, "tags": ["a"], "expected_revision": 3}), "POST", "/scratchpads/5/tags/add", json!({"tags": ["a"], "expected_revision": 3})),
        ("scratchpad_remove_tags", json!({"scratchpad_id": 5, "tags": ["a"], "expected_revision": 3, "response_mode": "rich"}), "POST", "/scratchpads/5/tags/remove", json!({"tags": ["a"], "expected_revision": 3, "response_mode": "rich"})),
        ("scratchpad_tags_list", json!({}), "GET", "/scratchpads/tags?project_id=6", Value::Null),
        ("scratchpad_clear", json!({"scratchpad_id": 5, "expected_revision": 3, "confirm": true}), "POST", "/scratchpads/5/clear", json!({"expected_revision": 3, "confirm": true})),
        ("scratchpad_delete", json!({"scratchpad_id": 5, "expected_revision": 3, "confirm": true}), "POST", "/scratchpads/5/delete", json!({"expected_revision": 3, "confirm": true})),
        ("scratchpad_archive", json!({"scratchpad_id": 5}), "POST", "/scratchpads/5/archive", json!({})),
        ("scratchpad_transfer", json!({"scratchpad_id": 5, "target_project_id": 9, "expected_revision": 3}), "POST", "/scratchpads/5/transfer", json!({"target_project_id": 9, "expected_revision": 3})),
        ("scratchpad_transfer", json!({"scratchpad_id": 5, "target_project_id": null}), "POST", "/scratchpads/5/transfer", json!({"target_project_id": null})),
        ("todo_create", json!({"title": "t", "priority": "high", "tags": ["x"]}), "POST", "/todos", json!({"title": "t", "priority": "high", "tags": ["x"], "project_id": 6})),
        ("todo_create", json!({"title": "t", "project_id": 2}), "POST", "/todos", json!({"title": "t", "project_id": 2})),
        ("todo_list", json!({"status": "open", "is_blocked": false, "sort": "priority_desc", "limit": 5}), "GET", "/todos?project_id=6&status=open&is_blocked=false&sort=priority_desc&limit=5", Value::Null),
        ("todo_get", json!({"todo_id": 7, "include_comments": true}), "GET", "/todos/7?include_comments=true", Value::Null),
        ("todo_add_tag", json!({"todo_id": 7, "tag": "x"}), "POST", "/todos/7/tags/add", json!({"tag": "x"})),
        ("todo_remove_tag", json!({"todo_id": 7, "tag": "x"}), "POST", "/todos/7/tags/remove", json!({"tag": "x"})),
        ("todo_set_blockers", json!({"todo_id": 7, "blocker_ids": [8]}), "POST", "/todos/7/blockers/set", json!({"blocker_ids": [8]})),
        ("todo_set_blockers", json!({"todo_id": 7}), "POST", "/todos/7/blockers/set", json!({"blocker_ids": []})),
        ("todo_add_blocker", json!({"todo_id": 7, "blocker_id": 8}), "POST", "/todos/7/blockers/add", json!({"blocker_id": 8})),
        ("todo_remove_blocker", json!({"todo_id": 7, "blocker_id": 8}), "POST", "/todos/7/blockers/remove", json!({"blocker_id": 8})),
        ("todo_complete", json!({"todo_id": 7, "completed": true, "release_lock": false}), "POST", "/todos/7/complete", json!({"completed": true, "release_lock": false})),
        ("todo_lock", json!({"todo_id": 7, "lease_ttl_seconds": 60}), "POST", "/todos/7/lock", json!({"lease_ttl_seconds": 60})),
        ("todo_unlock", json!({"todo_id": 7}), "POST", "/todos/7/unlock", json!({})),
        ("todo_comment_create", json!({"todo_id": 7, "body": "hi"}), "POST", "/todos/7/comments", json!({"body": "hi"})),
        ("todo_comment_update", json!({"comment_id": 3, "body": "edit"}), "POST", "/todo_comments/3", json!({"body": "edit"})),
        ("todo_comment_delete", json!({"comment_id": 3, "confirm": true}), "POST", "/todo_comments/3/delete", json!({"confirm": true})),
        ("todo_comment_list", json!({"todo_id": 7, "offset": 1}), "GET", "/todos/7/comments?offset=1", Value::Null),
        ("todo_transfer", json!({"todo_id": 7, "target_project_id": 9}), "POST", "/todos/7/transfer", json!({"target_project_id": 9})),
        ("todo_tags_list", json!({"project_id": 3}), "GET", "/todos/tags?project_id=3", Value::Null),
        ("todo_delete", json!({"todo_id": 7, "confirm": true}), "POST", "/todos/7/delete", json!({"confirm": true})),
    ];
    let tool_names: Vec<String> = tools().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    let mut exercised = std::collections::BTreeSet::new();
    for (tool, args, method, path, body) in cases {
        assert!(tool_names.contains(&tool.to_owned()), "{tool} is not in tools/list");
        exercised.insert(tool);
        let before = server.requests().len();
        let reply = call_tool(&client, tool, &args).unwrap_or_else(|e| panic!("{tool}: {e}"));
        let requests = server.requests();
        assert_eq!(requests.len(), before + 1, "{tool} must issue exactly one request");
        let req = &requests[before];
        assert_eq!(req.method, method, "{tool} method");
        assert_eq!(req.path, path, "{tool} path");
        assert_eq!(req.auth.as_deref(), Some("Bearer fake-token-123"), "{tool} token");
        assert_eq!(req.actor.as_deref(), Some("12"), "{tool} carries X-Chappa-Actor");
        assert_eq!(req.body, body, "{tool} body");
        let (_, expect) = canned(method, path);
        assert_eq!(reply, expect, "{tool} reply");
    }
    // Every coordination tool listed was exercised (todo_update is the
    // typed-error case below).
    exercised.insert("todo_update");
    exercised.insert("scratchpad_edit");
    let listed: std::collections::BTreeSet<&str> = tool_names
        .iter()
        .map(String::as_str)
        .filter(|n| is_coordination(n))
        .collect();
    assert_eq!(listed, exercised);
}

/// Every timer tool -> exactly one request at the documented route
/// with the documented body, the documented argument names on the tool side, and the
/// scheduling fields (already_idle / waiting_on / status / note) surfaced
/// VERBATIM. `delivery_process_id` is NOT injected client-side: the app
/// defaults it from the X-Chappa-Actor header this client already sends.
/// `project_id` IS (review fix): the identity's CHAPPA_AI_PROJECT_ID when
/// the call omits it, like every other coordination tool.
#[test]
fn every_timer_tool_round_trips() {
    let server = FakeControlServer::start();
    let client = server.client().with_identity(Some("12"), Some(6));
    let cases: Vec<(&str, Value, &str, &str, Value)> = vec![
        ("timer_set", json!({"delay_ms": 60000, "body": "check the build"}), "POST", "/timers", json!({"delay_ms": 60000, "body": "check the build", "project_id": 6})),
        ("timer_set", json!({"delay_ms": 2000, "body": "beat", "loop": true, "delivery_process_id": 5, "name": "heartbeat", "project_id": 2}), "POST", "/timers", json!({"delay_ms": 2000, "body": "beat", "loop": true, "delivery_process_id": 5, "name": "heartbeat", "project_id": 2})),
        ("timer_set", json!({"delay_ms": 1000, "body": "beat", "repeat_every_ms": 30000}), "POST", "/timers", json!({"delay_ms": 1000, "body": "beat", "repeat_every_ms": 30000, "project_id": 6})),
        ("timer_fire_when_idle_any", json!({"processes": [4, "worker b"], "max_wait_ms": 600000, "body": "B is quiet"}), "POST", "/timers/idle_any", json!({"processes": [4, "worker b"], "max_wait_ms": 600000, "body": "B is quiet", "project_id": 6})),
        ("timer_fire_when_idle_any", json!({"processes": [{"process_name": "worker b"}], "max_wait_ms": 1000, "body": "x", "idle_ms": 30000, "confirm_ms": 2000, "rearm": false}), "POST", "/timers/idle_any", json!({"processes": [{"process_name": "worker b"}], "max_wait_ms": 1000, "body": "x", "idle_ms": 30000, "confirm_ms": 2000, "rearm": false, "project_id": 6})),
        ("timer_fire_when_idle_all", json!({"processes": [4, 5], "max_wait_ms": 600000, "body": "all quiet"}), "POST", "/timers/idle_all", json!({"processes": [4, 5], "max_wait_ms": 600000, "body": "all quiet", "project_id": 6})),
        ("timer_list", json!({}), "GET", "/timers?project_id=6", Value::Null),
        ("timer_list", json!({"include_fired": true, "all": true, "limit": 10, "offset": 20, "project_id": 3}), "GET", "/timers?include_fired=true&all=true&limit=10&offset=20&project_id=3", Value::Null),
        ("timer_cancel", json!({"timer_id": 21}), "POST", "/timers/21/cancel", json!({})),
        ("timer_pause", json!({"timer_id": 21}), "POST", "/timers/21/pause", json!({})),
        ("timer_resume", json!({"timer_id": 21}), "POST", "/timers/21/resume", json!({})),
    ];
    let tool_names: Vec<String> = tools().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    let mut exercised = std::collections::BTreeSet::new();
    for (tool, args, method, path, body) in cases {
        assert!(tool_names.contains(&tool.to_owned()), "{tool} is not in tools/list");
        exercised.insert(tool);
        let before = server.requests().len();
        let reply = call_tool(&client, tool, &args).unwrap_or_else(|e| panic!("{tool}: {e}"));
        let requests = server.requests();
        assert_eq!(requests.len(), before + 1, "{tool} must issue exactly one request");
        let req = &requests[before];
        assert_eq!(req.method, method, "{tool} method");
        assert_eq!(req.path, path, "{tool} path");
        assert_eq!(req.auth.as_deref(), Some("Bearer fake-token-123"), "{tool} token");
        assert_eq!(req.actor.as_deref(), Some("12"), "{tool} carries X-Chappa-Actor (the delivery default)");
        assert_eq!(req.body, body, "{tool} body");
        let (_, expect) = canned(method, path);
        assert_eq!(reply, expect, "{tool} reply");
    }
    let listed: std::collections::BTreeSet<&str> = tool_names.iter().map(String::as_str).filter(|n| is_timer(n)).collect();
    assert_eq!(listed, exercised);

    // A call that does not name a target must NOT invent one client-side:
    // the app resolves it from the actor header (one source of truth). The
    // project scope IS filled in from the identity.
    let plain = server.requests().first().cloned().unwrap();
    assert_eq!(plain.body, json!({"delay_ms": 60000, "body": "check the build", "project_id": 6}));
    // Without an identity project there is nothing to fill in.
    let bare = server.client().with_identity(Some("12"), None);
    let before = server.requests().len();
    call_tool(&bare, "timer_set", &json!({"delay_ms": 1, "body": "x"})).unwrap();
    assert_eq!(server.requests()[before].body, json!({"delay_ms": 1, "body": "x"}));

    // The scheduling fields reach the model unchanged.
    let any = call_tool(&client, "timer_fire_when_idle_any", &json!({"processes": [4, 5], "max_wait_ms": 1, "body": "x"})).unwrap();
    assert_eq!(any["status"], "scheduled");
    assert_eq!(any["already_idle"], json!([4]));
    assert_eq!(any["waiting_on"], json!([5]));
    assert!(any["note"].as_str().unwrap().contains("NEW idle transition"));
    let all = call_tool(&client, "timer_fire_when_idle_all", &json!({"processes": [4, 5], "max_wait_ms": 1, "body": "x"})).unwrap();
    assert_eq!(all["status"], "already_satisfied");
    assert_eq!(all["timer_id"], Value::Null);
    // timer_list is the audit answer: an undelivered body says so.
    let listed = call_tool(&client, "timer_list", &json!({"include_fired": true})).unwrap();
    assert_eq!(listed["timers"][0]["delivery"]["delivered"], false);
    assert_eq!(listed["timers"][0]["delivery"]["status"], "failed");
    assert_eq!(listed["timers"][0]["delivery"]["error"], "process gone");
}

/// Client-side guards never reach the wire.
#[test]
fn timer_validation_never_hits_the_wire() {
    let server = FakeControlServer::start();
    let client = server.client().with_identity(Some("12"), Some(6));
    for (tool, args, needle) in [
        ("timer_set", json!({"body": "x"}), "`delay_ms`"),
        ("timer_set", json!({"delay_ms": 1}), "`body`"),
        ("timer_fire_when_idle_any", json!({"max_wait_ms": 1, "body": "x"}), "`processes`"),
        ("timer_fire_when_idle_any", json!({"processes": [], "max_wait_ms": 1, "body": "x"}), "empty"),
        ("timer_fire_when_idle_all", json!({"processes": [1], "body": "x"}), "`max_wait_ms`"),
        ("timer_cancel", json!({}), "`timer_id`"),
        ("timer_pause", json!({}), "`timer_id`"),
        ("timer_resume", json!({}), "`timer_id`"),
        ("timer_nope", json!({}), "unknown tool"),
    ] {
        let err = call_tool(&client, tool, &args).unwrap_err();
        assert!(err.to_string().contains(needle), "{tool}: {err}");
    }
    assert!(server.requests().is_empty(), "validation errors must not reach the app");
}

/// `revision_conflict` and `locked` reach the model as isError text
/// carrying the structured fields (current_revision / locked_by …), and the
/// mutating calls still went over the wire exactly once.
#[test]
fn coordination_errors_surface_their_structured_fields() {
    let server = FakeControlServer::start();
    let client = server.client().with_identity(Some("12"), Some(6));
    let edit = handle_message(
        &client,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "scratchpad_edit", "arguments": {"scratchpad_id": 5, "target": {"heading": "Log"}, "content": "x", "expected_revision": 2}}}),
    )
    .unwrap();
    assert_eq!(edit["result"]["isError"], true);
    let text = edit["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("revision_conflict"), "{text}");
    assert!(text.contains("\"current_revision\":3"), "{text}");
    assert!(text.contains("current_content"), "{text}");
    assert!(text.contains("409"), "{text}");
    assert_eq!(server.requests()[0].body, json!({"target": {"heading": "Log"}, "content": "x", "expected_revision": 2}));

    let update = handle_message(
        &client,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "todo_update", "arguments": {"todo_id": 7, "title": "mine"}}}),
    )
    .unwrap();
    assert_eq!(update["result"]["isError"], true);
    let text = update["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("locked"), "{text}");
    assert!(text.contains("\"locked_by\":\"2\""), "{text}");
    assert!(text.contains("lock_expires_at"), "{text}");
    assert_eq!(server.requests().len(), 2);
    assert_eq!(server.requests()[1].path, "/todos/7");
}

/// Identity: no CHAPPA_AI_PROCESS_ID → no actor header (the app
/// records "user"); no CHAPPA_AI_PROJECT_ID → unscoped list, and todo_create
/// refuses client-side without a project. Client-side confirm/guard checks
/// never reach the wire.
#[test]
fn coordination_identity_and_client_side_guards() {
    let server = FakeControlServer::start();
    let anonymous = server.client();
    call_tool(&anonymous, "scratchpad_list", &json!({})).unwrap();
    let req = &server.requests()[0];
    assert_eq!(req.actor, None, "no identity → no header");
    assert_eq!(req.path, "/scratchpads", "no project env → every pad");
    let err = call_tool(&anonymous, "todo_create", &json!({"title": "t"})).unwrap_err();
    assert!(matches!(err, ToolError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("project_id"));

    let client = server.client().with_identity(Some("12"), Some(6));
    let err = call_tool(&client, "scratchpad_clear", &json!({"scratchpad_id": 5, "expected_revision": 3})).unwrap_err();
    assert!(err.to_string().contains("confirm=true"), "{err}");
    let err = call_tool(&client, "scratchpad_delete", &json!({"scratchpad_id": 5, "expected_revision": 3, "confirm": false})).unwrap_err();
    assert!(err.to_string().contains("confirm=true"), "{err}");
    let err = call_tool(&client, "todo_delete", &json!({"todo_id": 7})).unwrap_err();
    assert!(err.to_string().contains("confirm=true"), "{err}");
    let err = call_tool(&client, "scratchpad_write", &json!({"scratchpad_id": 5, "name": "n", "content": "c"})).unwrap_err();
    assert!(err.to_string().contains("expected_revision"), "{err}");
    let err = call_tool(&client, "scratchpad_edit", &json!({"scratchpad_id": 5, "target": {"heading": "x"}, "content": "c"})).unwrap_err();
    assert!(err.to_string().contains("expected_revision"), "{err}");
    let err = call_tool(&client, "scratchpad_find", &json!({"scratchpad_id": 5, "query": "  "})).unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
    assert_eq!(server.requests().len(), 1, "guards never hit the wire");
}

/// Review fix: an EXPLICIT `project_id: null` on scratchpad_write creates a
/// GLOBAL pad even when CHAPPA_AI_PROJECT_ID is set — only an ABSENT field
/// falls back to the env scope. The list tools turn the same null into the
/// `global` marker the route understands.
#[test]
fn an_explicit_null_project_id_means_global_not_the_env_default() {
    let server = FakeControlServer::start();
    let client = server.client().with_identity(Some("12"), Some(6));
    call_tool(&client, "scratchpad_write", &json!({"name": "eval", "content": "x", "project_id": null})).unwrap();
    call_tool(&client, "scratchpad_write", &json!({"name": "eval", "content": "x"})).unwrap();
    call_tool(&client, "scratchpad_list", &json!({"project_id": null})).unwrap();
    call_tool(&client, "scratchpad_tags_list", &json!({"project_id": null})).unwrap();
    let reqs = server.requests();
    assert_eq!(reqs[0].path, "/scratchpads");
    assert_eq!(reqs[0].body, json!({"name": "eval", "content": "x", "project_id": null}), "explicit null travels as null");
    assert_eq!(reqs[1].body, json!({"name": "eval", "content": "x", "project_id": 6}), "absent → env scope");
    assert_eq!(reqs[2].path, "/scratchpads?project_id=global");
    assert_eq!(reqs[3].path, "/scratchpads/tags?project_id=global");
}

/// Submit atomicity, MCP side: text and submit travel in ONE request — never
/// "text" then a second request carrying Enter. (The app-side half — one
/// pty write of text+\r — is asserted in src-tauri/tests/control.rs.)
#[test]
fn submit_is_one_request_carrying_text_and_submit_together() {
    let server = FakeControlServer::start();
    let client = server.client();
    call_tool(&client, "send_input", &json!({"id": 1, "text": "echo hi", "submit": true})).unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "ONE write, not text-then-enter");
    assert_eq!(requests[0].body, json!({"text": "echo hi", "submit": true}));
    assert!(
        !requests[0].body["text"].as_str().unwrap().ends_with('\r'),
        "the app appends \\r to the same pty write; the client never pre-splits it"
    );
}

#[test]
fn unreachable_app_is_a_clean_error_naming_the_fix() {
    // Bind then drop: the port is closed, nothing listens.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let client = Client::new(format!("http://127.0.0.1:{port}"), TokenSource::Literal("x".into()));
    let started = std::time::Instant::now();
    let err = call_tool(&client, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Unreachable(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("chappa-ai-desktop is not running"), "{text}");
    assert!(text.contains("Start the chappa-ai desktop app"), "{text}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "must not hang");

    // Through JSON-RPC it is a tool error (isError), not a protocol error.
    let reply = handle_message(
        &client,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "list_processes", "arguments": {}}}),
    )
    .unwrap();
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["result"]["isError"], true);
    assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("not running"));
}

/// Cleanup: the token file is read ONCE and cached (an MCP session
/// makes many calls; the file does not change while the app runs), and the
/// cache is refreshed exactly when the app rejects it. A regenerated token
/// shows up as an identity proof the cached token cannot verify, so the stale
/// token is never even sent: the file is re-read and the call retried once.
#[test]
fn the_token_is_cached_and_re_read_only_after_a_rejection() {
    let server = FakeControlServer::start();
    let path = std::env::temp_dir().join(format!(
        "chappa-ai-mcp-token-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
    let client = Client::new(
        format!("http://{}", server.addr),
        TokenSource::File(path.clone()),
    );

    call_tool(&client, "list_processes", &json!({})).unwrap();
    assert_eq!(server.requests().len(), 1);

    // Cached: the call still works with the file GONE — no per-request read.
    std::fs::remove_file(&path).unwrap();
    call_tool(&client, "list_processes", &json!({})).unwrap();
    assert_eq!(server.requests().len(), 2);
    assert_eq!(server.identity_requests().len(), 2, "one identity check per connection");

    // The app regenerates its token: the cached one cannot verify the proof,
    // so the client re-reads the file once and retries — the tool call still
    // succeeds, and the old token never went out.
    std::fs::write(&path, "second-token").unwrap();
    server.accept_only("second-token");
    call_tool(&client, "list_processes", &json!({})).unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "only the verified attempt sends a request");
    assert_eq!(requests[2].auth.as_deref(), Some("Bearer second-token"));
    assert_eq!(server.identity_requests().len(), 4, "the failed check plus one retry");

    // And the refreshed token is what the NEXT call uses (one request).
    call_tool(&client, "list_processes", &json!({})).unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[3].auth.as_deref(), Some("Bearer second-token"));

    // A rejection the reload cannot fix is a plain error, not a retry loop.
    server.accept_only("third-token");
    let err = call_tool(&client, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");
    assert_eq!(server.requests().len(), 4, "nothing token-bearing was sent");
    assert_eq!(server.identity_requests().len(), 7, "one retry only");
    let _ = std::fs::remove_file(&path);
}

// ---- identity check -----------------------------------------------------------

/// A per-test directory holding an endpoint file.
fn endpoint_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chappa-ai-mcp-endpoint-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_endpoint(dir: &std::path::Path, addr: &str, nonce: &str) -> std::path::PathBuf {
    let path = dir.join(ENDPOINT_FILE);
    let ep = Endpoint { addr: addr.into(), nonce: nonce.into(), pid: Some(1) };
    std::fs::write(&path, ep.to_json()).unwrap();
    path
}

/// A program squatting on a port: records every byte it is sent and answers
/// each connection with `reply` (raw HTTP), never knowing the token.
struct Impostor {
    addr: String,
    received: Arc<Mutex<Vec<u8>>>,
}

impl Impostor {
    fn start(reply: impl Fn(&str) -> String + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let received: Arc<Mutex<Vec<u8>>> = Arc::default();
        let log = received.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut pending = Vec::new();
                // Answer the first request, then keep reading whatever else
                // the client sends on the connection until it hangs up.
                let first = read_request(&mut stream, &mut pending);
                let mut seen = format!("{first:?}").into_bytes();
                if let Some(req) = &first {
                    let _ = stream.write_all(reply(&req.path).as_bytes());
                }
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                seen.extend_from_slice(&pending);
                log.lock().unwrap().extend_from_slice(&seen);
            }
        });
        Self { addr, received }
    }

    fn saw(&self, needle: &str) -> bool {
        let got = self.received.lock().unwrap();
        find(&got, needle.as_bytes()).is_some()
    }
}

fn http_reply(status: u16, body: &Value) -> String {
    let payload = body.to_string();
    format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{payload}", payload.len())
}

#[test]
fn an_honest_server_named_by_the_endpoint_file_is_verified_and_used() {
    let server = FakeControlServer::start();
    let dir = endpoint_dir("honest");
    let path = write_endpoint(&dir, &server.addr, NONCE);
    let client = Client::from_endpoint_file(&path, TokenSource::Literal(TOKEN.into()));
    let out = call_tool(&client, "list_processes", &json!({})).unwrap();
    assert!(out.to_string().contains("shell"), "{out}");
    let ids = server.identity_requests();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].auth, None, "the identity check carries no credentials");
    assert_eq!(server.requests()[0].auth.as_deref(), Some(&*format!("Bearer {TOKEN}")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_impostor_on_the_port_never_receives_the_token() {
    // Three kinds of squatter: a plain server that wants auth (as an app too
    // old to answer the challenge would), one that answers with a made-up
    // proof, and one that echoes back a proof-shaped value.
    let impostors = [
        Impostor::start(|_| http_reply(401, &json!({"error": "unauthorized"}))),
        Impostor::start(|_| http_reply(200, &json!({"addr": "x", "nonce": NONCE, "proof": "00".repeat(32)}))),
        Impostor::start(|path| {
            let challenge = path.split("challenge=").nth(1).unwrap_or("").to_owned();
            http_reply(200, &json!({"addr": "x", "nonce": NONCE, "proof": challenge}))
        }),
    ];
    for impostor in &impostors {
        for client in [
            Client::new(format!("http://{}", impostor.addr), TokenSource::Literal(TOKEN.into())),
            {
                let dir = endpoint_dir("impostor");
                Client::from_endpoint_file(
                    write_endpoint(&dir, &impostor.addr, NONCE),
                    TokenSource::Literal(TOKEN.into()),
                )
            },
        ] {
            let err = call_tool(&client, "list_processes", &json!({})).unwrap_err();
            assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");
            assert!(err.to_string().contains("token was not sent"), "{err}");
        }
        // Give the impostor's reader a moment to drain the closed sockets.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(impostor.saw("/identity?challenge="), "the impostor was asked to prove itself");
        assert!(!impostor.saw(TOKEN), "the token leaked to an impostor");
        assert!(!impostor.saw("Authorization"), "credentials leaked to an impostor");
    }
}

/// A squatter that relays the challenge to a real instance listening
/// elsewhere gets a valid proof — but for the real instance's address, which
/// is not the address the client connected to.
#[test]
fn a_relayed_proof_for_another_address_is_refused() {
    let real = FakeControlServer::start();
    let real_addr = real.addr.clone();
    let impostor = Impostor::start(move |path| {
        let mut upstream = TcpStream::connect(&real_addr).unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        upstream.write_all(req.as_bytes()).unwrap();
        let _ = upstream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        // The fake keeps identity connections open; read until the body is in.
        while let Ok(n) = upstream.read(&mut buf) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if let Some(pos) = find(&raw, b"\r\n\r\n") {
                let body = &raw[pos + 4..];
                if serde_json::from_slice::<Value>(body).is_ok() {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    });
    let dir = endpoint_dir("relay");
    // The endpoint file names the impostor's port with the real run's nonce
    // (a stale file whose port was taken over).
    let path = write_endpoint(&dir, &impostor.addr, NONCE);
    let client = Client::from_endpoint_file(&path, TokenSource::Literal(TOKEN.into()));
    let err = call_tool(&client, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");
    assert!(err.to_string().contains("relayed"), "{err}");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!impostor.saw(TOKEN), "the token leaked through a relay");
    assert!(real.requests().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_or_missing_endpoint_file_is_refused_before_anything_is_sent() {
    let server = FakeControlServer::start();
    let dir = endpoint_dir("stale");

    // Missing: nothing to connect to, a clean "not running" naming the file.
    let missing = Client::from_endpoint_file(dir.join(ENDPOINT_FILE), TokenSource::Literal(TOKEN.into()));
    let err = call_tool(&missing, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Unreachable(_)), "{err:?}");
    assert!(err.to_string().contains(ENDPOINT_FILE), "{err}");

    // Stale: the right address, but a previous run's nonce.
    let path = write_endpoint(&dir, &server.addr, "an-earlier-run");
    let stale = Client::from_endpoint_file(&path, TokenSource::Literal(TOKEN.into()));
    let err = call_tool(&stale, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");
    assert!(err.to_string().contains("stale"), "{err}");

    // Garbage in the file.
    std::fs::write(&path, "not json").unwrap();
    let err = call_tool(&stale, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");

    assert!(server.requests().is_empty(), "no token-bearing request was sent");

    // The file is read per request: once it names the live run, the same
    // client works without being rebuilt.
    write_endpoint(&dir, &server.addr, NONCE);
    call_tool(&stale, "list_processes", &json!({})).unwrap();
    assert_eq!(server.requests().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_token_file_is_a_clean_error() {
    let client = Client::new(
        "http://127.0.0.1:1",
        TokenSource::File("C:/definitely/not/here/control_token".into()),
    );
    let err = call_tool(&client, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Token(_)), "{err:?}");
    assert!(err.to_string().contains("control_token"));
}

#[test]
fn http_errors_surface_the_apps_message() {
    let server = FakeControlServer::start();
    let client = server.client();
    let err = call_tool(&client, "get_process_status", &json!({"id": 404})).unwrap_err();
    assert_eq!(err, ToolError::Http { status: 404, message: "no such terminal: 404".into() });
    // A wrong token cannot verify the server's proof, so it is never sent.
    let before = server.requests().len();
    let bad = Client::new(format!("http://{}", server.addr), TokenSource::Literal("wrong".into()));
    let err = call_tool(&bad, "list_processes", &json!({})).unwrap_err();
    assert!(matches!(err, ToolError::Untrusted(_)), "{err:?}");
    assert_eq!(server.requests().len(), before);
}

#[test]
fn client_side_validation_never_hits_the_wire() {
    let server = FakeControlServer::start();
    let client = server.client();
    let err = call_tool(&client, "close_terminal", &json!({"id": 1, "confirm": false})).unwrap_err();
    assert!(matches!(err, ToolError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("confirm=true"));
    let err = call_tool(&client, "send_input", &json!({"id": 1})).unwrap_err();
    assert!(err.to_string().contains("`text`"));
    let err = call_tool(&client, "send_bytes", &json!({"id": 1, "bytes": [300]})).unwrap_err();
    assert!(err.to_string().contains("0..=255"));
    let err = call_tool(&client, "nope", &json!({})).unwrap_err();
    assert!(err.to_string().contains("unknown tool"));
    // A pre-joined extra_args string is a classic argument-handling mistake
    // — refused client-side, never re-split.
    let err = call_tool(&client, "spawn_agent", &json!({"agent_tool_id": 3, "extra_args": "--prompt two words"})).unwrap_err();
    assert!(err.to_string().contains("`extra_args`"), "{err}");
    let err = call_tool(&client, "spawn_agent", &json!({})).unwrap_err();
    assert!(err.to_string().contains("`agent_tool_id`"), "{err}");
    assert!(server.requests().is_empty(), "validation errors must not reach the app");
}

/// The tool schema (names, descriptions, input schemas, annotations) is
/// pinned: `UPDATE_SNAPSHOTS=1 cargo test -p chappa-ai-mcp` rewrites it.
#[test]
fn tool_schema_snapshot_including_annotations() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/tools.json");
    let rendered = serde_json::to_string_pretty(&tools()).unwrap() + "\n";
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
    assert_eq!(on_disk, rendered, "tools.json snapshot is stale — UPDATE_SNAPSHOTS=1 cargo test -p chappa-ai-mcp");

    // Annotation policy: reads are readOnlyHint (list_/get_ plus the
    // read tools), destructiveHint on close and on the confirm-gated
    // clear/delete tools, nothing else is destructive.
    const READS: [&str; 10] = [
        "scratchpad_read", "scratchpad_list", "scratchpad_find", "scratchpad_tail", "scratchpad_tags_list",
        "todo_list", "todo_get", "todo_comment_list", "todo_tags_list", "timer_list",
    ];
    const DESTRUCTIVE: [&str; 6] = ["close_terminal", "remove_record", "scratchpad_clear", "scratchpad_delete", "todo_delete", "todo_comment_delete"];
    for tool in tools() {
        let name = tool["name"].as_str().unwrap();
        let read_only = name.starts_with("list_") || name.starts_with("get_") || READS.contains(&name);
        assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
        assert_eq!(tool["annotations"]["destructiveHint"], DESTRUCTIVE.contains(&name), "{name}");
        assert!(tool["inputSchema"]["type"] == "object", "{name}");
        if DESTRUCTIVE.contains(&name) && name != "close_terminal" {
            assert!(tool["inputSchema"]["required"].as_array().unwrap().contains(&json!("confirm")), "{name} must require confirm");
        }
    }
}

#[test]
fn json_rpc_handshake_and_listing() {
    let server = FakeControlServer::start();
    let client = server.client();
    let init = handle_message(
        &client,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    )
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "chappa-ai");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    // Notifications get no reply.
    assert!(handle_message(&client, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    let list = handle_message(&client, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    assert_eq!(list["result"]["tools"], json!(tools()));
    let ping = handle_message(&client, &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"})).unwrap();
    assert_eq!(ping["result"], json!({}));
    let unknown = handle_message(&client, &json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"})).unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
    // A successful call renders the JSON reply as text content.
    let call = handle_message(
        &client,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "get_process_output", "arguments": {"id": 1}}}),
    )
    .unwrap();
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("echo hi"));
}
