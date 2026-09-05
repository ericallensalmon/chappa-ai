//! App settings (`settings.json`).
//!
//! Mirrors [`crate::projects::ProjectStore`] exactly: the config path is
//! INJECTED (tests use a tempdir, the app supplies its real app-config dir),
//! a missing or unparseable file is DEFAULTS and never an error, and every
//! write is atomic (tmp + rename).
//!
//! The struct is the wire contract for BOTH sides — same shape on disk and
//! over IPC — so it serializes camelCase:
//!
//! ```json
//! {
//!   "copyOnSelect": false,
//!   "scrollWheelSpeed": 3,
//!   "fontSize": 14,
//!   "fontFamily": "Geist Mono",
//!   "lineHeight": 1.2,
//!   "syntheticPromptMarks": false,
//!   "shellProfiles": [{"name": "Windows PowerShell",
//!                      "command": "powershell.exe -NoLogo",
//!                      "enabled": true}],
//!   "defaultExecProfile": "cmd",
//!   "ctrlVPastes": true,
//!   "ctrlCCopyOnly": true
//! }
//! ```
//!
//! Ranges/defaults are documented on each field.
//! Out-of-range values are CLAMPED to the nearest bound by [`Settings::normalize`]
//! rather than rejected ("out-of-range values → defaults per field, never an
//! error").

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One shell profile: a display name, a full command LINE, and the
/// enable toggle the new-terminal menu filters on.
///
/// Field-level `default` on purpose: without it, ONE half-written entry in a
/// hand-edited file fails the whole `Settings` parse and resets EVERY other
/// setting to defaults. A lenient entry parses (missing `enabled` = on) and
/// [`Settings::normalize`] prunes it if name/command came up empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShellProfile {
    pub name: String,
    pub command: String,
    pub enabled: bool,
}

impl Default for ShellProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            enabled: true,
        }
    }
}

/// `copy_on_select` — "default false".
fn default_copy_on_select() -> bool {
    false
}

/// `ctrl_v_pastes` — "Ctrl+V (no alt/shift/meta) is an app shortcut
/// that pastes the clipboard through the existing paste path". Default TRUE,
/// and the TRUE serde default is DELIBERATE, not silent-change avoidance:
/// the reflexive ^V must paste ("I do also want ctrl+v to work
/// in input"), so an existing settings.json that never mentions the field
/// must gain the new behavior on upgrade, not keep the 0x16 passthrough
/// (vim's literal-next, etc.). OFF restores the passthrough.
///
/// Consequence (documented, deliberate): while on, ^V no longer reaches TUIs.
fn default_ctrl_v_pastes() -> bool {
    true
}

/// `ctrl_c_copy_only` — "Ctrl+C is ALWAYS an app shortcut — with a
/// selection it copies (the existing copy path); without one it is a NO-OP".
/// While TRUE, 0x03 NEVER reaches the pty: there is no keyboard interrupt,
/// and stopping a process is a deliberate act on its rail row. Cmd+C (mac)
/// keeps today's behavior in both modes — this is about the Windows/Linux
/// reflex. Default TRUE for the same deliberate reason as `ctrl_v_pastes`:
/// a reflexive ^C must never kill a long agent run, so existing settings.json
/// files gain the behavior on upgrade.
///
/// Consequence (documented, deliberate): while on, ^C no longer reaches
/// TUIs/agent CLIs — INCLUDING their own cancel/exit gestures; Escape and
/// the rail stop are the remaining paths.
fn default_ctrl_c_copy_only() -> bool {
    true
}

/// `scroll_wheel_speed` — "1..=6, default 3".
pub const SCROLL_WHEEL_SPEED_RANGE: (u8, u8) = (1, 6);
fn default_scroll_wheel_speed() -> u8 {
    3
}

/// `font_size` — "px 10..=18, default 14 — real pixels, not percentages".
pub const FONT_SIZE_RANGE: (u8, u8) = (10, 18);
fn default_font_size() -> u8 {
    14
}

/// `font_family` — "bundled face name; default \"Geist Mono\" (fallback stack
/// is the frontend's job)". Only the two BUNDLED faces are admissible;
/// system-font enumeration is deliberately ruled out.
pub const FONT_FAMILIES: [&str; 2] = ["Geist Mono", "JetBrains Mono"];
fn default_font_family() -> String {
    FONT_FAMILIES[0].to_owned()
}

/// `line_height` — "1.0..=1.8 step 0.1, default 1.2 (terminal settings;
/// cellH = font_size × this)". The step is a UI affordance, not a
/// validation rule: arbitrary in-range floats round-trip untouched.
pub const LINE_HEIGHT_RANGE: (f32, f32) = (1.0, 1.8);
fn default_line_height() -> f32 {
    1.2
}

/// `synthetic_prompt_marks` defaults to FALSE, permanently: the heuristic
/// guesses (an Enter that cancels a dialog still plants a mark), so it is
/// strictly opt-in. Never flip this.
fn default_synthetic_prompt_marks() -> bool {
    false
}

/// `default_exec_profile` — "name ref; default \"cmd\" on windows, \"sh\" on
/// unix". These two builtin names mean "the execution profile,
/// unchanged"; any other value names a shell profile.
pub const EXEC_PROFILE_CMD: &str = "cmd";
pub const EXEC_PROFILE_SH: &str = "sh";
fn default_exec_profile() -> String {
    if cfg!(windows) {
        EXEC_PROFILE_CMD.to_owned()
    } else {
        EXEC_PROFILE_SH.to_owned()
    }
}

/// `agent_ready_quiet_ms` — how long a spawned agent's pty must
/// stay silent AFTER its first output before a queued `prompt` is written.
/// "configurable `agent_ready_quiet_ms` in settings, range 250–5000", default
/// 750.
pub const AGENT_READY_QUIET_MS_RANGE: (u32, u32) = (250, 5000);
fn default_agent_ready_quiet_ms() -> u32 {
    750
}

/// `agent_ready_max_wait_ms` — the ready gate's fallback:
/// a TUI that repaints continuously never satisfies the quiet window, so
/// after this long of output-since-first-byte the prompt is delivered anyway
/// (`prompt_receipt.reason = ready-by-timeout`). Default 5000; clamped to
/// 1 s ..= 20 s (the MCP prompt wait cap).
pub const AGENT_READY_MAX_WAIT_MS_RANGE: (u32, u32) = (1_000, 20_000);
fn default_agent_ready_max_wait_ms() -> u32 {
    5_000
}

/// `agent_stale_after_s` — a docker-exec agent with no output
/// for this long, whose winsize poke also produces nothing, is `stale`.
/// Default 900 (15 min); clamped to 30 s ..= 1 day.
pub const AGENT_STALE_AFTER_S_RANGE: (u32, u32) = (30, 86_400);
fn default_agent_stale_after_s() -> u32 {
    900
}

/// `idle_threshold_ms` — how long a process's pty
/// byte stream must stay silent before an idle timer considers it idle. The
/// "trust idle only > 120 s" rule every session note hand-rolled, made
/// server-side. Default 120 000; clamped to 1 s ..= 1 h.
pub const IDLE_THRESHOLD_MS_RANGE: (u32, u32) = (1_000, 3_600_000);
fn default_idle_threshold_ms() -> u32 {
    120_000
}

/// `timer_confirm_ms` — a met idle condition is re-checked
/// after this long before the body is delivered; a byte in the window re-arms
/// the timer instead of firing it. Default 5 000; clamped to 0 ..= 10 min
/// (0 = fire on the first observation, the old behaviour).
pub const TIMER_CONFIRM_MS_RANGE: (u32, u32) = (0, 600_000);
fn default_timer_confirm_ms() -> u32 {
    5_000
}

/// `timer_delivery_timeout_ms` — how long a firing waits for the
/// delivery target's ready gate before recording `delivered: false,
/// reason: "not ready"`. Default 30 000; clamped to 1 s ..= 5 min.
pub const TIMER_DELIVERY_TIMEOUT_MS_RANGE: (u32, u32) = (1_000, 300_000);
fn default_timer_delivery_timeout_ms() -> u32 {
    30_000
}

/// `timer_dedupe_ms` — an identical body delivered to the
/// same process within this window is coalesced instead of re-delivered.
/// Default 5 000; clamped to 0 ..= 10 min (0 = never coalesce).
pub const TIMER_DEDUPE_MS_RANGE: (u32, u32) = (0, 600_000);
fn default_timer_dedupe_ms() -> u32 {
    5_000
}

/// `timer_retention_hours` — how long fired/cancelled timers
/// stay visible to `timer_list(include_fired: true)`. Default 24; clamped to
/// 1 h ..= 30 days.
pub const TIMER_RETENTION_HOURS_RANGE: (u32, u32) = (1, 720);
fn default_timer_retention_hours() -> u32 {
    24
}

/// The whole settings surface. `#[serde(default)]` at the container level is
/// what makes a PARTIAL file fill per field: serde takes each missing field
/// from `Settings::default()` instead of failing the parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub copy_on_select: bool,
    pub scroll_wheel_speed: u8,
    pub font_size: u8,
    pub font_family: String,
    pub line_height: f32,
    pub synthetic_prompt_marks: bool,
    pub shell_profiles: Vec<ShellProfile>,
    pub default_exec_profile: String,
    /// The ready gate's quiet window for queued agent prompts.
    pub agent_ready_quiet_ms: u32,
    /// The ready gate's output-since-first-byte fallback.
    pub agent_ready_max_wait_ms: u32,
    /// The bridge probe's stale threshold for docker-exec agents.
    pub agent_stale_after_s: u32,
    /// Byte-stream silence before a process counts as idle.
    pub idle_threshold_ms: u32,
    /// The fire-time re-validation window.
    pub timer_confirm_ms: u32,
    /// How long a firing waits on the ready gate.
    pub timer_delivery_timeout_ms: u32,
    /// Duplicate-body coalescing window.
    pub timer_dedupe_ms: u32,
    /// How long fired timers stay listable.
    pub timer_retention_hours: u32,
    /// Ctrl+V (no alt/shift/meta) pastes the clipboard instead of
    /// sending 0x16 to the terminal. FRONTEND-ONLY consumer — the terminal
    /// input layer reads it live (like `copy_on_select`'s UI half), so there
    /// is no actor/broadcast work. Default TRUE (deliberate: existing
    /// settings.json files gain the behavior — see `default_ctrl_v_pastes`).
    pub ctrl_v_pastes: bool,
    /// Ctrl+C is always an app shortcut (selection → copy, no
    /// selection → no-op); 0x03 never reaches the pty while TRUE. Same
    /// frontend-only shape and deliberate TRUE default as `ctrl_v_pastes`.
    pub ctrl_c_copy_only: bool,
    /// Sweep docker containers for marker-bearing container
    /// processes no live row owns (orphans) at app start and on each bridge
    /// probe tick. Default TRUE — orphans from a hard restart / mid-close
    /// docker hiccup are exactly the leak this reaps. The explicit
    /// `POST /agents/reap` control route is NOT gated by this.
    pub agent_reap_orphans: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            copy_on_select: default_copy_on_select(),
            scroll_wheel_speed: default_scroll_wheel_speed(),
            font_size: default_font_size(),
            font_family: default_font_family(),
            line_height: default_line_height(),
            synthetic_prompt_marks: default_synthetic_prompt_marks(),
            // Empty here on purpose: the PLATFORM defaults are filled by
            // `SettingsStore::new` on FIRST LOAD only (missing file / missing
            // key). An explicit `"shellProfiles": []` is a user's delete-all
            // and must never resurrect the defaults.
            shell_profiles: Vec::new(),
            default_exec_profile: default_exec_profile(),
            agent_ready_quiet_ms: default_agent_ready_quiet_ms(),
            agent_ready_max_wait_ms: default_agent_ready_max_wait_ms(),
            agent_stale_after_s: default_agent_stale_after_s(),
            idle_threshold_ms: default_idle_threshold_ms(),
            timer_confirm_ms: default_timer_confirm_ms(),
            timer_delivery_timeout_ms: default_timer_delivery_timeout_ms(),
            timer_dedupe_ms: default_timer_dedupe_ms(),
            timer_retention_hours: default_timer_retention_hours(),
            // Both TRUE on purpose — see the default fns for why an
            // existing settings.json must gain the behavior, not keep the
            // legacy passthrough.
            ctrl_v_pastes: default_ctrl_v_pastes(),
            ctrl_c_copy_only: default_ctrl_c_copy_only(),
            agent_reap_orphans: default_agent_reap_orphans(),
        }
    }
}

fn default_agent_reap_orphans() -> bool {
    true
}

impl Settings {
    /// Clamp everything into its documented range. Called after every parse and
    /// before every write, so no out-of-range value ever reaches an actor or
    /// the disk.
    pub fn normalize(&mut self) {
        self.scroll_wheel_speed = self
            .scroll_wheel_speed
            .clamp(SCROLL_WHEEL_SPEED_RANGE.0, SCROLL_WHEEL_SPEED_RANGE.1);
        self.font_size = self.font_size.clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1);
        // NaN/inf cannot be clamped meaningfully (and JSON cannot even carry
        // them) — fall back to the default rather than propagate a poison
        // value into `cellH = font_size × line_height`.
        self.line_height = if self.line_height.is_finite() {
            self.line_height
                .clamp(LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1)
        } else {
            default_line_height()
        };
        // "unknown font_family (anything other than the two bundled faces)
        // back to default" — the frontend builds the fallback stack, but the
        // HEAD of that stack must be a face we actually ship.
        if !FONT_FAMILIES.contains(&self.font_family.as_str()) {
            self.font_family = default_font_family();
        }
        // Prune entries the lenient `ShellProfile` parse let through with an
        // empty name or command — they can neither be listed nor spawned.
        // Deliberately NO refill here: an empty list can be a user's
        // delete-all (first-load refill lives in `SettingsStore::new`).
        self.shell_profiles
            .retain(|p| !p.name.trim().is_empty() && !p.command.trim().is_empty());
        self.agent_ready_quiet_ms = self
            .agent_ready_quiet_ms
            .clamp(AGENT_READY_QUIET_MS_RANGE.0, AGENT_READY_QUIET_MS_RANGE.1);
        self.agent_ready_max_wait_ms = self
            .agent_ready_max_wait_ms
            .clamp(AGENT_READY_MAX_WAIT_MS_RANGE.0, AGENT_READY_MAX_WAIT_MS_RANGE.1);
        self.agent_stale_after_s = self
            .agent_stale_after_s
            .clamp(AGENT_STALE_AFTER_S_RANGE.0, AGENT_STALE_AFTER_S_RANGE.1);
        self.idle_threshold_ms = self
            .idle_threshold_ms
            .clamp(IDLE_THRESHOLD_MS_RANGE.0, IDLE_THRESHOLD_MS_RANGE.1);
        self.timer_confirm_ms = self
            .timer_confirm_ms
            .clamp(TIMER_CONFIRM_MS_RANGE.0, TIMER_CONFIRM_MS_RANGE.1);
        self.timer_delivery_timeout_ms = self.timer_delivery_timeout_ms.clamp(
            TIMER_DELIVERY_TIMEOUT_MS_RANGE.0,
            TIMER_DELIVERY_TIMEOUT_MS_RANGE.1,
        );
        self.timer_dedupe_ms = self
            .timer_dedupe_ms
            .clamp(TIMER_DEDUPE_MS_RANGE.0, TIMER_DEDUPE_MS_RANGE.1);
        self.timer_retention_hours = self
            .timer_retention_hours
            .clamp(TIMER_RETENTION_HOURS_RANGE.0, TIMER_RETENTION_HOURS_RANGE.1);
    }

    /// The profile named `name`, only when it is ENABLED (the spawn paths
    /// must never resolve a profile the user switched off).
    pub fn enabled_profile(&self, name: &str) -> Option<&ShellProfile> {
        self.shell_profiles
            .iter()
            .find(|p| p.enabled && p.name == name)
    }

    /// The profile named `name` regardless of its enable toggle (settings UI).
    pub fn profile(&self, name: &str) -> Option<&ShellProfile> {
        self.shell_profiles.iter().find(|p| p.name == name)
    }
}

/// Split a profile's command LINE into program + args. Three regimes, in
/// order:
///
/// 1. The whole (trimmed) line names an existing FILE → it is the program,
///    no args. This is what makes the shipped Git Bash default
///    (`C:\Program Files\Git\bin\bash.exe` — spaces, no quotes) spawnable;
///    a whitespace split would have produced program `C:\Program`.
/// 2. The line starts with a double quote → the quoted token is the program,
///    the remainder splits on whitespace.
/// 3. Otherwise: naive whitespace split. Still no escape handling and no
///    quoting inside ARGUMENTS — the documented limitation.
pub fn split_command_line(line: &str) -> (String, Vec<String>) {
    let trimmed = line.trim();
    if !trimmed.is_empty() && Path::new(trimmed).is_file() {
        return (trimmed.to_owned(), Vec::new());
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            let program = rest[..end].to_owned();
            let args = rest[end + 1..]
                .split_whitespace()
                .map(|s| s.to_owned())
                .collect();
            return (program, args);
        }
    }
    let mut parts = trimmed.split_whitespace().map(|s| s.to_owned());
    let program = parts.next().unwrap_or_default();
    (program, parts.collect())
}

/// Standard Git-for-Windows install locations, probed in order.
const GIT_BASH_PATHS: [&str; 2] = [
    r"C:\Program Files\Git\bin\bash.exe",
    r"C:\Program Files (x86)\Git\bin\bash.exe",
];

/// First-load shell profiles. Windows ships exactly these three:
/// `Windows PowerShell` = `powershell.exe -NoLogo`, `Command Prompt` =
/// `C:\WINDOWS\system32\cmd.exe`, `Git Bash` resolved via the standard
/// install path and "marked disabled if missing". Unix gets a single `$SHELL`
/// entry.
pub fn platform_shell_profiles() -> Vec<ShellProfile> {
    if cfg!(windows) {
        let found = GIT_BASH_PATHS.iter().find(|p| Path::new(p).is_file());
        vec![
            ShellProfile {
                name: "Windows PowerShell".to_owned(),
                command: "powershell.exe -NoLogo".to_owned(),
                enabled: true,
            },
            ShellProfile {
                name: "Command Prompt".to_owned(),
                command: r"C:\WINDOWS\system32\cmd.exe".to_owned(),
                enabled: true,
            },
            ShellProfile {
                name: "Git Bash".to_owned(),
                // Missing → the canonical path is still recorded (so the
                // settings pane shows something editable), enabled = false.
                command: found.unwrap_or(&GIT_BASH_PATHS[0]).to_string(),
                enabled: found.is_some(),
            },
        ]
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let name = Path::new(&shell)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".to_owned());
        vec![ShellProfile {
            name,
            command: shell,
            enabled: true,
        }]
    }
}

/// JSON store over an injected path. Loads on construction and persists
/// (atomically) on every mutation — same shape as [`crate::projects::ProjectStore`].
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
}

impl SettingsStore {
    /// `path` is the settings.json location (injected for tests). A missing,
    /// unreadable or unparseable file yields DEFAULTS, not an error — a
    /// corrupt settings file must never keep the app from starting.
    pub fn new(path: PathBuf) -> Self {
        let parsed = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        // `"shellProfiles": []` is a user's delete-all and must stick across
        // a restart; a MISSING key (or an unreadable/unparseable file) is
        // first-load. Only first-load gets the platform defaults.
        let first_load = parsed
            .as_ref()
            .map(|v| v.get("shellProfiles").is_none())
            .unwrap_or(true);
        let mut settings = parsed
            .and_then(|v| serde_json::from_value::<Settings>(v).ok())
            .unwrap_or_default();
        settings.normalize();
        if first_load && settings.shell_profiles.is_empty() {
            settings.shell_profiles = platform_shell_profiles();
        }
        Self { path, settings }
    }

    /// Reload from disk (discards in-memory changes).
    pub fn reload(&mut self) {
        *self = Self::new(self.path.clone());
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> &Settings {
        &self.settings
    }

    /// Replace the whole struct (the frontend has no Save button: every
    /// change is a full-struct `set_settings`). Returns the CLAMPED result —
    /// the caller echoes exactly what was persisted, so an out-of-range value
    /// snaps back visibly in the UI.
    pub fn set(&mut self, mut settings: Settings) -> Result<Settings, String> {
        settings.normalize();
        // Identical after clamping → no disk touch. The pane fires a
        // full-struct set per control interaction; a value that didn't
        // actually change must not rewrite the file or ripple further.
        if settings != self.settings {
            self.settings = settings;
            self.save()?;
        }
        Ok(self.settings.clone())
    }

    fn save(&self) -> Result<(), String> {
        crate::atomic::write_json_atomic(&self.path, &self.settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing: default OFF, permanently, and never enabled implicitly.
    /// A future change that flips this default must fail this test by name,
    /// not slip through.
    #[test]
    fn synthetic_prompt_marks_defaults_off_permanently() {
        assert!(
            !Settings::default().synthetic_prompt_marks,
            "synthetic_prompt_marks is opt-in — default OFF, permanently"
        );
        // …and a file that never mentions the field must not turn it on.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"fontSize": 16}"#).unwrap();
        assert!(!SettingsStore::new(path).get().synthetic_prompt_marks);
    }

    #[test]
    fn missing_file_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("nested").join("settings.json"));
        let s = store.get();
        assert!(!s.copy_on_select);
        assert_eq!(s.scroll_wheel_speed, 3);
        assert_eq!(s.font_size, 14);
        assert_eq!(s.font_family, "Geist Mono");
        assert_eq!(s.line_height, 1.2);
        assert!(!s.synthetic_prompt_marks);
        // The clipboard keys are the one settings whose missing
        // fields must default ON (deliberate upgrade behavior).
        assert!(s.ctrl_v_pastes);
        assert!(s.ctrl_c_copy_only);
        assert!(!s.shell_profiles.is_empty(), "platform defaults fill in");
        assert_eq!(
            s.default_exec_profile,
            if cfg!(windows) { "cmd" } else { "sh" }
        );
    }

    #[test]
    fn corrupt_file_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Corrupt counts as first-load: defaults INCLUDING platform profiles.
        let expected = {
            let mut d = Settings::default();
            d.normalize();
            d.shell_profiles = platform_shell_profiles();
            d
        };
        for garbage in ["{not json", "", "[]", r#"{"fontSize": "big"}"#] {
            fs::write(&path, garbage).unwrap();
            let store = SettingsStore::new(path.clone());
            assert_eq!(
                store.get(),
                &expected,
                "garbage {garbage:?} must degrade to defaults"
            );
        }
    }

    #[test]
    fn partial_file_fills_per_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Only two fields present (+ one unknown key, which is ignored).
        fs::write(
            &path,
            r#"{"copyOnSelect": true, "fontFamily": "JetBrains Mono", "letterSpacing": 3}"#,
        )
        .unwrap();
        let store = SettingsStore::new(path);
        let s = store.get();
        assert!(s.copy_on_select, "present field wins");
        assert_eq!(s.font_family, "JetBrains Mono");
        // …everything else falls back per field, not to a whole-struct reset.
        assert_eq!(s.scroll_wheel_speed, 3);
        assert_eq!(s.font_size, 14);
        assert_eq!(s.line_height, 1.2);
    }

    #[test]
    fn out_of_range_values_clamp_to_the_nearest_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Both sides of every numeric bound, plus an unknown font family.
        for (json, want_speed, want_size, want_lh, want_family) in [
            (
                r#"{"scrollWheelSpeed":0,"fontSize":1,"lineHeight":0.1,"fontFamily":"Comic Sans"}"#,
                1u8,
                10u8,
                1.0f32,
                "Geist Mono",
            ),
            (
                r#"{"scrollWheelSpeed":99,"fontSize":200,"lineHeight":9.0,"fontFamily":"MonoLisa"}"#,
                6,
                18,
                1.8,
                "Geist Mono",
            ),
            // In-range values survive untouched (both bounds are inclusive).
            (
                r#"{"scrollWheelSpeed":1,"fontSize":10,"lineHeight":1.0,"fontFamily":"JetBrains Mono"}"#,
                1,
                10,
                1.0,
                "JetBrains Mono",
            ),
            (
                r#"{"scrollWheelSpeed":6,"fontSize":18,"lineHeight":1.8,"fontFamily":"Geist Mono"}"#,
                6,
                18,
                1.8,
                "Geist Mono",
            ),
        ] {
            fs::write(&path, json).unwrap();
            let store = SettingsStore::new(path.clone());
            let s = store.get();
            assert_eq!(s.scroll_wheel_speed, want_speed, "{json}");
            assert_eq!(s.font_size, want_size, "{json}");
            assert_eq!(s.line_height, want_lh, "{json}");
            assert_eq!(s.font_family, want_family, "{json}");
        }
    }

    /// The agent knobs clamp like every other numeric field and default to
    /// a 750 ms quiet window and a 900 s stale threshold.
    #[test]
    fn agent_settings_default_and_clamp() {
        let d = Settings::default();
        assert_eq!(d.agent_ready_quiet_ms, 750);
        assert_eq!(d.agent_ready_max_wait_ms, 5000);
        assert_eq!(d.agent_stale_after_s, 900);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"agentReadyQuietMs": 10, "agentReadyMaxWaitMs": 5, "agentStaleAfterS": 1}"#).unwrap();
        let s = SettingsStore::new(path.clone());
        assert_eq!(s.get().agent_ready_quiet_ms, 250);
        assert_eq!(s.get().agent_ready_max_wait_ms, 1000);
        assert_eq!(s.get().agent_stale_after_s, 30);
        fs::write(&path, r#"{"agentReadyQuietMs": 99999, "agentReadyMaxWaitMs": 99999, "agentStaleAfterS": 999999}"#).unwrap();
        let s = SettingsStore::new(path);
        assert_eq!(s.get().agent_ready_quiet_ms, 5000);
        assert_eq!(s.get().agent_ready_max_wait_ms, 20_000);
        assert_eq!(s.get().agent_stale_after_s, 86_400);
    }

    /// The timer knobs have documented defaults and clamp like every other
    /// numeric field.
    #[test]
    fn timer_settings_default_and_clamp() {
        let d = Settings::default();
        assert_eq!(d.idle_threshold_ms, 120_000);
        assert_eq!(d.timer_confirm_ms, 5_000);
        assert_eq!(d.timer_delivery_timeout_ms, 30_000);
        assert_eq!(d.timer_dedupe_ms, 5_000);
        assert_eq!(d.timer_retention_hours, 24);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"idleThresholdMs": 1, "timerDeliveryTimeoutMs": 1, "timerRetentionHours": 0}"#,
        )
        .unwrap();
        let s = SettingsStore::new(path.clone());
        assert_eq!(s.get().idle_threshold_ms, 1_000);
        assert_eq!(s.get().timer_delivery_timeout_ms, 1_000);
        assert_eq!(s.get().timer_retention_hours, 1);
        // 0 is IN range for the two "disable me" knobs and survives untouched.
        assert_eq!(s.get().timer_confirm_ms, 5_000, "untouched key keeps its default");
        fs::write(
            &path,
            r#"{"idleThresholdMs": 99999999, "timerConfirmMs": 0, "timerDedupeMs": 0, "timerRetentionHours": 99999}"#,
        )
        .unwrap();
        let s = SettingsStore::new(path);
        assert_eq!(s.get().idle_threshold_ms, 3_600_000);
        assert_eq!(s.get().timer_confirm_ms, 0);
        assert_eq!(s.get().timer_dedupe_ms, 0);
        assert_eq!(s.get().timer_retention_hours, 720);
    }

    /// The clipboard keys default ON, and the TRUE serde default is
    /// DELIBERATE — a pre-settings.json that never mentions either
    /// field must gain the new behavior on upgrade (the requirement was: a
    /// reflexive ^V should paste, a reflexive ^C must not kill a long agent
    /// run), not keep the legacy passthrough.
    #[test]
    fn clipboard_keys_default_on_and_existing_files_gain_the_behavior() {
        let d = Settings::default();
        assert!(d.ctrl_v_pastes, "ctrl_v_pastes defaults ON");
        assert!(d.ctrl_c_copy_only, "ctrl_c_copy_only defaults ON");
        // A file written before (neither field present): both land
        // ON, and the present fields still win.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"copyOnSelect": true, "fontSize": 16}"#).unwrap();
        let store = SettingsStore::new(path);
        let s = store.get();
        assert!(s.ctrl_v_pastes, "missing field falls to the TRUE default, not FALSE");
        assert!(s.ctrl_c_copy_only, "missing field falls to the TRUE default, not FALSE");
        assert!(s.copy_on_select, "a present field still wins");
        assert_eq!(s.font_size, 16);
    }

    /// An explicit `false` is honored (the pane's OFF toggle) and
    /// round-trips across a full-struct `set` and a restart — the bools need
    /// no clamping, so the round-trip is the whole "clamp/round-trip like
    /// copy_on_select" surface for these fields.
    #[test]
    fn clipboard_keys_explicit_false_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"copyOnSelect": true, "ctrlVPastes": false, "ctrlCCopyOnly": false}"#,
        )
        .unwrap();
        let mut store = SettingsStore::new(path.clone());
        let s = store.get();
        assert!(!s.ctrl_v_pastes, "explicit false is honored");
        assert!(!s.ctrl_c_copy_only, "explicit false is honored");
        // The pane fires a full-struct set per control interaction; the
        // false values must survive that write and a fresh load.
        store.set(store.get().clone()).unwrap();
        let reopened = SettingsStore::new(path);
        assert!(!reopened.get().ctrl_v_pastes, "false survives set + restart");
        assert!(!reopened.get().ctrl_c_copy_only, "false survives set + restart");
        assert!(reopened.get().copy_on_select, "unrelated fields untouched");
    }

    #[test]
    fn set_clamps_persists_and_returns_the_clamped_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appconfig").join("settings.json");
        let mut store = SettingsStore::new(path.clone());

        let mut wanted = Settings::default();
        wanted.scroll_wheel_speed = 250;
        wanted.font_size = 2;
        wanted.line_height = 5.0;
        wanted.copy_on_select = true;
        let clamped = store.set(wanted).unwrap();
        assert_eq!(clamped.scroll_wheel_speed, 6);
        assert_eq!(clamped.font_size, 10);
        assert_eq!(clamped.line_height, 1.8);
        assert!(clamped.copy_on_select);

        // Persisted: a fresh store over the same path sees the clamped values.
        let reopened = SettingsStore::new(path);
        assert_eq!(reopened.get(), &clamped);
    }

    #[test]
    fn atomic_write_leaves_valid_json_on_a_simulated_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = SettingsStore::new(path.clone());
        let mut good = Settings::default();
        good.font_size = 16;
        store.set(good).unwrap();

        // Simulate a crash mid-write: the tmp file exists with the NEW
        // (here: truncated/garbage) content, but the rename never happened.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, r#"{"fontSize": 1"#).unwrap();

        store.reload();
        assert_eq!(store.get().font_size, 16, "old values survive the crash");
        assert!(tmp.is_file(), "the abandoned tmp is what we just wrote");
        // And the real file is still valid JSON.
        let text = fs::read_to_string(&path).unwrap();
        serde_json::from_str::<Settings>(&text).expect("settings.json stayed parseable");
    }

    #[test]
    fn wire_shape_is_camel_case() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        for key in [
            "copyOnSelect",
            "scrollWheelSpeed",
            "fontSize",
            "fontFamily",
            "lineHeight",
            "syntheticPromptMarks",
            "shellProfiles",
            "defaultExecProfile",
            "agentReadyQuietMs",
            "agentStaleAfterS",
            "ctrlVPastes",
            "ctrlCCopyOnly",
            "agentReapOrphans",
        ] {
            assert!(json.get(key).is_some(), "missing wire key {key}");
        }
        // snake_case must NOT leak onto the wire.
        assert!(json.get("copy_on_select").is_none());
        assert!(json.get("ctrl_v_pastes").is_none());
        assert!(json.get("ctrl_c_copy_only").is_none());
    }

    #[test]
    #[cfg(windows)]
    fn windows_defaults_are_the_three_profiles() {
        let profiles = platform_shell_profiles();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Windows PowerShell", "Command Prompt", "Git Bash"]
        );
        assert_eq!(profiles[0].command, "powershell.exe -NoLogo");
        assert_eq!(profiles[1].command, r"C:\WINDOWS\system32\cmd.exe");
        assert!(profiles[0].enabled && profiles[1].enabled);
        // Git Bash: enabled iff the resolved path actually exists.
        assert_eq!(
            profiles[2].enabled,
            Path::new(&profiles[2].command).is_file()
        );
    }

    #[test]
    #[cfg(unix)]
    fn unix_default_is_a_single_shell_entry() {
        let profiles = platform_shell_profiles();
        assert_eq!(profiles.len(), 1);
        assert!(profiles[0].enabled);
        assert!(!profiles[0].command.is_empty());
    }

    #[test]
    fn enabled_profile_lookup_respects_the_toggle() {
        let mut s = Settings::default();
        s.shell_profiles = vec![
            ShellProfile {
                name: "On".into(),
                command: "a b".into(),
                enabled: true,
            },
            ShellProfile {
                name: "Off".into(),
                command: "c".into(),
                enabled: false,
            },
        ];
        assert!(s.enabled_profile("On").is_some());
        assert!(
            s.enabled_profile("Off").is_none(),
            "disabled never resolves"
        );
        assert!(s.profile("Off").is_some(), "but it is still listed");
        assert!(s.enabled_profile("nope").is_none());
    }

    #[test]
    fn command_line_splits_on_whitespace() {
        assert_eq!(
            split_command_line("powershell.exe -NoLogo"),
            ("powershell.exe".to_owned(), vec!["-NoLogo".to_owned()])
        );
        assert_eq!(
            split_command_line("bash"),
            ("bash".to_owned(), Vec::<String>::new())
        );
        assert_eq!(
            split_command_line(""),
            (String::new(), Vec::<String>::new())
        );
        // Regime 2: a QUOTED program token with spaces splits correctly.
        assert_eq!(
            split_command_line(r#""C:\Program Files\Git\bin\bash.exe" -l"#),
            (
                r"C:\Program Files\Git\bin\bash.exe".to_owned(),
                vec!["-l".to_owned()]
            )
        );
    }

    /// Regime 1: the shipped Git Bash default is an UNQUOTED absolute path
    /// with spaces — the whole line naming an existing file must be the
    /// program with no args, not split at `C:\Program`.
    #[test]
    fn command_line_that_is_an_existing_file_is_the_whole_program() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Program Files");
        fs::create_dir(&sub).unwrap();
        let exe = sub.join("tool.exe");
        fs::write(&exe, b"x").unwrap();
        let line = exe.to_string_lossy().into_owned();
        assert_eq!(split_command_line(&line), (line.clone(), Vec::new()));
    }

    /// Delete-all is a user choice, live and across restarts — `normalize`
    /// must not resurrect the platform defaults (review finding).
    #[test]
    fn delete_all_profiles_sticks_across_set_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = SettingsStore::new(path.clone());
        assert!(
            !store.get().shell_profiles.is_empty(),
            "first load fills platform defaults"
        );
        let mut s = store.get().clone();
        s.shell_profiles.clear();
        let out = store.set(s).unwrap();
        assert!(
            out.shell_profiles.is_empty(),
            "delete-all must not resurrect the defaults"
        );
        let reopened = SettingsStore::new(path);
        assert!(
            reopened.get().shell_profiles.is_empty(),
            "…and the explicit empty list survives a restart"
        );
    }

    /// One half-written profile entry must degrade ALONE — before the lenient
    /// `ShellProfile` defaults, it failed the whole parse and reset every
    /// other setting (review finding).
    #[test]
    fn half_written_profile_entry_degrades_alone_not_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"fontSize": 16, "shellProfiles": [
                {"name": "ok", "command": "sh"},
                {"name": "no-command"},
                {"command": "orphan.exe"}
            ]}"#,
        )
        .unwrap();
        let store = SettingsStore::new(path);
        assert_eq!(store.get().font_size, 16, "the rest of the file survives");
        let profiles = &store.get().shell_profiles;
        assert_eq!(profiles.len(), 1, "empty name/command entries are pruned");
        assert_eq!(profiles[0].name, "ok");
        assert!(profiles[0].enabled, "missing `enabled` defaults ON");
    }

    /// An identical full-struct set (the pane sends one per control
    /// interaction) must be a no-op on disk.
    #[test]
    fn unchanged_set_does_not_touch_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = SettingsStore::new(path.clone());
        let mut s = store.get().clone();
        s.font_size = 16;
        store.set(s.clone()).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        store.set(s).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "identical set must not rewrite settings.json"
        );
    }
}
