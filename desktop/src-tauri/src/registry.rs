//! Terminal registry + event pump. Owns every live terminal the
//! app spawned and the one thread that forwards each terminal's `TermEvent`s
//! to the frontend.
//!
//! Shape: `State<Registry>` = a mutex-protected `HashMap<TermId, Entry>`.
//! `TermId` is a monotonic u32. Each `Entry` holds the `TermHandle` (the only
//! way to drive the actor), the pump's join handle, and the metadata the
//! debug HTTP surface reads back out (status/cols/rows/seq/event ring).
//!
//! The pump is the only piece that touches the outside world per terminal.
//! It sits between the actor's `TermEvent` channel and an [`EventSink`] — the
//! 2-method trait that abstracts the Tauri Channel (binary frames) and the
//! global `term://*` event emit (JSON events). Tests implement the trait with
//! a mock, so the pump is unit-testable without a Tauri runtime.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use project_model::settings::Settings;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tauri_plugin_clipboard_manager::ClipboardExt;
use term_core::actor::{
    epoch_ms, spawn_actor, spawn_actor_nopty, ActorConfig, FrameKind, MarkKind, TermEvent,
    TermHandle,
};

use crate::agent_json::JsonAgent;
use crate::event_ring::{EventRing, Sequenced};
use term_core::frame::encode_frame;
use term_core::pty::PtySpec;
use term_core::status::{
    transition, ActivityLimiter, ProcessStatus, StatusEvent, TerminalSnapshot,
};

use crate::agents::{
    kill_container_side, remove_pid_file, AgentMeta, Bridge, CloseVerification, ContainerState,
    DockerCli, ProbeTarget, RealDocker, KILL_GRACE, SHUTDOWN_CAP, SHUTDOWN_PER_CONTAINER,
    TERM_GRACE,
};
use crate::stats::{StatsTarget, StatsWaker};

pub type TermId = u32;

/// Scrollback every spawn path uses unless the caller asks for another size
/// (only `create_terminal`'s optional argument ever does).
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// The ONE place an [`ActorConfig`] is assembled. Every spawn path — the
/// interactive `create_terminal` command, project processes, the control
/// surface's `POST /processes`, the debug parity harness — must agree on the
/// scrollback size and on the per-actor controls seeded at spawn time (both
/// live-settable actor controls are also seeded at SPAWN).
///
/// `settings: None` means the built-in defaults, which is what the
/// parity harness needs: it asserts behavior at the defaults and
/// must never read the user's settings.json.
pub fn actor_config(
    spec: PtySpec,
    scrollback_lines: Option<usize>,
    settings: Option<&Settings>,
) -> ActorConfig {
    let defaults = ActorConfig::default();
    ActorConfig {
        spec,
        scrollback_lines: scrollback_lines.unwrap_or(DEFAULT_SCROLLBACK_LINES),
        synthetic_prompt_marks: settings
            .map_or(defaults.synthetic_prompt_marks, |s| s.synthetic_prompt_marks),
        wheel_speed: settings.map_or(defaults.wheel_speed, |s| s.scroll_wheel_speed),
    }
}

/// Called by the pump when a terminal's child exits: `(term_id, exit_code,
/// success)`. The project runner uses this to drive `auto_restart`
/// on its processes; plain shells leave it `None`.
pub type ExitHandler = Arc<dyn Fn(TermId, Option<i32>, bool) + Send + Sync>;

/// One outbound JSON event. `event` is the wire kind (`term://title`, …) and
/// also the Tauri global event name the frontend listens on; `seq` is the
/// per-terminal monotonic stream position (frames included), so the debug
/// `/events` `since` filter is a stable cursor.
#[derive(Debug, Clone, Serialize)]
pub struct JsonEvent {
    pub event: &'static str,
    pub term_id: TermId,
    pub seq: u64,
    #[serde(flatten)]
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// The 2-method sink the pump writes through. [`ChannelSink`] maps it onto
/// real IPC; tests use a mock.
pub trait EventSink: Send + Sync + 'static {
    /// Binary frame bytes (`encode_frame` output) → the frontend channel.
    fn send_binary(&self, bytes: Vec<u8>);
    /// JSON event → the app-wide `term://*` event.
    fn emit_json(&self, event: JsonEvent);
}

/// The real sink: binary frames over the per-terminal `frames` Channel, JSON
/// events emitted app-wide under their `term://…` name. Built inside a
/// command, where the AppHandle and the Channel are both available.
pub struct ChannelSink {
    frames: Channel<InvokeResponseBody>,
    app: AppHandle,
}

impl ChannelSink {
    pub fn new(frames: Channel<InvokeResponseBody>, app: AppHandle) -> Self {
        Self { frames, app }
    }
}

impl EventSink for ChannelSink {
    fn send_binary(&self, bytes: Vec<u8>) {
        let _ = self.frames.send(InvokeResponseBody::Raw(bytes));
    }

    fn emit_json(&self, event: JsonEvent) {
        let _ = self.app.emit(event.event, &event);
    }
}

/// Sink for harness-created terminals (debug HTTP): no webview channels, the
/// harness reads text via `TermHandle::dump_text` instead.
pub struct NullSink;

impl EventSink for NullSink {
    fn send_binary(&self, _bytes: Vec<u8>) {}
    fn emit_json(&self, _event: JsonEvent) {}
}

impl Sequenced for JsonEvent {
    fn seq(&self) -> u64 {
        self.seq
    }
}

/// One `send_input` / `send_bytes` delivery record: the
/// audit trail that answers "did my send land" without screen-scraping.
/// `text` is the UTF-8 form for text sends and the lossy rendering for raw
/// byte sends; `bytes_len` is always the exact payload size written.
#[derive(Debug, Clone, Serialize)]
pub struct InputRecord {
    /// Unix-epoch ms at the write.
    pub ts: u64,
    pub bytes_len: usize,
    pub text: String,
    pub submit: bool,
}

/// Bound on the per-process input journal ("last 100").
pub const INPUT_JOURNAL_CAP: usize = 100;

/// One process row for the control surface: the debug
/// snapshot fields plus byte-stream liveness straight off the actor's
/// `IoCounters`. `has_output == false` = still booting; `child_alive == false`
/// = the child is gone (exit observed) even if the row still says running
/// for a beat.
///
/// STATS HOOK: when process stats land, `cpu_pct` / `mem_bytes` /
/// `subproc_count` ride along HERE (optional fields, `null` until the first
/// poll) — the control surface is the only consumer that needs them
/// serialized, and the route table below already documents the slot. Not
/// implemented.
#[derive(Debug, Clone, Serialize)]
pub struct ControlSnapshot {
    /// The rail/debug row (`id, name, status, exit_code, cols, rows, seq`),
    /// built by the SAME `Entry` → [`TerminalSnapshot`] helper `Registry::list`
    /// uses, and flattened so the two listings can never disagree about a
    /// shared field. The wire shape is byte-identical to the hand-copied
    /// struct it replaced — same keys, same order (flatten emits the base
    /// fields where the field is declared), same lowercase status strings —
    /// which `CONTROL_API.md` and the parity harness depend on.
    #[serde(flatten)]
    pub base: TerminalSnapshot,
    /// Monotonic count of pty bytes the reader delivered (byte flow, not
    /// render). The `send_input` receipt's `seq_before`/`seq_after`.
    pub output_bytes: u64,
    /// Unix-epoch ms of the last pty byte; `null` while booting.
    pub last_output_at: Option<u64>,
    pub has_output: bool,
    pub child_alive: bool,
    /// `"shell"` or `"agent"`.
    pub kind: &'static str,
    /// The agent block (tool, model, runtime, container, bridge,
    /// close verification) — `null` for plain terminals.
    pub agent: Option<AgentMeta>,
    /// The STABLE identity of this spawn. Numeric ids restart
    /// at 1 on every launch, so anything that outlives the process table (a
    /// persisted timer target) must pin the uuid and treat a mismatch as
    /// "process gone" rather than re-binding to whatever reused the number.
    /// An agent's uuid is its `CHAPPA_AI_SPAWN_UUID`.
    pub uuid: String,
    /// The project that owns this process (a project process or an agent
    /// spawned into a project); `null` for a bare terminal.
    pub project_id: Option<u32>,
    /// The spawn's `close_on_exit` lifecycle flag, read-only — lets
    /// an orchestrator see which rows will self-reap when their child exits.
    /// `agent.parent_process_id` (the spawner) rides on the `agent` block, so
    /// the `/processes` row exposes both nesting facts.
    pub close_on_exit: bool,
}

/// The byte-stream liveness of one entry, read for many ids under ONE
/// registry lock ([`Registry::liveness_many`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRow {
    pub uuid: String,
    pub name: String,
    pub project_id: Option<u32>,
    pub child_alive: bool,
    pub last_output_at: Option<u64>,
    pub output_bytes: u64,
}

/// The app-wide `term://created` broadcast. Emitted by every
/// successful spawn PATH (never from inside `Registry::create_terminal*`
/// itself — the parity harness and the debug 8323 surface create via
/// the registry and are EXCLUDED by design: they are test scaffolding). The
/// webview adopts an id it does not know into a rail row from this.
pub const TERM_CREATED: &str = "term://created";

/// One `term://created` payload. `kind` is `terminal | agent | process`; the
/// binding facts (`project_id`, `agent_tool_id`) let the frontend drop the
/// row into the right rail section without a second round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedEvent {
    pub term_id: TermId,
    pub name: String,
    pub kind: &'static str,
    pub project_id: Option<u32>,
    pub agent_tool_id: Option<u32>,
    /// The creating process's registry id — what the rail nests this
    /// row under (the child of a spawned agent). `None` for a root.
    pub parent_process_id: Option<TermId>,
}

/// The `term://created` broadcast seam. The real impl forwards to
/// the Tauri app handle; tests use a recording fake — the "fake app handle
/// seam", the same injection discipline as
/// `FakeDocker`/`FakeControlServer` elsewhere.
pub trait CreatedBroadcast: Send + Sync + 'static {
    fn created(&self, event: &CreatedEvent);
}

/// The real broadcast: `term://created` via the tauri `Emitter` on the app
/// handle. The webview listens on this to adopt backend-spawned terminals
/// (MCP/control `spawn_terminal`, `spawn_agent`, project processes +
/// auto-restarts).
pub struct AppCreatedBroadcast(pub AppHandle);

impl CreatedBroadcast for AppCreatedBroadcast {
    fn created(&self, event: &CreatedEvent) {
        let _ = self.0.emit(TERM_CREATED, event);
    }
}

/// Fire `term://created` through a [`CreatedBroadcast`]. A thin convenience
/// so call sites read "emit_created(...)" instead of the raw trait call;
/// the trait object is what makes the spawn paths unit-testable without a
/// Tauri runtime.
pub fn emit_created(broadcast: &dyn CreatedBroadcast, event: CreatedEvent) {
    broadcast.created(&event);
}

/// The app-wide `term://closed` broadcast — the mirror of
/// `term://created`, emitted by [`Registry::close`] for EVERY close (the
/// frontend's ×, control/MCP `close_terminal`, project stop, `close_on_exit`).
/// This is the gap that lets an MCP-side `close_terminal` leave a STALE rail
/// row today; the frontend drops the row off this event. Debug-harness (8323)
/// closes go through [`Registry::close_silent`] and are EXCLUDED, as with
/// created.
pub const TERM_CLOSED: &str = "term://closed";

/// The `reason` vocabulary for a [`ClosedEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// A manual/ordinary close: the frontend ×, control/MCP `close_terminal`,
    /// project stop, respawn, shutdown.
    Closed,
    /// The entry closed itself because its child process exited with
    /// the spawn's `close_on_exit` flag set.
    CloseOnExit,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::Closed => "closed",
            CloseReason::CloseOnExit => "close_on_exit",
        }
    }
}

/// One `term://closed` payload: `{term_id, reason: "closed" | "close_on_exit"}`.
#[derive(Debug, Clone, Serialize)]
pub struct ClosedEvent {
    pub term_id: TermId,
    pub reason: &'static str,
}

/// The `term://closed` broadcast seam — the created seam's mirror
/// (real impl forwards to the Tauri app handle; tests use a recording fake).
pub trait ClosedBroadcast: Send + Sync + 'static {
    fn closed(&self, event: &ClosedEvent);
}

/// The real `term://closed` broadcast via the tauri `Emitter`.
pub struct AppClosedBroadcast(pub AppHandle);

impl ClosedBroadcast for AppClosedBroadcast {
    fn closed(&self, event: &ClosedEvent) {
        let _ = self.0.emit(TERM_CLOSED, event);
    }
}

/// A no-op `term://closed` seam, used where nothing may see a close (the
/// debug-harness route and the default registry).
pub struct NullClosedBroadcast;

impl ClosedBroadcast for NullClosedBroadcast {
    fn closed(&self, _: &ClosedEvent) {}
}

/// Fire `term://closed` through a [`ClosedBroadcast`] (the [`emit_created`]
/// mirror).
pub fn emit_closed(broadcast: &dyn ClosedBroadcast, event: ClosedEvent) {
    broadcast.closed(&event);
}

/// The RICH rail row the frontend reconciles/adopts from. `base` is
/// the exact `TerminalSnapshot` (so the flattened wire shape of the shared
/// fields is byte-identical to `Registry::list`), plus the kind/binding facts
/// the frontend needs to place a row. `attach_terminal` answers the same
/// shape. The remedy, in one sentence: this is the primitive the
/// dock window builds on — the frames channel of an EXISTING terminal is
/// hand-off-able to any webview, not only the one that spawned it.
#[derive(Debug, Clone, Serialize)]
pub struct RailRow {
    #[serde(flatten)]
    pub base: TerminalSnapshot,
    /// `"terminal" | "agent" | "process"` — the created-event vocabulary.
    pub kind: &'static str,
    pub project_id: Option<u32>,
    pub agent_tool_id: Option<u32>,
    /// Mirror of the created-event/agent parent — the rail nests this
    /// row under whichever live row has this id (a spawned agent's child).
    pub parent_process_id: Option<TermId>,
}

/// The registry's projection of one agent for the pure guards:
/// busy counting per tool / per container, and the delete refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: TermId,
    pub name: String,
    pub tool_id: u32,
    pub container: Option<String>,
    pub child_alive: bool,
    pub last_output_at: Option<u64>,
    /// Unix ms at spawn: a child inside the busy window counts as busy
    /// before its first byte.
    pub spawned_at_ms: u64,
    /// The spawn uuid (the identity the reaper matches container
    /// markers against — a live row's uuid is never an orphan). Missing here,
    /// the sweep could not tell a LIVE row from an orphan.
    pub spawn_uuid: String,
}

/// What a close reports. `verification` is `None` for a plain
/// terminal — only docker-exec agents have a container-side process to
/// verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseOutcome {
    pub verification: Option<CloseVerification>,
}

/// One live terminal: the actor handle, the pump thread, and the metadata the
/// shell layer reads back out.
struct Entry {
    handle: TermHandle,
    event_pump: Option<JoinHandle<()>>,
    name: String,
    status: ProcessStatus,
    exit_code: Option<i32>,
    cols: u16,
    rows: u16,
    /// Sequence number of the last binary frame pushed (debug listing).
    seq: u32,
    /// OS pid of the pty child — the root of the stats tree walk. `None`
    /// for a no-pty actor (the headless test path).
    pid: Option<u32>,
    events: EventRing<JsonEvent>,
    /// Fired by the pump on child exit (auto_restart hook).
    on_exit: Option<ExitHandler>,
    /// The bounded delivery audit for `send_input` / `send_bytes`.
    input_journal: VecDeque<InputRecord>,
    /// Set for agent spawns (`kind: "agent"`).
    agent: Option<AgentMeta>,
    /// Random per spawn (the agent's spawn uuid for agents).
    uuid: String,
    /// The owning project, when a project process is started into a project
    /// ([`Registry::set_project`]); agents carry theirs in `agent`.
    project_id: Option<u32>,
    /// The SHARED sink state the pump reads on every event, so a
    /// mid-life swap ([`Registry::attach_terminal`]) takes effect for the very
    /// next event the actor emits. This is the dock-window primitive:
    /// the frames channel of a terminal is a hand-off, not a birthright.
    sink: Arc<Mutex<Arc<dyn EventSink>>>,
    /// True from an attach until the actor's forced
    /// FULL flows — a delta emitted between the swap and the actor handling
    /// `request_full` would otherwise race ahead of the FULL onto the fresh
    /// subscriber's EMPTY decoder. The pump holds BINARY delivery while set;
    /// registry state (seq, status) still advances.
    sink_awaiting_full: Arc<AtomicBool>,
    /// The spawn's `close_on_exit` lifecycle instruction — when the
    /// child EXITS, the pump closes this entry through the same path
    /// `close_terminal` uses. Stored HERE (not on `AgentMeta`: it is a
    /// lifecycle instruction, not an agent fact), surfaced read-only on the
    /// `/processes` row.
    close_on_exit: bool,
}

struct Inner {
    entries: HashMap<TermId, Entry>,
    next_id: TermId,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }
}

/// Clonable app state (managed by Tauri). Commands borrow it via `State`; the
/// debug HTTP server thread keeps its own clone.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
    /// Released when a terminal is created, so the stats poller can
    /// park (cost nothing at all) while there is nothing to sample.
    waker: Arc<StatsWaker>,
    /// Released when a docker-exec agent is spawned, so the bridge
    /// probe can park while there is nothing to inspect.
    bridge_waker: Arc<StatsWaker>,
    /// The ONE docker seam — real CLI in the app, a scripted fake
    /// in tests. Every close/probe/stats call goes through it.
    docker: Arc<dyn DockerCli>,
    /// Held by an agent spawn from its busy-guard check until its
    /// entry exists, so two spawns can never both pass a `max_busy` limit
    /// off the same "0 busy" reading. Separate from `inner` on purpose — the
    /// spawn talks to docker and the pty while holding it.
    spawn_guard: Arc<Mutex<()>>,
    /// The json-transport agents (piped child + event ring)
    /// keyed by their entry id. Separate from `inner` so a send/receipt wait
    /// never holds the registry lock; `close` / `shutdown_all` kill them
    /// before the fronting actor goes.
    json_agents: Arc<Mutex<HashMap<TermId, Arc<JsonAgent>>>>,
    /// The `term://closed` broadcast seam, set once at app setup to
    /// the real app handle (so EVERY close — including the backend-initiated
    /// `close_on_exit` and MCP `close_terminal` — reaches the webview). Tests
    /// swap in a recording fake; the default ([`NullClosedBroadcast`]) stays
    /// silent so harness/test closes emit nothing.
    closed_broadcast: Arc<Mutex<Arc<dyn ClosedBroadcast>>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_docker(Arc::new(RealDocker))
    }
}

impl Registry {
    /// A registry over an explicit docker seam (tests: `FakeDocker`).
    pub fn with_docker(docker: Arc<dyn DockerCli>) -> Self {
        Self {
            inner: Arc::default(),
            waker: Arc::default(),
            bridge_waker: Arc::default(),
            docker,
            spawn_guard: Arc::default(),
            json_agents: Arc::default(),
            closed_broadcast: Arc::new(Mutex::new(Arc::new(NullClosedBroadcast) as Arc<dyn ClosedBroadcast>)),
        }
    }

    /// Point the `term://closed` broadcast at the real app handle.
    /// Called once in `lib.rs` setup; everything afterwards (manual closes,
    /// project stop, `close_on_exit`) broadcasts through it automatically.
    pub fn set_closed_broadcast(&self, broadcast: Arc<dyn ClosedBroadcast>) {
        *self.closed_broadcast.lock().unwrap_or_else(|e| e.into_inner()) = broadcast;
    }

    pub fn docker(&self) -> Arc<dyn DockerCli> {
        self.docker.clone()
    }

    /// The check-and-reserve lock for agent spawns (see the field).
    pub fn spawn_guard(&self) -> Arc<Mutex<()>> {
        self.spawn_guard.clone()
    }

    /// Reserve the next id WITHOUT creating an entry (the identity
    /// env `CHAPPA_AI_PROCESS_ID` must be in the child's environment, so the
    /// id has to exist before the spawn). The id is then passed to
    /// [`Registry::create_agent_terminal`].
    pub fn reserve_id(&self) -> TermId {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        id
    }

    /// Spawn an agent terminal into a reserved id with its metadata attached
    /// before the pump starts. Wakes the bridge probe for
    /// docker-exec runtimes.
    pub fn create_agent_terminal(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        id: TermId,
        meta: AgentMeta,
        close_on_exit: bool,
    ) -> Result<TermId, String> {
        let docker = meta.runtime.is_docker();
        let id = self.create_terminal_full(cfg, sink, on_notify, name, Some(id), None, Some(meta), false, close_on_exit)?;
        if docker {
            self.bridge_waker.wake();
        }
        Ok(id)
    }

    /// An agent entry fronted by a NO-PTY actor — the json
    /// transport runs its child over pipes and reports bytes/exit to the
    /// actor by hand (`TermHandle::note_output` / `note_child_exit`). Returns
    /// the handle the [`JsonAgent`] drives; `attach_json_agent` registers
    /// the agent so close/shutdown and the send/events routes find it.
    pub fn create_json_agent_terminal(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        id: TermId,
        meta: AgentMeta,
        close_on_exit: bool,
    ) -> Result<TermHandle, String> {
        let docker = meta.runtime.is_docker();
        let id = self.create_terminal_full(cfg, sink, on_notify, name, Some(id), None, Some(meta), true, close_on_exit)?;
        if docker {
            self.bridge_waker.wake();
        }
        self.handle(id).ok_or_else(|| "entry vanished during spawn".to_owned())
    }

    pub fn attach_json_agent(&self, id: TermId, agent: Arc<JsonAgent>) {
        self.json_agents.lock().unwrap_or_else(|e| e.into_inner()).insert(id, agent);
    }

    /// The json agent behind `id`, if the entry is a json-transport agent.
    pub fn json_agent(&self, id: TermId) -> Option<Arc<JsonAgent>> {
        self.json_agents.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
    }

    /// Forget the json agent behind `id` and mark it closing (no more
    /// events, sends or `awaiting_input`) — WITHOUT killing its child. The
    /// close path kills the container side first (the host client must
    /// still be alive for that), then calls `kill` on what this returns.
    fn detach_json_agent(&self, id: TermId) -> Option<Arc<JsonAgent>> {
        let agent = self.json_agents.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        if let Some(agent) = &agent {
            agent.begin_close();
        }
        agent
    }

    /// `awaiting_input` on the agent block
    /// is DERIVED at read time from the json agent — never mirrored, so it
    /// cannot go stale — and gated on the entry being alive: a dead agent
    /// is never "waiting".
    fn derive_awaiting(&self, id: TermId, meta: &mut AgentMeta, child_alive: bool) {
        meta.awaiting_input = child_alive && self.json_agent(id).is_some_and(|agent| agent.awaiting_input());
    }
    /// Spawn a terminal actor plus its event pump and register it. `name` is
    /// the display/debug label. Errors propagate from the pty spawn and leave
    /// the registry untouched. Plain shells pass no exit handler.
    pub fn create_terminal(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
    ) -> Result<TermId, String> {
        self.create_terminal_inner(cfg, sink, on_notify, name, None, None)
    }

    /// Same as [`Registry::create_terminal`], plus an exit handler (project
    /// processes; see [`ExitHandler`]).
    pub fn create_terminal_with_exit(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        on_exit: Option<ExitHandler>,
    ) -> Result<TermId, String> {
        self.create_terminal_inner(cfg, sink, on_notify, name, None, on_exit)
    }

    /// Respawn a process terminal into a SPECIFIC id (auto_restart):
    /// the frontend keeps one panel/term-id across restarts, so the respawned
    /// actor reuses the same registry key and frames channel. Any entry under
    /// that id is closed first. The old pump is joined before the new spawn,
    /// so the old actor's `Exited` cannot race the fresh one's `Starting`.
    pub fn respawn_terminal(
        &self,
        id: TermId,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        on_exit: Option<ExitHandler>,
    ) -> Result<TermId, String> {
        self.close(id);
        self.create_terminal_inner(cfg, sink, on_notify, name, Some(id), on_exit)
    }

    fn create_terminal_inner(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        at: Option<TermId>,
        on_exit: Option<ExitHandler>,
    ) -> Result<TermId, String> {
        self.create_terminal_full(cfg, sink, on_notify, name, at, on_exit, None, false, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_terminal_full(
        &self,
        cfg: ActorConfig,
        sink: Arc<dyn EventSink>,
        on_notify: Option<AppHandle>,
        name: String,
        at: Option<TermId>,
        on_exit: Option<ExitHandler>,
        agent: Option<AgentMeta>,
        nopty: bool,
        close_on_exit: bool,
    ) -> Result<TermId, String> {
        // The stable identity, before anything else can fail: an agent keeps
        // its spawn uuid (one identity end to end), a shell gets a fresh one.
        let uuid = match agent.as_ref() {
            Some(meta) => meta.spawn_uuid.clone(),
            None => crate::control_http::random_hex(16)
                .map_err(|e| format!("cannot generate a process uuid: {e}"))?,
        };
        let project_id = agent.as_ref().and_then(|meta| meta.project_id);
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let handle = if nopty {
            // No child of its own — the json transport's
            // piped child reports through `note_output` / `note_child_exit`.
            spawn_actor_nopty(cfg.clone(), events_tx).0
        } else {
            spawn_actor(cfg.clone(), events_tx).map_err(|e| e.to_string())?
        };
        let pid = handle.child_pid();

        let mut inner = self.inner.lock().unwrap();
        let id = at.unwrap_or_else(|| {
            let id = inner.next_id;
            inner.next_id = inner.next_id.wrapping_add(1);
            id
        });
        // The sink becomes SHARED state the pump reads per event and
        // the entry hands to `attach_terminal` for a mid-life swap. The pump
        // and the entry hold the SAME Arc, so a swap the pump sees is exactly
        // the swap `Entry.sink` now holds.
        let sink: Arc<Mutex<Arc<dyn EventSink>>> = Arc::new(Mutex::new(sink));
        let sink_awaiting_full = Arc::new(AtomicBool::new(false));
        let entry = Entry {
            handle,
            event_pump: None,
            name,
            // spawn = starting; the pump flips it to running on the first
            // frame (term://status).
            status: ProcessStatus::Starting,
            exit_code: None,
            cols: cfg.spec.cols,
            rows: cfg.spec.rows,
            seq: 0,
            pid,
            events: EventRing::new(100),
            on_exit,
            input_journal: VecDeque::with_capacity(INPUT_JOURNAL_CAP),
            agent,
            uuid,
            project_id,
            sink: sink.clone(),
            sink_awaiting_full: sink_awaiting_full.clone(),
            close_on_exit,
        };
        inner.entries.insert(id, entry);
        // Entry in place before the pump starts, so the pump can never race a
        // missing entry while pushing the actor's initial frame.
        inner.entries.get_mut(&id).unwrap().event_pump = Some(spawn_pump(
            id,
            events_rx,
            sink,
            sink_awaiting_full,
            self.inner.clone(),
            on_notify,
            self.clone(),
        ));
        drop(inner);
        // There is something to sample now: release a parked stats poller.
        // After the lock, so the poller's first `stats_targets()` can never
        // contend with the tail of this spawn.
        self.waker.wake();
        Ok(id)
    }

    pub fn handle(&self, id: TermId) -> Option<TermHandle> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&id)
            .map(|entry| entry.handle.clone())
    }

    /// Attribute a terminal to a project (the project-file start path calls this
    /// right after the spawn). Agents already carry theirs in their meta.
    pub fn set_project(&self, id: TermId, project_id: Option<u32>) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(&id) {
                entry.project_id = project_id;
            }
        }
    }

    /// The stable uuid of one entry, `None` when unknown.
    pub fn uuid_of(&self, id: TermId) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.entries.get(&id).map(|e| e.uuid.clone()))
    }

    /// Byte-stream liveness for MANY ids under one lock (the timer tick reads
    /// every watched process plus every delivery target once per second —
    /// one lock, not one per id). Missing ids are simply absent.
    pub fn liveness_many(&self, ids: &[TermId]) -> HashMap<TermId, LiveRow> {
        let Ok(inner) = self.inner.lock() else {
            return HashMap::new();
        };
        ids.iter()
            .filter_map(|id| inner.entries.get(id).map(|entry| (*id, live_row(entry))))
            .collect()
    }

    /// Shut the actor down, join the pump, drop the entry. The user asked for
    /// the terminal to close: the child is killed (the actor's session Drop)
    /// and the rail row is removed by the frontend that initiated the close.
    ///
    /// For a docker-exec agent the CONTAINER-side process is
    /// killed and verified FIRST (TERM → poll → KILL → verify), THEN the host
    /// client, THEN the pid file goes — killing docker.exe first would leave
    /// the in-container process an invisible orphan (the 2026-08-07 48 % CPU
    /// incident). The verification rides on the outcome.
    ///
    /// Blocks for the whole docker sequence (up to TERM_GRACE + KILL_GRACE
    /// plus a few CLI calls) — callers on the webview's main thread go
    /// through the async `close_terminal` command, which `spawn_blocking`s
    /// this.
    pub fn close(&self, id: TermId) -> Option<CloseOutcome> {
        self.close_inner(id, CloseReason::Closed, true)
    }

    /// Close an entry because its child EXITED with the spawn's
    /// `close_on_exit` flag set (the pump's exit-branch dispatch). Takes the
    /// SAME path `close_terminal` uses (docker verification + pid-file
    /// removal, json-agent detach) — nothing bespoke — but the broadcast
    /// reason is `close_on_exit` so the rail/observers can tell a self-reap
    /// from a manual close.
    pub fn close_on_exit(&self, id: TermId) -> Option<CloseOutcome> {
        self.close_inner(id, CloseReason::CloseOnExit, true)
    }

    /// A close that broadcasts NO `term://closed` — the debug-harness (8323)
    /// surface, whose terminals are test scaffolding by design (they never
    /// emitted `term://created` either; the exclusion, mirrored here).
    pub fn close_silent(&self, id: TermId) -> Option<CloseOutcome> {
        self.close_inner(id, CloseReason::Closed, false)
    }

    fn close_inner(&self, id: TermId, reason: CloseReason, broadcast: bool) -> Option<CloseOutcome> {
        let entry = self.inner.lock().unwrap().entries.remove(&id)?;
        // The json agent is marked closing (no more events) but its
        // piped child — for docker, the host `docker exec -i` client — stays
        // alive through the container-side kill, exactly like a pty agent's
        // client. Killing it first would orphan the in-container process.
        let json_agent = self.detach_json_agent(id);
        let verification = entry
            .agent
            .as_ref()
            .filter(|meta| meta.runtime.is_docker())
            .map(|meta| kill_container_side(self.docker.as_ref(), meta, TERM_GRACE, KILL_GRACE));
        // THEN the host side: the piped child, then the fronting actor.
        if let Some(agent) = json_agent {
            agent.kill();
        }
        entry.handle.shutdown();
        if let Some(pump) = entry.event_pump {
            let _ = pump.join();
        }
        // The pid file goes LAST (`remove_pid_file` skips the verdicts where
        // the exec could only fail or hang).
        if let (Some(meta), Some(v)) = (entry.agent.as_ref(), verification) {
            remove_pid_file(self.docker.as_ref(), meta, v);
        }
        if let Some(v) = verification {
            if v != CloseVerification::Gone {
                eprintln!("[agents] close: agent {id} container-side verification = {}", v.as_str());
            }
        }
        // Announce the close LAST — the entry is already gone and
        // the teardown done, so a webview dropping the row on this event sees
        // a fully-reaped terminal. This is the seam that stops an MCP/control
        // `close_terminal` from leaving a stale rail row.
        if broadcast {
            // Clone the Arc out from under the lock so the broadcast itself
            // (which may re-enter the app handle) never runs while holding it.
            let broadcast = self.closed_broadcast.lock().unwrap_or_else(|e| e.into_inner()).clone();
            emit_closed(broadcast.as_ref(), ClosedEvent { term_id: id, reason: reason.as_str() });
        }
        Some(CloseOutcome { verification })
    }

    /// Window close: children must not outlive the app. Entries are taken out
    /// under the lock first, so a pump that briefly locks the same mutex while
    /// an event lands can never deadlock against these joins. Hard-kills every
    /// child ("hard-kill stragglers after 2s"): the graceful path is
    /// just dropping the pty, so the hard kill is the whole story here — a
    /// SIGKILL on the session leader takes the tree down, and the actor's
    /// post-kill flush (EXIT_FLUSH_TIMEOUT) bounds the join to well under 2s.
    ///
    /// Docker-exec agents get the container-side kill sequence
    /// first — one thread per container (containers in parallel, agents in
    /// one container sequentially), each bounded to
    /// [`SHUTDOWN_PER_CONTAINER`], the whole wait never past [`SHUTDOWN_CAP`].
    /// A hung docker simply stops being waited for; the host clients are then
    /// killed regardless. Returns each agent's verification (logging).
    pub fn shutdown_all(&self) -> Vec<(TermId, CloseVerification)> {
        let entries: Vec<(TermId, Entry)> = {
            let mut inner = self.inner.lock().unwrap();
            std::mem::take(&mut inner.entries).into_iter().collect()
        };
        // Json agents: closing now (no more events), killed with the host
        // clients below — AFTER the container-side sequence.
        let json_agents: Vec<Arc<JsonAgent>> = entries
            .iter()
            .filter_map(|(id, _)| self.detach_json_agent(*id))
            .collect();
        // Group the docker agents by container.
        let mut by_container: HashMap<String, Vec<(TermId, AgentMeta)>> = HashMap::new();
        for (id, entry) in &entries {
            if let Some(meta) = &entry.agent {
                if let Some(container) = meta.container_name() {
                    by_container
                        .entry(container.to_owned())
                        .or_default()
                        .push((*id, meta.clone()));
                }
            }
        }
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel::<(TermId, CloseVerification)>();
        let mut expected = 0usize;
        for (_, agents) in by_container {
            let docker = self.docker.clone();
            let tx = tx.clone();
            expected += agents.len();
            std::thread::spawn(move || {
                let deadline = Instant::now() + SHUTDOWN_PER_CONTAINER;
                for (id, meta) in agents {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    // The per-container budget is split between the two
                    // graces; the whole thing is bounded by SHUTDOWN_CAP
                    // regardless (a hung docker simply stops being waited
                    // for).
                    let verdict = if remaining.is_zero() {
                        CloseVerification::Unresolved
                    } else {
                        let term = remaining.min(TERM_GRACE);
                        let kill = remaining.saturating_sub(term).min(KILL_GRACE);
                        kill_container_side(docker.as_ref(), &meta, term, kill)
                    };
                    let _ = tx.send((id, verdict));
                }
            });
        }
        drop(tx);
        let mut results = Vec::with_capacity(expected);
        while results.len() < expected {
            let remaining = SHUTDOWN_CAP.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(item) => results.push(item),
                Err(_) => break,
            }
        }
        // Host clients LAST, all of them, hung docker or not: the json
        // agents' piped children first, then every fronting actor.
        for agent in json_agents {
            agent.kill();
        }
        for (id, entry) in entries {
            entry.handle.kill();
            if let Some(pump) = entry.event_pump {
                let _ = pump.join();
            }
            if let Some(meta) = &entry.agent {
                if meta.runtime.is_docker() && !results.iter().any(|(rid, _)| *rid == id) {
                    results.push((id, CloseVerification::Unresolved));
                }
            }
        }
        for (id, verdict) in &results {
            if *verdict != CloseVerification::Gone {
                eprintln!("[agents] shutdown: agent {id} container-side verification = {}", verdict.as_str());
            }
        }
        results
    }

    // ---- agent projections ------------------------------------------

    /// Every agent entry as the pure guards see it.
    pub fn agent_rows(&self) -> Vec<AgentRow> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .entries
                    .iter()
                    .filter_map(|(id, entry)| {
                        let meta = entry.agent.as_ref()?;
                        let io = entry.handle.io();
                        Some(AgentRow {
                            id: *id,
                            name: entry.name.clone(),
                            tool_id: meta.tool_id,
                            container: meta.container_name().map(str::to_owned),
                            child_alive: io.child_alive(),
                            last_output_at: io.last_output_ms(),
                            spawned_at_ms: meta.spawned_at_ms,
                            spawn_uuid: meta.spawn_uuid.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The docker-exec agents whose client is still alive — what the bridge
    /// probe inspects. Handles are CLONES (the probe pokes them outside the
    /// lock).
    pub fn probe_targets(&self) -> Vec<ProbeTarget> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .entries
                    .iter()
                    .filter_map(|(id, entry)| {
                        let meta = entry.agent.as_ref()?;
                        let container = meta.container_name()?.to_owned();
                        if !entry.handle.io().child_alive() {
                            return None;
                        }
                        Some(ProbeTarget {
                            id: *id,
                            container,
                            spawned_at_ms: meta.spawned_at_ms,
                            handle: entry.handle.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record the probe's latest reading. Returns true when the BRIDGE
    /// changed (the once-per-transition `agent://bridge` gate); the container
    /// state is refreshed either way.
    pub fn update_bridge(&self, id: TermId, container: ContainerState, bridge: Bridge) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        let Some(meta) = inner.entries.get_mut(&id).and_then(|e| e.agent.as_mut()) else {
            return false;
        };
        let changed = meta.bridge != bridge;
        meta.bridge = bridge;
        meta.container = Some(container);
        changed
    }

    pub fn agent_meta(&self, id: TermId) -> Option<AgentMeta> {
        let (mut meta, alive) = self.inner.lock().ok().and_then(|inner| {
            inner
                .entries
                .get(&id)
                .and_then(|e| e.agent.clone().map(|meta| (meta, e.handle.io().child_alive())))
        })?;
        self.derive_awaiting(id, &mut meta, alive);
        Some(meta)
    }

    /// The park/wake pair the bridge probe blocks on while no docker-exec
    /// agent is alive.
    pub fn bridge_waker(&self) -> Arc<StatsWaker> {
        self.bridge_waker.clone()
    }

    /// Every live terminal's handle (`set_settings` broadcasts the
    /// per-actor live controls — synthetic marks, wheel speed — to all of
    /// them, because "a toggle must not require restarting terminals").
    /// Returns CLONES so the caller never holds the registry lock while it
    /// talks to actors.
    pub fn handles(&self) -> Vec<TermHandle> {
        self.inner
            .lock()
            .map(|inner| inner.entries.values().map(|e| e.handle.clone()).collect())
            .unwrap_or_default()
    }

    /// The debug listing / rail snapshot: one entry per live terminal.
    pub fn list(&self) -> Vec<TerminalSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|(id, entry)| snapshot_of(*id, entry))
            .collect()
    }

    /// The RICH rail listing the frontend reconciler adopts from —
    /// the `list` rows plus the kind/binding facts. Unlike [`Registry::list`]
    /// and the control listing (whose wire shapes the harness and
    /// CONTROL_API.md pin), this is the FRONTEND's own contract, so it can
    /// carry the placement facts without disturbing either pinned surface.
    pub fn list_rows(&self) -> Vec<RailRow> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|(id, entry)| rail_row(*id, entry))
            .collect()
    }

    /// Swap a webview's frames channel in as the sink for an
    /// EXISTING registry entry, and request a FULL so the new subscriber
    /// starts from a complete frame (the dock-window primitive needs).
    /// Re-attach (a panel rebuilt for the same terminal) is legal and
    /// idempotent — the actor's `request_full` is a cheap drop-in control
    /// message either way. A missing id is a clear error, not a silent no-op.
    pub fn attach_terminal(&self, id: TermId, sink: Arc<dyn EventSink>) -> Result<RailRow, String> {
        let row = {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner
                .entries
                .get_mut(&id)
                .ok_or_else(|| format!("no such terminal: {id}"))?;
            *entry.sink.lock().unwrap_or_else(|e| e.into_inner()) = sink;
            entry.sink_awaiting_full.store(true, Ordering::SeqCst);
            (rail_row(id, entry), entry.handle.clone())
        };
        // Outside the registry lock: the actor's request_full is non-blocking,
        // but never hold a shared lock across it.
        row.1.request_full();
        Ok(row.0)
    }

    /// The control-surface listing: every live terminal with its
    /// byte-stream liveness fields.
    pub fn list_control(&self) -> Vec<ControlSnapshot> {
        let inner = self.inner.lock().unwrap();
        let mut rows: Vec<ControlSnapshot> = inner
            .entries
            .iter()
            .map(|(id, entry)| self.control_snapshot_of(*id, entry))
            .collect();
        rows.sort_by_key(|r| r.base.id);
        rows
    }

    /// One terminal's control-surface row, `None` when unknown.
    pub fn control_snapshot(&self, id: TermId) -> Option<ControlSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&id)
            .map(|entry| self.control_snapshot_of(id, entry))
    }

    /// The control row with the derived agent facts filled in.
    fn control_snapshot_of(&self, id: TermId, entry: &Entry) -> ControlSnapshot {
        let mut row = control_snapshot(id, entry);
        if let Some(meta) = row.agent.as_mut() {
            self.derive_awaiting(id, meta, row.child_alive);
        }
        row
    }

    /// ONE atomic write of `bytes` to the pty (the
    /// caller has already appended `\r` for `submit` — text and Enter must
    /// never be split into two writes), journaled on the entry. Returns
    /// `false` for an unknown id.
    pub fn write_journaled(&self, id: TermId, bytes: &[u8], text: String, submit: bool) -> bool {
        let handle = match self.handle(id) {
            Some(handle) => handle,
            None => return false,
        };
        handle.write_input(bytes);
        self.record_input(id, bytes.len(), text, submit);
        true
    }

    /// Journal one delivery WITHOUT a pty write (the json
    /// transport writes its own stdin line; the audit trail is the same).
    pub fn record_input(&self, id: TermId, bytes_len: usize, text: String, submit: bool) {
        let record = InputRecord {
            ts: epoch_ms(),
            bytes_len,
            text,
            submit,
        };
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(&id) {
                if entry.input_journal.len() >= INPUT_JOURNAL_CAP {
                    entry.input_journal.pop_front();
                }
                entry.input_journal.push_back(record);
            }
        }
    }

    /// The input journal (oldest first, at most [`INPUT_JOURNAL_CAP`]);
    /// `None` for an unknown id.
    pub fn input_journal(&self, id: TermId) -> Option<Vec<InputRecord>> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&id)
            .map(|entry| entry.input_journal.iter().cloned().collect())
    }

    /// What the stats poller needs per tick: one row per live
    /// terminal, with the tree root and whether it is still alive. Kept
    /// separate from [`Registry::list`] deliberately — the pid is an internal
    /// fact, not part of the rail/debug snapshot the frontend and the parity
    /// harness read.
    pub fn stats_targets(&self) -> Vec<StatsTarget> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .entries
                    .iter()
                    .map(|(id, entry)| StatsTarget {
                        id: *id,
                        pid: entry.pid,
                        alive: matches!(
                            entry.status,
                            ProcessStatus::Starting | ProcessStatus::Running
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The park/wake pair the stats poller blocks on while the registry is
    /// empty.
    pub fn stats_waker(&self) -> Arc<StatsWaker> {
        self.waker.clone()
    }

    /// JSON events for one terminal with `seq > since` (debug `/events`).
    pub fn events_since(&self, id: TermId, since: u64) -> Vec<JsonEvent> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&id)
            .map(|entry| entry.events.since(since))
            .unwrap_or_default()
    }

    /// Keep the debug listing's size honest after a resize.
    pub fn set_size(&self, id: TermId, cols: u16, rows: u16) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(&id) {
                entry.cols = cols;
                entry.rows = rows;
            }
        }
    }
}

/// The per-terminal event pump: forward frames as binary, everything else as a
/// `term://*` JSON event, and keep the entry's debug ring + metadata current.
/// Runs until the actor's `TermEvent` sender drops (terminal closed or child
/// exited). `seq` is a per-terminal monotonic counter over every event the
/// actor emitted.
///
/// `on_notify` keeps its historical name but it serves ONLY the
/// OSC 52 clipboard write — the OS notification it was named for moved to the
/// frontend's `os_notify` command, behind the notification-level decision
/// function. The name/arity are left alone deliberately: `tests/pump.rs` pins
/// the 4-arg `create_terminal(cfg, sink, None, name)` shape.
fn spawn_pump(
    term_id: TermId,
    rx: Receiver<TermEvent>,
    sink: Arc<Mutex<Arc<dyn EventSink>>>,
    sink_awaiting_full: Arc<AtomicBool>,
    registry: Arc<Mutex<Inner>>,
    on_notify: Option<AppHandle>,
    // The owning Registry, held so the `close_on_exit` exit branch
    // can dispatch a real `Registry::close` (which needs the whole registry,
    // not just the inner lock) OFF the registry lock and OFF this thread.
    registry_owned: Registry,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut seq = 0u64;
        // term://activity rate limiter: at most one emit per second.
        let started_at = Instant::now();
        let mut activity = ActivityLimiter::new(Duration::from_secs(1));
        while let Ok(event) = rx.recv() {
            seq += 1;
            // Read the CURRENT sink once per event. A mid-life swap
            // ([`Registry::attach_terminal`]) therefore takes effect for the
            // very next event — never the event it was racing — and an event
            // is never split across two sinks. The lock is a pointer swap: it
            // is only contended on the rare attach path, not the frame path.
            let sink: Arc<dyn EventSink> = sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match event {
                TermEvent::Frame(frame) => {
                    // After an attach, hold deltas
                    // until the forced FULL flows — the fresh subscriber's
                    // decoder is empty and a racing delta would seed it with
                    // a partial picture. Seq/status below still advance.
                    let deliver = if sink_awaiting_full.load(Ordering::SeqCst) {
                        if frame.kind == FrameKind::Full {
                            sink_awaiting_full.store(false, Ordering::SeqCst);
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    };
                    if deliver {
                        let mut buf = Vec::new();
                        encode_frame(&frame, &mut buf);
                        sink.send_binary(buf);
                    }
                    if let Ok(mut inner) = registry.lock() {
                        if let Some(entry) = inner.entries.get_mut(&term_id) {
                            entry.seq = frame.seq;
                        }
                    }
                    // spawn = starting → running on the first frame.
                    apply_status(
                        term_id,
                        seq,
                        StatusEvent::FirstFrame,
                        None,
                        &sink,
                        &registry,
                    );
                    // Activity is emit-only and rate-limited: the rail shows a
                    // subtle dot for hidden terminals that produced output. It
                    // is deliberately NOT pushed into the per-terminal debug
                    // event ring (it would evict title/notify/exited events the
                    // parity harness asserts on).
                    if activity.due(started_at.elapsed().as_millis() as u64) {
                        sink.emit_json(JsonEvent {
                            event: "term://activity",
                            term_id,
                            seq,
                            data: json_map(&[]),
                        });
                    }
                }
                TermEvent::Title(title) => push(
                    term_id,
                    seq,
                    "term://title",
                    &sink,
                    &registry,
                    json_map(&[("title", json!(title))]),
                ),
                TermEvent::Bell => {
                    push(term_id, seq, "term://bell", &sink, &registry, json_map(&[]))
                }
                // Removed the unconditional OS notification that used
                // to fire here: "pump keeps emitting raw events — filtering
                // happens at the decision function, so levels change with zero
                // Rust churn". The frontend now runs
                // `decide(event_kind, level, is_active, window_focused)` on
                // this event and calls `os_notify` only when the answer says
                // to. `on_notify` stays threaded through — the OSC 52
                // clipboard arm below still needs the AppHandle.
                TermEvent::Notify { title, body } => push(
                    term_id,
                    seq,
                    "term://notify",
                    &sink,
                    &registry,
                    json_map(&[("title", json!(title)), ("body", json!(body))]),
                ),
                TermEvent::PromptMark { kind, row } => {
                    let kind = match kind {
                        MarkKind::PromptStart => "prompt_start",
                        MarkKind::PromptEnd => "prompt_end",
                        MarkKind::CommandStart => "command_start",
                        MarkKind::CommandEnd => "command_end",
                    };
                    push(
                        term_id,
                        seq,
                        "term://prompt_mark",
                        &sink,
                        &registry,
                        json_map(&[("kind", json!(kind)), ("row", json!(row))]),
                    );
                }
                TermEvent::HyperlinkTable(links) => {
                    let links: Vec<_> = links
                        .iter()
                        .map(|(id, uri)| json!({"id": id, "uri": uri}))
                        .collect();
                    push(
                        term_id,
                        seq,
                        "term://links",
                        &sink,
                        &registry,
                        json_map(&[("links", json!(links))]),
                    );
                }
                TermEvent::SearchStatus { total } => {
                    // Low-rate (new search / resize / request_full only): the
                    // frontend's "N matches" label. The count is a scan-time
                    // snapshot and can drift stale as scrollback scrolls.
                    push(
                        term_id,
                        seq,
                        "term://search",
                        &sink,
                        &registry,
                        json_map(&[("total", json!(total))]),
                    );
                }
                TermEvent::Clipboard(text) => {
                    // OSC 52 store → the OS clipboard. ClipboardLoad is
                    // deliberately unanswered by design: a terminal that can
                    // READ the OS clipboard is an exfiltration surface, so only
                    // the store direction is forwarded (documented asymmetry).
                    if let Some(app) = &on_notify {
                        let _ = app.clipboard().write_text(text.clone());
                    }
                    push(
                        term_id,
                        seq,
                        "term://clipboard",
                        &sink,
                        &registry,
                        json_map(&[("text", json!(text))]),
                    );
                }
                TermEvent::Exited(status) => {
                    apply_status(
                        term_id,
                        seq,
                        StatusEvent::Exited {
                            code: status.code,
                            success: status.success,
                        },
                        status.code,
                        &sink,
                        &registry,
                    );
                    // Fire the auto_restart hook OUTSIDE the registry
                    // lock: the project runner re-locks its own state and may
                    // spawn a respawn thread, but must never contend on the
                    // registry mutex while this pump still holds it.
                    // Read the spawn's `close_on_exit` flag in the
                    // same lock — the close itself happens OFF the lock and
                    // OFF this thread, below.
                    let (on_exit, close_on_exit) = match registry.lock() {
                        Ok(mut inner) => {
                            if let Some(entry) = inner.entries.get_mut(&term_id) {
                                entry.exit_code = status.code;
                                (entry.on_exit.clone(), entry.close_on_exit)
                            } else {
                                (None, false)
                            }
                        }
                        Err(_) => (None, false), // poisoned: nothing to report
                    };
                    if let Some(on_exit) = on_exit {
                        on_exit(term_id, status.code, status.success);
                    }
                    push(
                        term_id,
                        seq,
                        "term://exited",
                        &sink,
                        &registry,
                        json_map(&[
                            ("code", json!(status.code)),
                            ("success", json!(status.success)),
                        ]),
                    );
                    // `close_on_exit`: the exit branch's LAST act.
                    // This runs OFF the registry lock (the on_exit precedent
                    // above) and OFF this pump thread: `Registry::close` joins
                    // the pump, so calling it inline would deadlock on our own
                    // JoinHandle. A NON-ZERO exit closes too — the notification
                    // and the `term://exited` event already carried the
                    // failure; a lingering row is not the error report.
                    if close_on_exit {
                        let close = registry_owned.clone();
                        std::thread::spawn(move || {
                            let _ = close.close_on_exit(term_id);
                        });
                    }
                }
            }
        }
    })
}

/// Apply a status event to the entry; when the status actually changed, emit a
/// `term://status` event (recorded in the debug ring like any other JSON
/// event) carrying the status string and the current exit code.
fn apply_status(
    term_id: TermId,
    seq: u64,
    event: StatusEvent,
    exit_code: Option<i32>,
    sink: &Arc<dyn EventSink>,
    registry: &Arc<Mutex<Inner>>,
) {
    let (changed, status) = {
        let mut inner = match registry.lock() {
            Ok(inner) => inner,
            Err(_) => return, // poisoned: the terminal is gone anyway
        };
        match inner.entries.get_mut(&term_id) {
            Some(entry) => {
                let next = transition(entry.status, event);
                let changed = next != entry.status;
                entry.status = next;
                (changed, next)
            }
            None => return, // terminal already closed: nothing to announce
        }
    };
    if changed {
        let data = json_map(&[
            ("status", json!(status.as_str())),
            ("exit_code", json!(exit_code)),
        ]);
        push(term_id, seq, "term://status", sink, registry, data);
    }
}

/// Build and forward one JSON event, also keeping the debug ring current.
fn push(
    term_id: TermId,
    seq: u64,
    kind: &'static str,
    sink: &Arc<dyn EventSink>,
    registry: &Arc<Mutex<Inner>>,
    data: serde_json::Map<String, serde_json::Value>,
) {
    let event = JsonEvent {
        event: kind,
        term_id,
        seq,
        data,
    };
    sink.emit_json(event.clone());
    if let Ok(mut inner) = registry.lock() {
        if let Some(entry) = inner.entries.get_mut(&term_id) {
            entry.events.push(event);
        }
    }
}

/// The ONE `Entry` → rail-row conversion, shared by [`Registry::list`] and the
/// control snapshot below (which flattens it).
fn snapshot_of(id: TermId, entry: &Entry) -> TerminalSnapshot {
    TerminalSnapshot::new(
        id,
        entry.name.clone(),
        entry.status,
        entry.exit_code,
        entry.cols,
        entry.rows,
        entry.seq,
    )
}

/// The rich-row conversion for `list_terminals` / `attach_terminal`.
/// `kind` reuses the created-event vocabulary, so the frontend places a row
/// identically whether it learned of it from the broadcast or the reconcile.
fn rail_row(id: TermId, entry: &Entry) -> RailRow {
    RailRow {
        base: snapshot_of(id, entry),
        kind: if entry.agent.is_some() {
            "agent"
        } else if entry_project(entry).is_some() {
            "process"
        } else {
            "terminal"
        },
        project_id: entry_project(entry),
        agent_tool_id: entry.agent.as_ref().map(|meta| meta.tool_id),
        parent_process_id: entry.agent.as_ref().and_then(|meta| meta.parent_process_id),
    }
}

fn control_snapshot(id: TermId, entry: &Entry) -> ControlSnapshot {
    let io = entry.handle.io();
    ControlSnapshot {
        base: snapshot_of(id, entry),
        output_bytes: io.output_bytes(),
        last_output_at: io.last_output_ms(),
        has_output: io.has_output(),
        child_alive: io.child_alive(),
        kind: if entry.agent.is_some() { "agent" } else { "shell" },
        agent: entry.agent.clone(),
        uuid: entry.uuid.clone(),
        project_id: entry_project(entry),
        close_on_exit: entry.close_on_exit,
    }
}

fn entry_project(entry: &Entry) -> Option<u32> {
    entry
        .project_id
        .or_else(|| entry.agent.as_ref().and_then(|meta| meta.project_id))
}

fn live_row(entry: &Entry) -> LiveRow {
    let io = entry.handle.io();
    LiveRow {
        uuid: entry.uuid.clone(),
        name: entry.name.clone(),
        project_id: entry_project(entry),
        child_alive: io.child_alive(),
        last_output_at: io.last_output_ms(),
        output_bytes: io.output_bytes(),
    }
}

fn json_map(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}
