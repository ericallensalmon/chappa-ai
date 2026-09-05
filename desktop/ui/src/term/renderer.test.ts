// @vitest-environment jsdom
// The shared TermRenderer interface : both renderers must implement
// apply(frame, cells) / setMetrics / focus / dispose, and the DOM oracle's
// focus() must hollow the block cursor when the panel blurs.
import { describe, expect, it } from "vitest";
import { type Frame, type FrameCursor, type RowSpan } from "./protocol";
import { DomRenderer } from "./renderer_dom";
import type { RendererMetrics } from "./renderer";

const DEFAULT_FG = 0xd8d8d8ff;
const DEFAULT_BG = 0x181818ff;

function makeFrame(cursor?: Partial<FrameCursor>, rows?: RowSpan[]): Frame {
  const cols = 3;
  const rowsCount = 2;
  const size = cols * rowsCount;
  const cells = {
    cols,
    rows: rowsCount,
    ch: new Uint32Array(size),
    fg: new Uint32Array(size).fill(DEFAULT_FG),
    bg: new Uint32Array(size).fill(DEFAULT_BG),
    flags: new Uint16Array(size),
    link: new Uint16Array(size),
  };
  cells.ch[0] = 0x41;
  return {
    seq: 1,
    kind: "full",
    cursor: { row: 0, col: 0, shape: "block", visible: false, ...cursor },
    displayOffset: 0,
    historyLen: 0,
    selection: null,
    selectionActive: false,
    mouseCapture: false,
    altScreen: false,
    matches: [],
    rows: rows ?? [{ row: 0, colStart: 0, cellCount: cols }],
    zerowidth: [],
    cells,
  };
}

const METRICS: RendererMetrics = {
  cellW: 10,
  cellH: 16,
  dpr: 1,
  baseline: 12,
  fontFamily: "JetBrains Mono",
  fontSize: 14,
};

describe("TermRenderer conformance", () => {
  function makeDom(): { container: HTMLElement; r: DomRenderer } {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const r = new DomRenderer({
      container,
      cellW: 10,
      cellH: 16,
      fontFamily: "JetBrains Mono",
      fontSize: 14,
    });
    return { container, r };
  }

  it("DomRenderer implements the shared interface surface", () => {
    const { r } = makeDom();
    expect(typeof r.apply).toBe("function");
    expect(typeof r.setMetrics).toBe("function");
    expect(typeof r.focus).toBe("function");
    expect(typeof r.dispose).toBe("function");
  });

  it("apply(frame, cells) is equivalent to the DOM renderer's render", () => {
    const { container, r } = makeDom();
    r.apply(makeFrame());
    expect(container.querySelector('.term-row[data-row="0"]')).not.toBeNull();
  });

  it("setMetrics drops the row cache and DOM (font/metrics change)", () => {
    const { container, r } = makeDom();
    r.apply(makeFrame());
    expect(container.querySelectorAll(".term-row").length).toBe(1);
    r.setMetrics({ ...METRICS, cellW: 11, cellH: 18 });
    expect(container.querySelectorAll(".term-row").length).toBe(0);
  });

  it("focus(false) hollows the block cursor and focus(true) restores it", () => {
    const { container, r } = makeDom();
    r.apply(makeFrame({ row: 0, col: 0, visible: true, shape: "block" }));
    const cursor = container.querySelector(".cursor") as HTMLElement;
    expect(cursor.classList.contains("hollow")).toBe(false);

    r.focus(false);
    expect(cursor.classList.contains("hollow")).toBe(true);

    r.focus(true);
    expect(cursor.classList.contains("hollow")).toBe(false);
  });

  it("leaves a block cursor filled when focused (default)", () => {
    const { container, r } = makeDom();
    r.apply(makeFrame({ row: 0, col: 0, visible: true, shape: "block" }));
    const cursor = container.querySelector(".cursor") as HTMLElement;
    expect(cursor.classList.contains("hollow")).toBe(false);
  });
});
