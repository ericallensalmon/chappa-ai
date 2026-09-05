// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Hud, HUD_INTERVAL_MS, PAINT_WINDOW, p95, type DebugStatsDto, type HudData } from "./hud";

function makeHud(data: Partial<HudData> = {}): { container: HTMLDivElement; hud: Hud; el: HTMLElement } {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const hud = new Hud(container, {
    getRemoteStats: async () => null,
    getResyncs: () => 0,
    ...data,
  });
  const el = container.querySelector(".chappa-hud") as HTMLElement;
  return { container, hud, el };
}

describe("p95", () => {
  it("returns 0 for an empty sample", () => {
    expect(p95([])).toBe(0);
  });

  it("is the value the slowest 5% of frames still beat", () => {
    const samples = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
    expect(p95(samples)).toBe(20); // floor(20 * 0.95) = 19 → index 19
    expect(p95([3, 1, 2])).toBe(3); // floor(3 * 0.95) = 2 → max
    expect(p95([1])).toBe(1);
  });
});

describe("Hud", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("overlays the viewport: the inline style actually applies", () => {
    // Regression: HUD_CSS was once a selector-wrapped rule assigned to
    // style.cssText, which drops the garbage-prefixed first declaration —
    // position:absolute — leaving the HUD painted under the terminal.
    const { el, hud } = makeHud();
    expect(el.style.position).toBe("absolute");
    expect(el.style.zIndex).toBe("20");
    expect(el.style.pointerEvents).toBe("none");
    hud.dispose();
  });

  it("starts hidden and toggles on Ctrl+Shift+D", () => {
    const { el, hud } = makeHud();
    expect(hud.isVisible()).toBe(false);
    expect(el.style.display).toBe("none");

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "D", ctrlKey: true, shiftKey: true }));
    expect(hud.isVisible()).toBe(true);
    expect(el.style.display).toBe("block");

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "D", ctrlKey: true, shiftKey: true }));
    expect(hud.isVisible()).toBe(false);
    expect(el.style.display).toBe("none");
    hud.dispose();
  });

  it("ignores autorepeat and non-matching combos", () => {
    const { hud } = makeHud();
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "D", ctrlKey: true, shiftKey: true, repeat: true }),
    );
    expect(hud.isVisible()).toBe(false);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "d", ctrlKey: true }));
    expect(hud.isVisible()).toBe(false);
    hud.dispose();
  });

  it("shows local rates and actor stats on the 1s tick", async () => {
    const remote: DebugStatsDto = {
      framesSent: 12,
      bytesSent: 4_000_000,
      damageRowsLast: 5,
      coalescedTicks: 7,
      outstanding: true,
    };
    const { el, hud } = makeHud({
      getRemoteStats: async () => remote,
      getResyncs: () => 3,
    });
    hud.toggle();

    hud.recordFrame(1_000_000, 2);
    hud.recordFrame(1_000_000, 4);
    hud.recordPaint(1);
    hud.recordPaint(3);

    await vi.advanceTimersByTimeAsync(HUD_INTERVAL_MS);

    const text = el.textContent ?? "";
    expect(text).toContain("2.0 fps");
    expect(text).toContain("2.00 MB/s");
    expect(text).toContain("dmg 3.0 rows/f");
    expect(text).toContain("resyncs 3");
    expect(text).toContain("rAF p95 3.00 ms");
    expect(text).toContain("R 12");
    expect(text).toContain("OUTSTANDING");
    expect(text).toContain("coalesced 7");
    hud.dispose();
  });

  it("keeps only a rolling paint window", () => {
    const { hud } = makeHud();
    hud.toggle();
    for (let i = 0; i < PAINT_WINDOW + 50; i++) hud.recordPaint(i);
    // The oldest 50 samples fell off the window.
    expect((hud as unknown as { paint: number[] }).paint.length).toBe(PAINT_WINDOW);
    hud.dispose();
  });
});
