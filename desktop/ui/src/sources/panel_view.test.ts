// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { PanelPage, openPanelIn, panelView, readPanel, withPanels } from "./panel_view";
import { fixture } from "./test-utils";
import { ViewFrame, type SourceViewDeps } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");
const origin = { sourceId: "fx", sourceLabel: "Fixture", label: "Validate", endpoint: "/api/panel/validate" };
const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };
const ping = (label: string): ipc.SourceActionDto => ({
  action: { kind: "endpoint", id: "ping", label, mutates: false, confirm: null, params: null, note: null },
  verdict: ok,
  needsPrompt: false,
});

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
  it("a back arrow before the title, freshness, banners and sections through the shared renderer, its own actions live", async () => {
    const onBack = vi.fn();
    // Rust lists the page's own action and (same id) its banner's once each.
    const actions = vi.fn(async () => [ping("Re-run"), ping("Re-run")]);
    const run = vi.fn(async () => ({ kind: "done", message: "pong" }) as ipc.ActionOutcomeDto);
    const d = deps({ actions, run });
    const p = new PanelPage(d, origin, panelBody(), onBack, 0);
    const head = p.el.querySelector(".chappa-sv-header")!;
    const back = head.firstElementChild as HTMLButtonElement;
    expect(back.classList.contains("chappa-back-arrow")).toBe(true);
    expect(back.textContent).toBe("←");
    expect(back.getAttribute("aria-label")).toBe("Back to Fixture");
    expect(back.nextElementSibling?.textContent).toBe("Validate");
    p.setBackTo("Work");
    expect(back.getAttribute("aria-label")).toBe("Back to Work");
    expect(p.title).toBe("Validate");
    expect(p.el.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect(p.el.querySelector(".chappa-banner-title")?.textContent).toBe("Configuration has drifted");
    expect(p.el.querySelector(".chappa-banner")?.getAttribute("data-tone")).toBe("warn");
    expect(p.el.querySelector('.chappa-sec[data-section="checks"] .chappa-sec-head')?.textContent).toBe("▾ Checks");
    expect([...p.el.querySelectorAll(".chappa-sec-body > .chappa-sec-row .chappa-sec-label")].map((l) => l.textContent)).toEqual(["Lint", "Types"]);
    expect(p.el.querySelector(".chappa-sec-more")?.textContent).toBe("▸ Show skipped");
    await FLUSH();
    // Read as the page itself: the `panel:<endpoint>` view, no key.
    expect(actions).toHaveBeenCalledWith("fx", panelView("/api/panel/validate"), null);
    const buttons = [...p.el.querySelectorAll<HTMLButtonElement>(".chappa-panel-actions .chappa-sv-action")];
    expect(buttons.map((b) => [b.textContent, b.disabled])).toEqual([["Re-run", false]]);
    buttons[0].click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "panel:/api/panel/validate", null, "ping", false, null);
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

  it("pages stack in the frame that opened the first: each back steps one page, Escape too; leaving the view keeps them, and a page whose source left is dropped on return", async () => {
    const onClose = vi.fn();
    const frame = new ViewFrame("Work", "work", onClose);
    document.body.appendChild(frame.host);
    const d = withPanels(() => frame, deps({ onClose }));
    const first = openPanelIn(frame, d, origin, panelBody(), 0);
    expect(frame.host.classList.contains("covered")).toBe(true);
    expect(first.el.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to Work");
    d.openPanel!({ ...origin, label: "Checks", endpoint: "/api/panel/checks" }, { title: "Checks for #2" });
    expect(frame.subpages.length).toBe(2);
    const second = frame.subpages[1] as PanelPage;
    expect(second.el.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to Validate");
    expect(first.el.classList.contains("under")).toBe(true);
    second.el.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    expect(frame.subpages).toEqual([first]);
    frame.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(frame.subpages).toEqual([]);
    expect(frame.host.classList.contains("covered")).toBe(false);
    expect(onClose).not.toHaveBeenCalled();
    frame.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(onClose).toHaveBeenCalledTimes(1);
    const kept = openPanelIn(frame, d, origin, panelBody(), 0);
    frame.setActive(false);
    expect(frame.subpages).toEqual([kept]);
    frame.setActive(true);
    frame.dropStale(() => true);
    expect(frame.subpages).toEqual([kept]);
    frame.dropStale((id) => id !== origin.sourceId);
    expect(frame.subpages).toEqual([]);
    expect(frame.host.querySelector(".chappa-panel-page")).toBeNull();
    frame.dispose();
  });

  it("a page in another contract version says so and shows nothing of it", async () => {
    const p = new PanelPage(deps(), origin, { contract: 2, title: "X", sections: panelBody().sections }, () => {}, 0);
    expect(p.el.querySelector(".chappa-sv-error")?.textContent).toContain("Fixture needs an update");
    expect(p.el.querySelector(".chappa-sec")).toBeNull();
    await FLUSH();
    p.dispose();
  });
});
