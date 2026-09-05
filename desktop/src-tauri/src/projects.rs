//! Project/process lifecycle runner — the thin Tauri glue over `project-model`
//!. All pure logic (the `chappa.yml` parse/containment, trust hash,
//! status mapping incl. the 500ms-alive rule, restart backoff, the
//! restart_when_changed watcher, projects.json CRUD) lives and is tested in
//! `project-model`; this module only wires it to the terminal registry,
//! the frontend commands, and the lifecycle semantics that need a live child.
//!
//! ## Execution profile (DECIDED 2026-08-05)
//! A project-file `command` line runs via `cmd /C <command>` on Windows and
//! `sh -c <command>` elsewhere. This is deliberately a DIFFERENT path from
//! interactive terminals, which use `default_shell()` in commands.rs
//! (pwsh/powershell PATH probe / `$SHELL`). Rationale: the documented
//! execution profile runs yml command lines through cmd.exe, matching what
//! other project runners do. `py app/summarize.py` is a plain command line
//! for `cmd /C`, not an interactive pwsh session.
//!
//! ## Frontend-driven spawns (schema-forced)
//! `open_project` loads the yml, builds per-process runtime state (status
//! `stopped`, nothing spawned) and reports the trust gate; the actual spawns
//! are initiated by the frontend via `start_project_process`, which supplies
//! the per-terminal frames Channel the registry needs (a webview-only
//! artifact — the Rust side cannot fabricate one). After a Run on the trust
//! gate the frontend starts every `auto_start` process. Auto_restart respawns
//! REUSE the same registry TermId and frames Channel, so the frontend panel
//! survives restarts without re-subscribing. The S/A/P project chords are
//! frontend loops over `start_project_process` / `stop_project_process`
//! (each iteration supplies its own Channel), not Rust bulk commands — the
//! `Vec<Channel>` shape was too fragile.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use project_model::backoff::RestartBackoff;
use project_model::hash;
use project_model::projects::{ProcessOverride, ProjectStore};
use project_model::writeback;
use project_model::settings::{split_command_line, Settings, EXEC_PROFILE_CMD, EXEC_PROFILE_SH};
use project_model::status::{Lifecycle, ProcessEvent, RUNNING_AFTER_MS};
use project_model::watcher::{ChangeMatcher, RecursiveMode, RestartWatcher};
use project_model::yml::{ProcessDef, ProjectYml};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};
use term_core::actor::epoch_ms;
use term_core::pty::PtySpec;
use term_core::status::ProcessStatus;

use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, ChannelSink, CreatedEvent, ExitHandler,
    Registry, TermId,
};
use crate::settings::SettingsState;

/// One open project: the parsed yml (fresh at open), the trust hash, and the
/// per-process runtime state. Display name/icon are not duplicated here — the
/// DTOs read them from the yml/store at open time.
struct ProjectRuntime {
    root: PathBuf,
    yml_hash: String,
    processes: IndexMap<String, ProcessRuntime>,
    /// The reason the LAST reload of chappa.yml failed, or `None` while
    /// the file parses. While set, every mutation command refuses (the
    /// write-back also re-checks by loading fresh) and the UI shows it.
    load_error: Option<String>,
    /// The project-file watcher (non-recursive, debounced). Its consumer
    /// thread reloads the project; dropping this ends the thread.
    _yml_watcher: Option<RestartWatcher>,
}

/// One process of an open project. Status lives in `lifecycle` (project-model,
/// including the 500ms-alive tick); the registry entry is reached through
/// `term_id`, which is STABLE across auto-restarts so the frontend panel and
/// frames channel stay bound to one id.
struct ProcessRuntime {
    def: ProcessDef,
    lifecycle: Lifecycle,
    term_id: Option<TermId>,
    exit_code: Option<i32>,
    backoff: RestartBackoff,
    /// The frontend's frames channel, retained so auto_restart can respawn
    /// into the SAME terminal id/channel without the webview re-subscribing.
    channel: Option<Channel<InvokeResponseBody>>,
    /// The restart_when_changed watcher, kept alive for the process's
    /// lifetime. Dropping it (project close/remove) stops notify, which
    /// disconnects the debounce channel and ends the consumer thread.
    watcher: Option<RestartWatcher>,
    /// The debounced restart-signal stream (`None` when the yml lists no
    /// patterns or every glob was invalid).
    watcher_rx: Option<Receiver<()>>,
    /// True once the watcher-signal consumer thread is running. Started at
    /// the FIRST spawn; respawns never spawn a second consumer.
    watcher_started: bool,
    spawned_at: Instant,
    /// Incremented on every spawn/stop/restart; a scheduled auto-restart or a
    /// debounced watcher restart carries the generation it was scheduled
    /// under and bails if a user action superseded it while it waited.
    generation: u32,
}

#[derive(Clone, Default)]
pub struct Projects {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    store: Option<ProjectStore>,
    runtimes: HashMap<u32, ProjectRuntime>,
    /// Signalled whenever ANY process's `term_id`, lifecycle status or
    /// `generation` changes, so `control_start` can park on it instead of
    /// re-locking every 50ms. Paired with the mutex that wraps this `Inner`,
    /// so a waiter checking its predicate under the lock cannot miss a
    /// signal. Held behind an `Arc` because most write paths only ever see a
    /// `&mut Inner` (the guard), never the `Projects` handle — those call
    /// [`Inner::notify_changed`] directly.
    ///
    /// EVERY mutation of those three fields must notify. A missed signal does
    /// not corrupt anything (the wait is still deadline-bounded), but it
    /// degrades a start into a spurious `pending: true`.
    changed: Arc<Condvar>,
    app: Option<AppHandle>,
    /// The app settings, so spawns can resolve `default_exec_profile`
    /// and seed the per-actor controls. A clone of the managed state (its own
    /// mutex), locked only for a `snapshot()` and never in the reverse order
    /// — `set_settings` takes settings → registry, never projects.
    settings: SettingsState,
    shutting_down: bool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            store: None,
            runtimes: HashMap::new(),
            changed: Arc::new(Condvar::new()),
            app: None,
            settings: SettingsState::default(),
            shutting_down: false,
        }
    }
}

impl Inner {
    /// Wake anything parked on [`Inner::changed`] (today: `control_start`).
    /// Callable from a write path that only holds the guard; safe to call
    /// while still holding the lock — the waiter re-checks its predicate
    /// under the same mutex, so an extra wake is a no-op and a missing one is
    /// the only real bug.
    fn notify_changed(&self) {
        self.changed.notify_all();
    }
}

/// Why a control-surface project call failed. The HTTP layer maps VARIANTS to
/// status codes (`control_http::project_response`); it used to sniff
/// substrings of the message ("not open", "no process"), so a reworded error
/// silently changed the status code. `Display` reproduces those exact
/// messages, and `From<ControlError> for String` keeps the Tauri commands —
/// which answer `Result<_, String>` — working with plain `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// No project with that id, or no process by that name inside it.
    NotFound(String),
    /// The project exists but is not OPEN in the app, so it has no runtime to
    /// act on (chappa-ai can only drive processes of an open project).
    NotOpen(String),
    /// Everything else: store not initialized, poisoned state, spawn failure.
    Internal(String),
}

impl ControlError {
    fn no_project(id: u32) -> Self {
        ControlError::NotFound(format!("no such project: {id}"))
    }

    fn no_process(name: &str, id: u32) -> Self {
        ControlError::NotFound(format!("no process `{name}` in project {id}"))
    }

    fn not_open(id: u32) -> Self {
        ControlError::NotOpen(format!("project {id} is not open"))
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NotFound(m) | ControlError::NotOpen(m) | ControlError::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl From<ControlError> for String {
    fn from(err: ControlError) -> String {
        err.to_string()
    }
}

/// Errors from the layers below (spec building, the registry spawn) are
/// internal by definition — the caller asked for something that exists.
impl From<String> for ControlError {
    fn from(message: String) -> Self {
        ControlError::Internal(message)
    }
}

impl Projects {
    /// Called once from setup: point the store at the app-config dir, keep the
    /// handle for events, and start the 500ms-alive ticker.
    pub fn init(&self, app: &AppHandle, config_dir: PathBuf, settings: SettingsState) {
        // First-launch migration: every stored project materializes
        // `chappa.yml` from its store definitions, so the new project file
        // exists and nothing is lost — no user action and no behavior change
        // (same commands, same statuses). A project that had sync ON keeps its
        // chappa.yml untouched and un-watched; a project with no definitions
        // writes no file. A lasting chappa.yml that fails to parse logs here;
        // it surfaces in the UI on open, exactly as the load error.
        let mut store = ProjectStore::new(config_dir.join("projects.json"));
        match project_model::native::migrate(&mut store) {
            Ok(errors) => {
                for (id, e) in errors {
                    log::warn!("migration: project {id} chappa.yml failed to load: {e}");
                }
            }
            Err(e) => log::warn!("migration failed: {e}"),
        }
        let inner = self.inner.clone();
        {
            let mut guard = inner.lock().unwrap();
            guard.store = Some(store);
            guard.app = Some(app.clone());
            guard.settings = settings;
        }
        spawn_ticker(inner);
    }

    /// App-exit: stop every project process, cancel all pending restarts, and
    /// make the registry's `shutdown_all` authoritative (no respawn race).
    pub fn shutdown(&self, registry: &Registry) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutting_down = true;
        for (_, runtime) in inner.runtimes.iter_mut() {
            for proc in runtime.processes.values_mut() {
                proc.generation += 1; // cancels any scheduled respawn
                if let Some(id) = proc.term_id.take() {
                    registry.close(id);
                }
                // Dropping the watcher stops notify → the consumer thread's
                // channel disconnects and it exits on its next recv.
                proc.watcher = None;
                proc.watcher_rx = None;
            }
        }
        inner.runtimes.clear();
        inner.notify_changed();
    }

    /// The shared state Arc. Helper threads (exit handlers, watcher
    /// consumers, the ticker) clone this so they can lock/re-lock without a
    /// Tauri `State`.
    fn arc(&self) -> Arc<Mutex<Inner>> {
        self.inner.clone()
    }

    fn store<'a>(&'a self) -> Result<std::sync::MutexGuard<'a, Inner>, ControlError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| ControlError::Internal("projects state poisoned".to_owned()))?;
        if inner.store.is_none() {
            return Err(ControlError::Internal(
                "projects store not initialized".to_owned(),
            ));
        }
        Ok(inner)
    }

    /// A stored project's (name, root) for an agent spawn's cwd and
    /// `agent_instructions`. `None` for an unknown id or an uninitialized
    /// store.
    pub fn project_root(&self, id: u32) -> Option<(String, PathBuf)> {
        let inner = self.store().ok()?;
        let store = inner.store.as_ref()?;
        store.get(id).map(|p| (p.name.clone(), p.path.clone()))
    }
}

/// Wall-clock ms for the project-model lifecycle (fake-able there, real here).
/// One clock reading for the whole workspace — see `term_core::actor::epoch_ms`
/// (its never-zero clamp is the `last_output_ms` sentinel and is harmless
/// here: a lifecycle timestamp of 0 vs 1 ms after the epoch is the same lie).
fn now_ms() -> u64 {
    epoch_ms()
}

/// Project-file commands run through the execution profile (see the module doc
/// for why this differs from the interactive `default_shell()`).
///
/// Makes the profile a SETTING (`default_exec_profile`) instead of a
/// hardcode. Two builtin names keep the behavior exactly — `"cmd"`
/// (`cmd /C <command>`) and `"sh"` (`sh -c <command>`) — and one of them is
/// the platform default, so the out-of-the-box behavior is unchanged. Any
/// other value names a SHELL PROFILE; that profile's PROGRAM (its own args
/// are dropped — a profile line like `powershell.exe -NoLogo` is an
/// interactive invocation, not a command runner) is invoked with the flag
/// this heuristic picks:
///
/// - program file stem "powershell" or "pwsh" → `-NoLogo -Command <command>`
/// - program file stem "cmd"                  → `/C <command>`
/// - anything else (bash/zsh/sh/…)            → `-c <command>`
///
/// EXACT file-stem matching: substring matching handed
/// `cmder` cmd's `/C`. Still a heuristic for hand-entered shells; a real fix
/// would be a per-profile exec-flag field in the settings schema (candidate
/// refactor, NOT done here).
///
/// The profile lookup deliberately IGNORES the enable toggle — the pane
/// scopes that toggle to the new-terminal menu ("disabling a profile hides
/// it from the new-terminal menu, it does not retire it as the project-file
/// runner"), and a disabled runner silently downgrading to `cmd /C` would
/// be a surprise.
pub(crate) fn execution_command(command: &str, settings: &Settings) -> (String, Vec<String>) {
    let profile_name = settings.default_exec_profile.as_str();
    let is_builtin = profile_name == EXEC_PROFILE_CMD || profile_name == EXEC_PROFILE_SH;
    if !is_builtin {
        if let Some(profile) = settings.profile(profile_name) {
            let (program, _profile_args) = split_command_line(&profile.command);
            if !program.is_empty() {
                let stem = Path::new(&program)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                let mut args: Vec<String> = match stem.as_str() {
                    "powershell" | "pwsh" => {
                        vec!["-NoLogo".to_owned(), "-Command".to_owned()]
                    }
                    "cmd" => vec!["/C".to_owned()],
                    _ => vec!["-c".to_owned()],
                };
                args.push(command.to_owned());
                return (program, args);
            }
        }
    }
    // Builtins are honored EXPLICITLY (the pane offers both on every
    // platform): the platform default only decides what an untouched config
    // uses. An unknown profile name (or one with an empty command line)
    // falls back to the platform builtin.
    match profile_name {
        EXEC_PROFILE_SH => ("sh".to_owned(), vec!["-c".to_owned(), command.to_owned()]),
        EXEC_PROFILE_CMD => ("cmd".to_owned(), vec!["/C".to_owned(), command.to_owned()]),
        _ if cfg!(windows) => ("cmd".to_owned(), vec!["/C".to_owned(), command.to_owned()]),
        _ => ("sh".to_owned(), vec!["-c".to_owned(), command.to_owned()]),
    }
}

/// Resolve a process's spawn spec: execution profile + contained working_dir +
/// yml env merged over inherited (the actor applies `spec.env` last). A
/// working_dir that escapes the project root fails THIS process, not the
/// project.
fn build_spec(def: &ProcessDef, root: &Path, settings: &Settings) -> Result<PtySpec, String> {
    let cwd = match def.resolve_working_dir(root)? {
        Some(dir) => dir,
        None => root.to_path_buf(),
    };
    let (command, args) = execution_command(&def.command, settings);
    let (args, indirection_env) = cmd_quote_indirection(&command, args);
    Ok(PtySpec {
        command,
        args,
        cwd: Some(cwd),
        env: def
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(indirection_env)
            .collect(),
        cols: 80,
        rows: 24,
    })
}

/// Env var carrying a project command to `cmd.exe` when it contains double
/// quotes (Windows only). CreateProcessW argument quoting turns an embedded
/// `"` into `\"`, which `cmd /C` does NOT unescape (it is not a
/// CommandLineToArgvW consumer) — the child then sees the backslashes
/// (`py -c "import x"` → python: unterminated string literal, 2026-08-30).
/// Passing the text through an env var keeps it off the CreateProcess line
/// entirely: `cmd /C call %CHAPPA_CMD%` expands it verbatim at parse time,
/// and `call` gives the expanded text its own `%VAR%` expansion pass so a
/// command's own variables still resolve. Commands without quotes keep the
/// plain `/C <command>` form (identical behaviour to before).
pub const CMD_INDIRECTION_VAR: &str = "CHAPPA_CMD";

pub(crate) fn cmd_quote_indirection(program: &str, args: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    let is_cmd = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    let needs = is_cmd
        && args.len() == 2
        && args[0].eq_ignore_ascii_case("/c")
        && args[1].contains('"');
    if !needs {
        return (args, Vec::new());
    }
    let command = args.into_iter().nth(1).unwrap_or_default();
    (
        vec![
            "/C".to_owned(),
            "call".to_owned(),
            format!("%{CMD_INDIRECTION_VAR}%"),
        ],
        vec![(CMD_INDIRECTION_VAR.to_owned(), command)],
    )
}

/// Emit one `project://process_status` event (the process-row lifecycle
/// source for the frontend; the registry's `term://status` stays the
/// per-terminal source).
fn emit_status(
    app: &AppHandle,
    project_id: u32,
    name: &str,
    status: ProcessStatus,
    exit_code: Option<i32>,
    term_id: Option<TermId>,
) {
    let _ = app.emit(
        "project://process_status",
        &serde_json::json!({
            "project_id": project_id,
            "name": name,
            "status": status.as_str(),
            "exit_code": exit_code,
            "term_id": term_id,
        }),
    );
}

/// Wire one process's terminal spawn up with its auto_restart / watcher hooks.
/// `root` is an OWNED clone of the project root so the caller never holds a
/// `&ProjectRuntime` alongside the `&mut ProcessRuntime` it also passes
/// (borrow discipline). `term_id` is the registry key to (re)use — the first
/// spawn allocates it, respawns reuse it. Callers hold the projects lock.
fn spawn_process(
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    project_id: u32,
    name: &str,
    root: PathBuf,
    settings: &Settings,
    proc: &mut ProcessRuntime,
) -> Result<TermId, String> {
    let spec = build_spec(&proc.def, &root, settings)?;
    let channel = proc
        .channel
        .clone()
        .ok_or_else(|| format!("process `{name}` has no frames channel"))?;
    // Project terminals get the same spawn-time settings as
    // interactive ones (both are also live-settable afterwards).
    let cfg = actor_config(spec, None, Some(settings));
    proc.generation += 1;
    proc.spawned_at = Instant::now();
    proc.lifecycle.on(ProcessEvent::Spawned, now_ms());

    // The FIRST spawn arms the restart_when_changed consumer thread, which
    // takes ownership of the receiver; later respawns (auto_restart, watcher)
    // reuse that same long-lived thread.
    let start_watcher = proc.watcher_rx.is_some() && !proc.watcher_started;
    if start_watcher {
        proc.watcher_started = true;
    }
    let projects_for_handler = projects.clone();
    let on_exit = make_exit_handler(
        registry.clone(),
        app.clone(),
        projects,
        project_id,
        name.to_owned(),
    );
    let term_id = match proc.term_id {
        Some(id) => registry.respawn_terminal(
            id,
            cfg,
            Arc::new(ChannelSink::new(channel, app.clone())),
            Some(app.clone()),
            name.to_owned(),
            Some(on_exit),
        )?,
        None => registry.create_terminal_with_exit(
            cfg,
            Arc::new(ChannelSink::new(channel, app.clone())),
            Some(app.clone()),
            name.to_owned(),
            Some(on_exit),
        )?,
    };
    proc.term_id = Some(term_id);
    // The control snapshot (and timer name resolution) know
    // which project owns this terminal.
    registry.set_project(term_id, Some(project_id));
    // Every successful project-process create — first spawn AND
    // auto-restart/watcher respawn (both funnel through here) — broadcasts
    // `term://created` so a webview that does not already know the terminal
    // can adopt it into a rail row.
    emit_created(
        &AppCreatedBroadcast(app.clone()),
        CreatedEvent {
            term_id,
            name: name.to_owned(),
            kind: "process",
            project_id: Some(project_id),
            agent_tool_id: None,
            parent_process_id: None,
        },
    );
    if start_watcher {
        let rx = proc
            .watcher_rx
            .take()
            .expect("start_watcher implies a receiver");
        start_watcher_consumer(
            projects_for_handler,
            registry.clone(),
            app.clone(),
            project_id,
            name.to_owned(),
            rx,
        );
    }
    emit_status(
        app,
        project_id,
        name,
        ProcessStatus::Starting,
        proc.exit_code,
        Some(term_id),
    );
    Ok(term_id)
}

/// The pump's exited-event hook: update the project lifecycle, and when an
/// auto_restart process dies (not user-stopped) schedule a backoff respawn.
/// The respawn itself runs on a separate thread so this hook never blocks the
/// pump.
fn make_exit_handler(
    registry: Registry,
    app: AppHandle,
    projects: Arc<Mutex<Inner>>,
    project_id: u32,
    process_name: String,
) -> ExitHandler {
    Arc::new(move |_term_id, code, success| {
        let restart_plan = {
            let mut inner = match projects.lock() {
                Ok(inner) => inner,
                Err(_) => return, // poisoned: nothing left to manage
            };
            if inner.shutting_down {
                return;
            }
            let proc = match inner
                .runtimes
                .get_mut(&project_id)
                .and_then(|r| r.processes.get_mut(&process_name))
            {
                Some(proc) => proc,
                None => return,
            };
            proc.exit_code = code;
            let status = proc
                .lifecycle
                .on(ProcessEvent::Exited { success }, now_ms());
            emit_status(&app, project_id, &process_name, status, code, proc.term_id);
            // Restartable: auto_restart is set, this exit is not a user stop
            // (term_id would already be gone — stop takes it), and the process
            // isn't in the stopped state. The generation captured here is the
            // contract the scheduled respawn checks after the backoff sleep.
            let plan = if proc.def.auto_restart
                && proc.lifecycle.status() != ProcessStatus::Stopped
                && proc.term_id.is_some()
            {
                let delay = proc.backoff.delay();
                proc.backoff.record_failure();
                Some((delay, proc.generation))
            } else {
                None
            };
            // The status just moved (exited/failed): a `control_start` parked
            // on a spawn that died immediately gets its answer now.
            inner.notify_changed();
            plan
        };
        if let Some((delay, generation)) = restart_plan {
            let projects = projects.clone();
            let registry = registry.clone();
            let app = app.clone();
            let process_name = process_name.clone();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                respawn_process(
                    &registry,
                    &app,
                    &projects,
                    project_id,
                    &process_name,
                    generation,
                );
            });
        }
    })
}

/// One auto-restart / watcher respawn. Runs on its own thread (backoff sleep
/// or watcher consumer), locking the projects state fresh. `generation` is
/// the value the scheduling side captured; a user stop/start since then (or a
/// second signal) supersedes this respawn.
fn respawn_process(
    registry: &Registry,
    app: &AppHandle,
    projects: &Arc<Mutex<Inner>>,
    project_id: u32,
    process_name: &str,
    generation: u32,
) {
    let mut inner = match projects.lock() {
        Ok(inner) => inner,
        Err(_) => return, // poisoned: nothing left to manage
    };
    if inner.shutting_down {
        return;
    }
    // Clone the pieces we need BEFORE taking `&mut proc` (borrow discipline:
    // never hold `&ProjectRuntime` alongside `&mut ProcessRuntime`).
    let root = match inner.runtimes.get(&project_id) {
        Some(runtime) => runtime.root.clone(),
        None => return,
    };
    // Settings are re-read on every respawn, so an exec-profile change takes
    // effect on the next restart without reopening the project.
    let settings = inner.settings.snapshot();
    let proc = match inner
        .runtimes
        .get_mut(&project_id)
        .and_then(|r| r.processes.get_mut(process_name))
    {
        Some(proc) => proc,
        None => return,
    };
    if proc.generation != generation {
        return; // superseded by a user action
    }
    if proc.lifecycle.status() == ProcessStatus::Stopped {
        return; // stopped-by-user never restarts
    }
    if proc.term_id.is_none() {
        return; // never started: nothing to respawn
    }
    let _ = spawn_process(
        registry,
        app,
        projects.clone(),
        project_id,
        process_name,
        root,
        &settings,
        proc,
    );
    // A fresh term_id / generation for a process someone may be waiting on.
    inner.notify_changed();
}

/// ONE consumer thread per process, armed at first spawn. Each debounced
/// restart signal stops + respawns the process through the same
/// generation-checked path the auto_restart backoff uses, so a user action
/// during the debounce window wins. The thread ends when the project closes:
/// dropping `RestartWatcher` disconnects the signal channel.
fn start_watcher_consumer(
    projects: Arc<Mutex<Inner>>,
    registry: Registry,
    app: AppHandle,
    project_id: u32,
    process_name: String,
    rx: Receiver<()>,
) {
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            // Capture the generation at signal time: a stop that lands after
            // the debounce but before this thread locks still supersedes.
            let generation = match projects.lock() {
                Ok(inner) => inner
                    .runtimes
                    .get(&project_id)
                    .and_then(|r| r.processes.get(&process_name))
                    .map(|p| p.generation),
                Err(_) => return,
            };
            let Some(generation) = generation else { return };
            respawn_process(
                &registry,
                &app,
                &projects,
                project_id,
                &process_name,
                generation,
            );
        }
    });
}

/// One ticker: processes that have been alive ≥500ms with no output graduate
/// from starting → running (the part of the status mapping the registry's
/// first-frame transition doesn't cover — silent daemons produce no frame).
fn spawn_ticker(projects: Arc<Mutex<Inner>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(200));
        let now = now_ms();
        let mut inner = match projects.lock() {
            Ok(inner) => inner,
            Err(_) => continue,
        };
        if inner.shutting_down {
            return;
        }
        let app = match inner.app.clone() {
            Some(app) => app,
            None => continue,
        };
        let mut graduated = false;
        for (project_id, runtime) in inner.runtimes.iter_mut() {
            for (name, proc) in runtime.processes.iter_mut() {
                if proc.lifecycle.status() == ProcessStatus::Starting
                    && proc.spawned_at.elapsed() >= Duration::from_millis(RUNNING_AFTER_MS)
                {
                    let status = proc.lifecycle.tick(now);
                    graduated = true;
                    emit_status(
                        &app,
                        *project_id,
                        name,
                        status,
                        proc.exit_code,
                        proc.term_id,
                    );
                }
            }
        }
        if graduated {
            inner.notify_changed(); // starting → running is a status change
        }
    });
}

// ---- commands ---------------------------------------------------------------

/// `[{id, name, path, icon}]` — the switcher dropdown rows.
#[tauri::command]
pub fn list_projects(projects: State<'_, Projects>) -> Result<Vec<ProjectInfoDto>, String> {
    let inner = projects.store()?;
    let store = inner.store.as_ref().unwrap();
    Ok(store.list().iter().map(ProjectInfoDto::from).collect())
}

/// Open the OS folder picker and answer the chosen directory as a PLAIN path
/// string (never a `file://` URI — the frontend feeds it straight back into
/// `add_project`, which does `Path::new(&path)`). `None` = the user cancelled.
///
/// `tauri-plugin-dialog` is wired on the Rust side plus the capability only;
/// the npm package is deliberately NOT installed, so ipc.ts stays the only
/// Tauri surface. This command IS that surface.
///
/// ASYNC + blocking pool, deliberately (review fix): a synchronous
/// `#[tauri::command]` runs on the MAIN thread, and `blocking_pick_folder`
/// parks its calling thread on a channel while the dialog runs on the
/// platform's UI loop — parked main thread ⇒ deadlock/frozen UI. An async
/// command runs on the async runtime; the park itself still belongs on the
/// blocking pool, not a tokio worker.
#[tauri::command]
pub async fn pick_directory(
    app: AppHandle,
    title: Option<String>,
    start: Option<String>,
) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        let mut builder = app.dialog().file();
        if let Some(title) = title {
            builder = builder.set_title(title);
        }
        // A remembered start dir that no longer exists would make some
        // platform pickers open at an arbitrary place (or fail); skip it
        // unless it is a real directory.
        if let Some(start) = start {
            let dir = PathBuf::from(start);
            if dir.is_dir() {
                builder = builder.set_directory(dir);
            }
        }
        builder
            .blocking_pick_folder()
            .and_then(|picked| picked.into_path().ok())
            .map(|p| p.display().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Register a directory as a project: accepts ANY directory —
/// bare or not — with `chappa.yml` as the project file. `chappa.yml` in the
/// directory is adopted (name/icon/defs); a bare directory is a valid empty
/// project and gains a file only once a command is added. `icon` comes from
/// the project file when one exists.
///
/// `name` is the user's explicit choice from the add-project modal.
/// When it is `None` — or blank after trimming — the name is derived EXACTLY
/// as it was before: the yml's `name`, else the directory name.
///
/// Re-adding an already-registered directory WITH an explicit name applies it
/// as a rename to the stored row (`ProjectStore::rename`) — silently keeping
/// the old name after the user typed a new one read as a broken rename
///. `name: None` (untouched modal field) keeps the
/// return-existing-row behavior untouched.
#[tauri::command]
pub fn add_project(
    projects: State<'_, Projects>,
    path: String,
    name: Option<String>,
) -> Result<ProjectInfoDto, String> {
    let mut inner = projects.store()?;
    let project = add_to_store(inner.store.as_mut().unwrap(), &path, name)?;
    Ok(ProjectInfoDto::from(&project))
}

/// The whole body of `add_project`, minus the `State` — so the name rule is
/// testable against a tempdir store instead of a live Tauri app.
fn add_to_store(
    store: &mut ProjectStore,
    path: &str,
    name: Option<String>,
) -> Result<project_model::projects::Project, String> {
    let root = Path::new(path);
    // `chappa.yml` is the project file: a directory that carries one provides
    // the name/icon and becomes the project's store defs.
    let yml = ProjectYml::load(root);
    // The user's EXPLICIT choice (trimmed; blank counts as absent), kept
    // aside: `ProjectStore::add` is idempotent by canonical path and IGNORES
    // the name for an already-registered directory.
    let explicit = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    let resolved = match (&yml, explicit.clone()) {
        (Some(yml), _) => resolve_add_name(explicit.clone(), yml, root)?,
        // Bare directory, no project file: the explicit name, else the folder
        // name.
        (None, Some(name)) => name,
        (None, None) => root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| "cannot derive a project name".to_owned())?,
    };
    let icon = yml.as_ref().and_then(|y| y.icon.clone());
    let project = store
        .add(PathBuf::from(path), resolved, icon)
        .map_err(|e| e.to_string())?;
    if let Some(explicit) = explicit {
        if project.name != explicit {
            // The directory was already registered under another name (a
            // FRESH add takes `explicit` via resolve_add_name, so a mismatch
            // can only mean the existing-row path): the typed name is a
            // rename of the stored row, not noise to drop.
            store.rename(project.id, explicit)?;
            return store
                .get(project.id)
                .cloned()
                .ok_or_else(|| format!("project {} vanished during rename", project.id));
        }
    }
    // A FRESH row (never touched by the native store) adopts the project
    // file's defs; a re-added existing row keeps its stored data untouched.
    if store.get(project.id).is_some_and(|p| p.processes.is_none()) {
        adopt_project_file(store, project.id, yml.as_ref())?;
        return store
            .get(project.id)
            .cloned()
            .ok_or_else(|| format!("project {} vanished during add", project.id));
    }
    Ok(project)
}

/// A freshly added row adopts the project file's defs into the store (empty
/// when the directory has no `chappa.yml`). The file already exists if it is
/// there, so nothing is written; a bare project simply gains no file.
fn adopt_project_file(
    store: &mut ProjectStore,
    id: u32,
    yml: Option<&ProjectYml>,
) -> Result<(), String> {
    let defs = yml.map(|y| y.processes.clone()).unwrap_or_default();
    store.set_processes(id, defs)
}

/// The name rule. An explicit, non-blank-after-trim `name` wins; anything
/// else (`None`, `""`, whitespace) falls back to the old derivation —
/// the yml's `name`, else the directory name — so "defaults to the folder name
/// exactly as today" holds byte for byte.
fn resolve_add_name(
    explicit: Option<String>,
    yml: &ProjectYml,
    root: &Path,
) -> Result<String, String> {
    if let Some(name) = explicit
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
    {
        return Ok(name);
    }
    yml.name
        .clone()
        .or_else(|| root.file_name().map(|s| s.to_string_lossy().into_owned()))
        .ok_or_else(|| "cannot derive a project name".to_owned())
}

/// Rename a stored project — the switcher's per-row ✎ affordance.
/// Store-only, exactly like `set_notification_level`: nothing here touches the
/// project's project file or its live processes. A blank-after-trim name or an
/// unknown id is an `Err` with nothing persisted (validated in project-model).
#[tauri::command]
pub fn rename_project(projects: State<'_, Projects>, id: u32, name: String) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner.store.as_mut().unwrap().rename(id, name)
}

/// Forget a project. Never touches the directory; stops any live processes.
#[tauri::command]
pub fn remove_project(
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    if let Some(mut runtime) = inner.runtimes.remove(&id) {
        for proc in runtime.processes.values_mut() {
            proc.generation += 1; // cancels any scheduled respawn
            proc.watcher = None;
            proc.watcher_rx = None;
            if let Some(term_id) = proc.term_id.take() {
                registry.close(term_id);
            }
        }
        inner.notify_changed();
    }
    inner.store.as_mut().unwrap().remove(id)
}

/// Load a stored project's `chappa.yml` fresh and build per-process runtime
/// state (status `stopped`, nothing spawned). Returns the trust-gate answer:
/// when the current project-file hash isn't recorded as trusted, the frontend
/// must show the auto-start commands and call `confirm_project_trust` before
/// starting them.
#[tauri::command]
pub fn open_project(
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
) -> Result<OpenProjectResult, String> {
    let mut inner = projects.store()?;
    let store = inner.store.as_ref().unwrap();
    let stored = store
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no such project: {id}"))?;
    let root = stored.path.clone();
    // `chappa.yml` is the project file, always — processes come from
    // it, the trust gate guards it, and it is watched for external edits.
    let project_file = root.join(project_model::yml::CHAPPA_YML_NAME);
    let (processes, yml_hash, trust_pending, name, icon) = if !project_file.exists() {
        // A bare project: no file until the first command is added. Empty
        // defs, no trust gate, no load error — the first mutation
        // materializes chappa.yml, which the armed watcher then reloads.
        (
            IndexMap::new(),
            String::new(),
            false,
            stored.name.clone(),
            stored.icon.clone(),
        )
    } else {
        let yml = ProjectYml::load(&root).ok_or_else(|| {
            format!(
                "cannot load {} in {}",
                project_model::yml::CHAPPA_YML_NAME,
                root.display()
            )
        })?;
        let yml_hash = hash::file_content_hash(&project_file)
            .ok_or_else(|| {
                format!(
                    "cannot hash {} in {}",
                    project_model::yml::CHAPPA_YML_NAME,
                    root.display()
                )
            })?;
        // Trust is read here, while `store`'s borrow of `inner` is still live —
        // below we need `inner` mutably for the runtime swap.
        let trust_pending = !store.is_trusted(id, &yml_hash);
        let processes: IndexMap<String, ProcessRuntime> = yml
            .processes
            .into_iter()
            .map(|(n, def)| (n, fresh_runtime(&root, def)))
            .collect();
        let name = yml.name.clone().unwrap_or(stored.name.clone());
        let icon = yml.icon.clone().or(stored.icon.clone());
        (processes, yml_hash, trust_pending, name, icon)
    };

    // Closing any existing runtime makes open_project idempotent: the yml is
    // re-read fresh and previous statuses/watchers are discarded. Live
    // terminals are closed here — the reopen owns the project view, and
    // leaving them running would orphan the process (nothing references it).
    if let Some(mut old) = inner.runtimes.remove(&id) {
        for proc in old.processes.values_mut() {
            proc.generation += 1;
            proc.watcher = None;
            proc.watcher_rx = None;
            if let Some(term_id) = proc.term_id.take() {
                registry.close(term_id);
            }
        }
    }

    let trust_commands: Vec<String> = processes
        .values()
        .filter(|p| p.def.auto_start)
        .map(|p| p.def.command.clone())
        .collect();

    // Watch chappa.yml itself so an external edit (or our own
    // write-back) hot-reloads through the diffing path.
    let yml_watcher = match (inner.app.clone(), yml_watcher_for(&root)) {
        (Some(app), Some((watcher, rx))) => {
            start_yml_reload_consumer(projects.arc(), registry.inner().clone(), app, id, rx);
            Some(watcher)
        }
        _ => None,
    };

    inner.runtimes.insert(
        id,
        ProjectRuntime {
            root: root.clone(),
            yml_hash,
            processes,
            load_error: None,
            _yml_watcher: yml_watcher,
        },
    );
    // Every process of this project just got a fresh (stopped, no term_id)
    // runtime — including any a `control_start` is currently waiting on.
    inner.notify_changed();
    // The notification fields come off the STORED row (projects.json),
    // never the yml — "chappa.yml is NEVER touched for this".
    let overrides = stored.process_overrides.clone();
    Ok(OpenProjectResult {
        project: ProjectInfoDto {
            id,
            name,
            path: root.display().to_string(),
            icon: icon.clone(),
            notification_level: stored.notification_level.clone(),
        },
        trust_pending,
        trust_commands,
        processes: inner
            .runtimes
            .get(&id)
            .unwrap()
            .processes
            .iter()
            .map(|(name, p)| ProjectProcessDto::from_process(name, p, overrides.as_ref()))
            .collect(),
    })
}

/// The trust-gate answer. `run = true` records the CURRENT yml hash as trusted
/// (the frontend then starts the auto-start processes); `false` just declines
/// — the gate re-arms on the next open.
#[tauri::command]
pub fn confirm_project_trust(
    projects: State<'_, Projects>,
    id: u32,
    run: bool,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    let root = inner
        .runtimes
        .get(&id)
        .map(|r| r.root.clone())
        .or_else(|| {
            inner
                .store
                .as_ref()
                .unwrap()
                .get(id)
                .map(|p| p.path.clone())
        })
        .ok_or_else(|| format!("no such project: {id}"))?;
    if run {
        let hash = hash::file_content_hash(&root.join(project_model::yml::CHAPPA_YML_NAME))
            .ok_or_else(|| {
                format!(
                    "cannot hash {} in {}",
                    project_model::yml::CHAPPA_YML_NAME,
                    root.display()
                )
            })?;
        // Keep the runtime's hash in sync so a later hash comparison (if the
        // file changed again mid-session) sees the trusted value.
        if let Some(runtime) = inner.runtimes.get_mut(&id) {
            runtime.yml_hash = hash.clone();
        }
        inner.store.as_mut().unwrap().mark_trusted(id, hash)
    } else {
        Ok(())
    }
}

/// Current process statuses for the loaded project (reconcile / refresh).
#[tauri::command]
pub fn list_project_processes(
    projects: State<'_, Projects>,
    id: u32,
) -> Result<Vec<ProjectProcessDto>, String> {
    let inner = projects.store()?;
    let runtime = inner
        .runtimes
        .get(&id)
        .ok_or_else(|| format!("project {id} is not open"))?;
    // RAW per-process overrides straight off the stored row. A
    // project that is open but no longer stored (removed mid-session) simply
    // has no overrides.
    let overrides = inner
        .store
        .as_ref()
        .unwrap()
        .get(id)
        .and_then(|p| p.process_overrides.as_ref());
    Ok(runtime
        .processes
        .iter()
        .map(|(name, p)| ProjectProcessDto::from_process(name, p, overrides))
        .collect())
}

/// Persistence: set or clear a notification level.
///
/// `process_name = None` targets the PROJECT default; `Some(name)` targets
/// that process's override. `level = None` CLEARS (removes the override /
/// resets the project field to `None` = 'all'). A `level` outside
/// `"all" | "important" | "none"` is an error and persists nothing.
///
/// Deliberately store-only: the pump keeps emitting raw events and filtering
/// happens at the UI decision function, so levels change with zero Rust
/// churn. Nothing here reaches into the registry.
#[tauri::command]
pub fn set_notification_level(
    projects: State<'_, Projects>,
    project_id: u32,
    process_name: Option<String>,
    level: Option<String>,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner
        .store
        .as_mut()
        .unwrap()
        .set_notification_level(project_id, process_name.as_deref(), level)
}

// ---- chappa.yml write-back + chappa-side per-process toggles ------
//
// Every yml mutation goes through `project_model::writeback::apply` (fresh load
// → edit → one-time backup → concurrent-change re-hash → atomic write; a
// mid-flight external write refuses with `writeback::CHANGED_ON_DISK` and
// `refused_write` reloads immediately), then `after_own_write`: the
// trust hash follows the file when it was trusted before (the user made
// this edit), and the project reloads through the SAME diffing path the
// the chappa.yml watcher uses — so a mutation that only touches process B never
// restarts process A, and our own write echoing back through the watcher
// 500ms later is a no-op diff.

/// The editor's form (wire: camelCase). `working_dir` empty/blank = project
/// root; `env` keeps the row order the user typed (IndexMap end to end).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessDefDto {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default = "default_true")]
    pub auto_start: bool,
    #[serde(default)]
    pub auto_restart: bool,
    #[serde(default)]
    pub restart_when_changed: Vec<String>,
    /// Ordered `[key, value]` pairs — NOT an object: Tauri deserializes
    /// arguments through `serde_json::Value`, whose map is a BTreeMap in this
    /// workspace (no `preserve_order`), so an object would arrive alphabetized
    /// and every save would reorder the file's env block.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

fn default_true() -> bool {
    true
}

impl ProcessDefDto {
    pub(crate) fn into_def(self) -> (String, ProcessDef) {
        let working_dir = self
            .working_dir
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        (
            self.name,
            ProcessDef {
                command: self.command.trim().to_owned(),
                working_dir,
                auto_start: self.auto_start,
                auto_restart: self.auto_restart,
                restart_when_changed: self
                    .restart_when_changed
                    .into_iter()
                    .map(|g| g.trim().to_owned())
                    .filter(|g| !g.is_empty())
                    .collect(),
                env: self.env.into_iter().collect(),
                unknown: IndexMap::new(),
            },
        )
    }
}

/// `Edit command…` (`original_name = Some`) or `+ Add command` (`None`).
/// Returns the project's rows after the reload (empty when it is not open —
/// the file was still written).
///
/// The project file is unconditional — edits run through
/// `project_model::native::mutate` (store draft → validate → backwrite
/// chappa.yml, creating it on the first added command), then reload through
/// the diffing path.
#[tauri::command]
pub fn save_project_process(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    original_name: Option<String>,
    def: ProcessDefDto,
) -> Result<Vec<ProjectProcessDto>, String> {
    let mut inner = projects.store()?;
    refuse_while_load_error(&inner, id)?;
    let (name, def) = def.into_def();
    // The project file is unconditional: the mutation drafts from
    // the store, validates, and backwrites chappa.yml, creating it on the
    // first added command. A concurrent-change or load-error refusal refuses
    // the WHOLE mutation with the store untouched (`mutate`), and `refused_write`
    // reloads the project when the refusal was a concurrent disk change.
    let was_trusted = currently_trusted(&inner, id);
    project_model::native::mutate(
        inner.store.as_mut().unwrap(),
        id,
        |defs| match &original_name {
            Some(old) => project_model::native::edit_process(defs, old, &name, def),
            None => project_model::native::add_process(defs, &name, def),
        },
    )
    .map_err(|e| refused_write(&mut inner, &registry, &app, projects.arc(), id, e))?;
    // A rename is a key move in the file, so it is a key move
    // in the runtime too — the live terminal, panel and status carry across
    // under the new name instead of the reload reading remove+add and killing
    // the process. The chappa-side override record moves with it.
    if let Some(old) = original_name.as_deref().filter(|old| *old != name) {
        move_runtime_on_rename(&mut inner, id, old, &name);
    }
    after_own_write(&mut inner, &registry, &app, projects.arc(), id, was_trusted);
    Ok(process_dtos(&inner, id))
}

/// `Delete command "<name>"`. The project file is written FIRST — `mutate`
/// can still refuse (fresh load failed, name gone, backup/rename error) and a
/// refused delete must not have killed anything. Then the
/// running process, if any, is stopped under the lock (like `stop_process`),
/// and its chappa-side override record is pruned.
#[tauri::command]
pub fn delete_project_process(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    name: String,
) -> Result<Vec<ProjectProcessDto>, String> {
    let mut inner = projects.store()?;
    refuse_while_load_error(&inner, id)?;
    let was_trusted = currently_trusted(&inner, id);
    project_model::native::mutate(inner.store.as_mut().unwrap(), id, |defs| {
        project_model::native::remove_process(defs, &name)
    })
    .map_err(|e| refused_write(&mut inner, &registry, &app, projects.arc(), id, e))?;
    // Refused deletes never reach here, so the process is only stopped once
    // the definition is actually gone — delete still stops a running process.
    stop_locked(&mut inner, &registry, &app, id, &name);
    if let Err(e) = inner.store.as_mut().unwrap().remove_process_override(id, &name) {
        log::warn!("delete `{name}`: override record not pruned: {e}");
    }
    after_own_write(&mut inner, &registry, &app, projects.arc(), id, was_trusted);
    Ok(process_dtos(&inner, id))
}

/// `Duplicate to ▸` (one name) / `Duplicate all commands to ▸` (`names`
/// EMPTY = every command of the source, read from the SOURCE's own regime — a
/// UI snapshot of names can lag an external edit and must not fail the whole
/// batch; review). Definitions are read FRESH from the source (the
/// project file when backed, the STORE when native-only) and written ONLY to
/// the target;
/// the source project is not touched. `target == source` is the same-project
/// copy (` copy` suffix). Returns the TARGET's rows when it is open (it
/// hot-reloads).
///
/// The write honors the TARGET project's own regime — sync ON
/// backwrites the yml, native-only mutates the target's STORE.
#[tauri::command]
pub fn duplicate_project_processes(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    source_id: u32,
    names: Vec<String>,
    target_id: u32,
) -> Result<Vec<ProjectProcessDto>, String> {
    let mut inner = projects.store()?;
    refuse_while_load_error(&inner, target_id)?;
    // Source = the store's defs (canonical between reads, refreshed by every
    // import and mutation). Definitions are written ONLY to the target.
    let source_processes = inner.store.as_ref().unwrap().process_defs(source_id);
    let names: Vec<String> = if names.is_empty() {
        source_processes.keys().cloned().collect()
    } else {
        names
    };
    let defs: Vec<(String, ProcessDef)> = names
        .iter()
        .map(|n| {
            source_processes
                .get(n)
                .map(|d| (n.clone(), d.clone()))
                .ok_or_else(|| format!("no command named `{n}` in project {source_id}"))
        })
        .collect::<Result<_, _>>()?;
    if defs.is_empty() {
        return Err("nothing to duplicate".to_owned());
    }
    let was_trusted = currently_trusted(&inner, target_id);
    // The write always backwrites the target's chappa.yml.
    project_model::native::mutate(inner.store.as_mut().unwrap(), target_id, |out| {
        for (name, repro) in &defs {
            project_model::native::duplicate_process(out, name, repro.clone())?;
        }
        Ok(())
    })
    .map_err(|e| refused_write(&mut inner, &registry, &app, projects.arc(), target_id, e))?;
    after_own_write(&mut inner, &registry, &app, projects.arc(), target_id, was_trusted);
    Ok(process_dtos(&inner, target_id))
}

/// `Add to favorites` / its removal — projects.json only, NEVER the yml.
#[tauri::command]
pub fn set_process_favorite(
    projects: State<'_, Projects>,
    project_id: u32,
    process_name: String,
    favorite: bool,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner
        .store
        .as_mut()
        .unwrap()
        .set_process_favorite(project_id, &process_name, favorite)
}

/// `Disable automatic renaming` — projects.json only, NEVER the yml. The
/// frontend reads the flag off the process DTO and drops OSC title events
/// for that process's terminal.
#[tauri::command]
pub fn set_process_auto_rename(
    projects: State<'_, Projects>,
    project_id: u32,
    process_name: String,
    disabled: bool,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner
        .store
        .as_mut()
        .unwrap()
        .set_process_auto_rename_disabled(project_id, &process_name, disabled)
}

/// "Never write while a load error is outstanding." The write-back re-checks
/// by loading fresh; this is the early, cheaper refusal with the recorded
/// reason.
fn refuse_while_load_error(inner: &Inner, id: u32) -> Result<(), String> {
    match inner.runtimes.get(&id).and_then(|r| r.load_error.as_ref()) {
        Some(e) => Err(format!(
            "{} is not loadable, refusing to write: {e}",
            project_model::yml::CHAPPA_YML_NAME
        )),
        None => Ok(()),
    }
}

/// Is the file ON DISK right now the trusted one? Read before a write so
/// `after_own_write` can carry trust across our own edit.
fn currently_trusted(inner: &Inner, id: u32) -> bool {
    let store = inner.store.as_ref().unwrap();
    let Some(root) = store.get(id).map(|p| p.path.clone()) else {
        return false;
    };
    hash::file_content_hash(&root.join(project_model::yml::CHAPPA_YML_NAME))
        .map(|h| store.is_trusted(id, &h))
        .unwrap_or(false)
}

/// A refused write-back, before it propagates to the caller's modal: the
/// concurrent-change refusal (the guard in `writeback::apply`) additionally
/// fires the reload NOW — the watcher would catch the external edit
/// within its debounce anyway, but the rail should already show the file's
/// version when the "changed on disk" message is on screen. Other refusal
/// reasons (name clash, containment, load error) reload nothing.
fn refused_write(
    inner: &mut Inner,
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    id: u32,
    err: String,
) -> String {
    if err == writeback::CHANGED_ON_DISK {
        reload_locked(inner, registry, app, projects, id);
    }
    err
}

/// After a successful write-back: re-trust the new content when the old was
/// trusted (this edit is the user's own — re-arming the gate for it would be
/// noise), then reload through the diffing path. Reload failures are logged,
/// not propagated: the write succeeded and the watcher will retry.
fn after_own_write(
    inner: &mut Inner,
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    id: u32,
    was_trusted: bool,
) {
    if was_trusted {
        let root = inner.store.as_ref().unwrap().get(id).map(|p| p.path.clone());
        if let Some(hash) =
            root.and_then(|r| hash::file_content_hash(&r.join(project_model::yml::CHAPPA_YML_NAME)))
        {
            let _ = inner.store.as_mut().unwrap().mark_trusted(id, hash);
        }
    }
    reload_locked(inner, registry, app, projects, id);
}

/// A rename is a KEY MOVE in the process source, so it is a
/// key move in the runtime too — the live terminal, panel and status carry
/// across under the new name instead of the reload reading remove+add and
/// killing the process. The chappa-side override record moves with it under
/// the same lock. Used by `save_project_process` in BOTH sync regimes (the
/// move is source-agnostic).
fn move_runtime_on_rename(inner: &mut Inner, id: u32, old: &str, new: &str) {
    if old == new {
        return;
    }
    if let Some(runtime) = inner.runtimes.get_mut(&id) {
        if let Some(idx) = runtime.processes.get_index_of(old) {
            let (_, proc) = runtime.processes.shift_remove_index(idx).unwrap();
            runtime.processes.insert(new.to_owned(), proc);
            let last = runtime.processes.len() - 1;
            runtime.processes.move_index(last, idx);
        }
    }
    if let Err(e) = inner
        .store
        .as_mut()
        .unwrap()
        .rename_process_override(id, old, new)
    {
        log::warn!("rename `{old}` -> `{new}`: override record not moved: {e}");
    }
}

/// The stop half, callable while the caller already holds the lock. A
/// process that is not open / not live is a no-op (measured, 0.7.1).
fn stop_locked(inner: &mut Inner, registry: &Registry, app: &AppHandle, id: u32, name: &str) {
    let Some(proc) = inner
        .runtimes
        .get_mut(&id)
        .and_then(|r| r.processes.get_mut(name))
    else {
        return;
    };
    let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
    proc.generation += 1;
    if let Some(term_id) = proc.term_id.take() {
        registry.close(term_id);
    }
    let exit_code = proc.exit_code;
    inner.notify_changed();
    emit_status(app, id, name, status, exit_code, None);
}

/// The rows of an OPEN project (empty when it is not open).
fn process_dtos(inner: &Inner, id: u32) -> Vec<ProjectProcessDto> {
    let Some(runtime) = inner.runtimes.get(&id) else {
        return Vec::new();
    };
    // No unwrap under the lock (the control surface reaches this too): an
    // uninitialized or already-removed store just means no overrides to fold in.
    let overrides = inner
        .store
        .as_ref()
        .and_then(|store| store.get(id))
        .and_then(|p| p.process_overrides.as_ref());
    runtime
        .processes
        .iter()
        .map(|(name, p)| ProjectProcessDto::from_process(name, p, overrides))
        .collect()
}

/// A never-started runtime for one definition (open + reload's "added").
fn fresh_runtime(root: &Path, def: ProcessDef) -> ProcessRuntime {
    let (watcher, watcher_rx) = watcher_for(root, &def);
    ProcessRuntime {
        def,
        lifecycle: Lifecycle::new(),
        term_id: None,
        exit_code: None,
        backoff: RestartBackoff::new(),
        channel: None,
        watcher,
        watcher_rx,
        watcher_started: false,
        spawned_at: Instant::now(),
        generation: 0,
    }
}

/// Emit `project://yml_reloaded` — the frontend re-fetches the rows on
/// `error: null`, or shows the reason and disables mutations otherwise.
fn emit_reloaded(
    app: &AppHandle,
    project_id: u32,
    error: Option<&str>,
    trust_pending: bool,
    trust_commands: &[String],
) {
    let _ = app.emit(
        "project://yml_reloaded",
        &serde_json::json!({
            "project_id": project_id,
            "error": error,
            "trust_pending": trust_pending,
            "trust_commands": trust_commands,
        }),
    );
}

/// Reload one open project, applying the diff:
/// - parse failure (SYNC only) → `load_error` set, nothing restarted, nothing
///   removed;
/// - removed → stopped and dropped;
/// - added → fresh (never started) runtime;
/// - changed spawn inputs → def replaced, restart_when_changed watcher
///   rebuilt, and — only if LIVE — restarted in place (same term id and
///   frames channel, the auto_restart respawn path);
/// - policy-only change (`auto_start`/`auto_restart`/unknown keys) → def
///   replaced, process left running;
/// - untouched → untouched. Order follows the source.
///
/// The process SOURCE follows the sync regime. Sync ON loads (and
/// trusts) the project file — the trust gate guards
/// imported auto-start/command changes. Native-only loads from the STORE, never the
/// yml: there is no trust gate (the store is the user's own edits by
/// construction) and no yml watcher, so `trusted` is always true and a store
/// read can never fail to load.
fn reload_locked(
    inner: &mut Inner,
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    id: u32,
) {
    let Some(root) = inner.runtimes.get(&id).map(|r| r.root.clone()) else {
        return;
    };
    let settings = inner.settings.snapshot();
    // The process source is chappa.yml, always — load it, hash it,
    // and gate on trust.
    let yml = match ProjectYml::load_result(&root) {
        Ok(yml) => yml,
        Err(e) => {
            if let Some(runtime) = inner.runtimes.get_mut(&id) {
                runtime.load_error = Some(e.clone());
            }
            emit_reloaded(app, id, Some(&e), false, &[]);
            return;
        }
    };
    let new_hash = hash::file_content_hash(&root.join(project_model::yml::CHAPPA_YML_NAME))
        .unwrap_or_default();
    // Trust is per CONTENT HASH, not a one-time
    // gate at open. Content we have not confirmed — an external edit, an
    // editor, a git pull — must never be EXECUTED by a reload; the gate
    // re-arms instead.
    let trusted = inner.store.as_ref().unwrap().is_trusted(id, &new_hash);
    let new_defs = yml.processes;
    let mut held_back: Vec<String> = Vec::new();
    let runtime = inner.runtimes.get_mut(&id).unwrap();
    runtime.load_error = None;
    runtime.yml_hash = new_hash;
    let mut old = std::mem::take(&mut runtime.processes);
    let old_defs: IndexMap<String, ProcessDef> =
        old.iter().map(|(n, p)| (n.clone(), p.def.clone())).collect();
    let diff = writeback::diff_processes(&old_defs, &new_defs);

    let mut processes = IndexMap::new();
    for (name, def) in new_defs {
        let proc = match old.shift_remove(&name) {
            None => fresh_runtime(&root, def),
            Some(mut proc) => {
                if diff.changed.contains(&name) {
                    proc.def = def;
                    // New patterns → new watcher. The old consumer thread ends
                    // when the old watcher drops; the next spawn arms a new one.
                    proc.watcher = None;
                    proc.watcher_rx = None;
                    proc.watcher_started = false;
                    let (watcher, rx) = watcher_for(&root, &proc.def);
                    proc.watcher = watcher;
                    proc.watcher_rx = rx;
                    let live = proc.term_id.is_some()
                        && matches!(
                            proc.lifecycle.status(),
                            ProcessStatus::Starting | ProcessStatus::Running
                        );
                    if live && !trusted {
                        // Untrusted new definition of a RUNNING process: stop
                        // it (the old command must not keep running under a
                        // definition the user never saw) and hold the restart
                        // until the gate is answered.
                        held_back.push(proc.def.command.clone());
                        let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
                        proc.generation += 1;
                        if let Some(term_id) = proc.term_id.take() {
                            registry.close(term_id);
                        }
                        emit_status(app, id, &name, status, proc.exit_code, None);
                    } else if live {
                        // Validate the new spec BEFORE touching the running
                        // process: a bad working_dir must leave the old
                        // command running with its true status, not report
                        // Failed over a still-live child.
                        match build_spec(&proc.def, &root, &settings) {
                            Err(e) => {
                                log::warn!(
                                    "reload: `{name}` keeps running on its old definition — the new one does not spawn: {e}"
                                );
                            }
                            Ok(_) => {
                                proc.lifecycle.on(ProcessEvent::Stop, now_ms());
                                proc.generation += 1;
                                if let Err(e) = spawn_process(
                                    registry,
                                    app,
                                    projects.clone(),
                                    id,
                                    &name,
                                    root.clone(),
                                    &settings,
                                    &mut proc,
                                ) {
                                    log::warn!("reload: restart of `{name}` failed: {e}");
                                    proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
                                    emit_status(app, id, &name, ProcessStatus::Failed, None, proc.term_id);
                                }
                            }
                        }
                    }
                } else if diff.policy_only.contains(&name) {
                    proc.def = def;
                }
                proc
            }
        };
        processes.insert(name, proc);
    }
    // Whatever is left in `old` was removed from the file: stop it.
    for (_, mut proc) in old {
        proc.generation += 1;
        proc.watcher = None;
        proc.watcher_rx = None;
        if let Some(term_id) = proc.term_id.take() {
            registry.close(term_id);
        }
    }
    // The gate lists what the untrusted content would run: the held-back
    // restarts plus any newly added auto-start command.
    let mut trust_commands = held_back;
    if !trusted {
        for name in &diff.added {
            if let Some(p) = processes.get(name).filter(|p| p.def.auto_start) {
                trust_commands.push(p.def.command.clone());
            }
        }
    }
    inner.runtimes.get_mut(&id).unwrap().processes = processes;
    // The diff may have stopped, restarted, added or dropped processes.
    inner.notify_changed();
    emit_reloaded(app, id, None, !trusted, &trust_commands);
}

/// The `chappa.yml` watcher: ONE file, non-recursive, the shared 500ms
/// debounce. `for_chappa_yml`, NOT `new` — the restart matcher's own-artifact
/// exclusion covers the project file itself, and with `new` this watcher
/// filtered out every event it was armed for (found live 2026-08-31; external
/// edits never hot-reloaded).
fn yml_watcher_for(root: &Path) -> Option<(RestartWatcher, Receiver<()>)> {
    let matcher = ChangeMatcher::for_chappa_yml(root.to_path_buf());
    RestartWatcher::start_with_mode(root.to_path_buf(), matcher, RecursiveMode::NonRecursive)
        .ok()
}

/// Per open project: each debounced `chappa.yml` change reloads it. Ends when
/// the project's `yml_watcher` drops (close/remove/reopen/shutdown).
fn start_yml_reload_consumer(
    projects: Arc<Mutex<Inner>>,
    registry: Registry,
    app: AppHandle,
    id: u32,
    rx: Receiver<()>,
) {
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            let mut inner = match projects.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            if inner.shutting_down {
                return;
            }
            // Our own write-back already reloaded under the
            // lock; the watcher's echo of that write carries the same content
            // hash and is skipped, so the frontend sees ONE reload per edit.
            // A load error keeps the runtime hash stale, so retries still run.
            let same_content = inner.runtimes.get(&id).is_some_and(|r| {
                r.load_error.is_none()
                    && hash::file_content_hash(&r.root.join(project_model::yml::CHAPPA_YML_NAME))
                        .is_some_and(|h| h == r.yml_hash)
            });
            if same_content {
                continue;
            }
            reload_locked(&mut inner, &registry, &app, projects.clone(), id);
        }
    });
}

/// Start one process. The frontend supplies the frames Channel (per-terminal
/// webview artifact); the process then runs via the execution profile with its
/// working_dir/env resolved. Idempotent: starting an already-live process is a
/// no-op returning its existing term id.
#[tauri::command]
pub fn start_project_process(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    Ok(start_process(&app, &registry, &projects, id, name, frames)?)
}

/// Stop one process. Idempotent: stopping a non-running process is a no-op
/// success (measured, 0.7.1), and a stopped process never auto-restarts.
#[tauri::command]
pub fn stop_project_process(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    name: String,
) -> Result<(), String> {
    Ok(stop_process(&registry, &app, &projects, id, &name)?)
}

/// Restart one process: stop if live, then start (a stopped process just
/// starts — matches the visible actions). Needs a frames Channel because a
/// never-started process has none yet.
#[tauri::command]
pub fn restart_project_process(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    stop_process(&registry, &app, &projects, id, &name)?;
    Ok(start_process(&app, &registry, &projects, id, name, frames)?)
}

// ---- control surface ---------------------------------------------
//
// The control HTTP server (control_http.rs) drives the SAME lifecycle the
// frontend does, with one schema-forced twist for starts: a spawn needs the
// webview's frames Channel, which Rust cannot fabricate. So `control_start`
// asks the frontend to start the process (`project://start_requested`, the
// exact path a rail click takes) and then waits — bounded — for the runtime
// to show a live term id. Stop is pure Rust. The idempotency rules are the
// ones verbatim: start on live = no-op with the live id, stop on
// non-running = no-op success, restart on stopped = start.

/// One control-surface project row: the stored fields plus whether the
/// project is currently open (only open projects have process runtimes).
#[derive(Debug, Clone, Serialize)]
pub struct ControlProjectDto {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub icon: Option<String>,
    pub open: bool,
    /// Process rows when open (status, term_id, …), empty otherwise.
    pub processes: Vec<ProjectProcessDto>,
}

/// The reply to a control start/stop/restart: the process's current status
/// and term id after the action. `pending` is true when a requested start
/// had not produced a live terminal before the wait ran out (the frontend is
/// busy or gone) — poll `get_project` for the outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ControlLifecycleDto {
    pub project_id: u32,
    pub name: String,
    pub status: &'static str,
    pub term_id: Option<TermId>,
    pub pending: bool,
}

impl Projects {
    /// `list_projects` for the control surface: every stored project.
    pub fn control_list(&self) -> Result<Vec<ControlProjectDto>, ControlError> {
        let inner = self.store()?;
        // `store()` guarantees the store is Some; read it without unwrapping
        // under the lock all the same (a panic here poisons the app's whole
        // project state — the review's rule).
        let Some(store) = inner.store.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(store
            .list()
            .iter()
            .map(|p| control_project_dto(&inner, p))
            .collect())
    }

    /// `get_project(id)` for the control surface; unknown id = error.
    pub fn control_get(&self, id: u32) -> Result<ControlProjectDto, ControlError> {
        let inner = self.store()?;
        let project = inner
            .store
            .as_ref()
            .and_then(|store| store.get(id))
            .ok_or_else(|| ControlError::no_project(id))?;
        Ok(control_project_dto(&inner, project))
    }

    /// Stop one process (idempotency: non-running = no-op success).
    pub fn control_stop(
        &self,
        registry: &Registry,
        id: u32,
        name: &str,
    ) -> Result<ControlLifecycleDto, ControlError> {
        let app = self.app_handle()?;
        stop_process(registry, &app, self, id, name)?;
        self.lifecycle_dto(id, name, false)
    }

    /// Start one process. Live → idempotent no-op with its id. Otherwise the
    /// frontend is asked to start it and we wait up to `wait_ms` for the
    /// spawn to register, answering `pending: true` on timeout rather than
    /// failing — the start is still in flight.
    ///
    /// The wait PARKS on [`Inner::changed`] (cleanup: it used to
    /// re-lock the store every 50ms), so the answer lands as soon as the
    /// frontend's spawn registers instead of up to a tick later, and the
    /// request thread costs nothing while it waits. Every write path that
    /// touches term_id/status/generation notifies that condvar; the deadline
    /// remains the backstop.
    ///
    /// Review: the DTO is built from the SAME lock scope that
    /// inspected the process — `lifecycle_dto` re-locks the non-reentrant
    /// store, and calling it while a guard is alive self-deadlocked the
    /// request thread on the very first idempotent start. Nothing here
    /// unwraps under the lock (a panic would poison the store app-wide), and
    /// nothing bumps `generation` before the frontend has acted: every spawn
    /// (and every failed spawn attempt) bumps it itself, so the counter is
    /// only ever advanced by something that actually happened — an unanswered
    /// request must not cancel a pending auto-restart as a side effect.
    pub fn control_start(
        &self,
        id: u32,
        name: &str,
        wait_ms: u64,
    ) -> Result<ControlLifecycleDto, ControlError> {
        let app = self.app_handle()?;
        let before_generation = {
            let inner = self.store()?;
            let runtime = inner
                .runtimes
                .get(&id)
                .ok_or_else(|| ControlError::not_open(id))?;
            let proc = runtime
                .processes
                .get(name)
                .ok_or_else(|| ControlError::no_process(name, id))?;
            if live_term(proc).is_some() {
                return Ok(lifecycle_dto_of(id, name, proc, false)); // already live
            }
            proc.generation
        };
        let _ = app.emit(
            "project://start_requested",
            &serde_json::json!({"project_id": id, "name": name}),
        );
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut inner = self.store()?;
        loop {
            let proc = inner
                .runtimes
                .get(&id)
                .and_then(|r| r.processes.get(name))
                .ok_or_else(|| {
                    ControlError::NotFound(format!("process `{name}` vanished during start"))
                })?;
            let attempted = proc.generation > before_generation;
            let live = attempted && live_term(proc).is_some();
            // A spawn attempt that already failed is an answer too.
            let failed = attempted && proc.lifecycle.status() == ProcessStatus::Failed;
            if live || failed {
                return Ok(lifecycle_dto_of(id, name, proc, false));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(lifecycle_dto_of(id, name, proc, true));
            }
            // Park until a write path signals (or the deadline). The predicate
            // above is re-checked under this same lock on every wake, so a
            // signal cannot be missed and a spurious one is free. Never
            // unwrapped: a poisoned lock is reported, not panicked on.
            let changed = inner.changed.clone();
            inner = match changed.wait_timeout(inner, remaining) {
                Ok((inner, _)) => inner,
                Err(_) => {
                    return Err(ControlError::Internal("projects state poisoned".to_owned()))
                }
            };
        }
    }

    /// Review: a control-surface close of a terminal that a
    /// project process OWNS must go through the lifecycle (`stop_process`),
    /// or the runtime keeps a dead `term_id` as "running" forever — start
    /// answers "already live", output 404s, auto_restart never fires. Finds
    /// the owner under one lock, then stops it through the normal path.
    /// `Ok(false)` = no project process owns this terminal.
    pub fn stop_owner_of_terminal(
        &self,
        registry: &Registry,
        term_id: TermId,
    ) -> Result<bool, ControlError> {
        let owner = {
            let inner = self.store()?;
            inner.runtimes.iter().find_map(|(pid, r)| {
                r.processes
                    .iter()
                    .find(|(_, p)| p.term_id == Some(term_id))
                    .map(|(name, _)| (*pid, name.clone()))
            })
        };
        match owner {
            Some((pid, name)) => {
                let app = self.app_handle()?;
                stop_process(registry, &app, self, pid, &name)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Restart: stop (no-op when not running) then the start path above.
    pub fn control_restart(
        &self,
        registry: &Registry,
        id: u32,
        name: &str,
        wait_ms: u64,
    ) -> Result<ControlLifecycleDto, ControlError> {
        self.control_stop(registry, id, name)?;
        self.control_start(id, name, wait_ms)
    }

    fn app_handle(&self) -> Result<AppHandle, ControlError> {
        self.store()?
            .app
            .clone()
            .ok_or_else(|| ControlError::Internal("app handle not initialized".to_owned()))
    }

    fn lifecycle_dto(
        &self,
        id: u32,
        name: &str,
        pending: bool,
    ) -> Result<ControlLifecycleDto, ControlError> {
        let inner = self.store()?;
        let proc = inner
            .runtimes
            .get(&id)
            .and_then(|r| r.processes.get(name))
            .ok_or_else(|| ControlError::no_process(name, id))?;
        Ok(lifecycle_dto_of(id, name, proc, pending))
    }
}

/// The lifecycle answer for a process the caller already holds a reference
/// to (no locking — see `control_start`).
fn lifecycle_dto_of(id: u32, name: &str, proc: &ProcessRuntime, pending: bool) -> ControlLifecycleDto {
    ControlLifecycleDto {
        project_id: id,
        name: name.to_owned(),
        status: proc.lifecycle.status().as_str(),
        term_id: proc.term_id,
        pending,
    }
}

fn control_project_dto(inner: &Inner, p: &project_model::projects::Project) -> ControlProjectDto {
    ControlProjectDto {
        id: p.id,
        name: p.name.clone(),
        path: p.path.display().to_string(),
        icon: p.icon.clone(),
        open: inner.runtimes.contains_key(&p.id),
        // Same rows the frontend's project view gets (`process_dtos` is empty
        // for a project that is not open, which is exactly this contract).
        processes: process_dtos(inner, p.id),
    }
}

/// The start half of the lifecycle commands; a plain `&Registry` / `&Projects`
/// pair so commands never construct `tauri::State` by hand.
fn start_process(
    app: &AppHandle,
    registry: &Registry,
    projects: &Projects,
    id: u32,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, ControlError> {
    let mut inner = projects.store()?;
    // Live check + clone the owned pieces BEFORE taking `&mut proc` (borrow
    // discipline: `spawn_process` needs `&mut ProcessRuntime` and an owned
    // root, never a live `&ProjectRuntime` at the same time).
    let live = inner
        .runtimes
        .get(&id)
        .and_then(|r| r.processes.get(&name))
        .and_then(live_term);
    if let Some(term_id) = live {
        return Ok(term_id); // already live: idempotent
    }
    let root = inner
        .runtimes
        .get(&id)
        .map(|r| r.root.clone())
        .ok_or_else(|| ControlError::not_open(id))?;
    let def = inner
        .runtimes
        .get(&id)
        .and_then(|r| r.processes.get(&name))
        .map(|p| p.def.clone())
        .ok_or_else(|| ControlError::no_process(&name, id))?;
    // Read the execution profile + per-actor controls fresh at every
    // start (same borrow discipline — owned clone before `&mut proc`).
    let settings = inner.settings.snapshot();

    let proc = inner
        .runtimes
        .get_mut(&id)
        .and_then(|r| r.processes.get_mut(&name))
        .unwrap();
    proc.channel = Some(frames);
    // A dead-but-registered term (crashed, no auto_restart) lingers in the
    // registry; a user start must not leave it orphaned. Its pump is already
    // finished (the child exited), so closing it under the lock is safe.
    if let Some(term_id) = proc.term_id.take() {
        registry.close(term_id);
    }
    let result = match build_spec(&def, &root, &settings) {
        Ok(_) => {
            proc.term_id = None; // force a fresh id on this start
            let spawn = spawn_process(
                registry,
                app,
                projects.arc(),
                id,
                &name,
                root,
                &settings,
                proc,
            );
            match spawn {
                Ok(term_id) => {
                    proc.backoff.reset();
                    Ok(term_id)
                }
                Err(e) => {
                    proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
                    emit_status(app, id, &name, ProcessStatus::Failed, None, proc.term_id);
                    Err(ControlError::Internal(e))
                }
            }
        }
        Err(e) => {
            // A failed attempt still counts as an attempt: `control_start`
            // waits for the generation to move, and a user action supersedes
            // any scheduled auto-restart the same way a spawn would.
            proc.generation += 1;
            proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
            emit_status(app, id, &name, ProcessStatus::Failed, None, proc.term_id);
            Err(ControlError::Internal(e))
        }
    };
    // Every branch above moved term_id/status/generation — wake `control_start`
    // (which is parked on exactly this) before the guard drops.
    inner.notify_changed();
    result
}

/// The process's terminal WHEN IT COUNTS AS LIVE — the one
/// idempotency rule ("start on live = no-op with the live id"), shared by the
/// frontend start path and `control_start` so the two can never disagree
/// about what "already running" means.
fn live_term(proc: &ProcessRuntime) -> Option<TermId> {
    match proc.lifecycle.status() {
        ProcessStatus::Starting | ProcessStatus::Running => proc.term_id,
        _ => None,
    }
}

/// The stop half of the lifecycle commands; a plain `&Registry` / `&Projects`
/// pair so commands never construct `tauri::State` by hand.
fn stop_process(
    registry: &Registry,
    app: &AppHandle,
    projects: &Projects,
    id: u32,
    name: &str,
) -> Result<(), ControlError> {
    let mut inner = projects.store()?;
    let runtime = inner
        .runtimes
        .get_mut(&id)
        .ok_or_else(|| ControlError::not_open(id))?;
    let proc = runtime
        .processes
        .get_mut(name)
        .ok_or_else(|| ControlError::no_process(name, id))?;
    let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
    proc.generation += 1; // cancels any scheduled auto-restart / watcher respawn
    let term_id = proc.term_id.take();
    if let Some(term_id) = term_id {
        registry.close(term_id);
    }
    let exit_code = proc.exit_code;
    inner.notify_changed();
    emit_status(app, id, name, status, exit_code, None);
    Ok(())
}

/// Build the restart_when_changed watcher for a process. The handle is parked
/// in `ProcessRuntime` for the process's lifetime (dropping it stops notify);
/// the receiver carries the debounced restart signals, consumed by the
/// per-process consumer thread started at first spawn. Both are `None` when
/// the yml lists no patterns or every glob was invalid.
fn watcher_for(root: &Path, def: &ProcessDef) -> (Option<RestartWatcher>, Option<Receiver<()>>) {
    if def.restart_when_changed.is_empty() {
        return (None, None);
    }
    let matcher = ChangeMatcher::new(root.to_path_buf(), &def.restart_when_changed);
    if matcher.is_empty() {
        return (None, None); // every glob was invalid — nothing to watch
    }
    match RestartWatcher::start(root.to_path_buf(), matcher) {
        Ok((watcher, rx)) => (Some(watcher), Some(rx)),
        // notify can fail (unwatchable dir, missing inotify): degrade to no
        // watcher rather than failing the whole open.
        Err(_) => (None, None),
    }
}

// ---- DTOs -------------------------------------------------------------------

/// Switcher/header rows: `{id, name, path, icon}` plus the
/// `notificationLevel`.
///
/// `rename_all = "camelCase"` is a no-op for the four original single-word
/// keys and makes the new one land as the wire key `notificationLevel`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfoDto {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub icon: Option<String>,
    /// The RAW project default (`"all" | "important" | "none"`), or
    /// `null` when unset. NOT resolved — resolution (per-process override →
    /// project default → 'all') lives in the UI.
    pub notification_level: Option<String>,
}

impl From<&project_model::projects::Project> for ProjectInfoDto {
    fn from(p: &project_model::projects::Project) -> Self {
        Self {
            id: p.id,
            name: p.name.clone(),
            path: p.path.display().to_string(),
            icon: p.icon.clone(),
            notification_level: p.notification_level.clone(),
        }
    }
}

/// One process row in the project view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectProcessDto {
    pub name: String,
    pub command: String,
    pub status: &'static str,
    pub auto_start: bool,
    pub auto_restart: bool,
    pub restart_when_changed: Vec<String>,
    pub term_id: Option<u32>,
    pub exit_code: Option<i32>,
    /// The RAW per-process override for THIS process name (wire key
    /// `notificationLevel`), or `null` when the process has none. NOT
    /// resolved — the UI resolves per-process override → project default →
    /// 'all', so `null` here means "inherit", never 'all'.
    pub notification_level: Option<String>,
    /// The rest of the definition, so `Edit command…` can pre-fill
    /// without a second round trip. `working_dir` as written in the yml
    /// (null = project root); `env` in file order.
    pub working_dir: Option<String>,
    pub env: IndexMap<String, String>,
    /// Chappa-side toggles (projects.json, never the yml).
    pub favorite: bool,
    pub disable_auto_rename: bool,
}

impl ProjectProcessDto {
    /// `overrides` is the stored project's `process_overrides` map;
    /// it is passed in rather than looked up because the caller already holds
    /// the projects lock and the runtime borrow.
    fn from_process(
        name: &str,
        p: &ProcessRuntime,
        overrides: Option<&BTreeMap<String, ProcessOverride>>,
    ) -> Self {
        let record = overrides.and_then(|m| m.get(name));
        Self {
            name: name.to_owned(),
            command: p.def.command.clone(),
            status: p.lifecycle.status().as_str(),
            auto_start: p.def.auto_start,
            auto_restart: p.def.auto_restart,
            restart_when_changed: p.def.restart_when_changed.clone(),
            term_id: p.term_id,
            exit_code: p.exit_code,
            notification_level: record.and_then(|r| r.notification_level.clone()),
            working_dir: p.def.working_dir.as_ref().map(|d| d.display().to_string()),
            env: p.def.env.clone(),
            favorite: record.map_or(false, |r| r.favorite),
            disable_auto_rename: record.map_or(false, |r| r.disable_auto_rename),
        }
    }
}

/// The `open_project` answer: the project header, the process rows, and — when
/// the current project-file hash isn't trusted — the auto-start command list
/// for the Run/Skip confirm.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenProjectResult {
    pub project: ProjectInfoDto,
    pub trust_pending: bool,
    pub trust_commands: Vec<String>,
    pub processes: Vec<ProjectProcessDto>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::settings::ShellProfile;

    fn settings_with(exec: &str, profiles: Vec<ShellProfile>) -> Settings {
        let mut s = Settings::default();
        s.default_exec_profile = exec.to_owned();
        s.shell_profiles = profiles;
        s
    }

    fn profile(name: &str, command: &str, enabled: bool) -> ShellProfile {
        ShellProfile {
            name: name.to_owned(),
            command: command.to_owned(),
            enabled,
        }
    }

    /// The builtin names keep the execution profile byte for byte —
    /// and each builtin is honored EXPLICITLY (the pane
    /// offers both on every platform, so "sh" on Windows must mean sh, not
    /// silently cmd).
    #[test]
    fn builtin_exec_profiles_keep_the_task21_behavior() {
        let (args, env) = cmd_quote_indirection(
            "cmd",
            vec!["/C".to_owned(), r#"py -c "import time; time.sleep(999)""#.to_owned()],
        );
        assert_eq!(args, vec!["/C", "call", "%CHAPPA_CMD%"]);
        assert_eq!(
            env,
            vec![("CHAPPA_CMD".to_owned(), r#"py -c "import time; time.sleep(999)""#.to_owned())]
        );
        // No quotes → untouched (byte-identical behaviour to before).
        let (args, env) = cmd_quote_indirection("cmd", vec!["/C".to_owned(), "py app/x.py".to_owned()]);
        assert_eq!(args, vec!["/C", "py app/x.py"]);
        assert!(env.is_empty());
        // Not cmd → untouched.
        let (args, env) = cmd_quote_indirection("sh", vec!["-c".to_owned(), r#"echo "hi""#.to_owned()]);
        assert_eq!(args, vec!["-c", r#"echo "hi""#]);
        assert!(env.is_empty());
        let (program, args) = execution_command("py app/x.py", &settings_with("cmd", vec![]));
        assert_eq!(program, "cmd");
        assert_eq!(args, vec!["/C".to_owned(), "py app/x.py".to_owned()]);

        let (program, args) = execution_command("py app/x.py", &settings_with("sh", vec![]));
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-c".to_owned(), "py app/x.py".to_owned()]);
    }

    /// A profile-named exec target uses the profile's PROGRAM plus the flag
    /// this heuristic picks from the program name.
    #[test]
    fn named_profile_picks_the_exec_flag_by_program_name() {
        let profiles = vec![
            profile("PS", "powershell.exe -NoLogo", true),
            profile("PS7", "pwsh.exe", true),
            profile("CMD", "C:/WINDOWS/system32/cmd.exe", true),
            profile("Bash", "C:/tools/bash.exe", true),
        ];
        let cases: &[(&str, &str, &[&str])] = &[
            // The profile's OWN args are dropped: `-NoLogo` here is the flag
            // set the heuristic supplies, not the profile's.
            ("PS", "powershell.exe", &["-NoLogo", "-Command"]),
            ("PS7", "pwsh.exe", &["-NoLogo", "-Command"]),
            ("CMD", "C:/WINDOWS/system32/cmd.exe", &["/C"]),
            ("Bash", "C:/tools/bash.exe", &["-c"]),
        ];
        for (name, want_program, want_flags) in cases {
            let settings = settings_with(name, profiles.clone());
            let (program, args) = execution_command("make", &settings);
            assert_eq!(&program, want_program, "profile {name}");
            let mut want: Vec<String> = want_flags.iter().map(|s| (*s).to_owned()).collect();
            want.push("make".to_owned());
            assert_eq!(args, want, "profile {name}");
        }
    }

    /// An unknown exec-profile name (or one with a blank command) degrades to
    /// the builtin — a bad settings value must never make a project
    /// unstartable.
    #[test]
    fn unknown_or_blank_exec_profile_falls_back_to_the_builtin() {
        for settings in [
            settings_with("ghost", vec![]),
            settings_with("Blank", vec![profile("Blank", "   ", true)]),
        ] {
            let (program, args) = execution_command("make", &settings);
            let builtin = if cfg!(windows) { "cmd" } else { "sh" };
            assert_eq!(program, builtin);
            assert_eq!(args.len(), 2);
            assert_eq!(args[1], "make");
        }
    }

    // ---- notification-level DTO wire keys -------------------------

    fn process_runtime(command: &str) -> ProcessRuntime {
        ProcessRuntime {
            def: ProcessDef {
                command: command.to_owned(),
                ..Default::default()
            },
            lifecycle: Lifecycle::new(),
            term_id: None,
            exit_code: None,
            backoff: RestartBackoff::new(),
            channel: None,
            watcher: None,
            watcher_rx: None,
            watcher_started: false,
            spawned_at: Instant::now(),
            generation: 0,
        }
    }

    /// The TS half builds against these exact keys: the level rides the
    /// project DTO as `notificationLevel` (camelCase, alongside the untouched
    /// `{id, name, path, icon}`) and the process DTO as `notificationLevel`.
    #[test]
    fn notification_level_wire_keys_are_camel_case() {
        let info = ProjectInfoDto {
            id: 1,
            name: "chappa-ai".into(),
            path: "/chappa-ai".into(),
            icon: None,
            notification_level: Some("important".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["notificationLevel"], "important");
        assert!(json.get("notification_level").is_none());
        // The wire DTO is exactly these five keys — no sync flag.
        let keys: std::collections::BTreeSet<&str> =
            json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "name", "path", "icon", "notificationLevel"]
                .into_iter()
                .collect::<std::collections::BTreeSet<&str>>()
        );
        // The pre-keys are untouched by the added rename_all.
        assert_eq!(json["id"], 1);
        assert_eq!(json["name"], "chappa-ai");
        assert_eq!(json["path"], "/chappa-ai");
        assert!(json["icon"].is_null());

        let json = serde_json::to_value(ProjectProcessDto::from_process(
            "server",
            &process_runtime("py app/server.py"),
            None,
        ))
        .unwrap();
        assert_eq!(json["name"], "server");
        assert!(json.get("notificationLevel").is_some());
        assert!(json["notificationLevel"].is_null());
        assert!(json.get("notification_level").is_none());
    }

    /// The DTOs carry the RAW stored values — resolution ("per-process
    /// override → project default → 'all'") is the UI's job, so an
    /// un-overridden process is `null` here, NEVER the project default and
    /// NEVER 'all'.
    #[test]
    fn process_dto_carries_the_raw_override_not_a_resolved_level() {
        let level = |l: &str| ProcessOverride {
            notification_level: Some(l.to_owned()),
            ..Default::default()
        };
        let mut overrides = BTreeMap::new();
        overrides.insert("server".to_owned(), level("none"));
        // Name with a space: project-file process keys are free-form strings.
        overrides.insert("watch build".to_owned(), level("all"));

        let dto = ProjectProcessDto::from_process(
            "server",
            &process_runtime("py app/server.py"),
            Some(&overrides),
        );
        assert_eq!(dto.notification_level.as_deref(), Some("none"));

        let dto = ProjectProcessDto::from_process(
            "watch build",
            &process_runtime("npm run dev"),
            Some(&overrides),
        );
        assert_eq!(dto.notification_level.as_deref(), Some("all"));

        // Not overridden → null (inherit), even though the map is non-empty.
        let dto = ProjectProcessDto::from_process(
            "summarize",
            &process_runtime("py app/summarize.py"),
            Some(&overrides),
        );
        assert_eq!(dto.notification_level, None);

        // No map at all (nothing ever overridden) → null too.
        let dto = ProjectProcessDto::from_process("summarize", &process_runtime("py x.py"), None);
        assert_eq!(dto.notification_level, None);
    }

    // ---- the add-project name rule --------------------------------

    /// Write a project root with a project file. `yml_name` = the file's own
    /// `name:` key (None writes a nameless yml, the common case).
    fn project_root(parent: &Path, folder: &str, yml_name: Option<&str>) -> PathBuf {
        let root = parent.join(folder);
        std::fs::create_dir_all(&root).unwrap();
        let mut text = String::new();
        if let Some(name) = yml_name {
            text.push_str(&format!("name: {name}\n"));
        }
        // A bare project file with one process — printable escapes only, never
        // a literal control byte in source.
        text.push_str("processes:\n  server:\n    command: py app/server.py\n");
        std::fs::write(root.join(project_model::yml::CHAPPA_YML_NAME), text).unwrap();
        root
    }

    /// An explicit name from the add modal is what gets PERSISTED — the whole
    /// point of the add modal ("the default is fine, the inability to change it is
    /// not").
    #[test]
    fn add_with_an_explicit_name_persists_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        let root = project_root(dir.path(), "chappa-ai", None);

        let p = add_to_store(
            &mut store,
            &root.display().to_string(),
            Some("notes companion".to_owned()),
        )
        .unwrap();
        assert_eq!(p.name, "notes companion");
        assert_eq!(
            ProjectStore::new(store_path).get(p.id).unwrap().name,
            "notes companion"
        );
    }

    /// Without an explicit name the derivation is EXACTLY the old one:
    /// the yml's `name`, else the folder basename. Boundary regime on the
    /// "explicit" side: None, empty and whitespace-only all mean "default".
    #[test]
    fn default_name_is_still_the_yml_name_then_the_folder_basename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));

        // No `name:` in the yml → the folder basename.
        let plain = project_root(dir.path(), "my-project", None);
        for (i, blank) in [None, Some(""), Some("   "), Some("\t"), Some("\n")]
            .into_iter()
            .enumerate()
        {
            let yml = ProjectYml::load(&plain).unwrap();
            let name = resolve_add_name(blank.map(str::to_owned), &yml, &plain).unwrap();
            assert_eq!(name, "my-project", "blank case {i}");
        }
        let p = add_to_store(&mut store, &plain.display().to_string(), None).unwrap();
        assert_eq!(p.name, "my-project");

        // A yml that names itself still wins over the folder basename.
        let named = project_root(dir.path(), "checkout-dir", Some("chappa-ai"));
        let q = add_to_store(&mut store, &named.display().to_string(), None).unwrap();
        assert_eq!(q.name, "chappa-ai");
        // …but an explicit name beats the yml name too.
        let third = project_root(dir.path(), "third", Some("from-yml"));
        let r = add_to_store(
            &mut store,
            &third.display().to_string(),
            Some("  spaced out  ".to_owned()),
        )
        .unwrap();
        assert_eq!(r.name, "spaced out", "explicit names are trimmed");
    }

    /// Re-adding an already-registered directory: an explicit (dirty) name
    /// that differs is applied as a RENAME of the stored row; `None` (the
    /// modal's untouched field) keeps today's return-existing-row behavior.
    #[test]
    fn re_add_renames_only_with_an_explicit_name() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        let root = project_root(dir.path(), "chappa-ai", None);
        let path = root.display().to_string();

        let first =
            add_to_store(&mut store, &path, Some("old name".to_owned())).unwrap();
        assert_eq!(first.name, "old name");

        // Untouched field (None) — the existing row comes back unrenamed.
        let same = add_to_store(&mut store, &path, None).unwrap();
        assert_eq!(same.id, first.id);
        assert_eq!(same.name, "old name");

        // Dirty explicit name — the stored row is renamed and persisted, and
        // the answer carries the NEW name (the frontend adopts it verbatim).
        let renamed =
            add_to_store(&mut store, &path, Some("new name".to_owned())).unwrap();
        assert_eq!(renamed.id, first.id);
        assert_eq!(renamed.name, "new name");
        assert_eq!(store.list().len(), 1, "a rename must never duplicate the row");
        assert_eq!(
            ProjectStore::new(store_path).get(first.id).unwrap().name,
            "new name"
        );

        // Re-typing the CURRENT name is a no-op, not an error.
        let unchanged =
            add_to_store(&mut store, &path, Some("new name".to_owned())).unwrap();
        assert_eq!(unchanged.name, "new name");
    }

    /// The enable toggle is scoped to the NEW-TERMINAL menu, as the settings
    /// pane documents — a disabled profile still runs project-file commands (
    /// review: the pane documented this while the runner refused disabled
    /// profiles).
    #[test]
    fn disabled_exec_profile_still_runs_project_file_commands() {
        let settings = settings_with("Off", vec![profile("Off", "bash.exe", false)]);
        let (program, args) = execution_command("make", &settings);
        assert_eq!(program, "bash.exe");
        assert_eq!(args, vec!["-c".to_owned(), "make".to_owned()]);
    }
}
