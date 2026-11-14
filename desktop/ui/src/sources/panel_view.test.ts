// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { PANEL_ACTIONS_REASON, PanelPage, readPanel } from "./panel_view";
import { fixture } from "./test-utils";
import type { SourceViewDeps } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");
const origin = { sourceId: "fx", sourceLabel: "Fixture", label: "Validate", endpoint: "/api/panel/validate" };

function deps(over: Partial<SourceViewDeps> = {}): SourceViewDeps {
  return {
    actions: vi.fn(async () => []),
    run: vi.fn(async () => ({ kind: "done", message: null }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: vi.fn(),
    now: () => NOW,
    ...over,
  };
}

const panelBody = () => fixture("panel");

afterEach(() => vi.useRealTimers());

describe("readPanel", () => {
  it("reads the fixture page; the action's label stands in for a missing title", () => {
    const p = readPanel(panelBody(), "Validate");
    expect(p.title).toBe("Validate");
    expect(p.freshness.asOf).toBe("2026-01-15T10:00:00Z");
    expect(p.banners.map((b) => [b.id, b.tone, b.title, b.lines])).toEqual([["drift", "warn", "Configuration has drifted", ["2 files differ from the template"]]]);
    expect(p.sections.map((s) => s.id)).toEqual(["checks"]);
    expect(p.actions.map((a) => a.id)).toEqual(["ping"]);
    expect(p.wrongContract).toBe(false);
    expect(readPanel({}, "Fallback").title).toBe("Fallback");
    expect(readPanel(null, "Fallback").sections).toEqual([]);
    expect(readPanel({ contract: 2 }, "x").wrongContract).toBe(true);
  });
});

describe("PanelPage", () => {
  it("a back arrow before the title, freshness, banners and sections through the shared renderer, actions disabled with why", () => {
    const onBack = vi.fn();
    const p = new PanelPage(deps(), origin, panelBody(), onBack, 0);
    const back = p.el.querySelector<HTMLButtonElement>(".chappa-panel-back")!;
    expect(back.textContent).toBe("←Validate");
    expect(p.el.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect(p.el.querySelector(".chappa-banner-title")?.textContent).toBe("Configuration has drifted");
    expect(p.el.querySelector(".chappa-banner")?.getAttribute("data-tone")).toBe("warn");
    expect(p.el.querySelector('.chappa-sec[data-section="checks"] .chappa-sec-head')?.textContent).toBe("▾ Checks");
    expect([...p.el.querySelectorAll(".chappa-sec-body > .chappa-sec-row .chappa-sec-label")].map((l) => l.textContent)).toEqual(["Lint", "Types"]);
    expect(p.el.querySelector(".chappa-sec-more")?.textContent).toBe("▸ Show skipped");
    const action = p.el.querySelector<HTMLButtonElement>(".chappa-panel-actions .chappa-sv-action")!;
    expect([action.textContent, action.disabled, action.title]).toEqual(["Re-run", true, PANEL_ACTIONS_REASON]);
    back.click();
    expect(onBack).toHaveBeenCalledTimes(1);
    p.dispose();
  });

  it("re-polls while open, keeps the page on a failed refresh and says so, stops when disposed", async () => {
    vi.useFakeTimers();
    let n = 0;
    const panel = vi.fn(async () => {
      n++;
      if (n === 2) throw new Error("source is down");
      return { ...panelBody(), title: `Validate ${n}` };
    });
    const p = new PanelPage(deps({ panel }), origin, panelBody(), () => {}, 1000);
    await vi.advanceTimersByTimeAsync(1000);
    expect(panel).toHaveBeenCalledWith("fx", "/api/panel/validate");
    expect(p.el.querySelector(".chappa-panel-title")?.textContent).toBe("Validate 1");
    await vi.advanceTimersByTimeAsync(1000);
    expect(p.el.querySelector(".chappa-panel-title")?.textContent).toBe("Validate 1");
    expect(p.el.querySelector(".chappa-sv-error")?.textContent).toBe("Could not refresh this page: source is down");
    await vi.advanceTimersByTimeAsync(1000);
    expect(p.el.querySelector(".chappa-panel-title")?.textContent).toBe("Validate 3");
    expect(p.el.querySelector(".chappa-sv-error")).toBeNull();
    p.dispose();
    await vi.advanceTimersByTimeAsync(5000);
    expect(panel).toHaveBeenCalledTimes(3);
  });

  it("a page in another contract version says so and shows nothing of it", async () => {
    const p = new PanelPage(deps(), origin, { contract: 2, title: "X", sections: panelBody().sections }, () => {}, 0);
    expect(p.el.querySelector(".chappa-sv-error")?.textContent).toContain("Fixture needs an update");
    expect(p.el.querySelector(".chappa-sec")).toBeNull();
    await FLUSH();
    p.dispose();
  });
});
