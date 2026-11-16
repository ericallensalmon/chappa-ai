// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { SecuritySection, settingOf, switchHint, switchOf } from "./security_section";
import { SettingsStore, type SettingsApi, defaultSettings } from "./settings";

function store(): { store: SettingsStore; sets: Array<Record<string, unknown>> } {
  const sets: Array<Record<string, unknown>> = [];
  const api: SettingsApi = {
    getSettings: async () => defaultSettings(),
    setSettings: async (s) => {
      sets.push(s as unknown as Record<string, unknown>);
      return s;
    },
  };
  return { store: new SettingsStore(api), sets };
}

describe("the switch mapping", () => {
  it("round-trips the tri-state", () => {
    expect(switchOf(null)).toBe("auto");
    expect(switchOf(true)).toBe("on");
    expect(switchOf(false)).toBe("off");
    for (const v of [null, true, false]) expect(settingOf(switchOf(v))).toBe(v);
  });

  it("says what Automatic resolves to", () => {
    expect(switchHint(null, { found: true, program: "C:\\tools\\gitleaks.exe", effectiveEnabled: true })).toContain("on, gitleaks was found (C:\\tools\\gitleaks.exe)");
    expect(switchHint(null, { found: false, program: null, effectiveEnabled: false })).toContain("off, gitleaks is not installed");
    expect(switchHint(true, { found: false, program: null, effectiveEnabled: true })).toContain("nothing was scanned");
    expect(switchHint(false, null)).toContain("Off");
    expect(switchHint(null, null)).toBe("Automatic.");
  });
});

describe("SecuritySection", () => {
  it("commits the three settings on change and shows the scanner hint", async () => {
    const { store: s, sets } = store();
    await s.load();
    const info = vi.fn(async () => ({ found: true, program: "/usr/bin/gitleaks", effectiveEnabled: true }));
    const section = new SecuritySection({ settings: s, scannerInfo: info });
    document.body.appendChild(section.element);
    section.activate();
    await Promise.resolve();
    await Promise.resolve();
    expect(info).toHaveBeenCalled();
    const sw = section.element.querySelector<HTMLSelectElement>('[data-setting="secretScanBeforeAgents"]')!;
    const policy = section.element.querySelector<HTMLSelectElement>('[data-setting="secretScanPolicy"]')!;
    const program = section.element.querySelector<HTMLInputElement>('[data-setting="secretScanProgram"]')!;
    expect(sw.value).toBe("auto");
    expect(policy.value).toBe("block-agents");
    expect(section.element.querySelector('[data-role="scanHint"]')!.textContent).toContain("gitleaks was found (/usr/bin/gitleaks)");

    sw.value = "off";
    sw.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(sets.at(-1)!.secretScanBeforeAgents).toBe(false);
    policy.value = "warn";
    policy.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(sets.at(-1)!.secretScanPolicy).toBe("warn");
    program.value = "  C:\\tools\\gitleaks.exe ";
    program.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(sets.at(-1)!.secretScanProgram).toBe("C:\\tools\\gitleaks.exe");
    const config = section.element.querySelector<HTMLInputElement>('[data-setting="secretScanConfig"]')!;
    expect(config.value).toBe("");
    config.value = "  C:\\cfg\\gitleaks.toml ";
    config.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(sets.at(-1)!.secretScanConfig).toBe("C:\\cfg\\gitleaks.toml");
    // The store's value flows back into the controls.
    expect(s.get().secretScanBeforeAgents).toBe(false);
    expect(sw.value).toBe("off");
    section.dispose();
  });
});
