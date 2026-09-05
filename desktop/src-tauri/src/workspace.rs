//! Workspace-level commands.
//!
//! v1 of the workspace scope treats the app's existing top level (the bottom
//! rail area) as THE workspace; the only stored entity is a flat map of
//! workspace commands in `workspace.json` (beside projects.json). The STORE
//! half (read/write/validate/round-trip) lives in `project_model::workspace`;
//! this module is the thin Tauri glue: the managed state, the six frontend
//! commands, and the app-exit shutdown.
//!
//! The lifecycle reuses the SAME machinery the project processes use — the
//! registry (terminals, frames channels, exit handlers), the
//! project-model `Lifecycle`/`ProcessStatus` (incl. the 500ms-alive tick), and
//! the execution profile — but rows are addressed by a workspace
//! scope (command NAME) instead of a project id. No trust gate: workspace.json
//! is written only by the app (the native-store rationale).
//!
//! `restart_when_changed` is ACCEPTED on the stored definition but IGNORED
//! here — no watcher. The field rides the DTO/editor (the modal keeps its
//! row, exactly like the project editor; hiding it would be confusing), and
//! the spawn simply never watches. (A watcher for workspace commands is a
//! deliberate non-goal for now.)
//!
//! Auto_start at launch is FRONTEND-driven, exactly like project auto-starts:
//! a spawn MUST have the webview's frames channel, which Rust cannot
//! fabricate (the control-surface constraint). The UI reads the
//! stored `autoStart` flags off `list_workspace_commands` and starts each one
//! after settings load. `init` independently builds a never-started runtime
//! for every stored command so the rows are ready regardless.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::Serialize;
use project_model::backoff::RestartBackoff;
use project_model::settings::Settings;
use project_model::status::{Lifecycle, ProcessEvent, RUNNING_AFTER_MS};
use project_model::workspace::{validate_workspace_def, WorkspaceStore};
use project_model::yml::ProcessDef;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};
use term_core::actor::epoch_ms;
use term_core::pty::PtySpec;
use term_core::status::ProcessStatus;

use crate::projects::{cmd_quote_indirection, execution_command, ProcessDefDto};
use crate::registry::{actor_config, ChannelSink, ExitHandler, Registry, TermId};
use crate::settings::SettingsState;

/// One workspace command at runtime. Same shape as a project `ProcessRuntime`,
/// minus the project-root / watcher fields (no project, no restart_when_changed
/// watcher).
struct WsProc {
    def: ProcessDef,
    lifecycle: Lifecycle,
    term_id: Option<TermId>,
    exit_code: Option<i32>,
    backoff: RestartBackoff,
    /// The frontend's frames channel, retained so auto_restart can respawn
    /// into the SAME terminal id/channel without the webview re-subscribing.
    channel: Option<Channel<InvokeResponseBody>>,
    spawned_at: Instant,
    /// Incremented on every spawn/stop/restart; a scheduled auto-restart
    /// carries the generation it was scheduled under and bails if a user
    /// action superseded it while it waited.
    generation: u32,
}

fn fresh(def: ProcessDef) -> WsProc {
    WsProc {
        def,
        lifecycle: Lifecycle::new(),
        term_id: None,
        exit_code: None,
        backoff: RestartBackoff::new(),
        channel: None,
        spawned_at: Instant::now(),
        generation: 0,
    }
}

struct WsInner {
    store: Option<WorkspaceStore>,
    app: Option<AppHandle>,
    settings: SettingsState,
    /// Lifecycle state per command name, in stored order.
    procs: IndexMap<String, WsProc>,
    shutting_down: bool,
}

impl Default for WsInner {
    fn default() -> Self {
        Self {
            store: None,
            app: None,
            settings: SettingsState::default(),
            procs: IndexMap::new(),
            shutting_down: false,
        }
    }
}

/// The managed workspace state.
#[derive(Clone, Default)]
pub struct Workspace {
    inner: Arc<Mutex<WsInner>>,
}

impl Workspace {
    /// Called once from setup (AFTER settings load): point the
    /// store at `<app-config>/workspace.json` and start the 500ms-alive
    /// ticker. Builds a never-started runtime per stored command so the rows
    /// are addressable; actual auto_start spawns are frontend-initiated (the
    /// frames-channel constraint).
    pub fn init(&self, app: &AppHandle, config_dir: std::path::PathBuf, settings: SettingsState) {
        let store = WorkspaceStore::new(config_dir.join("workspace.json"));
        let defs = store.commands();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.store = Some(store);
            inner.app = Some(app.clone());
            inner.settings = settings;
            inner.procs = defs.into_iter().map(|(n, d)| (n, fresh(d))).collect();
        }
        spawn_ticker(self.inner.clone());
    }

    /// App-exit: stop every workspace command, cancel pending restarts, and
    /// let the registry's `shutdown_all` be authoritative.
    pub fn shutdown(&self, registry: &Registry) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutting_down = true;
        for proc in inner.procs.values_mut() {
            proc.generation += 1; // cancels any scheduled respawn
            if let Some(id) = proc.term_id.take() {
                registry.close(id);
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, WsInner>, String> {
        self.inner
            .lock()
            .map_err(|_| "workspace state poisoned".to_owned())
    }

    fn arc(&self) -> Arc<Mutex<WsInner>> {
        self.inner.clone()
    }
}

/// Wall-clock ms for the project-model lifecycle (see projects.rs `now_ms`).
fn now_ms() -> u64 {
    epoch_ms()
}

/// A workspace command's spawn spec. The working_dir rule is the workspace
/// one: ABSOLUTE path, or NONE = home (like a plain shell). Re-validated at
/// spawn so a hand-edited file cannot slip a relative path through.
fn build_workspace_spec(def: &ProcessDef, settings: &Settings) -> Result<PtySpec, String> {
    validate_workspace_def(def)?;
    let cwd = def.working_dir.as_ref().map(|d| d.to_path_buf());
    let (command, args) = execution_command(&def.command, settings);
    let (args, indirection_env) = cmd_quote_indirection(&command, args);
    Ok(PtySpec {
        command,
        args,
        cwd,
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

/// Emit `workspace://status` — the card-status channel for the COMMANDS rows
/// (the terminals themselves emit `term://status` from the registry pump, the
/// same events plain shells use; this mirrors `project://process_status`).
fn emit_status(
    app: &AppHandle,
    name: &str,
    status: ProcessStatus,
    exit_code: Option<i32>,
    term_id: Option<TermId>,
) {
    let _ = app.emit(
        "workspace://status",
        &serde_json::json!({
            "name": name,
            "status": status.as_str(),
            "exit_code": exit_code,
            "term_id": term_id,
        }),
    );
}

/// Wire one command's terminal spawn up with its auto_restart hook. `name` is
/// the command's key (there is no project id). Callers hold the workspace lock.
fn spawn_proc(
    registry: &Registry,
    app: &AppHandle,
    ws: Arc<Mutex<WsInner>>,
    name: &str,
    settings: &Settings,
    proc: &mut WsProc,
) -> Result<TermId, String> {
    let spec = build_workspace_spec(&proc.def, settings)?;
    let channel = proc
        .channel
        .clone()
        .ok_or_else(|| format!("command `{name}` has no frames channel"))?;
    let cfg = actor_config(spec, None, Some(settings));
    proc.generation += 1;
    proc.spawned_at = Instant::now();
    proc.lifecycle.on(ProcessEvent::Spawned, now_ms());

    let on_exit = make_exit_handler(registry.clone(), app.clone(), ws, name.to_owned());
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
    // Deliberately workspace-scoped: never bound to a project, so its
    // notification resolution follows the plain-shell rule (the UI's
    // expanded-project default fallback), "same rule as plain
    // shells".
    emit_status(
        app,
        name,
        ProcessStatus::Starting,
        proc.exit_code,
        Some(term_id),
    );
    Ok(term_id)
}

/// The pump's exited-event hook: update the lifecycle, and when an
/// auto_restart command dies (not user-stopped) schedule a backoff respawn.
/// The respawn runs on a separate thread so this hook never blocks the pump.
fn make_exit_handler(
    registry: Registry,
    app: AppHandle,
    ws: Arc<Mutex<WsInner>>,
    name: String,
) -> ExitHandler {
    Arc::new(move |_term_id, code, success| {
        let restart_plan = {
            let mut inner = match ws.lock() {
                Ok(inner) => inner,
                Err(_) => return, // poisoned: nothing left to manage
            };
            if inner.shutting_down {
                return;
            }
            let proc = match inner.procs.get_mut(&name) {
                Some(proc) => proc,
                None => return,
            };
            proc.exit_code = code;
            let status = proc.lifecycle.on(ProcessEvent::Exited { success }, now_ms());
            emit_status(&app, &name, status, code, proc.term_id);
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
            plan
        };
        if let Some((delay, generation)) = restart_plan {
            let ws = ws.clone();
            let registry = registry.clone();
            let app = app.clone();
            let name = name.clone();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                respawn_proc(&registry, &app, &ws, &name, generation);
            });
        }
    })
}

/// One auto-restart respawn, on its own thread. `generation` is the value the
/// scheduling side captured; a user stop/start since then supersedes it.
fn respawn_proc(
    registry: &Registry,
    app: &AppHandle,
    ws: &Arc<Mutex<WsInner>>,
    name: &str,
    generation: u32,
) {
    let mut inner = match ws.lock() {
        Ok(inner) => inner,
        Err(_) => return,
    };
    if inner.shutting_down {
        return;
    }
    let settings = inner.settings.snapshot();
    let proc = match inner.procs.get_mut(name) {
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
    let _ = spawn_proc(registry, app, ws.clone(), name, &settings, proc);
}

/// One ticker: commands alive ≥500ms with no output graduate starting →
/// running (the registry's first-frame transition doesn't cover silent
/// daemons). Mirrors projects.rs.
fn spawn_ticker(ws: Arc<Mutex<WsInner>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(200));
        let now = now_ms();
        let mut inner = match ws.lock() {
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
        for (name, proc) in inner.procs.iter_mut() {
            if proc.lifecycle.status() == ProcessStatus::Starting
                && proc.spawned_at.elapsed() >= Duration::from_millis(RUNNING_AFTER_MS)
            {
                let status = proc.lifecycle.tick(now);
                if matches!(status, ProcessStatus::Running) {
                    emit_status(&app, name, status, proc.exit_code, proc.term_id);
                }
            }
        }
    });
}

/// The process's terminal WHEN IT COUNTS AS LIVE — the idempotency
/// rule ("start on live = no-op with the live id"), shared with the start
/// path.
fn live_term(proc: &WsProc) -> Option<TermId> {
    match proc.lifecycle.status() {
        ProcessStatus::Starting | ProcessStatus::Running => proc.term_id,
        _ => None,
    }
}

/// Rebuild the runtime map from the store after a mutation, mirroring the
/// project reload: ADDED → fresh; CHANGED spawn inputs of a LIVE command →
/// restarted IN PLACE under the new definition (native-store rule: no trust
/// gate, so no hold-back; a bad new working_dir leaves it running on the old
/// def with its true status); REMOVED → stopped and dropped. Order follows
/// the store.
fn rebuild(inner: &mut WsInner, registry: &Registry, app: &AppHandle, ws: Arc<Mutex<WsInner>>) {
    let defs = inner
        .store
        .clone()
        .map(|s| s.commands())
        .unwrap_or_default();
    let settings = inner.settings.snapshot();
    let mut next: IndexMap<String, WsProc> = IndexMap::new();
    for (name, def) in defs {
        match inner.procs.shift_remove(&name) {
            Some(mut p) => {
                let was_live = p.term_id.is_some()
                    && matches!(
                        p.lifecycle.status(),
                        ProcessStatus::Starting | ProcessStatus::Running
                    );
                let changed = !p.def.runtime_eq(&def);
                p.def = def;
                if was_live && changed {
                    // Execute the new definition IN PLACE (native-store rule:
                    // no trust gate here, so no hold-back). Validate BEFORE
                    // touching the running process: a bad new working_dir must
                    // leave the old command running with its true status
                    // (the review).
                    if build_workspace_spec(&p.def, &settings).is_ok() && p.channel.is_some() {
                        p.lifecycle.on(ProcessEvent::Stop, now_ms());
                        p.generation += 1;
                        let _ = spawn_proc(registry, app, ws.clone(), &name, &settings, &mut p);
                    } else {
                        log::warn!(
                            "workspace `{name}` keeps running on its old definition — the new one does not spawn"
                        );
                    }
                }
                next.insert(name, p);
            }
            None => {
                next.insert(name, fresh(def));
            }
        }
    }
    // Anything left in `inner.procs` is no longer in the store → stop + drop.
    for (name, mut p) in inner.procs.drain(..) {
        if p.term_id.is_some() {
            let status = p.lifecycle.on(ProcessEvent::Stop, now_ms());
            p.generation += 1;
            if let Some(id) = p.term_id.take() {
                registry.close(id);
            }
            emit_status(app, &name, status, p.exit_code, None);
        }
    }
    inner.procs = next;
}

fn dtos(inner: &WsInner) -> Vec<WorkspaceCommandDto> {
    inner
        .procs
        .iter()
        .map(|(name, p)| WorkspaceCommandDto::from_proc(name, p))
        .collect()
}

// ---- frontend commands -------------------------------------------------------

/// The COMMANDS rows: stored definition + runtime status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCommandDto {
    pub name: String,
    pub command: String,
    pub status: &'static str,
    pub auto_start: bool,
    pub auto_restart: bool,
    pub restart_when_changed: Vec<String>,
    pub term_id: Option<u32>,
    pub exit_code: Option<i32>,
    pub working_dir: Option<String>,
    pub env: IndexMap<String, String>,
}

impl WorkspaceCommandDto {
    fn from_proc(name: &str, p: &WsProc) -> Self {
        Self {
            name: name.to_owned(),
            command: p.def.command.clone(),
            status: p.lifecycle.status().as_str(),
            auto_start: p.def.auto_start,
            auto_restart: p.def.auto_restart,
            restart_when_changed: p.def.restart_when_changed.clone(),
            term_id: p.term_id,
            exit_code: p.exit_code,
            working_dir: p.def.working_dir.as_ref().map(|d| d.display().to_string()),
            env: p.def.env.clone(),
        }
    }
}

/// `List the COMMANDS rows` (name → runtime status, stored order).
#[tauri::command]
pub fn list_workspace_commands(
    workspace: State<'_, Workspace>,
) -> Result<Vec<WorkspaceCommandDto>, String> {
    let inner = workspace.lock()?;
    Ok(dtos(&inner))
}

/// `+ Add command` (`original_name = None`) / `Edit command…`
/// (`Some`). Validates (working_dir absolute/empty, else refused), writes
/// workspace.json atomically, then rebuilds the runtimes. Returns the rows.
#[tauri::command]
pub fn save_workspace_command(
    app: AppHandle,
    registry: State<'_, Registry>,
    workspace: State<'_, Workspace>,
    original_name: Option<String>,
    def: ProcessDefDto,
) -> Result<Vec<WorkspaceCommandDto>, String> {
    let store = {
        let inner = workspace.lock()?;
        inner
            .store
            .clone()
            .ok_or_else(|| "workspace store not initialized".to_owned())?
    };
    let (name, ws_def) = def.into_def();
    store.save_command(original_name.as_deref(), &name, ws_def)?;
    let mut inner = workspace.lock()?;
    rebuild(&mut inner, &registry, &app, workspace.arc());
    Ok(dtos(&inner))
}

/// `Delete command "<name>"`. The file is written FIRST (a refused delete must
/// not have killed anything), then the running process, if any, is stopped.
#[tauri::command]
pub fn delete_workspace_command(
    app: AppHandle,
    registry: State<'_, Registry>,
    workspace: State<'_, Workspace>,
    name: String,
) -> Result<Vec<WorkspaceCommandDto>, String> {
    let store = {
        let inner = workspace.lock()?;
        inner
            .store
            .clone()
            .ok_or_else(|| "workspace store not initialized".to_owned())?
    };
    store.delete_command(&name)?;
    let mut inner = workspace.lock()?;
    rebuild(&mut inner, &registry, &app, workspace.arc());
    Ok(dtos(&inner))
}

/// Start one command. Live → idempotent no-op with its id. `frames` is the
/// per-terminal channel the registry needs.
#[tauri::command]
pub fn start_workspace_command(
    app: AppHandle,
    registry: State<'_, Registry>,
    workspace: State<'_, Workspace>,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    start_ws(&app, &registry, &workspace, name, frames)
}

fn start_ws(
    app: &AppHandle,
    registry: &Registry,
    workspace: &Workspace,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    let mut inner = workspace.lock()?;
    let live = inner.procs.get(&name).and_then(live_term);
    if let Some(id) = live {
        return Ok(id); // already live: idempotent
    }
    let settings = inner.settings.snapshot();
    let proc = inner
        .procs
        .get_mut(&name)
        .ok_or_else(|| format!("no command named `{name}`"))?;
    proc.channel = Some(frames);
    if let Some(id) = proc.term_id.take() {
        registry.close(id);
    }
    match build_workspace_spec(&proc.def, &settings) {
        Ok(_) => {
            proc.term_id = None; // force a fresh id on this start
            match spawn_proc(registry, app, workspace.arc(), &name, &settings, proc) {
                Ok(id) => {
                    proc.backoff.reset();
                    Ok(id)
                }
                Err(e) => {
                    proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
                    emit_status(app, &name, ProcessStatus::Failed, None, proc.term_id);
                    Err(e)
                }
            }
        }
        Err(e) => {
            proc.generation += 1;
            proc.lifecycle.on(ProcessEvent::SpawnFailed, now_ms());
            emit_status(app, &name, ProcessStatus::Failed, None, proc.term_id);
            Err(e)
        }
    }
}

/// Stop one command. Idempotent: stopping a non-running one is a no-op
/// success (measured, 0.7.1); a stopped command never auto-restarts.
#[tauri::command]
pub fn stop_workspace_command(
    app: AppHandle,
    registry: State<'_, Registry>,
    workspace: State<'_, Workspace>,
    name: String,
) -> Result<(), String> {
    stop_ws(&registry, &app, &workspace, &name)
}

fn stop_ws(registry: &Registry, app: &AppHandle, workspace: &Workspace, name: &str) -> Result<(), String> {
    let mut inner = workspace.lock()?;
    let proc = inner
        .procs
        .get_mut(name)
        .ok_or_else(|| format!("no command named `{name}`"))?;
    let status = proc.lifecycle.on(ProcessEvent::Stop, now_ms());
    proc.generation += 1; // cancels any scheduled auto-restart
    if let Some(id) = proc.term_id.take() {
        registry.close(id);
    }
    let exit_code = proc.exit_code;
    emit_status(app, name, status, exit_code, None);
    Ok(())
}

/// Restart one command: stop if live, then start (a stopped one just starts —
/// matches the visible actions). Needs a frames Channel because a never-started
/// one has none yet.
#[tauri::command]
pub fn restart_workspace_command(
    app: AppHandle,
    registry: State<'_, Registry>,
    workspace: State<'_, Workspace>,
    name: String,
    frames: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    stop_ws(&registry, &app, &workspace, &name)?;
    start_ws(&app, &registry, &workspace, name, frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::settings::ShellProfile;

    fn settings_with(exec: &str) -> Settings {
        let mut s = Settings::default();
        s.default_exec_profile = exec.to_owned();
        s.shell_profiles = vec![ShellProfile {
            name: exec.to_owned(),
            command: "sh".to_owned(),
            enabled: true,
        }];
        s
    }

    /// The workspace working-dir rule at SPAWN time (independent of the store
    /// validation): absolute → cwd set; None (empty = home) → cwd None, like a
    /// plain shell; RELATIVE → refused (re-validated, so a hand-edited file
    /// can't slip one through).
    #[test]
    fn spawn_spec_abs_or_home_and_refuses_relative() {
        let s = settings_with("sh");
        // Platform-correct absolute fixture: "/opt/ws" is NOT absolute on
        // Windows (no drive), and the validation rightly refuses it there —
        // the Linux run masked this (fixed 2026-08-31).
        #[cfg(windows)]
        let abs_dir = "C:\\ws";
        #[cfg(not(windows))]
        let abs_dir = "/opt/ws";
        let abs = ProcessDef {
            command: "echo hi".to_owned(),
            working_dir: Some(abs_dir.into()),
            ..Default::default()
        };
        let spec = build_workspace_spec(&abs, &s).unwrap();
        assert_eq!(spec.cwd.as_deref(), Some(std::path::Path::new(abs_dir)));

        let home = ProcessDef {
            command: "echo hi".to_owned(),
            working_dir: None,
            ..Default::default()
        };
        let spec = build_workspace_spec(&home, &s).unwrap();
        assert_eq!(spec.cwd, None, "empty working_dir = home, like a plain shell");

        let rel = ProcessDef {
            command: "echo hi".to_owned(),
            working_dir: Some("relative/dir".into()),
            ..Default::default()
        };
        assert!(build_workspace_spec(&rel, &s).is_err());
    }
}
