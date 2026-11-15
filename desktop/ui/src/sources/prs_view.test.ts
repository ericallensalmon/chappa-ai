// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildPrsModel } from "./prs_model";
import { PrsView, prsEmptyText, type PrsViewInput } from "./prs_view";
import { fixture, payload, pr, prsBody } from "./test-utils";
import type { PrsViewDeps } from "./prs_view";
import { source } from "./test-utils";

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

const groups = (v: PrsView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-sv-content > .chappa-sv-group")].map((g) => g.dataset.group!);
const group = (v: PrsView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-sv-content > .chappa-sv-group[data-group="${id}"]`)!;
const tab = (v: PrsView, id: string): HTMLButtonElement => v.host.querySelector<HTMLButtonElement>(`.chappa-sv-tab[data-tab="${id}"]`)!;
const rowsOf = (v: PrsView, bucket: string): string[] =>
  [...v.host.querySelectorAll<HTMLElement>(`.chappa-sv-group[data-bucket="${bucket}"] .chappa-pr-row`)].map((r) => r.dataset.id!);
const row = (v: PrsView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-pr-row[data-id="${id}"]`)!;

describe("PrsView", () => {
  it("draws the freshness line, notices, tabs with counts and the My groups in order", () => {
    const v = new PrsView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-back-arrow")?.textContent).toBe("←");
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("PRs");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect([...v.host.querySelectorAll(".chappa-sv-notice")].map((n) => n.textContent)).toEqual(["showing 10 of 24 pull requests"]);
    expect(v.host.querySelectorAll(".chappa-sv-error").length).toBe(0);
    expect([...v.host.querySelectorAll(".chappa-sv-tab")].map((t) => t.textContent)).toEqual(["My PRs (5)", "Reviewing (3)", "All (4)"]);
    expect(tab(v, "mine").getAttribute("aria-selected")).toBe("true");
    expect(groups(v)).toEqual(["action", "draft", "ready", "waiting", "recent"]);
    expect(v.host.querySelector('.chappa-sv-group[data-bucket="action"] .chappa-sv-group-label')?.textContent).toBe("▾ Blocked on me");
    v.dispose();
  });

  it("a row: title, repo#number · author · branch, reasons one per line, chips with tones", () => {
    const v = new PrsView(deps());
    v.update(input());
    const r = row(v, "github/owner/repo/#1");
    expect(r.querySelector(".chappa-sv-title")?.textContent).toBe("Change 1");
    expect(r.querySelector(".chappa-sv-meta")?.textContent).toBe("owner/repo#1 · Alex · abc-1-health → main");
    expect([...r.querySelectorAll(".chappa-sv-reason")].map((x) => x.textContent)).toEqual(["Checks are failing: lint", "1 thread needs your reply"]);
    expect([...r.querySelectorAll<HTMLElement>(".chappa-sv-chip")].map((c) => `${c.dataset.tone}:${c.textContent}`)).toEqual([
      "bad:blocked on me",
      "bad:1 check failing",
      "warn:1 thread for me",
      "bad:conflicts",
      "warn:1 waiting for author",
    ]);
    v.dispose();
  });

  it("the waiting group is folded with its count and unfolds on click; tabs switch", () => {
    const v = new PrsView(deps());
    v.update(input());
    const waiting = v.host.querySelector<HTMLElement>('.chappa-sv-group[data-bucket="waiting"]')!;
    expect(waiting.querySelector(".chappa-sv-group-label")?.textContent).toBe("▸ Waiting on reviewers");
    expect(waiting.querySelector(".chappa-sv-count")?.textContent).toBe("1");
    expect(waiting.querySelector(".chappa-sv-rows")?.classList.contains("folded")).toBe(true);
    waiting.querySelector<HTMLButtonElement>(".chappa-sv-group-head")!.click();
    expect(waiting.querySelector(".chappa-sv-rows")?.classList.contains("folded")).toBe(false);
    v.host.querySelector<HTMLButtonElement>('.chappa-sv-tab[data-tab="reviewing"]')!.click();
    expect(v.currentTab).toBe("reviewing");
    expect(groups(v)).toEqual(["replied", "vote", "mythreads"]);
    // The choice survives a repaint.
    v.host.querySelector<HTMLButtonElement>('.chappa-sv-tab[data-tab="mine"]')!.click();
    v.update(input());
    expect(v.host.querySelector('.chappa-sv-group[data-bucket="waiting"] .chappa-sv-rows')?.classList.contains("folded")).toBe(false);
    v.dispose();
  });

  it("All: groups by how I am on each review, team groups folded, each fold remembered across tabs and polls", () => {
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
    expect(tab(v, "all").textContent).toBe("All (4)");
    expect(tab(v, "reviewing").textContent).toBe("Reviewing (2)");
    tab(v, "all").click();
    expect(v.currentTab).toBe("all");
    expect(tab(v, "all").getAttribute("aria-selected")).toBe("true");
    expect(groups(v)).toEqual(["required", "optional", "team:web-team", "team"]);
    const label = (id: string) => group(v, id).querySelector(".chappa-sv-group-label")?.textContent;
    const folded = (id: string) => group(v, id).querySelector(".chappa-sv-rows")!.classList.contains("folded");
    expect([label("required"), label("optional"), label("team:web-team"), label("team")]).toEqual([
      "▾ Required by name",
      "▾ Optional",
      "▸ web-team",
      "▸ Through a team",
    ]);
    expect([folded("required"), folded("optional"), folded("team:web-team"), folded("team")]).toEqual([false, false, true, true]);
    expect(group(v, "team:web-team").querySelector(".chappa-sv-count")?.textContent).toBe("1");
    expect(group(v, "team:web-team").querySelector(".chappa-sv-group-head")?.getAttribute("aria-expanded")).toBe("false");
    // The other team shows as a chip; every row keeps its bucket chip.
    const chips = (id: string) => [...row(v, id).querySelectorAll(".chappa-sv-chip")].map((c) => c.textContent);
    expect(chips("github/owner/repo/#3")).toEqual(["via a team", "also via db-team"]);
    expect(chips("github/owner/repo/#1")).toEqual(["vote asked", "required"]);
    // Unfold a team, fold Optional: both survive a tab switch and a poll.
    group(v, "team:web-team").querySelector<HTMLButtonElement>(".chappa-sv-group-head")!.click();
    group(v, "optional").querySelector<HTMLButtonElement>(".chappa-sv-group-head")!.click();
    expect([folded("optional"), folded("team:web-team")]).toEqual([true, false]);
    tab(v, "reviewing").click();
    // A required review sits in Reviewing and All both, as the same row.
    const required = row(v, "github/owner/repo/#1");
    expect(groups(v)).toEqual(["vote", "mythreads"]);
    tab(v, "all").click();
    expect(row(v, "github/owner/repo/#1")).toBe(required);
    v.update(input([payload("a", body([rev("#5", "group", { reviewers: [team("api-team")] })]))], { versions: { a: 2 } }));
    expect(groups(v)).toEqual(["required", "optional", "team:api-team", "team:web-team", "team"]);
    // A team group that appears later starts folded too.
    expect([folded("optional"), folded("team:web-team"), folded("team:api-team")]).toEqual([true, false, true]);
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("");
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
    expect(rowsOf(v, "action")).toEqual(["github/owner/repo/#1"]);
    expect(rowsOf(v, "ready")).toEqual(["github/owner/repo/#2"]);
    v.update(input([payload("a", prsBody([pr("#1")]))], { versions: { a: 3 } }));
    expect(row(v, "github/owner/repo/#2")).toBeNull();
    expect(groups(v)).toEqual(["action"]);
    v.dispose();
  });

  it("the title opens the PR through the checked opener of the source that supplied it; a refusal is said", async () => {
    const d = deps({ openLink: vi.fn(async () => Promise.reject(new Error("registry.example is not a trusted host"))) });
    const v = new PrsView(d);
    v.update(input());
    row(v, "github/owner/repo/#1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/owner/repo/pull/1");
    await FLUSH();
    expect(d.said).toEqual(["registry.example is not a trusted host"]);
    v.dispose();
  });

  it("action buttons come from Rust's verdicts per origin; hidden ones are not drawn; a click runs through runAction", async () => {
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
    v.update(
      input([
        payload("a", prsBody([pr("#1", { actions: link })], "2026-01-15T10:00:00Z")),
        payload("b", prsBody([pr("#1", { actions: link })], "2026-01-15T09:00:00Z")),
      ], { versions: { a: 1, b: 1 } }),
    );
    await FLUSH();
    expect(actions.mock.calls.map((c) => c.slice(0, 3))).toEqual([
      ["a", "prs", "github:owner/repo:#1"],
      ["b", "prs", "github:owner/repo:#1"],
    ]);
    const buttons = [...row(v, "github/owner/repo/#1").querySelectorAll<HTMLButtonElement>(".chappa-sv-action")];
    // The two sources' identical links are one button; the hidden mutation is absent.
    expect(buttons.map((b) => [b.dataset.source, b.textContent, b.disabled])).toEqual([
      ["a", "Open", false],
      ["b", "Shell", true],
    ]);
    expect(buttons[1].title).toBe("not inside an open project (b)");
    buttons[0].click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("a", "prs", "github:owner/repo:#1", "open", false, null);
    expect(d.said).toEqual(["Opened https://registry.example/x."]);
    // The same versions do not read the actions again; a new one does.
    v.update(input([payload("a", prsBody([pr("#1", { actions: link })])), payload("b", prsBody([pr("#1", { actions: link })], null))], { versions: { a: 1, b: 1 } }));
    expect(actions).toHaveBeenCalledTimes(2);
    v.update(input([payload("a", prsBody([pr("#1", { actions: link })])), payload("b", prsBody([pr("#1", { actions: link })], null))], { versions: { a: 2, b: 1 } }));
    expect(actions).toHaveBeenCalledTimes(4);
    v.dispose();
  });

  it("rows without actions in the payload never ask Rust", async () => {
    const d = deps();
    const v = new PrsView(d);
    v.update(input([payload("a", prsBody([pr("#2")]))]));
    await FLUSH();
    expect(d.actions).not.toHaveBeenCalled();
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
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("no data yet");
    v.update(input([payload("a", prsBody([pr("#9", { role: "reviewing", bucket: "vote" })]))]));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe(prsEmptyText(input([payload("a", prsBody([]))]), "mine"));
    v.dispose();
  });

  it("Back and Escape leave", () => {
    const d = deps();
    const v = new PrsView(d);
    v.update(input());
    v.host.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    v.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(d.onClose).toHaveBeenCalledTimes(2);
    v.setBackTo("Needs you");
    expect(v.host.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to Needs you");
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
    expect(banner.querySelector(".chappa-banner-title")?.textContent).toBe("Review requests sync every 5 minutes");
    expect(actions).toHaveBeenCalledWith("fx", "prs", null);
    banner.querySelector<HTMLButtonElement>('[data-action="resync"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "prs", null, "resync", false, null);
    expect(d.said).toEqual(["Synced"]);
    v.dispose();
  });

  it("a PR's sections fold inside its row, more rows folded too; a panel action opens the page over the view", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === "github:owner/repo:#2"
        ? [{ action: { kind: "panel", id: "checks", label: "Checks", endpoint: "/api/panel/checks" }, verdict: ok, needsPrompt: false }]
        : [],
    );
    const run = vi.fn(async () => ({ kind: "panel", payload: { title: "Checks for #2", sections: [] } }) as ipc.ActionOutcomeDto);
    const d = deps({ actions, run });
    const v = new PrsView(d);
    v.update(input());
    await FLUSH();
    const r = row(v, "github/owner/repo/#2");
    const sec = r.querySelector<HTMLElement>('.chappa-sec[data-section="checks"]')!;
    expect(sec.classList.contains("collapsed")).toBe(true);
    expect(sec.querySelector(".chappa-sec-head")?.textContent).toBe("▸ Checks");
    sec.querySelector<HTMLButtonElement>(".chappa-sec-head")!.click();
    expect(sec.classList.contains("collapsed")).toBe(false);
    expect(sec.querySelector(".chappa-sec-more-rows")?.classList.contains("folded")).toBe(true);
    expect(row(v, "github/owner/repo/#3").querySelector(".chappa-sec")).toBeNull();
    // The fold survives a poll.
    v.update(input(undefined, { versions: { fx: 2 } }));
    expect(row(v, "github/owner/repo/#2").querySelector('.chappa-sec[data-section="checks"]')?.classList.contains("collapsed")).toBe(false);
    await FLUSH();
    row(v, "github/owner/repo/#2").querySelector<HTMLButtonElement>(':scope > .chappa-sv-actions [data-action="checks"]')!.click();
    await FLUSH();
    expect(run).toHaveBeenCalledWith("fx", "prs", "github:owner/repo:#2", "checks", false, null);
    const page = v.host.querySelector<HTMLElement>(".chappa-panel-page")!;
    expect(v.host.classList.contains("covered")).toBe(true);
    expect(page.querySelector(".chappa-page-title")?.textContent).toBe("Checks for #2");
    expect(page.querySelector(".chappa-back-arrow")?.getAttribute("aria-label")).toBe("Back to PRs");
    page.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    expect(v.host.querySelector(".chappa-panel-page")).toBeNull();
    expect(d.onClose).not.toHaveBeenCalled();
    v.dispose();
  });

  it("focus(key) picks the tab holding the PR (Reviewing first, then My PRs, then All), unfolds its group and marks it", () => {
    const d = deps();
    const v = new PrsView(d);
    // Asked before the first answer: it waits for it.
    expect(v.focus("github:owner/repo:#7")).toBe(true);
    v.update(input());
    expect(v.currentTab).toBe("reviewing");
    expect(row(v, "github/owner/repo/#7").classList.contains("focused")).toBe(true);
    // Mine.
    expect(v.focus("github:owner/repo:#4")).toBe(true);
    expect(v.currentTab).toBe("mine");
    expect(group(v, "waiting").querySelector(".chappa-sv-rows")?.classList.contains("folded")).toBe(false);
    expect(row(v, "github/owner/repo/#4").classList.contains("focused")).toBe(true);
    expect(row(v, "github/owner/repo/#7")?.classList.contains("focused") ?? false).toBe(false);
    // Only in All, under a team group that starts folded; forge and repo
    // compare case folded.
    expect(v.focus("GitHub:Owner/Repo:#10")).toBe(true);
    expect(v.currentTab).toBe("all");
    expect(group(v, "team:platform-team").querySelector(".chappa-sv-rows")?.classList.contains("folded")).toBe(false);
    expect(row(v, "github/owner/repo/#10").classList.contains("focused")).toBe(true);
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
  it("a PR joined to work items carries a chip per item key that opens the item in Work", () => {
    const openView = vi.fn(() => true);
    const v = new PrsView(deps({ openView }));
    const workItems = new Map([
      ["github/owner/repo/#1", [{ rowId: "fx|ABC-1", key: "ABC-1", title: "Add a health route to api", how: "linked" as const }, { rowId: "ws|ABC-9", key: "ABC-9", title: "ABC-9", how: "key in title" as const }]],
    ]);
    v.update(input(undefined, { workItems }));
    const chips = [...row(v, "github/owner/repo/#1").querySelectorAll<HTMLButtonElement>(".chappa-pr-item")];
    expect(chips.map((c) => [c.textContent, c.dataset.how, c.title])).toEqual([
      ["ABC-1", "linked", "ABC-1 Add a health route to api (linked by the item); show it in Work"],
      ["ABC-9", "key in title", "ABC-9 (matched by key in title); show it in Work"],
    ]);
    chips[1].click();
    expect(openView).toHaveBeenCalledWith("work", "ws|ABC-9");
    expect(row(v, "github/owner/repo/#2").querySelectorAll(".chappa-pr-item").length).toBe(0);
    v.dispose();
  });

  it("another source's item actions on a PR row are asked for under the PR's source, after the PR's own", async () => {
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
    await FLUSH();
    expect(actions).toHaveBeenCalledWith("ws", "prs", "github:owner/repo:#1", "fx");
    const btns = [...row(v, "github/owner/repo/#1").querySelectorAll<HTMLButtonElement>(".chappa-sv-actions > .chappa-sv-action")];
    expect(btns.map((b) => [b.dataset.source, b.dataset.action, b.dataset.owner ?? null])).toEqual([
      ["fx", "open", null],
      ["ws", "later", "fx"],
    ]);
    v.dispose();
  });
});
