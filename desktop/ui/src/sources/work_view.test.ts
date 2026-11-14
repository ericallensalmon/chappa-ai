// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { prId } from "./prs_model";
import { fixture, payload } from "./test-utils";
import { WorkView, workMeta, type WorkViewDeps, type WorkViewInput } from "./work_view";
import { buildWorkModel } from "./work_model";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");
const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

/** Rust's judged list for ABC-1, as the walker returns it (item + section). */
const ABC1_ACTIONS: ipc.SourceActionDto[] = [
  { action: { kind: "terminal", id: "terminal-here", label: "Terminal here", cwd: "/srv/example/abc-1", command: ["git", "status"], env: {} }, verdict: { ...ok, enabled: false, reason: "/srv/example/abc-1 is not inside an open project" }, needsPrompt: false },
  { action: { kind: "endpoint", id: "approve", label: "Approve", mutates: true, confirm: "Approve ABC-1?", params: { to: "done" }, note: "optional" }, verdict: ok, needsPrompt: false },
  { action: { kind: "endpoint", id: "ping", label: "Ping", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false },
  { action: { kind: "panel", id: "validate", label: "Validate", endpoint: "/api/panel/validate" }, verdict: ok, needsPrompt: false },
  { action: { kind: "link", id: "open-docs", label: "Open docs", url: "https://registry.example/docs/abc-1" }, verdict: ok, needsPrompt: false },
];

function deps(over: Partial<WorkViewDeps> = {}): WorkViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async (_s: string, _v: string, key: string) => (key === "ABC-1" ? ABC1_ACTIONS : [])),
    run: vi.fn(async () => ({ kind: "done", message: "pong" }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => NOW,
    openView: vi.fn(() => true),
    said,
    ...over,
  };
}

function input(over: Partial<WorkViewInput> = {}): WorkViewInput {
  return {
    payloads: [payload("fx", fixture("work"), { sourceLabel: "Fixture" })],
    needsUpdate: [],
    loadedPrs: new Set(),
    servedBy: ["Fixture"],
    versions: { fx: 1 },
    ...over,
  };
}

const groups = (v: WorkView) => [...v.host.querySelectorAll<HTMLElement>(".chappa-work-list > .chappa-sv-group")].map((g) => g.dataset.group!);
const visibleKeys = (v: WorkView) =>
  [...v.host.querySelectorAll<HTMLElement>(".chappa-work-list > .chappa-sv-group")]
    .filter((g) => !g.querySelector(".chappa-sv-rows")!.classList.contains("folded"))
    .flatMap((g) => [...g.querySelectorAll<HTMLElement>(".chappa-work-row")].map((r) => r.dataset.key!));
const row = (v: WorkView, key: string) => v.host.querySelector<HTMLElement>(`.chappa-work-row[data-key="${key}"]`)!;
const detail = (v: WorkView) => v.host.querySelector<HTMLElement>(".chappa-work-detail")!;
const field = (v: WorkView, name: string) => detail(v).querySelector<HTMLElement>(`.chappa-work-field-value[data-field="${name}"]`)?.textContent;
const assigned = (v: WorkView) => v.host.querySelector<HTMLButtonElement>(".chappa-work-assigned")!;
const search = (v: WorkView) => v.host.querySelector<HTMLInputElement>(".chappa-work-search")!;
const key = (el: HTMLElement, k: string) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true }));

afterEach(() => {
  document.body.textContent = "";
});

describe("WorkView", () => {
  it("the list: freshness, the source's banner, Assigned to me on by default, the active item pinned", () => {
    const v = new WorkView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-back-arrow")?.textContent).toBe("←");
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("Work");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago · refreshing");
    expect([...v.host.querySelectorAll(".chappa-sv-error")].map((e) => e.textContent)).toEqual(["rate limited, showing cached items"]);
    expect(v.host.querySelector(".chappa-work-banners .chappa-banner-title")?.textContent).toBe("Syncing");
    expect(assigned(v).getAttribute("aria-pressed")).toBe("true");
    expect(groups(v)).toEqual(["active"]);
    expect(visibleKeys(v)).toEqual(["ABC-1"]);
    v.dispose();
  });

  it("a row: key, title, status chip, meta, labels, badges, PR chips, its own actions only", async () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input());
    await FLUSH();
    const r = row(v, "ABC-1");
    expect(r.querySelector(".chappa-work-key")?.textContent).toBe("ABC-1");
    expect(r.querySelector(".chappa-sv-title")?.textContent).toBe("Add a health route to api");
    expect(r.querySelector(".chappa-work-status .chappa-sv-chip")?.textContent).toBe("In progress");
    expect(r.querySelector(".chappa-sv-meta")?.textContent).toBe("task · high priority · in ABC-100 · from Sam · touched 33 min ago");
    expect([...r.querySelectorAll(".chappa-sv-chips > *")].map((c) => c.textContent)).toEqual(["backend", "ops", "blocked", "owner/repo#1 open"]);
    expect(d.actions).toHaveBeenCalledWith("fx", "work", "ABC-1");
    // The section's open-docs is not the item's own action.
    expect([...r.querySelectorAll<HTMLButtonElement>(".chappa-sv-action")].map((b) => b.dataset.action)).toEqual(["terminal-here", "approve", "ping", "validate"]);
    expect(r.querySelector<HTMLButtonElement>('[data-action="terminal-here"]')?.title).toBe("/srv/example/abc-1 is not inside an open project");
    v.dispose();
  });

  it("the filters: Assigned to me toggles, the text box narrows, both remembered; empty states say why", () => {
    const v = new WorkView(deps());
    v.update(input());
    assigned(v).click();
    expect(assigned(v).getAttribute("aria-pressed")).toBe("false");
    expect(groups(v)).toEqual(["active", "todo", "done", "other"]);
    // Done is folded by default.
    expect(visibleKeys(v)).toEqual(["ABC-1", "ABC-2", "owner/repo#2"]);
    search(v).value = "rename";
    search(v).dispatchEvent(new Event("input"));
    expect(groups(v)).toEqual(["todo"]);
    // A poll keeps both choices.
    v.update(input({ versions: { fx: 2 } }));
    expect(groups(v)).toEqual(["todo"]);
    search(v).value = "nothing like it";
    search(v).dispatchEvent(new Event("input"));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("No item matches the filter.");
    // Escape in the box clears it and stays on the page.
    const d = deps();
    const w = new WorkView(d);
    w.update(input());
    search(w).value = "abc";
    search(w).dispatchEvent(new Event("input"));
    search(w).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(search(w).value).toBe("");
    expect(d.onClose).not.toHaveBeenCalled();
    v.dispose();
    w.dispose();
  });

  it("the Done group unfolds on its head; the choice survives a poll", () => {
    const v = new WorkView(deps());
    v.update(input());
    assigned(v).click();
    v.host.querySelector<HTMLButtonElement>('.chappa-sv-group[data-group="done"] .chappa-sv-group-head')!.click();
    expect(visibleKeys(v)).toContain("ABC-3");
    v.update(input({ versions: { fx: 2 } }));
    expect(visibleKeys(v)).toContain("ABC-3");
    v.dispose();
  });

  it("empty states: no source, no data yet, nothing assigned", () => {
    const v = new WorkView(deps());
    v.update(input({ payloads: [], servedBy: [] }));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toContain("No trusted source serves work items");
    v.update(input({ payloads: [payload("fx", null)] }));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("Waiting for the first answer from Fixture.");
    v.update(input({ payloads: [payload("fx", { items: [{ key: "A-1", assignedToMe: false }] })] }));
    expect(assigned(v).disabled).toBe(true);
    expect(visibleKeys(v)).toEqual(["A-1"]);
    v.dispose();
  });

  it("selecting a row opens its detail: every field, its actions without the moves, the status block, its sections", async () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input());
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    await FLUSH();
    expect(v.selectedId).toBe("fx|ABC-1");
    expect(row(v, "ABC-1").classList.contains("selected")).toBe(true);
    expect(v.host.querySelector(".chappa-work-split")?.classList.contains("has-detail")).toBe(true);
    expect(detail(v).querySelector(".chappa-work-detail-title")?.textContent).toBe("ABC-1 Add a health route to api");
    expect(field(v, "Key")).toBe("ABC-1");
    expect(field(v, "Status")).toBe("In progress");
    expect(field(v, "Type")).toBe("task");
    expect(field(v, "Priority")).toBe("high");
    expect(field(v, "Labels")).toBe("backendops");
    expect(field(v, "Parent")).toBe("ABC-100 Observability");
    expect(field(v, "Reporter")).toBe("Sam");
    expect(field(v, "Created")).toMatch(/^13 d ago/);
    expect(field(v, "Last touched")).toMatch(/^33 min ago/);
    expect(field(v, "Folder")).toBe("/srv/example/abc-1");
    expect(field(v, "Pull requests")).toBe("owner/repo#1 open");
    expect(field(v, "Link")).toBe("https://registry.example/tracker/ABC-1");
    expect(field(v, "Source")).toBe("Fixture");
    // The detail's buttons skip the moves the status block draws.
    expect([...detail(v).querySelectorAll<HTMLButtonElement>(":scope > .chappa-sv-actions .chappa-sv-action")].map((b) => b.dataset.action)).toEqual([
      "terminal-here",
      "ping",
      "validate",
    ]);
    const ls = detail(v).querySelector<HTMLElement>(".chappa-ls")!;
    expect(ls.hidden).toBe(false);
    expect(ls.querySelector(".chappa-ls-pending-title")?.textContent).toBe("Waiting for approval: move to done");
    expect([...ls.querySelectorAll<HTMLElement>(".chappa-ls-move")].map((m) => m.dataset.move)).toEqual(["approve", "in-progress"]);
    expect(detail(v).querySelector('.chappa-sec[data-section="details"]')).not.toBeNull();
    expect([...detail(v).querySelectorAll<HTMLButtonElement>(".chappa-sec .chappa-sv-action")].map((b) => b.dataset.action)).toEqual(["open-docs"]);
    // The detail's header starts with the shared arrow, then the item.
    const head = detail(v).querySelector(".chappa-work-detail-head")!;
    expect(head.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(head.firstElementChild?.getAttribute("aria-label")).toBe("Back to the Work list");
    expect(head.querySelector(".chappa-work-detail-title")?.textContent).toBe("ABC-1 Add a health route to api");
    // While it is open the view says so (on a narrow window its header then
    // gives way to the detail's, so there is one arrow).
    expect(v.host.classList.contains("detail-open")).toBe(true);
    // Back closes the detail; clicking the selected title again does too.
    detail(v).querySelector<HTMLButtonElement>(".chappa-work-detail-back")!.click();
    expect(v.selectedId).toBeNull();
    expect(v.host.querySelector(".chappa-work-split")?.classList.contains("has-detail")).toBe(false);
    expect(v.host.classList.contains("detail-open")).toBe(false);
    // Escape steps back one level: the detail first, then the view.
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    key(v.host, "Escape");
    expect(v.selectedId).toBeNull();
    expect(d.onClose).not.toHaveBeenCalled();
    key(v.host, "Escape");
    expect(d.onClose).toHaveBeenCalledTimes(1);
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    expect(v.selectedId).toBeNull();
    v.dispose();
  });

  it("an item without a local workflow hides the status block; the detail follows its item and closes when it leaves", () => {
    const v = new WorkView(deps());
    v.update(input());
    assigned(v).click();
    row(v, "owner/repo#2").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    expect(detail(v).querySelector<HTMLElement>(".chappa-ls")!.hidden).toBe(true);
    expect(detail(v).querySelector(".chappa-work-detail-title")?.textContent).toBe("owner/repo#2");
    const body = fixture("work");
    (body.items[2] as Record<string, unknown>).title = "Now titled";
    v.update(input({ payloads: [payload("fx", body, { sourceLabel: "Fixture" })], versions: { fx: 2 } }));
    expect(detail(v).querySelector(".chappa-work-detail-title")?.textContent).toBe("owner/repo#2 Now titled");
    body.items.splice(2, 1);
    v.update(input({ payloads: [payload("fx", body, { sourceLabel: "Fixture" })], versions: { fx: 3 } }));
    expect(v.selectedId).toBeNull();
    v.dispose();
  });

  it("a PR chip opens the PR in the PRs view when it is listed there, else its checked link", () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input());
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-work-pr")!.click();
    expect(d.openView).not.toHaveBeenCalled();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/owner/repo/pull/1");
    v.update(input({ loadedPrs: new Set([prId("github", "owner/repo", "#1")]) }));
    const chip = row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-work-pr")!;
    expect(chip.title).toBe("Show owner/repo#1 in PRs");
    chip.click();
    expect(d.openView).toHaveBeenCalledWith("prs", "github:owner/repo:#1");
    expect(d.openLink).toHaveBeenCalledTimes(1);
    v.dispose();
  });

  it("focus(key) from Needs you: clears the filters hiding it, unfolds its group, opens and marks it", () => {
    const v = new WorkView(deps());
    v.update(input());
    search(v).value = "health";
    search(v).dispatchEvent(new Event("input"));
    // ABC-3 is done (folded), not assigned to me, and filtered out by the text.
    expect(v.focus("ABC-3")).toBe(true);
    expect(search(v).value).toBe("");
    expect(assigned(v).getAttribute("aria-pressed")).toBe("false");
    expect(visibleKeys(v)).toContain("ABC-3");
    expect(row(v, "ABC-3").classList.contains("focused")).toBe(true);
    expect(v.selectedId).toBe("fx|ABC-3");
    expect(v.focus("ABC-404")).toBe(false);
    v.dispose();
  });

  it("a focus asked before the data arrives waits for it; one that never matches is said", () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input({ payloads: [payload("fx", null)] }));
    expect(v.focus("ABC-2")).toBe(true);
    v.update(input());
    expect(v.selectedId).toBe("fx|ABC-2");
    const w = new WorkView(d);
    w.focus("ABC-404");
    w.update(input());
    expect(d.said).toEqual(["ABC-404 is not in the Work list."]);
    v.dispose();
    w.dispose();
  });

  it("a panel action opens the drill-in page over the whole view; a page opened from it stacks; each arrow and Escape step back one", async () => {
    const validate = fixture("panel") as unknown as Record<string, unknown> & { actions: unknown[] };
    const first = { ...validate, actions: [...validate.actions, { kind: "panel", id: "deeper", label: "Deeper", endpoint: "/api/panel/deeper" }] };
    const run = vi.fn(async (_s: string, view: string, _k: string | null, id: string) =>
      (id === "ping"
        ? { kind: "done", message: "pong" }
        : { kind: "panel", payload: view === "work" ? first : { ...validate, title: "Deeper" } }) as ipc.ActionOutcomeDto);
    const actions = vi.fn(async (_s: string, view: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      view.startsWith("panel:")
        ? [
            { action: { kind: "endpoint", id: "ping", label: "Re-run", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false },
            { action: { kind: "panel", id: "deeper", label: "Deeper", endpoint: "/api/panel/deeper" }, verdict: ok, needsPrompt: false },
          ]
        : key === "ABC-1"
          ? [{ action: { kind: "panel", id: "validate", label: "Validate", endpoint: "/api/panel/validate" }, verdict: ok, needsPrompt: false }]
          : []);
    const d = deps({ run, actions });
    const v = new WorkView(d);
    v.setActive(true);
    v.update(input());
    await FLUSH();
    row(v, "ABC-1").querySelector<HTMLButtonElement>('[data-action="validate"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "work", "ABC-1", "validate", false, null);
    const page = v.openPanelPage!;
    expect(page.origin).toEqual({ sourceId: "fx", sourceLabel: "Fixture", label: "Validate", endpoint: "/api/panel/validate" });
    // The page covers the view: one header, the page's, starting with the arrow.
    expect(v.host.classList.contains("covered")).toBe(true);
    const head = page.el.querySelector(".chappa-sv-header")!;
    expect(head.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(head.querySelector(".chappa-page-title")?.textContent).toBe("Validate");
    expect(head.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to Work");
    expect(page.el.querySelector(".chappa-banner-title")?.textContent).toBe("Configuration has drifted");
    // Its own actions are read as the page (no key) and run there.
    await FLUSH();
    expect(actions).toHaveBeenCalledWith("fx", "panel:/api/panel/validate", null);
    page.el.querySelector<HTMLButtonElement>('.chappa-panel-actions [data-action="ping"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenLastCalledWith("fx", "panel:/api/panel/validate", null, "ping", false, null);
    // A panel from the page stacks over it and returns to it.
    page.el.querySelector<HTMLButtonElement>('.chappa-panel-actions [data-action="deeper"]')!.click();
    await FLUSH();
    const deeper = v.openPanelPage!;
    expect(deeper).not.toBe(page);
    expect(deeper.el.querySelector(".chappa-page-title")?.textContent).toBe("Deeper");
    expect(deeper.el.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to Validate");
    expect(page.el.classList.contains("under")).toBe(true);
    key(v.host, "Escape");
    expect(v.openPanelPage).toBe(page);
    expect(page.el.classList.contains("under")).toBe(false);
    page.el.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    expect(v.openPanelPage).toBeNull();
    expect(v.host.classList.contains("covered")).toBe(false);
    expect(v.host.querySelector(".chappa-panel-page")).toBeNull();
    expect(d.onClose).not.toHaveBeenCalled();
    v.dispose();
  });

  it("a poll repaints in place: unchanged rows keep their nodes; another contract is a banner", () => {
    const v = new WorkView(deps());
    v.update(input());
    assigned(v).click();
    const node = row(v, "ABC-2");
    v.update(input({ versions: { fx: 2 } }));
    expect(row(v, "ABC-2")).toBe(node);
    v.update(input({ payloads: [payload("fx", fixture("work"), { sourceLabel: "Fixture" }), payload("b", { contract: 2, items: [] }, { sourceLabel: "B" })] }));
    expect(v.host.querySelector(".chappa-sv-banner")?.textContent).toContain("B needs an update");
    expect(row(v, "ABC-2")).toBe(node);
    v.dispose();
  });

  it("workMeta leaves out what the item does not have", () => {
    const [bare] = buildWorkModel([payload("a", { items: [{ key: "K-1", reporter: { name: "Alex", isMe: true } }] })]).all;
    expect(workMeta(bare, NOW)).toBe("from me");
  });
});
