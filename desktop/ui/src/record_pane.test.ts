// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import {
  FrozenStrip,
  RecordPane,
  formatAgo,
  formatDuration,
  frozenStripText,
  paneState,
  recordView,
  type RecordPaneKind,
} from "./record_pane";

const KINDS: RecordPaneKind[] = ["command", "agent", "terminal"];
const STATUSES = ["starting", "running", "stopped", "exited", "failed"];

describe("paneState", () => {
  for (const kind of KINDS) {
    for (const status of STATUSES) {
      it(`${kind} · ${status}: an attached entry decides live or frozen`, () => {
        const record = { termId: 4 };
        const state = paneState(record, { status, exitCode: status === "failed" ? 2 : null });
        if (status === "starting" || status === "running") {
          expect(state).toEqual({ kind: "live" });
        } else {
          expect(state).toEqual({ kind: "frozen", status, exitCode: status === "failed" ? 2 : null });
        }
      });

      it(`${kind} · ${status}: nothing attached renders the record`, () => {
        // Even a record whose stored status says live has nothing to draw
        // here without a terminal attached in this window.
        expect(paneState({ termId: null }, null)).toEqual({ kind: "record" });
        expect(paneState({ termId: 9 }, null)).toEqual({ kind: "record" });
      });
    }
  }

  it("an ephemeral row (no record) follows its entry the same way", () => {
    expect(paneState(null, { status: "running", exitCode: null })).toEqual({ kind: "live" });
    expect(paneState(null, { status: "exited", exitCode: 0 })).toEqual({ kind: "frozen", status: "exited", exitCode: 0 });
    expect(paneState(null, null)).toEqual({ kind: "record" });
  });
});

describe("recordView", () => {
  const NOW = 1_700_000_600_000;

  it("names the row in the title, with the kind's hint and start label", () => {
    expect(recordView({ kind: "agent", name: "claude", lastRun: null }, NOW)).toEqual({
      title: "claude is stopped",
      hint: "Start it to begin a new session.",
      actions: [{ id: "start", label: "▶ Start new session" }],
      lastRun: null,
      session: "Resume unavailable",
      sessionTitle: null,
    });
    expect(recordView({ kind: "terminal", name: "pwsh", lastRun: null }, NOW)).toEqual({
      title: "pwsh is stopped",
      hint: "Start it to resume this shell.",
      actions: [{ id: "start", label: "▶ Start" }],
      lastRun: null,
      session: null,
      sessionTitle: null,
    });
    expect(recordView({ kind: "command", name: "server", lastRun: null }, NOW)).toEqual({
      title: "server is stopped",
      hint: "Start it to run this process.",
      actions: [{ id: "start", label: "▶ Start" }],
      lastRun: null,
      session: null,
      sessionTitle: null,
    });
  });

  it("an agent run with a saved session puts Resume first and names the provider and the id's tail", () => {
    const session = { provider: "claude", session_id: "abcd1234wxyz", captured_at_ms: 5, source: "json" as const };
    const run = { status: "exited", started_at_ms: 1000, ended_at_ms: 2000, exit_code: 0, session };
    const view = recordView({ kind: "agent", name: "claude", lastRun: run, canResume: true, captures: true }, NOW);
    expect(view.actions.map((a) => a.id)).toEqual(["resume", "start"]);
    expect(view.actions[0].label).toBe("↻ Resume last session");
    expect(view.session).toBe("Session saved · claude · …wxyz");
    expect(view.sessionTitle).toBe("abcd1234wxyz");
    // A run resumed from an earlier id says so.
    const resumed = { ...run, session: { ...session, resumed_from: "oldsession0001" } };
    expect(recordView({ kind: "agent", name: "claude", lastRun: resumed, canResume: true }, NOW).session).toBe(
      "Session saved · claude · …wxyz (resumed from …0001)",
    );
    // No resume command on the tool: the session is shown, the action is not.
    const noCommand = recordView({ kind: "agent", name: "claude", lastRun: run, canResume: false }, NOW);
    expect(noCommand.actions.map((a) => a.id)).toEqual(["start"]);
    // No session seen on a capturing tool says why resume is off.
    const plain = { status: "exited", started_at_ms: 1000, ended_at_ms: 2000, exit_code: 0 };
    expect(recordView({ kind: "agent", name: "claude", lastRun: plain, canResume: true, captures: true }, NOW).session).toBe(
      "Resume unavailable (no session id seen)",
    );
    expect(recordView({ kind: "agent", name: "claude", lastRun: plain, canResume: true, captures: false }, NOW).session).toBe(
      "Resume unavailable",
    );
  });

  it("falls back to the generic title per kind when the name is blank", () => {
    expect(recordView({ kind: "agent", name: " ", lastRun: null }, NOW).title).toBe("Agent is stopped");
    expect(recordView({ kind: "terminal", name: "", lastRun: null }, NOW).title).toBe("Terminal is stopped");
    expect(recordView({ kind: "command", name: "", lastRun: null }, NOW).title).toBe("Process is stopped");
  });

  it("summarizes the last run: outcome, duration, age", () => {
    const run = { status: "failed", started_at_ms: NOW - 200_000, ended_at_ms: NOW - 120_000, exit_code: 1 };
    const view = recordView({ kind: "command", name: "server", lastRun: run }, NOW);
    expect(view.lastRun).toMatchObject({ outcome: "exit 1", duration: "1m 20s", ended: "2 min ago" });
    expect(view.lastRun?.endedTitle).not.toBeNull();
  });

  it("a closed run reads stopped; a run with no end has no duration or age", () => {
    const stopped = { status: "stopped", started_at_ms: NOW - 5000, ended_at_ms: NOW, exit_code: null };
    expect(recordView({ kind: "agent", name: "a", lastRun: stopped }, NOW).lastRun?.outcome).toBe("stopped");
    const open = { status: "exited", started_at_ms: NOW - 5000, ended_at_ms: null, exit_code: null };
    expect(recordView({ kind: "agent", name: "a", lastRun: open }, NOW).lastRun).toEqual({
      outcome: "exited",
      duration: null,
      ended: null,
      endedTitle: null,
    });
  });
});

describe("time formatting", () => {
  it("formats durations at their unit boundaries", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(59_999)).toBe("59s");
    expect(formatDuration(60_000)).toBe("1m 0s");
    expect(formatDuration(3_599_000)).toBe("59m 59s");
    expect(formatDuration(3_600_000)).toBe("1h 0m");
  });

  it("formats ages at their unit boundaries", () => {
    const now = 10_000_000_000;
    expect(formatAgo(now - 44_000, now)).toBe("just now");
    expect(formatAgo(now - 45_000, now)).toBe("1 min ago");
    expect(formatAgo(now - 120_000, now)).toBe("2 min ago");
    expect(formatAgo(now - 3_600_000, now)).toBe("1 h ago");
    expect(formatAgo(now - 86_400_000, now)).toBe("1 d ago");
    // A clock that ran backwards never reads as the future.
    expect(formatAgo(now + 5000, now)).toBe("just now");
  });

  it("the frozen strip drops the code and the age it does not know", () => {
    const now = 10_000_000_000;
    expect(frozenStripText({ status: "exited", exitCode: 1, endedAtMs: now - 120_000 }, now)).toBe(
      "exited · code 1 · 2 min ago",
    );
    expect(frozenStripText({ status: "failed", exitCode: null, endedAtMs: null }, now)).toBe("failed");
  });
});

describe("RecordPane", () => {
  function mounted(): { pane: RecordPane; onAction: ReturnType<typeof vi.fn>; onNewAgent: ReturnType<typeof vi.fn> } {
    const onAction = vi.fn();
    const onNewAgent = vi.fn();
    const pane = new RecordPane({ onAction, onNewAgent });
    document.body.appendChild(pane.host);
    pane.show(recordView({ kind: "agent", name: "claude", lastRun: null }, 0));
    pane.setActive(true);
    return { pane, onAction, onNewAgent };
  }
  const key = (el: HTMLElement, init: KeyboardEventInit): void => {
    el.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init }));
  };

  it("renders the card with its keycap hints", () => {
    const { pane } = mounted();
    expect(pane.host.querySelector(".chappa-record-title")?.textContent).toBe("claude is stopped");
    expect([...pane.host.querySelectorAll(".chappa-record-action")].map((b) => b.textContent)).toEqual([
      "▶ Start new session",
    ]);
    expect([...pane.host.querySelectorAll(".chappa-keycap")].map((k) => k.textContent)).toEqual(["S", "Enter", "Ctrl+T"]);
    expect(document.activeElement).toBe(pane.host);
    pane.dispose();
  });

  it("S and Enter run the primary action; Ctrl+T asks for a new agent", () => {
    const { pane, onAction, onNewAgent } = mounted();
    key(pane.host, { key: "s" });
    key(pane.host, { key: "S" });
    key(pane.host, { key: "Enter" });
    expect(onAction.mock.calls).toEqual([["start"], ["start"], ["start"]]);
    key(pane.host, { key: "t", ctrlKey: true });
    expect(onNewAgent).toHaveBeenCalledTimes(1);
    // Modified keys are not the primary action.
    key(pane.host, { key: "s", ctrlKey: true });
    key(pane.host, { key: "S", shiftKey: true });
    key(pane.host, { key: "Enter", altKey: true });
    expect(onAction).toHaveBeenCalledTimes(3);
    pane.dispose();
  });

  it("the button runs the action too", () => {
    const { pane, onAction } = mounted();
    pane.host.querySelector<HTMLButtonElement>(".chappa-record-action")!.click();
    expect(onAction).toHaveBeenCalledWith("start");
    pane.dispose();
  });

  it("renders a text tail as monospace lines and an events tail as transcript blocks", () => {
    const { pane } = mounted();
    pane.setTail({ kind: "text", lines: ["$ make", "error: boom"] });
    expect(pane.host.querySelector(".chappa-record-tail-text")?.textContent).toBe("$ make\nerror: boom");
    pane.setTail({
      kind: "events",
      lines: [
        { id: 1, seq: 1, ts: 0, kind: "turn_started", payload: { model: "m" } },
        { id: 1, seq: 2, ts: 0, kind: "text", payload: { text: "all done" } },
      ],
    });
    expect(pane.host.querySelector(".chappa-record-tail-text")).toBeNull();
    expect(pane.host.querySelector(".chappa-record-tail-events .chappa-transcript-text")?.textContent).toBe("all done");
    pane.setTail(null);
    expect(pane.host.querySelector(".chappa-record-tail")?.childNodes.length).toBe(0);
    pane.dispose();
  });

  it("the last-run line reads outcome, duration and age; the summary slot stays empty", () => {
    const { pane } = mounted();
    pane.setRun({ outcome: "exit 1", duration: "3s", ended: "just now", endedTitle: "t" });
    expect(pane.host.querySelector(".chappa-record-run")?.textContent).toBe("last run · exit 1 · ran 3s · ended just now");
    expect(pane.host.querySelector(".chappa-record-summary")?.textContent).toBe("");
    pane.dispose();
  });
});

describe("FrozenStrip", () => {
  it("shows the words and the start action, or no button for an ephemeral row", () => {
    const strip = new FrozenStrip();
    const run = vi.fn();
    strip.show("exited · code 1", { label: "▶ Start new session", run });
    expect(strip.shown).toBe(true);
    const btn = strip.el.querySelector<HTMLButtonElement>(".chappa-frozen-strip-start")!;
    expect(btn.textContent).toBe("▶ Start new session");
    btn.click();
    expect(run).toHaveBeenCalledTimes(1);
    strip.show("exited", null);
    expect(btn.style.display).toBe("none");
    strip.hide();
    expect(strip.shown).toBe(false);
  });
});
