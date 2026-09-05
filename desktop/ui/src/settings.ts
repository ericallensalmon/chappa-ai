// Settings store. One tiny observable box around `get_settings` /
// `set_settings`, plus the pure helpers the UI and the panels share.
//
// House style: exported pure functions + a plain class (cf. railReducer /
// GlLru in app.ts). There is NO Save button anywhere — every control change
// calls `update()` immediately, and the value that lands in the store is the
// one Rust RETURNED (it clamps; the optimistic patch is never authoritative).
//
// Units, stated once:
//   scrollWheelSpeed  dimensionless multiplier, 1..=6 (TUI wheel only)
//   fontSize          CSS px, 10..=18
//   lineHeight        dimensionless multiplier, 1.0..=1.8 in 0.1 steps
//                     (cell height in CSS px = fontSize × lineHeight)
//   fontFamily        a BARE face name, never a stack — see terminalFontStack

import { cssFontFamily } from "./term/atlas";
import * as ipc from "./ipc";

export type ShellProfile = ipc.ShellProfileDto;
export type Settings = ipc.SettingsDto;

/** The two faces we bundle (OFL). No system-font enumeration, ever: a face we
 *  don't ship can't be rasterized into the atlas. Head of the list is the
 *  default. */
export const BUNDLED_FONTS = ["Geist Mono", "JetBrains Mono"] as const;

/** CSS px. */
export const FONT_SIZE_MIN = 10;
export const FONT_SIZE_MAX = 18;

/** Dimensionless multiplier, TUI wheel only. */
export const WHEEL_SPEED_MIN = 1;
export const WHEEL_SPEED_MAX = 6;

/** The 1.0…1.8 line-height ladder (0.1 steps) the segmented picker offers.
 *  Built by hand rather than by float accumulation so every entry is an exact
 *  one-decimal value. */
export const LINE_HEIGHTS = [1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8] as const;

/** The 1x…6x ladder the scroll-speed picker offers. */
export const WHEEL_SPEEDS = [1, 2, 3, 4, 5, 6] as const;

/** Builtin execution profiles (the runner): the shell that runs a
 *  chappa.yml `command` line when no shell profile is named. Both are offered
 *  regardless of platform — the webview can't read the OS reliably and Rust
 *  owns the platform default anyway. */
export const BUILTIN_EXEC_PROFILES = ["cmd", "sh"] as const;

/** The agent ready-gate quiet window (ms) and the bridge stale
 *  threshold (s). Ranges mirror `project_model::settings`. */
export const AGENT_READY_QUIET_MIN = 250;
export const AGENT_READY_QUIET_MAX = 5000;
export const AGENT_READY_MAX_WAIT_MIN = 1000;
export const AGENT_READY_MAX_WAIT_MAX = 20000;
export const AGENT_STALE_AFTER_MIN = 30;
export const AGENT_STALE_AFTER_MAX = 86400;

/** The timer knobs. Ranges mirror `project_model::settings` exactly —
 *  Rust is still the authority; these exist so the pane clamps visibly and so
 *  the store behaves outside Tauri. */
export const IDLE_THRESHOLD_MIN = 1000;
export const IDLE_THRESHOLD_MAX = 3600000;
export const TIMER_CONFIRM_MIN = 0;
export const TIMER_CONFIRM_MAX = 600000;
export const TIMER_DELIVERY_TIMEOUT_MIN = 1000;
export const TIMER_DELIVERY_TIMEOUT_MAX = 300000;
export const TIMER_DEDUPE_MIN = 0;
export const TIMER_DEDUPE_MAX = 600000;
export const TIMER_RETENTION_HOURS_MIN = 1;
export const TIMER_RETENTION_HOURS_MAX = 720;

/** The frontend's mirror of the Rust defaults. Used before `load()` resolves
 *  and outside Tauri (dev in a plain browser, vitest). Shell profiles start
 *  EMPTY here: their platform defaults are resolved Rust-side (PowerShell /
 *  Command Prompt / Git Bash on Windows, `$SHELL` on unix) and the webview
 *  must not invent shell command lines it cannot verify. */
export function defaultSettings(): Settings {
  return {
    copyOnSelect: false,
    // Both TRUE on purpose — the same deliberate upgrade behavior
    // Rust ships (a settings.json that never mentions the field must gain
    // paste-on-^V / copy-only-^C, not keep the legacy passthrough).
    ctrlVPastes: true,
    ctrlCCopyOnly: true,
    scrollWheelSpeed: 3,
    fontSize: 14,
    fontFamily: BUNDLED_FONTS[0],
    lineHeight: 1.2,
    syntheticPromptMarks: false,
    shellProfiles: [],
    defaultExecProfile: "cmd",
    agentReadyQuietMs: 750,
    agentReadyMaxWaitMs: 5000,
    agentStaleAfterS: 900,
    idleThresholdMs: 120000,
    timerConfirmMs: 5000,
    timerDeliveryTimeoutMs: 30000,
    timerDedupeMs: 5000,
    timerRetentionHours: 24,
    // On by default — orphans of a hard restart / mid-close docker
    // hiccup are exactly the leak this reaps (matches `project_model`).
    agentReapOrphans: true,
  };
}

function clampInt(v: unknown, lo: number, hi: number, fallback: number): number {
  const n = typeof v === "number" && Number.isFinite(v) ? Math.round(v) : NaN;
  if (Number.isNaN(n)) return fallback;
  return n < lo ? lo : n > hi ? hi : n;
}

/**
 * Range-clamp a line height to [1.0, 1.8]. The 0.1 step is a UI affordance,
 * NOT a validation rule (Rust: "arbitrary in-range floats round-trip
 * untouched") — snapping here would silently rewrite a hand-edited 1.25 the
 * next time any other setting changed, since every write sends the full
 * struct. {@link nearestLineHeight} is what the picker uses for display.
 */
function clampLineHeight(v: unknown, fallback: number): number {
  const n = typeof v === "number" && Number.isFinite(v) ? v : NaN;
  if (Number.isNaN(n)) return fallback;
  return n < 1.0 ? 1.0 : n > 1.8 ? 1.8 : n;
}

/** The ladder entry closest to `v` — the segmented picker can only show one of
 *  its own options, so an off-step stored value displays as its neighbour
 *  while the store keeps the exact number. */
export function nearestLineHeight(v: number): number {
  let best = LINE_HEIGHTS[0] as number;
  for (const h of LINE_HEIGHTS) {
    if (Math.abs(h - v) < Math.abs(best - v)) best = h;
  }
  return best;
}

/**
 * Range/shape guard for a settings value of unknown provenance. Rust is the
 * authority — this exists so the store still behaves outside Tauri and so a
 * malformed payload can never poison the font pipeline (a NaN font size
 * silently zeroes the grid). Boundary regime: values ON the limits pass
 * through untouched, values outside snap to the nearest limit, and anything
 * non-numeric falls back per field.
 */
export function clampSettings(raw: Partial<Settings> | null | undefined): Settings {
  const d = defaultSettings();
  const s = raw ?? {};
  const profiles = Array.isArray(s.shellProfiles)
    ? s.shellProfiles
        .filter((p) => p && typeof p.name === "string" && p.name.trim() !== "")
        .map((p) => ({
          name: p.name,
          command: typeof p.command === "string" ? p.command : "",
          enabled: p.enabled !== false,
        }))
    : d.shellProfiles;
  const family =
    typeof s.fontFamily === "string" && (BUNDLED_FONTS as readonly string[]).includes(s.fontFamily)
      ? s.fontFamily
      : d.fontFamily;
  return {
    copyOnSelect: s.copyOnSelect === true,
    // Defaults TRUE, so the per-field fallback is "treat everything
    // but an explicit false as the default" — the mirror image of
    // `copyOnSelect === true` above. A missing key or a garbage value
    // (hand-edited file) lands on the TRUE default, never on the legacy
    // passthrough.
    ctrlVPastes: s.ctrlVPastes !== false,
    ctrlCCopyOnly: s.ctrlCCopyOnly !== false,
    scrollWheelSpeed: clampInt(s.scrollWheelSpeed, WHEEL_SPEED_MIN, WHEEL_SPEED_MAX, d.scrollWheelSpeed),
    fontSize: clampInt(s.fontSize, FONT_SIZE_MIN, FONT_SIZE_MAX, d.fontSize),
    fontFamily: family,
    lineHeight: clampLineHeight(s.lineHeight, d.lineHeight),
    syntheticPromptMarks: s.syntheticPromptMarks === true,
    shellProfiles: profiles,
    defaultExecProfile:
      typeof s.defaultExecProfile === "string" && s.defaultExecProfile !== ""
        ? s.defaultExecProfile
        : d.defaultExecProfile,
    agentReadyQuietMs: clampInt(
      s.agentReadyQuietMs,
      AGENT_READY_QUIET_MIN,
      AGENT_READY_QUIET_MAX,
      d.agentReadyQuietMs,
    ),
    agentReadyMaxWaitMs: clampInt(
      s.agentReadyMaxWaitMs,
      AGENT_READY_MAX_WAIT_MIN,
      AGENT_READY_MAX_WAIT_MAX,
      d.agentReadyMaxWaitMs,
    ),
    agentStaleAfterS: clampInt(
      s.agentStaleAfterS,
      AGENT_STALE_AFTER_MIN,
      AGENT_STALE_AFTER_MAX,
      d.agentStaleAfterS,
    ),
    idleThresholdMs: clampInt(s.idleThresholdMs, IDLE_THRESHOLD_MIN, IDLE_THRESHOLD_MAX, d.idleThresholdMs),
    timerConfirmMs: clampInt(s.timerConfirmMs, TIMER_CONFIRM_MIN, TIMER_CONFIRM_MAX, d.timerConfirmMs),
    timerDeliveryTimeoutMs: clampInt(
      s.timerDeliveryTimeoutMs,
      TIMER_DELIVERY_TIMEOUT_MIN,
      TIMER_DELIVERY_TIMEOUT_MAX,
      d.timerDeliveryTimeoutMs,
    ),
    timerDedupeMs: clampInt(s.timerDedupeMs, TIMER_DEDUPE_MIN, TIMER_DEDUPE_MAX, d.timerDedupeMs),
    timerRetentionHours: clampInt(
      s.timerRetentionHours,
      TIMER_RETENTION_HOURS_MIN,
      TIMER_RETENTION_HOURS_MAX,
      d.timerRetentionHours,
    ),
    // Default TRUE — a missing/garbage value lands on the ON
    // default, mirroring the ctrl-v/ctrl-c deliberate-upgrade shape.
    agentReapOrphans: s.agentReapOrphans !== false,
  };
}

/**
 * The CSS `font-family` value for a terminal: the chosen face first, then the
 * other bundled face, then the generic `monospace`.
 *
 * The stored setting is a BARE face name; every bare name goes through
 * `cssFontFamily` (which quotes it), while `monospace` stays unquoted — a
 * quoted "monospace" would be a family lookup, not the generic keyword. A
 * value that ALREADY carries a comma or a quote is treated as a complete
 * stack and passed through untouched: re-quoting a stack makes the whole
 * declaration invalid CSS, which the engine silently drops (seen on the host:
 * cells measured in the body's proportional font).
 */
export function terminalFontStack(family: string): string {
  if (/[,"']/.test(family)) return family;
  const rest = BUNDLED_FONTS.filter((f) => f !== family);
  return [family, ...rest].map(cssFontFamily).concat("monospace").join(", ");
}

/** The IPC seam the store talks to; tests inject a fake. */
export interface SettingsApi {
  getSettings(): Promise<Settings>;
  setSettings(settings: Settings): Promise<Settings>;
}

/** The real adapter. Outside Tauri (plain browser, vitest) it degrades to the
 *  frontend defaults + local clamping so the UI is still drivable. */
export const tauriSettingsApi: SettingsApi = {
  getSettings: () => (ipc.inTauri() ? ipc.getSettings() : Promise.resolve(defaultSettings())),
  setSettings: (s) => (ipc.inTauri() ? ipc.setSettings(s) : Promise.resolve(clampSettings(s))),
};

/**
 * The observable box. `load()` once at boot; every control change calls
 * `update(patch)`, which merges, sends the FULL struct, and adopts whatever
 * Rust hands back before notifying subscribers.
 */
export class SettingsStore {
  private current: Settings = defaultSettings();
  /** False until a `load()` has actually succeeded. Guards `update()`: with
   *  only frontend defaults as the merge base, a full-struct write would
   *  overwrite the user's real settings.json with defaults+patch. */
  private loaded = false;
  private readonly subs = new Set<(s: Settings) => void>();

  constructor(private readonly api: SettingsApi = tauriSettingsApi) {}

  /** The current canonical settings (a copy — subscribers must not mutate the
   *  store's own object). */
  get(): Settings {
    return { ...this.current, shellProfiles: this.current.shellProfiles.map((p) => ({ ...p })) };
  }

  /** Boot load. A failure (not in Tauri, config unreadable) is not an error:
   *  the defaults stand and the app runs. */
  async load(): Promise<Settings> {
    try {
      this.current = clampSettings(await this.api.getSettings());
      this.loaded = true;
    } catch {
      this.current = defaultSettings();
    }
    this.notify();
    return this.get();
  }

  /**
   * Merge `patch`, persist the full struct, adopt the RETURNED canonical
   * value, notify. On an IPC failure the store keeps its previous value (a
   * rejected write must not leave the UI showing a setting that isn't stored).
   */
  async update(patch: Partial<Settings>): Promise<Settings> {
    // A failed boot load left the merge base on frontend defaults; re-fetch
    // the stored truth first so this write cannot flatten the user's real
    // settings.json into defaults+patch (review finding).
    if (!this.loaded) await this.load();
    const merged = clampSettings({ ...this.current, ...patch });
    try {
      this.current = clampSettings(await this.api.setSettings(merged));
    } catch {
      // The browser already flipped the control before the write failed —
      // re-broadcast the surviving canonical value so the pane snaps back
      // instead of showing a setting that isn't stored.
      this.notify();
      return this.get();
    }
    this.notify();
    return this.get();
  }

  /** Subscribe to canonical-value changes. Returns the unsubscribe fn. */
  subscribe(fn: (s: Settings) => void): () => void {
    this.subs.add(fn);
    return () => {
      this.subs.delete(fn);
    };
  }

  private notify(): void {
    const snapshot = this.get();
    for (const fn of [...this.subs]) fn(snapshot);
  }
}

/** The app-wide instance. Tests build their own `SettingsStore` with a fake
 *  api; App/TerminalPanel take one via options and default to this. */
export const settingsStore = new SettingsStore();
