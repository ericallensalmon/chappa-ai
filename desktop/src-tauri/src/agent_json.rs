//! The structured (`json`) agent transport.
//!
//! A `transport: json` tool spawns its CLI in MACHINE MODE over plain pipes
//! — no pty, no VT parsing. A line-delimited JSON reader turns each stdout
//! line into typed events through a per-CLI adapter ([`Adapter`]):
//!
//! ```text
//! agent://event {id, seq, ts, kind, payload}
//! kind = turn_started | turn_ended | tool_call | text | usage
//!      | awaiting_input | compaction | error | raw
//! ```
//!
//! Every event lands in a bounded per-agent ring ([`EventRing`],
//! [`EVENT_RING_CAP`]) that `get_agent_events(id, since)` reads back, and is
//! pushed to the webview as `agent://event`. Unknown native events pass
//! through as `kind: "raw"` with the original object — nothing is lost.
//!
//! Why pipes and not the pty: on Windows a ConPTY re-renders whatever the
//! child writes as a VT stream sized to the window (line wraps at `cols`,
//! cursor moves, colour resets), so a 6 KB `result` line would arrive
//! fragmented and decorated — unparseable without exactly the VT layer this
//! transport exists to skip. `std::process::Command` with piped stdio gives
//! byte-exact lines on every platform, and `docker exec -i` (no `-t`) does
//! the same for container runtimes. The registry entry is fronted by a
//! no-pty actor (`spawn_actor_nopty`) so the rail row, the liveness counters
//! (`has_output`, `last_output_at`, `child_alive`), the busy guard, the close
//! path and `list_processes` all keep working unchanged; the actor is told
//! about bytes and the exit through `TermHandle::note_output` /
//! `note_child_exit` and never sees the JSON itself.
//!
//! Two machine modes, two shapes:
//!
//! - **claude** (`-p --output-format stream-json --input-format stream-json
//!   --verbose`) is PERSISTENT: one child for the agent's life, user
//!   messages are written to its stdin as stream-json lines, every `result`
//!   is followed by `awaiting_input` because the process stays up waiting
//!   for the next line.
//! - **opencode** (`run --format json`) is PER-TURN: `run` answers one
//!   message and exits. The first child carries the spawn `prompt`; each
//!   later `send_input` spawns a fresh child continuing the same session
//!   (`-s <sessionID>` captured from the first event). Between children the
//!   agent is `awaiting_input`; a send while a child is still running is
//!   refused (`busy`).
//!
//! `send_input` on a json agent answers a [`TurnReceipt`]: delivered when the
//! NEXT `turn_started` arrived within `wait_ms`, else `reason: "timeout"`
//! (the message was still written — the receipt tells you the CLI did not
//! acknowledge it in time, not that it was lost).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use term_core::actor::{epoch_ms, TermHandle};
use term_core::pty::ExitStatus;

use crate::agent_tools::ToolType;
use crate::event_ring::{EventRing, Sequenced};
use crate::registry::TermId;

/// Per-agent event ring bound.
pub const EVENT_RING_CAP: usize = 1000;
/// `send_input` waits this long for the next `turn_started` unless asked
/// otherwise (claude takes ~1 s to acknowledge a message with its per-turn
/// `system/init`).
pub const TURN_RECEIPT_WAIT_DEFAULT_MS: u64 = 5_000;
/// Cap on the receipt wait — the MCP client's read timeout is 30 s.
pub const TURN_RECEIPT_WAIT_MAX_MS: u64 = 20_000;
/// The tool types with a machine mode (what the json transport requires).
/// The ONE source: `list_agent_tools` ships it to the webview
/// (`machine_mode_types`), the upsert validator names it, and a test pins
/// it to [`machine_mode`].
pub const MACHINE_MODE_TYPES: &[ToolType] = &[ToolType::Claude, ToolType::Opencode];

// ---- machine modes -----------------------------------------------------------------

/// Which native event dialect a CLI speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    ClaudeStreamJson,
    OpencodeJson,
}

/// How a CLI is driven in machine mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineMode {
    /// Flags inserted BEFORE the tool's own args (so `opencode run …` keeps
    /// `run` first and `-m model` after it).
    pub args: &'static [&'static str],
    pub adapter: AdapterKind,
    /// One child for the agent's life (input over stdin) vs one child per
    /// turn (input as the run's message, session continued with `-s`).
    pub persistent: bool,
}

/// Flags verified against the installed CLIs on 2026-08-29 (`claude --help`
/// 2.1.251, `opencode run --help`).
pub fn machine_mode(tool_type: ToolType) -> Option<MachineMode> {
    match tool_type {
        ToolType::Claude => Some(MachineMode {
            args: &[
                "-p",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--verbose",
            ],
            adapter: AdapterKind::ClaudeStreamJson,
            persistent: true,
        }),
        ToolType::Opencode => Some(MachineMode {
            args: &["run", "--format", "json"],
            adapter: AdapterKind::OpencodeJson,
            persistent: false,
        }),
        _ => None,
    }
}

// ---- typed events --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    TurnStarted,
    TurnEnded,
    ToolCall,
    Text,
    Usage,
    AwaitingInput,
    Compaction,
    Error,
    /// A native event the adapter does not classify: `payload` is the
    /// original object (or `{"line": …}` for a non-JSON line, `{"stderr": …}`
    /// for a stderr line).
    Raw,
}

impl AgentEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentEventKind::TurnStarted => "turn_started",
            AgentEventKind::TurnEnded => "turn_ended",
            AgentEventKind::ToolCall => "tool_call",
            AgentEventKind::Text => "text",
            AgentEventKind::Usage => "usage",
            AgentEventKind::AwaitingInput => "awaiting_input",
            AgentEventKind::Compaction => "compaction",
            AgentEventKind::Error => "error",
            AgentEventKind::Raw => "raw",
        }
    }
}

/// One `agent://event`. `seq` is per agent, 1-based, monotonic — the
/// `get_agent_events(id, since)` cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: TermId,
    pub seq: u64,
    /// Unix ms when the line was read.
    pub ts: u64,
    pub kind: AgentEventKind,
    pub payload: Value,
}

/// A mapped event before it is sequenced.
pub type Mapped = (AgentEventKind, Value);

// ---- adapters (pure) -------------------------------------------------------------------

/// Native line → typed events. Stateful only for turn tracking (a
/// `turn_started` is synthesized when content arrives outside a turn, so a
/// CLI that skips its start marker still yields well-formed turns) and the
/// session id (opencode's `-s` continuation).
#[derive(Debug, Clone)]
pub struct Adapter {
    pub kind: AdapterKind,
    pub in_turn: bool,
    /// The CLI's OWN session id — taken only from its top-level session
    /// line (claude `system/init` without a `parent_tool_use_id`, opencode's
    /// first `step_start`), never from nested/subagent events, which carry
    /// their own ids and would hijack the `-s` continuation.
    pub session_id: Option<String>,
    /// The last model name seen (claude's `system/init`), for the usage
    /// context-window lookup.
    model: Option<String>,
    /// A persistent CLI spawned WITHOUT a prompt: its first ready signal
    /// (claude's `system/init`) means "waiting for the first message", not
    /// "a turn started". Cleared by the first send.
    pub ready_as_awaiting: bool,
}

/// A stdout line after the cheap, lock-free step: the JSON object, or the
/// non-JSON text (→ `raw {line}`).
pub type ParsedLine = Result<Value, String>;

impl Adapter {
    pub fn new(kind: AdapterKind) -> Self {
        Self {
            kind,
            in_turn: false,
            session_id: None,
            model: None,
            ready_as_awaiting: false,
        }
    }

    /// Parse one stdout line OUTSIDE any lock: `None` for a blank line.
    pub fn parse_line(line: &str) -> Option<ParsedLine> {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim().is_empty() {
            return None;
        }
        Some(serde_json::from_str(trimmed).map_err(|_| trimmed.to_owned()))
    }

    /// Map one stdout line (parse + classify; the tests' convenience).
    pub fn map_line(&mut self, line: &str) -> Vec<Mapped> {
        match Self::parse_line(line) {
            Some(parsed) => self.map_parsed(parsed),
            None => Vec::new(),
        }
    }

    /// Classify a parsed line. A non-JSON line is `raw {line}`.
    pub fn map_parsed(&mut self, parsed: ParsedLine) -> Vec<Mapped> {
        let value = match parsed {
            Ok(v) => v,
            Err(line) => return vec![(AgentEventKind::Raw, json!({"line": line}))],
        };
        match self.kind {
            AdapterKind::ClaudeStreamJson => self.map_claude(value),
            AdapterKind::OpencodeJson => self.map_opencode(value),
        }
    }

    /// The child exited. Persistent CLIs: a non-zero exit is an `error`
    /// (the agent itself is over — the actor reports `exited`). Per-turn
    /// CLIs: the turn is over and the agent is waiting for the next message
    /// (`awaiting_input`), a failed run is an `error` first.
    pub fn on_exit(&mut self, status: ExitStatus, persistent: bool) -> Vec<Mapped> {
        let mut out = Vec::new();
        if self.in_turn {
            self.in_turn = false;
            out.push((AgentEventKind::TurnEnded, json!({"reason": "exit", "exit_code": status.code})));
        }
        if !status.success {
            out.push((
                AgentEventKind::Error,
                json!({"message": "agent process exited with an error", "exit_code": status.code}),
            ));
        }
        if !persistent {
            out.push((AgentEventKind::AwaitingInput, json!({"after": "exit", "exit_code": status.code})));
        }
        out
    }

    /// The stdin line for a user message (persistent CLIs only).
    pub fn input_line(&self, text: &str) -> Option<String> {
        match self.kind {
            AdapterKind::ClaudeStreamJson => Some(
                json!({
                    "type": "user",
                    "message": {"role": "user", "content": [{"type": "text", "text": text}]}
                })
                .to_string(),
            ),
            AdapterKind::OpencodeJson => None,
        }
    }

    fn start_turn(&mut self, out: &mut Vec<Mapped>, payload: Value) {
        if !self.in_turn {
            self.in_turn = true;
            out.push((AgentEventKind::TurnStarted, payload));
        }
    }

    // -- claude `--output-format stream-json` -----------------------------------
    //
    // Observed 2026-08-29 (claude 2.1.251, real capture in
    // tests/fixtures/claude_stream_json.jsonl):
    //   system/init            per turn, session_id + model         → turn_started
    //   system/thinking_tokens estimate deltas                      → raw
    //   system/compact_boundary                                     → compaction
    //   rate_limit_event                                            → raw
    //   assistant  content [thinking|text|tool_use…]                → text / text{thinking} / tool_call{phase: call}
    //   user       content [tool_result…]                           → tool_call{phase: result}
    //   result     subtype success|error…, usage, modelUsage        → usage, (error), turn_ended, awaiting_input
    //   stream_event (only with --include-partial-messages)         → raw
    fn map_claude(&mut self, value: Value) -> Vec<Mapped> {
        let mut out = Vec::new();
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
        // A nested (subagent) event carries a `parent_tool_use_id`; its
        // session line is NOT ours.
        let nested = value.get("parent_tool_use_id").is_some_and(|p| !p.is_null());
        match (ty, subtype) {
            ("system", "init") if nested => out.push((AgentEventKind::Raw, value)),
            ("system", "init") => {
                if let Some(session) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(session.to_owned());
                }
                self.model = value.get("model").and_then(Value::as_str).map(str::to_owned);
                let payload = json!({
                    "session_id": self.session_id,
                    "model": self.model,
                    "cwd": value.get("cwd"),
                    "claude_code_version": value.get("claude_code_version"),
                });
                if self.ready_as_awaiting {
                    // Spawned without a prompt: the first init is the CLI
                    // saying "ready" — it is waiting for the first message.
                    self.ready_as_awaiting = false;
                    let mut payload = payload;
                    payload["after"] = json!("init");
                    out.push((AgentEventKind::AwaitingInput, payload));
                    return out;
                }
                // A second init inside a turn (claude re-announces per user
                // message) is the start of the NEXT turn.
                self.in_turn = false;
                self.start_turn(&mut out, payload);
            }
            ("system", "compact_boundary") => {
                out.push((AgentEventKind::Compaction, value));
            }
            ("assistant", _) => {
                self.start_turn(&mut out, json!({"session_id": self.session_id, "model": self.model, "implicit": true}));
                let blocks = value
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => out.push((AgentEventKind::Text, json!({"text": block.get("text")}))),
                        Some("thinking") => {
                            out.push((AgentEventKind::Text, json!({"text": block.get("thinking"), "thinking": true})))
                        }
                        Some("tool_use") => out.push((
                            AgentEventKind::ToolCall,
                            json!({
                                "phase": "call",
                                "tool_use_id": block.get("id"),
                                "name": block.get("name"),
                                "input": block.get("input"),
                            }),
                        )),
                        _ => out.push((AgentEventKind::Raw, block)),
                    }
                }
            }
            ("user", _) => {
                let blocks = value
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut classified = false;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        classified = true;
                        out.push((
                            AgentEventKind::ToolCall,
                            json!({
                                "phase": "result",
                                "tool_use_id": block.get("tool_use_id"),
                                "content": block.get("content"),
                                "is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            }),
                        ));
                    }
                }
                if !classified {
                    out.push((AgentEventKind::Raw, value));
                }
            }
            ("result", _) => {
                out.push((AgentEventKind::Usage, claude_usage(&value)));
                let is_error = value.get("is_error").and_then(Value::as_bool).unwrap_or(false) || subtype != "success";
                if is_error {
                    out.push((
                        AgentEventKind::Error,
                        json!({
                            "subtype": subtype,
                            "message": value.get("result").cloned().unwrap_or(Value::Null),
                            "errors": value.get("errors").cloned().unwrap_or(Value::Null),
                        }),
                    ));
                }
                self.in_turn = false;
                out.push((
                    AgentEventKind::TurnEnded,
                    json!({
                        "subtype": subtype,
                        "num_turns": value.get("num_turns"),
                        "duration_ms": value.get("duration_ms"),
                        "result": value.get("result"),
                        "stop_reason": value.get("stop_reason"),
                    }),
                ));
                // stream-json input: the process stays up for the next line.
                out.push((AgentEventKind::AwaitingInput, json!({"after": "result"})));
            }
            ("error", _) => out.push((AgentEventKind::Error, value)),
            _ => out.push((AgentEventKind::Raw, value)),
        }
        out
    }

    // -- opencode `run --format json` --------------------------------------------
    //
    // Observed 2026-08-29 (real capture in tests/fixtures/opencode_json.jsonl,
    // tool_use / error shapes from the opencode source since the probe run
    // used no tools):
    //   step_start   part.type step-start                    → turn_started
    //   text         part.text                                → text
    //   reasoning    part.text                                → text{thinking}
    //   tool_use     part.tool, part.state{status,input,output}→ tool_call{phase: call|result}
    //   step_finish  part.tokens{input,output,reasoning,cache}, part.cost → usage, turn_ended
    //   error        error{name, data}                        → error
    fn map_opencode(&mut self, value: Value) -> Vec<Mapped> {
        let mut out = Vec::new();
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
        let part = value.get("part").cloned().unwrap_or(Value::Null);
        match ty {
            "step_start" => {
                // The run's own session is announced by its FIRST step;
                // later steps with another id are a subagent's.
                if self.session_id.is_none() {
                    self.session_id = value.get("sessionID").and_then(Value::as_str).map(str::to_owned);
                }
                self.in_turn = false;
                self.start_turn(&mut out, json!({"session_id": self.session_id, "message_id": part.get("messageID")}));
            }
            "text" => {
                self.start_turn(&mut out, json!({"session_id": self.session_id, "implicit": true}));
                out.push((AgentEventKind::Text, json!({"text": part.get("text")})));
            }
            "reasoning" => {
                self.start_turn(&mut out, json!({"session_id": self.session_id, "implicit": true}));
                out.push((AgentEventKind::Text, json!({"text": part.get("text"), "thinking": true})));
            }
            "tool_use" | "tool" => {
                self.start_turn(&mut out, json!({"session_id": self.session_id, "implicit": true}));
                let state = part.get("state").cloned().unwrap_or(Value::Null);
                let status = state.get("status").and_then(Value::as_str).unwrap_or("");
                let phase = if matches!(status, "completed" | "error") { "result" } else { "call" };
                out.push((
                    AgentEventKind::ToolCall,
                    json!({
                        "phase": phase,
                        "tool_use_id": part.get("callID").or_else(|| part.get("id")),
                        "name": part.get("tool"),
                        "input": state.get("input"),
                        "content": state.get("output"),
                        "title": state.get("title"),
                        "is_error": status == "error",
                    }),
                ));
            }
            "step_finish" => {
                let tokens = part.get("tokens").cloned().unwrap_or(Value::Null);
                let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
                let cache_read = tokens.pointer("/cache/read").and_then(Value::as_u64).unwrap_or(0);
                out.push((
                    AgentEventKind::Usage,
                    json!({
                        "input_tokens": input,
                        "output_tokens": tokens.get("output"),
                        "reasoning_tokens": tokens.get("reasoning"),
                        "cache_read_input_tokens": cache_read,
                        "cache_creation_input_tokens": tokens.pointer("/cache/write"),
                        "total_tokens": tokens.get("total"),
                        "context_tokens": input + cache_read,
                        "context_window": Value::Null,
                        "context_pct": Value::Null,
                        "total_cost_usd": part.get("cost"),
                    }),
                ));
                self.in_turn = false;
                out.push((AgentEventKind::TurnEnded, json!({"reason": part.get("reason"), "message_id": part.get("messageID")})));
            }
            "error" => out.push((
                AgentEventKind::Error,
                json!({
                    "message": value.pointer("/error/data/message").or_else(|| value.get("error")),
                    "name": value.pointer("/error/name"),
                }),
            )),
            _ => out.push((AgentEventKind::Raw, value)),
        }
        out
    }
}

/// claude `result.usage` + `modelUsage` → the usage payload. Context =
/// what the last API call carried (input + cache read + cache creation)
/// against the model's context window.
fn claude_usage(value: &Value) -> Value {
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let context_tokens = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    let context_window = value
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|m| m.values().next())
        .and_then(|m| m.get("contextWindow"))
        .and_then(Value::as_u64);
    let context_pct = context_window
        .filter(|w| *w > 0)
        .map(|w| (context_tokens as f64 * 100.0 / w as f64 * 10.0).round() / 10.0);
    json!({
        "input_tokens": n("input_tokens"),
        "output_tokens": n("output_tokens"),
        "cache_read_input_tokens": n("cache_read_input_tokens"),
        "cache_creation_input_tokens": n("cache_creation_input_tokens"),
        "context_tokens": context_tokens,
        "context_window": context_window,
        "context_pct": context_pct,
        "total_cost_usd": value.get("total_cost_usd"),
        "num_turns": value.get("num_turns"),
    })
}

// ---- event ring -----------------------------------------------------------------------------

impl Sequenced for AgentEvent {
    fn seq(&self) -> u64 {
        self.seq
    }
}

// ---- the live agent ---------------------------------------------------------------------

/// What a `send_input` on a json agent answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnReceipt {
    /// The next `turn_started` arrived within the wait.
    pub delivered: bool,
    /// `timeout` (written, not acknowledged in time), `exited` (no child to
    /// write to / the child died before its turn started — `exited: <why>`
    /// when the CLI said why), `busy` (per-turn CLI still running the
    /// previous turn), `unsupported` (the CLI takes no input after its
    /// prompt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub waited_ms: u64,
    /// The effective wait (after the default + cap), so the caller can tell
    /// a short timeout from a slow CLI.
    pub wait_ms: u64,
    /// The ring cursor at the write; events after it are this turn's.
    pub seq_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_started_seq: Option<u64>,
}

impl TurnReceipt {
    /// Whether the message reached the CLI (delivered, or written but not
    /// acknowledged in time). A refused send (`busy` / `exited` /
    /// `unsupported`) is NOT written — and is not journaled.
    pub fn written(&self) -> bool {
        self.delivered || self.reason.as_deref() == Some("timeout")
    }
}

/// How to launch (and re-launch) the child: the spawn plan minus the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecipe {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: IndexMap<String, String>,
}

/// Side effects the agent raises: the `agent://event` emitter, called
/// OUTSIDE the agent's lock. (`awaiting_input` is no longer mirrored
/// anywhere — the registry derives it from [`JsonAgent::awaiting_input`]
/// at read time.)
pub struct JsonHooks {
    pub emit: Arc<dyn Fn(&AgentEvent) + Send + Sync>,
}

impl JsonHooks {
    pub fn none() -> Self {
        Self { emit: Arc::new(|_| {}) }
    }
}

struct JsonInner {
    ring: EventRing<AgentEvent>,
    adapter: Adapter,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// The current child's stdout reader. A per-turn send JOINS it before
    /// spawning the continuation, so the previous child's tail (its
    /// `turn_ended` / `awaiting_input`, its wait) can never be lost to a
    /// newer child.
    reader: Option<JoinHandle<()>>,
    turns_started: u64,
    awaiting_input: bool,
    /// An exit was reported for the current child (reset per spawn) — the
    /// receipt wait's short-circuit.
    exit_reported: bool,
    /// The agent is over: a persistent child died, or a per-turn
    /// continuation could not be spawned. Never "waiting" again.
    exited: bool,
    closed: bool,
}

/// One json-transport agent: the piped child (or, per turn, the current
/// one), its adapter, its event ring and the turn signal.
pub struct JsonAgent {
    id: TermId,
    handle: TermHandle,
    recipe: SpawnRecipe,
    mode: MachineMode,
    inner: Mutex<JsonInner>,
    /// Serializes per-turn sends (join the previous reader, spawn the next
    /// child) so two concurrent sends cannot both pass the `busy` check.
    turn_lock: Mutex<()>,
    turn_signal: Condvar,
    hooks: JsonHooks,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl JsonAgent {
    pub fn new(id: TermId, handle: TermHandle, recipe: SpawnRecipe, mode: MachineMode, hooks: JsonHooks) -> Arc<Self> {
        Self::with_clock(id, handle, recipe, mode, hooks, Arc::new(epoch_ms))
    }

    /// Test seam: an injectable clock for the receipt wait.
    pub fn with_clock(
        id: TermId,
        handle: TermHandle,
        recipe: SpawnRecipe,
        mode: MachineMode,
        hooks: JsonHooks,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            handle,
            recipe,
            mode,
            inner: Mutex::new(JsonInner {
                ring: EventRing::new(EVENT_RING_CAP),
                adapter: Adapter::new(mode.adapter),
                child: None,
                stdin: None,
                reader: None,
                turns_started: 0,
                awaiting_input: false,
                exit_reported: false,
                exited: false,
                closed: false,
            }),
            turn_lock: Mutex::new(()),
            turn_signal: Condvar::new(),
            hooks,
            clock,
        })
    }

    pub fn id(&self) -> TermId {
        self.id
    }

    pub fn mode(&self) -> MachineMode {
        self.mode
    }

    pub fn recipe(&self) -> &SpawnRecipe {
        &self.recipe
    }

    /// The typed "waiting for input" fact. A closed or exited agent is
    /// never waiting.
    pub fn awaiting_input(&self) -> bool {
        let inner = self.lock();
        inner.awaiting_input && !inner.closed && !inner.exited
    }

    /// The agent is over (persistent child died / continuation could not
    /// spawn) or closed.
    pub fn exited(&self) -> bool {
        let inner = self.lock();
        inner.exited || inner.closed
    }

    /// Whether a child process is running right now (per-turn: inside a
    /// turn; persistent: the agent's one child).
    pub fn child_alive(&self) -> bool {
        Self::child_running(&mut self.lock())
    }

    pub fn session_id(&self) -> Option<String> {
        self.lock().adapter.session_id.clone()
    }

    /// A persistent CLI spawned without a prompt: its first ready signal is
    /// `awaiting_input`, not a turn. Call BEFORE `spawn_child`.
    pub fn expect_ready_as_awaiting(&self) {
        self.lock().adapter.ready_as_awaiting = true;
    }

    pub fn events_since(&self, since: u64) -> Vec<AgentEvent> {
        self.lock().ring.since(since)
    }

    pub fn last_seq(&self) -> u64 {
        self.lock().ring.last_seq()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JsonInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the current child is still running (per-turn `busy`).
    fn child_running(inner: &mut JsonInner) -> bool {
        match inner.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// The argv for one child: the recipe, plus (per-turn) the session
    /// continuation and the message.
    pub fn child_args(&self, session_id: Option<&str>, message: Option<&str>) -> Vec<String> {
        let mut args = self.recipe.args.clone();
        if !self.mode.persistent {
            if let Some(session) = session_id {
                args.push("-s".into());
                args.push(session.to_owned());
            }
            if let Some(message) = message {
                args.push(message.to_owned());
            }
        }
        args
    }

    /// Launch the child (the first one, or the per-turn continuation) and
    /// start its reader threads. The caller guarantees no previous reader
    /// is live (`send_input` joins it; the first spawn has none).
    pub fn spawn_child(self: &Arc<Self>, message: Option<&str>) -> Result<(), String> {
        let session = self.session_id();
        let args = self.child_args(session.as_deref(), message);
        let mut command = Command::new(&self.recipe.command);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.recipe.cwd {
            command.current_dir(cwd);
        }
        for (k, v) in &self.recipe.env {
            command.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: a console child must not flash a console.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot spawn {} in machine mode: {e}", self.recipe.command))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Per-turn CLIs read their message from argv; their stdin is closed
        // at once so a CLI that also polls stdin sees EOF instead of hanging.
        let stdin = if self.mode.persistent { child.stdin.take() } else { None };
        let me = self.clone();
        let stderr_reader = std::thread::spawn(move || me.read_stderr(stderr));
        drop(stderr_reader);
        {
            let mut inner = self.lock();
            inner.child = Some(child);
            inner.stdin = stdin;
            inner.awaiting_input = false;
            inner.exit_reported = false;
        }
        let me = self.clone();
        let reader = std::thread::spawn(move || me.read_stdout(stdout));
        // The reader may already have finished (a child that printed
        // nothing and exited): storing its handle afterwards is fine — the
        // next send joins a finished thread at no cost.
        self.lock().reader = Some(reader);
        Ok(())
    }

    fn read_stdout(self: Arc<Self>, stdout: Option<std::process::ChildStdout>) {
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            for line in reader.split(b'\n') {
                let Ok(line) = line else { break };
                let text = String::from_utf8_lossy(&line);
                self.on_line(&text);
            }
        }
        // EOF: reap the child and report. Never skipped — a per-turn
        // continuation is only spawned after this reader was joined.
        let status = {
            let mut inner = self.lock();
            let status = inner.child.take().map(|mut c| c.wait());
            inner.stdin = None;
            match status {
                Some(Ok(s)) => ExitStatus {
                    code: s.code(),
                    success: s.success(),
                },
                _ => ExitStatus {
                    code: None,
                    success: false,
                },
            }
        };
        self.on_exit(status);
    }

    fn read_stderr(self: Arc<Self>, stderr: Option<std::process::ChildStderr>) {
        let Some(stderr) = stderr else { return };
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf).trim_end().to_owned();
                    if text.is_empty() {
                        continue;
                    }
                    self.handle.note_output(buf.len());
                    self.push(vec![(AgentEventKind::Raw, json!({"stderr": text}))]);
                }
            }
        }
    }

    /// One stdout line → events (the seam tests feed directly). The JSON is
    /// parsed BEFORE the agent lock is taken — a 6 KB `result` line must
    /// not stall a concurrent send or a snapshot read.
    pub fn on_line(&self, line: &str) {
        self.handle.note_output(line.len() + 1);
        let Some(parsed) = Adapter::parse_line(line) else { return };
        let mapped = self.lock().adapter.map_parsed(parsed);
        self.push(mapped);
    }

    /// A per-turn agent spawned with no prompt: nothing runs yet, it is
    /// waiting for its first message.
    pub fn mark_awaiting(&self) {
        self.push(vec![(AgentEventKind::AwaitingInput, json!({"after": "spawn"}))]);
    }

    /// The child exited (tests feed this directly too). Persistent: the
    /// agent is over — `awaiting_input` drops (a dead agent is never
    /// waiting) and the fronting actor learns of the exit. Per-turn: the
    /// turn is over, the agent waits for the next message.
    pub fn on_exit(&self, status: ExitStatus) {
        let (mapped, closed) = {
            let mut inner = self.lock();
            let mapped = inner.adapter.on_exit(status, self.mode.persistent);
            (mapped, inner.closed)
        };
        if closed {
            return; // a close is in flight: no awaiting_input on a killed agent
        }
        self.push_exit(mapped, self.mode.persistent);
        if self.mode.persistent {
            self.handle.note_child_exit(status);
        }
    }

    /// A per-turn continuation could not be spawned: the entry is retired
    /// (status/busy/probe all see `child_alive == false`), the reason is in
    /// the ring.
    fn retire(&self, err: &str) {
        self.push_exit(
            vec![(
                AgentEventKind::Error,
                json!({"message": err, "after": "spawn"}),
            )],
            true,
        );
        self.handle.note_child_exit(ExitStatus {
            code: None,
            success: false,
        });
    }

    /// Sequence, ring, flag, emit — the emit runs outside the lock.
    fn push(&self, mapped: Vec<Mapped>) {
        self.push_inner(mapped, false, false);
    }

    /// An exit batch: marks the exit reported (the receipt wait's
    /// short-circuit) and, when the agent is over, retires it.
    fn push_exit(&self, mapped: Vec<Mapped>, retire: bool) {
        self.push_inner(mapped, true, retire);
    }

    fn push_inner(&self, mapped: Vec<Mapped>, from_exit: bool, retire: bool) {
        if mapped.is_empty() && !from_exit {
            return;
        }
        let ts = (self.clock)();
        let mut events = Vec::with_capacity(mapped.len());
        {
            let mut inner = self.lock();
            for (kind, payload) in mapped {
                match kind {
                    AgentEventKind::AwaitingInput => inner.awaiting_input = true,
                    AgentEventKind::TurnStarted => {
                        inner.turns_started += 1;
                        inner.awaiting_input = false;
                    }
                    _ => {}
                }
                let event = AgentEvent {
                    id: self.id,
                    seq: inner.ring.next_seq(),
                    ts,
                    kind,
                    payload,
                };
                inner.ring.push(event.clone());
                events.push(event);
            }
            if from_exit {
                inner.exit_reported = true;
            }
            if retire {
                inner.exited = true;
                inner.awaiting_input = false;
            }
        }
        // `has_output == false` means "still booting": any ring event (even
        // a synthesized `awaiting_input` with no bytes behind it) ends that.
        if !events.is_empty() && !self.handle.io().has_output() {
            self.handle.note_output(1);
        }
        // Every batch wakes a parked receipt wait: a turn_started delivers,
        // an exit short-circuits.
        self.turn_signal.notify_all();
        for event in &events {
            (self.hooks.emit)(event);
        }
    }

    /// Write a user message and wait for the next `turn_started`.
    pub fn send_input(self: &Arc<Self>, text: &str, wait: Duration) -> TurnReceipt {
        let started = (self.clock)();
        let wait_ms = wait.as_millis() as u64;
        let cursor = |inner: &JsonInner| (inner.ring.last_seq(), inner.turns_started);
        let (mut seq_before, mut turns_before) = cursor(&self.lock());
        let refuse = |reason: String, seq_before: u64| TurnReceipt {
            delivered: false,
            reason: Some(reason),
            waited_ms: 0,
            wait_ms,
            seq_before,
            turn_started_seq: None,
        };
        if self.mode.persistent {
            let line = match self.lock().adapter.input_line(text) {
                Some(line) => line,
                None => return refuse("unsupported".into(), seq_before),
            };
            let written = {
                let mut inner = self.lock();
                if inner.closed || inner.exited {
                    return refuse("exited".into(), seq_before);
                }
                // From here on an init is a turn, not "ready".
                inner.adapter.ready_as_awaiting = false;
                match inner.stdin.as_mut() {
                    Some(stdin) => stdin
                        .write_all(format!("{line}\n").as_bytes())
                        .and_then(|_| stdin.flush())
                        .is_ok(),
                    None => false,
                }
            };
            if !written {
                return refuse("exited".into(), seq_before);
            }
        } else {
            // One per-turn send at a time: the busy check, the join of the
            // previous reader and the spawn are one critical section.
            let _turn = self.turn_lock.lock().unwrap_or_else(|e| e.into_inner());
            let previous = {
                let mut inner = self.lock();
                if inner.closed || inner.exited {
                    return refuse("exited".into(), seq_before);
                }
                if Self::child_running(&mut inner) {
                    return refuse("busy".into(), seq_before);
                }
                inner.reader.take()
            };
            // The previous child is dead but its reader may still be
            // draining buffered lines: JOIN it, so its turn_ended /
            // awaiting_input land (and the child is waited) BEFORE the
            // continuation's events — never skipped, never reordered.
            if let Some(reader) = previous {
                let _ = reader.join();
            }
            // The cursor is taken AFTER the join: the previous turn's tail
            // belongs to the previous turn, not to this receipt.
            (seq_before, turns_before) = cursor(&self.lock());
            if let Err(err) = self.spawn_child(Some(text)) {
                self.retire(&err);
                return refuse(format!("exited: {err}"), seq_before);
            }
        }
        // The send itself clears the waiting state — before the CLI
        // acknowledges, so a rail that reads the flag sees it drop at once.
        self.lock().awaiting_input = false;
        self.wait_for_turn(turns_before, seq_before, started, wait)
    }

    /// Park on the turn signal until `turns_started` moves past
    /// `turns_before`, the child dies first (reason `exited`, with the CLI's
    /// error when it pushed one), or the clock passes `started + wait`.
    fn wait_for_turn(&self, turns_before: u64, seq_before: u64, started: u64, wait: Duration) -> TurnReceipt {
        let wait_ms = wait.as_millis() as u64;
        let deadline = started + wait_ms;
        let mut inner = self.lock();
        loop {
            if inner.turns_started > turns_before {
                let turn_started_seq = inner
                    .ring
                    .since(seq_before)
                    .iter()
                    .find(|e| e.kind == AgentEventKind::TurnStarted)
                    .map(|e| e.seq);
                return TurnReceipt {
                    delivered: true,
                    reason: None,
                    waited_ms: (self.clock)().saturating_sub(started),
                    wait_ms,
                    seq_before,
                    turn_started_seq,
                };
            }
            let now = (self.clock)();
            let gone = inner.closed
                || inner.exited
                || inner.exit_reported
                || (self.mode.persistent && inner.stdin.is_none());
            if now >= deadline || gone {
                let reason = if gone {
                    // The child died before its turn started: name the
                    // error it pushed (a failed run's `error` event).
                    match inner
                        .ring
                        .since(seq_before)
                        .iter()
                        .find(|e| e.kind == AgentEventKind::Error)
                        .and_then(|e| e.payload.get("message"))
                        .and_then(Value::as_str)
                    {
                        Some(message) => format!("exited: {message}"),
                        None => "exited".to_owned(),
                    }
                } else {
                    "timeout".to_owned()
                };
                return TurnReceipt {
                    delivered: false,
                    reason: Some(reason),
                    waited_ms: now.saturating_sub(started),
                    wait_ms,
                    seq_before,
                    turn_started_seq: None,
                };
            }
            // Short slices so an injected clock is re-read; the real clock
            // simply wakes on the signal.
            let slice = Duration::from_millis((deadline - now).min(25));
            inner = self
                .turn_signal
                .wait_timeout(inner, slice)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The close is in flight: no more events, no more sends, no
    /// `awaiting_input` — but the child is NOT killed yet. For a docker-exec
    /// agent the registry kills + verifies the CONTAINER side first (with
    /// this host client still alive), then calls [`kill`](Self::kill).
    pub fn begin_close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        self.turn_signal.notify_all();
    }

    /// Kill the current child WITHOUT closing the agent — what a CLI crash
    /// looks like from here (the reader sees EOF, reaps it and reports the
    /// exit through the normal path). Test seam.
    pub fn kill_child(&self) {
        if let Some(child) = self.lock().child.as_mut() {
            let _ = child.kill();
        }
    }

    /// Close: no more events, kill the child (if any) and reap it.
    pub fn kill(&self) {
        let child = {
            let mut inner = self.lock();
            inner.closed = true;
            inner.stdin = None; // EOF first — a well-behaved CLI exits on it
            inner.child.take()
        };
        self.turn_signal.notify_all();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for JsonAgent {
    fn drop(&mut self) {
        self.kill();
    }
}


/// The `agent://event` wire object.
pub fn event_json(event: &AgentEvent) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("event".into(), json!("agent://event"));
    map.insert("term_id".into(), json!(event.id));
    map.insert("id".into(), json!(event.id));
    map.insert("seq".into(), json!(event.seq));
    map.insert("ts".into(), json!(event.ts));
    map.insert("kind".into(), json!(event.kind.as_str()));
    map.insert("payload".into(), event.payload.clone());
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use term_core::actor::{spawn_actor_nopty, ActorConfig};

    fn fixture(name: &str) -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A no-pty actor needs a real grid size (0×0 panics in the cursor clamp).
    fn test_cfg() -> ActorConfig {
        ActorConfig {
            spec: term_core::pty::PtySpec {
                cols: 80,
                rows: 24,
                ..Default::default()
            },
            scrollback_lines: 100,
            ..ActorConfig::default()
        }
    }

    fn kinds(mapped: &[Mapped]) -> Vec<&'static str> {
        mapped.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// The claude adapter over the REAL capture (claude 2.1.251, two turns:
    /// "ok" then "bye"; session, message and request ids, timestamps and
    /// cost fields replaced with fixture values): the kind sequence is the
    /// mapping table.
    #[test]
    fn claude_fixture_maps_to_the_documented_kinds() {
        let mut adapter = Adapter::new(AdapterKind::ClaudeStreamJson);
        let mut all: Vec<Mapped> = Vec::new();
        for line in fixture("claude_stream_json.jsonl") {
            all.extend(adapter.map_line(&line));
        }
        let ks = kinds(&all);
        // Turn 1: init → turn_started; thinking_tokens + rate_limit → raw;
        // assistant thinking → text{thinking}; assistant text → text;
        // result → usage, turn_ended, awaiting_input. Turn 2 repeats.
        let turn1 = &ks[..ks.iter().position(|k| *k == "awaiting_input").unwrap() + 1];
        assert_eq!(turn1[0], "turn_started");
        assert!(turn1.contains(&"raw"), "thinking_tokens / rate_limit pass through as raw: {turn1:?}");
        let texts: Vec<&Value> = all.iter().filter(|(k, _)| *k == AgentEventKind::Text).map(|(_, p)| p).collect();
        assert_eq!(texts.len(), 4, "two thinking + two text blocks: {texts:?}");
        assert_eq!(texts[0]["thinking"], true);
        assert_eq!(texts[1]["text"], "ok");
        assert_eq!(texts[3]["text"], "bye");
        assert_eq!(&turn1[turn1.len() - 3..], ["usage", "turn_ended", "awaiting_input"]);
        assert_eq!(ks.iter().filter(|k| **k == "turn_started").count(), 2);
        assert_eq!(ks.iter().filter(|k| **k == "awaiting_input").count(), 2);
        assert!(!ks.contains(&"error"));
        // Usage: context = input + cache read + cache creation, pct against
        // the 200k window from modelUsage.
        let usage = all.iter().find(|(k, _)| *k == AgentEventKind::Usage).map(|(_, p)| p).unwrap();
        assert_eq!(usage["context_tokens"], 10 + 6871);
        assert_eq!(usage["context_window"], 200_000);
        assert_eq!(usage["context_pct"], 3.4);
        assert_eq!(usage["output_tokens"], 64);
        let ended = all.iter().find(|(k, _)| *k == AgentEventKind::TurnEnded).map(|(_, p)| p).unwrap();
        assert_eq!(ended["result"], "ok");
        assert_eq!(ended["subtype"], "success");
        assert_eq!(adapter.session_id.as_deref(), Some("f1c70000-0000-4000-8000-000000000000"));
        assert!(!adapter.in_turn);
    }

    /// Hand-written claude lines for the shapes the probe run could not
    /// produce (no tools were allowed): tool_use/tool_result, compaction,
    /// an error result, a non-JSON line.
    #[test]
    fn claude_tool_calls_compaction_errors_and_garbage() {
        let mut a = Adapter::new(AdapterKind::ClaudeStreamJson);
        let call = a.map_line(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"session_id":"s1"}"#);
        assert_eq!(kinds(&call), ["turn_started", "tool_call"], "implicit turn start before the first block");
        assert_eq!(call[0].1["implicit"], true);
        assert_eq!(call[1].1["phase"], "call");
        assert_eq!(call[1].1["name"], "Bash");
        assert_eq!(call[1].1["input"]["command"], "ls");
        let result = a.map_line(r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a\nb","is_error":false}]},"session_id":"s1"}"#);
        assert_eq!(kinds(&result), ["tool_call"]);
        assert_eq!(result[0].1["phase"], "result");
        assert_eq!(result[0].1["tool_use_id"], "toolu_1");
        let compact = a.map_line(r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto","pre_tokens":150000},"session_id":"s1"}"#);
        assert_eq!(kinds(&compact), ["compaction"]);
        let err = a.map_line(r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":2,"duration_ms":5,"usage":{"input_tokens":1,"output_tokens":1},"session_id":"s1"}"#);
        assert_eq!(kinds(&err), ["usage", "error", "turn_ended", "awaiting_input"]);
        assert_eq!(err[1].1["subtype"], "error_max_turns");
        let garbage = a.map_line("not json at all");
        assert_eq!(kinds(&garbage), ["raw"]);
        assert_eq!(garbage[0].1["line"], "not json at all");
        assert!(a.map_line("   ").is_empty());
        // An unknown typed event keeps its whole object.
        let unknown = a.map_line(r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#);
        assert_eq!(kinds(&unknown), ["raw"]);
        assert_eq!(unknown[0].1["event"]["type"], "content_block_delta");
        // A user message that is not a tool result is raw, not dropped.
        let plain_user = a.map_line(r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#);
        assert_eq!(kinds(&plain_user), ["raw"]);
    }

    /// The opencode adapter over the REAL capture (`opencode run --format
    /// json`, one turn), plus hand-written tool/error lines.
    #[test]
    fn opencode_fixture_and_hand_written_lines() {
        let mut a = Adapter::new(AdapterKind::OpencodeJson);
        let mut all = Vec::new();
        for line in fixture("opencode_json.jsonl") {
            all.extend(a.map_line(&line));
        }
        assert_eq!(kinds(&all), ["turn_started", "text", "usage", "turn_ended"]);
        assert_eq!(all[1].1["text"], "ok");
        assert_eq!(all[2].1["input_tokens"], 26832);
        assert_eq!(all[2].1["context_tokens"], 26832);
        assert_eq!(all[2].1["total_cost_usd"], 0);
        assert_eq!(all[3].1["reason"], "stop");
        assert_eq!(a.session_id.as_deref(), Some("ses_fixture000000000000000001"));
        // Per-turn: the exit is what makes it awaiting_input (no error on 0).
        let exit = a.on_exit(ExitStatus { code: Some(0), success: true }, false);
        assert_eq!(kinds(&exit), ["awaiting_input"]);
        // A failed run: error first, still awaiting (the session is intact).
        let failed = a.on_exit(ExitStatus { code: Some(1), success: false }, false);
        assert_eq!(kinds(&failed), ["error", "awaiting_input"]);
        // Hand-written (documented shapes): tool_use running → call, completed → result.
        let running = a.map_line(r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","callID":"call_1","tool":"bash","state":{"status":"running","input":{"command":"ls"}}}}"#);
        assert_eq!(kinds(&running), ["turn_started", "tool_call"]);
        assert_eq!(running[1].1["phase"], "call");
        assert_eq!(running[1].1["name"], "bash");
        let done = a.map_line(r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","callID":"call_1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a b","title":"ls"}}}"#);
        assert_eq!(kinds(&done), ["tool_call"]);
        assert_eq!(done[0].1["phase"], "result");
        assert_eq!(done[0].1["content"], "a b");
        let err = a.map_line(r#"{"type":"error","sessionID":"ses_1","error":{"name":"ProviderError","data":{"message":"boom"}}}"#);
        assert_eq!(kinds(&err), ["error"]);
        assert_eq!(err[0].1["message"], "boom");
        let unknown = a.map_line(r#"{"type":"session_status","sessionID":"ses_1","status":"idle"}"#);
        assert_eq!(kinds(&unknown), ["raw"]);
        assert_eq!(unknown[0].1["status"], "idle");
        // Persistent-mode exit with an error code: turn_ended + error, no awaiting.
        let mut p = Adapter::new(AdapterKind::ClaudeStreamJson);
        p.map_line(r#"{"type":"system","subtype":"init","session_id":"x","model":"m"}"#);
        let died = p.on_exit(ExitStatus { code: Some(2), success: false }, true);
        assert_eq!(kinds(&died), ["turn_ended", "error"]);
        assert_eq!(died[0].1["reason"], "exit");
    }

    #[test]
    fn machine_modes_and_input_lines() {
        let claude = machine_mode(ToolType::Claude).unwrap();
        assert_eq!(claude.args, ["-p", "--output-format", "stream-json", "--input-format", "stream-json", "--verbose"]);
        assert!(claude.persistent);
        let oc = machine_mode(ToolType::Opencode).unwrap();
        assert_eq!(oc.args, ["run", "--format", "json"]);
        assert!(!oc.persistent);
        for t in [ToolType::Codex, ToolType::Gemini, ToolType::Custom, ToolType::Kimi] {
            assert!(machine_mode(t).is_none(), "{t:?}");
            assert!(!MACHINE_MODE_TYPES.contains(&t));
        }
        for t in MACHINE_MODE_TYPES {
            assert!(machine_mode(*t).is_some(), "MACHINE_MODE_TYPES is the one source: {t:?}");
        }
        let line = Adapter::new(AdapterKind::ClaudeStreamJson).input_line("hi \"there\"").unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "hi \"there\"");
        assert!(!line.contains('\n'));
        assert!(Adapter::new(AdapterKind::OpencodeJson).input_line("x").is_none());
        for k in [
            AgentEventKind::TurnStarted,
            AgentEventKind::ToolCall,
            AgentEventKind::AwaitingInput,
            AgentEventKind::Raw,
        ] {
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.as_str()));
        }
    }

    #[test]
    fn event_ring_is_bounded_and_keeps_counting() {
        let mut ring: EventRing<AgentEvent> = EventRing::new(1000);
        for i in 0..1250u64 {
            let seq = ring.next_seq();
            assert_eq!(seq, i + 1);
            ring.push(AgentEvent {
                id: 1,
                seq,
                ts: i,
                kind: AgentEventKind::Text,
                payload: json!({"i": i}),
            });
        }
        assert_eq!(ring.len(), 1000, "bounded at the cap");
        assert_eq!(ring.last_seq(), 1250);
        let all = ring.since(0);
        assert_eq!(all.first().unwrap().seq, 251, "oldest 250 evicted");
        assert_eq!(all.last().unwrap().seq, 1250);
        assert_eq!(ring.since(1248).len(), 2);
        assert!(ring.since(1250).is_empty());
        assert!(!ring.is_empty());
    }

    struct Harness {
        agent: Arc<JsonAgent>,
        handle: TermHandle,
        clock: Arc<AtomicU64>,
        emitted: Arc<Mutex<Vec<AgentEvent>>>,
    }

    /// A json agent with NO child: lines and exits are fed by hand, the
    /// clock is a counter the test moves.
    fn harness(mode: MachineMode) -> Harness {
        harness_with(
            mode,
            SpawnRecipe {
                command: "claude".into(),
                args: vec![],
                cwd: None,
                env: IndexMap::new(),
            },
        )
    }

    fn harness_with(mode: MachineMode, recipe: SpawnRecipe) -> Harness {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (handle, _sink) = spawn_actor_nopty(test_cfg(), tx);
        let clock = Arc::new(AtomicU64::new(1_000));
        let emitted: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
        let hooks = JsonHooks {
            emit: {
                let emitted = emitted.clone();
                Arc::new(move |e: &AgentEvent| emitted.lock().unwrap().push(e.clone()))
            },
        };
        let c = clock.clone();
        let agent = JsonAgent::with_clock(7, handle.clone(), recipe, mode, hooks, Arc::new(move || c.load(Ordering::Relaxed)));
        Harness {
            agent,
            handle,
            clock,
            emitted,
        }
    }

    fn kinds_of(h: &Harness) -> Vec<&'static str> {
        h.emitted.lock().unwrap().iter().map(|e| e.kind.as_str()).collect()
    }

    /// A fake CLI that prints `lines` to stdout, then exits with `code` (a
    /// batch file on Windows — cmd's /C quoting mangles inline JSON — a
    /// `sh` script elsewhere).
    fn fake_script(dir: &std::path::Path, name: &str, lines: &[&str], code: i32) -> SpawnRecipe {
        let (command, args) = if cfg!(windows) {
            let script = dir.join(format!("{name}.cmd"));
            let mut body = String::from("@echo off\r\n");
            for line in lines {
                body.push_str(&format!("echo {line}\r\n"));
            }
            body.push_str(&format!("exit /b {code}\r\n"));
            std::fs::write(&script, body).unwrap();
            ("cmd".to_owned(), vec!["/C".to_owned(), script.to_string_lossy().into_owned()])
        } else {
            let script = dir.join(format!("{name}.sh"));
            let mut body = String::from("#!/bin/sh\n");
            for line in lines {
                body.push_str(&format!("echo '{line}'\n"));
            }
            body.push_str(&format!("exit {code}\n"));
            std::fs::write(&script, body).unwrap();
            ("sh".to_owned(), vec![script.to_string_lossy().into_owned()])
        };
        SpawnRecipe {
            command,
            args,
            cwd: None,
            env: IndexMap::new(),
        }
    }

    /// awaiting_input → the typed flag reads true; the next send clears it
    /// BEFORE the CLI acknowledges; turn_started keeps it false. No mirror
    /// anywhere: the registry reads `awaiting_input()` at snapshot time.
    #[test]
    fn awaiting_input_sets_and_the_next_send_clears() {
        let h = harness(machine_mode(ToolType::Opencode).unwrap());
        h.agent.on_line(r#"{"type":"step_start","sessionID":"ses_1","part":{}}"#);
        h.agent.on_line(r#"{"type":"step_finish","sessionID":"ses_1","part":{"reason":"stop","tokens":{"input":1,"output":1}}}"#);
        assert!(!h.agent.awaiting_input());
        h.agent.on_exit(ExitStatus { code: Some(0), success: true });
        assert!(h.agent.awaiting_input());
        assert_eq!(kinds_of(&h), ["turn_started", "usage", "turn_ended", "awaiting_input"]);
        assert_eq!(h.agent.session_id().as_deref(), Some("ses_1"));
        assert!(h.handle.io().child_alive(), "a per-turn agent is alive between turns");
        // The continuation argv carries -s <session> and the message LAST.
        assert_eq!(h.agent.child_args(Some("ses_1"), Some("two words")), ["-s", "ses_1", "two words"]);
        // A persistent agent: feed awaiting, then a send with no stdin →
        // exited; the flag drop happens only on a successful write.
        let p = harness(machine_mode(ToolType::Claude).unwrap());
        p.agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#);
        p.agent.on_line(r#"{"type":"result","subtype":"success","is_error":false,"usage":{}}"#);
        assert!(p.agent.awaiting_input());
        let r = p.agent.send_input("hi", Duration::from_millis(10));
        assert_eq!(r.reason.as_deref(), Some("exited"));
        assert_eq!(r.wait_ms, 10, "the receipt carries the effective wait");
        assert!(!r.written());
        assert!(p.agent.awaiting_input(), "no write happened: still waiting");
        // turn_started after an awaiting clears the flag too (an MCP send
        // whose receipt raced).
        p.agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#);
        assert!(!p.agent.awaiting_input());
        // Waiting again, then the persistent child dies: a dead agent is
        // never waiting, and the fronting actor sees the exit.
        p.agent.on_line(r#"{"type":"result","subtype":"success","is_error":false,"usage":{}}"#);
        assert!(p.agent.awaiting_input());
        p.agent.on_exit(ExitStatus { code: Some(1), success: false });
        assert!(!p.agent.awaiting_input(), "review fix: exit clears awaiting_input");
        assert!(p.agent.exited());
        assert!(!p.handle.io().child_alive());
        assert_eq!(p.agent.send_input("late", Duration::from_millis(10)).reason.as_deref(), Some("exited"));
    }

    /// Review fix: a per-turn continuation that cannot spawn RETIRES the
    /// entry — `child_alive` drops, the reason is an `error` event, the
    /// agent is never "waiting" again and every later send is `exited`.
    #[test]
    fn per_turn_spawn_failure_retires_the_entry() {
        let h = harness_with(
            machine_mode(ToolType::Opencode).unwrap(),
            SpawnRecipe {
                command: "no-such-program-chappa".into(),
                args: vec![],
                cwd: None,
                env: IndexMap::new(),
            },
        );
        h.agent.mark_awaiting();
        assert!(h.agent.awaiting_input());
        assert!(h.handle.io().has_output(), "a ring event ends 'booting' (has_output)");
        assert!(h.handle.io().child_alive());
        let r = h.agent.send_input("go", Duration::from_millis(10));
        assert!(!r.delivered);
        assert!(r.reason.as_deref().unwrap().starts_with("exited: cannot spawn"), "{r:?}");
        assert!(!h.agent.awaiting_input(), "retired: not waiting");
        assert!(h.agent.exited());
        assert!(!h.handle.io().child_alive(), "the fronting actor saw the exit");
        let ks = kinds_of(&h);
        assert_eq!(ks, ["awaiting_input", "error"]);
        let err = h.emitted.lock().unwrap()[1].clone();
        assert!(err.payload["message"].as_str().unwrap().contains("cannot spawn"), "{err:?}");
        assert_eq!(h.agent.send_input("again", Duration::from_millis(10)).reason.as_deref(), Some("exited"));
    }

    /// Review fix: a per-turn send JOINS the previous child's reader
    /// before spawning the continuation. A reader still draining buffered
    /// lines (child already dead) gets to finish: its turn_ended /
    /// awaiting_input land BEFORE the continuation's turn_started, never
    /// skipped, and the exit is reported exactly once.
    #[test]
    fn per_turn_send_joins_the_previous_reader_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let recipe = fake_script(
            dir.path(),
            "turn2",
            &[r#"{"type":"step_start","sessionID":"ses_1","part":{"messageID":"m2"}}"#],
            0,
        );
        let h = harness_with(machine_mode(ToolType::Opencode).unwrap(), recipe);
        // Turn 1 "ran": the child is gone but its reader is still draining
        // — modelled as a thread that delivers the tail after a delay.
        let agent = h.agent.clone();
        let slow_reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            agent.on_line(r#"{"type":"step_start","sessionID":"ses_1","part":{"messageID":"m1"}}"#);
            agent.on_line(r#"{"type":"step_finish","sessionID":"ses_1","part":{"reason":"stop","tokens":{"input":1,"output":1}}}"#);
            agent.on_exit(ExitStatus { code: Some(0), success: true });
        });
        h.agent.lock().reader = Some(slow_reader);
        let started = std::time::Instant::now();
        let r = h.agent.send_input("second", Duration::from_secs(10));
        assert!(started.elapsed() >= Duration::from_millis(150), "the send waited for the reader");
        assert!(r.delivered, "{r:?}");
        // The receipt answers under the lock; the emit hook runs after it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while kinds_of(&h).len() < 5 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ks = kinds_of(&h);
        assert!(
            ks.starts_with(&["turn_started", "usage", "turn_ended", "awaiting_input", "turn_started"]),
            "turn 1's tail lands before turn 2 starts: {ks:?}"
        );
        assert_eq!(r.turn_started_seq, Some(5));
        // The continuation carried the session captured from turn 1.
        assert_eq!(h.agent.session_id().as_deref(), Some("ses_1"));
        // Turn 2's child exits → its own awaiting_input, reported once.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !h.agent.awaiting_input() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.agent.awaiting_input());
        assert_eq!(kinds_of(&h).iter().filter(|k| **k == "awaiting_input").count(), 2);
        assert!(h.handle.io().child_alive(), "alive between turns");
    }

    /// Review fix: a per-turn child that exits WITHOUT a turn_started
    /// answers `exited: <the pushed error>` at once — not after the
    /// 20 s receipt timeout (the clock here is frozen: without the
    /// short-circuit the wait would never end).
    #[test]
    fn per_turn_child_death_short_circuits_the_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let recipe = fake_script(
            dir.path(),
            "dies",
            &[r#"{"type":"error","sessionID":"ses_1","error":{"name":"ProviderError","data":{"message":"boom"}}}"#],
            1,
        );
        let h = harness_with(machine_mode(ToolType::Opencode).unwrap(), recipe);
        let started = std::time::Instant::now();
        let r = h.agent.send_input("go", Duration::from_secs(20));
        assert!(started.elapsed() < Duration::from_secs(15), "short-circuited: {r:?}");
        assert!(!r.delivered);
        assert_eq!(r.reason.as_deref(), Some("exited: boom"), "{r:?}");
        assert_eq!(r.wait_ms, 20_000);
        let ks = kinds_of(&h);
        assert_eq!(ks, ["error", "error", "awaiting_input"], "the CLI's error, the exit error, then waiting: {ks:?}");
        // A failed RUN keeps the session: the agent waits for the next
        // message (per-turn semantics) and is still alive.
        assert!(h.agent.awaiting_input());
        assert!(h.handle.io().child_alive());
    }

    /// Review fix: a persistent CLI spawned WITHOUT a prompt — its first
    /// ready signal (claude `system/init`) is `awaiting_input`, not a turn;
    /// `has_output` flips with it (no longer "booting"). The first send
    /// turns the next init back into a turn_started.
    #[test]
    fn persistent_ready_signal_without_a_prompt_is_awaiting_input() {
        let h = harness(machine_mode(ToolType::Claude).unwrap());
        assert!(!h.handle.io().has_output());
        h.agent.expect_ready_as_awaiting();
        h.agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s1","model":"m"}"#);
        assert_eq!(kinds_of(&h), ["awaiting_input"]);
        assert_eq!(h.emitted.lock().unwrap()[0].payload["after"], "init");
        assert!(h.agent.awaiting_input());
        assert!(h.handle.io().has_output());
        assert_eq!(h.agent.session_id().as_deref(), Some("s1"));
        // Attach a stdin so the send writes, then the CLI's init is a turn.
        let (cmd, args): (&str, &[&str]) = if cfg!(windows) { ("cmd", &["/C", "more"]) } else { ("cat", &[]) };
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        {
            let mut inner = h.agent.lock();
            inner.stdin = Some(stdin);
            inner.child = Some(child);
        }
        let agent = h.agent.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s1","model":"m"}"#);
        });
        let r = h.agent.send_input("first", Duration::from_secs(5));
        feeder.join().unwrap();
        assert!(r.delivered, "{r:?}");
        assert_eq!(kinds_of(&h), ["awaiting_input", "turn_started"]);
        assert!(!h.agent.awaiting_input());
        h.agent.kill();
    }

    /// Review cleanup: the session id comes ONLY from the CLI's own
    /// top-level session line — a nested (subagent) claude init or a later
    /// opencode step with another sessionID never overwrites it.
    #[test]
    fn session_id_only_from_the_top_level_session_line() {
        let mut a = Adapter::new(AdapterKind::ClaudeStreamJson);
        a.map_line(r#"{"type":"system","subtype":"init","session_id":"top","model":"m"}"#);
        assert_eq!(a.session_id.as_deref(), Some("top"));
        let nested = a.map_line(r#"{"type":"system","subtype":"init","session_id":"sub","model":"m","parent_tool_use_id":"toolu_9"}"#);
        assert_eq!(kinds(&nested), ["raw"], "a subagent's init is not our turn");
        assert_eq!(a.session_id.as_deref(), Some("top"));
        a.map_line(r#"{"type":"assistant","session_id":"other","message":{"content":[{"type":"text","text":"x"}]}}"#);
        a.map_line(r#"{"type":"result","subtype":"success","session_id":"other","usage":{}}"#);
        assert_eq!(a.session_id.as_deref(), Some("top"), "only system/init carries the session");
        let mut o = Adapter::new(AdapterKind::OpencodeJson);
        o.map_line(r#"{"type":"text","sessionID":"early","part":{"text":"hi"}}"#);
        assert_eq!(o.session_id, None, "only step_start announces the session");
        o.map_line(r#"{"type":"step_start","sessionID":"ses_main","part":{}}"#);
        o.map_line(r#"{"type":"step_start","sessionID":"ses_sub","part":{}}"#);
        assert_eq!(o.session_id.as_deref(), Some("ses_main"));
    }

    /// The receipt is the NEXT turn_started: a fake clock that never moves
    /// keeps the wait open until the line arrives; a clock past the deadline
    /// answers `timeout` with the message still written.
    #[test]
    fn send_receipt_is_the_next_turn_started_with_a_fake_clock() {
        let h = harness(machine_mode(ToolType::Claude).unwrap());
        // Attach a stdin the agent can write to: a pipe whose read end we
        // drain on a thread (any Write works; ChildStdin is just the type).
        // Use a real child that sleeps: `cmd /C more` / `cat` reads stdin.
        let (cmd, args): (&str, &[&str]) = if cfg!(windows) { ("cmd", &["/C", "more"]) } else { ("cat", &[]) };
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        {
            let mut inner = h.agent.lock();
            inner.stdin = Some(stdin);
            inner.child = Some(child);
        }
        h.agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#);
        h.agent.on_line(r#"{"type":"result","subtype":"success","is_error":false,"usage":{}}"#);
        let seq_at_send = h.agent.last_seq();
        // Frozen clock: the send blocks until the turn_started line lands.
        let agent = h.agent.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            agent.on_line(r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":1}"#);
            agent.on_line(r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#);
        });
        let receipt = h.agent.send_input("second", Duration::from_millis(5_000));
        feeder.join().unwrap();
        assert!(receipt.delivered, "{receipt:?}");
        assert_eq!(receipt.seq_before, seq_at_send);
        assert_eq!(receipt.turn_started_seq, Some(seq_at_send + 2), "the raw line before it does not count");
        assert_eq!(receipt.waited_ms, 0, "frozen clock");
        assert!(!h.agent.awaiting_input());
        // Clock jumps past the deadline, no turn_started → timeout (the
        // write still happened: reason names the ack, not the delivery).
        h.clock.fetch_add(10_000, Ordering::Relaxed);
        let before = h.agent.last_seq();
        let agent = h.agent.clone();
        let c = h.clock.clone();
        let mover = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c.fetch_add(60_000, Ordering::Relaxed);
            let _ = agent;
        });
        let receipt = h.agent.send_input("third", Duration::from_millis(5_000));
        mover.join().unwrap();
        assert!(!receipt.delivered);
        assert_eq!(receipt.reason.as_deref(), Some("timeout"));
        assert_eq!(receipt.seq_before, before);
        assert!(receipt.waited_ms >= 5_000, "{receipt:?}");
        h.agent.kill();
        let receipt = h.agent.send_input("fourth", Duration::from_millis(10));
        assert_eq!(receipt.reason.as_deref(), Some("exited"));
    }

    /// End to end over a real piped child: a fake "CLI" (python or sh
    /// printing fixture lines) → reader thread → ring → emit → exit
    /// reported to the no-pty actor (child_alive drops, status exits).
    #[test]
    fn real_piped_child_feeds_the_ring_and_reports_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (handle, _sink) = spawn_actor_nopty(test_cfg(), tx);
        let emitted: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
        let hooks = JsonHooks {
            emit: {
                let emitted = emitted.clone();
                Arc::new(move |e: &AgentEvent| emitted.lock().unwrap().push(e.clone()))
            },
        };
        let line1 = r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#;
        let line2 = r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":1},"result":"ok"}"#;
        // A fake CLI as a script file (cmd's /C quote rules mangle inline
        // JSON; a batch file echoes it verbatim).
        let dir = tempfile::tempdir().unwrap();
        let (command, args) = if cfg!(windows) {
            let script = dir.path().join("fake_cli.cmd");
            std::fs::write(&script, format!("@echo off\r\necho {line1}\r\necho {line2}\r\necho oops 1>&2\r\n")).unwrap();
            ("cmd".to_owned(), vec!["/C".to_owned(), script.to_string_lossy().into_owned()])
        } else {
            ("sh".to_owned(), vec!["-c".to_owned(), format!("echo '{line1}'; echo '{line2}'; echo oops 1>&2")])
        };
        let recipe = SpawnRecipe {
            command,
            args,
            cwd: None,
            env: IndexMap::new(),
        };
        let agent = JsonAgent::new(3, handle.clone(), recipe, machine_mode(ToolType::Claude).unwrap(), hooks);
        agent.spawn_child(None).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while handle.io().child_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!handle.io().child_alive(), "exit reported to the actor");
        assert!(handle.io().has_output(), "bytes were noted, never parsed");
        let ks: Vec<&str> = emitted.lock().unwrap().iter().map(|e| e.kind.as_str()).collect();
        assert!(ks.starts_with(&["turn_started", "usage", "turn_ended", "awaiting_input"]), "{ks:?}");
        let stderr = emitted
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.kind == AgentEventKind::Raw && e.payload.get("stderr").is_some())
            .map(|e| e.payload["stderr"].clone());
        assert_eq!(stderr, Some(json!("oops")), "stderr lines pass through as raw");
        assert_eq!(agent.events_since(0).len(), ks.len());
        // The actor reported the exit through the normal event path.
        let mut saw_exit = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
            if matches!(ev, term_core::actor::TermEvent::Exited(s) if s.success) {
                saw_exit = true;
                break;
            }
        }
        assert!(saw_exit, "TermEvent::Exited from the no-pty actor");
        drop(agent);
    }
}
