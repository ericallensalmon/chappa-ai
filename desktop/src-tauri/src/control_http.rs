//! Control surface: the app's HTTP API on `127.0.0.1:8324` in every build,
//! bearer-token authenticated — terminals and processes plus the
//! orchestrator additions (receipts, input journal, byte-stream
//! liveness, unsplit argv, whole-screen output, raw-bytes door). The
//! `chappa-ai-mcp` stdio server (desktop/chappa-ai-mcp) is its main client;
//! it doubles as the curl-from-Claude-Code interface.
//!
//! ## Auth
//! A random 32-byte token is generated at first launch into
//! `<app-config>/control_token` (0600 on unix; on Windows inheritance is
//! stripped and only the current user is granted access via `icacls`).
//! EVERY request must carry `Authorization: Bearer <token>` — `GET /` too —
//! except `GET /identity`.
//!
//! ## Port
//! The everyday app binds the fixed [`BIND`]. A run under a separate profile
//! (`CHAPPA_PROFILE_DIR`) binds an OS-chosen loopback port instead
//! ([`PROFILE_BIND`]), so it can run beside the everyday app with a control
//! surface of its own; its clients find the port in the profile's endpoint
//! file, the same file that proves the server's identity. See [`bind_for`].
//!
//! ## Identity
//! Another local program can hold the port first. After a
//! successful bind the app writes `<app-config>/control_endpoint` (owner-only,
//! like the token) with the bound address and a per-run nonce, and answers
//! the tokenless `GET /identity?challenge=…` with a proof keyed by the token.
//! `chappa-ai-mcp` checks that proof (and that it names the endpoint file's
//! address and run) before it sends the token. The construction and the
//! reasoning live in `chappa_ai_mcp::identity`.
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
//! threads or allocate without limit by whatever else is on localhost.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::AppHandle;
use term_core::pty::PtySpec;

use crate::agent_tools::AgentToolsState;
use crate::agents::{
    agent_events, json_send_input, spawn_agent_headless, SpawnAgentRequest, SpawnContext,
};
use crate::caller::{AgentGrant, Caller};
use crate::commands::default_shell;
use crate::coordination::Coordination;
use crate::coordination_http as coord;
use crate::projects::{ControlError, Projects};
use crate::records::{self, Records};
use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, CreatedEvent, NullSink, Origin, Registry,
    TermId,
};
use crate::settings::SettingsState;
use crate::spawn_gate::{refusal_status, AppSpawnGate, GateRequest, SpawnKind};
use crate::timers::TimerService;
use crate::timers_http as timers;

/// The everyday app's address: fixed, so clients without an endpoint file
/// (a curl, an older client) still know where to look.
pub const BIND: &str = "127.0.0.1:8324";
/// A separate profile's address: loopback, port chosen by the OS. The port
/// is published only in the profile's endpoint file.
pub const PROFILE_BIND: &str = "127.0.0.1:0";

/// The port this instance's control surface bound (0 until it did). A
/// stack source is told it, so its relay never forwards a container to it.
pub static BOUND_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
pub const TOKEN_FILE: &str = "control_token";
/// `<app-config>/control_endpoint` (written on each successful bind) and the
/// one tokenless route. Defined in the MCP crate, which owns the client side
/// of the identity check; see `chappa_ai_mcp::identity` for the scheme.
pub use chappa_ai_mcp::{ENDPOINT_FILE, IDENTITY_PATH};
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
    /// tests with no Tauri runtime.
    pub config_dir: PathBuf,
    /// The agent-tool registry behind `list_agent_tools` /
    /// `spawn_agent`.
    pub agent_tools: AgentToolsState,
    /// Scratchpads + todos over `<app-config>/coordination.db`.
    pub coordination: Coordination,
    /// The timer scheduler (same database, own tables). The default
    /// is INERT — no threads, no store — so tests with no Tauri runtime
    /// construct a `ControlState` without one and the timer routes answer a
    /// clear 500 instead of pretending.
    pub timers: TimerService,
    /// The app handle the control surface broadcasts `term://created`
    /// on, so the webview can adopt a backend-spawned terminal into the rail.
    /// `None` in tests with no Tauri runtime (a silent no-op).
    pub app: Option<AppHandle>,
    /// What `GET /identity` proves. [`serve`] fills it from the bound
    /// address when left `None`.
    pub identity: Option<Arc<ServerIdentity>>,
    /// The durable row records (projects.json / workspace.json + the run
    /// ledger). The default is inert: spawns through it are ephemeral and the
    /// record routes answer a clear error.
    pub records: Records,
}

/// This run's identity: the address actually bound, a fresh random nonce,
/// and the token the proof is keyed by. Deliberately not `Debug` (it holds
/// the token).
pub struct ServerIdentity {
    pub addr: String,
    pub nonce: String,
    token: Arc<str>,
}

impl ServerIdentity {
    /// A new identity for `addr` with a fresh nonce from the OS RNG.
    pub fn new(addr: impl Into<String>, token: &str) -> std::io::Result<Self> {
        Ok(Self {
            addr: addr.into(),
            nonce: random_hex(16)?,
            token: token.into(),
        })
    }

    /// The identity for a bound server (its real address, never [`BIND`]
    /// assumed). Fails for a non-IP listener.
    pub fn for_server(server: &tiny_http::Server, token: &str) -> std::io::Result<Self> {
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| std::io::Error::other("control server is not bound to an IP address"))?;
        Self::new(addr.to_string(), token)
    }

    /// The proof for a client's challenge.
    pub fn prove(&self, challenge: &str) -> String {
        chappa_ai_mcp::identity_proof(&self.token, challenge, &self.addr, &self.nonce)
    }

    /// What the endpoint file records about this run.
    pub fn endpoint(&self) -> chappa_ai_mcp::Endpoint {
        chappa_ai_mcp::Endpoint {
            addr: self.addr.clone(),
            nonce: self.nonce.clone(),
            pid: Some(u64::from(std::process::id())),
        }
    }
}

pub type Resp = tiny_http::Response<std::io::Cursor<Vec<u8>>>;
type Handler = fn(&ControlState, &Params, &str, &str) -> Resp;

/// One row of the single-source table.
pub struct Route {
    pub method: &'static str,
    /// Path pattern; `{id}` = terminal id (u32), `{project}` = project id
    /// (u32), `{name}` = percent-encoded process name, `{rid}` = a
    /// coordination record id (scratchpad / todo / comment, i64),
    /// `{record_id}` = a durable row record's uuid.
    pub path: &'static str,
    /// Request fields: JSON body keys for POST, query keys for GET. `?` marks
    /// optional.
    pub fields: &'static [&'static str],
    pub description: &'static str,
    handler: Handler,
    /// Who may call it.
    pub access: Access,
}

/// Who may call a route. The person's token reaches every route; an agent
/// token reaches what its row's authority covers. See [`crate::caller`]
/// for why this holds only for agents that cannot read the person's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Any token. What it answers or does may still depend on the caller
    /// (a listing filtered to the agent's own rows, a spawn gated to its
    /// project, timers and coordination attributed to it).
    Any,
    /// `{id}` must be the agent's own row or one it spawned (transitively).
    Subtree,
    /// `{record_id}` must be a record of the agent's subtree: its live row
    /// is in the subtree, or its recorded parent chain reaches the agent's
    /// own record.
    SubtreeRecord,
    /// A project- or app-level read: allowed, with what is secret redacted.
    Project,
    /// `{project}` must be the agent's own project.
    OwnProject,
    /// The person only: an agent token is refused (403).
    Person,
}

impl Access {
    /// The doc table's "Who" column.
    pub fn label(self) -> &'static str {
        match self {
            Access::Any => "person; agent",
            Access::Subtree => "person; agent: own subtree",
            Access::SubtreeRecord => "person; agent: own subtree's records",
            Access::Project => "person; agent: project-level read",
            Access::OwnProject => "person; agent: own project",
            Access::Person => "person only",
        }
    }
}

pub const ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: "/",
        fields: &[],
        description: "This route table as JSON (discoverability), plus `instance` {pid, config_dir} — which running chappa-ai answered, so two instances (or a stale one holding the port) are diagnosable.",
        handler: route_table,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: IDENTITY_PATH,
        fields: &["challenge"],
        description: "The ONE route that needs no token: it lets a client check that this server is the app BEFORE sending the token to it (anything else can bind this port first). `challenge` = 32 random bytes as lowercase hex. Answers {addr, nonce, proof}: addr = the address this server bound, nonce = a random value fresh for this run (both also in the owner-only `<app-config>/control_endpoint`, written after each successful bind), proof = hex BLAKE3 keyed hash of `challenge 0x00 addr 0x00 nonce` under the key BLAKE3-derive_key(\"chappa-ai control-surface identity proof v1\", token). A client recomputes the proof from its own token, checks addr/nonce against the endpoint file, and only then sends its authenticated request on the same keep-alive connection.",
        handler: identity_route,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/whoami",
        fields: &[],
        description: "Who the presented token is: {kind: person | agent, process_id?, project_id?, parent?, tool?, routes: [{method, path, access}]} — for an agent token the row it was minted for, that row's project, the agent row that spawned it (null when the person did), and the routes it may call with how far each reaches. An agent token is minted for each spawned agent (and each terminal an agent opens), delivered only in the child's CHAPPA_AI_CONTROL_TOKEN, held only in the app's memory, and revoked when the row exits or closes; the X-Chappa-Actor header is ignored for it.",
        handler: whoami,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/processes",
        fields: &[],
        description: "Every live terminal/process: id, name, status, exit_code, cols, rows, seq, plus byte-stream liveness (output_bytes, last_output_at, has_output, child_alive), `close_on_exit` (the spawn flag — the row reaps itself when its child exits) and, for agents, `agent.parent_process_id` (the process that spawned it, null for a root). `record_id` + `last_run` (see Records) name the durable record a row belongs to: a stopped row with a record_id can be started again (`POST /records/{record_id}/start`) or, when `last_run.session` holds the provider's session id, resumed (`POST /records/{record_id}/resume`); both are null for an ephemeral row. `program_status` is what the program last reported with `OSC 777;status;<working|blocked|done|failed|idle>` (null when it never did or the run ended); `blocked` is the same attention as an agent waiting for input and holds timer and queued-prompt deliveries. Stats fields will ride along here.",
        handler: list_processes,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/processes",
        fields: &["command?", "args?", "cwd?", "name?", "cols?", "rows?", "env?"],
        description: "Spawn a terminal. `command` + `args` reach the OS verbatim (never re-split); no command = the default shell; `env` = {NAME: value} applied over the inherited environment. cols/rows 1..=4096. Returns {id, name}. Ephemeral: the row has no record and vanishes on close. The person's terminal starts as asked; a terminal an agent opens starts only inside the agent's own project roots (no cwd = the project root) and only where the data-safety and secret-scan checks allow (409 naming the check; 403 for a folder outside the roots), is in the agent's subtree, and gets its own agent token plus CHAPPA_AI_PROCESS_ID / CHAPPA_AI_PROJECT_ID in its env (a request's env cannot set those).",
        handler: spawn_terminal,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/processes/{id}",
        fields: &[],
        description: "One process row (same shape as the listing). has_output=false means still booting; child_alive=false means the child has exited.",
        handler: get_process,
        access: Access::Subtree,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/output",
        fields: &["lines?", "offset?"],
        description: "Terminal text. Default = the WHOLE current screen. `lines`=N adds the last N scrollback lines above it; `offset`=O (with lines) returns a history-only window skipping the O most recent scrollback lines. No silent cap.",
        handler: get_output,
        access: Access::Subtree,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/input_journal",
        fields: &[],
        description: "The last 100 send_input/send_bytes deliveries: [{ts, bytes_len, text, submit}], oldest first.",
        handler: get_input_journal,
        access: Access::Subtree,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/input",
        fields: &["text", "submit?", "wait_ms?"],
        description: "Write text to the pty in ONE atomic write (submit appends \\r to the same write). Returns a receipt {written, seq_before, seq_after, had_output_within_ms, wait_ms} off the byte counter. For a json-transport agent the text is delivered as ONE user message (claude: a stream-json user line on stdin; opencode: a `run -s <session>` continuation child) and the receipt is {written, transport: \"json\", delivered, reason?, waited_ms, seq_before, turn_started_seq?, wait_ms}: delivered=true means the CLI's next turn_started arrived within wait_ms (default 5000, max 20000); reason=timeout means written but not yet acknowledged, exited = no child to write to (or it died before its turn started: `exited: <why>`), busy = the previous opencode turn is still running. `wait_ms` is the effective wait. A json agent takes whole messages: `submit: false` or empty text is a 400, and a refused send (busy/exited/unsupported) is not journaled. Held: when the target is an agent working next to a database a stack source reports may hold real data (its data-safety block covers the agent's folder), nothing is written; the text is queued and the answer is {written: false, held: true, held_count, reason}. Queued messages are delivered in order once the block clears (clean check or acknowledgement); what the person types is never held. From an agent token (see Callers): held while the target's screen shows a confirmation-shaped state ({written: false, held: true, held_id, held_count, reason}; the person releases or drops it on the row), refused (403) when the text is only a choice key, and written with the sender line first when the target is another agent.",
        handler: send_input,
        access: Access::Subtree,
    },
    Route {
        method: "GET",
        path: "/processes/{id}/agent_events",
        fields: &["since?"],
        description: "A json-transport agent's typed events with seq > since (default 0), from a per-agent ring of the last 1000: [{id, seq, ts, kind, payload}], kind = turn_started | turn_ended | tool_call | text | usage | awaiting_input | compaction | error | raw. `raw` carries the unclassified native object (or {line} / {stderr}) so nothing is lost. 409 for a tty agent or a plain terminal.",
        handler: get_agent_events,
        access: Access::Subtree,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/bytes",
        fields: &["bytes", "wait_ms?"],
        description: "Raw bytes ([u8]) to the pty, same receipt and journal treatment as /input. The escape hatch for control sequences. Held like /input while a data-safety block covers an agent's folder. From an agent token: held and refused like /input, and refused (403) into another agent's terminal (raw bytes cannot carry the sender line).",
        handler: send_bytes,
        access: Access::Subtree,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/close",
        fields: &["confirm"],
        description: "Close a terminal (kills the child). Requires confirm=true. For a docker-exec agent the container-side process is killed and verified FIRST (TERM, poll, KILL), then the host client; the answer carries verification = gone | still-present | container-down | unresolved (null for plain terminals).",
        handler: close_terminal,
        access: Access::Subtree,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/session",
        fields: &["session_id"],
        description: "The process names the conversation it is running (the self-targeting half of the announce path: `set_session` over MCP sends the caller's own id here). The id lands on the row's current run under the run-uuid rule — the record head, the ledger row, and a `term://session` broadcast when it changed — and is what `POST /records/{record_id}/resume` continues later. The id must be one plain word, `[A-Za-z0-9][A-Za-z0-9._:-]{0,127}` (it later becomes a word on the resume command line): anything else is a 400. Answers {id, record_id, run_uuid, session, durable}; durable=false for an ephemeral row (stored on the live row only). The other announce path needs no call at all: print `OSC 777;session;<id>` (`printf '\\033]777;session;%s\\007' \"$ID\"`).",
        handler: records::route_set_session,
        access: Access::Subtree,
    },
    Route {
        method: "POST",
        path: "/processes/{id}/summary",
        fields: &["text"],
        description: "The process posts what its run was about (the self-targeting control twin of `OSC 777;summary;<text>`: `set_summary` over MCP sends the caller's own id here). Markdown, normalized like the OSC path (CRLF to LF, surrounding blank lines trimmed) and capped at 16 KiB with a truncation marker; lands on the row's CURRENT run — the record head and the ledger row, last writer wins — and broadcasts `term://summary` when it changed. Shown on the row's record card and the strip above its exited pane. Only a live run takes one: 409 {error: not_running} once the run has ended. Answers {id, record_id, run_uuid, summary, summary_source: mcp, truncated, durable}. The no-call path: print `OSC 777;summary;<text>` (`printf '\\033]777;summary;%s\\007' \"build ok, 3 warnings\"`).",
        handler: records::route_set_summary,
        access: Access::Subtree,
    },
    Route {
        method: "GET",
        path: "/agent_tools",
        fields: &[],
        description: "Registered agent tools (the shape {id, name, command, tool_type, enabled} plus model, runtime {kind: host | docker_exec, container?, user?, workdir?, tty?, max_busy_in_container?}, container (the tool's container name, or null), program, args, env (the variable NAMES only; values are never served), max_busy). `command` is DERIVED for display; program/args are the truth.",
        handler: list_agent_tools,
        access: Access::Project,
    },
    Route {
        method: "POST",
        path: "/agents",
        fields: &["agent_tool_id", "project_id?", "name?", "extra_args?", "prompt?", "working_dir?", "env?", "force?", "cols?", "rows?", "parent_process_id?", "close_on_exit?", "ephemeral?"],
        description: "Spawn an agent from a registered tool. extra_args are appended to the tool's args VERBATIM (an element with spaces stays one argv entry). Env = inherited < tool.env < `env` < identity (CHAPPA_AI_PROCESS_ID/PROJECT_ID/AGENT_TOOL_ID; `env` names must be [A-Z_][A-Z0-9_]*); docker-exec runtimes pass every env entry as -e and run the program under a pid-recording sh wrapper. `working_dir` starts the agent in a folder other than the project root; it must resolve inside the project's directory or one of its git worktrees (every open project's when there is no project_id) or a folder a trusted source may use, else 400 with the reason. Before any spawn with a working directory the folder is scanned for secrets when Settings ▸ Security has it on (default: on when gitleaks is installed): under the block-agents policy a finding refuses the spawn with `secret-scan-blocked:` + JSON {projectRoot, cwd, findings: [{file, rule, line}], skipped} — never a value, files named from the project's folder — until it is fixed or acknowledged; under warn it launches and notifies. A scan that does not finish (no scanner while the setting forces the scan on, a timeout, a scanner error, the scan's total size limit) refuses under either policy with the same prefix and `incomplete: <why>` instead of findings; only the person can launch past it, from the app's dialog. A folder covered by a stack source's data-safety block (its database may hold real data) or secret-scan block (possible secrets in its worktrees, or their scan did not finish) refuses the spawn with the reason, naming the check, until it is clean or acknowledged in Environment. `prompt`: a tool with a prompt_argv (claude and codex by default, as `-- <prompt>`) gets it on the command line, last, and nothing is typed (prompt_receipt.reason=argv), so it can never answer a startup question; a prompt that starts with `-` is refused unless the word before `{prompt}` in the prompt_argv is `--`, since the program would read it as an option; otherwise it queues until the pty is ready (has_output and agent_ready_quiet_ms of silence, default 750 ms — or agent_ready_max_wait_ms after the first byte for a TUI that never goes quiet, reason=ready-by-timeout), is HELD while a known startup or permission question is on screen (folder trust, a new MCP server, an external CLAUDE.md, the bypass-permissions warning: delivered=false, reason=`held: waiting on <question>` when the cap passes), then ONE atomic write of text+\\r (text with a line break or other control character goes as a bracketed paste when the program enabled that mode, so it cannot submit early); prompt_receipt reports delivered=false with reason=exited when the child dies first. Busy guards: over max_busy / max_busy_in_container the spawn is refused naming the busy processes; force=true overrides. Returns {process_id, term_id, record_id, name, agent_instructions, prompt_receipt?, container? (status, started_at, uptime_s, mem_bytes, mem_limit_bytes), agent, forced?}. The row NESTS under its parent in the rail — `parent_process_id` if given, else the `X-Chappa-Actor` header parsed as a process id (chappa-ai-mcp sends its own CHAPPA_AI_PROCESS_ID, so an agent spawning an agent nests automatically); the candidate must name a live process or the spawn records null (a root). An agent token's spawn nests under the calling agent (or a row of its own subtree it names) whatever the header says, starts only in the agent's own project (another project_id is refused) and its roots, passes the launch checks with no way past them, may add only the tool's allowed extra_args (400 otherwise), and may not send a first prompt that is only a choice key; a typed first prompt from an agent starts with the sender line and is held while the new agent's screen shows a confirmation-shaped state. Every spawned agent gets its own agent token in CHAPPA_AI_CONTROL_TOKEN. The spawn creates a durable agent RECORD (see Records): the row survives exit and close as a stopped row that can be started again, with the parent recorded by record id. `ephemeral=true` opts out (no record, the row vanishes on close); `close_on_exit=true` implies it and closes the row through the ordinary close path once the child exits (after term://exited and the exit notification; non-zero exits close too), broadcasting `term://closed {term_id, reason: \"close_on_exit\"}`; every other close broadcasts `reason: \"closed\"`. A bash-hosted tool never exits by itself: end its command with `; exit`.",
        handler: spawn_agent,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/agents/reap",
        fields: &[],
        description: "Sweep every ENABLED docker_exec tool's distinct (container, user) pair for container processes carrying CHAPPA_AI_SPAWN_UUID that no LIVE registry entry owns, and reap each orphan root through the shared TERM/poll/KILL/verify sequence (as the tool's user). Returns one entry per orphan handled: {container, user, pid, uuid, verdict: gone | still-present | container-down | unresolved}. NOT gated by the agentReapOrphans setting (that gates only the automatic startup/probe sweeps) - this is the on-demand path. A container that is down or docker unreachable is skipped silently. Does not touch processes without the marker, and never consults docker top.",
        handler: reap_orphans,
        access: Access::Person,
    },
    Route {
        method: "GET",
        path: "/projects",
        fields: &[],
        description: "Stored projects: [{id, name, path, icon, open, processes}]; processes are populated only for open projects.",
        handler: list_projects,
        access: Access::Project,
    },
    Route {
        method: "GET",
        path: "/projects/{project}",
        fields: &[],
        description: "One project with its process rows (status, term_id, exit_code, auto_start, …).",
        handler: get_project,
        access: Access::OwnProject,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/start",
        fields: &["wait_ms?"],
        description: "Start a project process (idempotent on a live one). The app's frontend performs the spawn; waits up to wait_ms (default 3000) for it to register, else answers pending=true.",
        handler: start_process,
        access: Access::Person,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/stop",
        fields: &[],
        description: "Stop a project process. Non-running = no-op success (never an error).",
        handler: stop_process,
        access: Access::Person,
    },
    Route {
        method: "POST",
        path: "/projects/{project}/processes/{name}/restart",
        fields: &["wait_ms?"],
        description: "Restart = stop (no-op if not running) then start.",
        handler: restart_process,
        access: Access::Person,
    },
    // ---- records ----
    Route {
        method: "GET",
        path: "/records/{record_id}/runs",
        fields: &["limit?"],
        description: "A record's run history, newest first: {record_id, runs: [{run_uuid, status: running | exited | failed | stopped, started_at_ms, ended_at_ms, exit_code, success, tail_kind: text | events, tail: [...], summary?, session?}]}. `tail` is the automatic tail captured at exit — the last 200 screen lines, or the last 200 typed events for a json-transport agent. The ledger keeps the last 20 runs per record (`limit`, default and max 20). 404 for an unknown record.",
        handler: records::route_record_runs,
        access: Access::SubtreeRecord,
    },
    Route {
        method: "POST",
        path: "/records/{record_id}/start",
        fields: &["wait_ms?"],
        description: "Start a stopped record: a fresh terminal and a NEW run (new run_uuid; the previous run stays in the ledger). An agent starts a new session — its stored prompt is never replayed. A project command is started by the app's frontend like start_process (waits up to wait_ms, default 3000, else pending=true); a workspace command answers 409 (it starts from the app). A record whose terminal is still live is a no-op answering that terminal. Answers {record_id, kind, term_id, run_uuid, pending}; the webview receives `term://created` carrying the record_id.",
        handler: records::route_start_record,
        access: Access::SubtreeRecord,
    },
    Route {
        method: "POST",
        path: "/records/{record_id}/resume",
        fields: &["session_id?"],
        description: "Resume an agent record: start it again with the PROVIDER continuing `session_id` (default: the last run's saved `last_run.session`), so the agent comes back with its complete prior conversation — nothing is replayed by the app. The tool's `resume_argv` (`{session_id}` substituted: claude `--resume=<id>`, opencode `-s <id>`) goes on the command line; the id must be one plain word (`[A-Za-z0-9][A-Za-z0-9._:-]{0,127}`), and a saved id of any other shape answers 409 resume_unavailable; a per-turn json CLI is continued through its adapter. The start is watched through the ready gate and answers {record_id, kind, term_id, run_uuid, session_id, check: quiet | ready_by_timeout | silent | deferred}. A child that exits before it is ready was refused by the provider: 409 {error: resume_rejected, record_id, run_uuid, term_id, exit_code, provider_text, message} with the provider's own words (e.g. `No conversation found with session ID: <id>`) — never a silent fresh start. 409 {error: no_session} when there is nothing to resume (start the record instead); 409 {error: resume_unavailable} for a running record, a non-agent, a missing tool or an empty resume_argv; 404 for an unknown record.",
        handler: records::route_resume_record,
        access: Access::SubtreeRecord,
    },
    Route {
        method: "POST",
        path: "/records/{record_id}/remove",
        fields: &["confirm"],
        description: "Remove a record and its run history (requires confirm=true). An attached terminal is closed first; for a command record this deletes the command definition itself. Answers {record_id, kind, removed, runs_deleted}.",
        handler: records::route_remove_record,
        access: Access::SubtreeRecord,
    },
    // ---- scratchpads ----
    Route {
        method: "GET",
        path: "/scratchpads",
        fields: &["project_id?", "include_global?", "query?", "tags?", "include_archived?", "offset?", "limit?"],
        description: "List scratchpads (no content): {scratchpads: [{scratchpad_id, project_id, name, revision, tags, archived, updated_at, updated_by, line_count, matched_fields?, snippet?}], total}. `project_id` scopes to one project (`global` = pads with no project; absent = every pad); include_global=true adds the global pads to a project scope; `query` matches name + content case-insensitively and adds matched_fields + a snippet; `tags` (comma list) = any-of; archived pads are hidden unless include_archived=true.",
        handler: coord::scratchpad_list,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads",
        fields: &["name", "content", "tags?", "project_id?", "response_mode?"],
        description: "Create a scratchpad (scratchpad_write without scratchpad_id). A leading `# Title` line in the content overrides `name`. project_id null/absent = a global pad. Answers a slim receipt {scratchpad_id, project_id, revision, name}; response_mode=rich answers the full row.",
        handler: coord::scratchpad_create,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/scratchpads/tags",
        fields: &["project_id?"],
        description: "Distinct scratchpad tags: {tags: [..]} (project_id scopes; absent = every pad).",
        handler: coord::scratchpad_tags_list,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}",
        fields: &["mode?", "section_heading?", "offset?", "limit?"],
        description: "Read a scratchpad with its revision metadata. mode = full (default; `offset`/`limit` window the lines) | outline (heading outline only, no content) | section (one markdown section — `section_heading` required, exact text after trimming, first duplicate wins). The `content`/`headings`/`lines` spellings are accepted.",
        handler: coord::scratchpad_read,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}",
        fields: &["name", "content", "expected_revision", "tags?", "response_mode?"],
        description: "Overwrite a scratchpad (scratchpad_write with scratchpad_id). expected_revision is REQUIRED; a mismatch is 409 {error: revision_conflict, current_revision, current_content}. A leading `# Title` overrides name; omitted tags are kept.",
        handler: coord::scratchpad_overwrite,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}/find",
        fields: &["query", "case_sensitive?", "limit?", "context_lines?", "scope?"],
        description: "Literal substring search in one pad: {matches: [{line (1-based), text, before, after}], total_matches, truncated}. limit default 20 (1..=100), context_lines default 1 (0..=3), scope = all | headings | content.",
        handler: coord::scratchpad_find,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/scratchpads/{rid}/tail",
        fields: &["lines?"],
        description: "The last `lines` lines (default 10) with revision metadata.",
        handler: coord::scratchpad_tail,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/rename",
        fields: &["name", "expected_revision", "response_mode?"],
        description: "Rename at expected_revision (required) without touching the content.",
        handler: coord::scratchpad_rename,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/append",
        fields: &["content", "expected_revision?", "response_mode?"],
        description: "Append at the end (a newline separates it from existing content). expected_revision is optional — appends cannot clobber, last-writer-wins is fine; the revision still advances.",
        handler: coord::scratchpad_append,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/append_section",
        fields: &["heading", "content", "expected_revision?", "response_mode?"],
        description: "Append at the end of the section under an EXISTING heading (before the next heading of the same or higher level). A missing heading is 400 and changes nothing. expected_revision optional.",
        handler: coord::scratchpad_append_section,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/edit",
        fields: &["target", "content", "expected_revision", "response_mode?"],
        description: "Replace one section or line range at expected_revision (required). target = {heading} (content starting with a heading replaces the whole section; otherwise the heading is kept and its body replaced) | {start_line, end_line} (1-based inclusive, bounds-checked). {type: section, section_heading} / {type: line_range, offset, limit} are accepted too. Never renames: the leading-H1 title override applies on create and full overwrite only, so an explicit rename survives edits.",
        handler: coord::scratchpad_edit,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/tags/add",
        fields: &["tags", "expected_revision", "response_mode?"],
        description: "Add tags in one revision bump at expected_revision (required).",
        handler: coord::scratchpad_add_tags,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/tags/remove",
        fields: &["tags", "expected_revision", "response_mode?"],
        description: "Remove tags in one revision bump at expected_revision (required).",
        handler: coord::scratchpad_remove_tags,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/clear",
        fields: &["expected_revision", "confirm", "response_mode?"],
        description: "Empty the content at expected_revision (required). Requires confirm=true.",
        handler: coord::scratchpad_clear,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/delete",
        fields: &["expected_revision", "confirm"],
        description: "Delete the pad at expected_revision (required). Requires confirm=true. Answers {scratchpad_id, project_id, deleted: true}.",
        handler: coord::scratchpad_delete,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/archive",
        fields: &["archived?", "response_mode?"],
        description: "Hide from the default list without deleting (archived=false un-hides). No revision guard.",
        handler: coord::scratchpad_archive,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/scratchpads/{rid}/transfer",
        fields: &["target_project_id", "expected_revision?", "response_mode?"],
        description: "Move the pad to another project (null = global). Only the project changes.",
        handler: coord::scratchpad_transfer,
        access: Access::Any,
    },
    // ---- todos ----
    Route {
        method: "GET",
        path: "/todos",
        fields: &["project_id?", "status?", "completed?", "is_blocked?", "priority?", "query?", "tags?", "sort?", "offset?", "limit?"],
        description: "Todo summaries (no body): {todos: [..], total}. Filters: status (open | in_progress | done), completed, is_blocked (derived: any blocker not completed), priority (low | normal | high | urgent), query (title + body + comments), tags (any-of). sort = updated_desc (default) | updated_asc | created_desc | created_asc | priority_desc | priority_asc | title. limit default 50, max 200.",
        handler: coord::todo_list,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos",
        fields: &["project_id", "title", "body?", "priority?", "tags?", "response_mode?"],
        description: "Create a todo in a project (project_id required — todos are project-scoped). Slim receipt {todo_id, project_id, revision, title}; response_mode=rich answers the full row.",
        handler: coord::todo_create,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/todos/tags",
        fields: &["project_id?"],
        description: "Distinct todo tags: {tags: [..]}.",
        handler: coord::todo_tags_list,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/todos/{rid}",
        fields: &["include_comments?"],
        description: "One todo: {todo_id, project_id, title, body, priority, status, completed, tags, revision, locked_by, lock_expires_at, is_blocked, blocker_ids, comment_count, created_at, updated_at, updated_by} (+ comments with include_comments=true). An expired lock reads as no lock.",
        handler: coord::todo_get,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}",
        fields: &["title?", "body?", "priority?", "status?", "tags?", "expected_revision?", "response_mode?"],
        description: "Update a subset of fields; omitted fields are preserved. status=done sets completed. A foreign live lock is 409 {error: locked, locked_by, lock_expires_at}; a stale expected_revision is 409 revision_conflict. Slim receipt = {todo_id, project_id, revision} + the changed fields.",
        handler: coord::todo_update,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/tags/add",
        fields: &["tag", "response_mode?"],
        description: "Add one tag (lock-guarded).",
        handler: coord::todo_add_tag,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/tags/remove",
        fields: &["tag", "response_mode?"],
        description: "Remove one tag (lock-guarded).",
        handler: coord::todo_remove_tag,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/set",
        fields: &["blocker_ids", "response_mode?"],
        description: "Replace the blocker list. Blockers must be in the same project; a cycle (including self) is 409 {error: cycle}. Answers blocker_ids + the derived is_blocked.",
        handler: coord::todo_set_blockers,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/add",
        fields: &["blocker_id", "response_mode?"],
        description: "Add one blocker (cycle-checked).",
        handler: coord::todo_add_blocker,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/blockers/remove",
        fields: &["blocker_id", "response_mode?"],
        description: "Remove one blocker.",
        handler: coord::todo_remove_blocker,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/complete",
        fields: &["completed", "release_lock?", "response_mode?"],
        description: "Mark complete (status=done) or incomplete (status back to open). Releases the caller's OWN lock unless release_lock=false. affected_todo_ids = todos blocked by this one.",
        handler: coord::todo_complete,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/lock",
        fields: &["lease_ms?", "lease_ttl_seconds?", "response_mode?"],
        description: "Take or renew an edit lease keyed by ACTOR id (default 300 s, max 24 h; lease_ttl_seconds saturates). A foreign live lock is 409 locked; expired leases are ignored. Advisory coordination only: for the person's token the actor is the X-Chappa-Actor header (any of the person's tools can claim any id), so a lock is a courtesy between cooperating callers, not access control; for an agent token the actor is the agent's own row.",
        handler: coord::todo_lock,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/unlock",
        fields: &["response_mode?"],
        description: "Release a lock you hold (none/expired = no-op success; a foreign live lock is 409).",
        handler: coord::todo_unlock,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/transfer",
        fields: &["target_project_id", "response_mode?"],
        description: "Move a todo to another project: comments kept, blockers (both directions) and lock cleared.",
        handler: coord::todo_transfer,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/delete",
        fields: &["confirm"],
        description: "Delete a todo with its comments and blocker edges. Requires confirm=true. Answers {todo_id, project_id, deleted, affected_todo_ids}.",
        handler: coord::todo_delete,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/todos/{rid}/comments",
        fields: &["offset?", "limit?"],
        description: "Comments on a todo, oldest first: {comments: [{comment_id, todo_id, actor, body, created_at, updated_at}], total}.",
        handler: coord::todo_comment_list,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todos/{rid}/comments",
        fields: &["body", "response_mode?"],
        description: "Add a comment (attributed to the actor). Slim receipt {comment_id, todo_id}.",
        handler: coord::todo_comment_create,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todo_comments/{rid}",
        fields: &["body", "response_mode?"],
        description: "Edit a comment's body.",
        handler: coord::todo_comment_update,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/todo_comments/{rid}/delete",
        fields: &["confirm"],
        description: "Delete a comment. Requires confirm=true.",
        handler: coord::todo_comment_delete,
        access: Access::Any,
    },
    // ---- timers ----
    Route {
        method: "POST",
        path: "/timers",
        fields: &["delay_ms", "body", "delivery_process_id?", "loop?", "repeat_every_ms?", "name?", "project_id?"],
        description: "timer_set: fire `body` into a process after delay_ms. `loop: true` repeats every delay_ms; `repeat_every_ms` overrides the interval. delivery_process_id defaults to the CALLER's own process (an agent token's row, else the X-Chappa-Actor header, which chappa-ai-mcp fills from CHAPPA_AI_PROCESS_ID; an agent token may deliver only to its own subtree, 403 otherwise) — a timer delivers to exactly one process, which must be LIVE now (404 otherwise) and is pinned by its uuid. project_id defaults to the actor's own project. The body is injected verbatim as a fresh user turn through the delivery path (quiet-only policy). It may hold line breaks but no other control character (ESC, CR, Ctrl-C and the like): 400 otherwise. A body an agent set is, at firing time, held while the target's screen shows a confirmation-shaped state (the firing stays in flight and is listed on the row for the person), refused when it is only a choice key (the firing fails with the reason), and starts with the sender line when it goes into another agent's terminal.",
        handler: timers::timer_set,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/timers/idle_any",
        fields: &["processes", "max_wait_ms", "body", "delivery_process_id?", "idle_ms?", "confirm_ms?", "rearm?", "name?", "project_id?"],
        description: "timer_fire_when_idle_any: fire when ANY watched process goes idle, or at max_wait_ms (reason=deadline). `processes` accepts ids or names — a name resolves within project_id (default: the actor's project) first, and one that matches in several projects with no scope is a 400 naming the candidates. Idle = child_alive and now - last_output_at >= idle_ms (default idle_threshold_ms, 120000) off the BYTE STREAM, never render diffs. Processes already idle at schedule time are IGNORED until they become busy again (`any` waits for a NEW transition) and reported as already_idle. A met condition enters `confirming` and is re-checked after confirm_ms; bytes in that window re-arm it (rearm defaults true). Answers {timer_id, status: scheduled, already_idle, waiting_on, note, deadline_at}.",
        handler: timers::timer_fire_when_idle_any,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/timers/idle_all",
        fields: &["processes", "max_wait_ms", "body", "delivery_process_id?", "idle_ms?", "confirm_ms?", "rearm?", "name?", "project_id?"],
        description: "timer_fire_when_idle_all: same, but every watched process must be idle. Already-idle processes COUNT AS SATISFIED; when all of them are, the answer is {status: \"already_satisfied\", timer_id: null} and NO timer is created.",
        handler: timers::timer_fire_when_idle_all,
        access: Access::Any,
    },
    Route {
        method: "GET",
        path: "/timers",
        fields: &["include_fired?", "all?", "limit?", "offset?", "project_id?"],
        description: "timer_list: {timers: [{id, name, kind, status: pending | confirming | paused | fired | cancelled | expired, delivery: {firing_id, process_id, status: in_flight | delivered | failed | coalesced, delivered: true | false | null, receipt, reason: condition | deadline | missed | expired, error, coalesced_with, at}, fired_count, next_fire_at, deadline_at, waiting_on, …}], total, limit, offset}. Pending first, newest first. Pending only by default; include_fired=true adds fired/cancelled/expired within timer_retention_hours. Owner-scoped (the actor that created them); all=true lists every actor's (the person's token only; an agent token always sees its own). project_id defaults to the actor's project.",
        handler: timers::timer_list,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/cancel",
        fields: &[],
        description: "timer_cancel: stop a pending/confirming timer (owner-scoped; the `user` actor may act on any timer). Answers {timer_id, project_id, cancelled, status}.",
        handler: timers::timer_cancel,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/pause",
        fields: &[],
        description: "timer_pause: STOPS THE CLOCK — the remaining delay (or remaining deadline) is captured, so a paused timer never comes due while paused.",
        handler: timers::timer_pause,
        access: Access::Any,
    },
    Route {
        method: "POST",
        path: "/timers/{rid}/resume",
        fields: &[],
        description: "timer_resume: continue from the captured remaining delay.",
        handler: timers::timer_resume,
        access: Access::Any,
    },
];

// ---- token ------------------------------------------------------------------

/// Read the token, creating it on first launch with owner-only permissions.
/// Never regenerates an existing owner-only file (clients cache nothing, but
/// an agent mid-session must not be locked out by an app restart). A file
/// that is NOT owner-only (a config dir on a shared path, a file copied in
/// or made by hand) may already have been read by another user, so its
/// token is replaced by a new one rather than reused.
pub fn load_or_create_token(config_dir: &Path) -> std::io::Result<String> {
    let path = config_dir.join(TOKEN_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            match stack_core::owner_only::is_owner_only(&path) {
                Ok(true) => return Ok(trimmed.to_owned()),
                Ok(false) => log::warn!(
                    "[control_http] {} is readable by others; replacing the token",
                    path.display()
                ),
                Err(e) => log::warn!(
                    "[control_http] cannot read the access list of {} ({e}); replacing the token",
                    path.display()
                ),
            }
        }
        // A new file, never the old one rewritten: a handle someone opened
        // on the old file must not see the new token.
        std::fs::remove_file(&path)?;
    }
    project_model::atomic::ensure_private_dir(config_dir).map_err(std::io::Error::other)?;
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
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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

/// Windows: the token goes into a sibling temp file through the owner-only
/// writer (created empty and unshared, restricted, THEN written), and only
/// then is renamed into place — so `control_token` never exists with the
/// directory's inherited ACL, not even for an instant, the secret is never
/// written under that ACL either, and a failed `icacls` leaves nothing
/// behind for the next launch to trust.
#[cfg(windows)]
fn write_owner_only(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    // A temp file a crashed run left is not ours to trust.
    let _ = std::fs::remove_file(&tmp);
    let result = stack_core::owner_only::write_new(&tmp, token.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(any(unix, windows)))]
fn write_owner_only(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, token)
}

/// The token file path for a config dir (what the MCP crate reads).
pub fn token_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TOKEN_FILE)
}

/// The endpoint file path for a config dir (what the MCP crate reads to find
/// and verify this instance).
pub fn endpoint_path(config_dir: &Path) -> PathBuf {
    config_dir.join(ENDPOINT_FILE)
}

/// Record a successfully bound server's address and nonce for clients, with
/// the same owner-only writer as the token. Owner-only is not what keeps the
/// token safe (the challenge does), but it keeps another user from pointing
/// clients somewhere else or deleting the record.
pub fn publish_endpoint(config_dir: &Path, identity: &ServerIdentity) -> std::io::Result<()> {
    project_model::atomic::ensure_private_dir(config_dir).map_err(std::io::Error::other)?;
    write_owner_only(&endpoint_path(config_dir), &identity.endpoint().to_json())
}

/// After a failed bind: an existing endpoint file is either a live sibling
/// instance's (same config dir, it holds the port; keep it, its clients are
/// fine) or a dead run's (remove it, so clients report "not running" instead
/// of a confusing identity failure). Told apart by running the same identity
/// check a client does, which never sends the token. Either way a stale file
/// could not have been trusted: the dead run's nonce cannot be proved by
/// anything that lacks the token.
pub fn retire_stale_endpoint(config_dir: &Path, token: &str) -> bool {
    let path = endpoint_path(config_dir);
    if !path.exists() {
        return false;
    }
    match chappa_ai_mcp::identity::probe_endpoint(&path, token, Duration::from_secs(2)) {
        Ok(ep) => {
            log::warn!(
                "[control_http] {ENDPOINT_FILE} belongs to a live instance (pid {:?} at {}); leaving it",
                ep.pid,
                ep.addr
            );
            false
        }
        Err(why) => {
            log::warn!("[control_http] removing stale {ENDPOINT_FILE}: {why}");
            std::fs::remove_file(&path).is_ok()
        }
    }
}

// ---- server -----------------------------------------------------------------

/// The address to bind: [`BIND`] for the everyday app, [`PROFILE_BIND`] when
/// the run has a separate profile.
pub fn bind_for(profile: Option<&crate::profile::Profile>) -> &'static str {
    if profile.is_some() {
        PROFILE_BIND
    } else {
        BIND
    }
}

/// The endpoint file in `config_dir` when it names a live instance that
/// proves it holds `token`, else `None`. Never sends the token.
fn live_endpoint(config_dir: &Path, token: &str) -> Option<chappa_ai_mcp::Endpoint> {
    let path = endpoint_path(config_dir);
    if !path.exists() {
        return None;
    }
    chappa_ai_mcp::identity::probe_endpoint(&path, token, Duration::from_secs(2)).ok()
}

/// App entry point: load/create the token and serve on `bind` (see
/// [`bind_for`]) from its own thread. A bind failure is loud but not fatal
/// (the app still runs; only the control surface is missing).
pub fn start(mut state: ControlState, config_dir: PathBuf, bind: &'static str) {
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
        let Some((server, identity)) = bind_and_publish(&config_dir, bind, &token) else {
            return;
        };
        eprintln!("[control_http] control surface on http://{}", identity.addr);
        state.identity = Some(identity);
        serve(server, state, token);
    });
}

/// Bind `bind` and publish this run in `config_dir`'s endpoint file; `None`
/// (logged) when this instance gets no control surface.
///
/// With an OS-chosen port no bind conflict can tell a second copy of the
/// same profile that the first is running, so that is checked first: when
/// the endpoint file names a live instance, this one binds nothing and
/// leaves the file to it, as a second copy on the fixed port would.
pub fn bind_and_publish(
    config_dir: &Path,
    bind: &str,
    token: &str,
) -> Option<(tiny_http::Server, Arc<ServerIdentity>)> {
    if bind == PROFILE_BIND {
        if let Some(ep) = live_endpoint(config_dir, token) {
            log::error!(
                "[control_http] another copy of this profile (pid {:?}) serves the control \
                 surface at {}; this one has none, and every chappa-ai-mcp agent tool \
                 reaches the other copy",
                ep.pid,
                ep.addr
            );
            eprintln!(
                "[control_http] this profile is already served at {}",
                ep.addr
            );
            return None;
        }
    }
    let server = match tiny_http::Server::http(bind) {
        Ok(server) => server,
        Err(err) => {
            retire_stale_endpoint(config_dir, token);
            // LOUD (review): the overwhelmingly likely cause is a SECOND
            // chappa-ai already holding the fixed port — in which case every
            // agent tool call lands in the OTHER instance, which looks like
            // the app ignoring commands rather than a bind failure. `GET /`
            // reports pid + config dir so the winner can be identified.
            log::error!(
                "[control_http] FAILED TO BIND {bind}: {err} — the control surface \
                 (and every chappa-ai-mcp agent tool) is DEAD in this instance. \
                 Another chappa-ai is probably already running and holding the port; \
                 `GET /` on {bind} names the pid and config dir of whichever one did bind."
            );
            eprintln!("[control_http] failed to bind {bind}: {err}");
            return None;
        }
    };
    // Bound: publish who we are. Without the endpoint file clients refuse
    // to talk to us (they cannot tell us from an impostor), so a failure
    // here is loud; any older file is removed rather than left to name
    // a previous run.
    if let Some(addr) = server.server_addr().to_ip() {
        BOUND_PORT.store(addr.port(), std::sync::atomic::Ordering::Relaxed);
    }
    let identity = match ServerIdentity::for_server(&server, token) {
        Ok(identity) => Arc::new(identity),
        Err(err) => {
            log::error!("[control_http] cannot create the instance identity: {err}");
            return None;
        }
    };
    if let Err(err) = publish_endpoint(config_dir, &identity) {
        let _ = std::fs::remove_file(endpoint_path(config_dir));
        log::error!(
            "[control_http] cannot write {ENDPOINT_FILE}: {err} — chappa-ai-mcp will refuse \
             to connect to this instance"
        );
    }
    Some((server, identity))
}

/// Serve forever on an already-bound server (tests bind an ephemeral port),
/// from a fixed pool of [`WORKERS`] threads — this thread is one of them, so
/// `serve` still blocks for the life of the server.
pub fn serve(server: tiny_http::Server, mut state: ControlState, token: String) {
    if state.identity.is_none() {
        match ServerIdentity::for_server(&server, &token) {
            Ok(identity) => state.identity = Some(Arc::new(identity)),
            // `GET /identity` then answers 500; every other route still works.
            Err(err) => log::error!("[control_http] cannot create the instance identity: {err}"),
        }
    }
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
    let presented: Vec<String> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_owned())
        .collect();
    let caller = caller_of(&state.registry, &presented, token);
    // Who is writing. For the person's token, the header: chappa-ai-mcp
    // sends its own CHAPPA_AI_PROCESS_ID, a bare curl is attributed to
    // "user". For an agent token the header is ignored: the actor is the
    // row the token was minted for.
    let header_actor = request
        .headers()
        .iter()
        .find(|h| h.field.equiv(ACTOR_HEADER))
        .map(|h| h.value.as_str().trim().to_owned())
        .filter(|a| !a.is_empty());
    // The identity challenge is answered without a token: its whole point is
    // that a client checks the server BEFORE handing the token over.
    let public = method == "GET" && url.split('?').next() == Some(IDENTITY_PATH);
    let response = if public {
        guarded(|| dispatch_as(state, None, &method, &url, ""))
    } else if let Some(caller) = caller {
        let body = read_body(&mut request.as_reader());
        let actor = match caller.grant() {
            Some(grant) => Some(grant.process_id.to_string()),
            None => header_actor,
        };
        guarded(|| dispatch_caller(state, &caller, actor.as_deref(), &method, &url, &body))
    } else {
        json(
            401,
            json!({"error": "unauthorized: send `Authorization: Bearer <token>` (token file: <app-config>/control_token)"}),
        )
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
            json(
                500,
                json!({"error": "internal", "message": format!("handler panicked: {what}")}),
            )
        }
    }
}

/// The caller an `Authorization` header names: the person's token, a live
/// agent token, or nobody (`None` = 401).
pub fn caller_of(registry: &Registry, headers: &[String], person_token: &str) -> Option<Caller> {
    if headers.iter().any(|h| bearer_matches(h, person_token)) {
        return Some(Caller::Person);
    }
    headers.iter().find_map(|h| {
        let presented = h.strip_prefix("Bearer ")?.trim();
        registry.tokens().lookup(presented).map(Caller::Agent)
    })
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
    /// `{record_id}`: a durable row record's uuid.
    pub record_id: Option<String>,
    /// The writing actor: `X-Chappa-Actor` or `"user"` for the person's
    /// token, the row's process id for an agent token.
    pub actor: String,
    /// Who presented the token.
    pub caller: Caller,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            id: None,
            project: None,
            name: None,
            record: None,
            record_id: None,
            actor: crate::coordination::USER_ACTOR.to_owned(),
            caller: Caller::Person,
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
            "{record_id}" => params.record_id = Some(percent_decode(g)),
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

/// [`dispatch`] with an explicit actor (`None` = `"user"`), as the person.
pub fn dispatch_as(
    state: &ControlState,
    actor: Option<&str>,
    method: &str,
    url: &str,
    body: &str,
) -> Resp {
    dispatch_caller(state, &Caller::Person, actor, method, url, body)
}

/// [`dispatch_as`] for `caller`: a route an agent token does not reach
/// answers 403 before its handler runs.
pub fn dispatch_caller(
    state: &ControlState,
    caller: &Caller,
    actor: Option<&str>,
    method: &str,
    url: &str,
    body: &str,
) -> Resp {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut path_matched = false;
    for route in ROUTES {
        if let Some(mut params) = match_route(route.path, path) {
            path_matched = true;
            if route.method == method {
                if let Some(actor) = actor {
                    params.actor = actor.to_owned();
                }
                params.caller = caller.clone();
                if let Caller::Agent(grant) = caller {
                    if let Err(why) = authorize(state, route.access, grant, &params) {
                        return json(403, json!({"error": why}));
                    }
                }
                return (route.handler)(state, &params, query, body);
            }
        }
    }
    if path_matched {
        json(405, json!({"error": "method not allowed"}))
    } else {
        json(
            404,
            json!({"error": "unknown route (GET / lists the surface)"}),
        )
    }
}

/// Whether an agent token reaches this route for these parameters.
fn authorize(
    state: &ControlState,
    access: Access,
    grant: &AgentGrant,
    params: &Params,
) -> Result<(), String> {
    let me = grant.process_id;
    match access {
        Access::Any | Access::Project => Ok(()),
        Access::Person => Err(
            "forbidden: this route is the person's; an agent token does not reach it".to_owned(),
        ),
        Access::Subtree => {
            let id = params.id.unwrap_or_default();
            if state.registry.in_subtree(me, id) {
                Ok(())
            } else {
                Err(format!(
                    "forbidden: process {id} is not this agent's own row or one it spawned"
                ))
            }
        }
        Access::OwnProject => {
            if params.project.is_some() && params.project == grant.project_id {
                Ok(())
            } else {
                Err("forbidden: an agent token reads only its own project".to_owned())
            }
        }
        Access::SubtreeRecord => {
            let record_id = params.record_id.clone().unwrap_or_default();
            if record_in_subtree(state, me, &record_id) {
                Ok(())
            } else {
                Err(format!(
                    "forbidden: record {record_id} is not one this agent's subtree spawned"
                ))
            }
        }
    }
}

/// Whether `record_id` belongs to agent row `me`'s subtree: its live row
/// is in the subtree, or walking its recorded parents reaches `me`'s own
/// record (a stopped child the agent spawned, a grandchild).
fn record_in_subtree(state: &ControlState, me: TermId, record_id: &str) -> bool {
    if let Some((term, _)) = state.registry.term_of_record(record_id) {
        if state.registry.in_subtree(me, term) {
            return true;
        }
    }
    let Some(mine) = state.registry.record_of(me) else {
        return false;
    };
    if mine == record_id {
        return true;
    }
    let mut at = record_id.to_owned();
    for _ in 0..64 {
        match state.records.parent_record_of(&at) {
            Some(parent) if parent == mine => return true,
            Some(parent) => at = parent,
            None => return false,
        }
    }
    false
}

// ---- handlers ---------------------------------------------------------------

/// `GET /whoami`.
fn whoami(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let routes: Vec<Value> = ROUTES
        .iter()
        .filter(|r| !(params.caller.is_agent() && r.access == Access::Person))
        .map(|r| {
            json!({
                "method": r.method,
                "path": r.path,
                "access": if params.caller.is_agent() { r.access.label() } else { "person" },
            })
        })
        .collect();
    let mut out = json!({ "kind": params.caller.kind(), "routes": routes });
    if let Some(grant) = params.caller.grant() {
        out["process_id"] = json!(grant.process_id);
        out["project_id"] = json!(grant.project_id);
        out["parent"] = json!(grant.parent_process_id);
        out["tool"] = json!(grant.tool);
        out["name"] = json!(state.registry.name_of(grant.process_id));
    }
    json(200, out)
}

fn route_table(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    json(
        200,
        json!({
            "service": "chappa-ai control surface",
            // The address actually bound: the fixed one, or a profile's
            // OS-chosen port.
            "bind": state.identity.as_deref().map_or(BIND, |i| i.addr.as_str()),
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

fn identity_route(state: &ControlState, _: &Params, query: &str, _: &str) -> Resp {
    let Some(identity) = state.identity.as_deref() else {
        return json(
            500,
            json!({"error": "this server has no instance identity"}),
        );
    };
    let challenge = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("challenge="))
        .unwrap_or("");
    if !chappa_ai_mcp::identity::is_valid_challenge(challenge) {
        return json(
            400,
            json!({"error": "challenge must be 32 random bytes as lowercase hex"}),
        );
    }
    json(
        200,
        json!({
            "addr": identity.addr,
            "nonce": identity.nonce,
            "proof": identity.prove(challenge),
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

fn list_processes(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let mut rows = state.registry.list_control();
    if let Some(grant) = params.caller.grant() {
        let mine = state.registry.subtree(grant.process_id);
        rows.retain(|row| mine.contains(&row.base.id));
    }
    json(200, json!(rows))
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

fn spawn_terminal(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
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
    let mut env: Vec<(String, String)> = spec.env.into_iter().collect();
    let caller = params.caller.clone();
    // A terminal an agent opens is the agent's: it starts only where the
    // agent may start things (the launch gate), into a reserved id so its
    // identity env can name it, with a token of its own scoped under the
    // agent. The person's terminal starts as asked.
    let (at, origin, cwd, notice, guard) = match caller.grant() {
        None => (
            None,
            Origin::person(),
            spec.cwd.map(PathBuf::from),
            None,
            None,
        ),
        Some(grant) => {
            let gate = AppSpawnGate {
                projects: state.projects.clone(),
                settings: state.settings.clone(),
            };
            let (pass, notice) = match state.registry.gate_spawn(
                &gate,
                &GateRequest {
                    caller: &caller,
                    kind: SpawnKind::Terminal,
                    project_id: grant.project_id,
                    working_dir: spec.cwd.as_deref(),
                    launch_anyway: None,
                },
            ) {
                Ok(cleared) => cleared,
                Err(err) => return json(refusal_status(&err), json!({ "error": err })),
            };
            let id = state.registry.reserve_id();
            let token = match state.registry.tokens().mint(AgentGrant {
                process_id: id,
                project_id: grant.project_id,
                parent_process_id: Some(grant.process_id),
                tool: None,
                created_ms: term_core::actor::epoch_ms(),
            }) {
                Ok(token) => token,
                Err(err) => return json(500, json!({ "error": err })),
            };
            env.retain(|(k, _)| !IDENTITY_NAMES.contains(&k.as_str()));
            env.push(("CHAPPA_AI_PROCESS_ID".into(), id.to_string()));
            if let Some(project) = grant.project_id {
                env.push(("CHAPPA_AI_PROJECT_ID".into(), project.to_string()));
            }
            env.push((crate::caller::TOKEN_ENV.into(), token));
            let cwd = pass.cwd().map(Path::to_path_buf);
            (
                Some(id),
                Origin::with_pass(caller.clone(), pass),
                cwd,
                notice,
                Some(id),
            )
        }
    };
    let cfg = actor_config(
        PtySpec {
            command,
            args: spec.args,
            cwd,
            env,
            cols,
            rows,
        },
        None,
        &settings,
    );
    // No webview channel behind an agent-spawned terminal: agents read text
    // back through /output. The row still
    // shows up in /processes and list_terminals. It also emits
    // `term://created` so an app with an OPEN webview can adopt it into the
    // rail (the NullSink invisibility fix).
    match state
        .registry
        .create_terminal_as(cfg, Arc::new(NullSink), name.clone(), at, origin)
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
                        parent_process_id: caller.grant().map(|g| g.process_id),
                        record_id: None,
                        last_run: None,
                        record_kind: None,
                    },
                );
            }
            if let Some(notice) = &notice {
                crate::secret_scan::emit_notice(state.app.as_ref(), id, notice);
            }
            json(200, json!({"id": id, "name": name}))
        }
        Err(err) => {
            if let Some(id) = guard {
                state.registry.tokens().revoke(id);
            }
            match refusal_status(&err) {
                500 => json(500, json!({"error": format!("spawn failed: {err}")})),
                status => json(status, json!({ "error": err })),
            }
        }
    }
}

/// The identity variables the app sets for a row it spawned for an agent;
/// a request's own env cannot set them.
const IDENTITY_NAMES: &[&str] = &[
    "CHAPPA_AI_PROCESS_ID",
    "CHAPPA_AI_PROJECT_ID",
    "CHAPPA_AI_AGENT_TOOL_ID",
    crate::caller::TOKEN_ENV,
];

fn get_process(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    match state.registry.control_snapshot(id) {
        Some(row) => json(200, json!(row)),
        None => no_such_terminal(id),
    }
}

/// `key=<u64>` out of a raw query string (`None` when absent or unparsable).
fn query_u64(query: &str, key: &str) -> Option<u64> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
}

fn get_output(state: &ControlState, params: &Params, query: &str, _: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let (handle, screen_rows) = match (
        state.registry.handle(id),
        state.registry.control_snapshot(id),
    ) {
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
        return json(
            400,
            json!({"error": format!("lines and offset must be <= {MAX_WINDOW}")}),
        );
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

/// A delivery to an agent that data safety holds: queued (in order behind
/// any held before it) and made by `deliver` once the block clears. The
/// answer says so; nothing was written and nothing is journaled yet.
fn held(
    state: &ControlState,
    params: &Params,
    id: TermId,
    reason: String,
    text: &str,
    deliver: Box<dyn FnOnce() + Send>,
) -> Resp {
    let count = state
        .registry
        .hold_delivery(id, params.actor.clone(), text.to_owned(), deliver);
    json(
        200,
        json!({"written": false, "held": true, "held_count": count, "reason": reason}),
    )
}

/// The journal's name for an agent sender.
fn agent_actor(me: TermId) -> String {
    format!("agent:{me}")
}

/// The bytes an agent's text becomes in a pty: a sender line makes it
/// several lines, which go as one bracketed paste when the program turned
/// that mode on (a raw line break would submit the sender line alone).
fn agent_payload(registry: &Registry, id: TermId, text: &str, submit: bool) -> Vec<u8> {
    let bracketed = registry
        .handle(id)
        .and_then(|h| h.modes())
        .is_some_and(|m| m.bracketed_paste);
    if bracketed && text.contains('\n') {
        let mode = term_core::keys::ModeSnapshot {
            bracketed_paste: true,
            ..Default::default()
        };
        let mut bytes = term_core::keys::encode_paste(text, &mode);
        if submit {
            bytes.push(b'\r');
        }
        bytes
    } else {
        submit_payload(text, submit)
    }
}

/// How an agent's pty delivery is made once nothing holds it, re-judged
/// when it was held and what held it lifted: a choice key is dropped, and
/// a question on screen holds it again. When the person releases it, it is
/// made as it is: the person has seen it.
fn agent_pty_delivery(
    registry: Registry,
    id: TermId,
    me: TermId,
    raw: Vec<u8>,
    payload: Vec<u8>,
    shown: String,
    submit: bool,
) -> crate::delivery_hold::Deliver {
    Box::new(move |release| {
        if release == crate::delivery_hold::Release::Lifted {
            if crate::agent_delivery::is_choice_key(&raw) {
                log::info!("[control] a held choice key from {me} to {id} was dropped");
                return;
            }
            if let Some(question) = registry.confirmation_on_screen(id) {
                let again = agent_pty_delivery(
                    registry.clone(),
                    id,
                    me,
                    raw,
                    payload,
                    shown.clone(),
                    submit,
                );
                registry.hold_for_screen(
                    id,
                    screen_hold_reason(&question),
                    agent_actor(me),
                    shown,
                    again,
                );
                return;
            }
        }
        registry.write_journaled_as(id, &payload, shown, submit, Some(&agent_actor(me)));
    })
}

fn screen_hold_reason(question: &str) -> String {
    format!("held: {question} is on screen; the person releases or drops it on the row")
}

/// `/input` and `/bytes` from an agent to a pty: held while a question is
/// on screen (or data safety holds the target), else refused when it is
/// only a choice key, else written with the sender line when the target is
/// another agent. `raw` is the caller's bytes as sent.
#[allow(clippy::too_many_arguments)]
fn agent_send(
    state: &ControlState,
    me: TermId,
    id: TermId,
    raw: Vec<u8>,
    text: String,
    submit: bool,
    wrap: bool,
    wait_ms: Option<u64>,
) -> Resp {
    let (payload, shown) = if wrap {
        let name = state.registry.name_of(me).unwrap_or_default();
        let wrapped = crate::agent_delivery::wrap(&name, me, &text);
        (
            agent_payload(&state.registry, id, &wrapped, submit),
            wrapped,
        )
    } else {
        (raw.clone(), text)
    };
    let held_answer = |count: usize, entry: Option<u64>, reason: String| {
        json(
            200,
            json!({"written": false, "held": true, "held_count": count, "held_id": entry, "reason": reason}),
        )
    };
    if let Some(reason) = state.registry.hold_reason(id) {
        let deliver = agent_pty_delivery(
            state.registry.clone(),
            id,
            me,
            raw,
            payload,
            shown.clone(),
            submit,
        );
        let count = state.registry.hold_delivery(
            id,
            agent_actor(me),
            shown,
            Box::new(move || deliver(crate::delivery_hold::Release::Lifted)),
        );
        return held_answer(count, None, reason);
    }
    if let Some(question) = state.registry.confirmation_on_screen(id) {
        let reason = screen_hold_reason(&question);
        let deliver = agent_pty_delivery(
            state.registry.clone(),
            id,
            me,
            raw,
            payload,
            shown.clone(),
            submit,
        );
        let (entry, count) =
            state
                .registry
                .hold_for_screen(id, reason.clone(), agent_actor(me), shown, deliver);
        return held_answer(count, Some(entry), reason);
    }
    if crate::agent_delivery::is_choice_key(&raw) {
        return json(
            403,
            json!({"error": crate::agent_delivery::CHOICE_KEY_REFUSAL}),
        );
    }
    deliver_as(
        state,
        id,
        &payload,
        shown,
        submit,
        wait_ms,
        Some(&agent_actor(me)),
    )
}

/// Whether text from agent `me` to row `id` carries the sender line: it
/// goes into ANOTHER agent's terminal.
fn needs_sender_line(state: &ControlState, me: TermId, id: TermId) -> bool {
    id != me && state.registry.is_agent(id)
}

fn send_input(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let body: InputBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    let me = params.caller.grant().map(|g| g.process_id);
    let hold = state.registry.hold_reason(id);
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
            return json(
                400,
                json!({"error": "text is empty: a json-transport agent takes whole messages"}),
            );
        }
        let mut text = body.text.clone();
        if let Some(me) = me {
            // A message has no screen to ask questions on; the bare-answer
            // and sender-line rules still hold.
            if crate::agent_delivery::is_choice_key(text.as_bytes()) {
                return json(
                    403,
                    json!({"error": crate::agent_delivery::CHOICE_KEY_REFUSAL}),
                );
            }
            if needs_sender_line(state, me, id) {
                let name = state.registry.name_of(me).unwrap_or_default();
                text = crate::agent_delivery::wrap(&name, me, &text);
            }
        }
        if let Some(reason) = hold {
            let registry = state.registry.clone();
            let queued = text.clone();
            return held(
                state,
                params,
                id,
                reason,
                &text,
                Box::new(move || {
                    if let Err(e) = json_send_input(&registry, id, &queued, None) {
                        log::warn!("[control] a held message to {id} was not delivered: {e}");
                    }
                }),
            );
        }
        return match json_send_input(&state.registry, id, &text, body.wait_ms) {
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
    if let Some(me) = me {
        let wrap = needs_sender_line(state, me, id);
        let typed = submit_payload(&body.text, submit);
        return agent_send(state, me, id, typed, body.text, submit, wrap, body.wait_ms);
    }
    let payload = submit_payload(&body.text, submit);
    if let Some(reason) = hold {
        let registry = state.registry.clone();
        let text = body.text.clone();
        return held(
            state,
            params,
            id,
            reason,
            &body.text,
            Box::new(move || {
                registry.write_journaled(id, &payload, text, submit);
            }),
        );
    }
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
        None => json(
            409,
            json!({"error": format!("process {id} is not a json-transport agent")}),
        ),
    }
}

fn send_bytes(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let id = params.id.unwrap_or_default();
    let body: BytesBody = match serde_json::from_str(body) {
        Ok(body) => body,
        Err(err) => return json(400, json!({"error": format!("bad body: {err}")})),
    };
    if state.registry.json_agent(id).is_some() {
        return json(
            409,
            json!({"error": format!("process {id} is a json-transport agent: it takes messages via /input, not raw bytes")}),
        );
    }
    let text = String::from_utf8_lossy(&body.bytes).into_owned();
    if let Some(me) = params.caller.grant().map(|g| g.process_id) {
        // Raw bytes cannot carry a sender line: into another agent's
        // terminal an agent sends text, through /input.
        if needs_sender_line(state, me, id) {
            return json(
                403,
                json!({"error": format!("forbidden: raw bytes from an agent cannot go into another agent's terminal (process {id}); send text with /input, which names the sender")}),
            );
        }
        return agent_send(state, me, id, body.bytes, text, false, false, body.wait_ms);
    }
    if let Some(reason) = state.registry.hold_reason(id) {
        let registry = state.registry.clone();
        let (bytes, shown) = (body.bytes.clone(), text.clone());
        return held(
            state,
            params,
            id,
            reason,
            &text,
            Box::new(move || {
                registry.write_journaled(id, &bytes, shown, false);
            }),
        );
    }
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
    deliver_as(state, id, payload, text, submit, wait_ms, None)
}

/// [`deliver`], the journal naming `actor`.
fn deliver_as(
    state: &ControlState,
    id: TermId,
    payload: &[u8],
    text: String,
    submit: bool,
    wait_ms: Option<u64>,
    actor: Option<&str>,
) -> Resp {
    let wait_ms = wait_ms
        .unwrap_or(RECEIPT_WAIT_DEFAULT_MS)
        .min(RECEIPT_WAIT_MAX_MS);
    let handle = match state.registry.handle(id) {
        Some(handle) => handle,
        None => return no_such_terminal(id),
    };
    let seq_before = handle.io().output_bytes();
    if !state
        .registry
        .write_journaled_as(id, payload, text, submit, actor)
    {
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
    // registry close would leave the runtime pointing at a dead id.
    // A store that is not initialized (tests) owns nothing.
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
            row["command"] = json!(t.command_line_without_env_values());
            row["container"] = json!(t.runtime.container());
            // A tool's env values are often its credentials (API keys,
            // tokens), and every token holder can read this route: the
            // names say what the tool is given without handing it out.
            row["env"] = json!(t.env.keys().collect::<Vec<_>>());
            row
        })
        .collect()
}

fn list_agent_tools(state: &ControlState, _: &Params, _: &str, _: &str) -> Resp {
    json(200, json!(agent_tool_rows(state)))
}

/// A spawn request as the control surface (and so MCP) can send it. The
/// internal fields (a source's journal actor, a resume, the launch dialog's
/// "Launch anyway") are not on the wire: whatever the body says about them
/// is dropped here, so a scan that did not finish can only be passed by a
/// person in the app's own dialog.
fn parse_spawn_body(body: &str) -> Result<SpawnAgentRequest, String> {
    serde_json::from_str(body).map_err(|err| format!("bad body: {err}"))
}

fn spawn_agent(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let req = match parse_spawn_body(body) {
        Ok(req) => req,
        Err(err) => return json(400, json!({ "error": err })),
    };
    // The SPAWNING actor — `X-Chappa-Actor` parsed as a numeric
    // process id. A non-numeric / `user` actor → `None` (a root spawn). This
    // is the default parent candidate the spawn validates against a live
    // entry; the rail nests the child under it.
    let actor = match params.caller.grant() {
        Some(grant) => Some(grant.process_id),
        None => params.actor.parse::<TermId>().ok(),
    };
    let ctx = SpawnContext {
        registry: &state.registry,
        tools: &state.agent_tools,
        settings: &state.settings,
        projects: &state.projects,
        records: &state.records,
        actor,
        caller: params.caller.clone(),
    };
    match spawn_agent_headless(ctx, req) {
        Ok(resp) => {
            // spawn_agent_with's own emit
            // is gated on ITS on_notify, which this headless route
            // deliberately leaves None — so MCP/control-spawned agents (the
            // exact invisibility the adopt path exists to fix) never broadcast.
            // Emit here off the control server's app handle, mirroring
            // spawn_terminal above. Found live: an orchestrating agent's
            // first spawn after the adopt path landed still drew no rail row.
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
                        record_id: resp.record_id.clone(),
                        last_run: state.registry.run_head(resp.process_id),
                        record_kind: None,
                    },
                );
            }
            json(200, json!(resp))
        }
        Err(err) if err.starts_with("no such") => json(404, json!({"error": err})),
        Err(err) if err.starts_with(crate::agent_tools::AGENT_ARGS_REFUSAL_LEAD) => {
            json(400, json!({"error": err}))
        }
        Err(err) if err == crate::agent_delivery::CHOICE_KEY_REFUSAL => {
            json(403, json!({"error": err}))
        }
        Err(err)
            if err.starts_with("refused")
                || err.starts_with(crate::secret_scan::BLOCK_PREFIX)
                || err.contains("disabled")
                || crate::data_safety::is_refusal(&err) =>
        {
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
        // The project's content is not trusted: only the person can change
        // that, in the app's trust prompt.
        Err(err @ ControlError::Untrusted(_)) => json(409, json!({"error": err.to_string()})),
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
    let (project, name) = (
        params.project.unwrap_or_default(),
        params.name.clone().unwrap_or_default(),
    );
    lifecycle_response(state.projects.control_start(project, &name, wait_ms))
}

fn stop_process(state: &ControlState, params: &Params, _: &str, _: &str) -> Resp {
    let (project, name) = (
        params.project.unwrap_or_default(),
        params.name.clone().unwrap_or_default(),
    );
    lifecycle_response(state.projects.control_stop(&state.registry, project, &name))
}

fn restart_process(state: &ControlState, params: &Params, _: &str, body: &str) -> Resp {
    let wait_ms = match parse_wait(body) {
        Ok(w) => w,
        Err(resp) => return resp,
    };
    let (project, name) = (
        params.project.unwrap_or_default(),
        params.name.clone().unwrap_or_default(),
    );
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
/// clean 400 rather than an OOM.
fn read_body(reader: &mut dyn std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = reader.take(MAX_BODY_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Upper bound on a request body (1 MiB). The largest legitimate payload is a
/// `send_bytes` array or a paste-sized `send_input`, orders of magnitude below.
pub const MAX_BODY_BYTES: u64 = 1 << 20;

/// A JSON response with an explicit status code and content-type header.
/// Shared with the route modules that answer under this server.
pub fn json(status: u16, value: Value) -> Resp {
    let mut response =
        tiny_http::Response::from_data(serde_json::to_vec(&value).unwrap_or_default())
            .with_status_code(status);
    response.add_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .unwrap_or_else(|_| unreachable!("literal header bytes")),
    );
    response
}

// ---- doc emitter -------------------------------------------------------------

/// The `## Callers` section: the two kinds of token and how far each
/// reaches. The table's "Who" column is [`Access::label`].
pub const CALLERS_DOC: &str = "## Callers: the person and agents\n\n\
Two kinds of token are accepted.\n\n\
- **The person's token**: `<app-config>/control_token`. Full authority over every route, for the \
person's own tools and scripts. `X-Chappa-Actor` attributes a call among them (absent = `user`). The app \
never puts it in an agent's environment, MCP configuration or command line.\n\
- **Agent tokens**: one per agent the app spawns, and one per terminal an agent opens. 32 random bytes \
held only in the app's memory (only a digest of each), never written to disk, delivered to the child in \
`CHAPPA_AI_CONTROL_TOKEN` (a docker-exec tool gets it by name with `-e`, never on the command line), and \
revoked when the row exits or closes (a started-again or resumed row gets a new one). `chappa-ai-mcp` \
prefers that variable over the token file. For an agent token the server sets the actor from the token \
(the row's process id) and ignores `X-Chappa-Actor`; records, the input journal (`agent:<id>`), \
coordination writes and timers show that actor.\n\n\
What an agent token reaches (the Who column): **own subtree** = the agent's own row and every row it \
spawned with its token, transitively (anything else answers 403); **own subtree's records** = records whose \
live row is in the subtree or whose recorded parents lead back to the agent's own record; **own project** = \
the project its row works in; **project-level read** = allowed, with secrets redacted (tool env values are \
never served); **person only** = 403 for an agent token. `GET /processes` lists an agent's own subtree. \
A spawn by an agent (`POST /processes`, `POST /agents`) must start inside its own project's roots (no \
folder = the project root; an agent with no project must name a folder inside an open project; another \
project is refused) and passes the data-safety and secret-scan checks with no way past them; an agent's \
`extra_args` must be words on the tool's allowlist unless the person turned on \"agents may pass any \
arguments\" for that tool (Settings ▸ Agents). Text an agent sends (`/input`, `/bytes`, timer bodies, a typed \
first prompt) is held while the target's screen shows a confirmation-shaped state (a known startup question, \
a numbered choice, a `(y/n)` question, an Allow / Deny choice) — the answer is `{written: false, held: true, \
held_id, reason}` and the person releases or drops it on the row; a body that is only a choice key (`1`–`9`, \
`y`, `n`, `a`, Enter, Escape, arrow keys) is refused (403); into another agent's terminal it goes with a \
first line `[from <row name> (process <id>) · agent]` that the body cannot forge (leading lines of that \
shape are removed from it first), and raw `/bytes` into another agent's terminal are refused. \
Settings, trust and acknowledgements are not on this surface at all.\n\n\
**This boundary is complete only for agents that cannot read the person's files** — containerised \
(docker-exec) tools that see only their own workspace. An agent running unsandboxed on the host, with a \
shell, as the same OS user, can still read `<app-config>/control_token` and act as the person, and can edit \
the app's own files. Nothing here can tell such an agent from the person. A guard rail for host Claude \
Code tools: each launch is given a settings file of its own (owner-only, in the app's data folder, removed \
when the row exits) whose `permissions.deny` holds `Read`/`Edit` rules for every folder with the app's \
state and every other open project's folder and worktree, so a permission-limited agent is refused there by \
its own CLI. It does not hold for a tool started with its permissions bypassed or in auto mode, nor for a \
script that opens files itself; Settings ▸ Agents shows per tool which applies.\n\n";

/// `desktop/CONTROL_API.md`, rendered from [`ROUTES`]. The test
/// `control_api_doc_is_in_sync` fails when the checked-in file differs;
/// regenerate with `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop
/// control_api_doc`.
pub fn render_api_doc() -> String {
    let mut out = String::new();
    out.push_str("# chappa-ai control API\n\n");
    out.push_str(
        "GENERATED from `desktop/src-tauri/src/control_http.rs::ROUTES` — do not edit by hand.\n",
    );
    out.push_str(
        "Regenerate: `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop control_api_doc`.\n\n",
    );
    out.push_str(&format!(
        "Bound to `http://{BIND}` in every build; a run under `CHAPPA_PROFILE_DIR` binds\n\
         `127.0.0.1` on a port the OS chooses instead and publishes it only in that profile's\n\
         endpoint file (below). Every request must carry\n\
         `Authorization: Bearer <token>` where the token is the contents of\n\
         `<app-config>/control_token` (created at first launch, owner-only permissions;\n\
         `<app-config>` is Tauri's app config dir for `com.chappa-ai.desktop`, e.g.\n\
         `%APPDATA%\\com.chappa-ai.desktop` on Windows, or `<profile>/config` when the app runs\n\
         with `CHAPPA_PROFILE_DIR=<profile>`). Responses are JSON; errors are\n\
         `{{\"error\": \"…\"}}` with a 4xx/5xx status. `GET /` returns this same table.\n\n\
         The one exception to the token rule is `GET /identity`: anything can bind this\n\
         port before the app does, so a client should verify the server before it sends\n\
         the token. After each successful bind the app writes the owner-only\n\
         `<app-config>/control_endpoint` = `{{\"addr\", \"nonce\", \"pid\"}}`; a client reads it,\n\
         connects to `addr`, sends `GET /identity?challenge=<fresh random hex>` with no\n\
         credentials, checks the proof against its own token and the returned\n\
         addr/nonce against the file, and only then sends its authenticated request on\n\
         the same connection (`chappa-ai-mcp` does exactly this and refuses otherwise).\n\n\
         Path placeholders: `{{id}}` = terminal id, `{{project}}` = project id, `{{name}}` =\n\
         percent-encoded process name, `{{rid}}` = scratchpad / todo / comment id, `{{record_id}}` =\n\
         a durable row record's uuid.\n\
         Fields marked `?` are optional; POST fields are JSON body keys, GET fields are\n\
         query parameters (comma-separated for list fields).\n\n\
         This is the MCP server's transport (`chappa-ai-mcp`) and a curl interface —\n\
         not a public API. Localhost only, no CLI binary.\n\n"
    ));
    out.push_str(CALLERS_DOC);
    out.push_str("| Method | Path | Who | Fields | Description |\n|---|---|---|---|---|\n");
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
            "| `{}` | `{}` | {} | {} | {} |\n",
            r.method,
            r.path,
            r.access.label(),
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
         rail listing also reports. Process stats (cpu/mem/subprocess counts) are not in\n\
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
         (`seq_before`/`seq_after` are event-ring cursors there, `had_output_within_ms` = delivered).\n\n\
         **Program status.** Any process can say what it is doing by printing\n\
         `OSC 777;status;<working|blocked|done|failed|idle>` (BEL or ST terminated; only the exact\n\
         lowercase word counts): `printf '\\033]777;status;blocked\\007'`. The row's `program_status`\n\
         carries the last report (null when it never reported, and cleared when the run exits), and a\n\
         change broadcasts `term://program_status {term_id, status}`. `blocked` is attention at the\n\
         agent-waiting rank and HOLDS deliveries through the ready gate (timer bodies and a queued spawn\n\
         prompt) until the program reports anything else; at the cap the receipt says `reason: blocked`.\n\
         `done` / `failed` badge the row like a bell until it is viewed; `working` / `idle` clear both.\n\
         Raw `send_input` / `send_bytes` writes are not held.\n",
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
         Trust model: with the person's token `X-Chappa-Actor` is UNAUTHENTICATED attribution — any\n\
         of the person's tools can claim any actor id. With an agent token the actor is the agent's\n\
         own row, set from the token. Todo locks are advisory coordination either way (a courtesy\n\
         that prevents lost updates), and scratchpads and todos are shared by every caller.\n",
    );
    out.push_str("\n## Timers\n\n");
    out.push_str(crate::timers_http::DOC);
    out.push_str("\n## Records\n\n");
    out.push_str(records::DOC);
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
        assert!(
            v["message"].as_str().unwrap().contains("boom in a route"),
            "{body}"
        );
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
    fn a_spawn_body_cannot_launch_past_an_unfinished_scan() {
        let note = "launch it anyway";
        for field in [
            "launch_anyway",
            "launchAnyway",
            "LaunchAnyway",
            "launch-anyway",
        ] {
            let body = json!({ "agent_tool_id": 1, field: note }).to_string();
            let req = parse_spawn_body(&body).unwrap();
            assert_eq!(req.launch_anyway, None, "{field}");
        }
        // Nor the other internal fields.
        let body = json!({
            "agent_tool_id": 1,
            "record_id": "r1",
            "journal_actor": "source:x",
            "resume": { "session_id": "s" },
        })
        .to_string();
        let req = parse_spawn_body(&body).unwrap();
        assert_eq!(
            (req.record_id, req.journal_actor, req.resume.is_none()),
            (None, None, true)
        );
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
        let cases: [(ControlError, u16); 4] = [
            (ControlError::NotFound("no such project: 9".into()), 404),
            (ControlError::NotOpen("project 9 is not open".into()), 404),
            (ControlError::Untrusted("not trusted: `web`".into()), 409),
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
