// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { TimersSection } from "./timers_section";
import { stubSettings } from "./term/test-utils";

const FLUSH = async (): Promise<void> => {
  await Promise.resolve();
  await Promise.resolve();
};

async function section() {
  const settings = stubSettings();
  await settings.store.load();
  const host = document.createElement("div");
  document.body.appendChild(host);
  const el = new TimersSection({ settings: settings.store });
  host.appendChild(el.element);
  el.activate();
  await FLUSH();
  return { settings, host, section: el };
}

describe("Timers settings section", () => {
  // The five timer knobs behave like every other setting — commit on
  // change, adopt the clamped value Rust returns, never a Save button.
  it("the timer settings commit through the settings store and snap back to the clamped value", async () => {
    const { host, settings, section: s } = await section();
    const get = (key: string) => host.querySelector<HTMLInputElement>(`[data-setting="${key}"]`)!;
    for (const [key, initial] of [
      ["idleThresholdMs", "120000"],
      ["timerConfirmMs", "5000"],
      ["timerDeliveryTimeoutMs", "30000"],
      ["timerDedupeMs", "5000"],
      ["timerRetentionHours", "24"],
    ] as const) {
      expect(get(key).value, key).toBe(initial);
    }
    expect(host.textContent).not.toMatch(/\bSave\b/);
    const idle = get("idleThresholdMs");
    idle.value = "1";
    idle.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.idleThresholdMs).toBe(1000);
    expect(idle.value).toBe("1000");
    // 0 is IN range for the two "disable me" knobs and survives.
    const dedupe = get("timerDedupeMs");
    dedupe.value = "0";
    dedupe.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.timerDedupeMs).toBe(0);
    const retention = get("timerRetentionHours");
    retention.value = "99999";
    retention.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.timerRetentionHours).toBe(720);
    expect(retention.value).toBe("720");
    s.dispose();
  });

  it("explains each knob beside it and stops syncing once deactivated", async () => {
    const { host, settings, section: s } = await section();
    expect(host.textContent).toMatch(/silent before an idle timer calls it idle/);
    s.deactivate();
    await settings.store.update({ timerConfirmMs: 9000 });
    expect(host.querySelector<HTMLInputElement>('[data-setting="timerConfirmMs"]')!.value).toBe("5000");
    s.dispose();
  });
});
