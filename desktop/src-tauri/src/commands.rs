//! Frontend-facing IPC commands + DTOs. This layer is plumbing:
//! every command resolves a `TermId` to its `TermHandle` and forwards to
//! term-core. The serde derives here are the wire contract — they must decode
//! exactly the JSON shapes in `desktop/ui/src/ipc.ts`: keys are internally
//! tagged on `kind`, mods are the
//! term-core `Mods` bitmask as a plain number, selection ops are internally
//! tagged on `op`, and points are `{row, col}`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use project_model::settings::{split_command_line, Settings};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_notification::NotificationExt;
use term_core::actor::{Point, SearchNavDir, SelectionKind, SelectionOp};
use term_core::keys::{Key, KeyEvent, Mods, MouseEvent, MouseKind};
use term_core::pty::PtySpec;

use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, ChannelSink, CreatedEvent, RailRow, Registry,
    TermId,
};
use crate::settings::SettingsState;

// ---- DTOs ------------------------------------------------------------------

/// Wire form of `keys::Key`, internally tagged to match
/// `ui/src/ipc.ts::KeyDto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyDto {
    Char { ch: char },
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F { n: u8 },
}

impl KeyDto {
    fn to_key(self) -> Key {
        match self {
            KeyDto::Char { ch } => Key::Char(ch),
            KeyDto::Enter => Key::Enter,
            KeyDto::Tab => Key::Tab,
            KeyDto::Backspace => Key::Backspace,
            KeyDto::Escape => Key::Escape,
            KeyDto::Up => Key::Up,
            KeyDto::Down => Key::Down,
            KeyDto::Left => Key::Left,
            KeyDto::Right => Key::Right,
            KeyDto::Home => Key::Home,
            KeyDto::End => Key::End,
            KeyDto::PageUp => Key::PageUp,
            KeyDto::PageDown => Key::PageDown,
            KeyDto::Insert => Key::Insert,
            KeyDto::Delete => Key::Delete,
            KeyDto::F { n } => Key::F(n),
        }
    }
}

/// `{key, mods}` with `mods` the `Mods` bitmask (shift=1 alt=2 ctrl=4 super=8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct KeyEventDto {
    pub key: KeyDto,
    pub mods: u8,
}

/// Wire form of `keys::MouseKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKindDto {
    Press,
    Release,
    Drag,
    Move,
    WheelUp,
    WheelDown,
}

/// Wire form of `keys::MouseEvent` (0-based viewport cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct MouseEventDto {
    pub kind: MouseKindDto,
    pub button: u8,
    pub col: u16,
    pub row: u16,
    pub mods: u8,
}

/// Wire form of `pty::PtySpec`. `env` is a flat map; the webview can't read
/// `$SHELL`, so an empty `command` means "platform default shell".
#[derive(Debug, Clone, Deserialize)]
pub struct PtySpecDto {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// Name of a settings shell profile. Consulted ONLY when
    /// `command` is empty; an unknown or DISABLED name falls back to
    /// `default_shell()` (today's behavior), never to an error.
    pub profile: Option<String>,
}

/// Wire form of `actor::SelectionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionKindDto {
    Simple,
    Block,
    Lines,
    Semantic,
}

/// Wire form of `actor::SelectionOp`, internally tagged on `op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SelectionOpDto {
    Start {
        point: PointDto,
        kind: SelectionKindDto,
    },
    Update {
        point: PointDto,
        kind: SelectionKindDto,
    },
    Clear,
}

/// Viewport point, row 0 = top of screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct PointDto {
    pub row: i32,
    pub col: u16,
}

/// Wire form of the actor's `FrameStats` (debug HUD). Keys are camelCase to
/// match `ui/src/ipc.ts::DebugStatsDto`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugStatsDto {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub damage_rows_last: u32,
    pub coalesced_ticks: u64,
    pub outstanding: bool,
}

// ---- conversions -----------------------------------------------------------

/// Rebuild the `Mods` bitmask (private inner field, so combine constants).
fn mods_from_bits(bits: u8) -> Mods {
    let mut m = Mods::EMPTY;
    if bits & 1 != 0 {
        m = m | Mods::SHIFT;
    }
    if bits & 2 != 0 {
        m = m | Mods::ALT;
    }
    if bits & 4 != 0 {
        m = m | Mods::CTRL;
    }
    if bits & 8 != 0 {
        m = m | Mods::SUPER;
    }
    m
}

fn to_mouse(dto: MouseEventDto) -> MouseEvent {
    MouseEvent {
        kind: match dto.kind {
            MouseKindDto::Press => MouseKind::Press,
            MouseKindDto::Release => MouseKind::Release,
            MouseKindDto::Drag => MouseKind::Drag,
            MouseKindDto::Move => MouseKind::Move,
            MouseKindDto::WheelUp => MouseKind::WheelUp,
            MouseKindDto::WheelDown => MouseKind::WheelDown,
        },
        button: dto.button,
        col: dto.col,
        row: dto.row,
        mods: mods_from_bits(dto.mods),
    }
}

/// The user's shell. Windows: pwsh (PowerShell 7) when it's on PATH, else
/// powershell.exe — pwsh is an optional install and CreateProcessW does not
/// search PATH the way a shell would, so probe the PATH entries ourselves.
/// Elsewhere: `$SHELL` falling back to /bin/sh. The webview cannot read
/// `$SHELL` (or PATH), which is why this policy lives Rust-side. Shared with
/// the control surface (`spawn_terminal` with no command).
pub(crate) fn default_shell() -> String {
    if cfg!(windows) {
        let has_pwsh = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file())
        });
        if has_pwsh {
            "pwsh.exe".into()
        } else {
            "powershell.exe".into()
        }
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// Resolve the spawn spec. Adds ONE branch: an empty `command` plus a
/// `profile` naming an ENABLED settings profile spawns that profile's command
/// line (split on whitespace into program + args — see
/// `project_model::settings::split_command_line` for the naive-split caveat).
/// Empty command with no (or an unknown/disabled) profile keeps today's
/// `default_shell()` behavior unchanged.
fn to_pty_spec(dto: PtySpecDto, settings: &Settings) -> PtySpec {
    let (command, mut args) = if dto.command.is_empty() {
        match dto
            .profile
            .as_deref()
            .and_then(|name| settings.enabled_profile(name))
            // A profile whose command line splits to an EMPTY program (a
            // hand-edited file) must not spawn an empty command — degrade to
            // the default shell like an unknown profile does.
            .map(|profile| split_command_line(&profile.command))
            .filter(|(program, _)| !program.is_empty())
        {
            Some(split) => split,
            None => (default_shell(), Vec::new()),
        }
    } else {
        (dto.command.clone(), Vec::new())
    };
    // Caller-supplied args append to whatever the profile's line contributed.
    args.extend(dto.args.unwrap_or_default());
    PtySpec {
        command,
        args,
        cwd: dto.cwd.map(PathBuf::from),
        env: dto.env.unwrap_or_default().into_iter().collect(),
        cols: dto.cols.unwrap_or(80),
        rows: dto.rows.unwrap_or(24),
    }
}

fn to_point(p: PointDto) -> Point {
    Point::new((p.row).into(), (p.col as usize).into())
}

fn to_selection_kind(kind: SelectionKindDto) -> SelectionKind {
    match kind {
        SelectionKindDto::Simple => SelectionKind::Simple,
        SelectionKindDto::Block => SelectionKind::Block,
        SelectionKindDto::Lines => SelectionKind::Lines,
        SelectionKindDto::Semantic => SelectionKind::Semantic,
    }
}

fn to_selection_op(op: SelectionOpDto) -> SelectionOp {
    match op {
        SelectionOpDto::Start { point, kind } => SelectionOp::Start {
            point: to_point(point),
            kind: to_selection_kind(kind),
        },
        SelectionOpDto::Update { point, kind } => SelectionOp::Update {
            point: to_point(point),
            kind: to_selection_kind(kind),
        },
        SelectionOpDto::Clear => SelectionOp::Clear,
    }
}

/// Resolve a `TermId` to its handle, or a clean error string for the promise.
fn handle_for(registry: &Registry, id: TermId) -> Result<term_core::actor::TermHandle, String> {
    registry
        .handle(id)
        .ok_or_else(|| format!("no such terminal: {id}"))
}

// ---- commands --------------------------------------------------------------

/// Spawn a terminal and wire up its event pump. Binary frames arrive on
/// `frames`; JSON events go out under the `term://*` global event names.
#[tauri::command]
pub fn create_terminal(
    app: AppHandle,
    registry: State<'_, Registry>,
    settings: State<'_, SettingsState>,
    spec: PtySpecDto,
    scrollback: Option<usize>,
    frames: Channel<InvokeResponseBody>,
) -> Result<TermId, String> {
    // Both live-settable actor controls are also seeded at SPAWN, so
    // a terminal opened after a settings change starts out correct.
    let settings = settings.snapshot();
    let name = if !spec.command.is_empty() {
        spec.command.clone()
    } else {
        // A profile spawn labels the rail row with the profile name; a plain
        // default-shell spawn keeps the historical "shell".
        spec.profile.clone().unwrap_or_else(|| "shell".to_owned())
    };
    let cfg = actor_config(to_pty_spec(spec, &settings), scrollback, Some(&settings));
    let id = registry.create_terminal(
        cfg,
        Arc::new(ChannelSink::new(frames, app.clone())),
        Some(app.clone()),
        name.clone(),
    )?;
    // Every successful spawn broadcasts `term://created`. The
    // frontend KNOWS this id (it initiated the spawn) and dedupes by id, so
    // this only matters for backend-created terminals — but the broadcast is
    // unconditional-by-design here because "which webview spawned it" is not
    // something callers should have to special-case.
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
    Ok(id)
}

#[tauri::command]
pub fn write_key(registry: State<'_, Registry>, id: TermId, ev: KeyEventDto) -> Result<(), String> {
    handle_for(&registry, id)?.write_key(KeyEvent {
        key: ev.key.to_key(),
        mods: mods_from_bits(ev.mods),
    });
    Ok(())
}

#[tauri::command]
pub fn paste(registry: State<'_, Registry>, id: TermId, text: String) -> Result<(), String> {
    handle_for(&registry, id)?.paste(&text);
    Ok(())
}

#[tauri::command]
pub fn mouse(registry: State<'_, Registry>, id: TermId, ev: MouseEventDto) -> Result<(), String> {
    handle_for(&registry, id)?.mouse(to_mouse(ev));
    Ok(())
}

#[tauri::command]
pub fn resize(
    registry: State<'_, Registry>,
    id: TermId,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let handle = handle_for(&registry, id)?;
    handle.resize(cols, rows);
    registry.set_size(id, cols, rows);
    Ok(())
}

#[tauri::command]
pub fn scroll(registry: State<'_, Registry>, id: TermId, delta: i32) -> Result<(), String> {
    handle_for(&registry, id)?.scroll(delta);
    Ok(())
}

#[tauri::command]
pub fn set_display_offset(
    registry: State<'_, Registry>,
    id: TermId,
    offset: usize,
) -> Result<(), String> {
    handle_for(&registry, id)?.set_display_offset(offset);
    Ok(())
}

#[tauri::command]
pub fn ack(registry: State<'_, Registry>, id: TermId, seq: u32) -> Result<(), String> {
    handle_for(&registry, id)?.ack(seq);
    Ok(())
}

#[tauri::command]
pub fn request_full(registry: State<'_, Registry>, id: TermId) -> Result<(), String> {
    handle_for(&registry, id)?.request_full();
    Ok(())
}

#[tauri::command]
pub fn selection(
    registry: State<'_, Registry>,
    id: TermId,
    op: SelectionOpDto,
) -> Result<(), String> {
    handle_for(&registry, id)?.selection(to_selection_op(op));
    Ok(())
}

/// Serialize the current selection and mirror it onto the OS clipboard. The
/// text is already rtrimmed per line by the actor (trailing cell padding).
///
/// `skip_whitespace_only` (default false) is the copy-on-select
/// guard: "SKIP whitespace-only selections (accidental micro-drags must not
/// replace the clipboard)". When it is on and the selection trims to nothing,
/// this returns `Ok(None)` WITHOUT touching the clipboard.
#[tauri::command]
pub fn copy_selection(
    app: AppHandle,
    registry: State<'_, Registry>,
    id: TermId,
    skip_whitespace_only: Option<bool>,
) -> Result<Option<String>, String> {
    let handle = handle_for(&registry, id)?;
    let text = handle.copy_selection();
    if skip_whitespace_only.unwrap_or(false)
        && text.as_deref().map_or(true, |t| t.trim().is_empty())
    {
        return Ok(None);
    }
    if let Some(text) = &text {
        let _ = app.clipboard().write_text(text.clone());
    }
    Ok(text)
}

#[tauri::command]
pub fn search(
    registry: State<'_, Registry>,
    id: TermId,
    regex: Option<String>,
) -> Result<(), String> {
    handle_for(&registry, id)?.search(regex);
    Ok(())
}

/// Wire form of `actor::SearchNavDir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchNavDirDto {
    Next,
    Prev,
}

/// Move the viewport to the next/previous regex match. Wraps
/// around; no-op when no matches.
#[tauri::command]
pub fn search_nav(
    registry: State<'_, Registry>,
    id: TermId,
    dir: SearchNavDirDto,
) -> Result<(), String> {
    let dir = match dir {
        SearchNavDirDto::Next => SearchNavDir::Next,
        SearchNavDirDto::Prev => SearchNavDir::Prev,
    };
    handle_for(&registry, id)?.search_nav(dir);
    Ok(())
}

/// Actor-side frame statistics for the debug HUD. Async because
/// the round-trip into the actor thread can block for as long as one
/// byte-drain pass: a sync command runs on the MAIN thread, where that wait
/// also stalls every queued `ack` — the HUD polling froze the frame stream
/// under flood. Errors propagate as a rejected promise the HUD swallows.
#[tauri::command]
pub async fn debug_stats(
    registry: State<'_, Registry>,
    id: TermId,
) -> Result<DebugStatsDto, String> {
    let handle = handle_for(&registry, id)?;
    let stats = tauri::async_runtime::spawn_blocking(move || handle.stats())
        .await
        .map_err(|e| e.to_string())?;
    Ok(DebugStatsDto {
        frames_sent: stats.frames_sent,
        bytes_sent: stats.bytes_sent,
        damage_rows_last: stats.damage_rows_last,
        coalesced_ticks: stats.coalesced_ticks,
        outstanding: stats.outstanding,
    })
}

/// Close a terminal. Answers the container-side verification
/// (`gone | still-present | container-down | unresolved`) for a docker-exec
/// agent, `null` for anything else — the UI toasts a non-`gone` answer.
///
/// Async + `spawn_blocking` (the `spawn_agent` / `debug_stats` pattern): a
/// docker-exec close runs the TERM → poll → KILL → poll sequence inline —
/// several docker calls and up to TERM_GRACE + KILL_GRACE of waiting — and a
/// sync command would run all of that on the webview's MAIN thread, freezing
/// the window for the duration (the `pick_directory` lesson).
#[tauri::command]
pub async fn close_terminal(registry: State<'_, Registry>, id: TermId) -> Result<Option<String>, String> {
    let registry = registry.inner().clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || registry.close(id))
        .await
        .map_err(|e| e.to_string())?;
    match outcome {
        Some(outcome) => Ok(outcome.verification.map(|v| v.as_str().to_owned())),
        None => Err(format!("no such terminal: {id}")),
    }
}

/// Fire one OS notification. Deliberately dumb: the UI's pure
/// decision function decides WHEN — "pump keeps emitting raw events —
/// filtering happens at the decision function, so levels change with zero Rust
/// churn" — and this only fires. It carries no level, no term id and no
/// suppression logic of its own.
#[tauri::command]
pub fn os_notify(app: AppHandle, title: String, body: String) -> Result<(), String> {
    app.notification()
        .builder()
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| e.to_string())
}

/// Snapshot for the process rail: every live terminal with its status-vocabulary
/// status and exit code, PLUS the kind/binding facts the
/// frontend reconciles/adopts from. The rail hydrates from this so an entry is
/// never stuck at "starting" after the pump's status event raced the create.
#[tauri::command]
pub fn list_terminals(registry: State<'_, Registry>) -> Vec<RailRow> {
    registry.list_rows()
}

/// Swap a webview's frames channel in as the sink for an EXISTING
/// terminal (a backend-spawned terminal the frontend adopted as a rail row).
/// The actor's `request_full` fires so the first frame is a FULL, then
/// input/resize/scroll work exactly as for a frontend-spawned panel. Returns
/// the terminal's row facts (name, kind, geometry, project/agent binding).
/// Attaching to a missing id is a clear error; re-attach is idempotent.
#[tauri::command]
pub fn attach_terminal(
    app: AppHandle,
    registry: State<'_, Registry>,
    id: TermId,
    frames: Channel<InvokeResponseBody>,
) -> Result<RailRow, String> {
    let sink: Arc<dyn crate::registry::EventSink> =
        Arc::new(ChannelSink::new(frames, app.clone()));
    registry.attach_terminal(id, sink)
}

/// Convenience spawn for a fresh shell panel: the platform default shell
/// (pwsh/powershell on Windows, $SHELL elsewhere — see `default_shell`),
/// optionally rooted at `cwd`. `command: ""` makes create_terminal resolve the
/// same policy, so this is just a nicer entry point for Ctrl+Shift+T.
///
/// `profile` picks one of the settings shell profiles instead — the
/// rail's new-terminal menu passes the name of an enabled profile.
#[tauri::command]
pub fn spawn_shell(
    app: AppHandle,
    registry: State<'_, Registry>,
    settings: State<'_, SettingsState>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    profile: Option<String>,
    frames: Channel<InvokeResponseBody>,
) -> Result<TermId, String> {
    create_terminal(
        app,
        registry,
        settings,
        PtySpecDto {
            command: String::new(),
            args: None,
            cwd,
            env: None,
            cols,
            rows,
            profile,
        },
        None,
        frames,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::settings::ShellProfile;

    fn dto(command: &str, profile: Option<&str>) -> PtySpecDto {
        PtySpecDto {
            command: command.to_owned(),
            args: None,
            cwd: None,
            env: None,
            cols: None,
            rows: None,
            profile: profile.map(|p| p.to_owned()),
        }
    }

    fn settings_with(profiles: Vec<ShellProfile>) -> Settings {
        let mut s = Settings::default();
        s.shell_profiles = profiles;
        s
    }

    #[test]
    fn empty_command_with_an_enabled_profile_spawns_that_profile() {
        let settings = settings_with(vec![ShellProfile {
            name: "Windows PowerShell".into(),
            command: "powershell.exe -NoLogo".into(),
            enabled: true,
        }]);
        let spec = to_pty_spec(dto("", Some("Windows PowerShell")), &settings);
        assert_eq!(spec.command, "powershell.exe");
        assert_eq!(spec.args, vec!["-NoLogo".to_owned()]);
    }

    #[test]
    fn disabled_or_unknown_profile_falls_back_to_the_default_shell() {
        let settings = settings_with(vec![ShellProfile {
            name: "Git Bash".into(),
            command: "bash.exe".into(),
            enabled: false,
        }]);
        // A disabled profile must never resolve...
        let spec = to_pty_spec(dto("", Some("Git Bash")), &settings);
        assert_eq!(spec.command, default_shell());
        assert!(spec.args.is_empty());
        // ...nor an unknown name, and neither is an error.
        let spec = to_pty_spec(dto("", Some("nope")), &settings);
        assert_eq!(spec.command, default_shell());
    }

    #[test]
    fn an_explicit_command_ignores_the_profile_entirely() {
        let settings = settings_with(vec![ShellProfile {
            name: "P".into(),
            command: "other.exe".into(),
            enabled: true,
        }]);
        let spec = to_pty_spec(dto("htop", Some("P")), &settings);
        assert_eq!(spec.command, "htop");
        assert!(spec.args.is_empty());
    }

    #[test]
    fn no_profile_keeps_todays_default_shell_behavior_unchanged() {
        let spec = to_pty_spec(dto("", None), &Settings::default());
        assert_eq!(spec.command, default_shell());
        assert_eq!(spec.cols, 80);
        assert_eq!(spec.rows, 24);
    }
}
