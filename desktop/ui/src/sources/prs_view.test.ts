// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildPrsModel } from "./prs_model";
import { PrsView, prsEmptyText, type PrsViewInput } from "./prs_view";
import { fixture, payload, pr, prsBody } from "./test-utils";
import type { SourceViewDeps } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");

const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

function deps(over: Partial<SourceViewDeps> = {}): SourceViewDeps & { said: string[] } {
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

const groups = (v: PrsView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-sv-content > .chappa-sv-group")].map((g) => g.dataset.bucket!);
const rowsOf = (v: PrsView, bucket: string): string[] =>
  [...v.host.querySelectorAll<HTMLElement>(`.chappa-sv-group[data-bucket="${bucket}"] .chappa-pr-row`)].map((r) => r.dataset.id!);
const row = (v: PrsView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-pr-row[data-id="${id}"]`)!;

describe("PrsView", () => {
  it("draws the freshness line, notices, tabs with counts and the My groups in order", () => {
    const v = new PrsView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-back")?.textContent).toBe("←PRs");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect([...v.host.querySelectorAll(".chappa-sv-notice")].map((n) => n.textContent)).toEqual(["showing 10 of 24 pull requests"]);
    expect(v.host.querySelectorAll(".chappa-sv-error").length).toBe(0);
    expect([...v.host.querySelectorAll(".chappa-sv-tab")].map((t) => t.textContent)).toEqual(["My PRs (5)", "Reviewing (5)"]);
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
    expect(groups(v)).toEqual(["replied", "vote", "mythreads", "voted", "group"]);
    // The choice survives a repaint.
    v.host.querySelector<HTMLButtonElement>('.chappa-sv-tab[data-tab="mine"]')!.click();
    v.update(input());
    expect(v.host.querySelector('.chappa-sv-group[data-bucket="waiting"] .chappa-sv-rows')?.classList.contains("folded")).toBe(false);
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
    v.host.querySelector<HTMLButtonElement>(".chappa-sv-back")!.click();
    v.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(d.onClose).toHaveBeenCalledTimes(2);
    v.dispose();
  });
});
