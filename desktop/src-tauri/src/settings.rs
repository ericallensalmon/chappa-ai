//! Settings glue — the thin Tauri layer over
//! `project_model::settings`, mirroring `projects.rs`.
//!
//! All pure logic (defaults, per-field fill, range clamping, platform shell
//! profiles, atomic persistence) lives and is tested in `project-model`; this
//! module only owns the app-config path, the managed state, the two commands,
//! and the LIVE BROADCAST that makes a settings change reach terminals that
//! are already open.
//!
//! Wire contract — the struct serializes camelCase and is the SAME shape on
//! disk and over IPC:
//!
//! ```json
//! {"copyOnSelect": false, "scrollWheelSpeed": 3, "fontSize": 14,
//!  "fontFamily": "Geist Mono", "lineHeight": 1.2,
//!  "syntheticPromptMarks": false,
//!  "shellProfiles": [{"name": "…", "command": "…", "enabled": true}],
//!  "defaultExecProfile": "cmd",
//!  "ctrlVPastes": true, "ctrlCCopyOnly": true}
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use project_model::settings::{Settings, SettingsStore};
use tauri::State;

use crate::registry::Registry;

/// Managed state: the store, initialized once from the `.setup` closure.
/// `Option` because `Default` must work before the app path is known (same
/// shape as `Projects`).
#[derive(Clone, Default)]
pub struct SettingsState {
    inner: Arc<Mutex<Option<SettingsStore>>>,
}

impl SettingsState {
    /// Called once from setup: point the store at `<app-config>/settings.json`
    /// and load it (missing/corrupt → defaults, never an error).
    pub fn init(&self, config_dir: PathBuf) {
        let store = SettingsStore::new(config_dir.join("settings.json"));
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(store);
        }
    }

    /// The current settings, or the defaults when the store has not
    /// been initialized (or the mutex is poisoned). Deliberately infallible:
    /// the spawn paths read this on every terminal create and must never fail
    /// a spawn over a settings problem.
    pub fn snapshot(&self) -> Settings {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|store| store.get().clone()))
            .unwrap_or_default()
    }
}

/// Current settings (the frontend loads these once at boot).
#[tauri::command]
pub fn get_settings(settings: State<'_, SettingsState>) -> Result<Settings, String> {
    Ok(settings.snapshot())
}

/// Persist the whole struct (no Save button: every change is a full-struct
/// set) and return the CLAMPED result, so an out-of-range value snaps back
/// visibly in the UI.
///
/// Order matters: clamp+persist FIRST, then broadcast the live per-actor
/// controls, so what open terminals get is exactly what landed on disk.
/// The payload key on the wire is `settings` (Tauri derives command argument
/// names from the parameter list), so the managed state must be called
/// something else here.
#[tauri::command]
pub fn set_settings(
    registry: State<'_, Registry>,
    state: State<'_, SettingsState>,
    settings: Settings,
) -> Result<Settings, String> {
    let (old, stored) = {
        let mut guard = state
            .inner
            .lock()
            .map_err(|_| "settings state poisoned".to_owned())?;
        let store = guard
            .as_mut()
            .ok_or_else(|| "settings store not initialized".to_owned())?;
        let old = store.get().clone();
        let stored = store.set(settings)?;
        (old, stored)
    };
    // "synthetic_prompt_marks → a live per-terminal control to every open
    // actor plus the ActorConfig at spawn (a toggle must not require
    // restarting terminals)"; the wheel speed rides the same path ("the
    // setting reaches ACTORS: `ActorConfig.wheel_speed` at spawn + a live
    // control on every open actor"). Done OUTSIDE the settings lock, and
    // ONLY when one of the two actor-facing fields actually changed — a
    // font-size click must not message every open terminal.
    if old.synthetic_prompt_marks != stored.synthetic_prompt_marks
        || old.scroll_wheel_speed != stored.scroll_wheel_speed
    {
        for handle in registry.handles() {
            handle.set_synthetic_marks(stored.synthetic_prompt_marks);
            handle.set_wheel_speed(stored.scroll_wheel_speed);
        }
    }
    Ok(stored)
}
