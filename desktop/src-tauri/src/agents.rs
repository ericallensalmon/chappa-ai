//! Agents first-class: spawn, docker-exec bridge
//! awareness, verified container-side close, busy guards.
//!
//! Everything docker-shaped goes through the [`DockerCli`] seam — a real
//! `std::process::Command` implementation for the app and a scripted
//! [`FakeDocker`] for tests — so the whole close/probe/guard logic is unit-
//! tested with no docker on the box. The pure pieces (spawn plan, env merge,
//! ready gate, busy check, bridge derivation) take plain values and are
//! tested with fake clocks and fake registries.
//!
//! What this module deliberately does NOT know: anything about a specific
//! agent CLI. The claude alt-screen env lives in the claude TEMPLATE
//! (`agent_tools.rs`), never here.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};
pub use term_core::actor::epoch_ms;
use term_core::actor::{ActorConfig, TermHandle};
use term_core::pty::PtySpec;

use crate::agent_json::{
    event_json, machine_mode, AgentEvent, JsonAgent, JsonHooks, SpawnRecipe, TurnReceipt,
    TURN_RECEIPT_WAIT_DEFAULT_MS, TURN_RECEIPT_WAIT_MAX_MS,
};
use crate::agent_tools::{docker_exec_prefix, AgentTool, AgentToolsState, Runtime, ToolType, Transport};
use crate::projects::Projects;
use crate::registry::{
    emit_created, AgentRow, AppCreatedBroadcast, ChannelSink, CreatedEvent, EventSink, JsonEvent,
    NullSink, Registry, TermId,
};
use crate::settings::SettingsState;

// ---- constants (tuning numbers, named once) ----------------------------------

/// Busy = child alive AND (`last_output_at` within this window OR spawned
/// within it — a just-launched agent that has not spoken yet is booting, not
/// idle).
pub const BUSY_WINDOW_MS: u64 = 120_000;
/// Bridge probe period; parks when no docker-exec agent is alive.
pub const PROBE_PERIOD: Duration = Duration::from_secs(5);
/// The winsize poke's wait for bytes before an agent is called `stale`.
pub const POKE_WAIT: Duration = Duration::from_secs(2);
/// One docker CLI call's timeout (`inspect`, `stats`, `exec …`).
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// TERM → poll `/proc/<pid>` for at most this long before KILL.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// KILL → poll `/proc/<pid>` for at most this long before `still-present`
/// (a SIGKILLed process needs a beat to be reaped; reporting the instant
/// after the kill is a false alarm).
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// Per-container budget at app exit; the whole `shutdown_all` never blocks
/// quit longer than [`SHUTDOWN_CAP`].
pub const SHUTDOWN_PER_CONTAINER: Duration = Duration::from_secs(3);
pub const SHUTDOWN_CAP: Duration = Duration::from_secs(5);
/// A queued `prompt` waits at most this long for the ready gate — the MCP
/// client's read timeout is 30 s, so the response must come back before it.
pub const PROMPT_WAIT_CAP: Duration = Duration::from_secs(20);
/// Where the pid-recording wrapper writes inside the container.
pub const PID_DIR: &str = "/tmp/.chappa-ai";
/// Env var carrying the spawn uuid into the container-side process, so the
/// no-`sh` fallback can match it exactly (never by program name).
pub const SPAWN_UUID_ENV: &str = "CHAPPA_AI_SPAWN_UUID";

/// The tool names, quoted in `agent_instructions` ("chappa-ai MCP
/// tools: …") so an orchestrator prepending the sentence to its first prompt
/// tells the agent what it can call. Mirrors `chappa-ai-mcp::tools()`.
pub const MCP_TOOL_NAMES: &[&str] = &[
    "list_processes",
    "get_process_status",
    "get_process_output",
    "get_input_journal",
    "list_projects",
    "get_project",
    "list_agent_tools",
    "spawn_terminal",
    "spawn_agent",
    "get_agent_events",
    "send_input",
    "send_bytes",
    "start_process",
    "stop_process",
    "restart_process",
    "close_terminal",
];

// ---- docker CLI seam ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DockerOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The ONE way this module talks to docker. `args` is everything after
/// `docker`; `Err` = the CLI could not be run or timed out (distinct from a
/// non-zero exit, which is `Ok` with `success == false`).
pub trait DockerCli: Send + Sync {
    fn run(&self, args: &[String], timeout: Duration) -> Result<DockerOutput, String>;
}

/// `std::process::Command::new("docker")` with a hard timeout: the child is
/// killed when the deadline passes (a hung docker daemon must never wedge a
/// close or the probe thread).
pub struct RealDocker;

impl DockerCli for RealDocker {
    fn run(&self, args: &[String], timeout: Duration) -> Result<DockerOutput, String> {
        let mut child = std::process::Command::new(docker_program())
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run docker: {e}"))?;
        // Drain both pipes on their own threads so a chatty command cannot
        // block on a full pipe while we poll for exit.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || read_all(stdout));
        let err_thread = std::thread::spawn(move || read_all(stderr));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Err(e) => return Err(format!("docker wait failed: {e}")),
            }
        };
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        match status {
            Some(status) => Ok(DockerOutput {
                success: status.success(),
                stdout,
                stderr,
            }),
            None => Err(format!("docker {} timed out after {:?}", args.join(" "), timeout)),
        }
    }
}

fn read_all(pipe: Option<impl Read>) -> String {
    let mut buf = String::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_string(&mut buf);
    }
    buf
}

/// `docker` resolved through PATH on Windows (CreateProcessW does not search
/// PATH for a bare name the way a shell does — the `default_shell` lesson);
/// a bare `docker` elsewhere. Resolved ONCE per process: the PATH walk hits
/// the filesystem and this runs on every docker call and every spawn plan.
pub fn docker_program() -> &'static str {
    static PROGRAM: OnceLock<String> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        if cfg!(windows) {
            if let Some(path) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path) {
                    let candidate = dir.join("docker.exe");
                    if candidate.is_file() {
                        return candidate.to_string_lossy().into_owned();
                    }
                }
            }
        }
        "docker".to_owned()
    })
}

/// `kebab-case` names for the small status enums, derived from the SAME
/// serde attribute the wire uses — one spelling, never a hand-copied match.
macro_rules! kebab_str {
    ($ty:ty { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(<$ty>::$variant => $name,)+
                }
            }
        }
        #[cfg(test)]
        impl $ty {
            fn all() -> Vec<$ty> {
                vec![$(<$ty>::$variant),+]
            }
        }
    };
}

/// A scripted docker CLI for tests: answers by matching the START of the
/// argv against a rule list, records every call, and can hang (sleep for
/// the whole timeout, then report a timeout) to simulate a wedged daemon.
#[derive(Default)]
pub struct FakeDocker {
    rules: Mutex<Vec<(Vec<String>, Result<DockerOutput, String>)>>,
    calls: Mutex<Vec<Vec<String>>>,
    hang: Mutex<bool>,
    /// Invoked before every call (tests assert invariants such as "the host
    /// client is still alive while the container-side kill runs").
    on_call: Mutex<Option<Box<dyn Fn(&[String]) + Send + Sync>>>,
}

impl FakeDocker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Answer any call whose argv starts with `prefix` (later rules win, so a
    /// test can override a default mid-way).
    pub fn on(&self, prefix: &[&str], out: Result<DockerOutput, String>) {
        self.rules
            .lock()
            .unwrap()
            .push((prefix.iter().map(|s| s.to_string()).collect(), out));
    }

    pub fn ok(&self, prefix: &[&str], stdout: &str) {
        self.on(
            prefix,
            Ok(DockerOutput {
                success: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            }),
        );
    }

    pub fn fail(&self, prefix: &[&str], stderr: &str) {
        self.on(
            prefix,
            Ok(DockerOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_owned(),
            }),
        );
    }

    pub fn set_hang(&self, hang: bool) {
        *self.hang.lock().unwrap() = hang;
    }

    pub fn set_on_call(&self, f: impl Fn(&[String]) + Send + Sync + 'static) {
        *self.on_call.lock().unwrap() = Some(Box::new(f));
    }

    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// Calls whose argv starts with `prefix`.
    pub fn calls_with(&self, prefix: &[&str]) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| starts_with(c, prefix))
            .collect()
    }
}

fn starts_with(argv: &[String], prefix: &[impl AsRef<str>]) -> bool {
    prefix.len() <= argv.len() && prefix.iter().zip(argv).all(|(p, a)| p.as_ref() == a)
}

impl DockerCli for FakeDocker {
    fn run(&self, args: &[String], timeout: Duration) -> Result<DockerOutput, String> {
        if let Some(f) = self.on_call.lock().unwrap().as_ref() {
            f(args);
        }
        self.calls.lock().unwrap().push(args.to_vec());
        if *self.hang.lock().unwrap() {
            std::thread::sleep(timeout);
            return Err("docker hung (fake)".to_owned());
        }
        let rules = self.rules.lock().unwrap();
        rules
            .iter()
            .rev()
            .find(|(prefix, _)| starts_with(args, prefix))
            .map(|(_, out)| out.clone())
            .unwrap_or_else(|| Err(format!("fake docker: no rule for {args:?}")))
    }
}

// ---- container state / bridge ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerStatus {
    Running,
    Exited,
    Missing,
    Unknown,
}

/// What the probe last saw for a container (`docker inspect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerState {
    pub status: ContainerStatus,
    /// RFC 3339 as docker prints it; `None` when unknown/missing.
    pub started_at: Option<String>,
    /// Seconds since `started_at` at probe time.
    pub uptime_s: Option<u64>,
    /// `docker stats` memory, only on the spawn response (one call, 2 s
    /// timeout, omitted-not-failed when docker is slow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_limit_bytes: Option<u64>,
}

impl ContainerState {
    pub fn unknown() -> Self {
        Self {
            status: ContainerStatus::Unknown,
            started_at: None,
            uptime_s: None,
            mem_bytes: None,
            mem_limit_bytes: None,
        }
    }
}

/// The bridge field (NOT a ProcessStatus — that enum is the wire contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bridge {
    Ok,
    ContainerDown,
    Stale,
}

kebab_str!(Bridge { Ok => "ok", ContainerDown => "container-down", Stale => "stale" });

/// How the container-side pid is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NsPid {
    /// The wrapper wrote `<uuid>.pid`; read it at close.
    PidFile,
    /// No `sh` in the container: match `CHAPPA_AI_SPAWN_UUID` in /proc at close.
    Unresolved,
}

/// The close-path verification, logged on the entry, returned by
/// `close_terminal`, toasted when not `gone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloseVerification {
    Gone,
    StillPresent,
    ContainerDown,
    Unresolved,
}

kebab_str!(CloseVerification {
    Gone => "gone",
    StillPresent => "still-present",
    ContainerDown => "container-down",
    Unresolved => "unresolved",
});

/// What the registry entry records for an agent (the `agent` block on the
/// process record). The close verification is NOT here: the entry is gone
/// by the time the verdict exists — it travels on the close response
/// (`close_terminal` answers it) and the log line instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentMeta {
    pub tool_id: u32,
    pub tool_type: ToolType,
    pub model: Option<String>,
    pub runtime: Runtime,
    /// The project the agent was spawned into (cwd + `CHAPPA_AI_PROJECT_ID`),
    /// so the rail attributes it and "Close project" closes it too.
    pub project_id: Option<u32>,
    /// The registry id of the process that SPAWNED this agent over
    /// chappa-ai-mcp/control, `None` for a root (a user-spawned agent, or a
    /// spawn whose candidate parent could not be validated). ALWAYS present
    /// on the wire (serialized as a number or null — no `skip_serializing_if`)
    /// so MCP consumers can rely on it. IMMUTABLE: set once at spawn, never
    /// updated — parent EXIT/CLOSE does not touch it (history, not a live
    /// link; the frontend decides how an orphan renders).
    ///
    /// Trust model: the parent comes from the unauthenticated
    /// `X-Chappa-Actor` header (any bearer of the control token can claim any
    /// id), so this is a HINT for the rail — never authorization. A candidate
    /// names a live registry entry of any kind or it records `None` silently.
    pub parent_process_id: Option<TermId>,
    /// Unix ms at spawn — the `started_at`-newer-than-spawn comparison.
    pub spawned_at_ms: u64,
    pub spawn_uuid: String,
    pub nspid: NsPid,
    pub container: Option<ContainerState>,
    pub bridge: Bridge,
    /// `tty` (terminal panel) or `json` (typed events, transcript).
    pub transport: Transport,
    /// The json transport's typed "waiting for input" fact — the
    /// rail's `agent-waiting` attention. DERIVED by the registry at read
    /// time (`JsonAgent::awaiting_input` gated on the entry being alive),
    /// never stored: the value in the entry is always false. Always false
    /// for tty agents.
    pub awaiting_input: bool,
}

impl AgentMeta {
    pub fn container_name(&self) -> Option<&str> {
        self.runtime.container()
    }

    pub fn pid_file(&self) -> String {
        format!("{PID_DIR}/{}.pid", self.spawn_uuid)
    }
}

// ---- spawn plan (pure) ----------------------------------------------------------

/// Env merge: inherited < tool.env < identity. Returns document order with
/// later layers overriding earlier keys in place (so a caller can see exactly
/// which value wins for a key, and the docker `-e` list has no duplicates).
pub fn merge_env(
    inherited: &[(String, String)],
    tool_env: &IndexMap<String, String>,
    identity: &[(String, String)],
) -> IndexMap<String, String> {
    let mut out: IndexMap<String, String> = IndexMap::new();
    for (k, v) in inherited {
        out.insert(k.clone(), v.clone());
    }
    for (k, v) in tool_env {
        out.insert(k.clone(), v.clone());
    }
    for (k, v) in identity {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// The identity bootstrap: `CHAPPA_AI_PROCESS_ID`, `CHAPPA_AI_PROJECT_ID`,
/// `CHAPPA_AI_AGENT_TOOL_ID`, plus the spawn uuid marker.
pub fn identity_env(process_id: TermId, project_id: Option<u32>, tool_id: u32, uuid: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("CHAPPA_AI_PROCESS_ID".to_owned(), process_id.to_string()),
        ("CHAPPA_AI_AGENT_TOOL_ID".to_owned(), tool_id.to_string()),
        (SPAWN_UUID_ENV.to_owned(), uuid.to_owned()),
    ];
    if let Some(project) = project_id {
        env.insert(1, ("CHAPPA_AI_PROJECT_ID".to_owned(), project.to_string()));
    }
    env
}

/// The sentence orchestrators prepend to the first prompt, exactly like the
/// bootstrap the caller prepends to its first message.
pub fn agent_instructions(process_id: TermId, project: Option<(u32, &str)>) -> String {
    let project_part = match project {
        Some((id, name)) => format!(" in project \"{name}\" (project id {id})"),
        None => String::new(),
    };
    format!(
        "You are running as chappa-ai process {process_id}{project_part}. \
         chappa-ai MCP tools: {}.",
        MCP_TOOL_NAMES.join(", ")
    )
}

/// The exact wrapper script. `$$` is the NAMESPACE pid of `sh`, which
/// `exec` preserves; `"$0" "$@"` are the program + args passed after `-c`.
pub fn wrapper_script(uuid: &str) -> String {
    format!(
        "mkdir -p {PID_DIR} && echo $$ > {PID_DIR}/{uuid}.pid && exec \"$0\" \"$@\""
    )
}

/// Everything a spawn needs, derived from the tool + request. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// The env the CHILD sees (tool.env + identity; the pty layer applies it
    /// over the inherited process env). For docker every entry is also a
    /// `-e` flag in `args` — the client alone is useless.
    pub env: IndexMap<String, String>,
    pub nspid: NsPid,
}

/// Build the argv. `with_wrapper` is false when the container has no `sh`.
pub fn build_spawn_plan(
    tool: &AgentTool,
    extra_args: &[String],
    env: &IndexMap<String, String>,
    project_root: Option<PathBuf>,
    uuid: &str,
    with_wrapper: bool,
) -> SpawnPlan {
    let mut program_args: Vec<String> = tool.args.clone();
    program_args.extend(extra_args.iter().cloned()); // VERBATIM
    match &tool.runtime {
        Runtime::Host => SpawnPlan {
            command: tool.program.clone(),
            args: program_args,
            cwd: project_root,
            env: env.clone(),
            nspid: NsPid::Unresolved,
        },
        Runtime::DockerExec { .. } => {
            // `exec -u U -it -w W -e K=V… CONTAINER` — the same words the
            // display command line shows, minus the leading `docker`.
            let mut args = docker_exec_prefix(&tool.runtime, env);
            let nspid = if with_wrapper {
                args.push("sh".into());
                args.push("-c".into());
                args.push(wrapper_script(uuid));
                NsPid::PidFile
            } else {
                NsPid::Unresolved
            };
            args.push(tool.program.clone());
            args.extend(program_args);
            SpawnPlan {
                command: docker_program().to_owned(),
                args,
                // cwd = `-w` inside the container; the host client runs from
                // the project root when there is one.
                cwd: project_root,
                env: env.clone(),
                nspid,
            }
        }
    }
}

// ---- ready gate (pure) -------------------------------------------------------------

/// A snapshot of the actor's byte-flow counters, as the gate sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoView {
    pub has_output: bool,
    pub last_output_ms: Option<u64>,
    pub child_alive: bool,
}

impl IoView {
    pub fn of(handle: &TermHandle) -> Self {
        let io = handle.io();
        Self {
            has_output: io.has_output(),
            last_output_ms: io.last_output_ms(),
            child_alive: io.child_alive(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Pending,
    Ready,
    /// The quiet window never opened (a TUI that repaints continuously) but
    /// `max_wait_ms` have passed since the first byte: deliver anyway.
    ReadyByTimeout,
    Exited,
}

/// The gate's inputs beyond the io snapshot: when the first byte was seen
/// (`None` = still booting) and the two windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyGate {
    pub quiet_ms: u64,
    /// Continuous output since the first byte for this long counts as ready
    /// (`agent_ready_max_wait_ms`).
    pub max_wait_ms: u64,
    pub first_output_ms: Option<u64>,
}

/// Ready = `has_output` AND ≥ `quiet_ms` with no bytes from the child, OR
/// ≥ `max_wait_ms` since the first byte (the fallback for fast-repainting
/// TUIs that never go quiet). A dead child is `Exited` (checked first — a
/// prompt must never be written into a pty whose child is gone).
pub fn readiness(view: IoView, now_ms: u64, gate: ReadyGate) -> Readiness {
    if !view.child_alive {
        return Readiness::Exited;
    }
    if !view.has_output {
        return Readiness::Pending;
    }
    match view.last_output_ms {
        Some(last) if now_ms.saturating_sub(last) >= gate.quiet_ms => Readiness::Ready,
        _ => match gate.first_output_ms {
            Some(first) if now_ms.saturating_sub(first) >= gate.max_wait_ms => Readiness::ReadyByTimeout,
            _ => Readiness::Pending,
        },
    }
}

/// `prompt_receipt` on the spawn response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptReceipt {
    pub delivered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waited_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub had_output_within_ms: Option<bool>,
}

/// How a body may be written into a pty: the quiet window,
/// whether a TUI that never goes quiet may be written to anyway, and how long
/// the gate may be waited on at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryPolicy {
    pub quiet_ms: u64,
    /// `Some(ms)`: continuous output for this long since the first byte
    /// counts as ready (`agent_ready_max_wait_ms`, the SPAWN-PROMPT fallback
    /// for TUIs that repaint forever). `None`: only GENUINE quiet opens the
    /// gate — a timer body typed into an agent mid-turn is the bug, so a
    /// timer waits for quiet up to `cap` and then reports `not-ready`.
    pub max_wait_ms: Option<u64>,
    /// How long the gate may be waited on ([`PROMPT_WAIT_CAP`] for a queued
    /// spawn prompt, `timer_delivery_timeout_ms` for a timer firing).
    pub cap: Duration,
}

impl DeliveryPolicy {
    /// The queued spawn prompt: quiet, or ready-by-timeout.
    pub fn spawn_prompt(quiet_ms: u64, max_wait_ms: u64) -> Self {
        Self {
            quiet_ms,
            max_wait_ms: Some(max_wait_ms),
            cap: PROMPT_WAIT_CAP,
        }
    }

    /// A timer body: genuine quiet only, up to `cap`.
    pub fn quiet_only(quiet_ms: u64, cap: Duration) -> Self {
        Self {
            quiet_ms,
            max_wait_ms: None,
            cap,
        }
    }
}

/// One decision of the gate loop, pure over the clock so it is testable
/// without a pty: `None` = keep waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateStep {
    /// Write now. `by_timeout` = the `max_wait_ms` fallback opened the gate.
    Write { by_timeout: bool },
    /// Stop without writing: `"exited"` or `"not-ready"`.
    GiveUp(&'static str),
}

pub fn gate_step(
    view: IoView,
    now_ms: u64,
    first_output_ms: Option<u64>,
    policy: DeliveryPolicy,
    waited: Duration,
) -> Option<GateStep> {
    let gate = ReadyGate {
        quiet_ms: policy.quiet_ms,
        // `None` = the fallback never applies: `now - first >= u64::MAX` is
        // false for every real instant.
        max_wait_ms: policy.max_wait_ms.unwrap_or(u64::MAX),
        first_output_ms,
    };
    match readiness(view, now_ms, gate) {
        Readiness::Ready => Some(GateStep::Write { by_timeout: false }),
        Readiness::ReadyByTimeout => Some(GateStep::Write { by_timeout: true }),
        Readiness::Exited => Some(GateStep::GiveUp("exited")),
        Readiness::Pending if waited >= policy.cap => Some(GateStep::GiveUp("not-ready")),
        Readiness::Pending => None,
    }
}

/// Block until the gate opens (or the child exits / the cap passes), then
/// ONE atomic write of text + `\r` through the journaled send path.
///
/// `pub`: a TIMER body is delivered exactly this way (ready
/// gate → one atomic write → receipt), so the two paths cannot drift; the
/// [`DeliveryPolicy`] is the only difference between the two callers.
pub fn deliver_prompt_when_ready(
    registry: &Registry,
    id: TermId,
    prompt: &str,
    policy: DeliveryPolicy,
) -> PromptReceipt {
    let handle = match registry.handle(id) {
        Some(h) => h,
        None => {
            return PromptReceipt {
                delivered: false,
                reason: Some("exited".into()),
                waited_ms: None,
                seq_before: None,
                seq_after: None,
                had_output_within_ms: None,
            }
        }
    };
    let started = Instant::now();
    // The counters carry only the LAST byte's time; the first byte is
    // observed here (the gate polls every 25 ms, so it is at most that late).
    let mut first_output_ms: Option<u64> = None;
    let by_timeout = loop {
        let view = IoView::of(&handle);
        if first_output_ms.is_none() && view.has_output {
            first_output_ms = Some(epoch_ms());
        }
        match gate_step(view, epoch_ms(), first_output_ms, policy, started.elapsed()) {
            Some(GateStep::Write { by_timeout }) => break by_timeout,
            Some(GateStep::GiveUp(reason)) => {
                return PromptReceipt {
                    delivered: false,
                    reason: Some(reason.to_owned()),
                    waited_ms: Some(started.elapsed().as_millis() as u64),
                    seq_before: None,
                    seq_after: None,
                    had_output_within_ms: None,
                }
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    let payload = crate::control_http::submit_payload(prompt, true);
    let seq_before = handle.io().output_bytes();
    let written = registry.write_journaled(id, &payload, prompt.to_owned(), true);
    let watch = Instant::now() + Duration::from_millis(250);
    let mut seq_after = handle.io().output_bytes();
    while seq_after == seq_before && Instant::now() < watch {
        std::thread::sleep(Duration::from_millis(5));
        seq_after = handle.io().output_bytes();
    }
    PromptReceipt {
        delivered: written,
        reason: if !written {
            Some("exited".to_owned())
        } else if by_timeout {
            Some("ready-by-timeout".to_owned())
        } else {
            None
        },
        waited_ms: Some(started.elapsed().as_millis() as u64),
        seq_before: Some(seq_before),
        seq_after: Some(seq_after),
        had_output_within_ms: Some(seq_after != seq_before),
    }
}

// ---- busy guard (pure) --------------------------------------------------------------

/// Busy per the field-notes idle rule, made server-side. A child spawned
/// within the window counts as busy even before its first byte — otherwise
/// two spawns a second apart both pass a `max_busy = 1` guard because the
/// first has not printed a prompt yet.
pub fn is_busy(row: &AgentRow, now_ms: u64) -> bool {
    row.child_alive
        && (row
            .last_output_at
            .is_some_and(|t| now_ms.saturating_sub(t) <= BUSY_WINDOW_MS)
            || now_ms.saturating_sub(row.spawned_at_ms) <= BUSY_WINDOW_MS)
}

/// Refuse (naming the busy processes and their `last_output_at`) when the
/// tool's `max_busy` or the container's `max_busy_in_container` is reached.
/// `force` overrides — the caller logs it. `Ok(Some(msg))` = forced past a
/// limit (the message to log); `Ok(None)` = under every limit.
pub fn busy_guard(
    tool: &AgentTool,
    rows: &[AgentRow],
    now_ms: u64,
    force: bool,
) -> Result<Option<String>, String> {
    let describe = |r: &AgentRow| {
        format!(
            "{} (process {}, last_output_at {})",
            r.name,
            r.id,
            r.last_output_at.map(|t| t.to_string()).unwrap_or_else(|| "never".into())
        )
    };
    let mut violations = Vec::new();
    if let Some(max) = tool.max_busy {
        let busy: Vec<&AgentRow> = rows
            .iter()
            .filter(|r| r.tool_id == tool.id && is_busy(r, now_ms))
            .collect();
        if busy.len() as u32 >= max {
            violations.push(format!(
                "tool \"{}\" has {} busy agent(s), max_busy = {max}: {}",
                tool.name,
                busy.len(),
                busy.iter().map(|r| describe(r)).collect::<Vec<_>>().join("; ")
            ));
        }
    }
    if let Runtime::DockerExec {
        container,
        max_busy_in_container: Some(max),
        ..
    } = &tool.runtime
    {
        let busy: Vec<&AgentRow> = rows
            .iter()
            .filter(|r| r.container.as_deref() == Some(container.as_str()) && is_busy(r, now_ms))
            .collect();
        if busy.len() as u32 >= *max {
            violations.push(format!(
                "container \"{container}\" has {} busy agent(s), max_busy_in_container = {max}: {}",
                busy.len(),
                busy.iter().map(|r| describe(r)).collect::<Vec<_>>().join("; ")
            ));
        }
    }
    if violations.is_empty() {
        Ok(None)
    } else if force {
        Ok(Some(format!("forced past busy guard: {}", violations.join(" | "))))
    } else {
        Err(format!(
            "refused: {} (pass force=true to override)",
            violations.join(" | ")
        ))
    }
}

// ---- bridge derivation (pure) ------------------------------------------------------

/// Inputs the probe gathers for one docker-exec agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeInputs {
    pub container_status: ContainerStatus,
    /// Container `StartedAt` as unix ms, when parseable.
    pub container_started_ms: Option<u64>,
    pub spawned_at_ms: u64,
    pub last_output_ms: Option<u64>,
    pub now_ms: u64,
    pub stale_after_ms: u64,
    /// Whether a winsize poke produced bytes — `None` = no poke was needed or
    /// made (the quiet threshold was not reached).
    pub poke_produced_output: Option<bool>,
}

/// `container-down` = container status ≠ running, or `started_at` newer than
/// the spawn (restarted under us). `stale` = running, quiet ≥ threshold AND
/// a poke produced nothing. `unknown` status never flips anything. The
/// poke's "needed?" question is [`stale_candidate`].
pub fn derive_bridge(i: BridgeInputs) -> Bridge {
    match i.container_status {
        ContainerStatus::Unknown => {}
        ContainerStatus::Exited | ContainerStatus::Missing => return Bridge::ContainerDown,
        ContainerStatus::Running => {
            if i.container_started_ms.is_some_and(|s| s > i.spawned_at_ms) {
                return Bridge::ContainerDown;
            }
        }
    }
    if i.container_status == ContainerStatus::Running
        && stale_candidate(i.last_output_ms, i.now_ms, i.stale_after_ms)
        && i.poke_produced_output == Some(false)
    {
        return Bridge::Stale;
    }
    Bridge::Ok
}

/// Quiet for ≥ the threshold (a booting agent with no output yet counts from
/// nothing — it is not stale, it is booting).
pub fn stale_candidate(last_output_ms: Option<u64>, now_ms: u64, stale_after_ms: u64) -> bool {
    last_output_ms.is_some_and(|last| now_ms.saturating_sub(last) >= stale_after_ms)
}

/// `docker inspect -f '{{.State.Status}} {{.State.StartedAt}}' C` → state.
pub fn inspect_container(docker: &dyn DockerCli, container: &str) -> ContainerState {
    let args: Vec<String> = vec![
        "inspect".into(),
        "-f".into(),
        "{{.State.Status}} {{.State.StartedAt}}".into(),
        container.into(),
    ];
    match docker.run(&args, DOCKER_CALL_TIMEOUT) {
        Ok(out) if out.success => parse_inspect(&out.stdout, epoch_ms()),
        Ok(out) if out.stderr.contains("No such") || out.stdout.contains("No such") => ContainerState {
            status: ContainerStatus::Missing,
            started_at: None,
            uptime_s: None,
            mem_bytes: None,
            mem_limit_bytes: None,
        },
        _ => ContainerState::unknown(),
    }
}

/// Pure half of [`inspect_container`].
pub fn parse_inspect(stdout: &str, now_ms: u64) -> ContainerState {
    let mut words = stdout.split_whitespace();
    let status = match words.next() {
        Some("running") => ContainerStatus::Running,
        Some("exited") | Some("dead") | Some("created") | Some("paused") | Some("removing") => {
            ContainerStatus::Exited
        }
        Some(_) => ContainerStatus::Unknown,
        None => ContainerStatus::Unknown,
    };
    let started_at = words.next().map(str::to_owned);
    let started_ms = started_at.as_deref().and_then(parse_rfc3339_ms);
    ContainerState {
        status,
        uptime_s: match status {
            ContainerStatus::Running => started_ms.map(|s| now_ms.saturating_sub(s) / 1000),
            _ => None,
        },
        started_at,
        mem_bytes: None,
        mem_limit_bytes: None,
    }
}

/// `docker stats --no-stream --format json C` → (mem_bytes, mem_limit_bytes).
/// Omitted-not-failed: any problem is `None`.
pub fn container_memory(docker: &dyn DockerCli, container: &str) -> Option<(u64, u64)> {
    let args: Vec<String> = vec![
        "stats".into(),
        "--no-stream".into(),
        "--format".into(),
        "json".into(),
        container.into(),
    ];
    let out = docker.run(&args, DOCKER_CALL_TIMEOUT).ok()?;
    if !out.success {
        return None;
    }
    parse_stats_mem(&out.stdout)
}

/// `{"MemUsage":"1.5GiB / 3GiB", …}` → bytes. Pure.
pub fn parse_stats_mem(stdout: &str) -> Option<(u64, u64)> {
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'))?;
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let usage = value.get("MemUsage")?.as_str()?;
    let (used, limit) = usage.split_once('/')?;
    Some((parse_size(used.trim())?, parse_size(limit.trim())?))
}

/// `1.5GiB`, `512MiB`, `3GB`, `1024B` → bytes (docker's humanize units).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "KiB" => 1024.0,
        "MB" => 1e6,
        "MiB" => 1024.0 * 1024.0,
        "GB" => 1e9,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1e12,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult).round() as u64)
}

/// Minimal RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`) → unix ms. No
/// chrono: docker's timestamps are the only input and always this shape.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let mut padded = frac.to_owned();
        padded.truncate(3);
        while padded.len() < 3 {
            padded.push('0');
        }
        millis = padded.parse().ok()?;
    }
    let offset_s: i64 = match b.get(i) {
        Some(&b'Z') | None => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let oh = num(i + 1, i + 3)?;
            let om = num(i + 4, i + 6)?;
            let off = oh * 3600 + om * 60;
            if sign == b'+' {
                off
            } else {
                -off
            }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_s;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000 + millis as u64)
}

// ---- close sequence ------------------------------------------------------------------

/// Signal a namespace pid through the SHELL BUILTIN `kill`, never a bare
/// `kill` executable: worker images routinely lack procps/util-linux (the
/// slim worker images often have no /bin/kill — a bare `docker exec … kill`
/// failed with exit 127, the result was discarded, and the close honestly
/// reported `still-present` while the in-container bash lived on; found live
/// 2026-09-02, six orphans). The wrapper already requires `sh`, so this adds
/// no dependency; the pid rides as `$1` so the script is a fixed string the
/// tests can pin. `user` runs the exec as the tool's user (the
/// reaper — which only ever sees the orphan's environ as the owning user —
/// signals as that same user; `None` = the default exec user, the close path).
fn signal_args(container: &str, user: Option<&str>, signal: &str, pid: u32) -> Vec<String> {
    let script = format!("kill -{signal} \"$1\"");
    exec_args_user(container, user, &["sh", "-c", &script, "kill", &pid.to_string()])
}

fn exec_args(container: &str, cmd: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into(), container.into()];
    args.extend(cmd.iter().map(|s| s.to_string()));
    args
}

/// `docker exec [-u <user>] container …` — the reaper's execs run AS the
/// tool's user: measured 2026-09-02 in a live container, the
/// container ROOT gets `Permission denied` reading another user's
/// `/proc/<pid>/environ` when Docker's default capability set has no
/// `CAP_SYS_PTRACE`, while the owning user reads its own processes fine.
/// So the orphan scan and its signalling run as the same user that spawned
/// the process.
fn exec_args_user(container: &str, user: Option<&str>, cmd: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(user) = user {
        args.push("-u".into());
        args.push(user.to_owned());
    }
    args.push(container.into());
    args.extend(cmd.iter().map(|s| s.to_string()));
    args
}

/// The tool's docker user, if its runtime is docker-exec (the user it
/// spawned processes as — the only user that can read their environ).
fn runtime_user(runtime: &Runtime) -> Option<&str> {
    match runtime {
        Runtime::DockerExec { user, .. } => user.as_deref(),
        Runtime::Host => None,
    }
}

/// Resolve the NAMESPACE pid: the wrapper's pid file, or (no `sh` at spawn)
/// an exact `CHAPPA_AI_SPAWN_UUID=<uuid>` match over `/proc/*/environ`.
/// NEVER `docker top` — those are HOST pids. The environ fallback runs
/// as the TOOL's user: see [`exec_args_user`] — a non-root tool
/// whose process lives under another uid cannot be matched by a default-user
/// exec. The pid-file path reads as the default user (the file lives in
/// world-readable `/tmp/.chappa-ai`).
pub fn resolve_nspid(docker: &dyn DockerCli, meta: &AgentMeta) -> Option<u32> {
    let container = meta.container_name()?;
    let out = match meta.nspid {
        NsPid::PidFile => docker.run(&exec_args(container, &["cat", &meta.pid_file()]), DOCKER_CALL_TIMEOUT),
        NsPid::Unresolved => {
            let script = format!(
                "for p in /proc/[0-9]*; do if tr '\\0' '\\n' < \"$p/environ\" 2>/dev/null | grep -qx '{SPAWN_UUID_ENV}={}'; then echo \"${{p#/proc/}}\"; fi; done",
                meta.spawn_uuid
            );
            docker.run(
                &exec_args_user(container, runtime_user(&meta.runtime), &["sh", "-c", &script]),
                DOCKER_CALL_TIMEOUT,
            )
        }
    };
    let out = out.ok()?;
    if !out.success {
        return None;
    }
    // The environ match can list several pids (the wrapper-less process and
    // its children); the smallest is the root we exec'd.
    out.stdout
        .split_whitespace()
        .filter_map(|w| w.parse::<u32>().ok())
        .min()
}

/// `test -d /proc/<pid>` inside the container.
fn nspid_present(docker: &dyn DockerCli, container: &str, pid: u32) -> Option<bool> {
    let out = docker
        .run(&exec_args(container, &["test", "-d", &format!("/proc/{pid}")]), DOCKER_CALL_TIMEOUT)
        .ok()?;
    Some(out.success)
}

/// Poll `test -d /proc/<pid>` until it is gone or `grace` passes. `None` =
/// docker itself stopped answering.
fn wait_nspid_gone(docker: &dyn DockerCli, container: &str, pid: u32, grace: Duration) -> Option<bool> {
    let deadline = Instant::now() + grace;
    loop {
        match nspid_present(docker, container, pid)? {
            false => return Some(true),
            true if Instant::now() >= deadline => return Some(false),
            true => std::thread::sleep(Duration::from_millis(250).min(grace)),
        }
    }
}

/// Container-side steps over an ALREADY-RESOLVED namespace pid: TERM →
/// poll ≤ `term_grace` → KILL → poll ≤ `kill_grace` → verdict. Shared by
/// close (`kill_container_side`) and the orphan reaper (`reap_orphans`),
/// which have no `AgentMeta` between them — just a container and a pid (the
/// reaper's orphan root). Does NOT touch the host client, does NOT remove
/// the pid file, and does NOT inspect the container (callers do that once,
/// so the reaper can short-circuit a down container before its first orphan
/// and the close keeps its pinned `inspect → cat → kill → test → rm` order).
pub fn reap_pid(
    docker: &dyn DockerCli,
    container: &str,
    user: Option<&str>,
    pid: u32,
    term_grace: Duration,
    kill_grace: Duration,
) -> CloseVerification {
    let _ = docker.run(&signal_args(container, user, "TERM", pid), DOCKER_CALL_TIMEOUT);
    match wait_nspid_gone(docker, container, pid, term_grace) {
        Some(true) => return CloseVerification::Gone,
        Some(false) => {}
        None => return CloseVerification::Unresolved,
    }
    let _ = docker.run(&signal_args(container, user, "KILL", pid), DOCKER_CALL_TIMEOUT);
    match wait_nspid_gone(docker, container, pid, kill_grace) {
        Some(true) => CloseVerification::Gone,
        Some(false) => CloseVerification::StillPresent,
        None => CloseVerification::Unresolved,
    }
}

/// Container-side steps: inspect → (down? short-circuit) → read nspid →
/// [`reap_pid`] → verdict. Does NOT touch the host client and does NOT remove
/// the pid file — the registry does those after, in that order (client killed
/// LAST, then the pid file goes).
pub fn kill_container_side(
    docker: &dyn DockerCli,
    meta: &AgentMeta,
    term_grace: Duration,
    kill_grace: Duration,
) -> CloseVerification {
    let Some(container) = meta.container_name() else {
        return CloseVerification::Unresolved;
    };
    let state = inspect_container(docker, container);
    if matches!(state.status, ContainerStatus::Exited | ContainerStatus::Missing) {
        return CloseVerification::ContainerDown;
    }
    let Some(pid) = resolve_nspid(docker, meta) else {
        return CloseVerification::Unresolved;
    };
    // Signal/poll as the DEFAULT exec user here: the close path resolves the
    // pid (pid file read as root; the environ fallback now honours the tool
    // user — see `resolve_nspid`) and root can signal/poll any process, so
    // the close keeps its long-pinned docker args. Only the REAPER (which
    // must run AS the owner to read its environ) signals as the tool user.
    reap_pid(docker, container, None, pid, term_grace, kill_grace)
}

/// Remove the wrapper's pid file (after the client is gone). Skipped when
/// there is no file (`nspid: unresolved`), when the container is down
/// (nothing to clean, the exec would only fail) and when the verdict is
/// `unresolved` (docker is not answering — one more call would only hang).
pub fn remove_pid_file(docker: &dyn DockerCli, meta: &AgentMeta, verdict: CloseVerification) {
    if meta.nspid != NsPid::PidFile
        || matches!(verdict, CloseVerification::ContainerDown | CloseVerification::Unresolved)
    {
        return;
    }
    if let Some(container) = meta.container_name() {
        let _ = docker.run(&exec_args(container, &["rm", "-f", &meta.pid_file()]), DOCKER_CALL_TIMEOUT);
    }
}

/// Does the container have `sh`? One 2 s call before a docker spawn; a
/// missing shell means no wrapper (`nspid: unresolved`). Docker unreachable
/// counts as "no wrapper" too — the spawn itself will then fail visibly.
pub fn container_has_sh(docker: &dyn DockerCli, container: &str) -> bool {
    docker
        .run(&exec_args(container, &["sh", "-c", "exit 0"]), DOCKER_CALL_TIMEOUT)
        .map(|o| o.success)
        .unwrap_or(false)
}

// ---- docker orphan reaper ----------------------------------------------------

/// One marker-bearing container process the scan found: its namespace pid,
/// its spawn uuid, and an approximate spawn time (Unix ms, from the pid's
/// `/proc/stat` starttime relative to `/proc/stat` `btime`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRow {
    pub pid: u32,
    pub uuid: String,
    pub spawned_at_ms: u64,
}

/// An orphan ROOT: the marker's uuid no live registry entry owns. `pid` is
/// the SMALLEST of the uuid's matching pids (the exec'd root — the wrapper
/// `exec`s into the program so it is the same pid; children die with the
/// root or get a later sweep).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub pid: u32,
    pub uuid: String,
}

/// What the control route answers per orphan (the sweep's per-pid result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReapResult {
    pub container: String,
    pub user: Option<String>,
    pub pid: u32,
    pub uuid: String,
    pub verdict: CloseVerification,
}

/// The pure orphan classifier: a uuid present in `live_uuids` is
/// never an orphan; an UNKNOWN uuid whose process was spawned within
/// `busy_window_ms` of `now_ms` is skipped — its registry entry may still be
/// materializing (the environ appears before `create_agent_terminal`
/// returns); otherwise the SMALLEST pid per unknown uuid is the orphan root.
/// Platform-neutral: the caller supplies the scan rows (and their spawn
/// times), so tests need no docker or `/proc`.
pub fn orphans(scan_rows: &[ScanRow], live_uuids: &HashSet<String>, busy_window_ms: u64, now_ms: u64) -> Vec<Orphan> {
    // BTreeMap gives a deterministic (and test-friendly) ordering by uuid.
    let mut roots: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for row in scan_rows {
        if live_uuids.contains(&row.uuid) {
            continue;
        }
        if now_ms.saturating_sub(row.spawned_at_ms) < busy_window_ms {
            continue; // inside the busy window: possibly a still-materializing spawn
        }
        let entry = roots.entry(row.uuid.as_str()).or_insert(row.pid);
        if row.pid < *entry {
            *entry = row.pid;
        }
    }
    roots
        .into_iter()
        .map(|(uuid, pid)| Orphan {
            pid,
            uuid: uuid.to_owned(),
        })
        .collect()
}

/// The in-container scan script: walk every `/proc/[0-9]*`, print
/// `<pid> <uuid> <spawned_at_ms>` for each whose environ carries a
/// `CHAPPA_AI_SPAWN_UUID` marker. It must run AS the tool's user — the
/// container ROOT gets `Permission denied` reading another user's environ
/// (Docker's default caps have no `CAP_SYS_PTRACE`). Spawn time is
/// approximated from the pid's `/proc/stat` starttime (userland clock ticks,
/// standard 100 Hz) over `/proc/stat` `btime` (boot epoch seconds) — the
/// busy-window grace needs a "is this fresh?" signal per pid. Fixed string so
/// the FakeDocker sequence can pin it.
pub const ORPHAN_SCAN_SCRIPT: &str = "btime=$(awk '/^btime/{print $2}' /proc/stat 2>/dev/null); btime=${btime:-0}; \
for p in /proc/[0-9]*; do line=$(tr '\\0' '\\n' < \"$p/environ\" 2>/dev/null | grep '^CHAPPA_AI_SPAWN_UUID=' | head -n 1); \
[ -z \"$line\" ] && continue; u=${line#CHAPPA_AI_SPAWN_UUID=}; \
s=$(awk '{print $22}' \"$p/stat\" 2>/dev/null); echo \"${p#/proc/} $u $(( btime*1000 + ${s:-0}*10 ))\"; done";

/// One scan of a container as a user: every marker-bearing process's pid,
/// uuid and spawn time. `None` = docker unreachable or the exec failed
/// (skip silently — the bridge probe already reports down/unreachable).
pub fn scan_markers(docker: &dyn DockerCli, container: &str, user: Option<&str>) -> Option<Vec<ScanRow>> {
    let out = docker
        .run(&exec_args_user(container, user, &["sh", "-c", ORPHAN_SCAN_SCRIPT]), DOCKER_CALL_TIMEOUT)
        .ok()?;
    if !out.success {
        return None;
    }
    Some(parse_scan(&out.stdout))
}

/// Pure half of [`scan_markers`]: `<pid> <uuid> <spawned_at_ms>` per line.
pub fn parse_scan(stdout: &str) -> Vec<ScanRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse::<u32>().ok()?;
            let uuid = it.next()?.to_owned();
            if uuid.is_empty() {
                return None;
            }
            let spawned_at_ms = it.next().and_then(|w| w.parse::<u64>().ok()).unwrap_or(0);
            Some(ScanRow { pid, uuid, spawned_at_ms })
        })
        .collect()
}

/// The distinct (container, user) pairs across ENABLED docker-exec tools —
/// the sweep's unit (one scan + signalling set per pair, since the exec must
/// run as the owning user).
fn distinct_docker_users(tools: &AgentToolsState) -> Vec<(String, Option<String>)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tool in tools.list() {
        if !tool.enabled {
            continue;
        }
        let Runtime::DockerExec { container, user, .. } = &tool.runtime else {
            continue;
        };
        let key = (container.clone(), user.clone());
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

/// The wrapper's pid file for an arbitrary uuid (the row is long gone, so
/// there is no `AgentMeta` to read the path from).
fn orphan_pid_file(uuid: &str) -> String {
    format!("{PID_DIR}/{uuid}.pid")
}

/// One full sweep: every enabled docker_exec tool's distinct
/// (container, user) pair is inspected once, scanned as that user, and each
/// orphan ROOT is reaped through the shared TERM/poll/KILL sequence with its
/// pid file removed. Holding `spawn_guard` for the whole sweep serialises it
/// against in-progress spawns (whose environ appears before their entry
/// materializes — the busy window is the second line of defence). `containers`
/// restricts the sweep to a subset (the bridge probe's `ProbeTarget`s); `None`
/// sweeps every enabled tool. `emit_notify` gets `(container, count)` ONCE per
/// container that reaped anything. The EXPLICIT route (`POST /agents/reap`)
/// calls this regardless of the `agentReapOrphans` setting — the setting gates
/// only the startup/probe periodic sweeps.
pub fn reap_orphans(
    registry: &Registry,
    tools: &AgentToolsState,
    containers: Option<&HashSet<String>>,
    emit_notify: Option<&(dyn Fn(&str, usize) + Send + Sync)>,
) -> Vec<ReapResult> {
    let docker = registry.docker();
    // Bind the Arc before locking: `spawn_guard()` hands back a clone, so
    // locking the temporary directly would drop it while the guard lives
    // (E0716). Same shape as the spawn path's own hold.
    let spawn_guard = registry.spawn_guard();
    let _serialized = spawn_guard.lock().unwrap_or_else(|e| e.into_inner());
    let now_ms = epoch_ms();
    let live: HashSet<String> = registry.agent_rows().into_iter().map(|r| r.spawn_uuid).collect();
    let mut out = Vec::new();
    let mut reaped_per_container: HashMap<String, usize> = HashMap::new();
    for (container, user) in distinct_docker_users(tools) {
        if let Some(wanted) = containers {
            if !wanted.contains(&container) {
                continue;
            }
        }
        let state = inspect_container(docker.as_ref(), &container);
        if state.status != ContainerStatus::Running {
            // Down, missing, or docker unreachable: nothing to sweep.
            continue;
        }
        let Some(scan) = scan_markers(docker.as_ref(), &container, user.as_deref()) else {
            continue;
        };
        for orph in orphans(&scan, &live, BUSY_WINDOW_MS, now_ms) {
            let verdict = reap_pid(docker.as_ref(), &container, user.as_deref(), orph.pid, TERM_GRACE, KILL_GRACE);
            if verdict == CloseVerification::Gone {
                let _ = docker.run(
                    &exec_args_user(&container, user.as_deref(), &["rm", "-f", &orphan_pid_file(&orph.uuid)]),
                    DOCKER_CALL_TIMEOUT,
                );
            }
            eprintln!(
                "[agents] reap orphan pid={} uuid={} container={} user={:?} verdict={}",
                orph.pid,
                orph.uuid,
                container,
                user,
                verdict.as_str()
            );
            let result = ReapResult {
                container: container.clone(),
                user: user.clone(),
                pid: orph.pid,
                uuid: orph.uuid,
                verdict,
            };
            if verdict == CloseVerification::Gone {
                *reaped_per_container.entry(container.clone()).or_insert(0) += 1;
            }
            out.push(result);
        }
    }
    for (container, count) in &reaped_per_container {
        if let Some(emit) = emit_notify {
            emit(container, *count);
        }
    }
    out
}


// ---- bridge probe thread ---------------------------------------------------------------

/// One docker-exec agent the probe watches (a registry projection).
#[derive(Clone)]
pub struct ProbeTarget {
    pub id: TermId,
    pub container: String,
    pub spawned_at_ms: u64,
    pub handle: TermHandle,
}

/// The `agent://bridge` payload (once per transition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeEvent {
    pub id: TermId,
    pub bridge: Bridge,
    pub container: ContainerState,
}

/// Start the one probe thread. Parks while no docker-exec agent is alive
/// (the registry wakes it on an agent spawn). `emit` receives each
/// transition — the app wires it to the global `agent://bridge` event.
/// Each tick ALSO runs one orphan sweep over the containers the
/// probe already inspects (`ProbeTarget`s), gated by the `agentReapOrphans`
/// setting and surfaced via `reap_emit` — so an orphan from a mid-session
/// crash of a close is caught within a probe period.
pub fn spawn_bridge_probe(
    registry: Registry,
    settings: SettingsState,
    tools: AgentToolsState,
    emit: Arc<dyn Fn(BridgeEvent) + Send + Sync>,
    reap_emit: Arc<dyn Fn(&str, usize) + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let waker = registry.bridge_waker();
        loop {
            let targets = registry.probe_targets();
            if targets.is_empty() {
                waker.park();
                continue;
            }
            let stale_after_ms = u64::from(settings.snapshot().agent_stale_after_s) * 1000;
            let docker = registry.docker();
            for event in probe_once(docker.as_ref(), &targets, stale_after_ms, epoch_ms()) {
                if registry.update_bridge(event.id, event.container.clone(), event.bridge) {
                    emit(event);
                }
            }
            if settings.snapshot().agent_reap_orphans {
                let probe_containers: HashSet<String> = targets.iter().map(|t| t.container.clone()).collect();
                reap_orphans(&registry, &tools, Some(&probe_containers), Some(&*reap_emit));
            }
            std::thread::sleep(PROBE_PERIOD);
        }
    })
}

/// One probe tick: inspect each distinct container ONCE, derive per agent,
/// poke only the stale candidates. Returns the (possibly unchanged) state per
/// agent; the registry decides what is a transition.
pub fn probe_once(docker: &dyn DockerCli, targets: &[ProbeTarget], stale_after_ms: u64, now_ms: u64) -> Vec<BridgeEvent> {
    let containers: HashSet<&str> = targets.iter().map(|t| t.container.as_str()).collect();
    let states: HashMap<&str, ContainerState> = containers
        .into_iter()
        .map(|c| (c, inspect_container(docker, c)))
        .collect();
    let mut out = Vec::new();
    for t in targets {
        let state = states.get(t.container.as_str()).cloned().unwrap_or_else(ContainerState::unknown);
        let last_output_ms = t.handle.io().last_output_ms();
        let poke = if state.status == ContainerStatus::Running
            && stale_candidate(last_output_ms, now_ms, stale_after_ms)
        {
            Some(poke_produces_output(&t.handle))
        } else {
            None
        };
        let bridge = derive_bridge(BridgeInputs {
            container_status: state.status,
            container_started_ms: state.started_at.as_deref().and_then(parse_rfc3339_ms),
            spawned_at_ms: t.spawned_at_ms,
            last_output_ms,
            now_ms,
            stale_after_ms,
            poke_produced_output: poke,
        });
        out.push(BridgeEvent {
            id: t.id,
            bridge,
            container: state,
        });
    }
    out
}

/// A zero-byte winsize poke (`TermHandle::poke`: a pty-level rows-1 / back
/// wiggle, grid untouched — a same-size `resize` is a no-op in term-core and
/// would flag every quiet agent) and a wait on the byte counter for
/// [`POKE_WAIT`].
pub fn poke_produces_output(handle: &TermHandle) -> bool {
    let before = handle.io().output_bytes();
    handle.poke();
    handle.io().wait_for_output(before, POKE_WAIT) != before
}

// ---- spawn ----------------------------------------------------------------------------

/// Resolve + validate the parent the rail nests a spawned agent
/// under. Resolution order: an explicit request field → the control actor (a
/// numeric id) → `None`. A candidate is recorded ONLY when it names a live
/// registry entry at spawn time (any kind — a plain hand-started `claude`
/// shell is a legitimate parent; the identity env is what makes it an actor),
/// and never the agent's own reserved id. A stale, bogus or `user`/unparsable
/// actor yields `None` silently. The trust model is the one
/// (unauthenticated attribution via `X-Chappa-Actor`) — this is a hint for
/// the rail, never authorization.
fn resolve_parent_process_id(
    explicit: Option<TermId>,
    actor: Option<TermId>,
    id: TermId,
    entry_exists: impl Fn(TermId) -> bool,
) -> Option<TermId> {
    explicit
        .or(actor)
        .filter(|p| *p != id && entry_exists(*p))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SpawnAgentRequest {
    pub agent_tool_id: u32,
    #[serde(default)]
    pub project_id: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    /// Appended to the tool's args VERBATIM.
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// An EXPLICIT parent-process override for the rail's nesting.
    /// Resolution order in the spawn: this field → the control actor (when it
    /// parses as a numeric id) → `None`. The candidate is still VALIDATED at
    /// spawn time (a live registry entry of any kind; never the agent itself;
    /// stale/bogus/`user` → `None` silently) — the trust model is the
    /// unauthenticated one, so this is a hint for the rail, never auth.
    #[serde(default)]
    pub parent_process_id: Option<TermId>,
    /// When the child process EXITS, the registry closes this entry
    /// through the same path `close_terminal` uses (nothing bespoke) — so an
    /// orchestrator can fire many short-lived workers without leaving a column
    /// of dead rows. A NON-ZERO exit closes too (the failure travelled in the
    /// `get_process_output`, not in a lingering row). Default
    /// false. NB for `bash`-hosted tools (chappa-a/b): they never exit on
    /// their own — end the command line with `; exit` (or spawn with
    /// `extra_args ["-lc", "<command>"]`) so the row reaps itself when the run
    /// returns; there is deliberately no idle-based close here.
    #[serde(default)]
    pub close_on_exit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpawnAgentResponse {
    pub process_id: TermId,
    /// Same value — control-surface callers read `process_id`, the rail reads `term_id`.
    pub term_id: TermId,
    pub name: String,
    pub agent_instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_receipt: Option<PromptReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerState>,
    pub agent: AgentMeta,
    /// The busy-guard override note when `force` was needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced: Option<String>,
}

/// Everything a spawn reaches for.
pub struct SpawnContext<'a> {
    pub registry: &'a Registry,
    pub tools: &'a AgentToolsState,
    pub settings: &'a SettingsState,
    pub projects: &'a Projects,
    /// The SPAWNING actor's process id (the control route parses the
    /// `X-Chappa-Actor` header; `None` for the Tauri command route / a
    /// `user` / unparsable actor). This is the default parent candidate when
    /// the request carries no explicit `parent_process_id`.
    pub actor: Option<TermId>,
}

/// The one spawn path (Tauri command, control route and MCP tool all land
/// here). `sink` is the frames channel (or `NullSink` for agent-spawned
/// agents).
pub fn spawn_agent_with(
    ctx: SpawnContext<'_>,
    req: SpawnAgentRequest,
    sink: Arc<dyn EventSink>,
    on_notify: Option<AppHandle>,
) -> Result<SpawnAgentResponse, String> {
    let tool = ctx
        .tools
        .get(req.agent_tool_id)
        .ok_or_else(|| format!("no such agent tool: {}", req.agent_tool_id))?;
    if !tool.enabled {
        return Err(format!("agent tool \"{}\" ({}) is disabled", tool.name, tool.id));
    }
    let settings = ctx.settings.snapshot();
    let project = match req.project_id {
        Some(id) => Some(
            ctx.projects
                .project_root(id)
                .ok_or_else(|| format!("no such project: {id}"))?,
        ),
        None => None,
    };
    let uuid = new_uuid()?;
    // The guard check and the entry that makes the NEXT check see this
    // spawn happen under one lock: two concurrent spawns can otherwise both
    // read "0 busy" and both pass a `max_busy = 1` limit.
    let spawn_guard = ctx.registry.spawn_guard();
    let _serialized = spawn_guard.lock().unwrap_or_else(|e| e.into_inner());
    let now = epoch_ms();
    let forced = busy_guard(&tool, &ctx.registry.agent_rows(), now, req.force)?;
    if let Some(note) = &forced {
        eprintln!("[agents] {note}");
    }
    let docker = ctx.registry.docker();
    let with_wrapper = match tool.runtime.container() {
        Some(container) => container_has_sh(docker.as_ref(), container),
        None => false,
    };
    let id = ctx.registry.reserve_id();
    // Resolve + validate the parent the rail will nest this agent
    // under (see [`resolve_parent_process_id`] — pure and unit-tested).
    let parent_process_id = resolve_parent_process_id(
        req.parent_process_id,
        ctx.actor,
        id,
        |p| ctx.registry.handle(p).is_some(),
    );
    let identity = identity_env(id, req.project_id, tool.id, &uuid);
    let env = merge_env(&[], &tool.env, &identity);
    // A json-transport tool is planned as its machine-mode
    // variant (flags prepended, docker `-i` only — a pipe, not a tty).
    let json_mode = match tool.transport {
        Transport::Json => Some(machine_mode(tool.tool_type).ok_or_else(|| {
            format!(
                "agent tool \"{}\" uses the json transport but type \"{}\" has no machine mode",
                tool.name,
                tool.tool_type.as_str()
            )
        })?),
        Transport::Tty => None,
    };
    let planned_tool = match json_mode {
        Some(mode) => json_planned_tool(&tool, mode),
        None => tool.clone(),
    };
    let plan = build_spawn_plan(
        &planned_tool,
        &req.extra_args,
        &env,
        project.as_ref().map(|(_, root)| root.clone()),
        &uuid,
        with_wrapper,
    );
    let cols = req.cols.unwrap_or(80).clamp(1, 4096);
    let rows = req.rows.unwrap_or(24).clamp(1, 4096);
    let cfg = ActorConfig {
        spec: PtySpec {
            command: plan.command.clone(),
            args: plan.args.clone(),
            cwd: plan.cwd.clone(),
            env: plan.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            cols,
            rows,
        },
        scrollback_lines: 10_000,
        synthetic_prompt_marks: settings.synthetic_prompt_marks,
        wheel_speed: settings.scroll_wheel_speed,
    };
    let name = req.name.clone().unwrap_or_else(|| tool.name.clone());
    let meta = AgentMeta {
        tool_id: tool.id,
        tool_type: tool.tool_type,
        model: tool.model.clone(),
        runtime: tool.runtime.clone(),
        project_id: req.project_id,
        parent_process_id,
        spawned_at_ms: now,
        spawn_uuid: uuid,
        nspid: plan.nspid,
        container: None,
        bridge: Bridge::Ok,
        transport: tool.transport,
        awaiting_input: false,
    };
    // Capture the broadcast app handle BEFORE the json-agent match
    // below moves `on_notify` (the emit is after it). A cheap Arc clone.
    let created_app = on_notify.clone();
    let json_agent: Option<Arc<JsonAgent>> = match json_mode {
        None => {
            ctx.registry
                .create_agent_terminal(cfg, sink, on_notify, name.clone(), id, meta.clone(), req.close_on_exit)?;
            None
        }
        Some(mode) => {
            let handle = ctx.registry.create_json_agent_terminal(
                cfg,
                sink,
                on_notify.clone(),
                name.clone(),
                id,
                meta.clone(),
                req.close_on_exit,
            )?;
            let recipe = SpawnRecipe {
                command: plan.command.clone(),
                args: plan.args.clone(),
                cwd: plan.cwd.clone(),
                env: plan.env.clone(),
            };
            let agent = JsonAgent::new(id, handle, recipe, mode, json_hooks(on_notify));
            ctx.registry.attach_json_agent(id, agent.clone());
            Some(agent)
        }
    };
    // The entry exists: the next spawn's guard sees it. Everything below is
    // observation (inspect/stats) and the prompt wait — no lock needed.
    drop(_serialized);
    let instructions = agent_instructions(id, project.as_ref().map(|(n, _)| (req.project_id.unwrap_or_default(), n.as_str())));
    let container = tool.runtime.container().map(|c| {
        let mut state = inspect_container(docker.as_ref(), c);
        if let Some((mem, limit)) = container_memory(docker.as_ref(), c) {
            state.mem_bytes = Some(mem);
            state.mem_limit_bytes = Some(limit);
        }
        state
    });
    if let Some(state) = &container {
        ctx.registry.update_bridge(id, state.clone(), Bridge::Ok);
    }
    let prompt = req.prompt.as_deref().filter(|p| !p.is_empty());
    let prompt_receipt = match &json_agent {
        Some(agent) => match json_boot(ctx.registry, agent, prompt) {
            Ok(receipt) => receipt,
            Err(err) => {
                // The child never started: no entry to leave behind.
                ctx.registry.close(id);
                return Err(err);
            }
        },
        None => prompt.map(|p| {
            deliver_prompt_when_ready(
                ctx.registry,
                id,
                p,
                DeliveryPolicy::spawn_prompt(
                    u64::from(settings.agent_ready_quiet_ms),
                    u64::from(settings.agent_ready_max_wait_ms),
                ),
            )
        }),
    };
    // The webview adopts every backend-spawned agent into the rail
    // off this broadcast (project-bound agents to their project's AGENTS
    // subsection, unbound ones to the workspace list). `on_notify` is the
    // app handle (None in tests/harness → a silent no-op).
    if let Some(app) = created_app.as_ref() {
        emit_created(
            &AppCreatedBroadcast(app.clone()),
            CreatedEvent {
                term_id: id,
                name: name.clone(),
                kind: "agent",
                project_id: req.project_id,
                agent_tool_id: Some(tool.id),
                parent_process_id,
            },
        );
    }
    let agent = ctx.registry.agent_meta(id).unwrap_or(meta);
    Ok(SpawnAgentResponse {
        process_id: id,
        term_id: id,
        name,
        agent_instructions: instructions,
        prompt_receipt,
        container,
        agent,
        forced,
    })
}

// ---- Json transport glue -------------------------------------------------------

/// The tool as the json transport runs it: machine-mode flags BEFORE the
/// tool's own args (`opencode run --format json -m …`, `claude -p
/// --output-format stream-json … --model …`), and a docker runtime without
/// `-t` (a pipe must not be a tty — the CLI would render, not stream).
pub fn json_planned_tool(tool: &AgentTool, mode: crate::agent_json::MachineMode) -> AgentTool {
    let mut planned = tool.clone();
    let mut args: Vec<String> = mode.args.iter().map(|s| s.to_string()).collect();
    args.extend(tool.args.iter().cloned());
    planned.args = args;
    if let Runtime::DockerExec { tty, .. } = &mut planned.runtime {
        *tty = false;
    }
    planned
}

/// The json agent's side effects: `agent://event` to the webview (when
/// there is one). `awaiting_input` is NOT mirrored: the registry derives it
/// from the agent at read time.
fn json_hooks(app: Option<AppHandle>) -> JsonHooks {
    JsonHooks {
        emit: Arc::new(move |event: &AgentEvent| {
            if let Some(app) = &app {
                let _ = app.emit("agent://event", &event_json(event));
            }
        }),
    }
}

/// One json send, journaled only when it was WRITTEN (delivered, or
/// written-but-unacknowledged) — a refused send (`busy` / `exited` /
/// `unsupported`) never shows up as a `submit: true` delivery record, the
/// same rule as the pty path's `write_journaled`.
fn json_send_journaled(registry: &Registry, agent: &Arc<JsonAgent>, text: &str, wait: Duration) -> TurnReceipt {
    let receipt = agent.send_input(text, wait);
    if receipt.written() {
        registry.record_input(agent.id(), text.len() + 1, text.to_owned(), true);
    }
    receipt
}

/// Start the json agent's child. Persistent CLIs: spawn now, then the
/// prompt (if any) goes down stdin — without one the CLI's first ready
/// signal is `awaiting_input`. Per-turn CLIs: the prompt IS the first
/// child's message; without one the agent simply waits (`awaiting_input`).
fn json_boot(registry: &Registry, agent: &Arc<JsonAgent>, prompt: Option<&str>) -> Result<Option<PromptReceipt>, String> {
    if agent.mode().persistent {
        if prompt.is_none() {
            agent.expect_ready_as_awaiting();
        }
        agent.spawn_child(None)?;
    }
    match prompt {
        Some(prompt) => {
            let receipt = json_send_journaled(registry, agent, prompt, PROMPT_WAIT_CAP);
            if !agent.mode().persistent && receipt.reason.as_deref().is_some_and(|r| r.starts_with("exited:")) {
                return Err(receipt.reason.unwrap_or_default());
            }
            Ok(Some(prompt_receipt_of(&receipt)))
        }
        None => {
            if !agent.mode().persistent {
                agent.mark_awaiting();
            }
            Ok(None)
        }
    }
}

/// The spawn response's `prompt_receipt` shape for a json agent: delivered
/// = the CLI's next `turn_started` arrived; `seq_before`/`seq_after` are
/// event-ring cursors here, not byte counters.
pub fn prompt_receipt_of(receipt: &TurnReceipt) -> PromptReceipt {
    PromptReceipt {
        delivered: receipt.delivered,
        reason: receipt.reason.clone(),
        waited_ms: Some(receipt.waited_ms),
        seq_before: Some(receipt.seq_before),
        seq_after: receipt.turn_started_seq,
        had_output_within_ms: Some(receipt.delivered),
    }
}

/// `send_input` on a json agent (Tauri command, control route, MCP tool):
/// one user-message line (claude) or one continuation child (opencode),
/// journaled, receipt = the next `turn_started`. `Err` when `id` is not a
/// json agent — the caller falls back to the pty path.
pub fn json_send_input(registry: &Registry, id: TermId, text: &str, wait_ms: Option<u64>) -> Result<TurnReceipt, String> {
    let agent = registry
        .json_agent(id)
        .ok_or_else(|| format!("process {id} is not a json-transport agent"))?;
    let wait_ms = wait_ms.unwrap_or(TURN_RECEIPT_WAIT_DEFAULT_MS).min(TURN_RECEIPT_WAIT_MAX_MS);
    Ok(json_send_journaled(registry, &agent, text, Duration::from_millis(wait_ms)))
}

/// Events with `seq > since` for a json agent; `None` when `id` is not one.
pub fn agent_events(registry: &Registry, id: TermId, since: u64) -> Option<Vec<AgentEvent>> {
    registry.json_agent(id).map(|agent| agent.events_since(since))
}

/// `send_agent_input` from the webview (the transcript view's input box).
/// Async + `spawn_blocking`: the receipt waits on the CLI.
#[tauri::command]
pub async fn send_agent_input(
    registry: State<'_, Registry>,
    id: TermId,
    text: String,
    wait_ms: Option<u64>,
) -> Result<TurnReceipt, String> {
    let registry = registry.inner().clone();
    tauri::async_runtime::spawn_blocking(move || json_send_input(&registry, id, &text, wait_ms))
        .await
        .map_err(|e| e.to_string())?
}

/// The event ring since a cursor (the transcript view's catch-up after a
/// reveal, and the debug surface).
#[tauri::command]
pub fn get_agent_events(registry: State<'_, Registry>, id: TermId, since: Option<u64>) -> Result<Vec<AgentEvent>, String> {
    agent_events(&registry, id, since.unwrap_or(0)).ok_or_else(|| format!("process {id} is not a json-transport agent"))
}

/// The spawn uuid: the ONLY thing the close path matches the container-side
/// process by. No fallback on a failed RNG — a clock-derived id can collide
/// across two spawns in the same millisecond and the close would then kill
/// the WRONG worker; the spawn fails instead.
fn new_uuid() -> Result<String, String> {
    crate::control_http::random_hex(16).map_err(|e| format!("cannot generate a spawn uuid: {e}"))
}

/// The `agent://bridge` emitter for the real app.
pub fn app_bridge_emitter(app: AppHandle) -> Arc<dyn Fn(BridgeEvent) + Send + Sync> {
    Arc::new(move |event: BridgeEvent| {
        let json = JsonEvent {
            event: "agent://bridge",
            term_id: event.id,
            seq: 0,
            data: [
                ("id".to_owned(), json!(event.id)),
                ("bridge".to_owned(), json!(event.bridge.as_str())),
                ("container".to_owned(), json!(event.container)),
            ]
            .into_iter()
            .collect(),
        };
        let _ = app.emit(json.event, &json);
    })
}

/// The `agent://reap` emitter for the real app: ONE notification
/// per container a sweep reaped anything in, carrying the reaped count. The
/// frontend records it into the notification center.
pub fn app_reap_emitter(app: AppHandle) -> Arc<dyn Fn(&str, usize) + Send + Sync> {
    Arc::new(move |container: &str, count: usize| {
        let _ = app.emit("agent://reap", &json!({ "container": container, "count": count }));
    })
}

// ---- commands ----------------------------------------------------------------------------

/// Spawn an agent into this webview (frames channel attached), rail row
/// included. Async + `spawn_blocking`: a docker spawn makes up to three
/// CLI calls (`sh` probe, inspect, stats — 2 s timeout each) and a `prompt`
/// can wait on the ready gate; a sync command would run all of that on the
/// MAIN thread and freeze the window (the `pick_directory` lesson).
#[tauri::command]
pub async fn spawn_agent(
    app: AppHandle,
    registry: State<'_, Registry>,
    tools: State<'_, AgentToolsState>,
    settings: State<'_, SettingsState>,
    projects: State<'_, Projects>,
    req: SpawnAgentRequest,
    frames: Channel<InvokeResponseBody>,
) -> Result<SpawnAgentResponse, String> {
    let (registry, tools, settings, projects) = (
        registry.inner().clone(),
        tools.inner().clone(),
        settings.inner().clone(),
        projects.inner().clone(),
    );
    tauri::async_runtime::spawn_blocking(move || {
        spawn_agent_with(
            SpawnContext {
                registry: &registry,
                tools: &tools,
                settings: &settings,
                projects: &projects,
                // The rail's own "New agent ▸" spawn has NO actor —
                // user-spawned agents are roots (parent `None`).
                actor: None,
            },
            req,
            Arc::new(ChannelSink::new(frames, app.clone())),
            Some(app),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The control-surface spawn (no webview channel — agents read output back
/// through `/output`, exactly like `spawn_terminal`).
pub fn spawn_agent_headless(ctx: SpawnContext<'_>, req: SpawnAgentRequest) -> Result<SpawnAgentResponse, String> {
    spawn_agent_with(ctx, req, Arc::new(NullSink), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_tools::parse_command_line;

    fn docker_tool() -> AgentTool {
        parse_command_line("docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast").tool
    }

    /// A row spawned long ago (outside the busy window), so only its output
    /// decides busy-ness — the pre-review shape of every guard case.
    fn row(id: u32, tool_id: u32, container: Option<&str>, alive: bool, last: Option<u64>) -> AgentRow {
        AgentRow {
            id,
            name: format!("agent-{id}"),
            tool_id,
            container: container.map(str::to_owned),
            child_alive: alive,
            last_output_at: last,
            spawned_at_ms: 0,
            spawn_uuid: format!("u-{id}"),
        }
    }

    /// The kebab names are the serde names — `as_str` must never drift from
    /// the wire (both enums, every variant).
    #[test]
    fn kebab_names_match_serde() {
        for b in Bridge::all() {
            assert_eq!(serde_json::to_value(b).unwrap(), json!(b.as_str()));
        }
        for v in CloseVerification::all() {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(v.as_str()));
        }
    }

    #[test]
    fn env_precedence_inherited_lt_tool_lt_identity() {
        let inherited = vec![
            ("PATH".to_owned(), "/bin".to_owned()),
            ("CHAPPA_AI_PROCESS_ID".to_owned(), "stale".to_owned()),
            ("FOO".to_owned(), "inherited".to_owned()),
        ];
        let mut tool_env = IndexMap::new();
        tool_env.insert("FOO".to_owned(), "tool".to_owned());
        tool_env.insert("CHAPPA_AI_AGENT_TOOL_ID".to_owned(), "forged".to_owned());
        tool_env.insert("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_owned(), "1".to_owned());
        let identity = identity_env(7, Some(6), 3, "u-u-i-d");
        let merged = merge_env(&inherited, &tool_env, &identity);
        assert_eq!(merged["PATH"], "/bin", "inherited survives when unset above");
        assert_eq!(merged["FOO"], "tool", "tool.env beats inherited");
        assert_eq!(merged["CHAPPA_AI_PROCESS_ID"], "7", "identity beats inherited");
        assert_eq!(merged["CHAPPA_AI_AGENT_TOOL_ID"], "3", "identity beats tool.env");
        assert_eq!(merged["CHAPPA_AI_PROJECT_ID"], "6");
        assert_eq!(merged[SPAWN_UUID_ENV], "u-u-i-d");
        assert_eq!(merged["CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"], "1");
        // Without a project the project var is absent, not empty.
        let no_project: IndexMap<_, _> = identity_env(1, None, 2, "x").into_iter().collect();
        assert!(!no_project.contains_key("CHAPPA_AI_PROJECT_ID"));
    }

    /// An `extra_args` element with spaces is ONE argv entry, and for
    /// docker every env entry becomes a `-e` flag the container-side process
    /// sees. The wrapper is `sh -c '<script>' program args…`.
    #[test]
    fn spawn_plan_keeps_extra_args_verbatim_and_flags_env_for_docker() {
        let tool = docker_tool();
        let env = merge_env(&[], &tool.env, &identity_env(9, None, tool.id, "abc"));
        let plan = build_spawn_plan(&tool, &["--prompt".into(), "two words here".into()], &env, None, "abc", true);
        assert!(plan.args.contains(&"two words here".to_owned()), "{:?}", plan.args);
        let idx = plan.args.iter().position(|a| a == "--prompt").unwrap();
        assert_eq!(plan.args[idx + 1], "two words here");
        assert_eq!(&plan.args[..2], &["exec".to_owned(), "-u".to_owned()]);
        assert!(plan.args.windows(2).any(|w| w[0] == "-e" && w[1] == "CHAPPA_AI_PROCESS_ID=9"));
        assert!(plan.args.windows(2).any(|w| w[0] == "-e" && w[1] == format!("{SPAWN_UUID_ENV}=abc")));
        let c = plan.args.iter().position(|a| a == "dev-worker").unwrap();
        assert_eq!(&plan.args[c + 1..c + 4], &["sh".to_owned(), "-c".to_owned(), wrapper_script("abc")]);
        assert_eq!(plan.args[c + 4], "opencode");
        assert_eq!(plan.nspid, NsPid::PidFile);
        assert_eq!(
            wrapper_script("abc"),
            "mkdir -p /tmp/.chappa-ai && echo $$ > /tmp/.chappa-ai/abc.pid && exec \"$0\" \"$@\""
        );
        // No sh → no wrapper, program follows the container token directly.
        let plan = build_spawn_plan(&tool, &[], &env, None, "abc", false);
        assert_eq!(plan.args[c + 1], "opencode");
        assert_eq!(plan.nspid, NsPid::Unresolved);
        // Host: program + args, cwd = project root, no docker words.
        let mut host = AgentTool::template(ToolType::Claude);
        host.args = vec!["--verbose".into()];
        let plan = build_spawn_plan(&host, &["a b".into()], &env, Some(PathBuf::from("/p")), "u", false);
        assert_eq!(plan.command, "claude");
        assert_eq!(plan.args, vec!["--verbose", "a b"]);
        assert_eq!(plan.cwd, Some(PathBuf::from("/p")));
    }

    fn gate(first_output_ms: Option<u64>) -> ReadyGate {
        ReadyGate {
            quiet_ms: 750,
            max_wait_ms: 5_000,
            first_output_ms,
        }
    }

    #[test]
    fn ready_gate_with_a_fake_clock() {
        // No output → never ready, however long we wait.
        let booting = IoView { has_output: false, last_output_ms: None, child_alive: true };
        assert_eq!(readiness(booting, 0, gate(None)), Readiness::Pending);
        assert_eq!(readiness(booting, 1_000_000, gate(None)), Readiness::Pending);
        // Output, then 750 ms quiet → ready (not a ms sooner).
        let spoke = IoView { has_output: true, last_output_ms: Some(10_000), child_alive: true };
        assert_eq!(readiness(spoke, 10_749, gate(Some(10_000))), Readiness::Pending);
        assert_eq!(readiness(spoke, 10_750, gate(Some(10_000))), Readiness::Ready);
        // Exit before ready → the receipt reason is "exited".
        let dead = IoView { has_output: true, last_output_ms: Some(10_000), child_alive: false };
        assert_eq!(readiness(dead, 10_100, gate(Some(10_000))), Readiness::Exited);
        assert_eq!(readiness(dead, 20_000, gate(Some(10_000))), Readiness::Exited, "a dead child is never 'ready'");
    }

    /// Review fix: a TUI that repaints every few ms never yields 750 ms of
    /// silence — after `max_wait_ms` of continuous output since the FIRST
    /// byte the gate opens anyway (`ready-by-timeout`), and a ms sooner it
    /// does not.
    #[test]
    fn ready_gate_falls_back_to_the_max_wait_for_a_chatty_tui() {
        let first = 10_000;
        // Output that never stops: last byte is always "just now".
        let chatty = |now: u64| IoView { has_output: true, last_output_ms: Some(now - 5), child_alive: true };
        assert_eq!(readiness(chatty(14_999), 14_999, gate(Some(first))), Readiness::Pending);
        assert_eq!(readiness(chatty(15_000), 15_000, gate(Some(first))), Readiness::ReadyByTimeout);
        assert_eq!(readiness(chatty(60_000), 60_000, gate(Some(first))), Readiness::ReadyByTimeout);
        // The fallback counts from the first byte, never from the spawn: no
        // first byte, no timeout (a silent boot stays pending until the cap).
        assert_eq!(
            readiness(IoView { has_output: false, last_output_ms: None, child_alive: true }, 60_000, gate(None)),
            Readiness::Pending
        );
        // A quiet window still wins when it happens first.
        let quiet = IoView { has_output: true, last_output_ms: Some(11_000), child_alive: true };
        assert_eq!(readiness(quiet, 11_750, gate(Some(first))), Readiness::Ready);
        // A dead child beats the timeout too.
        assert_eq!(
            readiness(IoView { child_alive: false, ..chatty(20_000) }, 20_000, gate(Some(first))),
            Readiness::Exited
        );
    }

    #[test]
    fn busy_guard_refuses_allows_forces_and_names_processes() {
        let now = 1_000_000;
        let mut tool = docker_tool();
        tool.id = 2;
        tool.name = "worker".into();
        tool.max_busy = Some(1);
        if let Runtime::DockerExec { max_busy_in_container, .. } = &mut tool.runtime {
            *max_busy_in_container = Some(2);
        }
        // Under every limit: allowed.
        assert_eq!(busy_guard(&tool, &[], now, false), Ok(None));
        // One busy agent of this tool (output 30 s ago) → per-tool refusal naming it.
        let rows = vec![row(4, 2, Some("dev-worker"), true, Some(now - 30_000))];
        let err = busy_guard(&tool, &rows, now, false).unwrap_err();
        assert!(err.contains("agent-4 (process 4, last_output_at 970000)"), "{err}");
        assert!(err.contains("max_busy = 1"), "{err}");
        // Idle (121 s quiet) or dead agents do not count.
        let idle = vec![
            row(4, 2, Some("dev-worker"), true, Some(now - 121_000)),
            row(5, 2, Some("dev-worker"), false, Some(now)),
        ];
        assert_eq!(busy_guard(&tool, &idle, now, false), Ok(None));
        // Container limit is shared ACROSS tools naming it.
        let rows = vec![
            row(6, 9, Some("dev-worker"), true, Some(now)),
            row(7, 8, Some("dev-worker"), true, Some(now - 1)),
        ];
        let err = busy_guard(&tool, &rows, now, false).unwrap_err();
        assert!(err.contains("max_busy_in_container = 2"), "{err}");
        assert!(err.contains("agent-6") && err.contains("agent-7"), "{err}");
        // force overrides, and says so.
        let note = busy_guard(&tool, &rows, now, true).unwrap().expect("forced note");
        assert!(note.starts_with("forced past busy guard"), "{note}");
    }

    /// Review fix: two spawns a second apart with `max_busy_in_container = 1`
    /// — the first has not printed a byte yet, and it must STILL count as
    /// busy (it was spawned inside the window), so the second is refused
    /// without `force`. Once the window has passed with no output it is idle.
    #[test]
    fn a_just_spawned_silent_agent_is_busy() {
        let now = 1_000_000;
        let mut tool = docker_tool();
        tool.id = 2;
        if let Runtime::DockerExec { max_busy_in_container, .. } = &mut tool.runtime {
            *max_busy_in_container = Some(1);
        }
        let silent = |spawned_at_ms: u64| AgentRow {
            spawned_at_ms,
            ..row(4, 2, Some("dev-worker"), true, None)
        };
        assert!(is_busy(&silent(now - 1_000), now));
        let err = busy_guard(&tool, &[silent(now - 1_000)], now, false).unwrap_err();
        assert!(err.contains("max_busy_in_container = 1"), "{err}");
        assert!(err.contains("last_output_at never"), "{err}");
        assert!(busy_guard(&tool, &[silent(now - 1_000)], now, true).unwrap().is_some(), "force still works");
        // Spawned 121 s ago and never spoke: idle.
        assert!(!is_busy(&silent(now - 121_000), now));
        assert_eq!(busy_guard(&tool, &[silent(now - 121_000)], now, false), Ok(None));
        // Dead children never count, however fresh.
        assert!(!is_busy(&AgentRow { child_alive: false, ..silent(now) }, now));
    }

    #[test]
    fn container_probe_transitions() {
        let base = BridgeInputs {
            container_status: ContainerStatus::Running,
            container_started_ms: Some(1_000),
            spawned_at_ms: 5_000,
            last_output_ms: Some(9_000),
            now_ms: 10_000,
            stale_after_ms: 900_000,
            poke_produced_output: None,
        };
        assert_eq!(derive_bridge(base), Bridge::Ok);
        // running → exited flips.
        assert_eq!(derive_bridge(BridgeInputs { container_status: ContainerStatus::Exited, ..base }), Bridge::ContainerDown);
        assert_eq!(derive_bridge(BridgeInputs { container_status: ContainerStatus::Missing, ..base }), Bridge::ContainerDown);
        // started_at newer than the spawn = restarted under us.
        assert_eq!(derive_bridge(BridgeInputs { container_started_ms: Some(6_000), ..base }), Bridge::ContainerDown);
        // unknown never flips (docker slow ≠ container gone).
        assert_eq!(derive_bridge(BridgeInputs { container_status: ContainerStatus::Unknown, container_started_ms: None, ..base }), Bridge::Ok);
    }

    #[test]
    fn stale_needs_both_quiet_and_a_failed_poke() {
        let quiet = BridgeInputs {
            container_status: ContainerStatus::Running,
            container_started_ms: Some(1_000),
            spawned_at_ms: 5_000,
            last_output_ms: Some(10_000),
            now_ms: 10_000 + 900_000,
            stale_after_ms: 900_000,
            poke_produced_output: Some(false),
        };
        assert_eq!(derive_bridge(quiet), Bridge::Stale);
        // Quiet but the poke produced bytes → ok.
        assert_eq!(derive_bridge(BridgeInputs { poke_produced_output: Some(true), ..quiet }), Bridge::Ok);
        // Poke failed but NOT quiet long enough → ok.
        assert_eq!(derive_bridge(BridgeInputs { now_ms: 10_000 + 899_999, ..quiet }), Bridge::Ok);
        // Never spoke = booting, not stale.
        assert_eq!(derive_bridge(BridgeInputs { last_output_ms: None, ..quiet }), Bridge::Ok);
        assert!(!stale_candidate(None, 1_000_000, 10));
        assert!(stale_candidate(Some(0), 900_000, 900_000));
        // Container down wins over stale.
        assert_eq!(derive_bridge(BridgeInputs { container_status: ContainerStatus::Exited, ..quiet }), Bridge::ContainerDown);
    }

    #[test]
    fn inspect_stats_and_time_parsers() {
        let s = parse_inspect("running 2026-08-28T10:00:00.123456789Z\n", parse_rfc3339_ms("2026-08-28T10:00:10Z").unwrap());
        assert_eq!(s.status, ContainerStatus::Running);
        assert_eq!(s.uptime_s, Some(9));
        assert_eq!(s.started_at.as_deref(), Some("2026-08-28T10:00:00.123456789Z"));
        assert_eq!(parse_inspect("exited 2026-08-28T10:00:00Z", 0).status, ContainerStatus::Exited);
        assert_eq!(parse_inspect("", 0).status, ContainerStatus::Unknown);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01Z"), Some(1_000));
        assert_eq!(parse_rfc3339_ms("2026-08-28T10:00:00+02:00"), Some(parse_rfc3339_ms("2026-08-28T08:00:00Z").unwrap()));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
        assert_eq!(
            parse_stats_mem(r#"{"MemUsage":"1.5GiB / 3GiB","Name":"dev-worker"}"#),
            Some((1_610_612_736, 3_221_225_472))
        );
        assert_eq!(parse_size("512MiB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("3GB"), Some(3_000_000_000));
        assert_eq!(parse_size("nope"), None);
        assert_eq!(parse_stats_mem("not json"), None);
    }

    fn meta(nspid: NsPid) -> AgentMeta {
        AgentMeta {
            tool_id: 1,
            tool_type: ToolType::Opencode,
            model: None,
            runtime: docker_tool().runtime,
            parent_process_id: None,
            project_id: None,
            spawned_at_ms: 0,
            spawn_uuid: "u1".into(),
            nspid,
            container: None,
            bridge: Bridge::Ok,
            transport: Transport::Tty,
            awaiting_input: false,
        }
    }

    const NO_KILL_GRACE: Duration = Duration::ZERO;

    /// The docker call "kind" a test orders on: an `exec … sh -c "kill …"` is
    /// the signal step (the shell builtin — see `signal_args`), any other
    /// exec is its program, everything else is the docker verb.
    fn call_kind(c: &[String]) -> &str {
        if c[0] != "exec" {
            &c[0]
        } else if c[2] == "sh" && c.get(4).is_some_and(|s| s.starts_with("kill ")) {
            "kill"
        } else {
            &c[2]
        }
    }

    /// The container-side half of against the fake CLI: TERM → poll →
    /// KILL → verify, never `docker top`.
    #[test]
    fn close_sequence_term_poll_kill_verify() {
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "dev-worker", "cat", "/tmp/.chappa-ai/u1.pid"], "4242\n");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\""], "");
        docker.ok(&["exec", "dev-worker", "test", "-d", "/proc/4242"], ""); // still there
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -KILL \"$1\""], "");
        let verdict = kill_container_side(docker.as_ref(), &meta(NsPid::PidFile), Duration::from_millis(300), NO_KILL_GRACE);
        assert_eq!(verdict, CloseVerification::StillPresent, "ignores KILL: still-present");
        let kinds: Vec<String> = docker
            .calls()
            .iter()
            .map(|c| call_kind(c).to_owned())
            .collect();
        let term = kinds.iter().position(|k| k == "kill").unwrap();
        assert_eq!(kinds[0], "inspect");
        assert_eq!(kinds[1], "cat");
        // The signal goes through the shell builtin (`sh -c 'kill -SIG "$1"'
        // kill <pid>`), never a bare `kill` executable — worker images lack one.
        assert_eq!(docker.calls()[term][2..5], ["sh", "-c", "kill -TERM \"$1\""].map(String::from));
        assert_eq!(docker.calls()[term][6], "4242");
        let last_kill = docker.calls().iter().rposition(|c| call_kind(c) == "kill").unwrap();
        assert_eq!(docker.calls()[last_kill][4], "kill -KILL \"$1\"");
        assert!(docker.calls_with(&["top"]).is_empty(), "docker top must never be consulted");
        assert!(docker.calls_with(&["exec", "dev-worker", "test"]).len() >= 2, "polled then re-verified");

        // The process obeys TERM: gone, no KILL.
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "dev-worker", "cat"], "4242");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\""], "");
        docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/4242"], "");
        assert_eq!(kill_container_side(docker.as_ref(), &meta(NsPid::PidFile), TERM_GRACE, KILL_GRACE), CloseVerification::Gone);
        assert!(docker.calls_with(&["exec", "dev-worker", "sh", "-c", "kill -KILL \"$1\""]).is_empty());

        // Container already down → short-circuit: no exec at all.
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "exited 2026-01-01T00:00:00Z");
        assert_eq!(kill_container_side(docker.as_ref(), &meta(NsPid::PidFile), TERM_GRACE, KILL_GRACE), CloseVerification::ContainerDown);
        assert!(docker.calls_with(&["exec"]).is_empty());

        // No pid file (no sh at spawn) → the environ match, exact on the uuid,
        // run as the TOOL's user (the owning user reads its own
        // environ; the container root gets Permission denied without
        // CAP_SYS_PTRACE).
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "-u", "dev", "dev-worker", "sh", "-c"], "77\n78\n");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\""], "");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -KILL \"$1\""], "");
        docker.fail(&["exec", "dev-worker", "test", "-d", "/proc/77"], "");
        assert_eq!(kill_container_side(docker.as_ref(), &meta(NsPid::Unresolved), TERM_GRACE, KILL_GRACE), CloseVerification::Gone);
        let script = &docker.calls_with(&["exec", "-u", "dev", "dev-worker", "sh"])[0][6];
        assert!(script.contains("CHAPPA_AI_SPAWN_UUID=u1"), "{script}");
        assert!(docker.calls_with(&["exec", "dev-worker", "cat"]).is_empty());
        // …and when even that finds nothing: unresolved.
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "-u", "dev", "dev-worker", "sh", "-c"], "");
        assert_eq!(kill_container_side(docker.as_ref(), &meta(NsPid::Unresolved), TERM_GRACE, KILL_GRACE), CloseVerification::Unresolved);
    }

    /// Review fix: a SIGKILLed process takes a beat to be reaped. The verdict
    /// polls `/proc/<pid>` for `kill_grace` after the KILL instead of reading
    /// it once — a process that goes within the grace is `gone`, one that
    /// outlives it is `still-present`, and no `rm` of the pid file happens
    /// for an `unresolved` verdict.
    #[test]
    fn kill_is_followed_by_a_grace_poll() {
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "dev-worker", "cat"], "4242");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\""], "");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -KILL \"$1\""], "");
        // Present until the KILL lands, then present for two more polls,
        // then gone.
        let after_kill = Arc::new(Mutex::new(false));
        let polls_after_kill = Arc::new(Mutex::new(0usize));
        // The rule list is consulted AFTER on_call, so the fake can flip the
        // answer for `test -d` once enough post-KILL polls have happened.
        let docker_for_rule = docker.clone();
        let polls = polls_after_kill.clone();
        docker.set_on_call({
            let after_kill = after_kill.clone();
            move |args| {
                if args.get(4).map(String::as_str) == Some("kill -KILL \"$1\"") {
                    *after_kill.lock().unwrap() = true;
                } else if args.get(2).map(String::as_str) == Some("test") && *after_kill.lock().unwrap() {
                    let mut n = polls.lock().unwrap();
                    *n += 1;
                    if *n == 3 {
                        docker_for_rule.fail(&["exec", "dev-worker", "test", "-d", "/proc/4242"], "");
                    }
                }
            }
        });
        docker.ok(&["exec", "dev-worker", "test", "-d", "/proc/4242"], "");
        let verdict = kill_container_side(docker.as_ref(), &meta(NsPid::PidFile), Duration::from_millis(100), KILL_GRACE);
        assert_eq!(verdict, CloseVerification::Gone, "went within the KILL grace");
        assert!(*polls_after_kill.lock().unwrap() >= 3, "polled after the KILL, not read once");

        // Never goes: still-present after the grace, and the grace is honoured
        // (bounded, but not zero polls).
        let docker = FakeDocker::new();
        docker.ok(&["inspect"], "running 2026-01-01T00:00:00Z");
        docker.ok(&["exec", "dev-worker", "cat"], "4242");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -TERM \"$1\""], "");
        docker.ok(&["exec", "dev-worker", "sh", "-c", "kill -KILL \"$1\""], "");
        docker.ok(&["exec", "dev-worker", "test", "-d", "/proc/4242"], "");
        let started = Instant::now();
        let verdict = kill_container_side(docker.as_ref(), &meta(NsPid::PidFile), Duration::ZERO, Duration::from_millis(600));
        assert_eq!(verdict, CloseVerification::StillPresent);
        assert!(started.elapsed() >= Duration::from_millis(600), "waited the KILL grace");
        let kill_at = docker.calls().iter().position(|c| c.get(4).map(String::as_str) == Some("kill -KILL \"$1\"")).unwrap();
        let polls_after = docker.calls()[kill_at + 1..].iter().filter(|c| c.get(2).map(String::as_str) == Some("test")).count();
        assert!(polls_after >= 2, "{polls_after} post-KILL polls");

        // remove_pid_file: skipped for container-down AND unresolved, runs for
        // gone / still-present.
        for (verdict, expect_rm) in [
            (CloseVerification::ContainerDown, false),
            (CloseVerification::Unresolved, false),
            (CloseVerification::Gone, true),
            (CloseVerification::StillPresent, true),
        ] {
            let docker = FakeDocker::new();
            docker.ok(&["exec"], "");
            remove_pid_file(docker.as_ref(), &meta(NsPid::PidFile), verdict);
            assert_eq!(!docker.calls_with(&["exec", "dev-worker", "rm"]).is_empty(), expect_rm, "{verdict:?}");
        }
        let docker = FakeDocker::new();
        remove_pid_file(docker.as_ref(), &meta(NsPid::Unresolved), CloseVerification::Gone);
        assert!(docker.calls().is_empty(), "no pid file to remove without the wrapper");
    }

    #[test]
    fn agent_instructions_name_the_mcp_tools() {
        let s = agent_instructions(12, Some((6, "dev")));
        assert!(s.contains("process 12"));
        assert!(s.contains("\"dev\" (project id 6)"));
        assert!(s.contains("chappa-ai MCP tools: list_processes"));
        assert!(s.contains("spawn_agent"));
        assert!(!agent_instructions(1, None).contains("in project"));
    }

    // ---- parent resolution (pure) -----------------------------------

    /// The candidate parent is resolved in order explicit > actor > None.
    #[test]
    fn parent_resolution_order_is_explicit_then_actor_then_none() {
        let exists = |p| p == 7 || p == 9;
        // Explicit wins even when the actor differs.
        assert_eq!(
            resolve_parent_process_id(Some(7), Some(9), 1, exists),
            Some(7),
            "explicit beats actor"
        );
        // No explicit → the actor.
        assert_eq!(resolve_parent_process_id(None, Some(9), 1, exists), Some(9));
        // Neither → None (a user-spawned root).
        assert_eq!(resolve_parent_process_id(None, None, 1, exists), None);
    }

    /// A stale, bogus or `user` actor records None silently — never a dangling
    /// id, and never the agent's own reserved id (self-parenting is guarded).
    #[test]
    fn stale_user_or_self_actor_records_none() {
        let exists = |p| p == 9 || p == 41;
        // Stale: names no live entry.
        assert_eq!(resolve_parent_process_id(None, Some(99), 1, exists), None);
        // `user`/unparsable actor already arrived as None upstream.
        assert_eq!(resolve_parent_process_id(None, None, 1, exists), None);
        // Self-parenting (actor == the child's own reserved id): guarded, even
        // if the id somehow already had an entry.
        assert_eq!(resolve_parent_process_id(None, Some(41), 41, exists), None);
        assert_eq!(resolve_parent_process_id(Some(41), None, 41, exists), None);
        // A legitimate live parent of ANY kind is accepted (a plain shell).
        assert_eq!(resolve_parent_process_id(None, Some(9), 41, exists), Some(9));
    }

    // ---- the orphan classifier (pure) -------------------------------

    #[test]
    fn orphans_skips_live_uuids_and_busy_window_fresh_processes() {
        let live = HashSet::from(["u-live".to_owned()]);
        let now = 1_000_000;
        // u-live: never an orphan. u-fresh: unknown but spawned inside the
        // busy window — skipped (its entry may still be materializing).
        let rows = vec![
            ScanRow { pid: 100, uuid: "u-live".into(), spawned_at_ms: now - 1 },
            ScanRow { pid: 200, uuid: "u-fresh".into(), spawned_at_ms: now },
        ];
        assert_eq!(orphans(&rows, &live, BUSY_WINDOW_MS, now), Vec::new());
        // Once the fresh one ages out of the window, it IS an orphan.
        let rows = vec![
            ScanRow { pid: 200, uuid: "u-fresh".into(), spawned_at_ms: now - BUSY_WINDOW_MS - 1 },
        ];
        assert_eq!(
            orphans(&rows, &live, BUSY_WINDOW_MS, now),
            vec![Orphan { pid: 200, uuid: "u-fresh".into() }]
        );
    }

    /// The SMALLEST pid per uuid is the root (the wrapper `exec`s into the
    /// program, so children of the same uuid are larger pids and are not
    /// signalled — they die with the root or get a later sweep).
    #[test]
    fn orphans_roots_the_smallest_pid_per_uuid() {
        let now = 1_000_000;
        let rows = vec![
            ScanRow { pid: 6927, uuid: "u1".into(), spawned_at_ms: 0 },
            ScanRow { pid: 532, uuid: "u1".into(), spawned_at_ms: 0 },
            ScanRow { pid: 8875, uuid: "u2".into(), spawned_at_ms: 0 },
        ];
        let got = orphans(&rows, &HashSet::new(), BUSY_WINDOW_MS, now);
        assert_eq!(
            got,
            vec![
                Orphan { pid: 532, uuid: "u1".into() }, // BTreeMap orders by uuid
                Orphan { pid: 8875, uuid: "u2".into() },
            ]
        );
        assert!(!got.iter().any(|o| o.pid == 6927), "the larger sibling is not the root");
    }

    #[test]
    fn parse_scan_reads_pid_uuid_and_spawn_time() {
        let rows = parse_scan("532 u1 0\n6927 u1 5\n8875 u2 999\n\njunk line\n");
        assert_eq!(
            rows,
            vec![
                ScanRow { pid: 532, uuid: "u1".into(), spawned_at_ms: 0 },
                ScanRow { pid: 6927, uuid: "u1".into(), spawned_at_ms: 5 },
                ScanRow { pid: 8875, uuid: "u2".into(), spawned_at_ms: 999 },
            ]
        );
        // A missing spawn-time column collapses to a fresh-ish 0 (still
        // outside the busy window when the process is old in reality).
        assert_eq!(parse_scan("1 u1\n"), vec![ScanRow { pid: 1, uuid: "u1".into(), spawned_at_ms: 0 }]);
    }
}
