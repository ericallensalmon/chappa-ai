// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildPrsModel, prId } from "./prs_model";
import { fixture, overlayFixture, payload, source } from "./test-utils";
import { WORK_COLUMNS, WorkView, workMeta, type WorkViewDeps, type WorkViewInput } from "./work_view";
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

const groups = (v: WorkView) => [...v.host.querySelectorAll<HTMLElement>(".chappa-dt-body > .chappa-dt-group")].map((g) => g.dataset.group!);
const visibleKeys = (v: WorkView) =>
  [...v.host.querySelectorAll<HTMLElement>(".chappa-dt-body > .chappa-dt-group")]
    .filter((g) => !g.querySelector(".chappa-dt-rows")!.classList.contains("folded"))
    .flatMap((g) => [...g.querySelectorAll<HTMLElement>(".chappa-work-row")].map((r) => r.dataset.key!));
const row = (v: WorkView, key: string) => v.host.querySelector<HTMLElement>(`.chappa-work-row[data-key="${key}"]`)!;
const detail = (v: WorkView) => v.host.querySelector<HTMLElement>(".chappa-work-detail")!;
const field = (v: WorkView, name: string) => detail(v).querySelector<HTMLElement>(`.chappa-work-field-value[data-field="${name}"]`)?.textContent;
const assigned = (v: WorkView) => v.host.querySelector<HTMLButtonElement>(".chappa-work-assigned")!;
const search = (v: WorkView) => v.host.querySelector<HTMLInputElement>(".chappa-work-search")!;
const key = (el: HTMLElement, k: string) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true }));
/** Open a row's "⋯" menu and wait for Rust's answers. */
const menuOf = async (v: WorkView, k: string): Promise<HTMLElement> => {
  row(v, k).querySelector<HTMLButtonElement>(".chappa-dt-more")!.click();
  await FLUSH();
  return v.host.querySelector<HTMLElement>(".chappa-dt-menu")!;
};

afterEach(() => {
  document.body.textContent = "";
  localStorage.clear();
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

  it("a row is aligned cells: key with its active chip, title over its meta and chips, status, workflow, branches and PRs, reporter and age", async () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input());
    await FLUSH();
    expect([...v.host.querySelectorAll(".chappa-dt-cols > *")].map((c) => c.textContent)).toEqual(WORK_COLUMNS);
    const r = row(v, "ABC-1");
    expect(r.querySelectorAll(":scope > .chappa-dt-cell").length).toBe(WORK_COLUMNS.length);
    expect(r.querySelector(".chappa-work-key")?.textContent).toBe("ABC-1");
    expect(r.querySelector<HTMLElement>(".chappa-work-active")!.hidden).toBe(false);
    expect(r.dataset.accent).toBe("active");
    expect(r.querySelector(".chappa-sv-title")?.textContent).toBe("Add a health route to api");
    expect(r.querySelector(".chappa-work-status .chappa-sv-chip")?.textContent).toBe("In progress");
    expect(r.querySelector(".chappa-sv-meta")?.textContent).toBe("task · high priority · in ABC-100 · from Sam · touched 33 min ago");
    expect([...r.querySelectorAll(".chappa-sv-chips > *")].map((c) => c.textContent)).toEqual(["backend", "ops", "blocked"]);
    expect([...r.querySelectorAll(".chappa-work-c-branch .chappa-work-pr")].map((c) => c.textContent)).toEqual(["owner/repo#1 open"]);
    expect(r.querySelector(".chappa-work-c-flow .chappa-sv-chip")?.textContent).toBe("In review");
    expect(r.querySelector<HTMLElement>(".chappa-work-c-flow .chappa-sv-chip")?.dataset.tone).toBe("warn");
    expect(r.querySelector(".chappa-work-from-name")?.textContent).toBe("Sam");
    expect(r.querySelector(".chappa-work-c-from .chappa-dt-sub")?.textContent).toBe("33 min");
    // Nothing is asked of Rust until the menu opens; inline is only Open.
    expect(d.actions).not.toHaveBeenCalled();
    expect([...r.querySelectorAll(".chappa-dt-acts button")].map((b) => b.textContent)).toEqual(["Open", "⋯"]);
    const menu = await menuOf(v, "ABC-1");
    expect(d.actions).toHaveBeenCalledWith("fx", "work", "ABC-1");
    // The section's open-docs is not the item's own action, and `approve`
    // is a move of its local status: the status block in the panel draws
    // it, so the menu does not repeat it.
    expect([...menu.querySelectorAll<HTMLButtonElement>(".chappa-sv-action[data-action]")].map((b) => b.dataset.action)).toEqual(["terminal-here", "ping", "validate"]);
    expect(menu.querySelector<HTMLButtonElement>('[data-action="terminal-here"]')?.title).toBe("/srv/example/abc-1 is not inside an open project");
    expect(menu.querySelector(".chappa-dt-menu-entry")?.textContent).toBe("Open details");
    v.dispose();
  });

  it("Open opens the item's link through the checked opener; a click elsewhere on the row opens its page", () => {
    const d = deps();
    const v = new WorkView(d);
    v.update(input());
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-dt-open")!.click();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/tracker/ABC-1");
    expect(v.selectedId).toBeNull();
    row(v, "ABC-1").querySelector<HTMLElement>(".chappa-work-c-status")!.click();
    expect(v.selectedId).toBe("fx|ABC-1");
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

  it("the Done group unfolds on its compact head; the choice survives a poll and a new page", () => {
    const v = new WorkView(deps());
    v.update(input());
    assigned(v).click();
    const head = () => v.host.querySelector<HTMLButtonElement>('.chappa-dt-group[data-group="done"] .chappa-dt-group-head')!;
    expect(head().textContent).toBe("▸ Done1");
    head().click();
    expect(visibleKeys(v)).toContain("ABC-3");
    v.update(input({ versions: { fx: 2 } }));
    expect(visibleKeys(v)).toContain("ABC-3");
    v.dispose();
    const w = new WorkView(deps());
    w.update(input());
    assigned(w).click();
    expect(visibleKeys(w)).toContain("ABC-3");
    w.dispose();
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
    (await menuOf(v, "ABC-1")).querySelector<HTMLButtonElement>('[data-action="validate"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "work", "ABC-1", "validate", false, null);
    // An action chosen from the menu closes it.
    expect(v.host.querySelector(".chappa-dt-menu")).toBeNull();
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

describe("WorkView with an item fed by two sources", () => {
  const endpoint = (id: string, label: string, confirm = `${label}?`): ipc.SourceActionDto => ({
    action: { kind: "endpoint", id, label, mutates: true, confirm, params: null, note: null },
    verdict: ok,
    needsPrompt: false,
  });
  const listing = (): ipc.SourceDto[] => [
    source("fx", ["work", "prs"], { label: "Fixture", manifestId: "fixture" }),
    source("ws", ["work"], {
      label: "Workspaces",
      manifestId: "workspaces",
      trust: {
        id: "workspaces",
        label: "Workspaces",
        linkHosts: [],
        roots: [],
        views: ["work"],
        hash: "h",
        itemActions: [{ id: "start-work", label: "Start work", view: "work", sources: ["fixture"], mutates: true, confirm: "Create a workspace for {key} — {title}" }],
      },
    }),
  ];
  /** Rust's answers: the overlay's own actions, its banner's, and the item
   *  action with its confirm filled from the row. */
  const pairDeps = () =>
    deps({
      actions: vi.fn(async (sourceId: string, _view: string, key: string | null, owner?: string) => {
        if (owner) return key === "ABC-3" ? [endpoint("start-work", "Start work", "Create a workspace for ABC-3 — Retire the old queue")] : [];
        if (sourceId === "ws" && key === "ABC-2") return [endpoint("ship", "Ship"), endpoint("archive", "Archive"), endpoint("rebuild", "Rebuild")];
        if (sourceId === "fx" && key === "ABC-1") return ABC1_ACTIONS;
        return [];
      }),
    });
  const pairInput = (over: Partial<WorkViewInput> = {}): WorkViewInput =>
    input({
      payloads: [payload("fx", fixture("work"), { sourceLabel: "Fixture" }), payload("ws", overlayFixture("work"), { sourceLabel: "Workspaces" })],
      servedBy: ["Fixture", "Workspaces"],
      versions: { fx: 1, ws: 1 },
      sources: listing(),
      ...over,
    });
  const showAll = (v: WorkView) => {
    if (assigned(v).getAttribute("aria-pressed") === "true") assigned(v).click();
  };
  const open = async (v: WorkView, k: string) => {
    showAll(v);
    row(v, k).querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    await FLUSH();
  };
  const buttons = (el: Element | null) =>
    [...(el?.querySelectorAll<HTMLButtonElement>(".chappa-sv-action") ?? [])].map((b) => [b.dataset.source, b.dataset.action, b.dataset.owner ?? null]);

  it("one row per item: a row with banners carries a dot of their worst tone, each source's actions run under its own key", async () => {
    const v = new WorkView(pairDeps());
    v.update(pairInput());
    showAll(v);
    expect(v.host.querySelectorAll('.chappa-work-row[data-key="ABC-2"]').length).toBe(1);
    const dot = row(v, "ABC-2").querySelector<HTMLElement>(".chappa-work-dot")!;
    expect([dot.hidden, dot.dataset.tone, dot.title]).toEqual([false, "warn", "ABC-2: the stack runs older commits than its branch"]);
    expect(row(v, "ABC-3").querySelector<HTMLElement>(".chappa-work-dot")!.hidden).toBe(true);
    // The overlay's badges join the tracker's row; owner-missing rows say why.
    expect([...row(v, "ABC-2").querySelectorAll(".chappa-sv-chips > *")].map((c) => c.textContent)).toEqual(["1 unpushed"]);
    expect(row(v, "ABC-2").querySelector(".chappa-work-branches")?.textContent).toBe("owner/repo: task/ABC-2-web");
    expect([...row(v, "ABC-9").querySelectorAll(".chappa-sv-chips > *")].map((c) => c.textContent)).toEqual(["not listed by Fixture"]);
    expect([...row(v, "XYZ-1").querySelectorAll(".chappa-sv-chips > *")].map((c) => c.textContent)).toEqual(["tracker unavailable"]);
    await FLUSH();
    const menuButtons = async (k: string) => {
      const m = await menuOf(v, k);
      const out = buttons(m.querySelector(".chappa-dt-menu > .chappa-sv-actions:not(.chappa-dt-menu-own)"));
      v.host.querySelector<HTMLButtonElement>('.chappa-dt-more[aria-expanded="true"]')?.click();
      return out;
    };
    // `ship` is a move of the status the overlay set: its status block in
    // the panel draws it, so the menu does not.
    expect(await menuButtons("ABC-2")).toEqual([["ws", "archive", null]]);
    // Another source's offer, asked for under the owner, only where it does not add to the row yet.
    expect(await menuButtons("ABC-3")).toEqual([["ws", "start-work", "fx"]]);
    expect(await menuButtons("ABC-9")).toEqual([]);
    v.dispose();
  });

  it("the item panel: banners on top, then each source's actions and sections under its name, the status from the source that set it", async () => {
    const v = new WorkView(pairDeps());
    v.update(pairInput());
    await open(v, "ABC-2");
    const d = detail(v);
    expect(d.querySelector(".chappa-work-detail-banners .chappa-banner-title")?.textContent).toBe("ABC-2: the stack runs older commits than its branch");
    expect(buttons(d.querySelector(".chappa-work-detail-banners"))).toEqual([["ws", "rebuild", null]]);
    expect(field(v, "Folder")).toBe("/srv/example/abc-2 (from Workspaces)");
    expect(field(v, "Branches")).toBe("owner/repo: task/ABC-2-web");
    expect(field(v, "Source")).toBe("Fixture, Workspaces");
    expect(field(v, "Status")).toBe("To do");
    // The tracker's part (no actions on ABC-2), then the workspace source's.
    const from = d.querySelector<HTMLElement>(":scope > .chappa-work-from")!;
    expect([from.hidden, from.textContent]).toEqual([false, "Fixture"]);
    const extra = d.querySelector<HTMLElement>('.chappa-work-extra[data-source="ws"]')!;
    expect([extra.dataset.role, extra.querySelector(".chappa-work-from")?.textContent]).toEqual(["contributor", "Workspaces"]);
    // Its move "Ship" is drawn by the status block, not again among its buttons.
    expect(buttons(extra.querySelector(":scope > .chappa-sv-actions"))).toEqual([["ws", "archive", null]]);
    expect(extra.querySelector('.chappa-sec[data-section="repos"]')).not.toBeNull();
    const ls = d.querySelector<HTMLElement>(".chappa-ls")!;
    expect(ls.hidden).toBe(false);
    expect([...ls.querySelectorAll<HTMLElement>(".chappa-ls-move")].map((m) => m.dataset.move)).toEqual(["ship"]);
    v.dispose();
  });

  it("another source's offer sits under its name and is asked for with the row's owner", async () => {
    const dd = pairDeps();
    const v = new WorkView(dd);
    v.update(pairInput());
    await open(v, "ABC-3");
    const offer = detail(v).querySelector<HTMLElement>('.chappa-work-extra[data-role="offers"]')!;
    expect(offer.querySelector(".chappa-work-from")?.textContent).toBe("Workspaces");
    expect(dd.actions).toHaveBeenCalledWith("ws", "work", "ABC-3", "fx");
    expect(buttons(offer)).toEqual([["ws", "start-work", "fx"]]);
    v.dispose();
  });

  it("a lone row keeps the plain layout: no source names, no extra blocks", async () => {
    const v = new WorkView(deps());
    v.update(input({ sources: [source("fx", ["work"], { label: "Fixture", manifestId: "fixture" })] }));
    await open(v, "ABC-1");
    expect(detail(v).querySelector<HTMLElement>(":scope > .chappa-work-from")!.hidden).toBe(true);
    expect(detail(v).querySelectorAll(".chappa-work-extra").length).toBe(0);
    v.dispose();
  });

  it("PRs joined by branch or key show on the row saying how, and open in the PRs view", () => {
    const prs = buildPrsModel([payload("fx", fixture("prs"), { sourceLabel: "Fixture" })]);
    const seen = new Set<string>();
    const prRows = [...prs.mine, ...prs.reviewing, ...prs.all].flatMap((g) => g.rows).filter((r) => !seen.has(r.id) && !!seen.add(r.id));
    const d = pairDeps();
    const v = new WorkView(d);
    v.update(pairInput({ prRows, loadedPrs: new Set(prRows.map((r) => r.id)) }));
    showAll(v);
    const chips = (k: string) => [...row(v, k).querySelectorAll<HTMLButtonElement>(".chappa-work-pr")].map((c) => [c.textContent, c.dataset.how]);
    expect(chips("ABC-1")).toEqual([["owner/repo#1 open", "linked"]]);
    // The PR's branch carries refs/heads/ and the overlay's does not: one branch still.
    expect(chips("ABC-2")).toEqual([["owner/repo#3 open · branch", "branch"]]);
    expect(chips("ABC-3")).toEqual([["owner/repo#5 merged · key in branch", "key in branch"]]);
    // `ABC-12-other` is not ABC-1.
    expect(chips("owner/repo#2")).toEqual([]);
    const chip = row(v, "ABC-3").querySelector<HTMLButtonElement>(".chappa-work-pr")!;
    expect(chip.title).toBe("Show owner/repo#5 in PRs (matched by key in branch)");
    chip.click();
    expect(d.openView).toHaveBeenCalledWith("prs", "github:owner/repo:#5");
    v.dispose();
  });

  it("focus by row id picks one of two rows sharing a key", () => {
    const v = new WorkView(deps());
    v.update(input({ payloads: [payload("a", { items: [{ key: "K-1" }] }, { sourceLabel: "A" }), payload("b", { items: [{ key: "K-1" }] }, { sourceLabel: "B" })] }));
    expect(v.focus("b|K-1")).toBe(true);
    expect(v.selectedId).toBe("b|K-1");
    v.dispose();
  });
  it("data safety: the covered item wears the bad badge, its panel a banner to Environment, its agent button the reason, disabled", async () => {
    const reason = "agents blocked — data safety on app/0: db: 1 finding (card-number ×1) — acknowledge in Environment";
    const agentHere: ipc.SourceActionDto = {
      action: { kind: "agent", id: "agent-here", label: "Agent here", cwd: "/srv/example/abc-1", prompt: "Go", tool: null, title: null, env: {} },
      verdict: { ...ok, enabled: false, reason },
      needsPrompt: false,
    };
    const d = deps({ actions: vi.fn(async (_s: string, _v: string, key: string) => (key === "ABC-1" ? [agentHere, ...ABC1_ACTIONS] : [])) });
    const v = new WorkView(d);
    const block: ipc.DataSafetyBlockDto = {
      sourceId: "st",
      sourceLabel: "Stack",
      instance: "app/0",
      state: "not-clean",
      current: true,
      reason: "db: 1 finding (card-number ×1) — acknowledge in Environment",
      folders: ["/srv/example/abc-1"],
      bound: null,
      component: "db",
      ackActionId: "data-safety-ack",
    };
    v.update(input({ dataSafety: [block] }));
    const chip = [...row(v, "ABC-1").querySelectorAll<HTMLElement>(".chappa-sv-chip")].find((c) => c.textContent === "agents blocked — data safety")!;
    expect(chip).toBeTruthy();
    expect(chip.dataset.tone).toBe("bad");
    expect(chip.title).toBe(block.reason);
    // Items outside the covered folder are not marked.
    const marked = [...v.host.querySelectorAll<HTMLElement>(".chappa-work-row .chappa-sv-chip")].filter((c) => c.textContent === "agents blocked — data safety");
    expect(marked.length).toBe(1);
    row(v, "ABC-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    await FLUSH();
    await FLUSH();
    const banner = detail(v).querySelector<HTMLElement>('.chappa-work-detail-safety .chappa-banner[data-banner="data-safety"]')!;
    expect(banner.dataset.tone).toBe("bad");
    expect(banner.querySelector(".chappa-banner-title")?.textContent).toBe("Agents are blocked here: the data on app/0 may be real");
    expect(banner.textContent).toContain(block.reason);
    banner.querySelector<HTMLButtonElement>(".chappa-work-safety-open")!.click();
    expect(d.openView).toHaveBeenCalledWith("environment", "db");
    const btn = [...detail(v).querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Agent here")!;
    expect(btn.disabled).toBe(true);
    expect(btn.title).toBe(reason);
    // The block lifts: badge and banner go.
    v.update(input({ dataSafety: [] }));
    expect([...row(v, "ABC-1").querySelectorAll(".chappa-sv-chip")].some((c) => c.textContent === "agents blocked — data safety")).toBe(false);
    expect(detail(v).querySelector(".chappa-work-detail-safety .chappa-banner")).toBeNull();
    v.dispose();
  });
});
