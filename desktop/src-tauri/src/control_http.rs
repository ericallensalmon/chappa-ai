//! Control surface: the debug subset graduated to ALL
//! builds on `127.0.0.1:8324`, bearer-token authenticated, plus the
//! orchestrator additions (receipts, input journal, byte-stream
//! liveness, unsplit argv, whole-screen output, raw-bytes door). The
//! `chappa-ai-mcp` stdio server (desktop/chappa-ai-mcp) is its main client;
//! it doubles as the curl-from-Claude-Code interface. Debug-only routes
//! (raw write, resize, events, modes, key) stay on 8323, tokenless, in
//! `debug_http.rs` — untouched, so the parity harness keeps working.
//!
//! ## Auth
//! A random 32-byte token is generated at first launch into
//! `<app-config>/control_token` (0600 on unix; on Windows inheritance is
//! stripped and only the current user is granted access via `icacls`).
//! EVERY request must carry `Authorization: Bearer <token>` — `GET /` too.
//!
//! ## Single-source route table
//! [`ROUTES`] is the ONE definition of the surface: the dispatcher walks it,
//! `GET /` serializes it, and [`render_api_doc`] emits `desktop/CONTROL_API.md`
//! from it (a test keeps the checked-in doc in sync). Adding a route = adding
//! a table row + handler; nothing else to update.
//!
//! tiny_http with a small FIXED pool of request threads (see [`WORKERS`]) and
//! a hard cap on request bodies ([`MAX_BODY_BYTES`]): the clients are local
//! agents, not the internet, but a bounded server cannot be made to spawn
//! threads or allocate without limit by whatever else is on localhost. The
//! JSON/body/query plumbing here is shared with `debug_http` (which imports
//! [`json`], [`read_body`] and [`query_u64`]) — one implementation, two
//! servers.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::AppHandle;
use term_core::pty::PtySpec;

use crate::agent_tools::AgentToolsState;
use crate::agents::{agent_events, json_send_input, spawn_agent_headless, SpawnAgentRequest, SpawnContext};
use crate::commands::default_shell;
use crate::coordination::Coordination;
use crate::coordination_http as coord;
use crate::projects::{ControlError, Projects};
use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, CreatedEvent, NullSink, Registry, TermId,
};
use crate::settings::SettingsState;
use crate::timers::TimerService;
use crate::timers_http as timers;

pub const BIND: &str = "127.0.0.1:8324";
pub const TOKEN_FILE: &str = "control_token";
/// The request header carrying the writing actor's process id.
pub const ACTOR_HEADER: &str = "X-Chappa-Actor";

/// Default/maximum time a `send_input`/`send_bytes` receipt watches the byte
/// counter for activity after the write.
const RECEIPT_WAIT_DEFAULT_MS: u64 = 250;
const RECEIPT_WAIT_MAX_MS: u64 = 5_000;
/// Default/maximum time a requested project-process start waits for the
/// frontend's spawn to register.
const START_WAIT_DEFAULT_MS: u64 = 3_000;
const START_WAIT_MAX_MS: u64 = 15_000;

/// Number of request threads. tiny_http hands each incoming request to
/// whichever worker calls `recv` first, so the pool is the concurrency limit:
/// a burst of connections queues in the accept backlog instead of spawning a
/// thread apiece. Four is generous for a surface whose only clients are one
/// stdio MCP server and the odd curl — but note a handler that WAITS
/// (`/input`'s receipt up to 5s, a project start up to 15s) holds its worker,
/// so four concurrent waits are the ceiling before requests queue.
const WORKERS: usize = 4;

/// Everything a handler can reach. `Projects` needs an initialized store (and
/// an `AppHandle` for start requests); a default one answers 500 with a
/// clear message, which is what tests without a Tauri runtime see.
#[derive(Clone, Default)]
pub struct ControlState {
    pub registry: Registry,
    pub projects: Projects,
    pub settings: SettingsState,
    /// Tauri's app-config dir, reported by `GET /` so an integrator can tell
    /// WHICH chappa-ai instance answered (and where its token lives). Empty in
    /// tests/harnesses with no Tauri runtime.
    pub config_dir: PathBuf,
    /// The agent-tool registry behind `list_agent_tools` /
    /// `spawn_agent`.
    pub agent_tools: AgentToolsState,
    /// Scratchpads + todos over `<app-config>/coordination.db`.
    pub coordination: Coordination,
    /// The timer scheduler (same database, own tables). The default
    /// is INERT — no threads, no store — so tests and the parity harness
    /// construct a `ControlState` without one and the timer routes answer a
    /// clear 500 instead of pretending.
    pub timers: TimerService,
    /// The app handle the control surface broadcasts `term://created`
    /// on, so the webview can adopt a backend-spawned terminal into the rail.
    /// `None` in tests/harness with no Tauri runtime (a silent no-op).
    pub app: Option<AppHandle>,
}

pub type Resp = tiny_http::Response<std::io::Cursor<Vec<u8>>>;
type Handler = fn(&ControlState, &Params, &str, &str) -> Resp;

/// One row of the single-source table.
pub struct Route {
    pub method: &'static str,
    /// Path pattern; `{id}` = terminal id (u32), `{project}` = project id
    /// (u32), `{name}` = percent-encoded process name, `{rid}` = a
    /// coordination record id (scratchpad / todo / comment, i64).
    pub path: &'static str,
    /// Request fields: JSON body keys for POST, query keys for GET. `?` marks
    /// optional.
    pub fields: &'static [&'static str],
    pub description: &'static str,
    handler: Handler,
}

pub const ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: "/",
        fields: &[],
        description: "This route table as JSON (discoverability), plus `instance` {pid, config_dir} — which running chappa-ai answered, so two instances (or a stale one holding the port) are diagnosable.",
        handler: route_table,
    },
    Route {
        method: "GET",
        path: "/processes",
        fields: &[],
        description: "Every live terminal/process: id, name, status, exit_code, cols, rows, seq, plus byte-stream liveness (output_bytes, last_output_at, has_output, child_alive), `close_on_exit` (the spawn flag — the row reaps itself when its child exits) and, for agents, `agent.parent_process_id` (the process that spawned it, null for a root). Stats fields will ride along here.",
        handler: list_processes,
    },
    Route {
        method: "POST",
        path: "/processes",
        fields: &["command?", "args?", "cwd?", "name?", "cols?", "rows?", "env?"],
        description: "Spawn a terminal. `command` + `args` reach the OS verbatim (never re-split); no command = the default shell; `env` = {NAME: value} applied over the inherited environment. cols/rows 1..=4096. Returns {id, name}.",
        handler: spawn_terminal,
    },
    Route {
        method: "GET",
        path: "/processes/{id}",
        fields: &[],
        description: "One process row (same shape as the listing). has_output=false means still booting; child_alive=false means the child has exited.",
        handler: get_process,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/output",
        fields: &["lines?", "offset?"],
        description: "Terminal text. Default = the WHOLE current screen. `lines`=N adds the last N scrollback lines above it; `offset`=O (with lines) returns a history-only window skipping the O most recent scrollback lines. No silent cap.",
        handler: get_output,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/input_journal",
        fields: &[],
        description: "The last 100 send_input/send_bytes deliveries: [{ts, bytes_len, text, submit}], oldest first.",
        handler: get_input_journal,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/input",
        fields: &["text", "submit?", "wait_ms?"],
        description: "Write text to the pty in ONE atomic write (submit appends \\r to the same write). Returns a receipt {written, seq_before, seq_after, had_output_within_ms, wait_ms} off the byte counter. For a json-transport agent the text is delivered as ONE user message (claude: a stream-json user line on stdin; opencode: a `run -s <session>` continuation child) and the receipt is {written, transport: \"json\", delivered, reason?, waited_ms, seq_before, turn_started_seq?, wait_ms}: delivered=true means the CLI's next turn_started arrived within wait_ms (default 5000, max 20000); reason=timeout means written but not yet acknowledged, exited = no child to write to (or it died before its turn started: `exited: <why>`), busy = the previous opencode turn is still running. `wait_ms` is the effective wait. A json agent takes whole messages: `submit: false` or empty text is a 400, and a refused send (busy/exited/unsupported) is not journaled.",
        handler: send_input,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/agent_events",
        fields: &["since?"],
        description: "A json-transport agent's typed events with seq > since (default 0), from a per-agent ring of the last 1000: [{id, seq, ts, kind, payload}], kind = turn_started | turn_ended | tool_call | text | usage | awaiting_input | compaction | error | raw. `raw` carries the unclassified native object (or {line} / {stderr}) so nothing is lost. 409 for a tty agent or a plain terminal.",
        handler: get_agent_events,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/bytes",
        fields: &["bytes", "wait_ms?"],
        description: "Raw bytes ([u8]) to the pty, same receipt and journal treatment as /input. The escape hatch for control sequences.",
        handler: send_bytes,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/close",
        fields: &["confirm"],
        description: "Close a terminal (kills the child). Requires confirm=true. For a docker-exec agent the container-side process is killed and verified FIRST (TERM, poll, KILL), then the host client; the answer carries verification = gone | still-present | container-down | unresolved (null for plain terminals).",
        handler: close_terminal,
    },
    Route {
        method: "GET",
        path: "/agent_tools",
        fields: &[],
        description: "Registered agent tools (the shape {id, name, command, tool_type, enabled} plus model, runtime {kind: host | docker_exec, container?, user?, workdir?, tty?, max_busy_in_container?}, container (the tool's container name, or null), program, args, env, max_busy). `command` is DERIVED for display; program/args are the truth.",
        handler: list_agent_tools,
    },
    Route {
        method: "POST",
        path: "/agents",
        fields: &["agent_tool_id", "project_id?", "name?", "extra_args?", "prompt?", "force?", "cols?", "rows?", "parent_process_id?", "close_on_exit?"],
        description: "Spawn an agent from a registered tool. extra_args are appended to the tool's args VERBATIM (an element with spaces stays one argv entry). Env = inherited < tool.env < identity (CHAPPA_AI_PROCESS_ID/PROJECT_ID/AGENT_TOOL_ID); docker-exec runtimes pass every env entry as -e and run the program under a pid-recording sh wrapper. `prompt` queues until the pty is ready (has_output and agent_ready_quiet_ms of silence, default 750 ms — or agent_ready_max_wait_ms after the first byte for a TUI that never goes quiet, reason=ready-by-timeout), then ONE atomic write of text+\\r; prompt_receipt reports delivered=false with reason=exited when the child dies first. Busy guards: over max_busy / max_busy_in_container the spawn is refused naming the busy processes; force=true overrides. Returns {process_id, term_id, name, agent_instructions, prompt_receipt?, container? (status, started_at, uptime_s, mem_bytes, mem_limit_bytes), agent, forced?}. The row NESTS under its parent in the rail — `parent_process_id` if given, else the `X-Chappa-Actor` header parsed as a process id (chappa-ai-mcp sends its own CHAPPA_AI_PROCESS_ID, so an agent spawning an agent nests automatically); the candidate must name a live process or the spawn records null (a root) — a hint for the rail, never authorization. `close_on_exit=true` closes the row through the ordinary close path once the child exits (after term://exited and the exit notification; non-zero exits close too) and broadcasts `term://closed {term_id, reason: \"close_on_exit\"}`; every other close broadcasts `reason: \"closed\"`. A bash-hosted tool never exits by itself: end its command with `; exit`.",
        handler: spawn_agent,
    },
    Route {
        method: "POST",
        path: "/agents/reap",
        fields: &[],
        description: "Sweep every ENABLED docker_exec tool's distinct (container, user) pair for container processes carrying CHAPPA_AI_SPAWN_UUID that no LIVE registry entry owns, and reap each orphan root through the shared TERM/poll/KILL/verify sequence (as the tool's user). Returns one entry per orphan handled: {container, user, pid, uuid, verdict: gone | still-present | container-down | unresolved}. NOT gated by the agentReapOrphans setting (that gates only the automatic startup/probe sweeps) - this is the on-demand path. A container that is down or docker unreachable is skipped silently. Does not touch processes without the marker, and never consults docker top.",
        handler: reap_orphans,
    },
    Route {
        method: "GET",
        path: "/projects",
        fields: &[],
        description: "Stored projects: [{id, name, path, icon, open, processes}]; processes are populated only for open projects.",
        handler: list_projects,
    },
    Route {
        method: "GET",
        path: "/projects/{project}",
        fields: &[],
        description: "One project with its process rows (status, term_id, exit_code, auto_start, …).",
        handler: get_project,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/start",
        fields: &["wait_ms?"],
        description: "Start a project process (idempotent on a live one). The app's frontend performs the spawn; waits up to wait_ms (default 3000) for it to register, else answers pending=true.",
        handler: start_process,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/stop",
        fields: &[],
        description: "Stop a project process. Non-running = no-op success (never an error).",
        handler: stop_process,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/restart",
        fields: &["wait_ms?"],
        description: "Restart = stop (no-op if not running) then start.",
        handler: restart_process,
    },
    // ---- scratchpads ----
    Route {
        method: "GET",
        path: "/scratchpads",
        fields: &["project_id?", "include_global?", "query?", "tags?", "include_archived?", "offset?", "limit?"],
        description: "List scratchpads (no content): {scratchpads: [{scratchpad_id, project_id, name, revision, tags, archived, updated_at, updated_by, line_count, matched_fields?, snippet?}], total}. `project_id` scopes to one project (`global` = pads with no project; absent = every pad); include_global=true adds the global pads to a project scope; `query` matches name + content case-insensitively and adds matched_fields + a snippet; `tags` (comma list) = any-of; archived pads are hidden unless include_archived=true.",
        handler: coord::scratchpad_list,
    },
    Route {
        method: "POST",
        path: "/scratchpads",
        fields: &["name", "content", "tags?", "project_id?", "response_mode?"],
        description: "Create a scratchpad (scratchpad_write without scratchpad_id). A leading `# Title` line in the content overrides `name`. project_id null/absent = a global pad. Answers a slim receipt {scratchpad_id, project_id, revision, name}; response_mode=rich answers the full row.",
        handler: coord::scratchpad_create,
    },
    Route {
        method: "GET",
        path: "/scratchpads/tags",
        fields: &["project_id?"],
        description: "Distinct scratchpad tags: {tags: [..]} (project_id scopes; absent = every pad).",
        handler: coord::scratchpad_tags_list,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}",
        fields: &["mode?", "section_heading?", "offset?", "limit?"],
        description: "Read a scratchpad with its revision metadata. mode = full (default; `offset`/`limit` window the lines) | outline (heading outline only, no content) | section (one markdown section — `section_heading` required, exact text after trimming, first duplicate wins). The `content`/`headings`/`lines` spellings are accepted.",
        handler: coord::scratchpad_read,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}",
        fields: &["name", "content", "expected_revision", "tags?", "response_mode?"],
        description: "Overwrite a scratchpad (scratchpad_write with scratchpad_id). expected_revision is REQUIRED; a mismatch is 409 {error: revision_conflict, current_revision, current_content}. A leading `# Title` overrides name; omitted tags are kept.",
        handler: coord::scratchpad_overwrite,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}/find",
        fields: &["query", "case_sensitive?", "limit?", "context_lines?", "scope?"],
        description: "Literal substring search in one pad: {matches: [{line (1-based), text, before, after}], total_matches, truncated}. limit default 20 (1..=100), context_lines default 1 (0..=3), scope = all | headings | content.",
        handler: coord::scratchpad_find,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}/tail",
        fields: &["lines?"],
        description: "The last `lines` lines (default 10) with revision metadata.",
        handler: coord::scratchpad_tail,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/rename",
        fields: &["name", "expected_revision", "response_mode?"],
        description: "Rename at expected_revision (required) without touching the content.",
        handler: coord::scratchpad_rename,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/append",
        fields: &["content", "expected_revision?", "response_mode?"],
        description: "Append at the end (a newline separates it from existing content). expected_revision is optional — appends cannot clobber, last-writer-wins is fine; the revision still advances.",
        handler: coord::scratchpad_append,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/append_section",
        fields: &["heading", "content", "expected_revision?", "response_mode?"],
        description: "Append at the end of the section under an EXISTING heading (before the next heading of the same or higher level). A missing heading is 400 and changes nothing. expected_revision optional.",
        handler: coord::scratchpad_append_section,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/edit",
        fields: &["target", "content", "expected_revision", "response_mode?"],
        description: "Replace one section or line range at expected_revision (required). target = {heading} (content starting with a heading replaces the whole section; otherwise the heading is kept and its body replaced) | {start_line, end_line} (1-based inclusive, bounds-checked). {type: section, section_heading} / {type: line_range, offset, limit} are accepted too. Never renames: the leading-H1 title override applies on create and full overwrite only, so an explicit rename survives edits.",
        handler: coord::scratchpad_edit,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/tags/add",
        fields: &["tags", "expected_revision", "response_mode?"],
        description: "Add tags in one revision bump at expected_revision (required).",
        handler: coord::scratchpad_add_tags,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/tags/remove",
        fields: &["tags", "expected_revision", "response_mode?"],
        description: "Remove tags in one revision bump at expected_revision (required).",
        handler: coord::scratchpad_remove_tags,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/clear",
        fields: &["expected_revision", "confirm", "response_mode?"],
        description: "Empty the content at expected_revision (required). Requires confirm=true.",
        handler: coord::scratchpad_clear,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/delete",
        fields: &["expected_revision", "confirm"],
        description: "Delete the pad at expected_revision (required). Requires confirm=true. Answers {scratchpad_id, project_id, deleted: true}.",
        handler: coord::scratchpad_delete,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/archive",
        fields: &["archived?", "response_mode?"],
        description: "Hide from the default list without deleting (archived=false un-hides). No revision guard.",
        handler: coord::scratchpad_archive,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/transfer",
        fields: &["target_project_id", "expected_revision?", "response_mode?"],
        description: "Move the pad to another project (null = global). Only the project changes.",
        handler: coord::scratchpad_transfer,
    },
    // ---- todos ----
    Route {
        method: "GET",
        path: "/todos",
        fields: &["project_id?", "status?", "completed?", "is_blocked?", "priority?", "query?", "tags?", "sort?", "offset?", "limit?"],
        description: "Todo summaries (no body): {todos: [..], total}. Filters: status (open | in_progress | done), completed, is_blocked (derived: any blocker not completed), priority (low | normal | high | urgent), query (title + body + comments), tags (any-of). sort = updated_desc (default) | updated_asc | created_desc | created_asc | priority_desc | priority_asc | title. limit default 50, max 200.",
        handler: coord::todo_list,
    },
    Route {
        method: "POST",
        path: "/todos",
        fields: &["project_id", "title", "body?", "priority?", "tags?", "response_mode?"],
        description: "Create a todo in a project (project_id required — todos are project-scoped). Slim receipt {todo_id, project_id, revision, title}; response_mode=rich answers the full row.",
        handler: coord::todo_create,
    },
    Route {
        method: "GET",
        path: "/todos/tags",
        fields: &["project_id?"],
        description: "Distinct todo tags: {tags: [..]}.",
        handler: coord::todo_tags_list,
    },
    Route {
        method: "GET",
        path: "/todos/{rid}",
        fields: &["include_comments?"],
        description: "One todo: {todo_id, project_id, title, body, priority, status, completed, tags, revision, locked_by, lock_expires_at, is_blocked, blocker_ids, comment_count, created_at, updated_at, updated_by} (+ comments with include_comments=true). An expired lock reads as no lock.",
        handler: coord::todo_get,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}",
        fields: &["title?", "body?", "priority?", "status?", "tags?", "expected_revision?", "response_mode?"],
        description: "Update a subset of fields; omitted fields are preserved. status=done sets completed. A foreign live lock is 409 {error: locked, locked_by, lock_expires_at}; a stale expected_revision is 409 revision_conflict. Slim receipt = {todo_id, project_id, revision} + the changed fields.",
        handler: coord::todo_update,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/tags/add",
        fields: &["tag", "response_mode?"],
        description: "Add one tag (lock-guarded).",
        handler: coord::todo_add_tag,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/tags/remove",
        fields: &["tag", "response_mode?"],
        description: "Remove one tag (lock-guarded).",
        handler: coord::todo_remove_tag,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/set",
        fields: &["blocker_ids", "response_mode?"],
        description: "Replace the blocker list. Blockers must be in the same project; a cycle (including self) is 409 {error: cycle}. Answers blocker_ids + the derived is_blocked.",
        handler: coord::todo_set_blockers,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/add",
        fields: &["blocker_id", "response_mode?"],
        description: "Add one blocker (cycle-checked).",
        handler: coord::todo_add_blocker,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/remove",
        fields: &["blocker_id", "response_mode?"],
        description: "Remove one blocker.",
        handler: coord::todo_remove_blocker,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/complete",
        fields: &["completed", "release_lock?", "response_mode?"],
        description: "Mark complete (status=done) or incomplete (status back to open). Releases the caller's OWN lock unless release_lock=false. affected_todo_ids = todos blocked by this one.",
        handler: coord::todo_complete,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/lock",
        fields: &["lease_ms?", "lease_ttl_seconds?", "response_mode?"],
        description: "Take or renew an edit lease keyed by ACTOR id (default 300 s, max 24 h; lease_ttl_seconds saturates). A foreign live lock is 409 locked; expired leases are ignored. Advisory coordination only: the actor is the unauthenticated X-Chappa-Actor header, so a lock is a courtesy between cooperating agents, not access control.",
        handler: coord::todo_lock,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/unlock",
        fields: &["response_mode?"],
        description: "Release a lock you hold (none/expired = no-op success; a foreign live lock is 409).",
        handler: coord::todo_unlock,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/transfer",
        fields: &["target_project_id", "response_mode?"],
        description: "Move a todo to another project: comments kept, blockers (both directions) and lock cleared.",
        handler: coord::todo_transfer,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/delete",
        fields: &["confirm"],
        description: "Delete a todo with its comments and blocker edges. Requires confirm=true. Answers {todo_id, project_id, deleted, affected_todo_ids}.",
        handler: coord::todo_delete,
    },
    Route {
        method: "GET",
        path: "/todos/{rid}/comments",
        fields: &["offset?", "limit?"],
        description: "Comments on a todo, oldest first: {comments: [{comment_id, todo_id, actor, body, created_at, updated_at}], total}.",
        handler: coord::todo_comment_list,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/comments",
        fields: &["body", "response_mode?"],
        description: "Add a comment (attributed to the actor). Slim receipt {comment_id, todo_id}.",
        handler: coord::todo_comment_create,
    },
    Route {
        method: "POST",
        path: "/todo_comments/{rid}",
        fields: &["body", "response_mode?"],
        description: "Edit a comment's body.",
        handler: coord::todo_comment_update,
    },
    Route {
        method: "POST",
        path: "/todo_comments/{rid}/delete",
        fields: &["confirm"],
        description: "Delete a comment. Requires confirm=true.",
        handler: coord::todo_comment_delete,
    },
    // ---- timers ----
    Route {
        method: "POST",
        path: "/timers",
        fields: &["delay_ms", "body", "delivery_process_id?", "loop?", "repeat_every_ms?", "name?", "project_id?"],
        description: "timer_set: fire `body` into a process after delay_ms. `loop: true` repeats every delay_ms; `repeat_every_ms` overrides the interval. delivery_process_id defaults to the CALLER's own process (the X-Chappa-Actor header, which chappa-ai-mcp fills from CHAPPA_AI_PROCESS_ID) — a timer delivers to exactly one process, which must be LIVE now (404 otherwise) and is pinned by its uuid. project_id defaults to the actor's own project. The body is injected verbatim as a fresh user turn through the delivery path (quiet-only policy).",
        handler: timers::timer_set,
    },
    Route {
        method: "POST",
        path: "/timers/idle_any",
        fields: &["processes", "max_wait_ms", "body", "delivery_process_id?", "idle_ms?", "confirm_ms?", "rearm?", "name?", "project_id?"],
        description: "timer_fire_when_idle_any: fire when ANY watched process goes idle, or at max_wait_ms (reason=deadline). `processes` accepts ids or names — a name resolves within project_id (default: the actor's project) first, and one that matches in several projects with no scope is a 400 naming the candidates. Idle = child_alive and now - last_output_at >= idle_ms (default idle_threshold_ms, 120000) off the BYTE STREAM, never render diffs. Processes already idle at schedule time are IGNORED until they become busy again (`any` waits for a NEW transition) and reported as already_idle. A met condition enters `confirming` and is re-checked after confirm_ms; bytes in that window re-arm it (rearm defaults true). Answers {timer_id, status: scheduled, already_idle, waiting_on, note, deadline_at}.",
        handler: timers::timer_fire_when_idle_any,
    },
    Route {
        method: "POST",
        path: "/timers/idle_all",
        fields: &["processes", "max_wait_ms", "body", "delivery_process_id?", "idle_ms?", "confirm_ms?", "rearm?", "name?", "project_id?"],
        description: "timer_fire_when_idle_all: same, but every watched process must be idle. Already-idle processes COUNT AS SATISFIED; when all of them are, the answer is {status: \"already_satisfied\", timer_id: null} and NO timer is created.",
        handler: timers::timer_fire_when_idle_all,
    },
    Route {
        method: "GET",
        path: "/timers",
        fields: &["include_fired?", "all?", "limit?", "offset?", "project_id?"],
        description: "timer_list: {timers: [{id, name, kind, status: pending | confirming | paused | fired | cancelled | expired, delivery: {firing_id, process_id, status: in_flight | delivered | failed | coalesced, delivered: true | false | null, receipt, reason: condition | deadline | missed | expired, error, coalesced_with, at}, fired_count, next_fire_at, deadline_at, waiting_on, …}], total, limit, offset}. Pending first, newest first. Pending only by default; include_fired=true adds fired/cancelled/expired within timer_retention_hours. Owner-scoped (the actor that created them); all=true lists every actor's, for the orchestrator seat. project_id defaults to the actor's project.",
        handler: timers::timer_list,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/cancel",
        fields: &[],
        description: "timer_cancel: stop a pending/confirming timer (owner-scoped; the `user` actor may act on any timer). Answers {timer_id, project_id, cancelled, status}.",
        handler: timers::timer_cancel,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/pause",
        fields: &[],
        description: "timer_pause: STOPS THE CLOCK — the remaining delay (or remaining deadline) is captured, so a paused timer never comes due while paused.",
        handler: timers::timer_pause,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/resume",
        fields: &[],
        description: "timer_resume: continue from the captured remaining delay.",
        handler: timers::timer_resume,
    },
];

// ---- token ------------------------------------------------------------------

/// Read the token, creating it on first launch with owner-only permissions.
/// Never regenerates an existing file (clients cache nothing, but an agent
/// mid-session must not be locked out by an app restart).
pub fn load_or_create_token(config_dir: &Path) -> std::io::Result<String> {
    let path = config_dir.join(TOKEN_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    std::fs::create_dir_all(config_dir)?;
    let token = random_token()?;
    write_owner_only(&path, &token)?;
    Ok(token)
}

fn random_token() -> std::io::Result<String> {
    random_hex(32)
}

/// `n` bytes from the OS RNG as lowercase hex. The one random-id source in
/// the app (the bearer token, the spawn uuid): an RNG failure is an
/// `Err`, never a weaker fallback.
pub fn random_hex(n: usize) -> std::io::Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn write_owner_only(path: &Path, token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // Mode set at CREATE time: the file is never world-readable, not even
    // for the instant between create and chmod.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows: the token is written to a sibling temp file, its ACL restricted
/// THERE, and only then renamed into place — so `control_token` never exists
/// with the directory's inherited ACL, not even for an instant, and a failed
/// `icacls` leaves nothing behind for the next launch to trust (review: the
/// old create-then-restrict order persisted an unrestricted file when icacls
/// failed, and `load_or_create_token` reused it forever).
#[cfg(windows)]
fn write_owner_only(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(token.as_bytes())?;
        drop(file);
        restrict_acl_windows(&tmp)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Strip inherited ACEs and grant only the current user. `icacls` ships with
/// every Windows; shelling out keeps the crate windows-sys-free.
#[cfg(windows)]
fn restrict_acl_windows(path: &Path) -> std::io::Result<()> {
    let user = std::env::var("USERNAME")
        .map_err(|_| std::io::Error::other("USERNAME not set; cannot restrict the token ACL"))?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:F"))
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn write_owner_only(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, token)
}

/// The token file path for a config dir (what the MCP crate reads).
pub fn token_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TOKEN_FILE)
}

// ---- server -----------------------------------------------------------------

/// App entry point: load/create the token and serve on [`BIND`] from its own
/// thread. A bind failure is loud but not fatal (the app still runs; only
/// the control surface is missing).
pub fn start(mut state: ControlState, config_dir: PathBuf) {
    state.config_dir = config_dir.clone();
    std::thread::spawn(move || {
        let token = match load_or_create_token(&config_dir) {
            Ok(token) => token,
            Err(err) => {
                log::error!("[control_http] cannot create {TOKEN_FILE}: {err}");
                eprintln!("[control_http] cannot create {TOKEN_FILE}: {err}");
                return;
            }
        };
        let server = match tiny_http::Server::http(BIND) {
            Ok(server) => server,
            Err(err) => {
                // LOUD (review): the overwhelmingly likely cause is a SECOND
                // chappa-ai already holding 8324 — in which case every agent
                // tool call lands in the OTHER instance, which looks like the
                // app ignoring commands rather than a bind failure. `GET /`
                // reports pid + config dir so the winner can be identified.
                log::error!(
                    "[control_http] FAILED TO BIND {BIND}: {err} — the control surface \
                     (and every chappa-ai-mcp agent tool) is DEAD in this instance. \
                     Another chappa-ai is probably already running and holding the port; \
                     `GET /` on {BIND} names the pid and config dir of whichever one did bind."
                );
                eprintln!("[control_http] failed to bind {BIND}: {err}");
                return;
            }
        };
        eprintln!("[control_http] control surface on http://{BIND}");
        serve(server, state, token);
    });
}

/// Serve forever on an already-bound server (tests bind an ephemeral port),
/// from a fixed pool of [`WORKERS`] threads — this thread is one of them, so
/// `serve` still blocks for the life of the server.
pub fn serve(server: tiny_http::Server, state: ControlState, token: String) {
    let server = Arc::new(server);
    let token: Arc<str> = token.into();
    let mut pool = Vec::with_capacity(WORKERS - 1);
    for _ in 1..WORKERS {
        let (server, state, token) = (server.clone(), state.clone(), token.clone());
        pool.push(std::thread::spawn(move || {
            accept_loop(&server, &state, &token)
        }));
    }
    accept_loop(&server, &state, &token);
    for worker in pool {
        let _ = worker.join();
    }
}

/// One worker: take the next request off the shared server and answer it.
/// Ends when the server is dropped (`recv` errors).
fn accept_loop(server: &tiny_http::Server, state: &ControlState, token: &str) {
    while let Ok(request) = server.recv() {
        respond(state, token, request);
    }
}

fn respond(state: &ControlState, token: &str, mut request: tiny_http::Request) {
    let method = request.method().as_str().to_owned();
    let url = request.url().to_owned();
    let authorized = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Authorization"))
        .any(|h| bearer_matches(h.value.as_str(), token));
    // Who is writing. chappa-ai-mcp sends its own
    // CHAPPA_AI_PROCESS_ID here; a bare curl (or an agent outside
    // chappa-ai) is attributed to "user".
    let actor = request
        .headers()
        .iter()
        .find(|h| h.field.equiv(ACTOR_HEADER))
        .map(|h| h.value.as_str().trim().to_owned())
        .filter(|a| !a.is_empty());
    let response = if authorized {
        let body = read_body(&mut request.as_reader());
        guarded(|| dispatch_as(state, actor.as_deref(), &method, &url, &body))
    } else {
        json(401, json!({"error": "unauthorized: send `Authorization: Bearer <token>` (token file: <app-config>/control_token)"}))
    };
    let _ = request.respond(response);
}

/// Run one handler under `catch_unwind`: a panic inside a route answers a
/// clean 500 and the worker thread survives to take the next request
/// (without this one bad request would silently shrink the pool).
fn guarded(f: impl FnOnce() -> Resp) -> Resp {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(resp) => resp,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".into());
            log::error!("[control_http] handler panicked: {what}");
            json(500, json!({"error": "internal", "message": format!("handler panicked: {what}")}))
        }
    }
}

/// Constant-time-ish compare of the header value against the token.
fn bearer_matches(header: &str, token: &str) -> bool {
    let presented = match header.strip_prefix("Bearer ") {
        Some(p) => p.trim(),
        None => return false,
    };
    if presented.len() != token.len() {
        return false;
    }
    presented
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Captured path placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    pub id: Option<TermId>,
    pub project: Option<u32>,
    pub name: Option<String>,
    /// `{rid}`: a coordination record id.
    pub record: Option<i64>,
    /// The writing actor: `X-Chappa-Actor` or `"user"`.
    pub actor: String,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            id: None,
            project: None,
            name: None,
            record: None,
            actor: crate::coordination::USER_ACTOR.to_owned(),
        }
    }
}

/// Match a concrete path against a pattern, capturing placeholders.
fn match_route(pattern: &str, path: &str) -> Option<Params> {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let got: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if pat.len() != got.len() {
        return None;
    }
    let mut params = Params::default();
    for (p, g) in pat.iter().zip(got.iter()) {
        match *p {
            "{id}" => params.id = Some(g.parse().ok()?),
            "{project}" => params.project = Some(g.parse().ok()?),
            "{name}" => params.name = Some(percent_decode(g)),
            "{rid}" => params.record = Some(g.parse().ok()?),
            literal if literal == *g => {}
            _ => return None,
        }
    }
    Some(params)
}

/// Walk [`ROUTES`] — the dispatcher IS the table, so `GET /` can never
/// drift from what is actually served.
pub fn dispatch(state: &ControlState, method: &str, url: &str, body: &str) -> Resp {
    dispatch_as(state, None, method, url, body)
}

/// [`dispatch`] with an explicit actor (`None` = `"user"`).
pub fn dispatch_as(state: &ControlState, actor: Option<&str>, method: &str, url: &str, body: &str) -> Resp {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut path_matched = false;
    for route in ROUTES {
        if let Some(mut params) = match_route(route.path, path) {
            path_matched = true;
            if route.method == method {
                if let Some(actor) = actor {
                    params.actor = actor.to_owned();
                }
                return (route.handler)(state, &params, query, body);
            }
        }
    }
    if path_matched {
        json(405, json!({"error": "method not allowed"}))
    } else {
        json(404, json!({"error": "unknown route (GET / lists the surface)"}))
    }
}

// ---- handlers ---------------------------------------------------------------

fn route_table(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    json(
        200,
        json!({
            "service": "chappa-ai control surface",
            "bind": BIND,
            "auth": "Authorization: Bearer <contents of <app-config>/control_token>",
            // WHICH instance answered. 8324 is a single fixed port: when two
            // chappa-ai builds are open, one loses the bind and the other
            // silently serves every agent tool call. pid + config dir make
            // that diagnosable from the client side.
            "instance": {
                "pid": std::process::id(),
                "config_dir": state.config_dir.display().to_string(),
            },
            "routes": route_table_json(),
        }),
    )
}

/// The table rows as JSON (shared by `GET /` and its test).
pub fn route_table_json() -> Vec<Value> {
    ROUTES
        .iter()
        .map(|r| {
            json!({
                "method": r.method,
                "path": r.path,
                "fields": r.fields,
                "description": r.description,
            })
        })
        .collect()
}

fn list_processes(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    json(200, json!(state.registry.list_control()))
}

#[derive(Debug, Deserialize, Default)]
struct SpawnBody {
    #[serde(default)]
    command: String,
    /// Verbatim argv: never re-split on whitespace.
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    name: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    /// Extra environment, applied over the inherited one (document order —
    /// serde_json deserializes straight into the IndexMap here). Review: the
    /// body silently dropped it before, so an agent could not opt a claude
    /// session out of the alt screen.
    #[serde(default)]
    env: indexmap::IndexMap<String, String>,
}

/// Terminal sizes a spawn accepts: a zero row/col count builds a grid the
/// actor cannot address (review), and anything past this is a typo.
const MAX_SPAWN_DIM: u16 = 4096;

fn spawn_terminal(state: &ControlState, _: &Params, _: &str, body: &str) -> Resp {
    let spec: SpawnBody = if body.trim().is_empty() {
        SpawnBody::default()
    } else {
        match serde_json::from_str(body) {
            Ok(spec) => spec,
            Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
        }
    };
    let settings = state.settings.snapshot();
    let cols = spec.cols.unwrap_or(80);
    let rows = spec.rows.unwrap_or(24);
    if !(1..=MAX_SPAWN_DIM).contains(&cols) || !(1..=MAX_SPAWN_DIM).contains(&rows) {
        return json(
            400,
            json!({"error": format!("cols and rows must be 1..={MAX_SPAWN_DIM} (got {cols}x{rows})")}),
        );
    }
    let command = if spec.command.is_empty() {
        default_shell()
    } else {
        spec.command
    };
    let name = spec.name.unwrap_or_else(|| command.clone());
    let cfg = actor_config(
        PtySpec {
            command,
            args: spec.args,
            cwd: spec.cwd.map(Into::into),
            env: spec.env.into_iter().collect(),
            cols,
            rows,
        },
        None,
        Some(&settings),
    );
    // No webview channel behind an agent-spawned terminal: agents read text
    // back through /output (same as the parity harness). The row still
    // shows up in /processes and list_terminals. It also emits
    // `term://created` so an app with an OPEN webview can adopt it into the
    // rail (the NullSink invisibility fix).
    match state
        .registry
        .create_terminal(cfg, Arc::new(NullSink), None, name.clone())
    {
        Ok(id) => {
            if let Some(app) = &state.app {
                emit_created(
                    &AppCreatedBroadcast(app.clone()),
                    CreatedEvent {
                        term_id: id,
                        name: name.clone(),
                        kind: "terminal",
                        project_id: None,
                        agent_tool_id: None,
                        parent_process_id: None,
                    },
                );
            }
            json(200, json!({"id": id, "name": name}))
        }
        Err(err) => json(500, json!({"error": format!("spawn failed: {err}")})),
    }
}

fn get_process(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    match state.registry.control_snapshot(id) {
        Some(row) => json(200, json!(row)),
        None => no_such_terminal(id),
    }
}

/// `key=<u64>` out of a raw query string (`None` when absent or unparsable).
/// Shared with `debug_http`.
pub fn query_u64(query: &str, key: &str) -> Option<u64> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
}

fn get_output(state: &ControlState, params: &Params, query: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let (handle, screen_rows) = match (state.registry.handle(id), state.registry.control_snapshot(id)) {
        (Some(handle), Some(row)) => (handle, row.base.rows as usize),
        _ => return no_such_terminal(id),
    };
    // Bounded (review): the sum fed to dump_text must not overflow, and the
    // actor casts it to i64. No SILENT cap — a request past the bound is a
    // 400, not a truncated answer. The scrollback itself is 10k lines.
    const MAX_WINDOW: u64 = 1 << 20;
    let lines = query_u64(query, "lines").unwrap_or(0);
    let offset = query_u64(query, "offset").unwrap_or(0);
    if lines > MAX_WINDOW || offset > MAX_WINDOW {
        return json(400, json!({"error": format!("lines and offset must be <= {MAX_WINDOW}")}));
    }
    let (lines, offset) = (lines as usize, offset as usize);
    // dump_text(n) = last n history lines + the whole screen, oldest first.
    // offset=0: exactly that. offset>0: a history-only window — drop the
    // screen and the `offset` most recent history lines, keep `lines`.
    let dump = handle.dump_text(lines + offset);
    let rows: Vec<String> = if offset == 0 {
        dump
    } else {
        let history_len = dump.len().saturating_sub(screen_rows);
        let end = history_len.saturating_sub(offset);
        let start = end.saturating_sub(lines);
        dump[start..end].to_vec()
    };
    json(
        200,
        json!({"rows": rows, "screen_rows": screen_rows, "lines": lines, "offset": offset}),
    )
}

fn get_input_journal(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    match state.registry.input_journal(id) {
        Some(records) => json(200, json!({"records": records})),
        None => no_such_terminal(id),
    }
}

#[derive(Debug, Deserialize)]
struct InputBody {
    text: String,
    /// pty: append Enter to the same write (default false). json agents
    /// take whole messages: an explicit `false` is refused (400).
    submit: Option<bool>,
    wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BytesBody {
    bytes: Vec<u8>,
    wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ConfirmBody {
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, Default)]
struct WaitBody {
    wait_ms: Option<u64>,
}

/// The ONE payload a submit produces: text + `\r`, written together
/// (split sends are flaky).
pub fn submit_payload(text: &str, submit: bool) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    if submit {
        bytes.push(b'\r');
    }
    bytes
}

fn send_input(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let body: InputBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    // A json-transport agent takes a user MESSAGE, not keystrokes
    // — `submit` is implied (an explicit false, or nothing to say, is a
    // caller error), the receipt is the next turn_started.
    if state.registry.json_agent(id).is_some() {
        if body.submit == Some(false) {
            return json(
                400,
                json!({"error": format!("process {id} is a json-transport agent: it takes whole messages, submit:false is not a thing here")}),
            );
        }
        if body.text.trim().is_empty() {
            return json(400, json!({"error": "text is empty: a json-transport agent takes whole messages"}));
        }
        return match json_send_input(&state.registry, id, &body.text, body.wait_ms) {
            Ok(receipt) => {
                let mut value = serde_json::to_value(&receipt).unwrap_or_default();
                value["written"] = json!(receipt.written());
                value["transport"] = json!("json");
                json(200, value)
            }
            Err(err) => json(409, json!({"error": err})),
        };
    }
    let submit = body.submit.unwrap_or(false);
    let payload = submit_payload(&body.text, submit);
    deliver(state, id, &payload, body.text, submit, body.wait_ms)
}

/// `GET /processes/{id}/agent_events?since=N`.
fn get_agent_events(state: &ControlState, params: &Params, query: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    if state.registry.handle(id).is_none() {
        return no_such_terminal(id);
    }
    let since = query_u64(query, "since").unwrap_or(0);
    match agent_events(&state.registry, id, since) {
        Some(events) => json(200, json!({"id": id, "since": since, "events": events})),
        None => json(409, json!({"error": format!("process {id} is not a json-transport agent")})),
    }
}

fn send_bytes(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let body: BytesBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    if state.registry.json_agent(id).is_some() {
        return json(409, json!({"error": format!("process {id} is a json-transport agent: it takes messages via /input, not raw bytes")}));
    }
    let text = String::from_utf8_lossy(&body.bytes).into_owned();
    deliver(state, id, &body.bytes, text, false, body.wait_ms)
}

/// Write + journal + receipt. `seq_before`/`seq_after` are the actor's byte
/// counter around the write; the receipt watches it for `wait_ms` so the
/// caller can SEE whether the write produced terminal activity.
///
/// The watch is ONE parked `wait_for_output` on the actor's condvar (
/// cleanup — it used to sleep-poll the counter every 5ms), so the reply lands
/// the moment the first byte comes back rather than up to 5ms later, and a
/// quiet process costs the waiting thread no CPU at all.
///
/// Known limitation (documented in the result): on an ALREADY
/// streaming process the counter moves for reasons unrelated to this write,
/// so `had_output_within_ms` can be a false positive. The seq counter is the
/// primitive, and `seq_before`/`seq_after` let the caller judge.
fn deliver(
    state: &ControlState,
    id: TermId,
    payload: &[u8],
    text: String,
    submit: bool,
    wait_ms: Option<u64>,
) -> Resp {
    let wait_ms = wait_ms
        .unwrap_or(RECEIPT_WAIT_DEFAULT_MS)
        .min(RECEIPT_WAIT_MAX_MS);
    let handle = match state.registry.handle(id) {
        Some(handle) => handle,
        None => return no_such_terminal(id),
    };
    let seq_before = handle.io().output_bytes();
    if !state.registry.write_journaled(id, payload, text, submit) {
        return no_such_terminal(id);
    }
    let seq_after = handle
        .io()
        .wait_for_output(seq_before, Duration::from_millis(wait_ms));
    json(
        200,
        json!({
            "written": true,
            "bytes_len": payload.len(),
            "seq_before": seq_before,
            "seq_after": seq_after,
            "had_output_within_ms": seq_after != seq_before,
            "wait_ms": wait_ms,
        }),
    )
}

fn close_terminal(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let body: ConfirmBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    if !body.confirm {
        return json(400, json!({"error": "close requires confirm=true"}));
    }
    // A terminal a project process owns is stopped through its lifecycle
    // (status → stopped, term_id cleared, auto_restart cancelled) — a bare
    // registry close would leave the runtime pointing at a dead id (review).
    // A store that is not initialized (parity harness, tests) owns nothing.
    if let Ok(true) = state.projects.stop_owner_of_terminal(&state.registry, id) {
        return json(200, json!({"ok": true, "id": id, "stopped_process": true}));
    }
    match state.registry.close(id) {
        Some(outcome) => json(
            200,
            json!({"ok": true, "id": id, "verification": outcome.verification.map(|v| v.as_str())}),
        ),
        None => no_such_terminal(id),
    }
}

/// `list_agent_tools`: the row shape plus the structured fields.
pub fn agent_tool_rows(state: &ControlState) -> Vec<Value> {
    state
        .agent_tools
        .list()
        .iter()
        .map(|t| {
            let mut row = serde_json::to_value(t).unwrap_or_default();
            row["command"] = json!(t.command_line());
            row["container"] = json!(t.runtime.container());
            row
        })
        .collect()
}

fn list_agent_tools(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    json(200, json!(agent_tool_rows(state)))
}

fn spawn_agent(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let req: SpawnAgentRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    // The SPAWNING actor — `X-Chappa-Actor` parsed as a numeric
    // process id. A non-numeric / `user` actor → `None` (a root spawn). This
    // is the default parent candidate the spawn validates against a live
    // entry; the rail nests the child under it.
    let actor = params.actor.parse::<TermId>().ok();
    let ctx = SpawnContext {
        registry: &state.registry,
        tools: &state.agent_tools,
        settings: &state.settings,
        projects: &state.projects,
        actor,
    };
    match spawn_agent_headless(ctx, req) {
        Ok(resp) => {
            // spawn_agent_with's own emit
            // is gated on ITS on_notify, which this headless route
            // deliberately leaves None — so MCP/control-spawned agents (the
            // exact invisibility the adopt path exists to fix) never broadcast.
            // Emit here off the control server's app handle, mirroring
            // spawn_terminal above. Found live: an orchestrating agent's
            // first post-47 spawn still drew no rail row.
            if let Some(app) = &state.app {
                emit_created(
                    &AppCreatedBroadcast(app.clone()),
                    CreatedEvent {
                        term_id: resp.process_id,
                        name: resp.name.clone(),
                        kind: "agent",
                        project_id: resp.agent.project_id,
                        agent_tool_id: Some(resp.agent.tool_id),
                        parent_process_id: resp.agent.parent_process_id,
                    },
                );
            }
            json(200, json!(resp))
        }
        Err(err) if err.starts_with("no such") => json(404, json!({"error": err})),
        Err(err) if err.starts_with("refused") || err.contains("disabled") => {
            json(409, json!({"error": err}))
        }
        Err(err) => json(500, json!({"error": format!("spawn failed: {err}")})),
    }
}

/// `POST /agents/reap`: one full orphan sweep over every ENABLED
/// docker_exec tool's distinct (container, user) pair. Returns the per-orphan
/// handling — {container, user, pid, uuid, verdict} — so an orchestrator (and
/// a human) can see exactly what was found and what happened to each.
/// Not gated by the `agentReapOrphans` setting.
fn reap_orphans(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    let results = crate::agents::reap_orphans(&state.registry, &state.agent_tools, None, None);
    json(200, json!({ "results": results }))
}

fn list_projects(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    project_response(state.projects.control_list())
}

fn get_project(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let id = params.project.unwrap_or_default();
    project_response(state.projects.control_get(id))
}

fn parse_wait(body: &str) -> Result<u64, Resp> {
    let wait: WaitBody = if body.trim().is_empty() {
        WaitBody::default()
    } else {
        serde_json::from_str(body)
            .map_err(|err| json(400, json!({"error": format!("bad body: {err}")})))?
    };
    Ok(wait
        .wait_ms
        .unwrap_or(START_WAIT_DEFAULT_MS)
        .min(START_WAIT_MAX_MS))
}

/// The ONE mapping from a project-layer failure to an HTTP status. The
/// project layer returns a typed [`ControlError`], so this is a match on
/// variants — it used to sniff substrings of the message ("not open",
/// "no process"), which silently reclassified any error whose wording drifted.
fn project_response<T: serde::Serialize>(result: Result<T, ControlError>) -> Resp {
    match result {
        Ok(value) => json(200, json!(value)),
        // Unknown project/process AND "the project is not open" are both 404s:
        // from the caller's side the addressed thing is not there to act on.
        Err(err @ (ControlError::NotFound(_) | ControlError::NotOpen(_))) => {
            json(404, json!({"error": err.to_string()}))
        }
        Err(err) => json(500, json!({"error": err.to_string()})),
    }
}

fn lifecycle_response(result: Result<crate::projects::ControlLifecycleDto, ControlError>) -> Resp {
    project_response(result)
}

fn start_process(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let wait_ms = match parse_wait(body) {
        Ok(w) => w,
        Err(resp) => return resp,
    };
    let (project, name) = (params.project.unwrap_or_default(), params.name.clone().unwrap_or_default());
    lifecycle_response(state.projects.control_start(project, &name, wait_ms))
}

fn stop_process(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let (project, name) = (params.project.unwrap_or_default(), params.name.clone().unwrap_or_default());
    lifecycle_response(state.projects.control_stop(&state.registry, project, &name))
}

fn restart_process(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let wait_ms = match parse_wait(body) {
        Ok(w) => w,
        Err(resp) => return resp,
    };
    let (project, name) = (params.project.unwrap_or_default(), params.name.clone().unwrap_or_default());
    lifecycle_response(
        state
            .projects
            .control_restart(&state.registry, project, &name, wait_ms),
    )
}

// ---- helpers ----------------------------------------------------------------

fn no_such_terminal(id: TermId) -> Resp {
    json(404, json!({"error": format!("no such terminal: {id}")}))
}

/// Minimal percent-decoding for the `{name}` segment (process names carry
/// spaces and `+`; `+` is NOT a space here — clients encode spaces as %20).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read the whole request body, bounded at [`MAX_BODY_BYTES`]. Bodies here
/// are small JSON payloads; the cap stops a bad Content-Length (or a client
/// that never stops writing) from growing this buffer without limit. A body
/// over the cap is simply truncated, which then fails to parse as JSON — a
/// clean 400 rather than an OOM. Shared with `debug_http`.
pub fn read_body(reader: &mut dyn std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = reader.take(MAX_BODY_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Upper bound on a request body (1 MiB). The largest legitimate payload is a
/// `send_bytes` array or a paste-sized `send_input`, orders of magnitude below.
pub const MAX_BODY_BYTES: u64 = 1 << 20;

/// A JSON response with an explicit status code and content-type header.
/// Shared with `debug_http`.
pub fn json(status: u16, value: Value) -> Resp {
    let mut response = tiny_http::Response::from_data(serde_json::to_vec(&value).unwrap_or_default())
        .with_status_code(status);
    response.add_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .unwrap_or_else(|_| unreachable!("literal header bytes")),
    );
    response
}

// ---- doc emitter -------------------------------------------------------------

/// `desktop/CONTROL_API.md`, rendered from [`ROUTES`]. The test
/// `control_api_doc_is_in_sync` fails when the checked-in file differs;
/// regenerate with `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop
/// control_api_doc`.
pub fn render_api_doc() -> String {
    let mut out = String::new();
    out.push_str("# chappa-ai control API\n\n");
    out.push_str("GENERATED from `desktop/src-tauri/src/control_http.rs::ROUTES` — do not edit by hand.\n");
    out.push_str("Regenerate: `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop control_api_doc`.\n\n");
    out.push_str(&format!(
        "Bound to `http://{BIND}` in every build. Every request must carry\n\
         `Authorization: Bearer <token>` where the token is the contents of\n\
         `<app-config>/control_token` (created at first launch, owner-only permissions;\n\
         `<app-config>` is Tauri's app config dir for `com.chappa-ai.desktop`, e.g.\n\
         `%APPDATA%\\com.chappa-ai.desktop` on Windows). Responses are JSON; errors are\n\
         `{{\"error\": \"…\"}}` with a 4xx/5xx status. `GET /` returns this same table.\n\n\
         Path placeholders: `{{id}}` = terminal id, `{{project}}` = project id, `{{name}}` =\n\
         percent-encoded process name, `{{rid}}` = scratchpad / todo / comment id.\n\
         Fields marked `?` are optional; POST fields are JSON body keys, GET fields are\n\
         query parameters (comma-separated for list fields).\n\n\
         This is the MCP server's transport (`chappa-ai-mcp`) and a curl interface —\n\
         not a public API. Localhost only, no CLI binary.\n\n"
    ));
    out.push_str("| Method | Path | Fields | Description |\n|---|---|---|---|\n");
    for r in ROUTES {
        let fields = if r.fields.is_empty() {
            "—".to_owned()
        } else {
            r.fields
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} |\n",
            r.method,
            r.path,
            fields,
            r.description.replace('|', "\\|")
        ));
    }
    out.push_str("\n## Receipts\n\n");
    out.push_str(
        "`/input` and `/bytes` answer `{written, bytes_len, seq_before, seq_after, had_output_within_ms, wait_ms}`.\n\
         `seq_*` is the actor's pty byte counter (byte flow, not render) read before the write and\n\
         after watching it for `wait_ms` (default 250, max 5000): `had_output_within_ms=true` means the\n\
         write produced terminal activity. Every delivery is appended to the per-process input journal.\n\n",
    );
    out.push_str("## Liveness fields\n\n");
    out.push_str(
        "`output_bytes` / `last_output_at` (unix ms) come from the pty reader, so they move even while\n\
         the parser is busy. `has_output=false` = still booting (distinguishable from dead);\n\
         `child_alive=false` = the child's exit has been observed. `seq` is the frame sequence the\n\
         debug listing also reports. Process stats (cpu/mem/subprocess counts) are not in\n\
         these rows; they arrive on the `term://stats` channel.\n\n",
    );
    out.push_str("## Agents\n\n");
    out.push_str(
        "Process rows carry `kind` (`shell` | `agent`) and, for agents, an `agent` block:\n\
         `{tool_id, tool_type, model, runtime, project_id, spawned_at_ms, spawn_uuid, nspid (pid-file | unresolved),\n\
         container? {status: running | exited | missing | unknown, started_at, uptime_s}, bridge}`.\n\
         `bridge` is `ok` | `container-down` (client alive, container not running or restarted since the\n\
         spawn) | `stale` (container running, no output for `agent_stale_after_s`, and a winsize poke\n\
         produced nothing within 2 s). The status enum itself is untouched. The probe\n\
         runs every 5 s while a docker-exec agent is alive and emits `agent://bridge {id, bridge, container}`\n\
         once per transition. The close verification (`gone` | `still-present` | `container-down` |\n\
         `unresolved`) is answered by the close route, not stored on the row.\n\n\
         A queued `prompt` is delivered after the first output plus `agent_ready_quiet_ms` of silence, or —\n\
         for a TUI that never goes quiet — `agent_ready_max_wait_ms` (default 5000) after the first byte\n\
         (`prompt_receipt.reason = ready-by-timeout`). Busy counting treats an agent spawned within the\n\
         last 120 s as busy even before its first byte.\n\n\
         Docker-exec spawns run the program as\n\
         `sh -c 'mkdir -p /tmp/.chappa-ai && echo $$ > /tmp/.chappa-ai/<uuid>.pid && exec \"$0\" \"$@\"' program args…`\n\
         so the NAMESPACE pid is known; close kills that pid (never a `docker top` pid — those are host\n\
         pids), verifies with `test -d /proc/<pid>`, then kills the host client, then removes the pid file.\n\
         Without `sh` in the container the spawn runs unwrapped (`nspid: unresolved`) and close matches\n\
         `CHAPPA_AI_SPAWN_UUID=<uuid>` in `/proc/*/environ` instead.\n\n\
         **Json transport.** A tool with `transport: json` runs its CLI in machine mode over\n\
         PIPES (no pty, no VT parsing; docker runtimes use `-i` without `-t`): claude\n\
         `-p --output-format stream-json --input-format stream-json --verbose` (one persistent child,\n\
         user messages as stream-json lines on stdin), opencode `run --format json` (one child per turn,\n\
         later messages continue the session with `-s <sessionID>`). Machine-mode flags go BEFORE the\n\
         tool's own args. Each stdout line maps through a per-CLI adapter to a typed event pushed as\n\
         `agent://event {id, seq, ts, kind, payload}` and kept in a 1000-event ring (`/agent_events`).\n\
         The agent block gains `transport` and `awaiting_input` — derived at read time from the json\n\
         agent, never mirrored: true after claude's `result` or an opencode child's exit, false again\n\
         on the next send, never on a dead agent (a persistent child's exit, or a per-turn\n\
         continuation that cannot spawn, retires the entry) — the rail's `agent-waiting` attention.\n\
         Close order for a docker json agent is the same as for a pty agent: container side first,\n\
         then the host `docker exec -i` client, then the pid file.\n\
         `prompt` on a json spawn is the first message; its `prompt_receipt` is the turn receipt\n\
         (`seq_before`/`seq_after` are event-ring cursors there, `had_output_within_ms` = delivered).\n",
    );
    out.push_str("\n## Coordination\n\n");
    out.push_str(
        "Scratchpads and todos live in `<app-config>/coordination.db` (SQLite, WAL, `PRAGMA user_version`\n\
         migrations), the app's own file, shared with nothing else. Every write is attributed to the `X-Chappa-Actor`\n\
         request header (chappa-ai-mcp sends its own `CHAPPA_AI_PROCESS_ID`; absent = `user`) as\n\
         `updated_by`. Scratchpads are scoped to a chappa-ai project id or global (`project_id: null`);\n\
         todos always belong to one project.\n\n\
         Typed errors (JSON `error` word, HTTP status): `revision_conflict` (409, carries\n\
         `current_revision`; on the content-bearing scratchpad ops write/edit/clear it also carries\n\
         `current_content`, capped at 8 KB with `current_content_truncated`), `locked` (409,\n\
         `locked_by`, `lock_expires_at`), `cycle` (409), `not_found` (404), `invalid` (400).\n\
         `expected_revision` is REQUIRED on write/edit/rename/clear/delete/tags and optional on\n\
         append/append_section. Archive does not bump the revision. The leading-H1 title override\n\
         applies on create and full overwrite only — edit never renames.\n\
         Write routes answer slim receipts (`{id, project_id, revision}` + the changed fields);\n\
         `response_mode: \"rich\"` answers the full row. Timestamps are unix ms.\n\n\
         Trust model: `X-Chappa-Actor` is UNAUTHENTICATED attribution — any bearer of the control\n\
         token can claim any actor id; the token is the only credential. Todo locks are therefore\n\
         advisory coordination between cooperating agents (a courtesy that prevents lost updates),\n\
         not access control; nothing here isolates one agent from another.\n",
    );
    out.push_str("\n## Timers\n\n");
    out.push_str(crate::timers_http::DOC);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review fix: a handler panic answers 500 instead of killing the
    /// worker (the accept loop wraps every dispatch in `guarded`).
    #[test]
    fn a_panicking_handler_answers_500_and_the_caller_survives() {
        let resp = guarded(|| panic!("boom in a route"));
        assert_eq!(resp.status_code().0, 500);
        let mut body = String::new();
        resp.into_reader().read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "internal");
        assert!(v["message"].as_str().unwrap().contains("boom in a route"), "{body}");
        // And a non-panicking handler passes straight through.
        let ok = guarded(|| json(204, json!({"ok": true})));
        assert_eq!(ok.status_code().0, 204);
    }

    #[test]
    fn route_patterns_capture_placeholders() {
        let p = match_route("/processes/{id}/output", "/processes/7/output").unwrap();
        assert_eq!(p.id, Some(7));
        assert!(match_route("/processes/{id}/output", "/processes/x/output").is_none());
        assert!(match_route("/processes/{id}", "/processes/7/output").is_none());
        let p = match_route(
            "/projects/{project}/processes/{name}/start",
            "/projects/3/processes/import%20%2B%20summarize/start",
        )
        .unwrap();
        assert_eq!(p.project, Some(3));
        assert_eq!(p.name.as_deref(), Some("import + summarize"));
    }

    #[test]
    fn submit_appends_cr_to_the_same_payload() {
        assert_eq!(submit_payload("echo hi", true), b"echo hi\r".to_vec());
        assert_eq!(submit_payload("echo hi", false), b"echo hi".to_vec());
    }

    /// The body read is CAPPED: a huge (or endless) body is truncated to
    /// [`MAX_BODY_BYTES`] instead of being buffered without limit. Truncated
    /// JSON then fails to parse, which is the handlers' plain 400.
    #[test]
    fn request_bodies_are_capped() {
        let big = vec![b'x'; (MAX_BODY_BYTES + 4096) as usize];
        let body = read_body(&mut &big[..]);
        assert_eq!(body.len() as u64, MAX_BODY_BYTES);
        let small = b"{\"confirm\": true}";
        assert_eq!(read_body(&mut &small[..]), "{\"confirm\": true}");
    }

    /// Project-layer failures map to HTTP by TYPE, not by message text: the
    /// addressed thing being absent (unknown id, unknown process, project not
    /// open) is a 404, everything else a 500. The messages are unchanged.
    #[test]
    fn control_errors_map_to_statuses_by_variant() {
        let cases: [(ControlError, u16); 3] = [
            (ControlError::NotFound("no such project: 9".into()), 404),
            (ControlError::NotOpen("project 9 is not open".into()), 404),
            (
                ControlError::Internal("projects store not initialized".into()),
                500,
            ),
        ];
        for (err, want) in cases {
            let message = err.to_string();
            let response = project_response::<Value>(Err(err));
            assert_eq!(response.status_code().0, want, "{message}");
        }
        assert_eq!(
            project_response(Ok(json!({"ok": true}))).status_code().0,
            200
        );
    }

    #[test]
    fn bearer_compare_is_exact() {
        assert!(bearer_matches("Bearer abc", "abc"));
        assert!(!bearer_matches("Bearer abd", "abc"));
        assert!(!bearer_matches("Bearer ab", "abc"));
        assert!(!bearer_matches("abc", "abc"));
    }

    // The doc-sync, route-walk, token and HTTP tests live in
    // tests/control.rs: anything that touches ROUTES (a table of handler fn
    // pointers) links the whole Tauri stack into the test binary, and the
    // lib unit-test harness exe then fails to load on the Windows host
    // (STATUS_ENTRYPOINT_NOT_FOUND) — the integration binaries link the same
    // stack and load fine. Keeping the unit tests pure sidesteps it.
}
