//! `chappa-ai-mcp`: a stdio MCP server that translates tool calls
//! into requests against the chappa-ai desktop control surface
//! (`desktop/src-tauri/src/control_http.rs`, `desktop/CONTROL_API.md`).
//!
//! Stateless per request — every call opens one HTTP connection and closes
//! it, holding nothing across calls except the token (read once, re-read only
//! when the app answers 401 to it). A restarted app just starts answering
//! again; an unreachable app is a clean tool error naming the fix, never a
//! hang or a queue (connect timeout 2s).
//!
//! Wire: newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). Handled methods: `initialize`, `notifications/initialized`,
//! `ping`, `tools/list`, `tools/call`. Tool names are the plain verb-and-noun
//! an agent would guess (list_processes, get_process_output, send_input, …);
//! `readOnlyHint`/`destructiveHint` annotations are set so clients stop
//! nagging for approval on harmless calls.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub const SERVER_NAME: &str = "chappa-ai";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const DEFAULT_CONTROL_URL: &str = "http://127.0.0.1:8324";
/// Tauri's app identifier — the app-config dir is named after it.
pub const APP_IDENTIFIER: &str = "com.chappa-ai.desktop";
pub const TOKEN_FILE: &str = "control_token";
/// The header every request carries when this server runs inside a
/// chappa-ai-spawned process — the app attributes coordination writes to it.
pub const ACTOR_HEADER: &str = "X-Chappa-Actor";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Long enough for a start_process wait (max 15s server-side) plus slack.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

// ---- errors -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// Nothing listening / connection refused / timed out.
    Unreachable(String),
    /// The token file could not be read.
    Token(String),
    /// The app answered with an error status.
    Http { status: u16, message: String },
    /// Bad tool name or arguments (client-side).
    Invalid(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Unreachable(detail) => write!(
                f,
                "chappa-ai-desktop is not running (no control surface at the configured address: {detail}). \
                 Start the chappa-ai desktop app, then retry — this server is stateless and will pick it up immediately."
            ),
            ToolError::Token(detail) => write!(
                f,
                "cannot read the chappa-ai control token ({detail}). The app writes it at first launch to \
                 <app-config>/control_token; set CHAPPA_AI_CONTROL_TOKEN_FILE if the app-config dir is elsewhere."
            ),
            ToolError::Http { status, message } => write!(f, "chappa-ai answered {status}: {message}"),
            ToolError::Invalid(detail) => write!(f, "invalid tool call: {detail}"),
        }
    }
}

// ---- client -----------------------------------------------------------------

/// Where the app lives and where its token is. Built once from the
/// environment; holds no connection. Statelessness is about the SERVER side —
/// no session, no queue, nothing to reconnect — so the token (a file that
/// does not change while the app runs) is cached here and re-read only when
/// the app actually rejects it.
#[derive(Debug, Clone)]
pub struct Client {
    pub base_url: String,
    pub token: TokenSource,
    /// The last token read from `token`. Shared across clones so the reload
    /// after a 401 benefits every handle.
    cached: Arc<Mutex<Option<String>>>,
    /// Identity: this process's `CHAPPA_AI_PROCESS_ID`, sent as
    /// `X-Chappa-Actor` on every request (absent → the app records "user").
    pub actor: Option<String>,
    /// `CHAPPA_AI_PROJECT_ID`: the default project scope for the
    /// coordination tools when a call omits `project_id`.
    pub project_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum TokenSource {
    /// A literal token (tests, `CHAPPA_AI_CONTROL_TOKEN`).
    Literal(String),
    /// Read this file (once, then cached — see [`Client::token`]).
    File(PathBuf),
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: TokenSource) -> Self {
        Self {
            base_url: base_url.into(),
            token,
            cached: Arc::new(Mutex::new(None)),
            actor: None,
            project_id: None,
        }
    }

    /// Set the identity (tests; `from_env` reads it from the environment).
    pub fn with_identity(mut self, actor: Option<&str>, project_id: Option<u64>) -> Self {
        self.actor = actor.map(str::to_owned);
        self.project_id = project_id;
        self
    }

    /// `CHAPPA_AI_CONTROL_URL` (default `http://127.0.0.1:8324`);
    /// `CHAPPA_AI_CONTROL_TOKEN` > `CHAPPA_AI_CONTROL_TOKEN_FILE` > the
    /// platform app-config token path. Identity from
    /// `CHAPPA_AI_PROCESS_ID` / `CHAPPA_AI_PROJECT_ID`, read from THIS
    /// process's environment — the agent that spawned us inherits them from
    /// the app's spawn_agent, so the app can attribute our writes.
    pub fn from_env() -> Self {
        let actor = std::env::var("CHAPPA_AI_PROCESS_ID")
            .ok()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty());
        let project_id = std::env::var("CHAPPA_AI_PROJECT_ID")
            .ok()
            .and_then(|p| p.trim().parse().ok());
        let base_url =
            std::env::var("CHAPPA_AI_CONTROL_URL").unwrap_or_else(|_| DEFAULT_CONTROL_URL.to_owned());
        let token = if let Ok(t) = std::env::var("CHAPPA_AI_CONTROL_TOKEN") {
            TokenSource::Literal(t)
        } else if let Ok(p) = std::env::var("CHAPPA_AI_CONTROL_TOKEN_FILE") {
            TokenSource::File(PathBuf::from(p))
        } else {
            TokenSource::File(default_token_path())
        };
        Self::new(base_url, token).with_identity(actor.as_deref(), project_id)
    }

    /// The bearer token to send, plus whether it came from the cache.
    /// `reload` forces a fresh read of the file (see [`Client::request`]).
    fn token(&self, reload: bool) -> Result<(String, bool), ToolError> {
        if !reload {
            if let Some(cached) = self.cached.lock().ok().and_then(|c| c.clone()) {
                return Ok((cached, true));
            }
        }
        let fresh = match &self.token {
            TokenSource::Literal(t) => t.clone(),
            TokenSource::File(path) => std::fs::read_to_string(path)
                .map(|s| s.trim().to_owned())
                .map_err(|e| ToolError::Token(format!("{}: {e}", path.display())))?,
        };
        if let Ok(mut cache) = self.cached.lock() {
            *cache = Some(fresh.clone());
        }
        Ok((fresh, false))
    }

    /// One request, one connection. `body` is serialized as JSON for POSTs.
    ///
    /// A 401 answered to a CACHED token means the app regenerated its token
    /// file (fresh install, deleted file) since we read it, so the file is
    /// re-read once and the request retried — the one place staleness can
    /// bite. A 401 on a freshly-read token is a real error and is returned.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ToolError> {
        let (token, from_cache) = self.token(false)?;
        let (status, value) = self.send(method, path, body, &token)?;
        if status == 401 && from_cache {
            let (token, _) = self.token(true)?;
            let (status, value) = self.send(method, path, body, &token)?;
            return outcome(status, value);
        }
        outcome(status, value)
    }

    /// One HTTP/1.1 round trip on a fresh connection.
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        token: &str,
    ) -> Result<(u16, Value), ToolError> {
        let (host, port, prefix) = parse_http_url(&self.base_url)
            .ok_or_else(|| ToolError::Invalid(format!("bad control url: {}", self.base_url)))?;
        let addr = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| ToolError::Unreachable(e.to_string()))?
            .next()
            .ok_or_else(|| ToolError::Unreachable(format!("{host}:{port} did not resolve")))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| ToolError::Unreachable(format!("{addr}: {e}")))?;
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        // A path PREFIX in CHAPPA_AI_CONTROL_URL is honoured (e.g. a reverse
        // proxy in front of a container's app at `http://host:8324/chappa`);
        // route paths are appended to it verbatim.
        let target = format!("{prefix}{path}");
        let actor = self
            .actor
            .as_deref()
            .map(|a| format!("{ACTOR_HEADER}: {a}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\n{actor}\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| ToolError::Unreachable(e.to_string()))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| ToolError::Unreachable(format!("reading reply: {e}")))?;
        let (status, body) = parse_http_response(&raw)
            .ok_or_else(|| ToolError::Unreachable("malformed HTTP reply".to_owned()))?;
        // Parsed straight off the reply bytes — no intermediate String copies
        // of the whole response (a whole-screen `get_process_output` is the
        // big one). A non-JSON body is kept verbatim for the error message.
        let value = serde_json::from_slice(body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
        Ok((status, value))
    }
}

/// A control-surface reply → a tool outcome: 2xx is the payload, anything
/// else carries the app's own `error` message (or the raw body).
///
/// The coordination routes answer STRUCTURED errors
/// (`{error: "revision_conflict", current_revision, current_content}`,
/// `{error: "locked", locked_by, lock_expires_at}`). Those fields must reach
/// the model, so when the body carries more than the `error` word the whole
/// object is rendered into the message text.
fn outcome(status: u16, value: Value) -> Result<Value, ToolError> {
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let message = match &value {
        Value::Object(map) if map.len() > 1 && map.get("error").is_some_and(Value::is_string) => {
            let word = map["error"].as_str().unwrap_or("");
            let human = map.get("message").and_then(Value::as_str).unwrap_or("");
            format!("{word}: {human} {}", cap_text(&serde_json::to_string(&value).unwrap_or_default()))
        }
        _ => value["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| match &value {
                Value::String(text) => text.trim().to_owned(),
                other => other.to_string(),
            }),
    };
    Err(ToolError::Http { status, message })
}

/// The most error text a tool result carries. The app already caps a
/// `revision_conflict`'s `current_content` at 8 KB; this is the belt for
/// any other large structured body (and the braces around that one).
pub const ERROR_TEXT_CAP: usize = 9 * 1024;

/// Cap `text` at [`ERROR_TEXT_CAP`] bytes on a char boundary with a marker.
fn cap_text(text: &str) -> String {
    if text.len() <= ERROR_TEXT_CAP {
        return text.to_owned();
    }
    let mut cut = ERROR_TEXT_CAP;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [truncated {} bytes]", &text[..cut], text.len() - cut)
}

/// `%APPDATA%\com.chappa-ai.desktop\control_token` on Windows,
/// `~/Library/Application Support/<id>/control_token` on macOS,
/// `$XDG_CONFIG_HOME/<id>/control_token` (or `~/.config/…`) elsewhere —
/// Tauri v2's `app_config_dir` for this identifier.
pub fn default_token_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_default().join(APP_IDENTIFIER).join(TOKEN_FILE)
}

/// `http://host:port[/…]` → (host, port, path-prefix). Only plain http: the
/// control surface is localhost-only by contract.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().ok()?),
        None => (authority.to_owned(), 80),
    };
    Some((host, port, path.trim_end_matches('/').to_owned()))
}

/// `(status, body)` from a raw HTTP/1.1 reply, working on the BYTES: the
/// status comes off the status line inside the header block and the body is
/// borrowed from `raw` (so `serde_json::from_slice` parses it in place). The
/// old version made three whole-response String copies, which for a
/// whole-screen `get_process_output` is the largest allocation in the crate.
fn parse_http_response(raw: &[u8]) -> Option<(u16, &[u8])> {
    let split = find(raw, b"\r\n\r\n")?;
    let head = &raw[..split];
    let line_end = find(head, b"\r\n").unwrap_or(head.len());
    let status_line = std::str::from_utf8(&head[..line_end]).ok()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, &raw[split + 4..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Percent-encode a path segment (process names carry spaces and `+`).
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- tools ------------------------------------------------------------------

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn annotations(read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": false,
    })
}

/// The tool table (`tools/list`). Snapshot-tested in tests/mcp.rs.
pub fn tools() -> Vec<Value> {
    let id = json!({"type": "integer", "minimum": 0, "description": "Terminal/process id from list_processes or spawn_terminal."});
    let project_id = json!({"type": "integer", "minimum": 0, "description": "Project id from list_projects."});
    let name = json!({"type": "string", "description": "project process name (as shown by get_project)."});
    let wait = |what: &str, default: u64, max: u64| {
        json!({"type": "integer", "minimum": 0, "maximum": max,
               "description": format!("{what} (default {default}, max {max}).")})
    };
    vec![
        json!({
            "name": "list_processes",
            "description": "Every live terminal/process in chappa-ai: id, name, status (starting|running|stopped|exited|failed), exit_code, cols, rows, seq, plus byte-stream liveness: output_bytes, last_output_at (unix ms), has_output (false = still booting, distinct from dead), child_alive. `agent.parent_process_id` = the process that spawned it, null for a root; `agent.close_on_exit` is the /processes row's close_on_exit for agents.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "get_process_status",
            "description": "One process row (same fields as list_processes). `agent.parent_process_id` = the process that spawned it, null for a root. Use has_output/child_alive/last_output_at for liveness — never infer it from screen text.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "get_process_output",
            "description": "Terminal text as rows. Default = the WHOLE current screen. lines=N also includes the last N scrollback lines above it; offset=O (with lines) returns a history-only window that skips the O most recent scrollback lines. No silent cap.",
            "inputSchema": schema(json!({
                "id": id,
                "lines": {"type": "integer", "minimum": 0, "description": "Scrollback lines to include above the screen (default 0)."},
                "offset": {"type": "integer", "minimum": 0, "description": "Skip this many most-recent scrollback lines (history-only window when > 0)."},
            }), &["id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "get_input_journal",
            "description": "The last 100 send_input/send_bytes deliveries to a process: [{ts, bytes_len, text, submit}], oldest first — the audit trail for 'did my send land'.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "list_projects",
            "description": "Stored projects: [{id, name, path, icon, open, processes}]. processes (status, term_id, exit_code, …) are populated only for open projects.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "get_project",
            "description": "One project with its process rows.",
            "inputSchema": schema(json!({"id": project_id}), &["id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "list_agent_tools",
            "description": "Registered agent tools: [{id, name, command, tool_type, enabled, model, runtime {kind: host | docker_exec, container?, user?, workdir?, tty?, max_busy_in_container?}, container, program, args, env, max_busy}]. `command` is a derived display string; program/args are the truth, and spawn_agent takes the id.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "spawn_agent",
            "description": "Spawn an agent from a registered tool. extra_args are appended to the tool's args VERBATIM — an element containing spaces reaches the child as ONE argument, so pass [\"--prompt\", \"two words here\"], never a pre-joined string. The child gets CHAPPA_AI_PROCESS_ID / CHAPPA_AI_PROJECT_ID / CHAPPA_AI_AGENT_TOOL_ID in its env and the reply carries agent_instructions to prepend to your first message. `prompt` is queued until the TUI is ready (first output, then agent_ready_quiet_ms of silence — or agent_ready_max_wait_ms after the first byte for a TUI that never goes quiet, reason=ready-by-timeout) and delivered as one atomic write with Enter; prompt_receipt tells you whether it landed (delivered=false, reason=exited when the child died first). Busy guards refuse when the tool's max_busy or the container's max_busy_in_container is reached, naming the busy processes; force=true overrides. Docker-exec replies include container {status, uptime_s, mem_bytes, mem_limit_bytes} so you can decline the next spawn with evidence. The reply's `agent.parent_process_id` is the process that spawned you (null for a root) — the rail nests this row under it. close_on_exit=true makes the row close itself when the child exits; a bash-hosted tool (chappa-a/b) never exits on its own, so end its command line with `; exit` (or spawn with extra_args [\"-lc\", \"<command>\"]) and the row reaps when the run returns.",
            "inputSchema": schema(json!({
                "agent_tool_id": {"type": "integer", "minimum": 1, "description": "Tool id from list_agent_tools."},
                "project_id": {"type": "integer", "minimum": 0, "description": "Project id from list_projects (cwd = its root; identity env carries it)."},
                "name": {"type": "string", "description": "Display name for the process row (default: the tool name)."},
                "extra_args": {"type": "array", "items": {"type": "string"}, "description": "argv appended to the tool's args, passed unsplit."},
                "prompt": {"type": "string", "description": "First input, written once the TUI is ready (one atomic write + Enter)."},
                "force": {"type": "boolean", "description": "Override the busy guards (logged)."},
                "parent_process_id": {"type": ["integer", "null"], "minimum": 0, "description": "Explicit parent for rail nesting — overrides the X-Chappa-Actor default. Validated against a live process; null/absent = nest under the caller (or a root)."},
                "close_on_exit": {"type": "boolean", "description": "Close this row automatically when the child exits (default false). Note: a bash-hosted tool never exits on its own — end the command with `; exit` or spawn with extra_args [\"-lc\", \"<command>\"] so the row reaps when the run returns."},
                "cols": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Columns (default 80)."},
                "rows": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Rows (default 24)."},
            }), &["agent_tool_id"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "get_agent_events",
            "description": "A json-transport agent's typed events since a cursor — {id, since, events: [{id, seq, ts, kind, payload}]}, kind = turn_started | turn_ended | tool_call | text | usage | awaiting_input | compaction | error | raw (raw = the unclassified native object, nothing is lost). Ring of the last 1000 per agent; poll with since = the last seq you saw. awaiting_input means the agent is idle and wants a message (send_input). Fails for tty agents and plain terminals — read those with get_process_output.",
            "inputSchema": schema(json!({
                "id": id,
                "since": {"type": "integer", "minimum": 0, "description": "Return events with seq > since (default 0 = everything still in the ring)."},
            }), &["id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "spawn_terminal",
            "description": "Spawn a terminal in chappa-ai. command + args are handed to the OS verbatim (an argument with spaces stays one argument); omit command for the default shell. Returns {id, name}. Read it back with get_process_output.",
            "inputSchema": schema(json!({
                "command": {"type": "string", "description": "Program to run (default: the platform shell)."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "argv, passed unsplit."},
                "cwd": {"type": "string", "description": "Working directory."},
                "name": {"type": "string", "description": "Display name (default: the command)."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables applied over the inherited ones, e.g. {\"CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN\": \"1\"}."},
                "cols": {"type": "integer", "minimum": 1, "maximum": 4096, "description": "Columns (default 80)."},
                "rows": {"type": "integer", "minimum": 1, "description": "Rows (default 24)."},
            }), &[]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "send_input",
            "description": "Type text into a process. submit=true appends Enter (\\r) in the SAME write as the text. Returns a receipt {written, seq_before, seq_after, had_output_within_ms, wait_ms} off the pty byte counter, so you can see whether the write produced activity without diffing screens. On a json-transport agent the text is ONE user message (submit implied) and the receipt is {written, transport: \"json\", delivered, reason?, waited_ms, seq_before, turn_started_seq?, wait_ms}: delivered=true when the CLI's next turn_started arrived within wait_ms (default 5000, max 20000); reason=timeout is written-but-unacknowledged, exited = no child (or it died before its turn started: `exited: <why>`), busy = the previous opencode turn is still running; wait_ms is the effective wait. A json agent takes whole messages: submit=false or empty text is refused (400), and a refused send is not journaled. Read the reply with get_agent_events(since = seq_before).",
            "inputSchema": schema(json!({
                "id": id,
                "text": {"type": "string", "description": "Text to write (no key encoding)."},
                "submit": {"type": "boolean", "description": "Append Enter in the same write (default false)."},
                "wait_ms": wait("How long to watch the byte counter after the write", 250, 5000),
            }), &["id", "text"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "send_bytes",
            "description": "Write raw bytes to a process (control sequences, e.g. [3] for Ctrl-C, [13] for Enter). Same receipt and input-journal treatment as send_input.",
            "inputSchema": schema(json!({
                "id": id,
                "bytes": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 255}, "description": "Byte values."},
                "wait_ms": wait("How long to watch the byte counter after the write", 250, 5000),
            }), &["id", "bytes"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "start_process",
            "description": "Start a project process of an open project. Idempotent: a live process answers with its term_id. The app's frontend performs the spawn; the call waits up to wait_ms for it to register and answers pending=true if it hasn't yet (then poll get_project).",
            "inputSchema": schema(json!({
                "project_id": project_id,
                "name": name,
                "wait_ms": wait("How long to wait for the spawn to register", 3000, 15000),
            }), &["project_id", "name"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "stop_process",
            "description": "Stop a project process. Stopping a non-running process is a no-op success, never an error.",
            "inputSchema": schema(json!({"project_id": project_id, "name": name}), &["project_id", "name"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "restart_process",
            "description": "Restart a project process: stop (no-op if not running) then start. A stopped process simply starts.",
            "inputSchema": schema(json!({
                "project_id": project_id,
                "name": name,
                "wait_ms": wait("How long to wait for the spawn to register", 3000, 15000),
            }), &["project_id", "name"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "close_terminal",
            "description": "Close a terminal and kill its child. Requires confirm=true — there is no undo.",
            "inputSchema": schema(json!({
                "id": id,
                "confirm": {"type": "boolean", "description": "Must be true."},
            }), &["id", "confirm"]),
            "annotations": annotations(false, true, true),
        }),
        json!({
            "name": "reap_orphans",
            "description": "Sweep every ENABLED docker_exec tool's distinct (container, user) pair for container processes carrying CHAPPA_AI_SPAWN_UUID that no LIVE chappa-ai row owns — orphans left by a hard app restart or a docker hiccup mid-close — and kill each orphan root through the normal close sequence (TERM, poll, KILL, verify), as the tool's user. Returns [{container, user, pid, uuid, verdict}], verdict = gone | still-present | container-down | unresolved. A down or unreachable container is skipped silently; processes without the marker and the app's live rows are never touched; `docker top` is never consulted.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(false, false, true),
        }),
    ]
    .into_iter()
    .chain(coordination_tools())
    .chain(timer_tools())
    .collect()
}

/// Timers. Everything but
/// `timer_list` schedules or changes a wake-up, so only the listing is
/// readOnly; nothing here is destructive (a cancelled timer loses no data).
fn timer_tools() -> Vec<Value> {
    let tid = json!({"type": "integer", "minimum": 1, "description": "Timer id from timer_set / timer_list."});
    let target = json!({"type": "integer", "minimum": 0, "description": "Process that receives the body. Defaults to YOUR process (CHAPPA_AI_PROCESS_ID); a timer delivers to exactly one process."});
    let body = json!({"type": "string", "description": "Injected VERBATIM as a fresh user turn when the timer fires — write it as instructions to the receiving agent."});
    let processes = json!({
        "type": "array",
        "description": "Processes to watch. Each entry is a process id, a process name, or {process_id} / {process_name}.",
        "items": {"type": ["integer", "string", "object"]},
    });
    let idle_ms = json!({"type": "integer", "minimum": 0, "description": "Byte-stream silence that counts as idle, overriding the idle_threshold_ms setting (default 120000)."});
    let confirm_ms = json!({"type": "integer", "minimum": 0, "description": "Fire-time re-validation window, overriding timer_confirm_ms (default 5000). A byte in this window re-arms the timer instead of firing it."});
    let rearm = json!({"type": "boolean", "description": "Keep waiting after a flicker (default true). false = one chance at the idle condition; after that only the deadline can fire it."});
    let name = json!({"type": "string", "description": "Label for timer_list."});
    let project = json!({"type": "integer", "minimum": 0, "description": "Project scope override (default: this process's CHAPPA_AI_PROJECT_ID)."});
    vec![
        json!({
            "name": "timer_set",
            "description": "Wake a process after delay_ms with `body` injected as a fresh user turn (delivered only when the target is genuinely quiet, up to timer_delivery_timeout_ms — never mid-turn). loop=true repeats every delay_ms; repeat_every_ms overrides the interval. Delivery defaults to YOUR process; the target must be live now and is pinned by its uuid, so a delay timer whose target is gone at fire time expires with error=process gone (repeating ones too). Timers persist across app restarts: one whose fire time passed while chappa-ai was down records ONE reason=missed firing, delivered only if the same spawn is still live within timer_missed_grace_ms — after a relaunch that is normally {delivered: false, error: process gone}, visible in timer_list.",
            "inputSchema": schema(json!({
                "delay_ms": {"type": "integer", "minimum": 0, "description": "Delay in milliseconds."},
                "body": body,
                "delivery_process_id": target,
                "loop": {"type": "boolean", "description": "Repeat using delay_ms as the interval (default false)."},
                "repeat_every_ms": {"type": "integer", "minimum": 0, "description": "Repeat interval in ms; omitted means one-shot (or delay_ms when loop=true)."},
                "name": name,
                "project_id": project,
            }), &["delay_ms", "body"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "timer_fire_when_idle_any",
            "description": "Wake a process when ANY watched process goes idle, or at max_wait_ms (reason=deadline). Idle is derived from the pty BYTE STREAM — child_alive and now - last_output_at >= idle_ms (default 120000) — never from render diffs, so false-idle flickers from a repaint cannot happen. Processes already idle when you schedule are IGNORED until they become busy again (`any` waits for a NEW transition) and reported as already_idle. A met condition enters `confirming` and is re-checked after confirm_ms before the body is delivered. Names resolve within project_id (default: your CHAPPA_AI_PROJECT_ID) first; a name found in several projects with no scope is an error naming the candidates. Answers {timer_id, status: scheduled, already_idle, waiting_on, deadline_at, note}.",
            "inputSchema": schema(json!({
                "processes": processes,
                "max_wait_ms": {"type": "integer", "minimum": 0, "description": "Hard deadline in milliseconds — fires the body with reason=deadline."},
                "body": body,
                "delivery_process_id": target,
                "idle_ms": idle_ms,
                "confirm_ms": confirm_ms,
                "rearm": rearm,
                "name": name,
                "project_id": project,
            }), &["processes", "max_wait_ms", "body"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "timer_fire_when_idle_all",
            "description": "Wake a process when ALL watched processes are idle, or at max_wait_ms. Already-idle processes COUNT AS SATISFIED here: if every one of them is idle the answer is {status: \"already_satisfied\", timer_id: null} and no timer is created. Same byte-stream idle rule and confirm/re-arm behaviour as timer_fire_when_idle_any.",
            "inputSchema": schema(json!({
                "processes": processes,
                "max_wait_ms": {"type": "integer", "minimum": 0, "description": "Hard deadline in milliseconds — fires the body with reason=deadline."},
                "body": body,
                "delivery_process_id": target,
                "idle_ms": idle_ms,
                "confirm_ms": confirm_ms,
                "rearm": rearm,
                "name": name,
                "project_id": project,
            }), &["processes", "max_wait_ms", "body"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "timer_list",
            "description": "Your timers, pending first then newest first: [{id, name, kind, status: pending | confirming | paused | fired | cancelled | expired, delivery: {firing_id, process_id, status: in_flight | delivered | failed | coalesced, delivered: true | false | null, receipt, reason: condition | deadline | missed | expired, error, coalesced_with, at}, fired_count, next_fire_at, deadline_at, waiting_on}] plus total/limit/offset. Pending only by default; include_fired=true also returns fired/cancelled/expired timers within the retention window (24 h) — that is how you check AFTER THE FACT whether a wake-up was actually delivered. all=true lists every actor's timers, not just yours. Scoped to project_id (default: your CHAPPA_AI_PROJECT_ID).",
            "inputSchema": schema(json!({
                "include_fired": {"type": "boolean", "description": "Also return fired/cancelled/expired timers within the retention window."},
                "all": {"type": "boolean", "description": "List every actor's timers (orchestrator seat), not only your own."},
                "limit": {"type": "integer", "minimum": 0, "description": "Maximum rows (default 50, max 200)."},
                "offset": {"type": "integer", "minimum": 0, "description": "Rows to skip (paging; total is the unpaged count)."},
                "project_id": project,
            }), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "timer_cancel",
            "description": "Cancel one of your pending timers. A cancelled timer never fires, not even from `confirming` and not at its deadline.",
            "inputSchema": schema(json!({"timer_id": tid}), &["timer_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "timer_pause",
            "description": "Pause one of your timers. This STOPS ITS CLOCK: the remaining delay (or remaining deadline) is captured, so it cannot come due while paused.",
            "inputSchema": schema(json!({"timer_id": tid}), &["timer_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "timer_resume",
            "description": "Resume a paused timer from the delay it had left when you paused it.",
            "inputSchema": schema(json!({"timer_id": tid}), &["timer_id"]),
            "annotations": annotations(false, false, true),
        }),
    ]
}

/// Scratchpads + todos. Reads
/// carry readOnlyHint; clear/delete carry destructiveHint and require
/// confirm=true client-side.
fn coordination_tools() -> Vec<Value> {
    let sid = json!({"type": "integer", "minimum": 1, "description": "Scratchpad id from scratchpad_list / scratchpad_write."});
    let tid = json!({"type": "integer", "minimum": 1, "description": "Todo id from todo_list / todo_create."});
    let cid = json!({"type": "integer", "minimum": 1, "description": "Comment id from todo_comment_list / todo_comment_create."});
    let project = json!({"type": "integer", "minimum": 0, "description": "Project scope override (default: this process's CHAPPA_AI_PROJECT_ID)."});
    let pad_project = json!({"type": ["integer", "null"], "minimum": 0, "description": "Project scope: an id, or an explicit null = the global (cross-project) pads. Omitted = this process's CHAPPA_AI_PROJECT_ID (every pad when that is unset too)."});
    let rev_req = json!({"type": "integer", "minimum": 1, "description": "REQUIRED revision guard: the revision you last read. A mismatch is a revision_conflict error carrying current_revision (and, on write/edit/clear, current_content capped at 8 KB with current_content_truncated) — re-read and retry, never overwrite blind."});
    let rev_write = json!({"type": "integer", "minimum": 1, "description": "REQUIRED when scratchpad_id is given (overwrite): the revision you last read; a mismatch is a revision_conflict carrying current_revision + current_content (capped at 8 KB). Ignored on create (no scratchpad_id)."});
    let rev_opt = json!({"type": "integer", "minimum": 1, "description": "Optional revision guard (appends cannot clobber, so last-writer-wins is acceptable; the revision still advances)."});
    let tags = json!({"type": "array", "items": {"type": "string"}, "description": "Tag labels."});
    let mode = json!({"type": "string", "enum": ["slim", "rich"], "description": "slim (default) = {id, project_id, revision} + the changed fields; rich = the full row."});
    let offset = json!({"type": "integer", "minimum": 0, "description": "Zero-based offset."});
    let limit = json!({"type": "integer", "minimum": 0, "description": "Maximum rows (default 50, max 200)."});
    vec![
        json!({
            "name": "scratchpad_write",
            "description": "Create a scratchpad (no scratchpad_id) or replace one's full content at expected_revision (REQUIRED when scratchpad_id is given). A leading `# Title` line overrides name on create and full overwrite ONLY (edit never renames). Prefer scratchpad_append / scratchpad_append_section / scratchpad_edit for targeted changes. project_id: an explicit null = a global (cross-project) pad; omitted = CHAPPA_AI_PROJECT_ID (global when that is unset too).",
            "inputSchema": schema(json!({
                "scratchpad_id": {"type": "integer", "minimum": 1, "description": "Omit to create; set to overwrite (then expected_revision is required)."},
                "name": {"type": "string"},
                "content": {"type": "string"},
                "expected_revision": rev_write,
                "tags": tags,
                "project_id": pad_project,
                "response_mode": mode,
            }), &["name", "content"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_read",
            "description": "Read a scratchpad with its revision (read before every guarded write). mode = full (default; offset/limit window the lines) | outline (heading list only) | section (one markdown section; section_heading required — exact heading text after trimming, first duplicate wins). The content/headings spellings are accepted.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "mode": {"type": "string", "enum": ["full", "outline", "section", "content", "headings", "lines"]},
                "section_heading": {"type": "string", "description": "Required when mode=section."},
                "offset": {"type": "integer", "minimum": 0, "description": "Line offset (0-based) for mode=full."},
                "limit": {"type": "integer", "minimum": 0, "description": "Maximum lines for mode=full."},
            }), &["scratchpad_id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "scratchpad_list",
            "description": "List scratchpads without content: name, revision, tags, updated_at, updated_by, line_count. query matches name + content (rows carry matched_fields and a snippet); tags = any-of; archived pads hidden unless include_archived. Scoped to project_id (default: CHAPPA_AI_PROJECT_ID); all_projects=true lists every pad.",
            "inputSchema": schema(json!({
                "query": {"type": "string"},
                "tags": tags,
                "project_id": pad_project,
                "all_projects": {"type": "boolean", "description": "Ignore the project scope and list every pad (including global ones)."},
                "include_archived": {"type": "boolean"},
                "offset": offset,
                "limit": limit,
            }), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "scratchpad_find",
            "description": "Literal substring search inside one scratchpad: matches with 1-based line numbers and context, without returning the whole document.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "query": {"type": "string", "description": "Literal substring (empty is rejected)."},
                "case_sensitive": {"type": "boolean", "description": "Default false."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Max matching lines (default 20)."},
                "context_lines": {"type": "integer", "minimum": 0, "maximum": 3, "description": "Lines before/after each match (default 1)."},
                "scope": {"type": "string", "enum": ["all", "headings", "content"]},
            }), &["scratchpad_id", "query"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "scratchpad_tail",
            "description": "The last N lines of a scratchpad (default 10) with revision metadata.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "lines": {"type": "integer", "minimum": 0, "description": "Trailing lines (default 10)."},
            }), &["scratchpad_id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "scratchpad_append",
            "description": "Append content at the end of a scratchpad (a newline separates it). expected_revision optional. Use scratchpad_append_section to append under a heading.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "content": {"type": "string"},
                "expected_revision": rev_opt,
                "response_mode": mode,
            }), &["scratchpad_id", "content"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_append_section",
            "description": "Append at the end of the section under an EXISTING markdown heading (before the next heading of the same or higher level). Heading match is exact text after trimming (`## Log` or `Log`); a missing heading is an error that changes nothing. expected_revision optional.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "heading": {"type": "string"},
                "content": {"type": "string"},
                "expected_revision": rev_opt,
                "response_mode": mode,
            }), &["scratchpad_id", "heading", "content"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_edit",
            "description": "Replace one markdown section or a line range at expected_revision (REQUIRED). target = {heading} (content starting with a heading replaces the whole section; otherwise the heading is kept and its body replaced) or {start_line, end_line} (1-based inclusive, bounds-checked). {type: section, section_heading} / {type: line_range, offset, limit} are accepted too. Never renames the pad: the leading-H1 title override applies on scratchpad_write (create / full overwrite) only, so a scratchpad_rename survives later edits.",
            "inputSchema": schema(json!({
                "scratchpad_id": sid,
                "target": {"type": "object", "description": "{heading} | {start_line, end_line} | {type: \"section\", section_heading} | {type: \"line_range\", offset, limit}", "properties": {
                    "heading": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "type": {"type": "string", "enum": ["section", "line_range"]},
                    "section_heading": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1},
                }},
                "content": {"type": "string"},
                "expected_revision": rev_req,
                "response_mode": mode,
            }), &["scratchpad_id", "target", "content", "expected_revision"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_rename",
            "description": "Rename a scratchpad at expected_revision (REQUIRED) without rewriting its content.",
            "inputSchema": schema(json!({"scratchpad_id": sid, "name": {"type": "string"}, "expected_revision": rev_req, "response_mode": mode}), &["scratchpad_id", "name", "expected_revision"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_add_tags",
            "description": "Add tags in one revision bump at expected_revision (REQUIRED).",
            "inputSchema": schema(json!({"scratchpad_id": sid, "tags": tags, "expected_revision": rev_req, "response_mode": mode}), &["scratchpad_id", "tags", "expected_revision"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_remove_tags",
            "description": "Remove tags in one revision bump at expected_revision (REQUIRED).",
            "inputSchema": schema(json!({"scratchpad_id": sid, "tags": tags, "expected_revision": rev_req, "response_mode": mode}), &["scratchpad_id", "tags", "expected_revision"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "scratchpad_tags_list",
            "description": "Distinct scratchpad tags in the project scope.",
            "inputSchema": schema(json!({"project_id": pad_project, "all_projects": {"type": "boolean"}}), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "scratchpad_clear",
            "description": "Empty a scratchpad's content at expected_revision (REQUIRED). Requires confirm=true — the content is gone.",
            "inputSchema": schema(json!({"scratchpad_id": sid, "expected_revision": rev_req, "confirm": {"type": "boolean", "description": "Must be true."}, "response_mode": mode}), &["scratchpad_id", "expected_revision", "confirm"]),
            "annotations": annotations(false, true, true),
        }),
        json!({
            "name": "scratchpad_delete",
            "description": "Delete a scratchpad at expected_revision (REQUIRED). Requires confirm=true — no undo.",
            "inputSchema": schema(json!({"scratchpad_id": sid, "expected_revision": rev_req, "confirm": {"type": "boolean", "description": "Must be true."}}), &["scratchpad_id", "expected_revision", "confirm"]),
            "annotations": annotations(false, true, true),
        }),
        json!({
            "name": "scratchpad_archive",
            "description": "Hide a scratchpad from lists without deleting it (archived=false un-hides).",
            "inputSchema": schema(json!({"scratchpad_id": sid, "archived": {"type": "boolean", "description": "Default true."}, "response_mode": mode}), &["scratchpad_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "scratchpad_transfer",
            "description": "Move a scratchpad to another project (target_project_id null = global). Only the project changes. expected_revision optional.",
            "inputSchema": schema(json!({"scratchpad_id": sid, "target_project_id": {"type": ["integer", "null"], "minimum": 0}, "expected_revision": rev_opt, "response_mode": mode}), &["scratchpad_id", "target_project_id"]),
            "annotations": annotations(false, false, false),
        }),
        // ---- todos ----
        json!({
            "name": "todo_create",
            "description": "Create a project-scoped todo (project_id defaults to CHAPPA_AI_PROJECT_ID). priority = low | normal | high | urgent (`medium` = normal). Slim receipt {todo_id, project_id, revision, title}.",
            "inputSchema": schema(json!({
                "title": {"type": "string"},
                "body": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent", "medium"]},
                "tags": tags,
                "project_id": project,
                "response_mode": mode,
            }), &["title"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "todo_list",
            "description": "Todo summaries (no body) with filters, sort and pagination. status = open | in_progress | done; is_blocked is derived from blockers that are not completed; query searches title, body and comments; sort = updated_desc (default) | updated_asc | created_desc | created_asc | priority_desc | priority_asc | title. Use todo_get for the body and comments.",
            "inputSchema": schema(json!({
                "status": {"type": "string", "enum": ["open", "in_progress", "done", "completed"]},
                "completed": {"type": "boolean"},
                "is_blocked": {"type": "boolean"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent", "medium"]},
                "query": {"type": "string"},
                "tags": tags,
                "sort": {"type": "string", "enum": ["updated_desc", "updated_asc", "created_desc", "created_asc", "priority_desc", "priority_asc", "title"]},
                "project_id": project,
                "offset": offset,
                "limit": limit,
            }), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "todo_get",
            "description": "One todo: fields, revision, lock (locked_by / lock_expires_at, an expired lease reads as none), blocker_ids, derived is_blocked, comment_count; include_comments=true adds the comments.",
            "inputSchema": schema(json!({"todo_id": tid, "include_comments": {"type": "boolean"}}), &["todo_id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "todo_update",
            "description": "Update a subset of fields; omitted fields are preserved. status=done marks completed. Refused with a `locked` error ({locked_by, lock_expires_at}) while another actor holds a live lock; expected_revision (optional) refuses with revision_conflict when stale.",
            "inputSchema": schema(json!({
                "todo_id": tid,
                "title": {"type": "string"},
                "body": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent", "medium"]},
                "status": {"type": "string", "enum": ["open", "in_progress", "done", "completed"]},
                "tags": tags,
                "expected_revision": rev_opt,
                "response_mode": mode,
            }), &["todo_id"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "todo_add_tag",
            "description": "Add one tag without replacing the others (lock-guarded).",
            "inputSchema": schema(json!({"todo_id": tid, "tag": {"type": "string"}, "response_mode": mode}), &["todo_id", "tag"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_remove_tag",
            "description": "Remove one tag without replacing the others (lock-guarded).",
            "inputSchema": schema(json!({"todo_id": tid, "tag": {"type": "string"}, "response_mode": mode}), &["todo_id", "tag"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_set_blockers",
            "description": "Replace a todo's blocker list. Blockers must be in the same project; a cycle (including self) is a `cycle` error. Answers blocker_ids + is_blocked.",
            "inputSchema": schema(json!({"todo_id": tid, "blocker_ids": {"type": "array", "items": {"type": "integer", "minimum": 1}}, "response_mode": mode}), &["todo_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_add_blocker",
            "description": "Add one blocker (cycle-checked) without replacing the others.",
            "inputSchema": schema(json!({"todo_id": tid, "blocker_id": {"type": "integer", "minimum": 1}, "response_mode": mode}), &["todo_id", "blocker_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_remove_blocker",
            "description": "Remove one blocker without replacing the others.",
            "inputSchema": schema(json!({"todo_id": tid, "blocker_id": {"type": "integer", "minimum": 1}, "response_mode": mode}), &["todo_id", "blocker_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_complete",
            "description": "Mark a todo complete (status=done) or incomplete (back to open). Releases YOUR lock unless release_lock=false. affected_todo_ids = todos this one was blocking.",
            "inputSchema": schema(json!({"todo_id": tid, "completed": {"type": "boolean"}, "release_lock": {"type": "boolean", "description": "Default true."}, "response_mode": mode}), &["todo_id", "completed"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_lock",
            "description": "Take (or renew) an edit lease on a todo, keyed by YOUR process id. Other actors' todo_update / tags / blockers / complete / transfer calls are refused with `locked` naming you until it expires or you todo_unlock. Default lease 300 s, max 24 h. ADVISORY coordination, not access control: the actor id is the unauthenticated X-Chappa-Actor header (any holder of the control token can claim any id), so a lock is a courtesy between cooperating agents that prevents lost updates — never rely on it for isolation.",
            "inputSchema": schema(json!({
                "todo_id": tid,
                "lease_ttl_seconds": {"type": "integer", "minimum": 1, "description": "Lease length in seconds (default 300)."},
                "lease_ms": {"type": "integer", "minimum": 1, "description": "Lease length in milliseconds (alternative to lease_ttl_seconds)."},
                "response_mode": mode,
            }), &["todo_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_unlock",
            "description": "Release a todo lock you hold (none/expired = no-op success; someone else's live lock = `locked` error).",
            "inputSchema": schema(json!({"todo_id": tid, "response_mode": mode}), &["todo_id"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_comment_create",
            "description": "Add a comment to a todo (attributed to you). Slim receipt {comment_id, todo_id}.",
            "inputSchema": schema(json!({"todo_id": tid, "body": {"type": "string"}, "response_mode": mode}), &["todo_id", "body"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "todo_comment_update",
            "description": "Edit a comment's body.",
            "inputSchema": schema(json!({"comment_id": cid, "body": {"type": "string"}, "response_mode": mode}), &["comment_id", "body"]),
            "annotations": annotations(false, false, true),
        }),
        json!({
            "name": "todo_comment_delete",
            "description": "Delete a comment. Requires confirm=true.",
            "inputSchema": schema(json!({"comment_id": cid, "confirm": {"type": "boolean", "description": "Must be true."}}), &["comment_id", "confirm"]),
            "annotations": annotations(false, true, true),
        }),
        json!({
            "name": "todo_comment_list",
            "description": "Comments on a todo, oldest first.",
            "inputSchema": schema(json!({"todo_id": tid, "offset": offset, "limit": limit}), &["todo_id"]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "todo_transfer",
            "description": "Move a todo to another project: comments and completion kept, blockers (both directions) and lock cleared. affected_todo_ids = todos it was blocking.",
            "inputSchema": schema(json!({"todo_id": tid, "target_project_id": {"type": "integer", "minimum": 0}, "response_mode": mode}), &["todo_id", "target_project_id"]),
            "annotations": annotations(false, false, false),
        }),
        json!({
            "name": "todo_tags_list",
            "description": "Distinct todo tags in the project scope.",
            "inputSchema": schema(json!({"project_id": project}), &[]),
            "annotations": annotations(true, false, true),
        }),
        json!({
            "name": "todo_delete",
            "description": "Delete a todo with its comments and blocker edges. Requires confirm=true — no undo. Answers affected_todo_ids (todos it was blocking).",
            "inputSchema": schema(json!({"todo_id": tid, "confirm": {"type": "boolean", "description": "Must be true."}}), &["todo_id", "confirm"]),
            "annotations": annotations(false, true, true),
        }),
    ]
}

/// A REQUIRED non-negative integer argument. Separate from [`opt_u64`] on
/// purpose: one `arg_u64(_, _, required: bool) -> Option<u64>` forced every
/// required call site to `.unwrap()` a value the flag had already guaranteed.
fn req_u64(args: &Value, key: &str) -> Result<u64, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Err(ToolError::Invalid(format!("`{key}` is required"))),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| ToolError::Invalid(format!("`{key}` must be a non-negative integer"))),
    }
}

/// An OPTIONAL non-negative integer argument (absent and `null` are the same).
fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::Invalid(format!("`{key}` must be a non-negative integer"))),
    }
}

fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::Invalid(format!("`{key}` is required and must be a string")))
}

/// Copy the optional keys that exist in `args` into a POST body.
fn pick(args: &Value, keys: &[&str]) -> Value {
    let mut body = serde_json::Map::new();
    for key in keys {
        if let Some(v) = args.get(*key) {
            if !v.is_null() {
                body.insert((*key).to_owned(), v.clone());
            }
        }
    }
    Value::Object(body)
}

/// Translate one tool call into exactly ONE control request.
pub fn call_tool(client: &Client, name: &str, args: &Value) -> Result<Value, ToolError> {
    let args = if args.is_null() { &Value::Object(Default::default()) } else { args };
    match name {
        "list_processes" => client.request("GET", "/processes", None),
        "get_process_status" => {
            let id = req_u64(args, "id")?;
            client.request("GET", &format!("/processes/{id}"), None)
        }
        "get_process_output" => {
            let id = req_u64(args, "id")?;
            let mut query = Vec::new();
            if let Some(lines) = opt_u64(args, "lines")? {
                query.push(format!("lines={lines}"));
            }
            if let Some(offset) = opt_u64(args, "offset")? {
                query.push(format!("offset={offset}"));
            }
            let qs = if query.is_empty() { String::new() } else { format!("?{}", query.join("&")) };
            client.request("GET", &format!("/processes/{id}/output{qs}"), None)
        }
        "get_input_journal" => {
            let id = req_u64(args, "id")?;
            client.request("GET", &format!("/processes/{id}/input_journal"), None)
        }
        "list_projects" => client.request("GET", "/projects", None),
        "get_project" => {
            let id = req_u64(args, "id")?;
            client.request("GET", &format!("/projects/{id}"), None)
        }
        "list_agent_tools" => client.request("GET", "/agent_tools", None),
        "reap_orphans" => client.request("POST", "/agents/reap", Some(&json!({}))),
        "get_agent_events" => {
            let id = req_u64(args, "id")?;
            let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
            client.request("GET", &format!("/processes/{id}/agent_events?since={since}"), None)
        }
        "spawn_agent" => {
            let tool = req_u64(args, "agent_tool_id")?;
            if let Some(a) = args.get("extra_args") {
                if !a.is_null() && !a.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                    return Err(ToolError::Invalid("`extra_args` must be an array of strings".into()));
                }
            }
            let mut body = pick(
                args,
                &["project_id", "name", "extra_args", "prompt", "force", "cols", "rows", "parent_process_id", "close_on_exit"],
            );
            body["agent_tool_id"] = json!(tool);
            client.request("POST", "/agents", Some(&body))
        }
        "spawn_terminal" => {
            if let Some(a) = args.get("args") {
                if !a.is_null() && !a.is_array() {
                    return Err(ToolError::Invalid("`args` must be an array of strings".into()));
                }
            }
            let body = pick(args, &["command", "args", "cwd", "name", "cols", "rows", "env"]);
            client.request("POST", "/processes", Some(&body))
        }
        "send_input" => {
            let id = req_u64(args, "id")?;
            let text = arg_str(args, "text")?;
            // text + submit travel in ONE request: the app appends \r to the
            // same pty write (never text now, Enter later).
            let mut body = pick(args, &["submit", "wait_ms"]);
            body["text"] = Value::String(text);
            client.request("POST", &format!("/processes/{id}/input"), Some(&body))
        }
        "send_bytes" => {
            let id = req_u64(args, "id")?;
            let bytes = args
                .get("bytes")
                .and_then(Value::as_array)
                .ok_or_else(|| ToolError::Invalid("`bytes` is required and must be an array".into()))?;
            if bytes.iter().any(|b| !b.as_u64().is_some_and(|v| v <= 255)) {
                return Err(ToolError::Invalid("`bytes` values must be integers 0..=255".into()));
            }
            let mut body = pick(args, &["wait_ms"]);
            body["bytes"] = Value::Array(bytes.clone());
            client.request("POST", &format!("/processes/{id}/bytes"), Some(&body))
        }
        "start_process" | "stop_process" | "restart_process" => {
            let project = req_u64(args, "project_id")?;
            let pname = arg_str(args, "name")?;
            let action = name.strip_suffix("_process").unwrap();
            let body = pick(args, &["wait_ms"]);
            client.request(
                "POST",
                &format!("/projects/{project}/processes/{}/{action}", encode_segment(&pname)),
                Some(&body),
            )
        }
        "close_terminal" => {
            let id = req_u64(args, "id")?;
            let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            if !confirm {
                return Err(ToolError::Invalid(
                    "close_terminal requires confirm=true (it kills the child; no undo)".into(),
                ));
            }
            client.request("POST", &format!("/processes/{id}/close"), Some(&json!({"confirm": true})))
        }
        other if other.starts_with("timer_") => call_timer_tool(client, other, args),
        other => call_coordination_tool(client, other, args),
    }
}

/// Tools → timer routes. `delivery_process_id` is deliberately NOT
/// filled in here: the app defaults it from the `X-Chappa-Actor` header this
/// client already sends, so there is ONE source of truth for "the caller's
/// own process". `project_id` IS filled in from `CHAPPA_AI_PROJECT_ID` like
/// every other coordination tool ([`scoped`]).
fn call_timer_tool(client: &Client, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "timer_set" => {
            req_u64(args, "delay_ms")?;
            arg_str(args, "body")?;
            let body = scoped(
                client,
                args,
                &["delay_ms", "body", "delivery_process_id", "loop", "repeat_every_ms", "name", "project_id"],
            )?;
            client.request("POST", "/timers", Some(&body))
        }
        "timer_fire_when_idle_any" | "timer_fire_when_idle_all" => {
            let processes = args
                .get("processes")
                .and_then(Value::as_array)
                .ok_or_else(|| ToolError::Invalid("`processes` is required and must be an array".into()))?;
            if processes.is_empty() {
                return Err(ToolError::Invalid("`processes` must not be empty".into()));
            }
            req_u64(args, "max_wait_ms")?;
            arg_str(args, "body")?;
            let route = if name.ends_with("_any") { "idle_any" } else { "idle_all" };
            let body = scoped(
                client,
                args,
                &["processes", "max_wait_ms", "body", "delivery_process_id", "idle_ms", "confirm_ms", "rearm", "name", "project_id"],
            )?;
            client.request("POST", &format!("/timers/{route}"), Some(&body))
        }
        "timer_list" => {
            let scoped = scoped(client, args, &["include_fired", "all", "limit", "offset", "project_id"])?;
            let qs = query_string(&scoped, &["include_fired", "all", "limit", "offset", "project_id"]);
            client.request("GET", &format!("/timers{qs}"), None)
        }
        "timer_cancel" | "timer_pause" | "timer_resume" => {
            let id = req_u64(args, "timer_id")?;
            let action = name.strip_prefix("timer_").unwrap_or(name);
            client.request("POST", &format!("/timers/{id}/{action}"), Some(&json!({})))
        }
        other => Err(ToolError::Invalid(format!("unknown tool `{other}`"))),
    }
}

/// Percent-encode a query value.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `?k=v&…` from the listed args (arrays become comma lists, scalars their
/// text). Absent/null keys are skipped; an empty set is an empty string.
fn query_string(args: &Value, keys: &[&str]) -> String {
    let mut parts = Vec::new();
    for key in keys {
        let text = match args.get(*key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
                .collect::<Vec<_>>()
                .join(","),
            Some(other) => other.to_string(),
        };
        parts.push(format!("{key}={}", encode_query(&text)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// The project scope for a coordination call: an explicit `project_id`
/// (an explicit `null` is honoured — it means GLOBAL, and is sent as JSON
/// null), else this process's `CHAPPA_AI_PROJECT_ID` when the field is
/// ABSENT. `all_projects: true` drops it.
fn scoped(client: &Client, args: &Value, keys: &[&str]) -> Result<Value, ToolError> {
    let mut body = pick(args, keys);
    if args.get("all_projects").and_then(Value::as_bool) == Some(true) {
        return Ok(body);
    }
    match args.get("project_id") {
        Some(Value::Null) => body["project_id"] = Value::Null,
        Some(_) => body["project_id"] = json!(req_u64(args, "project_id")?),
        None => {
            if let Some(p) = client.project_id {
                body["project_id"] = json!(p);
            }
        }
    }
    Ok(body)
}

/// For the GET list tools: an explicit `project_id: null` from [`scoped`]
/// becomes the `global` marker the `/scratchpads` routes understand
/// (`query_string` would otherwise drop the null and list every pad).
fn global_marker(scoped: &mut Value) {
    if matches!(scoped.get("project_id"), Some(Value::Null)) {
        scoped["project_id"] = json!("global");
    }
}

fn require_confirm(args: &Value, tool: &str) -> Result<(), ToolError> {
    if args.get("confirm").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(ToolError::Invalid(format!("{tool} requires confirm=true (no undo)")))
    }
}

/// Tools → coordination routes. Every call is still exactly one
/// request; reads are GETs with query strings, writes POST JSON bodies.
fn call_coordination_tool(client: &Client, name: &str, args: &Value) -> Result<Value, ToolError> {
    const MODE: &str = "response_mode";
    match name {
        "scratchpad_write" => {
            arg_str(args, "name")?;
            arg_str(args, "content")?;
            match opt_u64(args, "scratchpad_id")? {
                Some(id) => {
                    if opt_u64(args, "expected_revision")?.is_none() {
                        return Err(ToolError::Invalid(
                            "scratchpad_write with scratchpad_id (overwrite) requires expected_revision — read the pad first".into(),
                        ));
                    }
                    let body = pick(args, &["name", "content", "tags", "expected_revision", MODE]);
                    client.request("POST", &format!("/scratchpads/{id}"), Some(&body))
                }
                None => {
                    let body = scoped(client, args, &["name", "content", "tags", MODE])?;
                    client.request("POST", "/scratchpads", Some(&body))
                }
            }
        }
        "scratchpad_read" => {
            let id = req_u64(args, "scratchpad_id")?;
            let qs = query_string(args, &["mode", "section_heading", "offset", "limit"]);
            client.request("GET", &format!("/scratchpads/{id}{qs}"), None)
        }
        "scratchpad_list" => {
            let mut scoped = scoped(client, args, &["query", "tags", "include_archived", "offset", "limit"])?;
            global_marker(&mut scoped);
            let qs = query_string(&scoped, &["project_id", "query", "tags", "include_archived", "offset", "limit"]);
            client.request("GET", &format!("/scratchpads{qs}"), None)
        }
        "scratchpad_find" => {
            let id = req_u64(args, "scratchpad_id")?;
            let query = arg_str(args, "query")?;
            if query.trim().is_empty() {
                return Err(ToolError::Invalid("`query` must not be empty".into()));
            }
            let qs = query_string(args, &["query", "case_sensitive", "limit", "context_lines", "scope"]);
            client.request("GET", &format!("/scratchpads/{id}/find{qs}"), None)
        }
        "scratchpad_tail" => {
            let id = req_u64(args, "scratchpad_id")?;
            let qs = query_string(args, &["lines"]);
            client.request("GET", &format!("/scratchpads/{id}/tail{qs}"), None)
        }
        "scratchpad_append" => {
            let id = req_u64(args, "scratchpad_id")?;
            arg_str(args, "content")?;
            let body = pick(args, &["content", "expected_revision", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/append"), Some(&body))
        }
        "scratchpad_append_section" => {
            let id = req_u64(args, "scratchpad_id")?;
            arg_str(args, "heading")?;
            arg_str(args, "content")?;
            let body = pick(args, &["heading", "content", "expected_revision", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/append_section"), Some(&body))
        }
        "scratchpad_edit" => {
            let id = req_u64(args, "scratchpad_id")?;
            if !args.get("target").is_some_and(Value::is_object) {
                return Err(ToolError::Invalid("`target` is required and must be an object".into()));
            }
            arg_str(args, "content")?;
            req_u64(args, "expected_revision")?;
            let body = pick(args, &["target", "content", "expected_revision", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/edit"), Some(&body))
        }
        "scratchpad_rename" => {
            let id = req_u64(args, "scratchpad_id")?;
            arg_str(args, "name")?;
            req_u64(args, "expected_revision")?;
            let body = pick(args, &["name", "expected_revision", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/rename"), Some(&body))
        }
        "scratchpad_add_tags" | "scratchpad_remove_tags" => {
            let id = req_u64(args, "scratchpad_id")?;
            if !args.get("tags").is_some_and(Value::is_array) {
                return Err(ToolError::Invalid("`tags` is required and must be an array of strings".into()));
            }
            req_u64(args, "expected_revision")?;
            let action = if name == "scratchpad_add_tags" { "add" } else { "remove" };
            let body = pick(args, &["tags", "expected_revision", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/tags/{action}"), Some(&body))
        }
        "scratchpad_tags_list" => {
            let mut scoped = scoped(client, args, &[])?;
            global_marker(&mut scoped);
            let qs = query_string(&scoped, &["project_id"]);
            client.request("GET", &format!("/scratchpads/tags{qs}"), None)
        }
        "scratchpad_clear" | "scratchpad_delete" => {
            let id = req_u64(args, "scratchpad_id")?;
            req_u64(args, "expected_revision")?;
            require_confirm(args, name)?;
            let action = if name == "scratchpad_clear" { "clear" } else { "delete" };
            let mut body = pick(args, &["expected_revision", MODE]);
            body["confirm"] = json!(true);
            client.request("POST", &format!("/scratchpads/{id}/{action}"), Some(&body))
        }
        "scratchpad_archive" => {
            let id = req_u64(args, "scratchpad_id")?;
            let body = pick(args, &["archived", MODE]);
            client.request("POST", &format!("/scratchpads/{id}/archive"), Some(&body))
        }
        "scratchpad_transfer" => {
            let id = req_u64(args, "scratchpad_id")?;
            let target = match args.get("target_project_id") {
                None => return Err(ToolError::Invalid("`target_project_id` is required (null = global)".into())),
                Some(Value::Null) => Value::Null,
                Some(_) => json!(req_u64(args, "target_project_id")?),
            };
            let mut body = pick(args, &["expected_revision", MODE]);
            body["target_project_id"] = target;
            client.request("POST", &format!("/scratchpads/{id}/transfer"), Some(&body))
        }
        // ---- todos ----
        "todo_create" => {
            arg_str(args, "title")?;
            let body = scoped(client, args, &["title", "body", "priority", "tags", MODE])?;
            if body.get("project_id").is_none() {
                return Err(ToolError::Invalid(
                    "`project_id` is required (todos are project-scoped) — pass it, or run inside a chappa-ai-spawned agent where CHAPPA_AI_PROJECT_ID is set".into(),
                ));
            }
            client.request("POST", "/todos", Some(&body))
        }
        "todo_list" => {
            let scoped = scoped(client, args, &["status", "completed", "is_blocked", "priority", "query", "tags", "sort", "offset", "limit"])?;
            let qs = query_string(&scoped, &["project_id", "status", "completed", "is_blocked", "priority", "query", "tags", "sort", "offset", "limit"]);
            client.request("GET", &format!("/todos{qs}"), None)
        }
        "todo_get" => {
            let id = req_u64(args, "todo_id")?;
            let qs = query_string(args, &["include_comments"]);
            client.request("GET", &format!("/todos/{id}{qs}"), None)
        }
        "todo_update" => {
            let id = req_u64(args, "todo_id")?;
            let body = pick(args, &["title", "body", "priority", "status", "tags", "expected_revision", MODE]);
            client.request("POST", &format!("/todos/{id}"), Some(&body))
        }
        "todo_add_tag" | "todo_remove_tag" => {
            let id = req_u64(args, "todo_id")?;
            arg_str(args, "tag")?;
            let action = if name == "todo_add_tag" { "add" } else { "remove" };
            let body = pick(args, &["tag", MODE]);
            client.request("POST", &format!("/todos/{id}/tags/{action}"), Some(&body))
        }
        "todo_set_blockers" => {
            let id = req_u64(args, "todo_id")?;
            let mut body = pick(args, &["blocker_ids", MODE]);
            if body.get("blocker_ids").is_none() {
                body["blocker_ids"] = json!([]);
            } else if !body["blocker_ids"].is_array() {
                return Err(ToolError::Invalid("`blocker_ids` must be an array of integers".into()));
            }
            client.request("POST", &format!("/todos/{id}/blockers/set"), Some(&body))
        }
        "todo_add_blocker" | "todo_remove_blocker" => {
            let id = req_u64(args, "todo_id")?;
            req_u64(args, "blocker_id")?;
            let action = if name == "todo_add_blocker" { "add" } else { "remove" };
            let body = pick(args, &["blocker_id", MODE]);
            client.request("POST", &format!("/todos/{id}/blockers/{action}"), Some(&body))
        }
        "todo_complete" => {
            let id = req_u64(args, "todo_id")?;
            if !args.get("completed").is_some_and(Value::is_boolean) {
                return Err(ToolError::Invalid("`completed` is required and must be a boolean".into()));
            }
            let body = pick(args, &["completed", "release_lock", MODE]);
            client.request("POST", &format!("/todos/{id}/complete"), Some(&body))
        }
        "todo_lock" => {
            let id = req_u64(args, "todo_id")?;
            let body = pick(args, &["lease_ttl_seconds", "lease_ms", MODE]);
            client.request("POST", &format!("/todos/{id}/lock"), Some(&body))
        }
        "todo_unlock" => {
            let id = req_u64(args, "todo_id")?;
            let body = pick(args, &[MODE]);
            client.request("POST", &format!("/todos/{id}/unlock"), Some(&body))
        }
        "todo_comment_create" => {
            let id = req_u64(args, "todo_id")?;
            arg_str(args, "body")?;
            let body = pick(args, &["body", MODE]);
            client.request("POST", &format!("/todos/{id}/comments"), Some(&body))
        }
        "todo_comment_update" => {
            let id = req_u64(args, "comment_id")?;
            arg_str(args, "body")?;
            let body = pick(args, &["body", MODE]);
            client.request("POST", &format!("/todo_comments/{id}"), Some(&body))
        }
        "todo_comment_delete" => {
            let id = req_u64(args, "comment_id")?;
            require_confirm(args, name)?;
            client.request("POST", &format!("/todo_comments/{id}/delete"), Some(&json!({"confirm": true})))
        }
        "todo_comment_list" => {
            let id = req_u64(args, "todo_id")?;
            let qs = query_string(args, &["offset", "limit"]);
            client.request("GET", &format!("/todos/{id}/comments{qs}"), None)
        }
        "todo_transfer" => {
            let id = req_u64(args, "todo_id")?;
            req_u64(args, "target_project_id")?;
            let body = pick(args, &["target_project_id", MODE]);
            client.request("POST", &format!("/todos/{id}/transfer"), Some(&body))
        }
        "todo_tags_list" => {
            let scoped = scoped(client, args, &[])?;
            let qs = query_string(&scoped, &["project_id"]);
            client.request("GET", &format!("/todos/tags{qs}"), None)
        }
        "todo_delete" => {
            let id = req_u64(args, "todo_id")?;
            require_confirm(args, name)?;
            client.request("POST", &format!("/todos/{id}/delete"), Some(&json!({"confirm": true})))
        }
        other => Err(ToolError::Invalid(format!("unknown tool `{other}`"))),
    }
}

// ---- JSON-RPC ---------------------------------------------------------------

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// A tool outcome as MCP `tools/call` content. Errors are `isError: true`
/// text (the model sees the message and can act on it) — never a JSON-RPC
/// error, which clients render as a protocol failure.
fn tool_result(outcome: Result<Value, ToolError>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_default();
            json!({"content": [{"type": "text", "text": text}], "isError": false})
        }
        Err(err) => json!({"content": [{"type": "text", "text": err.to_string()}], "isError": true}),
    }
}

/// Handle one incoming JSON-RPC message. `None` = nothing to write back
/// (notifications, and requests without an id).
pub fn handle_message(client: &Client, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = id.is_null() || method.starts_with("notifications/");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            rpc_result(
                &id,
                json!({
                    "protocolVersion": requested,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                    "instructions": "Tools proxy to the chappa-ai desktop app's control surface. Read-only tools are safe to call freely. Prefer send_input receipts + get_process_status liveness over screen-diffing; get_process_output returns the whole screen by default. Scratchpads/todos (scratchpad_*, todo_*) are project-scoped working memory shared with other agents: read before you write, pass expected_revision on guarded writes, and treat revision_conflict / locked errors as 'someone else moved first' — re-read, never overwrite blind. Timers (timer_*) wake a process with a body injected as a fresh user turn: idle is measured off the pty byte stream and re-validated before firing, so trust timer_fire_when_idle_* instead of hand-rolling an idle_seconds rule, and check timer_list(include_fired: true) to see whether a wake-up was actually delivered.",
                }),
            )
        }
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            rpc_result(&id, tool_result(call_tool(client, name, &args)))
        }
        _ if is_notification => return None,
        other => rpc_error(&id, -32601, format!("method not found: {other}")),
    };
    if is_notification {
        None
    } else {
        Some(reply)
    }
}

/// The stdio loop: one JSON message per line in, one per line out.
pub fn serve_stdio(client: &Client) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break, // client hung up
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(trimmed) {
            Ok(msg) => handle_message(client, &msg),
            Err(err) => Some(rpc_error(&Value::Null, -32700, format!("parse error: {err}"))),
        };
        if let Some(reply) = reply {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{reply}");
            let _ = out.flush();
        }
    }
}

use std::io::BufRead as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// Review fix: a structured error body is rendered whole, but capped —
    /// a 40 KB `current_content` must not become a 40 KB tool error.
    #[test]
    fn structured_error_text_is_capped_on_a_char_boundary() {
        assert_eq!(cap_text("short"), "short");
        let big = "é".repeat(ERROR_TEXT_CAP);
        let capped = cap_text(&big);
        assert!(capped.len() < big.len());
        assert!(capped.contains("[truncated"), "{}", &capped[capped.len() - 40..]);
        let err = outcome(409, json!({"error": "revision_conflict", "message": "m", "current_revision": 3, "current_content": big})).unwrap_err();
        let text = err.to_string();
        assert!(text.len() < ERROR_TEXT_CAP + 200, "{}", text.len());
        assert!(text.starts_with("409") || text.contains("revision_conflict"), "{text}");
    }

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:8324"),
            Some(("127.0.0.1".into(), 8324, String::new()))
        );
        assert_eq!(
            parse_http_url("http://localhost:1/x/"),
            Some(("localhost".into(), 1, "/x".into()))
        );
        assert_eq!(parse_http_url("https://x"), None);
    }

    /// A path prefix in `CHAPPA_AI_CONTROL_URL` is HONOURED, not dropped: the
    /// request line is prefix + route path. (It used to parse the prefix and
    /// then ignore it, so a proxied base url silently hit the wrong paths.)
    #[test]
    fn a_url_path_prefix_is_prepended_to_route_paths() {
        let (_, _, prefix) = parse_http_url("http://host:8324/chappa/").unwrap();
        assert_eq!(prefix, "/chappa");
        assert_eq!(format!("{prefix}/processes/1/output"), "/chappa/processes/1/output");
        let (_, _, none) = parse_http_url("http://host:8324").unwrap();
        assert_eq!(format!("{none}/processes"), "/processes", "no prefix = unchanged");
    }

    /// The reply is parsed on the byte slice: status off the header block,
    /// body borrowed for `from_slice`.
    #[test]
    fn http_replies_are_parsed_on_the_bytes() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"error\":\"nope\"}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 404);
        assert_eq!(body, b"{\"error\":\"nope\"}");
        assert_eq!(
            outcome(status, serde_json::from_slice(body).unwrap()).unwrap_err(),
            ToolError::Http { status: 404, message: "nope".into() }
        );
        // A body with no header terminator is malformed, not a silent empty.
        assert!(parse_http_response(b"HTTP/1.1 200 OK").is_none());
        // A 200 with a non-JSON body still comes back as the raw text.
        let (status, body) = parse_http_response(b"HTTP/1.1 200 OK\r\n\r\nnot json").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"not json");
    }

    #[test]
    fn segment_encoding_keeps_spaces_and_plus_safe() {
        assert_eq!(encode_segment("import + summarize on open"), "import%20%2B%20summarize%20on%20open");
        assert_eq!(encode_segment("chappa-ai_tui.v2~x"), "chappa-ai_tui.v2~x");
    }
}
