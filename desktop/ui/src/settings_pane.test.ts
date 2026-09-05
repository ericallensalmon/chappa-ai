// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { SettingsPane } from "./settings_pane";
import { stubSettings } from "./term/test-utils";
import type { Settings } from "./settings";

async function makePane(
  seed: Partial<Settings> = {},
  canonical: (s: Settings) => Settings = (s) => s,
): Promise<{ pane: SettingsPane; harness: ReturnType<typeof stubSettings> }> {
  const harness = stubSettings(seed, canonical);
  await harness.store.load();
  const pane = new SettingsPane({ store: harness.store });
  pane.open();
  return { pane, harness };
}

function control<T extends HTMLElement>(pane: SettingsPane, setting: string): T {
  return pane.element.querySelector<T>(`[data-setting="${setting}"]`)!;
}

describe("SettingsPane — terminal section", () => {
  it("renders the synthetic-prompt-marks toggle OFF on fresh defaults", async () => {
    // Deliberate: the heuristic guesses (an Enter that only cancels a dialog
    // still plants a mark), so nothing may turn it on implicitly.
    const { pane } = await makePane();
    const toggle = control<HTMLInputElement>(pane, "syntheticPromptMarks");
    expect(toggle.checked).toBe(false);
    pane.dispose();
  });

  it("says out loud that synthetic marks guess", async () => {
    const { pane } = await makePane();
    expect(pane.element.textContent).toMatch(/guess/i);
    pane.dispose();
  });

  it("persists a toggle immediately — there is no Save button", async () => {
    const { pane, harness } = await makePane();
    expect(pane.element.textContent).not.toMatch(/\bSave\b/);
    const toggle = control<HTMLInputElement>(pane, "copyOnSelect");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.api.setSettings).toHaveBeenCalledTimes(1);
    expect(harness.saved()!.copyOnSelect).toBe(true);
    pane.dispose();
  });

  it("offers EXACTLY the two bundled faces — no system enumeration", async () => {
    const { pane } = await makePane();
    const select = control<HTMLSelectElement>(pane, "fontFamily");
    expect([...select.options].map((o) => o.value)).toEqual(["Geist Mono", "JetBrains Mono"]);
    pane.dispose();
  });

  it("steps the font size in CSS px and stops ON the bounds", async () => {
    const { pane, harness } = await makePane({ fontSize: 17 });
    const up = pane.element.querySelector<HTMLButtonElement>('[data-action="fontSizeUp"]')!;
    const down = pane.element.querySelector<HTMLButtonElement>('[data-action="fontSizeDown"]')!;
    expect(control<HTMLElement>(pane, "fontSize").textContent).toBe("17 px");
    up.click();
    await Promise.resolve();
    expect(harness.saved()!.fontSize).toBe(18);
    expect(control<HTMLElement>(pane, "fontSize").textContent).toBe("18 px");
    // At the ceiling: the button disables and one more press writes nothing.
    expect(up.disabled).toBe(true);
    expect(down.disabled).toBe(false);
    up.click();
    await Promise.resolve();
    expect(harness.api.setSettings).toHaveBeenCalledTimes(1);
    pane.dispose();
  });

  it("snaps a control back when Rust returns a different canonical value", async () => {
    // The store adopts the RETURNED value; the pane re-syncs from it, so a
    // clamp is visible instead of silently diverging from the terminal.
    const { pane, harness } = await makePane({ fontSize: 17 }, (s) => ({ ...s, fontSize: 12 }));
    pane.element.querySelector<HTMLButtonElement>('[data-action="fontSizeUp"]')!.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(harness.store.get().fontSize).toBe(12);
    expect(control<HTMLElement>(pane, "fontSize").textContent).toBe("12 px");
    pane.dispose();
  });

  it("scroll-speed and line-height pickers write through the segmented rows", async () => {
    const { pane, harness } = await makePane();
    const speed = control<HTMLElement>(pane, "scrollWheelSpeed");
    speed.querySelectorAll<HTMLButtonElement>("button")[5].click(); // "6x"
    await Promise.resolve();
    expect(harness.saved()!.scrollWheelSpeed).toBe(6);
    const lh = control<HTMLElement>(pane, "lineHeight");
    expect([...lh.querySelectorAll("button")].map((b) => b.textContent)).toEqual([
      "1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "1.8",
    ]);
    lh.querySelectorAll<HTMLButtonElement>("button")[5].click(); // 1.5
    await Promise.resolve();
    expect(harness.saved()!.lineHeight).toBe(1.5);
    pane.dispose();
  });
});

describe("SettingsPane — clipboard keys", () => {
  it("renders both toggles ON on fresh defaults (the deliberate upgrade default)", async () => {
    const { pane } = await makePane();
    expect(control<HTMLInputElement>(pane, "ctrlVPastes").checked).toBe(true);
    expect(control<HTMLInputElement>(pane, "ctrlCCopyOnly").checked).toBe(true);
    pane.dispose();
  });

  it("persists each toggle immediately — no Save button", async () => {
    const { pane, harness } = await makePane();
    const v = control<HTMLInputElement>(pane, "ctrlVPastes");
    v.checked = false;
    v.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.api.setSettings).toHaveBeenCalledTimes(1);
    expect(harness.saved()!.ctrlVPastes).toBe(false);
    // The other clipboard key is untouched by the same full-struct write.
    expect(harness.saved()!.ctrlCCopyOnly).toBe(true);

    const c = control<HTMLInputElement>(pane, "ctrlCCopyOnly");
    c.checked = false;
    c.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.saved()!.ctrlCCopyOnly).toBe(false);
    expect(harness.saved()!.ctrlVPastes).toBe(false);
    pane.dispose();
  });

  it("says out loud that Ctrl+C never reaches the terminal (honest description)", async () => {
    const { pane } = await makePane();
    expect(pane.element.textContent).toMatch(/Ctrl\+C never reaches the terminal; stop a process from its rail row/);
    // …and that ^V's TUI consequence is deliberate, not an oversight.
    expect(pane.element.textContent).toMatch(/\^V never reaches a TUI — deliberate/);
    pane.dispose();
  });

  it("re-syncs from a store notification (a Rust canonical value snaps the control)", async () => {
    const { pane, harness } = await makePane();
    expect(control<HTMLInputElement>(pane, "ctrlVPastes").checked).toBe(true);
    // An outside update (another window, a Rust clamp) re-syncs the control.
    await harness.store.update({ ctrlVPastes: false, ctrlCCopyOnly: false });
    expect(control<HTMLInputElement>(pane, "ctrlVPastes").checked).toBe(false);
    expect(control<HTMLInputElement>(pane, "ctrlCCopyOnly").checked).toBe(false);
    pane.dispose();
  });
});

describe("SettingsPane — shell profiles", () => {
  const profiles = [
    { name: "Windows PowerShell", command: "powershell.exe -NoLogo", enabled: true },
    { name: "Git Bash", command: "C:\\Program Files\\Git\\bin\\bash.exe", enabled: false },
  ];

  it("lists every profile with its enable toggle", async () => {
    const { pane } = await makePane({ shellProfiles: profiles });
    const rows = pane.element.querySelectorAll<HTMLElement>("[data-profile]");
    expect([...rows].map((r) => r.dataset.profile)).toEqual(["Windows PowerShell", "Git Bash"]);
    const toggles = pane.element.querySelectorAll<HTMLInputElement>('[data-field="enabled"]');
    expect([...toggles].map((t) => t.checked)).toEqual([true, false]);
    pane.dispose();
  });

  it("an enable toggle rewrites only that profile", async () => {
    const { pane, harness } = await makePane({ shellProfiles: profiles });
    const toggle = pane.element.querySelectorAll<HTMLInputElement>('[data-field="enabled"]')[1];
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.saved()!.shellProfiles).toEqual([
      profiles[0],
      { ...profiles[1], enabled: true },
    ]);
    pane.dispose();
  });

  it("adding a profile requires non-empty fields (the only validation)", async () => {
    const { pane, harness } = await makePane();
    const name = pane.element.querySelector<HTMLInputElement>('[data-field="newProfileName"]')!;
    const command = pane.element.querySelector<HTMLInputElement>('[data-field="newProfileCommand"]')!;
    const add = pane.element.querySelector<HTMLButtonElement>('[data-action="addProfile"]')!;

    add.click(); // both blank
    await Promise.resolve();
    expect(harness.api.setSettings).not.toHaveBeenCalled();

    name.value = "   ";
    command.value = "sh";
    add.click(); // whitespace-only name
    await Promise.resolve();
    expect(harness.api.setSettings).not.toHaveBeenCalled();

    name.value = "Bash";
    command.value = "";
    add.click(); // no command line
    await Promise.resolve();
    expect(harness.api.setSettings).not.toHaveBeenCalled();

    name.value = "Bash";
    command.value = "/bin/bash -l";
    add.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(harness.saved()!.shellProfiles).toEqual([
      { name: "Bash", command: "/bin/bash -l", enabled: true },
    ]);
    // The add fields clear so the next entry starts blank.
    expect(name.value).toBe("");
    expect(command.value).toBe("");
    pane.dispose();
  });

  it("edits commit on `change` (blur/Enter), not per keystroke", async () => {
    const { pane, harness } = await makePane({ shellProfiles: profiles });
    const cmd = pane.element.querySelectorAll<HTMLInputElement>('[data-field="command"]')[0];
    cmd.value = "pwsh.exe -NoLogo";
    cmd.dispatchEvent(new Event("input"));
    expect(harness.api.setSettings).not.toHaveBeenCalled();
    cmd.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.saved()!.shellProfiles[0].command).toBe("pwsh.exe -NoLogo");
    pane.dispose();
  });

  it("an emptied name reverts instead of storing a nameless profile", async () => {
    const { pane, harness } = await makePane({ shellProfiles: profiles });
    const nameInput = pane.element.querySelectorAll<HTMLInputElement>('[data-field="name"]')[0];
    nameInput.value = "";
    nameInput.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.api.setSettings).not.toHaveBeenCalled();
    expect(nameInput.value).toBe("Windows PowerShell");
    pane.dispose();
  });

  it("the execution-profile dropdown offers the builtins plus every profile name", async () => {
    const { pane } = await makePane({ shellProfiles: profiles, defaultExecProfile: "cmd" });
    const select = control<HTMLSelectElement>(pane, "defaultExecProfile");
    expect([...select.options].map((o) => o.value)).toEqual([
      "cmd",
      "sh",
      "Windows PowerShell",
      "Git Bash",
    ]);
    expect(select.value).toBe("cmd");
    pane.dispose();
  });
});

describe("SettingsPane — full-window layout", () => {
  it("covers the whole window instead of docking as a 420px drawer", async () => {
    // The host verdict: the drawer was too narrow and the "Line
    // height" label sat under its picker.
    const { pane } = await makePane();
    const s = pane.element.style;
    expect(s.position).toBe("fixed");
    expect([s.top, s.right, s.bottom, s.left]).toEqual(["0px", "0px", "0px", "0px"]);
    expect(s.width).toBe("");
    pane.dispose();
  });

  it("stretches the content to the full window width (× at the top right)", async () => {
    const { pane } = await makePane();
    const column = pane.element.querySelector<HTMLElement>(".chappa-settings-column")!;
    // Host round 2: no centred max-width column — full-width so the header
    // (and its ×) reaches the screen's top-right corner.
    expect(column.style.maxWidth).toBe("");
    expect(column.style.width).toBe("100%");
    // Header, TERMINAL, SHELL PROFILES and AGENTS all live inside it.
    expect(column.querySelectorAll(".chappa-settings-section").length).toBe(3);
    expect(column.querySelector(".chappa-settings-close")).not.toBeNull();
    pane.dispose();
  });

  it("gives the row label a stable basis so a wide control cannot cover it", async () => {
    const { pane } = await makePane();
    const lh = control<HTMLElement>(pane, "lineHeight");
    const label = lh.parentElement!.querySelector("label")!;
    expect(label.style.flex).toBe("0 0 180px");
    expect(lh.parentElement!.style.flexWrap).toBe("wrap");
    pane.dispose();
  });
});

describe("SettingsPane — dismissal", () => {
  it("Escape closes the pane", async () => {
    const { pane } = await makePane();
    expect(pane.isOpen()).toBe(true);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(pane.isOpen()).toBe(false);
    expect(pane.element.style.display).toBe("none");
    pane.dispose();
  });

  it("the × closes the pane and reports it", async () => {
    const harness = stubSettings();
    await harness.store.load();
    let closes = 0;
    const pane = new SettingsPane({ store: harness.store, onClose: () => (closes += 1) });
    pane.open();
    pane.element.querySelector<HTMLButtonElement>(".chappa-settings-close")!.click();
    expect(pane.isOpen()).toBe(false);
    expect(closes).toBe(1);
    pane.dispose();
  });

  it("a press on empty space INSIDE the view does not close it", async () => {
    // There is no outside-click dismiss any more: a full-window view has no
    // outside, and the old capture-phase listener could only ever have closed
    // the pane mid-gesture.
    const { pane, harness } = await makePane();
    const column = pane.element.querySelector<HTMLElement>(".chappa-settings-column")!;
    column.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    pane.element.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(pane.isOpen()).toBe(true);

    // …and the controls beneath the press still work.
    const toggle = control<HTMLInputElement>(pane, "copyOnSelect");
    toggle.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(harness.api.setSettings).toHaveBeenCalledTimes(1);
    expect(pane.isOpen()).toBe(true);
    pane.dispose();
  });

  it("a press outside the view no longer closes it either", async () => {
    const { pane } = await makePane();
    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(pane.isOpen()).toBe(true);
    pane.dispose();
  });

  it("a closed pane stops listening (no stray Escape handling)", async () => {
    const { pane } = await makePane();
    pane.close();
    const ev = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
    document.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
    pane.dispose();
  });
});
