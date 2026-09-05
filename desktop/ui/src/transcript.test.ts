// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { mergeBySeq, TranscriptPanel, usageLine } from "./transcript";
import type * as ipc from "./ipc";

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

function event(seq: number, kind: ipc.AgentEventKind, payload: Record<string, unknown> = {}): ipc.AgentEventDto {
  return { id: 5, seq, ts: 1, kind, payload };
}

function mount(ring: ipc.AgentEventDto[] = []) {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const sent: Array<[number, string]> = [];
  const api = {
    sendAgentInput: vi.fn(async (id: number, text: string): Promise<ipc.TurnReceiptDto> => {
      sent.push([id, text]);
      return { delivered: true, waited_ms: 3, seq_before: 0, turn_started_seq: 1 };
    }),
    getAgentEvents: vi.fn(async () => ring),
  };
  const created: number[] = [];
  const sentHook: string[] = [];
  const panel = new TranscriptPanel({
    container,
    api,
    spawn: async () => 5,
    onCreated: (id) => created.push(id),
    onSent: (_id, text) => sentHook.push(text),
  });
  return { container, api, panel, sent, created, sentHook };
}

describe("TranscriptPanel", () => {
  it("start spawns, fires onCreated, replays the ring; every kind renders its block", async () => {
    const ring = [
      event(1, "turn_started", { model: "claude-haiku" }),
      event(2, "text", { text: "thinking hard", thinking: true }),
      event(3, "tool_call", { phase: "call", name: "Bash", input: { command: "ls" } }),
      event(4, "tool_call", { phase: "result", tool_use_id: "t1", content: [{ type: "text", text: "a\nb" }], is_error: false }),
      event(5, "text", { text: "done" }),
      event(6, "usage", { input_tokens: 10, output_tokens: 64, context_pct: 3.4, total_cost_usd: 0.015017 }),
      event(7, "turn_ended", { subtype: "success" }),
      event(8, "compaction", {}),
      event(9, "error", { message: "boom" }),
      event(10, "raw", { type: "system", subtype: "thinking_tokens", estimated_tokens: 3 }),
      event(11, "raw", { stderr: "warn: x" }),
      event(12, "awaiting_input", { after: "result" }),
    ];
    const { container, panel, created } = mount(ring);
    expect(container.classList.contains("chappa-transcript-host")).toBe(true);
    const id = await panel.start();
    expect(id).toBe(5);
    expect(created).toEqual([5]);
    const log = container.querySelector(".chappa-transcript-log")!;
    const seps = log.querySelectorAll(".chappa-transcript-sep");
    expect(seps[0].textContent).toBe("turn 1 · claude-haiku");
    expect(seps[1].textContent).toBe("context compacted");
    const thinking = log.querySelector<HTMLDetailsElement>(".chappa-transcript-thinking")!;
    expect(thinking.open).toBe(false);
    expect(thinking.textContent).toContain("thinking hard");
    const tools = log.querySelectorAll<HTMLDetailsElement>(".chappa-transcript-tool");
    expect(tools.length).toBe(2);
    expect(tools[0].dataset.phase).toBe("call");
    expect(tools[0].querySelector("summary")?.textContent).toBe("▸ Bash");
    expect(tools[0].open).toBe(false);
    expect(tools[0].querySelector("pre")?.textContent).toBe('{\n  "command": "ls"\n}');
    expect(tools[1].dataset.phase).toBe("result");
    expect(tools[1].querySelector("pre")?.textContent).toBe("a\nb");
    const texts = [...log.querySelectorAll(".chappa-transcript-text")].map((t) => t.textContent);
    expect(texts[1]).toBe("done");
    expect(log.querySelector(".chappa-transcript-usage")?.textContent).toBe("in 10 · out 64 · ctx 3.4% · $0.0150");
    expect(log.querySelector(".chappa-transcript-error")?.textContent).toBe("boom");
    const raws = log.querySelectorAll<HTMLDetailsElement>(".chappa-transcript-raw");
    expect(raws[0].querySelector("summary")?.textContent).toBe("raw · system/thinking_tokens");
    expect(raws[1].querySelector("summary")?.textContent).toBe("stderr");
    expect(raws[1].querySelector("pre")?.textContent).toBe("warn: x");
    // turn_ended draws nothing; awaiting_input lights the box.
    expect(log.querySelectorAll("[data-seq]").length).toBe(10);
    expect(panel.waitingForInput).toBe(true);
    // Replayed seqs are not rendered twice by a later live delivery.
    panel.apply(event(5, "text", { text: "done" }));
    expect(log.querySelectorAll(".chappa-transcript-text").length).toBe(2);
    panel.dispose(false);
  });

  it("review fix: a live event during the replay await is held, merged by seq with the history (deduped, ordered), rendered once", async () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    let answer: (ring: ipc.AgentEventDto[]) => void = () => {};
    const ring = new Promise<ipc.AgentEventDto[]>((resolve) => (answer = resolve));
    const api = {
      sendAgentInput: vi.fn(async (): Promise<ipc.TurnReceiptDto> => ({ delivered: true, waited_ms: 0, seq_before: 0 })),
      getAgentEvents: vi.fn(() => ring),
    };
    const panel = new TranscriptPanel({
      container,
      api,
      spawn: async () => 5,
      onCreated: () => {
        // The App forwards live events the moment the id is known — while
        // the ring request is still in flight.
        panel.apply(event(4, "text", { text: "live-4" }));
        panel.apply(event(3, "turn_ended", {}));
      },
    });
    const started = panel.start();
    await FLUSH();
    const log = container.querySelector(".chappa-transcript-log")!;
    expect(log.querySelectorAll("[data-seq]").length).toBe(0);
    answer([event(1, "turn_started", { model: "m" }), event(2, "text", { text: "history-2" }), event(3, "turn_ended", {})]);
    expect(await started).toBe(5);
    const seqs = [...log.querySelectorAll<HTMLElement>("[data-seq]")].map((el) => el.dataset.seq);
    expect(seqs).toEqual(["1", "2", "4"]);
    const texts = [...log.querySelectorAll(".chappa-transcript-text")].map((t) => t.textContent);
    expect(texts).toEqual(["history-2", "live-4"]);
    // After the replay, live events render directly; older seqs are ignored.
    panel.apply(event(5, "text", { text: "live-5" }));
    panel.apply(event(2, "text", { text: "dup" }));
    expect([...log.querySelectorAll(".chappa-transcript-text")].map((t) => t.textContent)).toEqual(["history-2", "live-4", "live-5"]);
    expect(mergeBySeq([event(2, "text"), event(1, "text")], [event(2, "raw"), event(3, "text")]).map((e) => [e.seq, e.kind])).toEqual([
      [1, "text"],
      [2, "text"],
      [3, "text"],
    ]);
    panel.dispose(false);
  });

  it("Enter sends (Shift+Enter inserts), echoes a `you` block, clears waiting and calls onSent; failures are shown, not swallowed", async () => {
    const { container, panel, api, sent, sentHook } = mount([event(1, "awaiting_input")]);
    await panel.start();
    expect(panel.waitingForInput).toBe(true);
    const input = container.querySelector<HTMLTextAreaElement>(".chappa-transcript-input")!;
    // Empty text is ignored.
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    await FLUSH();
    expect(sent).toEqual([]);
    input.value = "line one\nline two ";
    const shift = new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true, cancelable: true });
    input.dispatchEvent(shift);
    expect(shift.defaultPrevented).toBe(false);
    expect(sent).toEqual([]);
    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    input.dispatchEvent(enter);
    expect(enter.defaultPrevented).toBe(true);
    await FLUSH();
    expect(sent).toEqual([[5, "line one\nline two"]]);
    expect(sentHook).toEqual(["line one\nline two"]);
    expect(container.querySelector(".chappa-transcript-you")?.textContent).toBe("line one\nline two");
    expect(panel.waitingForInput).toBe(false);
    expect(input.value).toBe("");
    // The Send button is the same path.
    input.value = "again";
    container.querySelector<HTMLButtonElement>(".chappa-transcript-send")!.click();
    await FLUSH();
    expect(sent.length).toBe(2);
    // A refused receipt (exited/busy) shows inline; a timeout does not (it
    // is written-but-unacknowledged, the next turn separator settles it).
    api.sendAgentInput.mockImplementationOnce(async () => ({ delivered: false, reason: "busy", waited_ms: 0, seq_before: 0 }));
    input.value = "x";
    container.querySelector<HTMLButtonElement>(".chappa-transcript-send")!.click();
    await FLUSH();
    expect([...container.querySelectorAll(".chappa-transcript-error")].map((e) => e.textContent)).toEqual(["not delivered: busy"]);
    api.sendAgentInput.mockImplementationOnce(async () => ({ delivered: false, reason: "timeout", waited_ms: 5000, seq_before: 0 }));
    input.value = "y";
    container.querySelector<HTMLButtonElement>(".chappa-transcript-send")!.click();
    await FLUSH();
    expect(container.querySelectorAll(".chappa-transcript-error").length).toBe(1);
    api.sendAgentInput.mockImplementationOnce(async () => {
      throw new Error("ipc down");
    });
    input.value = "z";
    container.querySelector<HTMLButtonElement>(".chappa-transcript-send")!.click();
    await FLUSH();
    await FLUSH();
    expect(container.querySelectorAll(".chappa-transcript-error").length).toBe(2);
    expect(container.querySelectorAll(".chappa-transcript-error")[1].textContent).toBe("send failed: ipc down");
    // Nothing after dispose.
    panel.dispose(false);
    panel.apply(event(9, "text", { text: "late" }));
    expect(container.textContent).toBe("");
  });

  it("PanelHost surface: activation focuses the box, pasteText inserts at the caret, GL/font hooks are inert", async () => {
    const { container, panel } = mount();
    await panel.start();
    const input = container.querySelector<HTMLTextAreaElement>(".chappa-transcript-input")!;
    panel.setActive(false);
    input.blur();
    panel.setActive(true);
    expect(document.activeElement).toBe(input);
    input.value = "ab";
    input.selectionStart = input.selectionEnd = 1;
    panel.pasteText("/p/x");
    expect(input.value).toBe("a/p/xb");
    panel.setActive(false);
    panel.pasteText("nope");
    expect(input.value).toBe("a/p/xb");
    expect(panel.glLive).toBe(false);
    panel.releaseGlContext();
    panel.restoreGlContext();
    panel.setSubprocessCount(3);
    await panel.setFontMetrics("x", 12, 1.2);
    expect(await panel.start()).toBe(5); // idempotent
    panel.dispose(false);
  });

  it("usageLine formats tokens, context and cost", () => {
    expect(usageLine({ input_tokens: 26832, output_tokens: 2, context_tokens: 26832, total_cost_usd: 0 })).toBe("in 26.8k · out 2 · ctx 26.8k");
    expect(usageLine({ input_tokens: 150000, output_tokens: 1200, context_pct: 78.2, total_cost_usd: 1.5 })).toBe("in 150k · out 1.2k · ctx 78.2% · $1.5000");
    expect(usageLine({})).toBe("");
  });
});
