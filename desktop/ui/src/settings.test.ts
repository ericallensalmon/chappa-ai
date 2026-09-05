import { describe, expect, it, vi } from "vitest";
import {
  BUNDLED_FONTS,
  SettingsStore,
  clampSettings,
  defaultSettings,
  nearestLineHeight,
  terminalFontStack,
  type Settings,
  type SettingsApi,
} from "./settings";

/** A fake Rust side. `canonical` models the clamping `set_settings` does — the
 *  whole point of the full-struct set contract is that its ANSWER wins. */
function fakeApi(
  seed: Partial<Settings> = {},
  canonical: (s: Settings) => Settings = (s) => s,
): SettingsApi & { setSettings: ReturnType<typeof vi.fn> } {
  let current = clampSettings({ ...defaultSettings(), ...seed });
  return {
    getSettings: vi.fn(async () => current),
    setSettings: vi.fn(async (s: Settings) => {
      current = canonical(s);
      return current;
    }),
  } as SettingsApi & { setSettings: ReturnType<typeof vi.fn> };
}

describe("defaults", () => {
  it("ships synthetic prompt marks OFF — the opt-in is deliberate", () => {
    expect(defaultSettings().syntheticPromptMarks).toBe(false);
  });

  it("defaults the rest to the documented values", () => {
    const d = defaultSettings();
    expect(d.copyOnSelect).toBe(false);
    // The clipboard keys default ON — the same deliberate upgrade
    // behavior Rust ships (existing settings.json gains the behavior).
    expect(d.ctrlVPastes).toBe(true);
    expect(d.ctrlCCopyOnly).toBe(true);
    expect(d.scrollWheelSpeed).toBe(3); // dimensionless TUI multiplier
    expect(d.fontSize).toBe(14); // CSS px
    expect(d.fontFamily).toBe("Geist Mono"); // bare face name, not a stack
    expect(d.lineHeight).toBe(1.2); // dimensionless (cellH = px × this)
  });

  it("explicit false is honored, missing/garbage falls back to the TRUE default", () => {
    // The mirror image of `copyOnSelect === true`: everything but an
    // EXPLICIT false lands on the default (true) — a missing key (a
    // pre-file) must gain the behavior, not lose it.
    expect(clampSettings({ ctrlVPastes: false, ctrlCCopyOnly: true }).ctrlVPastes).toBe(false);
    expect(clampSettings({ ctrlVPastes: false, ctrlCCopyOnly: true }).ctrlCCopyOnly).toBe(true);
    expect(clampSettings({}).ctrlVPastes).toBe(true);
    expect(clampSettings({}).ctrlCCopyOnly).toBe(true);
    expect(clampSettings({ ctrlVPastes: undefined, ctrlCCopyOnly: undefined }).ctrlVPastes).toBe(true);
    // Garbage (a hand-edited file) is a per-field fallback to the default.
    expect(clampSettings({ ctrlVPastes: "yes" as unknown as boolean }).ctrlVPastes).toBe(true);
    expect(clampSettings({ ctrlCCopyOnly: 0 as unknown as boolean }).ctrlCCopyOnly).toBe(true);
  });
});

describe("clampSettings", () => {
  it("passes values ON the limits through untouched (boundary regime)", () => {
    const lo = clampSettings({ fontSize: 10, scrollWheelSpeed: 1, lineHeight: 1.0 });
    expect(lo.fontSize).toBe(10);
    expect(lo.scrollWheelSpeed).toBe(1);
    expect(lo.lineHeight).toBe(1.0);
    const hi = clampSettings({ fontSize: 18, scrollWheelSpeed: 6, lineHeight: 1.8 });
    expect(hi.fontSize).toBe(18);
    expect(hi.scrollWheelSpeed).toBe(6);
    expect(hi.lineHeight).toBe(1.8);
  });

  it("snaps values one step OUTSIDE each limit back onto it", () => {
    expect(clampSettings({ fontSize: 9 }).fontSize).toBe(10); // CSS px
    expect(clampSettings({ fontSize: 19 }).fontSize).toBe(18);
    expect(clampSettings({ scrollWheelSpeed: 0 }).scrollWheelSpeed).toBe(1);
    expect(clampSettings({ scrollWheelSpeed: 7 }).scrollWheelSpeed).toBe(6);
    expect(clampSettings({ lineHeight: 0.9 }).lineHeight).toBe(1.0);
    expect(clampSettings({ lineHeight: 1.9 }).lineHeight).toBe(1.8);
  });

  // The timer knobs mirror `project_model::settings` — same defaults,
  // same ranges, and 0 IS in range for the two "disable me" windows.
  it("defaults and clamps the timer knobs like Rust does", () => {
    const d = defaultSettings();
    expect(d.idleThresholdMs).toBe(120000);
    expect(d.timerConfirmMs).toBe(5000);
    expect(d.timerDeliveryTimeoutMs).toBe(30000);
    expect(d.timerDedupeMs).toBe(5000);
    expect(d.timerRetentionHours).toBe(24);
    expect(clampSettings({ idleThresholdMs: 999 }).idleThresholdMs).toBe(1000);
    expect(clampSettings({ idleThresholdMs: 3600001 }).idleThresholdMs).toBe(3600000);
    expect(clampSettings({ timerConfirmMs: 0 }).timerConfirmMs).toBe(0);
    expect(clampSettings({ timerConfirmMs: -1 }).timerConfirmMs).toBe(0);
    expect(clampSettings({ timerDedupeMs: 0 }).timerDedupeMs).toBe(0);
    expect(clampSettings({ timerDeliveryTimeoutMs: 1 }).timerDeliveryTimeoutMs).toBe(1000);
    expect(clampSettings({ timerRetentionHours: 0 }).timerRetentionHours).toBe(1);
    expect(clampSettings({ timerRetentionHours: 721 }).timerRetentionHours).toBe(720);
    // A non-numeric value falls back per field, never NaN.
    expect(clampSettings({ idleThresholdMs: "soon" as unknown as number }).idleThresholdMs).toBe(120000);
  });

  it("round-trips an off-step line height untouched (the step is UI-only)", () => {
    // Rust: "arbitrary in-range floats round-trip untouched". Snapping here
    // would silently rewrite a hand-edited 1.25 the next time ANY other
    // setting changed, because every write sends the full struct.
    expect(clampSettings({ lineHeight: 1.25 }).lineHeight).toBe(1.25);
    // …but the picker can only display one of its own steps.
    expect(nearestLineHeight(1.24)).toBe(1.2);
    expect(nearestLineHeight(1.26)).toBe(1.3);
    expect(nearestLineHeight(1.5)).toBe(1.5);
  });

  it("falls back per field for garbage, never throwing", () => {
    const s = clampSettings({
      fontSize: Number.NaN,
      lineHeight: Number.POSITIVE_INFINITY,
      fontFamily: "Comic Sans",
      shellProfiles: undefined,
    } as unknown as Partial<Settings>);
    expect(s.fontSize).toBe(14);
    expect(s.lineHeight).toBe(1.2);
    // Only bundled faces survive: a face we don't ship cannot be rasterized.
    expect(s.fontFamily).toBe("Geist Mono");
    expect(s.shellProfiles).toEqual([]);
  });

  it("drops nameless shell profiles and defaults `enabled` to true", () => {
    const s = clampSettings({
      shellProfiles: [
        { name: "  ", command: "x", enabled: true },
        { name: "Git Bash", command: "bash.exe" },
      ] as Settings["shellProfiles"],
    });
    expect(s.shellProfiles).toEqual([{ name: "Git Bash", command: "bash.exe", enabled: true }]);
  });
});

describe("terminalFontStack", () => {
  it("puts the chosen face first, the other bundled face next, monospace last", () => {
    expect(terminalFontStack("Geist Mono")).toBe('"Geist Mono", "JetBrains Mono", monospace');
    expect(terminalFontStack("JetBrains Mono")).toBe('"JetBrains Mono", "Geist Mono", monospace');
  });

  it("never re-quotes a value that is already a STACK", () => {
    // Double-wrapping a stack makes the whole declaration invalid CSS, which
    // the engine silently drops (seen on the host: the grid was measured in
    // the body's proportional font).
    const stack = '"Geist Mono", "JetBrains Mono", monospace';
    expect(terminalFontStack(stack)).toBe(stack);
    expect(terminalFontStack(stack)).not.toContain('""');
  });

  it("leaves the generic `monospace` keyword unquoted", () => {
    for (const face of BUNDLED_FONTS) {
      expect(terminalFontStack(face).endsWith(", monospace")).toBe(true);
      expect(terminalFontStack(face)).not.toContain('"monospace"');
    }
  });
});

describe("SettingsStore", () => {
  it("loads once and hands subscribers the loaded value", async () => {
    const api = fakeApi({ fontSize: 16 });
    const store = new SettingsStore(api);
    const seen: Settings[] = [];
    store.subscribe((s) => seen.push(s));
    await store.load();
    expect(api.getSettings).toHaveBeenCalledTimes(1);
    expect(store.get().fontSize).toBe(16);
    expect(seen).toHaveLength(1);
  });

  it("falls back to defaults when the load fails (no Tauri, unreadable config)", async () => {
    const store = new SettingsStore({
      getSettings: async () => {
        throw new Error("no tauri");
      },
      setSettings: async (s) => s,
    });
    await store.load();
    expect(store.get()).toEqual(defaultSettings());
  });

  it("notifies subscribers with the CANONICAL returned value, not the patch", async () => {
    // Rust clamps: the store must publish what came BACK, or the pane would
    // show 30 px while the terminal renders 18.
    const api = fakeApi({}, (s) => ({ ...s, fontSize: 18 }));
    const store = new SettingsStore(api);
    await store.load();
    const seen: number[] = [];
    store.subscribe((s) => seen.push(s.fontSize));
    const result = await store.update({ fontSize: 30 });
    expect(api.setSettings).toHaveBeenCalledTimes(1);
    // The full struct goes over the wire, not a patch.
    expect(Object.keys(api.setSettings.mock.calls[0][0]).sort()).toEqual(
      Object.keys(defaultSettings()).sort(),
    );
    expect(seen).toEqual([18]);
    expect(result.fontSize).toBe(18);
    expect(store.get().fontSize).toBe(18);
  });

  it("merges a patch onto the current value instead of replacing it", async () => {
    const api = fakeApi({ copyOnSelect: true, fontSize: 16 });
    const store = new SettingsStore(api);
    await store.load();
    await store.update({ lineHeight: 1.5 });
    expect(store.get()).toMatchObject({ copyOnSelect: true, fontSize: 16, lineHeight: 1.5 });
  });

  it("keeps the previous value when the write fails", async () => {
    const store = new SettingsStore({
      getSettings: async () => ({ ...defaultSettings(), fontSize: 14 }),
      setSettings: async () => {
        throw new Error("disk full");
      },
    });
    await store.load();
    const seen: Settings[] = [];
    store.subscribe((s) => seen.push(s));
    await store.update({ fontSize: 18 });
    expect(store.get().fontSize).toBe(14);
    // A rejected write re-publishes the SURVIVING canonical value (never the
    // failed patch): the browser already flipped the control before the write
    // failed, and this snap-back notify is what un-flips it.
    expect(seen.map((s) => s.fontSize)).toEqual([14]);
  });

  it("re-fetches the stored truth before a write when boot load never succeeded", async () => {
    // Boundary regime: the merge base after a failed load is
    // frontend DEFAULTS — writing from that state would flatten the user's
    // real settings.json into defaults+patch.
    let failLoads = 1;
    let written: Settings | null = null;
    const stored = { ...defaultSettings(), fontSize: 17, copyOnSelect: true };
    const store = new SettingsStore({
      getSettings: async () => {
        if (failLoads > 0) {
          failLoads -= 1;
          throw new Error("ipc hiccup");
        }
        return { ...stored };
      },
      setSettings: async (s) => {
        written = s;
        return s;
      },
    });
    await store.load(); // fails → defaults stand
    expect(store.get().fontSize).toBe(14);
    await store.update({ scrollWheelSpeed: 5 });
    // The write's base is the RE-FETCHED stored truth, not the defaults.
    expect(written!.fontSize).toBe(17);
    expect(written!.copyOnSelect).toBe(true);
    expect(written!.scrollWheelSpeed).toBe(5);
  });

  it("hands out copies: mutating a snapshot cannot poison the store", async () => {
    const api = fakeApi({ shellProfiles: [{ name: "sh", command: "/bin/sh", enabled: true }] });
    const store = new SettingsStore(api);
    await store.load();
    const snapshot = store.get();
    snapshot.shellProfiles[0].enabled = false;
    snapshot.fontSize = 18;
    expect(store.get().shellProfiles[0].enabled).toBe(true);
    expect(store.get().fontSize).toBe(14);
  });

  it("unsubscribe stops delivery", async () => {
    const api = fakeApi();
    const store = new SettingsStore(api);
    await store.load();
    let count = 0;
    const off = store.subscribe(() => (count += 1));
    await store.update({ copyOnSelect: true });
    off();
    await store.update({ copyOnSelect: false });
    expect(count).toBe(1);
  });
});
