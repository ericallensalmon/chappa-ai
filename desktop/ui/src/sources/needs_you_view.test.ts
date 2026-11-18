// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
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
    terminals: [{ key: "term:4", name: "helper", project: "api", attention: "agent-waiting", programStatus: null }],
    work: [payload("fx", fixture("work"), { sourceLabel: "Fixture" })],
    prs: buildPrsModel([prs]),
    attention: [payload("fx", fixture("attention"), { sourceLabel: "Fixture" })],
  };
}

const sections = (v: NeedsYouView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-needs-section")].map((s) => s.dataset.section!);
const section = (v: NeedsYouView, id: string): HTMLElement => v.host.querySelector<HTMLElement>(`.chappa-needs-section[data-section="${id}"]`)!;
const row = (v: NeedsYouView, id: string): HTMLElement =>
  [...v.host.querySelectorAll<HTMLElement>(".chappa-needs-row")].find((r) => r.dataset.id === id)!;

beforeEach(() => localStorage.clear());

describe("NeedsYouView", () => {
  it("one compact section per kind with its count; a row is lead, title over its line, and the reason as one badge", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("Needs you");
    expect(sections(v)).toEqual(["work", "terminals", "prs", "group:Builds", "group:Incidents"]);
    const heads = [...v.host.querySelectorAll(".chappa-needs-section")].map(
      (s) => `${s.querySelector(".chappa-dt-group-label")?.textContent} ${s.querySelector(".chappa-dt-group-count")?.textContent}`,
    );
    expect(heads).toEqual(["▾ Work items 1", "▾ Terminals and agents 1", "▾ Pull requests 5", "▾ Builds 2", "▾ Incidents 2"]);
    const helper = row(v, "term:term:4");
    expect(helper.querySelector(".chappa-sv-title")?.textContent).toBe("helper");
    expect(helper.querySelector(".chappa-sv-detail")?.textContent).toBe("api");
    expect(helper.querySelector(".chappa-needs-badge .chappa-sv-chip")?.textContent).toBe("waiting for your input");
    const pr = row(v, "pr:github/owner/repo/#1");
    expect(pr.querySelector(".chappa-dt-num")?.textContent).toBe("#1");
    expect(pr.querySelector(".chappa-sv-title")?.textContent).toBe("Change 1");
    expect(pr.querySelector(".chappa-sv-detail")?.textContent).toBe("owner/repo | Checks are failing: lint");
    expect([...pr.querySelectorAll(".chappa-sv-chip")].map((c) => c.textContent)).toEqual(["blocked on me"]);
    const work = row(v, "work:fx|ABC-1");
    expect(work.querySelector(".chappa-dt-num")?.textContent).toBe("ABC-1");
    expect(work.querySelector(".chappa-sv-title")?.textContent).toBe("Add a health route to api");
    v.dispose();
  });

  it("an empty standing section says nothing waits there", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou({ ...fixtureInput(), terminals: [] }), versions: {} });
    expect(section(v, "terminals").querySelector(".chappa-dt-group-count")?.textContent).toBe("0");
    expect(section(v, "terminals").querySelector(".chappa-dt-empty")?.textContent).toBe("No terminal or agent is waiting for you.");
    expect(section(v, "work").querySelector(".chappa-dt-empty")?.textContent).toBe("");
    // With nothing in play at all the page's own empty line shows.
    v.update({ model: buildNeedsYou({ terminals: [], work: [], prs: null, attention: [], terminalsSection: false }), versions: {} });
    expect(sections(v)).toEqual([]);
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe(NEEDS_YOU_EMPTY);
    v.dispose();
  });

  it("a section folds on its header and the waiting PRs on their toggle; both are remembered", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const prs = section(v, "prs");
    const fold = prs.querySelector<HTMLButtonElement>(".chappa-sv-fold")!;
    expect(fold.textContent).toBe("▸ Waiting on others (1)");
    expect(prs.querySelector(".chappa-sv-fold-rows")?.classList.contains("folded")).toBe(true);
    fold.click();
    expect(prs.querySelector(".chappa-sv-fold-rows")?.classList.contains("folded")).toBe(false);
    expect(fold.textContent).toBe("▾ Waiting on others (1)");
    expect(section(v, "terminals").querySelector<HTMLElement>(".chappa-sv-fold")!.style.display).toBe("none");
    section(v, "group:Builds").querySelector<HTMLButtonElement>(".chappa-dt-group-head")!.click();
    expect(section(v, "group:Builds").querySelector(".chappa-needs-body")?.classList.contains("folded")).toBe(true);
    v.dispose();
    // A new page (the app restarted) reads both back.
    const again = new NeedsYouView(deps());
    again.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    expect(section(again, "group:Builds").querySelector(".chappa-needs-body")?.classList.contains("folded")).toBe(true);
    expect(section(again, "group:Builds").querySelector(".chappa-dt-group-head")?.getAttribute("aria-expanded")).toBe("false");
    expect(section(again, "prs").querySelector(".chappa-sv-fold-rows")?.classList.contains("folded")).toBe(false);
    again.dispose();
  });

  it("clicks: a terminal row selects it, a link opens checked, a view target opens the view or says it cannot", () => {
    const d = deps({ openView: vi.fn((view: string) => view === "prs") });
    const v = new NeedsYouView(d);
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const click = (id: string): void => row(v, id).querySelector<HTMLButtonElement>(".chappa-sv-title")!.click();
    click("term:term:4");
    expect(d.selectTerminal).toHaveBeenCalledWith("term:4");
    click("att:fx|nightly-build-1");
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://registry.example/owner/repo/actions/runs/1");
    // Anywhere on the row does what its title does.
    row(v, "pr:github/owner/repo/#7").querySelector<HTMLElement>(".chappa-sv-detail")!.click();
    expect(d.openView).toHaveBeenLastCalledWith("prs", "github:owner/repo:#7", "fx");
    click("att:fx|review-note-1");
    expect(d.openView).toHaveBeenLastCalledWith("work", "ABC-2", "fx");
    expect(d.said).toEqual(["No work view is available to show ABC-2."]);
    expect(row(v, "att:fx|plain-1").querySelector<HTMLButtonElement>(".chappa-sv-title")!.disabled).toBe(true);
    v.dispose();
  });

  it("a row's actions are asked for only when its menu opens, from its own source and view, and run through runAction", async () => {
    const actions = vi.fn(async (): Promise<ipc.SourceActionDto[]> => [
      { action: { kind: "endpoint", id: "ping", label: "Ping", mutates: false, confirm: null, params: null, note: null }, verdict: { enabled: true, reason: null, hidden: false, cwd: null, tool: null }, needsPrompt: false },
    ]);
    const d = deps({ actions });
    const v = new NeedsYouView(d);
    v.update({ model: buildNeedsYou(fixtureInput()), versions: { fx: 1 } });
    await FLUSH();
    expect(actions).not.toHaveBeenCalled();
    // A row with nothing to do keeps its menu button out of sight.
    expect(row(v, "att:fx|plain-1").querySelector<HTMLButtonElement>(".chappa-dt-more")!.hidden).toBe(true);
    row(v, "att:fx|review-note-1").querySelector<HTMLButtonElement>(".chappa-dt-more")!.click();
    await FLUSH();
    expect(actions.mock.calls.map((c) => c.slice(0, 3))).toEqual([["fx", "attention", "review-note-1"]]);
    const menu = v.host.querySelector<HTMLElement>(".chappa-dt-menu")!;
    expect([...menu.querySelectorAll<HTMLButtonElement>("button")].map((b) => b.textContent)).toEqual(["Open", "Ping"]);
    menu.querySelector<HTMLButtonElement>('.chappa-sv-action[data-action="ping"]')!.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "attention", "review-note-1", "ping", false, null);
    expect(d.said).toEqual(["pong"]);
    // Escape closes the menu and stays on the page.
    row(v, "att:fx|review-note-1").querySelector<HTMLButtonElement>(".chappa-dt-more")!.click();
    expect(v.host.querySelector(".chappa-dt-menu")).not.toBeNull();
    v.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(v.host.querySelector(".chappa-dt-menu")).toBeNull();
    expect(d.onClose).not.toHaveBeenCalled();
    v.dispose();
  });

  it("an app-side action (Add to .gitignore) sits in the row's menu", () => {
    const run = vi.fn();
    const v = new NeedsYouView(deps());
    v.update({
      model: buildNeedsYou({
        terminals: [],
        work: [],
        prs: null,
        attention: [],
        terminalsSection: false,
        agentSettings: [{ projectRoot: "/srv/example", file: { path: ".agent/settings.json", exposure: "not-ignored", message: "git would commit it", ignoreLine: ".agent/settings.json" } as ipc.AgentSettingsFileDto, addToGitignore: run }],
      }),
      versions: {},
    });
    const r = row(v, "agent-settings:.agent/settings.json");
    r.querySelector<HTMLButtonElement>(".chappa-dt-more")!.click();
    const entries = [...v.host.querySelectorAll<HTMLButtonElement>(".chappa-dt-menu .chappa-dt-menu-entry")];
    expect(entries.map((b) => b.textContent)).toEqual(["Add to .gitignore"]);
    entries[0].click();
    expect(run).toHaveBeenCalledTimes(1);
    v.dispose();
  });

  it("repaints in place", () => {
    const v = new NeedsYouView(deps());
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    const node = row(v, "att:fx|plain-1");
    v.update({ model: buildNeedsYou(fixtureInput()), versions: {} });
    expect(row(v, "att:fx|plain-1")).toBe(node);
    v.dispose();
  });
});
