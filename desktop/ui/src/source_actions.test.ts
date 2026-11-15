// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type { ActionVerdictDto, SourceActionDto } from "./ipc";
import {
  actionButtonState,
  agentPromptMessage,
  checkInputValues,
  confirmAgentPrompt,
  confirmEndpoint,
  endpointQuestion,
  outcomeText,
  runAction,
} from "./source_actions";

const ok: ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

function entry(action: SourceActionDto["action"], verdict: Partial<ActionVerdictDto> = {}, needsPrompt = false): SourceActionDto {
  return { action, verdict: { ...ok, ...verdict }, needsPrompt };
}

const agent = {
  kind: "agent" as const,
  id: "agent-here",
  label: "Agent here",
  cwd: "/srv/example/abc-1",
  prompt: "Review ABC-1 and summarise what is left.",
  tool: "claude",
  title: "ABC-1 review",
  env: { CHAPPA_STACK_SLOT: "1" },
};

describe("actionButtonState", () => {
  it("enables an allowed action with a title that says what it does", () => {
    const state = actionButtonState(entry(agent, { cwd: "C:\\wt\\abc-1", tool: [1, "claude"] }, true));
    expect(state).toEqual({ label: "Agent here", disabled: false, title: "Launch claude in C:\\wt\\abc-1", hidden: false, asksFirst: true });
    const link = actionButtonState(entry({ kind: "link", id: "o", label: "Open", url: "https://registry.example/x" }));
    expect(link.title).toBe("Open https://registry.example/x");
    const mutating = actionButtonState(entry({ kind: "endpoint", id: "a", label: "Approve", mutates: true, confirm: "Approve?", params: null, note: null }));
    expect(mutating.title).toBe("Changes something; asks first");
    expect(mutating.asksFirst).toBe(false);
  });

  it("disables with the verdict's reason and hides what must not be drawn", () => {
    const disabled = actionButtonState(entry(agent, { enabled: false, reason: "`/srv/example/abc-1` is not inside an open project" }));
    expect(disabled.disabled).toBe(true);
    expect(disabled.title).toContain("not inside an open project");
    expect(disabled.hidden).toBe(false);
    const hidden = actionButtonState(
      entry({ kind: "endpoint", id: "u", label: "Force", mutates: true, confirm: null, params: null, note: null }, { enabled: false, hidden: true, reason: "no confirm" }),
    );
    expect(hidden.hidden).toBe(true);
    expect(hidden.disabled).toBe(true);
    // A blank label falls back to the id so the button is never empty.
    expect(actionButtonState(entry({ ...agent, label: "  " })).label).toBe("agent-here");
  });
});

describe("endpoint questions and the agent prompt message", () => {
  it("asks only for mutating actions and carries the note rule", () => {
    expect(endpointQuestion({ kind: "endpoint", id: "p", label: "Ping", mutates: false, confirm: null, params: null, note: null })).toEqual({ confirm: null, note: null });
    expect(endpointQuestion({ kind: "endpoint", id: "a", label: "Approve", mutates: true, confirm: "Approve ABC-1?", params: null, note: "optional" })).toEqual({
      confirm: "Approve ABC-1?",
      note: "optional",
    });
    // A confirm on a non-mutating action is not a question.
    expect(endpointQuestion({ kind: "endpoint", id: "p", label: "Ping", mutates: false, confirm: "Sure?", params: null, note: null }).confirm).toBeNull();
  });

  it("shows the whole prompt, the tool, the folder and every env entry", () => {
    const text = agentPromptMessage(agent, { ...ok, cwd: "C:\\wt\\abc-1", tool: [1, "claude"] }, "Fixture");
    expect(text).toContain("Fixture wants to launch claude");
    expect(text).toContain("in C:\\wt\\abc-1");
    expect(text).toContain("  CHAPPA_STACK_SLOT=1");
    expect(text.endsWith(agent.prompt)).toBe(true);
    // Without a resolved tool the action's own name (or the default) is shown.
    expect(agentPromptMessage({ ...agent, tool: null, env: {} }, ok, "S")).toContain("the default agent tool");
    expect(agentPromptMessage({ ...agent, env: {} }, ok, "S")).not.toContain("with env");
  });

  it("describes every outcome in one line", () => {
    expect(outcomeText({ kind: "done", message: null })).toBe("Done.");
    expect(outcomeText({ kind: "done", message: "pong" })).toBe("pong");
    expect(outcomeText({ kind: "job", job_id: "job-1" })).toContain("job-1");
    expect(outcomeText({ kind: "run", job_id: "job-2", term_id: 7 })).toContain("in a pane here");
    expect(outcomeText({ kind: "uncertain", message: "may or may not" })).toBe("may or may not");
    expect(outcomeText({ kind: "opened", url: "https://x" })).toBe("Opened https://x.");
    expect(outcomeText({ kind: "launched", term_id: 3, what: "agent" })).toBe("Launched.");
  });
});

describe("the dialogs", () => {
  const press = (key: string) => document.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  const okButton = () => document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!;

  it("a non-mutating endpoint without a note needs no dialog", async () => {
    const answer = await confirmEndpoint({ kind: "endpoint", id: "p", label: "Ping", mutates: false, confirm: null, params: null, note: null });
    expect(answer).toEqual({ confirmed: false, note: null });
    expect(document.querySelector(".chappa-dialog")).toBeNull();
  });

  it("a mutating endpoint is confirmed only on the OK button", async () => {
    const pending = confirmEndpoint({ kind: "endpoint", id: "a", label: "Approve", mutates: true, confirm: "Approve ABC-1?", params: null, note: null });
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-message")!.textContent).toBe("Approve ABC-1?");
    expect(okButton().textContent).toBe("Approve");
    // Enter while Cancel has focus (the safe default) answers no.
    press("Enter");
    expect(await pending).toBeNull();
    const again = confirmEndpoint({ kind: "endpoint", id: "a", label: "Approve", mutates: true, confirm: "Approve ABC-1?", params: null, note: null });
    await Promise.resolve();
    okButton().click();
    expect(await again).toEqual({ confirmed: true, note: null });
  });

  it("a required note refuses an empty answer and keeps asking", async () => {
    const pending = confirmEndpoint({ kind: "endpoint", id: "d", label: "Deploy", mutates: true, confirm: "Deploy now?", params: null, note: "required" });
    await Promise.resolve();
    const input = () => document.querySelector<HTMLInputElement>(".chappa-dialog-input")!;
    expect(input()).not.toBeNull();
    okButton().click();
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-input")).not.toBeNull();
    input().value = "  hotfix for the login page ";
    okButton().click();
    expect(await pending).toEqual({ confirmed: true, note: "hotfix for the login page" });
  });

  it("the agent prompt dialog launches only on the Launch button", async () => {
    const pending = confirmAgentPrompt(agent, { ...ok, tool: [1, "claude"] }, "Fixture");
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-message")!.textContent).toContain(agent.prompt);
    expect(okButton().textContent).toBe("Launch");
    okButton().click();
    expect(await pending).toBe(true);
  });

  it("runAction asks first and sends confirmed only after the answer", async () => {
    const run = vi.fn(async () => ({ kind: "launched" as const, term_id: 9, what: "agent" as const }));
    // Needs the prompt: the dialog opens; cancelling sends nothing.
    const cancelled = runAction("src", "Fixture", "work", "ABC-1", entry(agent, {}, true), { run });
    await Promise.resolve();
    press("Escape");
    expect(await cancelled).toBeNull();
    expect(run).not.toHaveBeenCalled();
    // Accepting sends confirmed: true.
    const launched = runAction("src", "Fixture", "work", "ABC-1", entry(agent, {}, true), { run });
    await Promise.resolve();
    okButton().click();
    expect(await launched).toEqual({ kind: "launched", term_id: 9, what: "agent" });
    expect(run).toHaveBeenLastCalledWith("src", "work", "ABC-1", "agent-here", true, null);
    // Already shown: no dialog, still the user's click, still confirmed.
    await runAction("src", "Fixture", "work", "ABC-1", entry(agent, {}, false), { run });
    expect(document.querySelector(".chappa-dialog")).toBeNull();
    expect(run).toHaveBeenLastCalledWith("src", "work", "ABC-1", "agent-here", true, null);
    // A link needs nothing and sends confirmed: false.
    await runAction("src", "Fixture", "work", "ABC-1", entry({ kind: "link", id: "o", label: "Open", url: "https://x" }), { run });
    expect(run).toHaveBeenLastCalledWith("src", "work", "ABC-1", "o", false, null);
  });

  it("an item action on another source's row: its filled confirm is the question, and the call names the owner", async () => {
    const run = vi.fn(async () => ({ kind: "done" as const, message: "ok" }));
    const start = { kind: "endpoint" as const, id: "start-work", label: "Start work", mutates: true, confirm: "Create a workspace for ABC-3 — Retire the old queue", params: null, note: null };
    const pending = runAction("ws", "Workspaces", "work", "ABC-3", entry(start), { run }, { owner: "fx" });
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-message")!.textContent).toBe("Create a workspace for ABC-3 — Retire the old queue");
    okButton().click();
    await pending;
    expect(run).toHaveBeenLastCalledWith("ws", "work", "ABC-3", "start-work", true, null, "fx");
  });

  it("an action with inputs asks for them in one dialog and sends the values", async () => {
    const run = vi.fn(async () => ({ kind: "done" as const, message: "ok" }));
    const seeds = {
      kind: "endpoint" as const,
      id: "set-seeds",
      label: "State seeds",
      mutates: true,
      confirm: "Set the state seeds ABC-1 asks for",
      params: {},
      note: null,
      inputs: [
        { name: "seeds", label: "State seeds", kind: "multi" as const, options: [{ value: "db/empty", label: "db: empty" }, { value: "db/three", label: "db: three" }], required: false, default: ["db/empty"], max: null },
        { name: "size", label: "Size", kind: "choice" as const, options: [{ value: "s", label: "Small" }, { value: "l", label: "Large" }], required: true, default: null, max: null },
        { name: "count", label: "Count", kind: "number" as const, required: false, default: 3, max: 10 },
        { name: "why", label: "Why", kind: "text" as const, required: false, default: null, max: 12 },
      ],
    };
    const pending = runAction("st", "Stack", "work", "ABC-1", entry(seeds), { run });
    await Promise.resolve();
    const dialog = document.querySelector<HTMLElement>(".chappa-dialog")!;
    expect(dialog.querySelector(".chappa-dialog-message")!.textContent).toBe("Set the state seeds ABC-1 asks for");
    // One control per input, each starting at its default.
    const checks = [...dialog.querySelectorAll<HTMLInputElement>('[data-name="seeds"] input[type=checkbox]')];
    expect(checks.map((c) => [c.value, c.checked])).toEqual([["db/empty", true], ["db/three", false]]);
    const size = dialog.querySelector<HTMLSelectElement>('select[data-name="size"]')!;
    expect([...size.options].map((o) => o.value)).toEqual(["", "s", "l"]);
    expect(size.value).toBe("");
    const count = dialog.querySelector<HTMLInputElement>('input[data-name="count"]')!;
    expect([count.type, count.value, count.max]).toEqual(["number", "3", "10"]);
    const why = dialog.querySelector<HTMLInputElement>('input[data-name="why"]')!;
    expect([why.type, why.maxLength]).toEqual(["text", 12]);
    // A required one left empty keeps the dialog open and says why.
    okButton().click();
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog")).not.toBeNull();
    expect(document.querySelector(".chappa-dialog-error")!.textContent).toBe("Size is required.");
    expect(run).not.toHaveBeenCalled();
    size.value = "l";
    count.value = "11";
    okButton().click();
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-error")!.textContent).toBe("Count must be at most 10.");
    count.value = "4";
    checks[1].checked = true;
    okButton().click();
    await pending;
    // Empty ones are left out; the values ride with the call.
    expect(run).toHaveBeenLastCalledWith("st", "work", "ABC-1", "set-seeds", true, null, undefined, { seeds: ["db/empty", "db/three"], size: "l", count: 4 });
  });

  it("a note asked beside inputs is in the same dialog; an item action keeps its owner", async () => {
    const run = vi.fn(async () => ({ kind: "done" as const, message: "ok" }));
    const action = {
      kind: "endpoint" as const,
      id: "pick",
      label: "Pick",
      mutates: false,
      confirm: null,
      params: null,
      note: "required" as const,
      inputs: [{ name: "size", label: "Size", kind: "choice" as const, options: [{ value: "s", label: "Small" }], required: false, default: "s", max: null }],
    };
    const pending = runAction("ws", "Workspaces", "work", "ABC-3", entry(action), { run }, { owner: "fx" });
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-message")!.textContent).toBe("Pick?");
    okButton().click();
    await Promise.resolve();
    expect(document.querySelector(".chappa-dialog-error")!.textContent).toBe("A note is required.");
    document.querySelector<HTMLInputElement>(".chappa-dialog-input")!.value = " because ";
    okButton().click();
    await pending;
    expect(run).toHaveBeenLastCalledWith("ws", "work", "ABC-3", "pick", false, "because", "fx", { size: "s" });
    // Cancelling sends nothing.
    run.mockClear();
    const cancelled = runAction("ws", "Workspaces", "work", "ABC-3", entry(action), { run });
    await Promise.resolve();
    press("Escape");
    expect(await cancelled).toBeNull();
    expect(run).not.toHaveBeenCalled();
  });
});

describe("checkInputValues", () => {
  const inputs = [
    { name: "profile", label: "Profile", kind: "choice" as const, options: [{ value: "a", label: "A" }], required: true, default: null, max: null },
    { name: "many", label: "Many", kind: "multi" as const, options: [{ value: "x", label: "X" }, { value: "y", label: "Y" }], required: false, default: null, max: null },
    { name: "text", label: "Text", kind: "text" as const, required: false, default: null, max: null },
    { name: "n", label: "N", kind: "number" as const, required: false, default: null, max: 5 },
  ];
  it("the same rules as Rust: required, options, text length and lines, number range", () => {
    const table: [Record<string, unknown>, string | null][] = [
      [{ profile: "a" }, null],
      [{ profile: "a", many: ["x", "y"], text: "x".repeat(200), n: 5 }, null],
      [{}, "Profile is required."],
      [{ profile: "  " }, "Profile is required."],
      [{ profile: "b" }, "Profile: choose one of the options."],
      [{ profile: "a", many: ["z"] }, "Many: choose among the options."],
      [{ profile: "a", many: ["x", "x"] }, "Many: each option once."],
      [{ profile: "a", text: "x".repeat(201) }, "Text is longer than 200 characters."],
      [{ profile: "a", text: "two\nlines" }, "Text must be one line of printable text."],
      [{ profile: "a", n: 6 }, "N must be at most 5."],
      [{ profile: "a", n: Number.NaN }, "N must be a number."],
      [{ profile: "a", n: "3" }, "N must be a number."],
    ];
    for (const [values, expected] of table) expect(checkInputValues(inputs, values), JSON.stringify(values)).toBe(expected);
  });
});
