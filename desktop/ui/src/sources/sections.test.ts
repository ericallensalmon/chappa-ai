// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { SectionsBlock, bannerEl, paintBanners, readBanners, readSection, readSections, type SectionsContext } from "./sections";
import { fixture } from "./test-utils";
import type { SourceViewDeps } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

function deps(over: Partial<SourceViewDeps> = {}): SourceViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async () => []),
    run: vi.fn(async () => ({ kind: "done", message: "pong" }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => Date.parse("2026-01-15T10:03:00Z"),
    said,
    ...over,
  };
}

const details = () => readSections(fixture("work").items[0].sections);
const target = { sourceId: "fx", sourceLabel: "Fixture", view: "work", key: "ABC-1" };
const ctx = (over: Partial<SectionsContext> = {}): SectionsContext => ({ sourceId: "fx", target, version: "fx:1", ...over });

describe("reading sections and banners", () => {
  it("reads the fixture's section as sent", () => {
    expect(details()).toEqual([
      {
        id: "details",
        title: "Details",
        collapsed: false,
        rows: [
          { label: "Branch", value: "abc-1-health" },
          { label: "Docs", links: [{ label: "Spec", url: "https://registry.example/docs/abc-1" }], badges: [{ text: "draft", tone: "muted" }] },
        ],
        more: { label: "Show history", rows: [{ label: "Created", value: "2026-01-02" }] },
        actions: [{ kind: "link", id: "open-docs", label: "Open docs", url: "https://registry.example/docs/abc-1" }],
      },
    ]);
  });

  it("is lenient: id and title required, label-less rows and url-less links dropped, empty more dropped, first of a repeated id kept", () => {
    expect(readSection({ id: "x" })).toBeNull();
    expect(readSection({ title: "x" })).toBeNull();
    expect(readSection("x")).toBeNull();
    expect(readSections([{ id: "a", title: "A", rows: [{ label: "L", value: 3, links: [{ label: "no url" }, { url: "https://registry.example/x" }] }], more: { label: "More", rows: [] } }, { id: "a", title: "Again" }, null])).toEqual([
      { id: "a", title: "A", collapsed: false, rows: [{ label: "L", links: [{ label: "https://registry.example/x", url: "https://registry.example/x" }] }] },
    ]);
    expect(readSections("nope")).toEqual([]);
  });

  it("banners: id and title required, unknown tone is info, lines kept as text", () => {
    expect(readBanners([{ id: "a", tone: "bad", title: "A", lines: ["one", 2, "[two](https://x.example)"] }, { id: "b", tone: "loud", title: "B" }, { title: "no id" }])).toEqual([
      { id: "a", tone: "bad", title: "A", lines: ["one", "two"] },
      { id: "b", tone: "info", title: "B", lines: [] },
    ]);
  });
});

describe("SectionsBlock", () => {
  it("draws title, label/value rows, links through the checked opener, badges, the folded more rows", async () => {
    const d = deps();
    const b = new SectionsBlock(d);
    b.update(details(), ctx());
    const sec = b.el.querySelector<HTMLElement>('.chappa-sec[data-section="details"]')!;
    expect(sec.querySelector(".chappa-sec-head")?.textContent).toBe("▾ Details");
    const rows = [...sec.querySelectorAll<HTMLElement>(".chappa-sec-body > .chappa-sec-row")];
    expect(rows.map((r) => r.querySelector(".chappa-sec-label")?.textContent)).toEqual(["Branch", "Docs"]);
    expect(rows[0].querySelector(".chappa-sec-value")?.textContent).toBe("abc-1-health");
    expect(rows[1].querySelector(".chappa-sv-chip")?.textContent).toBe("draft");
    rows[1].querySelector<HTMLButtonElement>(".chappa-sec-link")!.click();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/docs/abc-1");
    const more = sec.querySelector<HTMLButtonElement>(".chappa-sec-more")!;
    expect(more.textContent).toBe("▸ Show history");
    expect(sec.querySelector(".chappa-sec-more-rows")?.classList.contains("folded")).toBe(true);
    more.click();
    expect(sec.querySelector(".chappa-sec-more")?.textContent).toBe("▾ Show history");
    expect(sec.querySelector(".chappa-sec-more-rows")?.classList.contains("folded")).toBe(false);
    expect(sec.querySelector(".chappa-sec-more-rows .chappa-sec-value")?.textContent).toBe("2026-01-02");
    await FLUSH();
  });

  it("collapses on its head, starts collapsed when the source says so, and keeps the choice across updates", () => {
    const b = new SectionsBlock(deps());
    const s = [{ id: "a", title: "A", collapsed: true, rows: [{ label: "L" }] }];
    b.update(s, ctx());
    const sec = () => b.el.querySelector<HTMLElement>('.chappa-sec[data-section="a"]')!;
    expect(sec().classList.contains("collapsed")).toBe(true);
    expect(sec().querySelector(".chappa-sec-head")?.getAttribute("aria-expanded")).toBe("false");
    sec().querySelector<HTMLButtonElement>(".chappa-sec-head")!.click();
    expect(sec().classList.contains("collapsed")).toBe(false);
    const node = sec();
    b.update([{ ...s[0], rows: [{ label: "L2" }] }], ctx({ version: "fx:2" }));
    expect(sec()).toBe(node);
    expect(sec().classList.contains("collapsed")).toBe(false);
    expect(sec().querySelector(".chappa-sec-label")?.textContent).toBe("L2");
    b.update([], ctx());
    expect(b.el.children.length).toBe(0);
  });

  it("section actions: only the section's own ids out of the item's judged list, run against the item", async () => {
    const actions = vi.fn(async (): Promise<ipc.SourceActionDto[]> => [
      { action: { kind: "endpoint", id: "ping", label: "Ping", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false },
      { action: { kind: "link", id: "open-docs", label: "Open docs", url: "https://registry.example/docs/abc-1" }, verdict: ok, needsPrompt: false },
    ]);
    const d = deps({ actions });
    const b = new SectionsBlock(d);
    b.update(details(), ctx());
    await FLUSH();
    expect(actions).toHaveBeenCalledWith("fx", "work", "ABC-1");
    const buttons = [...b.el.querySelectorAll<HTMLButtonElement>(".chappa-sv-action")];
    expect(buttons.map((x) => x.dataset.action)).toEqual(["open-docs"]);
    buttons[0].click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "work", "ABC-1", "open-docs", false, null);
    // The same version does not ask again.
    b.update(details(), ctx());
    expect(actions).toHaveBeenCalledTimes(1);
  });

  it("without a target, a section's actions are drawn disabled with the reason and never ask Rust", () => {
    const d = deps();
    const b = new SectionsBlock(d);
    b.update(details(), ctx({ target: null, reason: "not here" }));
    const btn = b.el.querySelector<HTMLButtonElement>(".chappa-sv-action")!;
    expect([btn.textContent, btn.disabled, btn.title]).toEqual(["Open docs", true, "not here"]);
    expect(d.actions).not.toHaveBeenCalled();
  });
});

describe("banners", () => {
  it("tone, title, lines; actions judged under the banner's id", async () => {
    const actions = vi.fn(async (): Promise<ipc.SourceActionDto[]> => [
      { action: { kind: "endpoint", id: "resync", label: "Resync", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false },
    ]);
    const d = deps({ actions });
    const [banner] = readBanners([{ id: "sync", tone: "warn", title: "Syncing", lines: ["a minute old"], actions: [{ kind: "endpoint", id: "resync", label: "Resync", mutates: false }] }]);
    const el = bannerEl(d, banner, ctx({ target: { ...target, key: "sync" } }));
    expect(el.dataset.tone).toBe("warn");
    expect(el.querySelector(".chappa-banner-title")?.textContent).toBe("Syncing");
    expect(el.querySelector(".chappa-banner-line")?.textContent).toBe("a minute old");
    await FLUSH();
    expect(actions).toHaveBeenCalledWith("fx", "work", "sync");
    expect(el.querySelector<HTMLButtonElement>(".chappa-sv-action")?.textContent).toBe("Resync");
  });

  it("paintBanners redraws only when they changed", () => {
    const parent = document.createElement("div");
    const [banner] = readBanners([{ id: "a", tone: "info", title: "A" }]);
    paintBanners(parent, deps(), [{ banner, ctx: ctx() }]);
    const first = parent.firstElementChild;
    paintBanners(parent, deps(), [{ banner, ctx: ctx() }]);
    expect(parent.firstElementChild).toBe(first);
    paintBanners(parent, deps(), []);
    expect(parent.children.length).toBe(0);
  });
});
