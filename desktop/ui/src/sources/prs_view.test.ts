// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildPrsModel } from "./prs_model";
import { PR_COLUMNS, PrsView, prGroupFoldKey, prsEmptyText, type PrsViewInput } from "./prs_view";
import { fixture, payload, pr, prsBody } from "./test-utils";
import type { PrsViewDeps } from "./prs_view";
import { source } from "./test-utils";
import { readFold } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");

const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

function deps(over: Partial<PrsViewDeps> = {}): PrsViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async () => []),
    run: vi.fn(async () => ({ kind: "opened", url: "https://registry.example/x" }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => NOW,
    said,
    ...over,
  };
}

function input(payloads = [payload("fx", fixture("prs"), { sourceLabel: "Fixture" })], over: Partial<PrsViewInput> = {}): PrsViewInput {
  return { model: buildPrsModel(payloads), servedBy: ["Fixture"], versions: { fx: 1 }, ...over };
}

const groups = (v: PrsView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-dt-body > .chappa-dt-group")].map((g) => g.dataset.group!);
const group = (v: PrsView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-dt-body > .chappa-dt-group[data-group="${id}"]`)!;
const tab = (v: PrsView, id: string): HTMLButtonElement => v.host.querySelector<HTMLButtonElement>(`.chappa-sv-tab[data-tab="${id}"]`)!;
const rowsOf = (v: PrsView, bucket: string): string[] =>
  [...v.host.querySelectorAll<HTMLElement>(`.chappa-dt-group[data-bucket="${bucket}"] .chappa-pr-row`)].map((r) => r.dataset.id!);
const row = (v: PrsView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-pr-row[data-id="${id}"]`)!;
const cell = (r: HTMLElement, c: string): HTMLElement => r.querySelector<HTMLElement>(`.chappa-pr-c-${c}`)!;
const label = (v: PrsView, id: string): string | null | undefined => group(v, id).querySelector(".chappa-dt-group-label")?.textContent;
const folded = (v: PrsView, id: string): boolean => group(v, id).querySelector(".chappa-dt-rows")!.classList.contains("folded");
const menu = (v: PrsView): HTMLElement | null => v.host.querySelector<HTMLElement>(".chappa-dt-menu");
const openMenu = (v: PrsView, id: string): void => row(v, id).querySelector<HTMLButtonElement>(".chappa-dt-more")!.click();

beforeEach(() => localStorage.clear());

describe("PrsView", () => {
  it("draws the freshness line, notices, tabs with counts, the column header and the My groups in order", () => {
    const v = new PrsView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("PRs");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect([...v.host.querySelectorAll(".chappa-sv-notice")].map((n) => n.textContent)).toEqual(["showing 10 of 24 pull requests"]);
    expect([...v.host.querySelectorAll(".chappa-sv-tab")].map((t) => t.textContent)).toEqual(["My PRs (5)", "Reviewing (3)", "All (4)"]);
    expect(tab(v, "mine").getAttribute("aria-selected")).toBe("true");
    expect([...v.host.querySelectorAll(".chappa-dt-cols > *")].map((c) => c.textContent)).toEqual(PR_COLUMNS);
    expect(groups(v)).toEqual(["action", "draft", "ready", "waiting", "recent"]);
    expect(label(v, "action")).toBe("▾ Blocked on me");
    expect(group(v, "action").querySelector(".chappa-dt-group-count")?.textContent).toBe("1");
    v.dispose();
  });

  it("a row is one line of aligned cells: repo over forge, number, title over author | branch, status chips, votes, comments, age", () => {
    const v = new PrsView(deps());
    v.update(input());
    const r = row(v, "github/owner/repo/#1");
    expect(r.querySelectorAll(":scope > .chappa-dt-cell").length).toBe(PR_COLUMNS.length);
    expect(cell(r, "repo").textContent).toBe("owner/repogithub | platform");
    expect(cell(r, "number").textContent).toBe("#1");
    expect(r.querySelector(".chappa-sv-title")?.textContent).toBe("Change 1");
    expect(r.querySelector(".chappa-pr-by")?.textContent).toBe("Alex | abc-1-health → main");
    // Both reasons restate a chip, so neither is said again as a sentence.
    expect(r.querySelector(".chappa-pr-why")?.textContent).toBe("");
    expect([...cell(r, "status").querySelectorAll<HTMLElement>(".chappa-sv-chip")].map((c) => `${c.dataset.tone}:${c.textContent}`)).toEqual([
      "bad:blocked on me",
      "bad:1 check failing",
      "bad:conflicts",
    ]);
    expect(cell(r, "votes").querySelector(".chappa-pr-votes")?.textContent).toBe("0 approved · 1 waiting · 1 required");
    expect(cell(r, "comments").textContent).toBe("2 active / 1 resolved");
    expect(cell(r, "comments").querySelector<HTMLElement>(".chappa-pr-comments")?.dataset.forMe).toBe("1");
    // My own vote shows on a review.
    tab(v, "reviewing").click();
    expect(cell(row(v, "github/owner/repo/#7"), "votes").textContent).toContain("me: no vote (required)");
    v.dispose();
  });

  it("a reason that says something no chip says stays under the title; the age is short", () => {
    const v = new PrsView(deps());
    v.update(input([payload("a", prsBody([pr("#1", { reasons: ["still a draft", "the release branch is frozen"], state: "draft", lastTouchedUtc: "2026-01-14T10:00:00Z" })]))]));
    const r = row(v, "github/owner/repo/#1");
    expect(r.querySelector(".chappa-pr-why")?.textContent).toBe("the release branch is frozen");
    expect(cell(r, "age").textContent).toBe("1 d");
    v.dispose();
  });

  it("a group folds on its compact header; the fold is remembered across tabs, polls and a new page", () => {
    const v = new PrsView(deps());
    v.update(input());
    expect(label(v, "waiting")).toBe("▸ Waiting on reviewers");
    expect(folded(v, "waiting")).toBe(true);
    group(v, "waiting").querySelector<HTMLButtonElement>(".chappa-dt-group-head")!.click();
    expect(folded(v, "waiting")).toBe(false);
    group(v, "action").querySelector<HTMLButtonElement>(".chappa-dt-group-head")!.click();
    expect(folded(v, "action")).toBe(true);
    tab(v, "reviewing").click();
    expect(v.currentTab).toBe("reviewing");
    expect(groups(v)).toEqual(["replied", "vote", "mythreads"]);
    tab(v, "mine").click();
    v.update(input(undefined, { versions: { fx: 2 } }));
    expect([folded(v, "waiting"), folded(v, "action")]).toEqual([false, true]);
    v.dispose();
    expect(readFold(prGroupFoldKey("mine", "waiting"), true)).toBe(false);
    const again = new PrsView(deps());
    again.update(input());
    expect([folded(again, "waiting"), folded(again, "action")]).toEqual([false, true]);
    again.dispose();
  });

  it("All: groups by how I am on each review, team groups folded until unfolded", () => {
    const team = (name: string) => ({ name, isGroup: true });
    const rev = (n: string, bucket: string, over: Record<string, unknown> = {}) => pr(n, { role: "reviewing", bucket, ...over });
    const body = (extra: Record<string, unknown>[] = []) =>
      prsBody([
        rev("#1", "vote", { me: { required: true, vote: "none" } }),
        rev("#2", "mythreads", { me: { required: false, vote: "none" } }),
        rev("#3", "group", { reviewers: [team("web-team"), team("db-team")] }),
        rev("#4", "group", { reviewers: [] }),
        ...extra,
      ]);
    const v = new PrsView(deps());
    v.update(input([payload("a", body())], { versions: { a: 1 } }));
    tab(v, "all").click();
    expect(groups(v)).toEqual(["required", "optional", "team:web-team", "team"]);
    expect([label(v, "required"), label(v, "optional"), label(v, "team:web-team"), label(v, "team")]).toEqual([
      "▾ Required by name",
      "▾ Optional",
      "▸ web-team",
      "▸ Through a team",
    ]);
    expect([folded(v, "required"), folded(v, "optional"), folded(v, "team:web-team"), folded(v, "team")]).toEqual([false, false, true, true]);
    const chips = (id: string) => [...cell(row(v, id), "status").querySelectorAll(".chappa-sv-chip")].map((c) => c.textContent);
    expect(chips("github/owner/repo/#3")).toEqual(["via a team", "also via db-team"]);
    // "required" is mine to give: it is in the votes column, not a status chip.
    expect(chips("github/owner/repo/#1")).toEqual(["vote asked"]);
    group(v, "team:web-team").querySelector<HTMLButtonElement>(".chappa-dt-group-head")!.click();
    v.update(input([payload("a", body([rev("#5", "group", { reviewers: [team("api-team")] })]))], { versions: { a: 2 } }));
    expect(groups(v)).toEqual(["required", "optional", "team:api-team", "team:web-team", "team"]);
    expect([folded(v, "team:web-team"), folded(v, "team:api-team")]).toEqual([false, true]);
    v.dispose();
  });

  it("each tab says why it is empty", () => {
    const v = new PrsView(deps());
    v.update(input([payload("a", prsBody([pr("#1")]))]));
    tab(v, "reviewing").click();
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe(prsEmptyText(input([payload("a", prsBody([]))]), "reviewing"));
    tab(v, "all").click();
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("No open pull request lists you as a reviewer.");
    expect(groups(v)).toEqual([]);
    expect(v.host.querySelector<HTMLElement>(".chappa-dt")!.hidden).toBe(true);
    v.dispose();
  });

  it("a poll repaints in place: unchanged rows keep their nodes, a moved row changes group", () => {
    const v = new PrsView(deps());
    const body = (bucket: string) => prsBody([pr("#1"), pr("#2", { bucket })]);
    v.update(input([payload("a", body("action"))], { versions: { a: 1 } }));
    const first = row(v, "github/owner/repo/#1");
    const moved = row(v, "github/owner/repo/#2");
    expect(rowsOf(v, "action")).toEqual(["github/owner/repo/#1", "github/owner/repo/#2"]);
    v.update(input([payload("a", body("ready"))], { versions: { a: 2 } }));
    expect(row(v, "github/owner/repo/#1")).toBe(first);
    expect(row(v, "github/owner/repo/#2")).toBe(moved);
    expect(rowsOf(v, "ready")).toEqual(["github/owner/repo/#2"]);
    v.update(input([payload("a", prsBody([pr("#1")]))], { versions: { a: 3 } }));
    expect(row(v, "github/owner/repo/#2")).toBeNull();
    expect(groups(v)).toEqual(["action"]);
    v.dispose();
  });

  it("Open (the row's one inline control) opens the PR through the checked opener of its source; a refusal is said", async () => {
    const d = deps({ openLink: vi.fn(async () => Promise.reject(new Error("registry.example is not a trusted host"))) });
    const v = new PrsView(d);
    v.update(input());
    const r = row(v, "github/owner/repo/#1");
    expect([...r.querySelectorAll(".chappa-dt-acts button")].map((b) => b.textContent)).toEqual(["Open", "⋯"]);
    r.querySelector<HTMLButtonElement>(".chappa-dt-open")!.click();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/owner/repo/pull/1");
    await FLUSH();
    expect(d.said).toEqual(["registry.example is not a trusted host"]);
    // Opening the link is not opening the row's page.
    expect(v.host.querySelector(".chappa-pr-page")).toBeNull();
    v.dispose();
  });

  it("the ⋯ menu asks Rust only when it opens; identical links of two sources are one entry, hidden ones are absent", async () => {
    const actions = vi.fn(async (sourceId: string, _view: string, key: string): Promise<ipc.SourceActionDto[]> =>
      sourceId === "a"
        ? [
            { action: { kind: "link", id: "open", label: "Open", url: `https://registry.example/${key}` }, verdict: ok, needsPrompt: false },
            { action: { kind: "endpoint", id: "unsafe", label: "Force", mutates: true, confirm: null, params: null, note: null }, verdict: { ...ok, enabled: false, hidden: true }, needsPrompt: false },
          ]
        : [
            { action: { kind: "link", id: "open", label: "Open", url: `https://registry.example/github:owner/repo:#1` }, verdict: ok, needsPrompt: false },
            { action: { kind: "terminal", id: "shell", label: "Shell", cwd: "/srv/example", command: [], env: {} }, verdict: { ...ok, enabled: false, reason: "not inside an open project" }, needsPrompt: false },
          ],
    );
    const d = deps({ actions });
    const v = new PrsView(d);
    const link = [{ kind: "link", id: "open", label: "Open", url: "https://registry.example/x" }];
    const both = (va: number) =>
      input([payload("a", prsBody([pr("#1", { actions: link })], "2026-01-15T10:00:00Z")), payload("b", prsBody([pr("#1", { actions: link })], "2026-01-15T09:00:00Z"))], { versions: { a: va, b: 1 } });
    v.update(both(1));
    await FLUSH();
    expect(actions).not.toHaveBeenCalled();
    openMenu(v, "github/owner/repo/#1");
    await FLUSH();
    expect(actions.mock.calls.map((c) => c.slice(0, 3))).toEqual([
      ["a", "prs", "github:owner/repo:#1"],
      ["b", "prs", "github:owner/repo:#1"],
    ]);
    expect([...menu(v)!.querySelectorAll(".chappa-dt-menu-entry")].map((b) => b.textContent)).toEqual(["Open details"]);
    const buttons = [...menu(v)!.querySelectorAll<HTMLButtonElement>(".chappa-sv-actions .chappa-sv-action[data-action]")];
    expect(buttons.map((b) => [b.dataset.source, b.textContent, b.disabled])).toEqual([
      ["a", "Open", false],
      ["b", "Shell", true],
    ]);
    expect(buttons[1].title).toBe("not inside an open project (b)");
    // A poll with a new version reads the open menu's actions again.
    v.update(both(2));
    await FLUSH();
    expect(actions).toHaveBeenCalledTimes(4);
    buttons[0].click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("a", "prs", "github:owner/repo:#1", "open", false, null);
    v.dispose();
  });

  it("clicking a row opens the PR's page: every fact, its sections, all its actions; the arrow returns and Escape too", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === "github:owner/repo:#2"
        ? [{ action: { kind: "panel", id: "checks", label: "Checks", endpoint: "/api/panel/checks" }, verdict: ok, needsPrompt: false }]
        : [],
    );
    const run = vi.fn(async () => ({ kind: "panel", payload: { title: "Checks for #2", sections: [] } }) as ipc.ActionOutcomeDto);
    const d = deps({ actions, run });
    const v = new PrsView(d);
    v.update(input());
    // Sections are not in the row; they are on its page.
    expect(row(v, "github/owner/repo/#2").querySelector(".chappa-sec")).toBeNull();
    row(v, "github/owner/repo/#2").querySelector<HTMLElement>(".chappa-pr-c-votes")!.click();
    await FLUSH();
    const page = v.host.querySelector<HTMLElement>(".chappa-pr-page")!;
    expect(v.host.classList.contains("covered")).toBe(true);
    expect(page.querySelector(".chappa-page-title")?.textContent).toBe("#2 Change 2");
    expect(page.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to PRs");
    const fields = [...page.querySelectorAll(".chappa-dt-page-grid dt")].map((d) => d.textContent);
    expect(fields).toEqual(expect.arrayContaining(["Repo", "Number", "Author", "Status", "Votes", "Link", "Source"]));
    expect(page.querySelector('.chappa-sec[data-section="checks"]')?.classList.contains("collapsed")).toBe(false);
    // A poll keeps it current.
    v.update(input(undefined, { versions: { fx: 2 } }));
    expect(v.host.querySelector(".chappa-pr-page")).toBe(page);
    page.querySelector<HTMLButtonElement>('.chappa-pr-page-actions [data-action="checks"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "prs", "github:owner/repo:#2", "checks", false, null);
    const panel = v.host.querySelector<HTMLElement>(".chappa-panel-page")!;
    expect(panel.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to #2 Change 2");
    v.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(v.host.querySelector(".chappa-panel-page")).toBeNull();
    page.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    expect(v.host.querySelector(".chappa-pr-page")).toBeNull();
    expect(d.onClose).not.toHaveBeenCalled();
    v.dispose();
  });

  it("the page says so when its PR is gone", () => {
    const v = new PrsView(deps());
    v.update(input([payload("a", prsBody([pr("#1"), pr("#2")]))], { versions: { a: 1 } }));
    row(v, "github/owner/repo/#2").click();
    v.update(input([payload("a", prsBody([pr("#1")]))], { versions: { a: 2 } }));
    expect(v.host.querySelector(".chappa-pr-page .chappa-dt-page-gone")?.textContent).toBe("This pull request is no longer listed.");
    v.dispose();
  });

  it("errors show as warning lines beside what came back; another contract is a banner", () => {
    const v = new PrsView(deps());
    v.update(
      input([
        payload("a", prsBody([pr("#1")], "2026-01-15T10:00:00Z", { errors: [{ source: "a:search", message: "search failed" }] }), { sourceLabel: "A" }),
        payload("b", { contract: 2, items: [] }, { sourceLabel: "B" }),
      ]),
    );
    expect([...v.host.querySelectorAll(".chappa-sv-error")].map((e) => e.textContent)).toEqual(["search failed"]);
    expect(v.host.querySelector(".chappa-sv-banner")?.textContent).toContain("B needs an update");
    expect(rowsOf(v, "action")).toEqual(["github/owner/repo/#1"]);
    v.dispose();
  });

  it("empty states say why", () => {
    const v = new PrsView(deps());
    v.update(input([], { servedBy: [] }));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toContain("No trusted source serves pull requests");
    v.update(input([payload("a", null)], { servedBy: ["A"] }));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("Waiting for the first answer from A.");
    v.dispose();
  });

  it("Back and Escape leave", () => {
    const d = deps();
    const v = new PrsView(d);
    v.update(input());
    v.host.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    v.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(d.onClose).toHaveBeenCalledTimes(2);
    v.dispose();
  });

  it("the view's banners are drawn with their actions, read as the view itself (no key)", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === null
        ? [{ action: { kind: "endpoint", id: "resync", label: "Sync now", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false }]
        : [],
    );
    const run = vi.fn(async () => ({ kind: "done", message: "Synced" }) as ipc.ActionOutcomeDto);
    const d = deps({ actions, run });
    const v = new PrsView(d);
    v.update(input());
    await FLUSH();
    const banner = v.host.querySelector<HTMLElement>('.chappa-pr-banners .chappa-banner[data-banner="review-sync"]')!;
    banner.querySelector<HTMLButtonElement>('[data-action="resync"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "prs", null, "resync", false, null);
    v.dispose();
  });

  it("focus(key) picks the tab holding the PR (Reviewing first, then My PRs, then All), unfolds its group and marks it", () => {
    const v = new PrsView(deps());
    expect(v.focus("github:owner/repo:#7")).toBe(true);
    v.update(input());
    expect(v.currentTab).toBe("reviewing");
    expect(row(v, "github/owner/repo/#7").classList.contains("focused")).toBe(true);
    expect(v.focus("github:owner/repo:#4")).toBe(true);
    expect(v.currentTab).toBe("mine");
    expect(folded(v, "waiting")).toBe(false);
    expect(row(v, "github/owner/repo/#4").classList.contains("focused")).toBe(true);
    expect(v.focus("GitHub:Owner/Repo:#10")).toBe(true);
    expect(v.currentTab).toBe("all");
    expect(folded(v, "team:platform-team")).toBe(false);
    expect(v.focus("github:owner/repo:#99")).toBe(false);
    v.dispose();
  });

  it("focus(key) asked before the data says so when the PR never arrives", () => {
    const d = deps();
    const v = new PrsView(d);
    v.focus("github:owner/repo:#99");
    v.update(input());
    expect(d.said).toEqual(["github:owner/repo:#99 is not in the PRs list."]);
    v.dispose();
  });
});

describe("PrsView and work items", () => {
  it("a PR joined to work items carries a chip per item key in its ITEMS cell that opens the item in Work", () => {
    const openView = vi.fn(() => true);
    const v = new PrsView(deps({ openView }));
    const workItems = new Map([
      ["github/owner/repo/#1", [{ rowId: "fx|ABC-1", key: "ABC-1", title: "Add a health route to api", how: "linked" as const }, { rowId: "ws|ABC-9", key: "ABC-9", title: "ABC-9", how: "key in title" as const }]],
    ]);
    v.update(input(undefined, { workItems }));
    const chips = [...cell(row(v, "github/owner/repo/#1"), "items").querySelectorAll<HTMLButtonElement>(".chappa-pr-item")];
    expect(chips.map((c) => [c.textContent, c.dataset.how, c.title])).toEqual([
      ["ABC-1", "linked", "ABC-1 Add a health route to api (linked by the item); show it in Work"],
      ["ABC-9", "key in title", "ABC-9 (matched by key in title); show it in Work"],
    ]);
    chips[1].click();
    expect(openView).toHaveBeenCalledWith("work", "ws|ABC-9");
    // The chip is not a click on the row.
    expect(v.host.querySelector(".chappa-pr-page")).toBeNull();
    v.dispose();
  });

  it("another source's item actions sit in the menu after the PR's own", async () => {
    const actions = vi.fn(async (sourceId: string, _view: string, _key: string | null, owner?: string): Promise<ipc.SourceActionDto[]> =>
      owner
        ? [{ action: { kind: "endpoint", id: "later", label: "Review later", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false }]
        : sourceId === "fx"
          ? [{ action: { kind: "link", id: "open", label: "Open", url: "https://registry.example/owner/repo/pull/1" }, verdict: ok, needsPrompt: false }]
          : [],
    );
    const v = new PrsView(deps({ actions }));
    const sources = [
      source("fx", ["prs"], { label: "Fixture", manifestId: "fixture" }),
      source("ws", ["work"], { label: "Workspaces", manifestId: "workspaces", trust: { id: "workspaces", label: "Workspaces", linkHosts: [], roots: [], views: ["work"], hash: "h", itemActions: [{ id: "later", label: "Review later", view: "prs", sources: ["*"], mutates: false, confirm: null }] } }),
    ];
    v.update(input(undefined, { sources, versions: { fx: 1, ws: 1 } }));
    openMenu(v, "github/owner/repo/#1");
    await FLUSH();
    expect(actions).toHaveBeenCalledWith("ws", "prs", "github:owner/repo:#1", "fx");
    const btns = [...menu(v)!.querySelectorAll<HTMLButtonElement>(".chappa-sv-actions > .chappa-sv-action[data-action]")];
    expect(btns.map((b) => [b.dataset.source, b.dataset.action, b.dataset.owner ?? null])).toEqual([
      ["fx", "open", null],
      ["ws", "later", "fx"],
    ]);
    v.dispose();
  });

  it("the same action offered by two stacks is one entry; which stack runs it is asked, then its own confirm", async () => {
    const review = (enabled: boolean): ipc.SourceActionDto => ({
      action: { kind: "endpoint", id: "review-env", label: "Review with environment", mutates: false, confirm: null, params: null, note: null },
      verdict: { ...ok, enabled, reason: enabled ? null : "untrusted" },
      needsPrompt: false,
    });
    const actions = vi.fn(async (sourceId: string, _v: string, _k: string | null, owner?: string): Promise<ipc.SourceActionDto[]> =>
      owner ? [review(sourceId !== "st-c")] : [],
    );
    const run = vi.fn(async () => ({ kind: "done", message: "Started" }) as ipc.ActionOutcomeDto);
    const v = new PrsView(deps({ actions, run }));
    const stack = (id: string, label: string) =>
      source(id, ["environment"], { label, manifestId: id, trust: { id, label, linkHosts: [], roots: [], views: ["environment"], hash: "h", itemActions: [{ id: "review-env", label: "Review with environment", view: "prs", sources: ["*"], mutates: false, confirm: null }] } });
    const sources = [source("fx", ["prs"], { label: "Fixture", manifestId: "fixture" }), stack("st-a", "Stack slot 0"), stack("st-b", "Stack slot 1"), stack("st-c", "Stack slot 2")];
    v.update(input(undefined, { sources, versions: { fx: 1, "st-a": 1, "st-b": 1, "st-c": 1 } }));
    openMenu(v, "github/owner/repo/#1");
    await FLUSH();
    const entries = [...menu(v)!.querySelectorAll<HTMLButtonElement>('.chappa-sv-action[data-action="review-env"]')];
    expect(entries.map((b) => [b.textContent, b.dataset.offers])).toEqual([["Review with environment…", "st-a st-b st-c"]]);
    entries[0].click();
    await FLUSH();
    // Only the stacks whose verdict allows it are offered.
    const select = document.querySelector<HTMLSelectElement>('.chappa-dialog-field[data-name="via"]')!;
    expect([...select.options].map((o) => o.textContent)).toEqual(["(choose one)", "Stack slot 0", "Stack slot 1"]);
    select.value = "1";
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("st-b", "prs", "github:owner/repo:#1", "review-env", false, null, "fx");
    v.dispose();
  });
});
