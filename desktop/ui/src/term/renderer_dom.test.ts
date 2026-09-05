// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { CELL_FLAGS, type Frame, type FrameCursor, type Range } from "./protocol";
import { DomRenderer, type DomRendererOptions } from "./renderer_dom";

const DEFAULT_FG = 0xd8d8d8ff;
const DEFAULT_BG = 0x181818ff;
const RED = 0xac4242ff;
const BLUE = 0x6a9fb5ff;

interface CellSpec {
  ch: string | number;
  fg?: number;
  bg?: number;
  flags?: number;
  link?: number;
}
interface RowSpec {
  row: number;
  colStart?: number;
  cells: CellSpec[];
}

function chCode(c: string | number): number {
  return typeof c === "number" ? c : (c.codePointAt(0) as number);
}

/** Build a Frame directly (the renderer consumes the decoded shape). For
 *  delta frames pass the same cols/rows as the full frame it follows so the
 *  renderer's grid doesn't resize. */
function makeFrame(opts: {
  rows: RowSpec[];
  cols?: number;
  rowsCount?: number;
  kind?: "full" | "delta";
  seq?: number;
  cursor?: Partial<FrameCursor> & { row: number; col: number };
  displayOffset?: number;
  historyLen?: number;
  selection?: Range | null;
  matches?: Range[];
  zerowidth?: { row: number; col: number; chars: number[] }[];
}): Frame {
  let cols = opts.cols ?? 0;
  let rows = opts.rowsCount ?? 0;
  for (const r of opts.rows) {
    cols = Math.max(cols, (r.colStart ?? 0) + r.cells.length);
    rows = Math.max(rows, r.row + 1);
  }
  cols = Math.max(cols, 1);
  rows = Math.max(rows, 1);

  const size = cols * rows;
  const store = {
    cols,
    rows,
    ch: new Uint32Array(size),
    fg: new Uint32Array(size).fill(DEFAULT_FG),
    bg: new Uint32Array(size).fill(DEFAULT_BG),
    flags: new Uint16Array(size),
    link: new Uint16Array(size),
  };

  const spans = [];
  for (const r of opts.rows) {
    const colStart = r.colStart ?? 0;
    r.cells.forEach((c, j) => {
      const idx = r.row * cols + colStart + j;
      store.ch[idx] = chCode(c.ch);
      store.fg[idx] = c.fg ?? DEFAULT_FG;
      store.bg[idx] = c.bg ?? DEFAULT_BG;
      store.flags[idx] = c.flags ?? 0;
      store.link[idx] = c.link ?? 0;
    });
    spans.push({ row: r.row, colStart, cellCount: r.cells.length });
  }

  return {
    seq: opts.seq ?? 1,
    kind: opts.kind ?? "full",
    cursor: {
      row: opts.cursor?.row ?? 0,
      col: opts.cursor?.col ?? 0,
      shape: opts.cursor?.shape ?? "block",
      visible: opts.cursor?.visible ?? false,
    },
    displayOffset: opts.displayOffset ?? 0,
    historyLen: opts.historyLen ?? 0,
    selection: opts.selection ?? null,
    selectionActive: (opts.selection ?? null) !== null,
    mouseCapture: false,
    altScreen: false,
    matches: opts.matches ?? [],
    rows: spans,
    zerowidth: (opts.zerowidth ?? []).map((z) => ({
      row: z.row,
      col: z.col,
      chars: Uint32Array.from(z.chars),
    })),
    cells: store,
  };
}

function makeRenderer(opts: Partial<DomRendererOptions> = {}): { container: HTMLDivElement; r: DomRenderer } {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const r = new DomRenderer({
    container,
    cellW: 10,
    cellH: 16,
    fontFamily: "JetBrains Mono",
    fontSize: 14,
    ...opts,
  });
  return { container, r };
}

function rowText(el: HTMLElement): string {
  return (el.textContent ?? "").replace(/\u0000/g, " ");
}

describe("span coalescing", () => {
  it("renders a uniform row as a single span", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        rows: [{ row: 0, cells: [{ ch: "a" }, { ch: "b" }, { ch: "c" }, { ch: "d" }] }],
      }),
    );
    const row = container.querySelector('.term-row[data-row="0"]') as HTMLElement;
    expect(row).not.toBeNull();
    const spans = row.querySelectorAll("span");
    expect(spans.length).toBe(1);
    expect(rowText(row)).toBe("abcd");
  });

  it("splits spans where the style changes", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        rows: [
          {
            row: 0,
            cells: [
              { ch: "a" },
              { ch: "b", fg: RED },
              { ch: "c", fg: RED },
              { ch: "d" },
            ],
          },
        ],
      }),
    );
    const row = container.querySelector('.term-row[data-row="0"]') as HTMLElement;
    const spans = row.querySelectorAll("span");
    expect(spans.length).toBe(3);
    expect(rowText(spans[0] as HTMLElement)).toBe("a");
    expect(rowText(spans[1] as HTMLElement)).toBe("bc");
    expect(rowText(spans[2] as HTMLElement)).toBe("d");
    expect((spans[1] as HTMLElement).style.color).toContain("172, 66, 66");
  });

  it("keeps rows keyed by absolute row index (sparse rows stay uncreated)", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        rows: [{ row: 0, cells: [{ ch: "a" }] }, { row: 2, cells: [{ ch: "c" }] }],
      }),
    );
    expect(r.rowElement(0)).not.toBeNull();
    expect(r.rowElement(1)).toBeNull();
    expect(r.rowElement(2)).not.toBeNull();
    expect(container.querySelectorAll(".term-row").length).toBe(2);
  });
});

describe("row layout and cache lifecycle", () => {
  it("positions rows absolutely by index, independent of creation order", () => {
    // Regression (Phase-1 gate): delta frames create row divs lazily and out
    // of order; document order must never be the layout.
    const { container, r } = makeRenderer();
    r.render(makeFrame({ rowsCount: 4, rows: [{ row: 3, cells: [{ ch: "x" }] }] }));
    r.render(
      makeFrame({ rowsCount: 4, rows: [{ row: 1, cells: [{ ch: "y" }] }], seq: 2, kind: "delta" }),
    );
    const row3 = container.querySelector('.term-row[data-row="3"]') as HTMLElement;
    const row1 = container.querySelector('.term-row[data-row="1"]') as HTMLElement;
    expect(row3.style.top).toBe(`${3 * 16}px`);
    expect(row1.style.top).toBe(`${1 * 16}px`);
  });

  it("drops stale row DOM when cell metrics change", () => {
    // Regression (Phase-1 gate): setCellMetrics cleared the cache but left
    // the old divs in the DOM — everything rebuilt after a font load or
    // resize rendered on top of/below orphaned rows.
    const { container, r } = makeRenderer();
    r.render(makeFrame({ rows: [{ row: 0, cells: [{ ch: "a" }] }] }));
    expect(container.querySelectorAll(".term-row").length).toBe(1);
    r.setCellMetrics(11, 18);
    expect(container.querySelectorAll(".term-row").length).toBe(0);
    r.render(makeFrame({ rows: [{ row: 0, cells: [{ ch: "b" }] }], seq: 2 }));
    expect(container.querySelectorAll(".term-row").length).toBe(1);
    expect(rowText(container.querySelector('.term-row[data-row="0"]') as HTMLElement)).toBe("b");
  });

  it("drops stale row DOM when the grid dims change", () => {
    const { container, r } = makeRenderer();
    r.render(makeFrame({ cols: 4, rowsCount: 2, rows: [{ row: 1, cells: [{ ch: "a" }] }] }));
    r.render(makeFrame({ cols: 6, rowsCount: 3, rows: [{ row: 0, cells: [{ ch: "b" }] }], seq: 2 }));
    const rows = container.querySelectorAll(".term-row");
    expect(rows.length).toBe(1);
    expect((rows[0] as HTMLElement).dataset.row).toBe("0");
  });
});

describe("dirty-row-only re-render", () => {
  it("mutates only the rows listed in the frame's dirty spans", async () => {
    const { r } = makeRenderer();
    r.render(
      makeFrame({
        rows: [
          { row: 0, cells: [{ ch: "a" }, { ch: "b" }] },
          { row: 1, cells: [{ ch: "c" }, { ch: "d" }] },
        ],
      }),
    );
    const row0 = r.rowElement(0) as HTMLElement;
    const row1 = r.rowElement(1) as HTMLElement;

    let m0 = 0;
    let m1 = 0;
    const o0 = new MutationObserver((recs) => (m0 += recs.length));
    const o1 = new MutationObserver((recs) => (m1 += recs.length));
    o0.observe(row0, { childList: true, characterData: true, subtree: true });
    o1.observe(row1, { childList: true, characterData: true, subtree: true });

    r.render(
      makeFrame({
        kind: "delta",
        cols: 2,
        rowsCount: 2,
        seq: 2,
        rows: [{ row: 1, cells: [{ ch: "X" }, { ch: "Y" }] }],
      }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    o0.disconnect();
    o1.disconnect();

    expect(m0).toBe(0);
    expect(m1).toBeGreaterThan(0);
    expect(rowText(row0)).toBe("ab");
    expect(rowText(row1)).toBe("XY");
  });

  it("repaints only the dirty span columns of a partial delta", () => {
    const { r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 4,
        rowsCount: 1,
        rows: [{ row: 0, cells: [{ ch: "a" }, { ch: "b" }, { ch: "c" }, { ch: "d" }] }],
      }),
    );
    // Delta covering only columns 1..2 of row 0; cols 0 and 3 keep their text.
    r.render(
      makeFrame({
        kind: "delta",
        cols: 4,
        rowsCount: 1,
        seq: 2,
        rows: [{ row: 0, colStart: 1, cells: [{ ch: "X" }, { ch: "Y" }] }],
      }),
    );
    expect(rowText(r.rowElement(0) as HTMLElement)).toBe("aXYd");
  });
});

describe("style mapping table", () => {
  function firstSpan(flags: number, fg = DEFAULT_FG, bg = DEFAULT_BG): CSSStyleDeclaration {
    const { container, r } = makeRenderer();
    r.render(makeFrame({ rows: [{ row: 0, cells: [{ ch: "a", flags, fg, bg }] }] }));
    const row = container.querySelector('.term-row[data-row="0"]') as HTMLElement;
    return (row.querySelector("span") as HTMLSpanElement).style;
  }

  it("maps fg/bg RGBA to color/background-color", () => {
    // jsdom serializes alpha-1 colors as rgb(); assert the channels.
    const s = firstSpan(0, RED, BLUE);
    expect(s.color).toContain("172, 66, 66");
    expect(s.backgroundColor).toContain("106, 159, 181");
  });

  it("skips the background when it equals the terminal default", () => {
    const s = firstSpan(0, RED, DEFAULT_BG);
    expect(s.backgroundColor).toBe("");
  });

  it("maps bold/italic/dim", () => {
    expect(firstSpan(CELL_FLAGS.bold).fontWeight).toBe("700");
    expect(firstSpan(CELL_FLAGS.italic).fontStyle).toBe("italic");
    expect(firstSpan(CELL_FLAGS.dim).opacity).toBe("0.6");
  });

  it("maps underline kinds to text-decoration-style", () => {
    expect(firstSpan(CELL_FLAGS.underline).textDecorationLine).toBe("underline");
    expect(firstSpan(CELL_FLAGS.underline).textDecorationStyle).toBe("solid");
    expect(firstSpan(CELL_FLAGS.doubleUnderline).textDecorationStyle).toBe("double");
    expect(firstSpan(CELL_FLAGS.undercurl).textDecorationStyle).toBe("wavy");
    expect(firstSpan(CELL_FLAGS.dottedUnderline).textDecorationStyle).toBe("dotted");
    expect(firstSpan(CELL_FLAGS.dashedUnderline).textDecorationStyle).toBe("dashed");
  });

  it("combines strikeout with underline", () => {
    const s = firstSpan(CELL_FLAGS.underline | CELL_FLAGS.strikeout);
    expect(s.textDecorationLine).toBe("underline line-through");
    expect(firstSpan(CELL_FLAGS.strikeout).textDecorationLine).toBe("line-through");
  });

  it("hides hidden cells", () => {
    expect(firstSpan(CELL_FLAGS.hidden).visibility).toBe("hidden");
  });

  it("does not double-apply inverse (colors arrive resolved)", () => {
    const s = firstSpan(CELL_FLAGS.inverse, BLUE, RED);
    expect(s.color).toContain("106, 159, 181");
    expect(s.backgroundColor).toContain("172, 66, 66");
  });
});

describe("wide + zero-width cells", () => {
  it("renders a wide glyph and skips its spacer cell", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 4,
        rowsCount: 1,
        rows: [
          {
            row: 0,
            cells: [
              { ch: "你", flags: CELL_FLAGS.wide },
              { ch: "", flags: CELL_FLAGS.wideSpacer },
              { ch: "A" },
              { ch: "B" },
            ],
          },
        ],
      }),
    );
    const row = container.querySelector('.term-row[data-row="0"]') as HTMLElement;
    expect(rowText(row)).toBe("你AB");
  });

  it("appends zero-width combiners to their base cell's text", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 3,
        rowsCount: 1,
        rows: [{ row: 0, cells: [{ ch: "e" }, { ch: "x" }, { ch: "t" }] }],
        zerowidth: [{ row: 0, col: 0, chars: [0x301] }], // combining acute
      }),
    );
    const row = container.querySelector('.term-row[data-row="0"]') as HTMLElement;
    expect(rowText(row)).toBe("e\u0301xt");
  });

  it("leaves a spacer in the accumulated store without emitting text", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 3,
        rowsCount: 1,
        rows: [
          {
            row: 0,
            cells: [
              { ch: "宽", flags: CELL_FLAGS.wide },
              { ch: "", flags: CELL_FLAGS.wideSpacer },
              { ch: "!" },
            ],
          },
        ],
      }),
    );
    expect(container.textContent).toContain("宽!");
  });
});

describe("cursor overlay", () => {
  function cursorEl(opts: Parameters<typeof makeFrame>[0]): HTMLElement {
    const { container, r } = makeRenderer();
    r.render(makeFrame(opts));
    return container.querySelector(".cursor") as HTMLElement;
  }

  it("hides when invisible or shape hidden", () => {
    expect(cursorEl({ rows: [], cursor: { row: 1, col: 2, visible: false } }).style.display).toBe("none");
    expect(
      cursorEl({ rows: [], cursor: { row: 1, col: 2, shape: "hidden", visible: true } }).style.display,
    ).toBe("none");
  });

  it("sizes block/beam/underline shapes at the cell", () => {
    const block = cursorEl({ rows: [], cursor: { row: 1, col: 2, shape: "block", visible: true } });
    expect(block.style.transform).toBe("translate(20px, 16px)");
    expect(block.style.width).toBe("10px");
    expect(block.style.height).toBe("16px");

    const beam = cursorEl({ rows: [], cursor: { row: 0, col: 0, shape: "beam", visible: true } });
    expect(beam.style.width).toBe("2px");
    expect(beam.style.height).toBe("16px");

    const underline = cursorEl({ rows: [], cursor: { row: 0, col: 0, shape: "underline", visible: true } });
    expect(underline.style.height).toBe("2px");
    expect(underline.style.transform).toBe("translate(0px, 14px)");
  });

  it("draws hollow as a border box", () => {
    const hollow = cursorEl({ rows: [], cursor: { row: 0, col: 0, shape: "hollow", visible: true } });
    expect(hollow.classList.contains("hollow")).toBe(true);
    expect(hollow.style.border).toContain("1px");
  });
});

describe("selection and search overlays", () => {
  it("draws one rect per selected row", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 4,
        rowsCount: 3,
        rows: [],
        selection: { startRow: 1, startCol: 1, endRow: 2, endCol: 2 },
      }),
    );
    const sels = container.querySelectorAll(".term-sel");
    expect(sels.length).toBe(2);
    const [first, second] = sels as unknown as HTMLElement[];
    expect(first.style.left).toBe("10px");
    expect(first.style.top).toBe("16px");
    expect(first.style.width).toBe("30px"); // cols 1..3
    expect(second.style.left).toBe("0px");
    expect(second.style.top).toBe("32px");
    expect(second.style.width).toBe("30px"); // cols 0..2 (end col inclusive → exclusive 3)
  });

  it("draws a single-row selection as one rect", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 4,
        rowsCount: 1,
        rows: [],
        selection: { startRow: 0, startCol: 1, endRow: 0, endCol: 2 },
      }),
    );
    const sels = container.querySelectorAll(".term-sel");
    expect(sels.length).toBe(1);
    expect((sels[0] as HTMLElement).style.width).toBe("20px"); // cols 1..2
  });

  it("draws search match rects", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 3,
        rowsCount: 1,
        rows: [],
        matches: [
          { startRow: 0, startCol: 0, endRow: 0, endCol: 2 },
          { startRow: 0, startCol: 2, endRow: 0, endCol: 2 },
        ],
      }),
    );
    const matches = container.querySelectorAll(".term-match");
    expect(matches.length).toBe(2);
  });

  it("clears overlays when selection disappears", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        cols: 3,
        rowsCount: 1,
        rows: [],
        selection: { startRow: 0, startCol: 0, endRow: 0, endCol: 1 },
      }),
    );
    expect(container.querySelectorAll(".term-sel").length).toBe(1);
    r.render(makeFrame({ cols: 3, rowsCount: 1, rows: [], selection: null }));
    expect(container.querySelectorAll(".term-sel").length).toBe(0);
  });
});

describe("scrolled pill + scrollbar", () => {
  it("shows the scrolled pill only when displayOffset > 0", () => {
    const { container, r } = makeRenderer();
    const pill = container.querySelector(".chappa-term-pill") as HTMLElement;
    r.render(makeFrame({ rows: [], displayOffset: 0 }));
    expect(pill.style.display).toBe("none");
    r.render(makeFrame({ rows: [], displayOffset: 5 }));
    expect(pill.style.display).toBe("block");
  });

  it("sizes the scrollbar thumb from history and offset", () => {
    const { container, r } = makeRenderer();
    r.render(
      makeFrame({
        rows: [],
        cols: 3,
        rowsCount: 3,
        displayOffset: 25,
        historyLen: 100,
      }),
    );
    const thumb = container.querySelector(".chappa-term-scrollbar .thumb") as HTMLElement;
    expect(thumb.style.display).toBe("block");
    const trackH = 3 * 16; // rows × cellH
    const thumbH = Math.max(24, (3 / 103) * trackH);
    const travel = Math.max(1, trackH - thumbH);
    expect(thumb.style.height).toBe(`${thumbH}px`);
    // Offset counts up from the bottom (0 = live); thumb top counts down
    // from the top — 25 lines back from live sits 75% down the history.
    expect(thumb.style.top).toBe(`${Math.max(0, Math.min(trackH - thumbH, (75 / 100) * travel))}px`);
  });

  it("puts the thumb at the bottom when following live (offset 0)", () => {
    // Regression (Phase-1 gate): the thumb sat at the TOP while following —
    // both scrollbar mappings were inverted, self-consistently.
    const { container, r } = makeRenderer();
    r.render(makeFrame({ rows: [], cols: 3, rowsCount: 3, displayOffset: 0, historyLen: 100 }));
    const thumb = container.querySelector(".chappa-term-scrollbar .thumb") as HTMLElement;
    const trackH = 3 * 16;
    const thumbH = Math.max(24, (3 / 103) * trackH);
    expect(thumb.style.top).toBe(`${trackH - thumbH}px`);
  });

  it("hides the scrollbar thumb with no history", () => {
    const { container, r } = makeRenderer();
    r.render(makeFrame({ rows: [], historyLen: 0 }));
    const thumb = container.querySelector(".chappa-term-scrollbar .thumb") as HTMLElement;
    expect(thumb.style.display).toBe("none");
  });
});
