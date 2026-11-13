// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import {
  Dashboard,
  cardView,
  historyRowText,
  isActiveCard,
  matchesFilter,
  orderCards,
  tailLine,
  type CardInput,
  type CardLive,
  type CardView,
  type DashGroupInput,
  type DashboardOptions,
} from "./dashboard";
import type * as ipc from "./ipc";

const NOW = 181_000;

function stoppedAgent(over: Partial<CardInput> = {}): CardInput {
  return {
    key: "rec:a1",
    kind: "agent",
    name: "claude",
    recordId: "a1",
    lastRun: { run_uuid: "r1", status: "exited", started_at_ms: 1000, ended_at_ms: 61_000, exit_code: 0 },
    live: null,
    ...over,
  };
}

function live(over: Partial<CardLive> = {}): CardLive {
  return {
    status: "running",
    exitCode: null,
    endedAtMs: null,
    attention: null,
    activity: false,
    stats: null,
    summary: null,
    ...over,
  };
}

const view = (over: Partial<CardView>): CardView => ({
  ...cardView(stoppedAgent(), NOW),
  ...over,
});

describe("cardView", () => {
  it("a stopped record reads like its record card: last run, summary, session, start", () => {
    const v = cardView(
      stoppedAgent({
        lastRun: {
          run_uuid: "r1",
          status: "exited",
          started_at_ms: 1000,
          ended_at_ms: 61_000,
          exit_code: 2,
          summary: "# Build\n- ok",
          summary_source: "osc",
          session: { provider: "claude", session_id: "abcd1234wxyz", captured_at_ms: 5, source: "json" },
        },
        canResume: true,
        captures: true,
        lastText: "ignored while a summary exists",
      }),
      NOW,
    );
    expect(v.status).toBe("exited");
    expect(v.live).toBe(false);
    expect(v.kindLabel).toBe("agent");
    expect(v.run).toBe("last run · exit 2 · ran 1m 0s · ended 2 min ago");
    expect(v.summary).toMatchObject({ text: "# Build\n- ok", source: "osc", ended: "2 min ago" });
    expect(v.line).toBeNull();
    expect(v.session).toBe("Session saved · claude · …wxyz");
    expect(v.actions.map((a) => a.id)).toEqual(["resume", "start"]);
    expect(v.stats).toBe("");
    expect(v.endedAtMs).toBe(61_000);
  });

  it("a record that never ran is stopped with only a start", () => {
    const v = cardView(stoppedAgent({ kind: "terminal", name: "pwsh", lastRun: null }), NOW);
    expect(v.status).toBe("stopped");
    expect(v.run).toBeNull();
    expect(v.kindLabel).toBe("shell");
    expect(v.actions).toEqual([{ id: "start", label: "▶ Start" }]);
    expect(v.session).toBeNull();
  });

  it("a run left running by a lost exit is not shown as running once nothing is attached", () => {
    const v = cardView(stoppedAgent({ lastRun: { status: "running", started_at_ms: 1000 } }), NOW);
    expect(v.status).toBe("stopped");
    expect(v.live).toBe(false);
  });

  it("a live row shows its stats, attention and the current run's summary, with stop and restart", () => {
    const v = cardView(
      stoppedAgent({
        live: live({
          attention: "agent-waiting",
          activity: true,
          stats: { cpuPct: 12.4, memBytes: 50 * 1024 * 1024, subprocCount: 2 },
          summary: { text: "halfway", source: "mcp" },
        }),
      }),
      NOW,
    );
    expect(v.status).toBe("running");
    expect(v.live).toBe(true);
    expect(v.stats).toBe("12%");
    expect(v.statsTitle).toBe("50.0 MB · 2 subprocesses");
    expect(v.attention).toBe("agent-waiting");
    expect(v.activity).toBe(true);
    // The stored head is the previous run's: no last-run line while live.
    expect(v.run).toBeNull();
    expect(v.summary).toMatchObject({ text: "halfway", source: "mcp", ended: null });
    expect(v.actions.map((a) => a.id)).toEqual(["stop", "restart"]);
  });

  it("an attached terminal that died reads like the frozen strip and offers the record's start", () => {
    const v = cardView(
      stoppedAgent({ live: live({ status: "failed", exitCode: 3, endedAtMs: 121_000, attention: "badge", stats: { cpuPct: 9, memBytes: 1, subprocCount: 0 } }) }),
      NOW,
    );
    expect(v.status).toBe("failed");
    expect(v.live).toBe(false);
    expect(v.run).toBe("failed · code 3 · 1 min ago");
    // A dead row is never waiting, and its last sample is stale.
    expect(v.attention).toBeNull();
    expect(v.stats).toBe("");
    expect(v.actions.map((a) => a.id)).toEqual(["start"]);
    expect(v.endedAtMs).toBe(121_000);
  });

  it("a row without a record can only be stopped, and has no history", () => {
    const running = cardView(stoppedAgent({ key: "term:5", recordId: null, lastRun: null, live: live() }), NOW);
    expect(running.actions.map((a) => a.id)).toEqual(["stop"]);
    expect(running.recorded).toBe(false);
    const dead = cardView(stoppedAgent({ key: "term:5", recordId: null, lastRun: null, live: live({ status: "exited" }) }), NOW);
    expect(dead.actions).toEqual([]);
  });

  it("without a summary the card shows the newest line; a blank name falls back to the kind", () => {
    const v = cardView(stoppedAgent({ name: "  ", lastText: "  Patched the parser.  " }), NOW);
    expect(v.line).toBe("Patched the parser.");
    expect(v.name).toBe("agent");
  });
});

describe("orderCards", () => {
  it("attention first (waiting above a bell), then live, then stopped by end time, newest first", () => {
    const cards = [
      view({ key: "old", endedAtMs: 1000 }),
      view({ key: "never", endedAtMs: null }),
      view({ key: "live", live: true }),
      view({ key: "bell", live: true, attention: "badge" }),
      view({ key: "new", endedAtMs: 5000 }),
      view({ key: "waiting", live: true, attention: "agent-waiting" }),
      view({ key: "live2", live: true }),
    ];
    expect(orderCards(cards).map((v) => v.key)).toEqual(["waiting", "bell", "live", "live2", "new", "old", "never"]);
  });
});

describe("matchesFilter / isActiveCard", () => {
  it("every word must match the name, kind, status or group, in any case", () => {
    const v = view({ name: "Claude Reviewer", kindLabel: "agent", status: "exited" });
    expect(matchesFilter(v, "chappa-ai", "")).toBe(true);
    expect(matchesFilter(v, "chappa-ai", "  ")).toBe(true);
    expect(matchesFilter(v, "chappa-ai", "review")).toBe(true);
    expect(matchesFilter(v, "chappa-ai", "CHAPPA claude")).toBe(true);
    expect(matchesFilter(v, "chappa-ai", "exited agent")).toBe(true);
    expect(matchesFilter(v, "chappa-ai", "claude shell")).toBe(false);
  });

  it("with stopped rows hidden, live and attention cards stay", () => {
    expect(isActiveCard(view({ live: true }))).toBe(true);
    expect(isActiveCard(view({ live: false, attention: "badge" }))).toBe(true);
    expect(isActiveCard(view({ live: false, attention: null }))).toBe(false);
  });
});

describe("tailLine / historyRowText", () => {
  it("an events tail answers its last non-thinking text event's first line", () => {
    const tail = [
      { kind: "text", payload: { text: "first answer" } },
      { kind: "text", payload: { text: "\nPatched the parser.\nmore" } },
      { kind: "text", payload: { text: "musing", thinking: true } },
      { kind: "turn_ended", payload: {} },
    ];
    expect(tailLine({ tail_kind: "events", tail })).toBe("Patched the parser.");
    expect(tailLine({ tail_kind: "events", tail: [{ kind: "tool_call", payload: {} }] })).toBeNull();
  });

  it("a text tail answers its last non-blank line; no tail answers null", () => {
    expect(tailLine({ tail_kind: "text", tail: ["$ make", "  built 3 targets  ", "", "   "] })).toBe("built 3 targets");
    expect(tailLine({ tail_kind: "text", tail: [] })).toBeNull();
    expect(tailLine({ tail_kind: "text" })).toBeNull();
  });

  it("a history row names when it started, how long it ran and how it ended", () => {
    const run: ipc.RunDto = { run_uuid: "r", status: "exited", started_at_ms: 1000, ended_at_ms: 4000, exit_code: 1, tail_kind: "text" };
    expect(historyRowText(run, NOW)).toBe("started 3 min ago · ran 3s · exit 1");
    expect(historyRowText({ ...run, status: "running", ended_at_ms: null, exit_code: null }, NOW)).toBe(
      "started 3 min ago · no end recorded · running",
    );
  });
});

describe("Dashboard", () => {
  function make(over: Partial<DashboardOptions> = {}): { dash: Dashboard; opts: DashboardOptions } {
    const opts: DashboardOptions = {
      onJump: vi.fn(),
      onAction: vi.fn(),
      onClose: vi.fn(),
      loadRuns: vi.fn(async () => []),
      ...over,
    };
    const dash = new Dashboard(opts);
    document.body.appendChild(dash.host);
    return { dash, opts };
  }

  const group = (key: string, name: string, cards: CardInput[]): DashGroupInput => ({
    key,
    name,
    badge: key === "ws" ? null : () => Object.assign(document.createElement("span"), { className: "badge", textContent: "CH" }),
    badgeSig: key,
    cards,
  });

  const cardEl = (dash: Dashboard, key: string): HTMLElement =>
    dash.host.querySelector<HTMLElement>(`.chappa-dash-card[data-key="${key}"]`)!;
  const keys = (dash: Dashboard, groupKey: string): string[] =>
    [...dash.host.querySelectorAll<HTMLElement>(`[data-group="${groupKey}"] .chappa-dash-card`)].map((c) => c.dataset.key!);

  const two = (): DashGroupInput[] => [
    group("p:1", "chappa-ai", [
      stoppedAgent({ key: "rec:a1" }),
      stoppedAgent({ key: "cmd:1:server", kind: "command", name: "server", recordId: "c1", live: live({ stats: { cpuPct: 4, memBytes: 10 * 1024 * 1024, subprocCount: 0 } }) }),
    ]),
    group("ws", "Workspace", [stoppedAgent({ key: "rec:t1", kind: "terminal", name: "pwsh", lastRun: null })]),
  ];

  it("draws one group per input in order, with the badge, name and totals, cards ordered", () => {
    const { dash } = make();
    dash.update(two(), NOW);
    const groups = [...dash.host.querySelectorAll<HTMLElement>(".chappa-dash-group")];
    expect(groups.map((g) => g.dataset.group)).toEqual(["p:1", "ws"]);
    expect(groups[0].querySelector(".chappa-dash-group-badge .badge")?.textContent).toBe("CH");
    expect(groups[0].querySelector(".chappa-dash-group-name")?.textContent).toBe("chappa-ai");
    expect(groups[0].querySelector(".chappa-dash-group-totals")?.textContent).toBe("1/2 running · 4% · 10.0 MB");
    expect(groups[1].querySelector(".chappa-dash-group-badge")?.childNodes.length).toBe(0);
    expect(groups[1].querySelector(".chappa-dash-group-totals")?.textContent).toBe("0/1 running");
    // The live command sorts above the stopped agent.
    expect(keys(dash, "p:1")).toEqual(["cmd:1:server", "rec:a1"]);
    dash.dispose();
  });

  it("the filter and the stopped toggle hide cards and empty groups; Escape clears the filter, then leaves", () => {
    const { dash, opts } = make();
    dash.update(two(), NOW);
    const filter = dash.host.querySelector<HTMLInputElement>(".chappa-dash-filter")!;
    filter.value = "pwsh";
    filter.dispatchEvent(new Event("input"));
    expect([...dash.host.querySelectorAll<HTMLElement>(".chappa-dash-group")].map((g) => g.dataset.group)).toEqual(["ws"]);
    filter.value = "nothing-matches";
    filter.dispatchEvent(new Event("input"));
    expect(dash.host.querySelector(".chappa-dash-empty")?.textContent).toBe("No rows match.");
    filter.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(filter.value).toBe("");
    expect(opts.onClose).not.toHaveBeenCalled();
    expect(dash.host.querySelectorAll(".chappa-dash-card").length).toBe(3);

    const toggle = dash.host.querySelector<HTMLInputElement>(".chappa-dash-stopped-toggle")!;
    toggle.checked = false;
    toggle.dispatchEvent(new Event("change"));
    expect([...dash.host.querySelectorAll<HTMLElement>(".chappa-dash-card")].map((c) => c.dataset.key)).toEqual(["cmd:1:server"]);
    dash.host.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(opts.onClose).toHaveBeenCalledTimes(1);
    dash.dispose();
  });

  it("a changed card repaints in place; an unchanged one is not touched", () => {
    const { dash } = make();
    const input = two();
    dash.update(input, NOW);
    const agent = cardEl(dash, "rec:a1");
    const shell = cardEl(dash, "rec:t1");
    const agentRun = agent.querySelector(".chappa-dash-run")!;
    const shellHtml = shell.innerHTML;
    const shellName = shell.querySelector(".chappa-dash-name")!;
    input[0].cards[0] = stoppedAgent({ key: "rec:a1", live: live({ status: "running" }) });
    dash.update(input, NOW);
    expect(cardEl(dash, "rec:a1")).toBe(agent);
    expect(agent.querySelector(".chappa-dash-run")).toBe(agentRun);
    expect(agent.dataset.status).toBe("running");
    expect(agentRun.textContent).toBe("");
    expect(shell.innerHTML).toBe(shellHtml);
    expect(shell.querySelector(".chappa-dash-name")).toBe(shellName);
    dash.dispose();
  });

  it("a click jumps, an action button acts without jumping", () => {
    const { dash, opts } = make();
    dash.update(two(), NOW);
    const agent = cardEl(dash, "rec:a1");
    agent.querySelector<HTMLElement>(".chappa-dash-name")!.click();
    expect(opts.onJump).toHaveBeenCalledWith("rec:a1");
    agent.querySelector<HTMLButtonElement>('.chappa-dash-action[data-action="start"]')!.click();
    expect(opts.onAction).toHaveBeenCalledWith("rec:a1", "start");
    expect(opts.onJump).toHaveBeenCalledTimes(1);
    agent.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(opts.onJump).toHaveBeenCalledTimes(2);
    dash.dispose();
  });

  it("the history reads the ledger once per opening, never on a repaint; a run unfolds to its summary and tail", async () => {
    const runs: ipc.RunDto[] = [
      { run_uuid: "r2", status: "exited", started_at_ms: 1000, ended_at_ms: 4000, exit_code: 1, tail_kind: "text", tail: ["boom"], summary: "## Broke\nsee log" },
      { run_uuid: "r1", status: "stopped", started_at_ms: 500, ended_at_ms: 900, tail_kind: "text", tail: [] },
    ];
    const loadRuns = vi.fn(async () => runs);
    const { dash } = make({ loadRuns });
    const input = two();
    dash.update(input, NOW);
    const agent = cardEl(dash, "rec:a1");
    const toggle = agent.querySelector<HTMLButtonElement>(".chappa-dash-history-toggle")!;
    expect(toggle.textContent).toBe("History ▸");
    toggle.click();
    expect(toggle.textContent).toBe("History ▾");
    await Promise.resolve();
    await Promise.resolve();
    expect(loadRuns).toHaveBeenCalledTimes(1);
    expect(loadRuns).toHaveBeenCalledWith("rec:a1");
    const rows = [...agent.querySelectorAll<HTMLButtonElement>(".chappa-dash-run-row")];
    expect(rows.map((r) => r.textContent)).toEqual([
      "started 3 min ago · ran 3s · exit 1 — Broke",
      "started 3 min ago · ran 0s · stopped",
    ]);
    // Repaints (a status change of this card included) do not read again.
    input[0].cards[0] = stoppedAgent({ key: "rec:a1", live: live() });
    dash.update(input, NOW);
    dash.update(input, NOW);
    expect(loadRuns).toHaveBeenCalledTimes(1);
    expect(agent.querySelectorAll(".chappa-dash-run-row").length).toBe(2);
    // A click on the history does not jump; a run unfolds.
    rows[0].click();
    const detail = agent.querySelector(".chappa-dash-run-detail")!;
    expect(detail.querySelector(".chappa-md h2")?.textContent).toBe("Broke");
    expect(detail.querySelector(".chappa-dash-run-tail")?.textContent).toBe("boom");
    rows[1].click();
    expect(agent.querySelectorAll(".chappa-dash-run-detail")[1]?.textContent).toBe("No summary or output was stored for this run.");
    // Close and reopen: one more read.
    toggle.click();
    expect(agent.querySelector(".chappa-dash-history")?.childNodes.length).toBe(0);
    toggle.click();
    await Promise.resolve();
    expect(loadRuns).toHaveBeenCalledTimes(2);
    dash.dispose();
  });

  it("a row without a record has no history toggle", () => {
    const { dash } = make();
    dash.update([group("ws", "Workspace", [stoppedAgent({ key: "term:4", recordId: null, lastRun: null, live: live() })])], NOW);
    expect(cardEl(dash, "term:4").querySelector<HTMLElement>(".chappa-dash-history-toggle")!.style.display).toBe("none");
    dash.dispose();
  });

  it("with nothing to show it says how to get rows", () => {
    const { dash } = make();
    dash.update([group("ws", "Workspace", [])], NOW);
    expect(dash.host.querySelector(".chappa-dash-empty")?.textContent).toBe(
      "Nothing here yet: open a project, or start a terminal or an agent.",
    );
    dash.dispose();
  });
});
