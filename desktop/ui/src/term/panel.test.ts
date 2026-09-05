// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { MOD_CTRL } from "../ipc";
import { TerminalPanel, gridForSize, probeCellMetrics, rendererFromQuery, type TerminalApi } from "./panel";
import { stubSettings } from "./test-utils";

function stubApi(): { api: TerminalApi; onFrame: (() => ((buf: ArrayBuffer) => void) | null) } {
  let onFrame: ((buf: ArrayBuffer) => void) | null = null;
  const api: TerminalApi = {
    createTerminal: vi.fn(async (opts) => {
      onFrame = opts.onFrame;
      return 42;
    }),
    writeKey: vi.fn(async () => {}),
    paste: vi.fn(async () => {}),
    mouse: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    scroll: vi.fn(async () => {}),
    setDisplayOffset: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    requestFull: vi.fn(async () => {}),
    selection: vi.fn(async () => {}),
    copySelection: vi.fn(async () => null),
    search: vi.fn(async () => {}),
    searchNav: vi.fn(async () => {}),
    closeTerminal: vi.fn(async () => null),
    debugStats: vi.fn(async () => null),
    listTerminals: vi.fn(async () => []),
    attachTerminal: vi.fn(async () => ({
      id: 0,
      name: "",
      status: "running",
      exit_code: null,
      cols: 80,
      rows: 24,
      seq: 1,
      kind: "terminal" as const,
      project_id: null,
      agent_tool_id: null,
    })),
    listProjects: vi.fn(async () => []),
    addProject: vi.fn(async () => ({ id: 1, name: "p", path: "/p", icon: null, notificationLevel: null })),
    renameProject: vi.fn(async () => {}),
    removeProject: vi.fn(async () => {}),
    pickDirectory: vi.fn(async () => null),
    openProject: vi.fn(async () => ({
      project: { id: 1, name: "p", path: "/p", icon: null, notificationLevel: null },
      trustPending: false,
      trustCommands: [],
      processes: [],
    })),
    confirmProjectTrust: vi.fn(async () => {}),
    listProjectProcesses: vi.fn(async () => []),
    startProjectProcess: vi.fn(async () => 42),
    stopProjectProcess: vi.fn(async () => {}),
    restartProjectProcess: vi.fn(async () => 42),
    setNotificationLevel: vi.fn(async () => {}),
    osNotify: vi.fn(async () => {}),
    saveProjectProcess: vi.fn(async () => []),
    deleteProjectProcess: vi.fn(async () => []),
    duplicateProjectProcesses: vi.fn(async () => []),
    setProcessFavorite: vi.fn(async () => {}),
    setProcessAutoRename: vi.fn(async () => {}),
    listWorkspaceCommands: vi.fn(async () => []),
    saveWorkspaceCommand: vi.fn(async () => []),
    deleteWorkspaceCommand: vi.fn(async () => []),
    startWorkspaceCommand: vi.fn(async () => 42),
    stopWorkspaceCommand: vi.fn(async () => {}),
    restartWorkspaceCommand: vi.fn(async () => 42),
    listAgentTools: vi.fn(async () => ({ tools: [], machine_mode_types: ["claude", "opencode"] as const })),
    upsertAgentTool: vi.fn(async (t) => t),
    deleteAgentTool: vi.fn(async () => {}),
    parseAgentCommand: vi.fn(async () => {
      throw new Error("not stubbed");
    }),
    spawnAgent: vi.fn(async () => {
      throw new Error("not stubbed");
    }),
    sendAgentInput: vi.fn(async () => ({ delivered: true, waited_ms: 0, seq_before: 0 })),
    getAgentEvents: vi.fn(async () => []),
  };
  return { api, onFrame: () => onFrame };
}

function mockHostRect(host: HTMLElement, w: number, h: number) {
  host.getBoundingClientRect = () =>
    ({
      left: 0,
      top: 0,
      right: w,
      bottom: h,
      width: w,
      height: h,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    }) as DOMRect;
}

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

/** Minimal valid full frame: cols×rows cells, no selection, hidden cursor. */
function minimalFrame(
  cols: number,
  rows: number,
  seq = 1,
  selection = false,
  ch = 0x20,
  mouseCapture = false,
): ArrayBuffer {
  const cells = cols * rows;
  const buf = new ArrayBuffer(37 + 6 * rows + cells * 16 + 2 + 2);
  const view = new DataView(buf);
  let p = 0;
  view.setUint8(p++, 0xd7);
  view.setUint8(p++, 1);
  view.setUint32(p, seq, true);
  p += 4;
  view.setUint8(p++, 0); // kind: full
  // flags: bit 0 = selection_active, bit 1 = mouse_capture
  view.setUint8(p++, (selection ? 1 : 0) | (mouseCapture ? 2 : 0));
  view.setUint16(p, 0, true); // cursor row
  p += 2;
  view.setUint16(p, 0, true); // cursor col
  p += 2;
  view.setUint8(p++, 4); // cursor shape: hidden
  view.setUint8(p++, 0); // cursor visible
  view.setUint32(p, 0, true); // display_offset
  p += 4;
  view.setUint32(p, 0, true); // history_len
  p += 4;
  view.setUint8(p++, selection ? 1 : 0); // selection present
  view.setUint32(p, 0, true); // start_row
  p += 4;
  view.setUint16(p, 0, true); // start_col
  p += 2;
  view.setUint32(p, 0, true); // end_row
  p += 4;
  view.setUint16(p, 1, true); // end_col
  p += 2;
  view.setUint16(p, rows, true); // row_count
  p += 2;
  for (let r = 0; r < rows; r++) {
    view.setUint16(p, r, true);
    p += 2;
    view.setUint16(p, 0, true);
    p += 2;
    view.setUint16(p, cols, true);
    p += 2;
    for (let c = 0; c < cols; c++) {
      view.setUint32(p, ch, true);
      p += 4;
      view.setUint32(p, 0xd8d8d8ff, true); // fg
      p += 4;
      view.setUint32(p, 0x181818ff, true); // bg
      p += 4;
      view.setUint16(p, 0, true); // flags
      p += 2;
      view.setUint16(p, 0, true); // link
      p += 2;
    }
  }
  view.setUint16(p, 0, true); // zerowidth_count
  p += 2;
  view.setUint16(p, 0, true); // match_count
  p += 2;
  return buf;
}

/** Minimal delta frame: only the listed rows are covered; cells are 'X'.
 *  Dims are implicit — a delta applies onto the caller's retained store
 *  (decodeFrame uses the store's grid). */
function deltaFrame(seq: number, covered: { row: number; colStart: number; cellCount: number }[]): ArrayBuffer {
  const cells = covered.reduce((n, c) => n + c.cellCount, 0);
  const buf = new ArrayBuffer(37 + covered.length * 6 + cells * 16 + 2 + 2);
  const view = new DataView(buf);
  let p = 0;
  view.setUint8(p++, 0xd7);
  view.setUint8(p++, 1);
  view.setUint32(p, seq, true);
  p += 4;
  view.setUint8(p++, 1); // kind: delta
  view.setUint8(p++, 0); // flags
  view.setUint16(p, 0, true); // cursor row
  p += 2;
  view.setUint16(p, 0, true); // cursor col
  p += 2;
  view.setUint8(p++, 4); // cursor shape: hidden
  view.setUint8(p++, 0); // cursor visible
  view.setUint32(p, 0, true); // display_offset
  p += 4;
  view.setUint32(p, 0, true); // history_len
  p += 4;
  view.setUint8(p++, 0); // selection present
  view.setUint32(p, 0, true); // start_row
  p += 4;
  view.setUint16(p, 0, true); // start_col
  p += 2;
  view.setUint32(p, 0, true); // end_row
  p += 4;
  view.setUint16(p, 0, true); // end_col
  p += 2;
  view.setUint16(p, covered.length, true); // row_count
  p += 2;
  for (const c of covered) {
    view.setUint16(p, c.row, true);
    p += 2;
    view.setUint16(p, c.colStart, true);
    p += 2;
    view.setUint16(p, c.cellCount, true);
    p += 2;
    for (let j = 0; j < c.cellCount; j++) {
      view.setUint32(p, 0x58, true); // ch = 'X'
      p += 4;
      view.setUint32(p, 0xd8d8d8ff, true); // fg
      p += 4;
      view.setUint32(p, 0x181818ff, true); // bg
      p += 4;
      view.setUint16(p, 0, true); // flags
      p += 2;
      view.setUint16(p, 0, true); // link
      p += 2;
    }
  }
  view.setUint16(p, 0, true); // zerowidth_count
  p += 2;
  view.setUint16(p, 0, true); // match_count
  p += 2;
  return buf;
}

describe("gridForSize", () => {
  it("floors the grid to whole cells", () => {
    expect(gridForSize(800, 400, 10, 20)).toEqual({ cols: 80, rows: 20 });
    expect(gridForSize(805, 419, 10, 20)).toEqual({ cols: 80, rows: 20 });
  });

  it("never reports a zero-dimension grid", () => {
    expect(gridForSize(5, 400, 10, 20)).toEqual({ cols: 1, rows: 20 });
    expect(gridForSize(0, 0, 10, 20)).toEqual({ cols: 1, rows: 1 });
  });
});

describe("probeCellMetrics", () => {
  it("falls back to heuristic metrics when the DOM cannot lay out (jsdom)", () => {
    const m = probeCellMetrics("JetBrains Mono", 14);
    expect(m.cellW).toBe(Math.round(14 * 0.6));
    // Height is the line-height model (fontSize × 1.2), not a layout
    // measurement — deterministic across fonts.
    expect(m.cellH).toBe(16.8);
  });
});

describe("TerminalPanel", () => {
  it("requests the default shell (empty command) on win32 — Rust resolves pwsh/powershell", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Win32" });
    mockHostRect(container, 800, 400);
    await panel.start();
    expect(api.createTerminal).toHaveBeenCalledTimes(1);
    const spec = (api.createTerminal as ReturnType<typeof vi.fn>).mock.calls[0][0].spec;
    expect(spec.command).toBe("");
    panel.dispose();
  });

  it("a press inside the context menu does not dismiss it before the click lands", async () => {
    // REGRESSION: THE CAPTURE-PHASE document dismiss ran before the
    // menu buttons' click could fire — Copy/Paste were unclickable.
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    const viewport = container.querySelector("div")!;
    viewport.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    const menus = document.querySelectorAll<HTMLDivElement>(".chappa-term-menu");
    const menu = menus[menus.length - 1];
    expect(menu.style.display).toBe("block");

    const copyBtn = menu.querySelector("button")!;
    copyBtn.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(menu.style.display).toBe("block");

    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(menu.style.display).toBe("none");
    panel.dispose();
  });

  it("replays frames that outran create_terminal's id and acks with the real id", async () => {
    // Regression (Phase-1 gate): the actor's opening frame can arrive on the
    // channel before the create_terminal promise resolves. Acking with the
    // then-unset id (-1) is dropped Rust-side and deadlocks the ack-gated
    // stream — the panel must hold the frame and replay it once the id lands.
    const { api } = stubApi();
    (api.createTerminal as ReturnType<typeof vi.fn>).mockImplementation(async (opts) => {
      opts.onFrame(minimalFrame(10, 4)); // fires before the id is returned
      return 42;
    });
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux x86_64" });
    mockHostRect(container, 800, 400);
    await panel.start();
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 1);
    expect(api.requestFull).not.toHaveBeenCalledWith(-1);
    panel.dispose();
  });

  it("requests the default shell (empty command) elsewhere", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux x86_64" });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spec = (api.createTerminal as ReturnType<typeof vi.fn>).mock.calls[0][0].spec;
    expect(spec.command).toBe("");
    panel.dispose();
  });

  it("sizes the viewport to fill the host (renderer's term layer is out-of-flow)", () => {
    // Regression (Phase-1 gate): an unsized viewport collapses to 0px and
    // overflow:hidden clips all rendered rows — blank panel with a healthy
    // frame stream. jsdom does no layout, so assert the inline sizing.
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    const viewport = container.querySelector("div") as HTMLDivElement;
    expect(viewport.style.position).toBe("absolute");
    expect(viewport.style.inset).toBe("0px");
    panel.dispose();
  });

  it("sizes the initial spec from the container and cell metrics", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spec = (api.createTerminal as ReturnType<typeof vi.fn>).mock.calls[0][0].spec;
    // jsdom probe fallback: cellW 8, cellH 17 → 100 cols, 23 rows.
    expect(spec.cols).toBe(100);
    expect(spec.rows).toBe(23);
    panel.dispose();
  });

  it("acks each rendered frame inside rAF", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();
    expect(onFrame()).not.toBeNull();

    onFrame()!(minimalFrame(3, 2, 7));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 7);
    panel.dispose();
  });

  it("requests a FULL frame when a frame fails to decode", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(new ArrayBuffer(2)); // truncated → decodeFrame throws
    await FLUSH();
    expect(api.requestFull).toHaveBeenCalledWith(42);
    expect(api.ack).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("rebuilds the retained store after a decode failure and resyncs", async () => {
    // Regression guard for the retained-store path: a decode failure clears
    // the store; the resync FULL that follows must decode fresh (not throw
    // "out of grid" against the stale store) and be acked normally.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(minimalFrame(3, 2, 1)); // healthy full → store 3x2
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    onFrame()!(new ArrayBuffer(1)); // decode failure → store cleared
    await FLUSH();
    expect(api.requestFull).toHaveBeenCalledWith(42);

    onFrame()!(minimalFrame(4, 3, 3)); // resync FULL, larger grid
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 3);
    panel.dispose();
  });

  it("resyncs (request_full) on a seq gap without acking the gap frame", async () => {
    // A frame whose seq skips the expected next value means a frame
    // was lost — the retained store no longer matches the actor's grid. The
    // panel must NOT paint/ack an untrustworthy delta; it requests a FULL.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(minimalFrame(3, 2, 1)); // seq 1 lands and is acked
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 1);

    const gapFrame = deltaFrame(3, [{ row: 0, colStart: 0, cellCount: 2 }]);
    onFrame()!(gapFrame); // seq 3 skips 2
    await FLUSH();
    expect(api.requestFull).toHaveBeenCalledWith(42);
    expect(api.ack).not.toHaveBeenCalledWith(42, 3);

    // The resync FULL (seq 4, expected 3+1) is trusted and acked again.
    onFrame()!(minimalFrame(3, 2, 4));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 4);
    panel.dispose();
  });

  it("stops acking while hidden and requests a FULL on reveal", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    // Hide the document (as a backgrounded webview would be): acks stop.
    Object.defineProperty(document, "visibilityState", {
      value: "hidden",
      configurable: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    await FLUSH();

    onFrame()!(minimalFrame(3, 2, 7));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).not.toHaveBeenCalledWith(42, 7);

    // Reveal: the panel asks for a FULL (the actor has been coalescing
    // behind the closed gate) and acks resume.
    Object.defineProperty(document, "visibilityState", {
      value: "visible",
      configurable: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    await FLUSH();
    expect(api.requestFull).toHaveBeenCalledWith(42);

    onFrame()!(minimalFrame(3, 2, 8));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 8);
    panel.dispose();
  });

  it("exposes a live selection so Ctrl+C copies", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(minimalFrame(3, 2, 1, true));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();

    const textarea = container.querySelector(".chappa-textarea") as HTMLTextAreaElement;
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true });
    textarea.dispatchEvent(ev);
    await FLUSH();
    expect(api.copySelection).toHaveBeenCalledWith(42);
    panel.dispose();
  });

  it("sends the wheel delta to scroll on the terminal", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();
    onFrame()!(minimalFrame(3, 2, 1));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, deltaMode: 0, bubbles: true }));
    await FLUSH();
    // cellH 17 (jsdom probe fallback) → 100/17 ≈ 5.88 → truncated to 5
    // (fractional deltas accumulate; the remainder carries).
    expect(api.scroll).toHaveBeenCalledWith(42, 5);
    expect(api.mouse).toHaveBeenCalledWith(
      42,
      expect.objectContaining({ kind: "wheel_up" }),
    );
    panel.dispose();
  });

  it("clears the pane-side mouse-capture flag on the exit-time final frame", async () => {
    // PLAN bugs: frameMouseCapture is taken from every frame, so the
    // engine's exit-time final frame (capture cleared) is what un-sticks a
    // dead pane — a plain press must start a local selection again instead
    // of being forwarded to the (dead) TUI.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(minimalFrame(3, 2, 1, false, 0x20, true)); // TUI owns the mouse
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 4, clientY: 4, button: 0, bubbles: true }),
    );
    await FLUSH();
    expect(api.selection).not.toHaveBeenCalled();

    onFrame()!(minimalFrame(3, 2, 2, false, 0x20, false)); // final frame: capture cleared
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 4, clientY: 4, button: 0, bubbles: true }),
    );
    await FLUSH();
    expect(api.selection).toHaveBeenCalledWith(42, expect.objectContaining({ op: "start" }));
    panel.dispose();
  });

  it("resizes the terminal when the host grows", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    expect(api.resize).not.toHaveBeenCalled();

    // Fonts settle → re-measure → grid may change only if metrics changed.
    // Simulate a real layout change by widening the host and re-driving the
    // ResizeObserver callback path via a manual handleResize-style resize.
    mockHostRect(container, 1600, 400);
    (panel as unknown as { resizeNow(): void }).resizeNow();
    await FLUSH();
    expect(api.resize).toHaveBeenCalledWith(42, 200, 23);
    panel.dispose();
  });

  it("a FULL frame replaces the retained store, not just fills a null one", async () => {
    // Regression: a full arriving when a store already exists was decoded
    // fresh and then discarded — the panel kept painting the stale retained
    // store (while acking normally). A resync full exists precisely because
    // the retained store is suspect, so it must be adopted wholesale.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    onFrame()!(minimalFrame(3, 2, 1, false, 0x41)); // full of 'A'
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    expect(viewport.textContent).toContain("A");

    onFrame()!(minimalFrame(3, 2, 2, false, 0x42)); // same-dims full of 'B'
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(viewport.textContent).toContain("B");
    expect(viewport.textContent).not.toContain("A");
    panel.dispose();
  });

  it("treats 'not the active panel' as a hidden condition", async () => {
    // Extends the hidden machinery: a non-active panel stops acking and
    // requests a FULL on activation, exactly like a backgrounded webview.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", active: true });
    mockHostRect(container, 800, 400);
    await panel.start();
    expect(api.ack).toHaveBeenCalledTimes(0);

    onFrame()!(minimalFrame(3, 2, 1));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 1);

    // Deactivate: acking stops.
    panel.setActive(false);
    onFrame()!(minimalFrame(3, 2, 2));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).not.toHaveBeenCalledWith(42, 2);

    // Reactivate: FULL requested (the actor has been coalescing), acks resume.
    panel.setActive(true);
    await FLUSH();
    expect(api.requestFull).toHaveBeenCalledWith(42);

    onFrame()!(minimalFrame(3, 2, 3));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 3);
    panel.dispose();
  });

  it("resolves start() with the terminal id and fires onCreated", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    let created: number | null = null;
    const panel = new TerminalPanel({
      container,
      api,
      platform: "Linux",
      onCreated: (id) => {
        created = id;
      },
    });
    mockHostRect(container, 800, 400);
    const id = await panel.start();
    expect(id).toBe(42);
    expect(created).toBe(42);
    panel.dispose();
  });

  it("GL LRU release/restore are no-ops on the DOM fallback (jsdom has no WebGL2)", async () => {
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", renderer: "gl" });
    mockHostRect(container, 800, 400);
    await panel.start();

    // jsdom falls back to the DOM oracle: no live GL context to release, and
    // the panel keeps rendering + acking normally.
    expect(panel.glLive).toBe(false);
    panel.releaseGlContext();
    expect(panel.glLive).toBe(false);
    panel.restoreGlContext();

    onFrame()!(minimalFrame(3, 2, 7));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 7);
    panel.dispose();
  });

  it("falls back to the DOM renderer when ?renderer=gl cannot get WebGL2 (jsdom)", async () => {
    // jsdom has no WebGL2: a gl-pinned panel must degrade to the DOM oracle
    // and keep the full paint/ack pipeline working, not crash at construction.
    const { api, onFrame } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", renderer: "gl" });
    mockHostRect(container, 800, 400);
    await panel.start();
    expect(onFrame()).not.toBeNull();

    onFrame()!(minimalFrame(3, 2, 7));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    await FLUSH();
    expect(api.ack).toHaveBeenCalledWith(42, 7);
    expect(container.querySelector(".chappa-term-viewport")).not.toBeNull();
    panel.dispose();
  });
});

describe("rendererFromQuery", () => {
  it("defaults to the WebGL renderer; ?renderer=dom opts into the oracle", () => {
    // Default flipped with the crispness fixes — GL is the product
    // renderer per PLAN; jsdom still lands on DOM via the construction-time
    // WebGL2 fallback, which the test above pins.
    expect(rendererFromQuery()).toBe("gl");
    window.history.replaceState(null, "", "?renderer=dom");
    try {
      expect(rendererFromQuery()).toBe("dom");
    } finally {
      window.history.replaceState(null, "", window.location.pathname);
    }
  });
});

// --- copy-on-select + live font metrics ----------------------------

/** The panel's own mouse-event target (the InputController listens on it; the
 *  renderer's `.chappa-term-viewport` is a bubbling child of it). */
function viewportOf(container: HTMLElement): HTMLElement {
  return container.querySelector("div") as HTMLElement;
}

/** One left-button press → drag → release: a finished selection. */
function dragSelect(el: HTMLElement): void {
  el.dispatchEvent(new MouseEvent("mousedown", { clientX: 4, clientY: 4, button: 0, bubbles: true }));
  el.dispatchEvent(
    new MouseEvent("mousemove", { clientX: 60, clientY: 4, buttons: 1, button: 0, bubbles: true }),
  );
  el.dispatchEvent(new MouseEvent("mouseup", { clientX: 60, clientY: 4, button: 0, bubbles: true }));
}

function flashOf(container: HTMLElement): HTMLElement {
  return container.querySelector(".chappa-link-flash") as HTMLElement;
}

describe("TerminalPanel copy-on-select", () => {
  it("copies the finished selection with the skip-whitespace flag and flashes Copied", async () => {
    const { api } = stubApi();
    (api.copySelection as ReturnType<typeof vi.fn>).mockResolvedValue("ls -la");
    const settings = stubSettings({ copyOnSelect: true });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    dragSelect(viewportOf(container));
    await FLUSH();
    // skipWhitespaceOnly = true: an accidental micro-drag must never replace
    // the clipboard.
    expect(api.copySelection).toHaveBeenCalledWith(42, true);
    const flash = flashOf(container);
    expect(flash.style.display).toBe("block");
    expect(flash.textContent).toBe("Copied");
    // The house green, not the error red (jsdom serializes to rgb()).
    expect(flash.style.color).toBe("rgb(63, 185, 80)");
    panel.dispose();
  });

  it("shows NO flash when the copy came back null (whitespace-only selection)", async () => {
    const { api } = stubApi(); // the stub answers null by default
    const settings = stubSettings({ copyOnSelect: true });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    dragSelect(viewportOf(container));
    await FLUSH();
    expect(api.copySelection).toHaveBeenCalledWith(42, true);
    expect(flashOf(container).style.display).not.toBe("block");
    panel.dispose();
  });

  it("does not call copy_selection at all while the setting is OFF", async () => {
    const { api } = stubApi();
    const settings = stubSettings({ copyOnSelect: false });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    dragSelect(viewportOf(container));
    await FLUSH();
    expect(api.copySelection).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("picks up a live toggle without rebuilding the panel", async () => {
    const { api } = stubApi();
    (api.copySelection as ReturnType<typeof vi.fn>).mockResolvedValue("x");
    const settings = stubSettings({ copyOnSelect: false });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    dragSelect(viewportOf(container));
    await FLUSH();
    expect(api.copySelection).not.toHaveBeenCalled();
    await settings.store.update({ copyOnSelect: true });
    dragSelect(viewportOf(container));
    await FLUSH();
    expect(api.copySelection).toHaveBeenCalledWith(42, true);
    panel.dispose();
  });
});

describe("TerminalPanel clipboard keys", () => {
  /** Stub navigator.clipboard.readText so the Ctrl+V path is drivable in jsdom. */
  function stubReadText(text: string): void {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { readText: vi.fn(async () => text) },
    });
  }

  function textareaOf(container: HTMLElement): HTMLTextAreaElement {
    return container.querySelector(".chappa-textarea") as HTMLTextAreaElement;
  }

  it("Ctrl+V pastes the clipboard through the existing paste path (setting ON)", async () => {
    const { api } = stubApi();
    stubReadText("ls -la");
    const settings = stubSettings({ ctrlVPastes: true });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    const ev = new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true, cancelable: true });
    textareaOf(container).dispatchEvent(ev);
    await FLUSH();
    expect(ev.defaultPrevented).toBe(true);
    // The SAME paste route a native paste uses (Rust's bracketed-paste
    // guard applies) — and the raw 0x16 must NOT have reached the terminal.
    expect(api.paste).toHaveBeenCalledWith(42, "ls -la");
    expect(api.writeKey).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("Ctrl+V passes the raw 0x16 through while the setting is OFF", async () => {
    const { api } = stubApi();
    const settings = stubSettings({ ctrlVPastes: false });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    textareaOf(container).dispatchEvent(new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true }));
    await FLUSH();
    expect(api.paste).not.toHaveBeenCalled();
    expect(api.writeKey).toHaveBeenCalledWith(42, { key: { kind: "char", ch: "v" }, mods: MOD_CTRL });
    panel.dispose();
  });

  it("Ctrl+C with nothing selected is a no-op while ON, 0x03 passthrough while OFF", async () => {
    const { api } = stubApi();
    const settings = stubSettings({ ctrlCCopyOnly: true });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true, cancelable: true });
    textareaOf(container).dispatchEvent(ev);
    await FLUSH();
    expect(ev.defaultPrevented).toBe(true);
    // Nothing reaches the pty: no 0x03, no copy (there is no selection).
    expect(api.writeKey).not.toHaveBeenCalled();
    expect(api.copySelection).not.toHaveBeenCalled();
    panel.dispose();

    // OFF mode = today's rule: the reflex ^C interrupts.
    const { api: apiOff } = stubApi();
    const settingsOff = stubSettings({ ctrlCCopyOnly: false });
    await settingsOff.store.load();
    const containerOff = document.createElement("div");
    const panelOff = new TerminalPanel({ container: containerOff, api: apiOff, platform: "Linux", settings: settingsOff.store });
    mockHostRect(containerOff, 800, 400);
    await panelOff.start();
    textareaOf(containerOff).dispatchEvent(new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true }));
    await FLUSH();
    expect(apiOff.copySelection).not.toHaveBeenCalled();
    expect(apiOff.writeKey).toHaveBeenCalledWith(42, { key: { kind: "char", ch: "c" }, mods: MOD_CTRL });
    panelOff.dispose();
  });

  it("picks up a live toggle of both keys without rebuilding the panel", async () => {
    // The copy-on-select live-toggle pattern, applied to the keys:
    // start with BOTH off (today's behavior) and flip them on from the store.
    const { api } = stubApi();
    stubReadText("echo hi");
    const settings = stubSettings({ ctrlVPastes: false, ctrlCCopyOnly: false });
    await settings.store.load();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", settings: settings.store });
    mockHostRect(container, 800, 400);
    await panel.start();

    // OFF: legacy behavior — 0x16 and 0x03 both reach the terminal.
    textareaOf(container).dispatchEvent(new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true }));
    await FLUSH();
    expect(api.paste).not.toHaveBeenCalled();
    expect(api.writeKey).toHaveBeenCalledWith(42, { key: { kind: "char", ch: "v" }, mods: MOD_CTRL });

    // The pane toggles BOTH on; the SAME open panel must change behavior.
    await settings.store.update({ ctrlVPastes: true, ctrlCCopyOnly: true });
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true, cancelable: true });
    textareaOf(container).dispatchEvent(ev);
    await FLUSH();
    expect(ev.defaultPrevented).toBe(true);
    expect(api.copySelection).not.toHaveBeenCalled(); // no selection → no-op, not 0x03
    const callsBefore = (api.writeKey as ReturnType<typeof vi.fn>).mock.calls.length;
    textareaOf(container).dispatchEvent(new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true, cancelable: true }));
    await FLUSH();
    expect(api.paste).toHaveBeenCalledWith(42, "echo hi");
    expect((api.writeKey as ReturnType<typeof vi.fn>).mock.calls.length).toBe(callsBefore); // 0x16 no longer passes
    panel.dispose();
  });
});

describe("TerminalPanel font metrics", () => {
  /** Spy on the live renderer's setMetrics (jsdom lands on the DOM oracle). */
  function metricsSpy(panel: TerminalPanel) {
    const renderer = (panel as unknown as { renderer: { setMetrics: (m: unknown) => void } })
      .renderer;
    return vi.spyOn(renderer, "setMetrics");
  }

  it("a font-size change re-measures, re-metrics the renderer and re-resizes the grid", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spy = metricsSpy(panel);
    (api.resize as ReturnType<typeof vi.fn>).mockClear();

    await panel.setFontMetrics("Geist Mono", 18, 1.2);
    await FLUSH();
    // jsdom cannot lay out: the advance falls back to round(fontSize × 0.6)
    // CSS px — 8 at 14 px, 11 at 18 px.
    expect(spy).toHaveBeenCalledTimes(1);
    const m = spy.mock.calls[0][0] as { cellW: number; cellH: number; fontSize: number };
    expect(m.fontSize).toBe(18);
    expect(m.cellW).toBe(11);
    expect(m.cellH).toBeCloseTo(21.6, 6); // CSS px: 18 × 1.2
    // 800 CSS px / 11 → 72 cols; 400 / 21.6 → 18 rows.
    expect(api.resize).toHaveBeenCalledWith(42, 72, 18);
    panel.dispose();
  });

  it("a FAMILY swap with an identical advance width still re-metrics (widened guard)", async () => {
    // Boundary regime, not the happy path: two faces can share an advance to
    // the pixel (in jsdom EVERY face does). The old early return keyed only on
    // cellW/cellH/dpr, so the swap was silently skipped and the renderer kept
    // rasterizing the previous face.
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spy = metricsSpy(panel);

    await panel.setFontMetrics("JetBrains Mono", 14, 1.2);
    await FLUSH();
    expect(spy).toHaveBeenCalledTimes(1);
    const m = spy.mock.calls[0][0] as { cellW: number; cellH: number; fontFamily: string };
    expect(m.cellW).toBe(8); // unchanged advance — the whole point
    expect(m.cellH).toBeCloseTo(16.8, 6);
    // The chosen face heads the stack, the other bundled face backs it up, and
    // the stack is NOT re-quoted.
    expect(m.fontFamily).toBe('"JetBrains Mono", "Geist Mono", monospace');
    panel.dispose();
  });

  it("re-applying the SAME metrics re-measures nothing", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spy = metricsSpy(panel);
    await panel.setFontMetrics("Geist Mono", 14, 1.2);
    await FLUSH();
    expect(spy).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("line height 1.2 → 1.5 makes cellH exactly fontSize × 1.5 CSS px", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux", fontSize: 14 });
    mockHostRect(container, 800, 400);
    await panel.start();
    const spy = metricsSpy(panel);

    await panel.setFontMetrics("Geist Mono", 14, 1.5);
    await FLUSH();
    const m = spy.mock.calls[0][0] as { cellW: number; cellH: number };
    expect(m.cellH, "cellH in CSS px must be fontSize(14) × lineHeight(1.5)").toBeCloseTo(21, 6);
    expect(m.cellW).toBe(8); // line height is vertical only
    panel.dispose();
  });
});

describe("hint bar subprocess count", () => {
  it("shows the count only above zero and never takes layout space", async () => {
    const { api } = stubApi();
    const container = document.createElement("div");
    const panel = new TerminalPanel({ container, api, platform: "Linux" });
    mockHostRect(container, 800, 400);
    await panel.start();

    const bar = container.querySelector<HTMLElement>(".chappa-hint-bar")!;
    // An overlay, not a layout row: the renderer sizes its grid off the host
    // box, so a bar in flow would reflow every terminal in the app.
    expect(bar.style.position).toBe("absolute");
    expect(bar.style.display).toBe("none");

    panel.setSubprocessCount(2);
    expect(bar.textContent).toBe("2 subprocesses");
    expect(bar.style.display).toBe("block");

    // Pluralised — "1 subprocesses" is the kind of wrong that gets noticed.
    panel.setSubprocessCount(1);
    expect(bar.textContent).toBe("1 subprocess");

    // Zero says nothing at all, rather than "0 subprocesses" on every shell.
    panel.setSubprocessCount(0);
    expect(bar.textContent).toBe("");
    expect(bar.style.display).toBe("none");
    panel.dispose();
  });
});

describe("probeCellMetrics line height", () => {
  it("cellH is fontSize × lineHeight in CSS px; the default stays 1.2", () => {
    expect(probeCellMetrics("Geist Mono", 14).cellH).toBeCloseTo(16.8, 6);
    expect(probeCellMetrics("Geist Mono", 14, 1, 1.0).cellH).toBeCloseTo(14, 6);
    expect(probeCellMetrics("Geist Mono", 14, 1, 1.8).cellH).toBeCloseTo(25.2, 6);
    // Width does not move with line height (units: CSS px advance of "M").
    expect(probeCellMetrics("Geist Mono", 14, 1, 1.8).cellW).toBe(
      probeCellMetrics("Geist Mono", 14, 1, 1.0).cellW,
    );
  });
});
