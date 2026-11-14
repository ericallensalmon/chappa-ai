// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { LocalStatusBlock, gateText, historyLine, statusMeta } from "./local_status";
import { fixture } from "./test-utils";
import type { SourceViewDeps } from "./view_common";
import { readLocalStatus } from "./work_model";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:00:00Z");
const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };
const target = { sourceId: "fx", sourceLabel: "Fixture", view: "work", key: "ABC-1" };

const approve: ipc.SourceActionDto = {
  action: { kind: "endpoint", id: "approve", label: "Approve", mutates: true, confirm: "Approve ABC-1?", params: { to: "done" }, note: "optional" },
  verdict: ok,
  needsPrompt: false,
};
const sendBack: ipc.SourceActionDto = {
  action: { kind: "endpoint", id: "in-progress", label: "Send back", mutates: true, confirm: "Send ABC-1 back?", params: null, note: null },
  verdict: ok,
  needsPrompt: false,
};

function deps(entries: ipc.SourceActionDto[], over: Partial<SourceViewDeps> = {}): SourceViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async () => entries),
    run: vi.fn(async () => ({ kind: "done", message: "Moved" }) as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => NOW,
    said,
    ...over,
  };
}

const fixtureStatus = () => readLocalStatus(fixture("work").items[0].localStatus)!;
const move = (b: LocalStatusBlock, id: string) => b.el.querySelector<HTMLElement>(`.chappa-ls-move[data-move="${id}"]`)!;

afterEach(() => {
  document.querySelectorAll(".chappa-dialog").forEach((d) => d.remove());
  document.body.textContent = "";
});

describe("the status texts", () => {
  it("gate, meta and history lines", () => {
    expect(gateText("human")).toBe("Only a person can move it on.");
    expect(gateText("human-exit")).toBe("Only a person can move it out of this status.");
    expect(gateText("any")).toBe("");
    expect(gateText(null)).toBe("");
    expect(statusMeta(fixtureStatus(), NOW)).toBe("since 18 h ago · by Alex · round 2");
    expect(statusMeta({ id: "s", label: "S", allowed: [] }, NOW)).toBe("");
    expect(historyLine({ at: "2026-01-14T16:00:00Z", from: "in-progress", to: "in-review", actor: "Alex", note: "Ready" }, NOW)).toBe(
      "18 h ago · Alex · in-progress → in-review · “Ready”",
    );
    expect(historyLine({ at: "garbage", to: "done" }, NOW)).toBe("garbage · ? → done");
  });
});

describe("LocalStatusBlock", () => {
  it("shows the label, meta, gate and the pending approval prominently; hides for no status", () => {
    const b = new LocalStatusBlock(deps([approve, sendBack]));
    b.update(fixtureStatus(), target, "fx:1");
    expect(b.el.hidden).toBe(false);
    expect(b.el.querySelector(".chappa-ls-label")?.textContent).toBe("In review");
    expect(b.el.querySelector(".chappa-ls-meta")?.textContent).toBe("since 18 h ago · by Alex · round 2");
    expect(b.el.querySelector(".chappa-ls-gate")?.textContent).toBe("Only a person can move it out of this status.");
    const pending = b.el.querySelector<HTMLElement>(".chappa-ls-pending")!;
    expect(pending.hidden).toBe(false);
    expect(pending.querySelector(".chappa-ls-pending-title")?.textContent).toBe("Waiting for approval: move to done");
    expect(pending.querySelector(".chappa-ls-pending-by")?.textContent).toBe("asked by Alex");
    expect(pending.querySelector(".chappa-ls-pending-note")?.textContent).toBe("Looks complete");
    b.update({ ...fixtureStatus(), pendingApproval: null }, target, "fx:1");
    expect(b.el.querySelector<HTMLElement>(".chappa-ls-pending")!.hidden).toBe(true);
    b.update(null, target, "fx:1");
    expect(b.el.hidden).toBe(true);
  });

  it("the history is a compact timeline, newest first, overrides flagged", () => {
    const b = new LocalStatusBlock(deps([]));
    b.update(fixtureStatus(), target, "fx:1");
    const entries = [...b.el.querySelectorAll<HTMLElement>(".chappa-ls-entry")];
    expect(entries.map((e) => e.firstElementChild?.textContent)).toEqual([
      "18 h ago · Alex · in-progress → in-review · “Ready”",
      "2 d ago · Alex · todo → in-progress",
    ]);
    expect(entries.map((e) => e.hasAttribute("data-override"))).toEqual([true, false]);
    expect(entries[0].querySelector(".chappa-sv-chip")?.textContent).toBe("override: skipped checks");
  });

  it("moves are the item's actions matched by id; a person-only move is marked; an unmatched move is disabled with why", async () => {
    const d = deps([approve]);
    const b = new LocalStatusBlock(d);
    b.update(fixtureStatus(), target, "fx:1");
    await FLUSH();
    expect(d.actions).toHaveBeenCalledWith("fx", "work", "ABC-1");
    const a = move(b, "approve");
    expect(a.querySelector("button")?.textContent).toBe("Approve");
    expect(a.querySelector("button")?.disabled).toBe(false);
    expect(a.querySelector(".chappa-sv-chip")?.textContent).toBe("person only");
    const back = move(b, "in-progress").querySelector("button")!;
    expect([back.textContent, back.disabled, back.title]).toEqual(["Send back", true, "The source offers no action for this move."]);
    // Same version: not asked again; a new one is.
    b.update(fixtureStatus(), target, "fx:1");
    expect(d.actions).toHaveBeenCalledTimes(1);
    b.update(fixtureStatus(), target, "fx:2");
    await FLUSH();
    expect(d.actions).toHaveBeenCalledTimes(2);
  });

  it("a disabled verdict shows its reason; a hidden one is not drawn", async () => {
    const b = new LocalStatusBlock(
      deps([
        { ...approve, verdict: { ...ok, enabled: false, reason: "the source is down" } },
        { ...sendBack, verdict: { ...ok, enabled: false, hidden: true } },
      ]),
    );
    b.update(fixtureStatus(), target, "fx:1");
    await FLUSH();
    expect(move(b, "approve").querySelector("button")?.title).toBe("the source is down");
    expect(move(b, "in-progress")).toBeNull();
  });

  it("a move runs through runAction: its confirm first, confirmed only after the answer, the outcome said", async () => {
    const d = deps([approve, sendBack]);
    const b = new LocalStatusBlock(d);
    b.update(fixtureStatus(), target, "fx:1");
    await FLUSH();
    move(b, "approve").querySelector("button")!.click();
    await FLUSH();
    // The action's note is optional: one prompt with the confirm text.
    expect(document.querySelector(".chappa-dialog-message")?.textContent).toContain("Approve ABC-1?");
    document.querySelector<HTMLInputElement>(".chappa-dialog-input")!.value = "ship it";
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "work", "ABC-1", "approve", true, "ship it");
    expect(d.said).toEqual(["Moved"]);
  });

  it("requireNote forces the note dialog even when the action takes no note, and refuses an empty one", async () => {
    const d = deps([approve, sendBack]);
    const b = new LocalStatusBlock(d);
    b.update(fixtureStatus(), target, "fx:1");
    await FLUSH();
    const btn = move(b, "in-progress").querySelector("button")!;
    expect(btn.textContent).toBe("Send back…");
    expect(btn.title).toContain("a note is required");
    btn.click();
    await FLUSH();
    expect(document.querySelector(".chappa-dialog-message")?.textContent).toContain("A note is required.");
    document.querySelector<HTMLInputElement>(".chappa-dialog-input")!.value = "  ";
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(d.run).not.toHaveBeenCalled();
    // Asked again; a real note goes out with the confirmation.
    document.querySelector<HTMLInputElement>(".chappa-dialog-input")!.value = "missing tests";
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "work", "ABC-1", "in-progress", true, "missing tests");
  });

  it("backing out of the dialog sends nothing; a failure is said", async () => {
    const d = deps([sendBack], { run: vi.fn(async () => Promise.reject(new Error("source is down"))) });
    const b = new LocalStatusBlock(d);
    b.update(fixtureStatus(), target, "fx:1");
    await FLUSH();
    move(b, "in-progress").querySelector("button")!.click();
    await FLUSH();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-cancel")!.click();
    await FLUSH();
    expect(d.run).not.toHaveBeenCalled();
    move(b, "in-progress").querySelector("button")!.click();
    await FLUSH();
    document.querySelector<HTMLInputElement>(".chappa-dialog-input")!.value = "why";
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(d.said).toEqual(["Send back: source is down"]);
  });
});
