// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildNeedsYou, type NeedsYouInput } from "./needs_you";
import { NEEDS_YOU_EMPTY, NeedsYouView, type NeedsYouViewDeps } from "./needs_you_view";
import { buildPrsModel } from "./prs_model";
import { fixture, payload } from "./test-utils";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

function deps(over: Partial<NeedsYouViewDeps> = {}): NeedsYouViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async () => []),
    run: vi.fn(async () => ({ kind: "done", message: "pong" }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => Date.parse("2026-01-15T10:00:30Z"),
    selectTerminal: vi.fn(),
    openView: vi.fn(() => true),
    said,
    ...over,
  };
}

function fixtureInput(): NeedsYouInput {
  const prs = payload("fx", fixture("prs"), { sourceLabel: "Fixture" });
  return {
    terminals: [{ key: "term:4", name: "claude", project: null, attention: "agent-waiting", programStatus: null }],
    work: [payload("fx", fixture("work"), { sourceLabel: "Fixture" })],
    prs: buildPrsModel([prs]),
    attention: [payload("fx", fixture("attention"), { sourceLabel: "Fixture" })],
  };
}

const sections = (v: NeedsYouView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-sv-content > .chappa-sv-group")].map((s) => s.dataset.section!);
const row = (v: NeedsYouView, id: string): HTMLElement =>
  [...v.host.querySelectorAll<HTMLElement>(".chappa-needs-row")].find((r) => r.dataset.id === id)!;

describe("NeedsYouView", () => {
  it("one section per kind, each titled with its count; rows lead with the item name", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-back-arrow")?.textContent).toBe("←");
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("Needs you");
    expect(sections(v)).toEqual(["terminals", "work", "prs", "group:Builds", "group:Incidents"]);
    const heads = [...v.host.querySelectorAll(".chappa-sv-content > .chappa-sv-group")].map(
      (s) => `${s.querySelector(".chappa-sv-group-label")?.textContent} ${s.querySelector(".chappa-sv-count")?.textContent}`,
    );
    expect(heads).toEqual(["Terminals and agents 1", "Work 1", "Pull requests 5", "Builds 2", "Incidents 2"]);
    const claude = row(v, "term:term:4");
    expect(claude.querySelector(".chappa-sv-title")?.textContent).toBe("claude");
    expect(claude.querySelector(".chappa-sv-detail")?.textContent).toBe("waiting for your input");
    expect(row(v, "work:fx|ABC-1").querySelector(".chappa-sv-title")?.textContent).toBe("ABC-1 Add a health route to api");
    v.dispose();
  });

  it("the folded PRs sit behind their own toggle with a count", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const prs = v.host.querySelector<HTMLElement>('[data-section="prs"]')!;
    const fold = prs.querySelector<HTMLButtonElement>(".chappa-sv-fold")!;
    expect(fold.textContent).toBe("▸ Waiting on others (1)");
    expect(prs.querySelector(".chappa-sv-fold-rows")?.classList.contains("folded")).toBe(true);
    expect(prs.querySelectorAll(".chappa-sv-fold-rows .chappa-needs-row").length).toBe(1);
    fold.click();
    expect(prs.querySelector(".chappa-sv-fold-rows")?.classList.contains("folded")).toBe(false);
    expect(fold.textContent).toBe("▾ Waiting on others (1)");
    // The terminals section has nothing folded: no toggle.
    expect(v.host.querySelector<HTMLElement>('[data-section="terminals"] .chappa-sv-fold')!.style.display).toBe("none");
    v.dispose();
  });

  it("clicks: a terminal row selects it, a link opens checked, a view target opens the view or says it cannot", async () => {
    const d = deps({ openView: vi.fn((view: string) => view === "prs") });
    const v = new NeedsYouView(d);
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const click = (id: string): void => row(v, id).querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    click("term:term:4");
    expect(d.selectTerminal).toHaveBeenCalledWith("term:4");
    click("att:fx|nightly-build-1");
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/owner/repo/actions/runs/1");
    // A PR opens the PRs view at it, with its source.
    click("pr:github/owner/repo/#7");
    expect(d.openView).toHaveBeenLastCalledWith("prs", "github:owner/repo:#7", "fx");
    click("att:fx|review-note-1");
    expect(d.openView).toHaveBeenLastCalledWith("work", "ABC-2", "fx");
    expect(d.said).toEqual(["No work view is available to show ABC-2."]);
    // An item that opens nothing has a disabled title.
    expect(row(v, "att:fx|plain-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.disabled).toBe(true);
    v.dispose();
  });

  it("actions are read per row from its own source and view, and run through runAction", async () => {
    const actions = vi.fn(async (): Promise<ipc.SourceActionDto[]> => [
      { action: { kind: "endpoint", id: "ping", label: "Ping", mutates: false, confirm: null, params: null, note: null }, verdict: { enabled: true, reason: null, hidden: false, cwd: null, tool: null }, needsPrompt: false },
    ]);
    const d = deps({ actions });
    const v = new NeedsYouView(d);
    v.update({ model: buildNeedsYou(fixtureInput()), versions: { fx: 1 } });
    await FLUSH();
    expect(actions.mock.calls.map((c) => c.slice(0, 3))).toEqual([
      ["fx", "work", "ABC-1"],
      ["fx", "prs", "github:owner/repo:#1"],
      ["fx", "prs", "github:owner/repo:#2"],
      ["fx", "attention", "nightly-build-1"],
      ["fx", "attention", "review-note-1"],
    ]);
    row(v, "att:fx|review-note-1").querySelector<HTMLButtonElement>('.chappa-sv-action[data-action="ping"]')!.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "attention", "review-note-1", "ping", false, null);
    expect(d.said).toEqual(["pong"]);
    v.dispose();
  });

  it("repaints in place and says why it is empty", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const node = row(v, "att:fx|plain-1");
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    expect(row(v, "att:fx|plain-1")).toBe(node);
    v.update({ model: buildNeedsYou({ terminals: [], work: [], prs: null, attention: [] }), versions: {} });
    expect(sections(v)).toEqual([]);
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe(NEEDS_YOU_EMPTY);
    v.dispose();
  });
});
