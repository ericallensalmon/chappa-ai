// @vitest-environment jsdom
// Pure instance-list builder tests (container-runnable — no GPU needed) plus
// shader snapshot tests and a headless mock-GL smoke test of the apply→draw
// path. Real WebGL2 rendering is host-verified: jsdom has no context, and the
// renderer throws cleanly when one is absent (the panel falls back to DOM).
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CELL_FLAGS, type CellStore, type Frame, type FrameCursor, type Range, type RowSpan } from "./protocol";
import type { AtlasSlot } from "./atlas";
import {
  buildBgFrameRects,
  buildCursorRects,
  blockCursorGlyph,
  COLOR_FRAG_SRC,
  COLOR_VERT_SRC,
  countFrameRects,
  CURSOR_COLOR,
  GLYPH_COLORED,
  GLYPH_FLOATS,
  GLYPH_FRAG_SRC,
  GLYPH_VERT_SRC,
  RECT_FLOATS,
  RECT_KIND_DASHED,
  RECT_KIND_DOTTED,
  RECT_KIND_DOUBLE,
  RECT_KIND_HOLLOW,
  RECT_KIND_PLAIN,
  RECT_KIND_STRIKE_UNDER,
  RECT_KIND_UNDERCURL,
  WebGLRenderer,
  writeBgCells,
  writeDecorCells,
  writeGlyphCells,
} from "./renderer_webgl";

const DEFAULT_FG = 0xd8d8d8ff;
const DEFAULT_BG = 0x181818ff;
const RED = 0xac4242ff;
const BLUE = 0x6a9fb5ff;

function store(cols: number, rows: number): CellStore {
  return {
    cols,
    rows,
    ch: new Uint32Array(cols * rows),
    fg: new Uint32Array(cols * rows).fill(DEFAULT_FG),
    bg: new Uint32Array(cols * rows).fill(DEFAULT_BG),
    flags: new Uint16Array(cols * rows),
    link: new Uint16Array(cols * rows),
  };
}

function put(cells: CellStore, row: number, col: number, ch: number, fg = DEFAULT_FG, bg = DEFAULT_BG, flags = 0): void {
  const idx = row * cells.cols + col;
  cells.ch[idx] = ch;
  cells.fg[idx] = fg;
  cells.bg[idx] = bg;
  cells.flags[idx] = flags;
}

function span(row: number, colStart: number, cellCount: number): RowSpan {
  return { row, colStart, cellCount };
}

function makeFrame(opts: {
  store: CellStore;
  rows?: RowSpan[];
  kind?: "full" | "delta";
  cursor?: Partial<FrameCursor> & { row: number; col: number };
  selection?: Range | null;
  matches?: Range[];
}): Frame {
  return {
    seq: 1,
    kind: opts.kind ?? "full",
    cursor: { row: 0, col: 0, shape: "block", visible: false, ...opts.cursor },
    displayOffset: 0,
    historyLen: 0,
    selection: opts.selection ?? null,
    selectionActive: (opts.selection ?? null) !== null,
    mouseCapture: false,
    altScreen: false,
    matches: opts.matches ?? [],
    rows: opts.rows ?? [{ row: 0, colStart: 0, cellCount: opts.store.cols }],
    zerowidth: [],
    cells: opts.store,
  };
}

/** An atlas whose lookup always succeeds with a fixed slot (for pure builder
 *  tests that don't want the miss path). */
function hitAtlas(): { lookup: (k: number) => AtlasSlot } {
  const slot: AtlasSlot = { page: 1, x: 100, y: 200, w: 9, h: 17 };
  return { lookup: () => slot };
}

function missAtlas(): { lookup: (k: number) => AtlasSlot | null } {
  return { lookup: () => null };
}

const M1 = { cellW: 10, cellH: 16, dpr: 1 };

describe("writeBgCells", () => {
  it("skips default-bg cells with a degenerate quad and draws the rest per-cell", () => {
    const cells = store(3, 1);
    put(cells, 0, 0, 0x20, DEFAULT_FG, RED);
    put(cells, 0, 2, 0x20, DEFAULT_FG, BLUE);
    const buf = new Float32Array(3 * RECT_FLOATS);
    writeBgCells(buf, cells, [span(0, 0, 3)], DEFAULT_BG, M1);

    const slot0 = buf.subarray(0, RECT_FLOATS);
    const slot1 = buf.subarray(RECT_FLOATS, 2 * RECT_FLOATS);
    const slot2 = buf.subarray(2 * RECT_FLOATS, 3 * RECT_FLOATS);
    expect(slot0[0]).toBe(0); // x
    expect(slot0[2]).toBe(10); // w = 1 cell
    expect(slot0[4]).toBeCloseTo(0xac / 255, 6); // RED r
    expect(slot1[2]).toBe(0); // default bg → zero area
    expect(slot2[2]).toBe(10);
    expect(slot2[0]).toBe(20); // x = col 2
    expect(slot2[4]).toBeCloseTo(0x6a / 255, 6);
  });

  it("scales geometry by dpr and positions by row/col", () => {
    const cells = store(2, 2);
    put(cells, 1, 1, 0x20, DEFAULT_FG, RED);
    const buf = new Float32Array(4 * RECT_FLOATS);
    writeBgCells(buf, cells, [span(1, 1, 1)], DEFAULT_BG, { cellW: 8, cellH: 14, dpr: 2 });
    const off = (1 * 2 + 1) * RECT_FLOATS;
    expect(buf[off]).toBe(1 * 8 * 2); // x
    expect(buf[off + 1]).toBe(1 * 14 * 2); // y
    expect(buf[off + 2]).toBe(8 * 2); // w
    expect(buf[off + 3]).toBe(14 * 2); // h
  });

  it("turns a previously-non-default bg into a degenerate quad when it reverts", () => {
    const cells = store(1, 1);
    put(cells, 0, 0, 0x20, DEFAULT_FG, RED);
    const buf = new Float32Array(RECT_FLOATS);
    writeBgCells(buf, cells, [span(0, 0, 1)], DEFAULT_BG, M1);
    expect(buf[2]).toBe(10);
    put(cells, 0, 0, 0x20, DEFAULT_FG, DEFAULT_BG);
    writeBgCells(buf, cells, [span(0, 0, 1)], DEFAULT_BG, M1);
    expect(buf[2]).toBe(0);
  });

  it("leaves untouched rows' buffer regions byte-identical", () => {
    const cells = store(3, 2);
    put(cells, 0, 0, 0x20, DEFAULT_FG, RED);
    put(cells, 1, 1, 0x20, DEFAULT_FG, BLUE);
    const rowFloats = 3 * RECT_FLOATS;

    const full = new Float32Array(2 * rowFloats);
    writeBgCells(full, cells, [span(0, 0, 3), span(1, 0, 3)], DEFAULT_BG, M1);

    // Rebuild ONLY row 1 into a copy of `full`; row 0 must not change.
    const inc = full.slice();
    writeBgCells(inc, cells, [span(1, 0, 3)], DEFAULT_BG, M1);
    expect(inc.subarray(0, rowFloats)).toEqual(full.subarray(0, rowFloats));
    expect(inc.subarray(rowFloats)).toEqual(full.subarray(rowFloats));
  });
});

describe("writeGlyphCells", () => {
  it("emits per-cell slots, skipping empty cells and wide spacers", () => {
    const cells = store(4, 1);
    put(cells, 0, 0, 0x41); // A
    put(cells, 0, 2, 0x42); // B
    put(cells, 0, 3, 0x0, 0, 0, CELL_FLAGS.wideSpacer);
    const buf = new Float32Array(4 * GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(0, 0, 4)], hitAtlas(), 2048, M1, []);

    expect(buf.subarray(0, GLYPH_FLOATS)[2]).toBe(10); // col 0 glyph
    expect(buf.subarray(GLYPH_FLOATS, 2 * GLYPH_FLOATS)[2]).toBe(0); // col 1 empty
    expect(buf.subarray(2 * GLYPH_FLOATS, 3 * GLYPH_FLOATS)[2]).toBe(10); // col 2
    expect(buf.subarray(3 * GLYPH_FLOATS, 4 * GLYPH_FLOATS)[2]).toBe(0); // spacer
  });

  it("orders instances by cell index and positions x/y from row/col", () => {
    const cells = store(3, 2);
    put(cells, 1, 2, 0x41);
    const buf = new Float32Array(6 * GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(1, 2, 1)], hitAtlas(), 2048, M1, []);
    const off = (1 * 3 + 2) * GLYPH_FLOATS;
    expect(buf[off]).toBe(2 * 10); // x = col 2
    expect(buf[off + 1]).toBe(1 * 16); // y = row 1
    expect(buf[off + 2]).toBe(10);
    expect(buf[off + 3]).toBe(16);
  });

  it("writes page/uv fractions (pad excluded — 1:1 texels) and the fg tint", () => {
    const cells = store(1, 1);
    put(cells, 0, 0, 0x41, RED);
    const buf = new Float32Array(GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(0, 0, 1)], hitAtlas(), 2048, M1, []);
    expect(buf[8]).toBe(1); // page
    // UVs cover the glyph region only: the 1px slot pad is bleed guard, and
    // sampling it scaled+shifted every glyph (fat-text host bug).
    expect(buf[4]).toBe(101 / 2048); // u = slot.x + pad
    expect(buf[5]).toBe(201 / 2048); // v = slot.y + pad
    expect(buf[6]).toBe(7 / 2048); // uw = slot.w - 2·pad
    expect(buf[7]).toBe(15 / 2048); // vh = slot.h - 2·pad
    expect(buf[9]).toBeCloseTo(0xac / 255, 6); // fg r
    expect(buf[10]).toBeCloseTo(0x42 / 255, 6);
    expect(buf[11]).toBeCloseTo(0x42 / 255, 6);
    expect(buf[13]).toBe(0); // not a color glyph
  });

  it("flags color glyphs (emoji) and marks wide glyphs as two cells wide", () => {
    const cells = store(4, 1);
    put(cells, 0, 0, 0x1f600); // 😀
    put(cells, 0, 2, 0x4f60, DEFAULT_FG, DEFAULT_BG, CELL_FLAGS.wide); // 你
    const buf = new Float32Array(4 * GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(0, 0, 4)], hitAtlas(), 2048, M1, []);
    expect(buf[GLYPH_COLORED === 1 ? 13 : 13]).toBe(GLYPH_COLORED); // emoji colored bit
    const wideOff = 2 * GLYPH_FLOATS;
    expect(buf[wideOff + 2]).toBe(2 * 10); // wide glyph spans 2 cells
    expect(buf[wideOff + 13]).toBe(0);
  });

  it("dims the fg tint to 0.66× when the cell is dim", () => {
    const cells = store(1, 1);
    put(cells, 0, 0, 0x41, RED, DEFAULT_BG, CELL_FLAGS.dim);
    const buf = new Float32Array(GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(0, 0, 1)], hitAtlas(), 2048, M1, []);
    expect(buf[9]).toBeCloseTo((0xac / 255) * 0.66, 4);
    expect(buf[10]).toBeCloseTo((0x42 / 255) * 0.66, 4);
  });

  it("collects atlas misses with key/wide/bold/italic/row/col", () => {
    const cells = store(4, 2);
    put(cells, 0, 1, 0x41);
    put(cells, 1, 3, 0x4f60, DEFAULT_FG, DEFAULT_BG, CELL_FLAGS.wide | CELL_FLAGS.bold);
    const misses: { key: number; codepoint: number; wide: boolean; bold: boolean; italic: boolean; row: number; col: number }[] = [];
    const buf = new Float32Array(8 * GLYPH_FLOATS);
    writeGlyphCells(buf, cells, [span(0, 0, 4), span(1, 0, 4)], missAtlas(), 2048, M1, misses);

    expect(misses).toHaveLength(2);
    expect(misses[0]).toMatchObject({ codepoint: 0x41, wide: false, bold: false, italic: false, row: 0, col: 1 });
    expect(misses[1]).toMatchObject({ codepoint: 0x4f60, wide: true, bold: true, italic: false, row: 1, col: 3 });
    // Missed cells become degenerate so they rasterize nothing this frame.
    expect(buf[(1 * 4 + 3) * GLYPH_FLOATS + 2]).toBe(0);
  });

  it("leaves untouched rows' buffer regions byte-identical", () => {
    const cells = store(3, 2);
    put(cells, 0, 0, 0x41);
    put(cells, 1, 1, 0x42);
    const rowFloats = 3 * GLYPH_FLOATS;
    const full = new Float32Array(2 * rowFloats);
    writeGlyphCells(full, cells, [span(0, 0, 3), span(1, 0, 3)], hitAtlas(), 2048, M1, []);
    const inc = full.slice();
    writeGlyphCells(inc, cells, [span(1, 0, 3)], hitAtlas(), 2048, M1, []);
    expect(inc.subarray(0, rowFloats)).toEqual(full.subarray(0, rowFloats));
    expect(inc.subarray(rowFloats)).toEqual(full.subarray(rowFloats));
  });
});

describe("writeDecorCells", () => {
  const thin = Math.max(2, Math.round(16 * 0.12)); // 2 device px @ dpr 1

  function decorFor(flags: number): Float32Array {
    const cells = store(1, 1);
    put(cells, 0, 0, 0x41, RED, DEFAULT_BG, flags);
    const buf = new Float32Array(RECT_FLOATS);
    writeDecorCells(buf, cells, [span(0, 0, 1)], M1);
    return buf;
  }

  it("emits a bottom thin rect for a plain underline", () => {
    const buf = decorFor(CELL_FLAGS.underline);
    expect(buf[0]).toBe(0);
    expect(buf[1]).toBe(16 - thin); // y at the cell bottom
    expect(buf[2]).toBe(10);
    expect(buf[3]).toBe(thin);
    expect(buf[8]).toBe(RECT_KIND_PLAIN);
  });

  it("emits a mid-line thin rect for strikeout", () => {
    const buf = decorFor(CELL_FLAGS.strikeout);
    expect(buf[1]).toBe((16 - thin) / 2);
    expect(buf[8]).toBe(RECT_KIND_PLAIN);
  });

  it("emits full-cell rects for procedural kinds", () => {
    expect(decorFor(CELL_FLAGS.undercurl)[8]).toBe(RECT_KIND_UNDERCURL);
    expect(decorFor(CELL_FLAGS.dashedUnderline)[8]).toBe(RECT_KIND_DASHED);
    expect(decorFor(CELL_FLAGS.dottedUnderline)[8]).toBe(RECT_KIND_DOTTED);
    expect(decorFor(CELL_FLAGS.doubleUnderline)[8]).toBe(RECT_KIND_DOUBLE);
    const undercurl = decorFor(CELL_FLAGS.undercurl);
    expect(undercurl[3]).toBe(16); // full height
  });

  it("emits the combined strike+underline kind and none for a plain cell", () => {
    expect(decorFor(CELL_FLAGS.underline | CELL_FLAGS.strikeout)[8]).toBe(RECT_KIND_STRIKE_UNDER);
    const plain = decorFor(0);
    expect(plain[2]).toBe(0); // degenerate
  });

  it("uses the cell fg (dimmed like the glyph)", () => {
    const buf = decorFor(CELL_FLAGS.underline | CELL_FLAGS.dim);
    expect(buf[4]).toBeCloseTo((0xac / 255) * 0.66, 4);
  });

  it("spans two cells for a wide decorated cell", () => {
    const cells = store(4, 1);
    put(cells, 0, 1, 0x4f60, RED, DEFAULT_BG, CELL_FLAGS.underline | CELL_FLAGS.wide);
    const buf = new Float32Array(4 * RECT_FLOATS);
    writeDecorCells(buf, cells, [span(0, 0, 4)], M1);
    const off = 1 * RECT_FLOATS;
    expect(buf[off + 2]).toBe(2 * 10);
    expect(buf[off + 0]).toBe(10);
  });
});

describe("bg frame rects (selection / matches / cursor fill)", () => {
  function selRectCount(frame: Frame, cols: number, rows: number, focused: boolean): number {
    const buf = new Float32Array(64 * RECT_FLOATS);
    return buildBgFrameRects(buf, frame, cols, rows, 0, M1, CURSOR_COLOR, focused);
  }

  it("draws one rect per selected row, viewport-clipped", () => {
    const cells = store(4, 3);
    const frame = makeFrame({
      store: cells,
      selection: { startRow: 1, startCol: 1, endRow: 2, endCol: 2 },
    });
    const buf = new Float32Array(64 * RECT_FLOATS);
    const n = buildBgFrameRects(buf, frame, 4, 3, 0, M1, CURSOR_COLOR, true);
    expect(n).toBe(2);
    expect(buf[0]).toBe(1 * 10); // row 1 x
    expect(buf[1]).toBe(1 * 16);
    // Middle rows are full-width (matches the DOM oracle): row 1 runs from
    // its start col to the right edge, row 2 from the left edge to endCol+1.
    expect(buf[2]).toBe((4 - 1) * 10);
    expect(buf[3]).toBe(16);
    expect(buf[4]).toBeCloseTo(0x5e / 255, 6); // selection color
    expect(buf[5]).toBeCloseTo(0xa6 / 255, 6);
    expect(buf[7]).toBeCloseTo(0x4d / 255, 6);
    // second rect: row 2, cols 0..endCol+1
    const off = RECT_FLOATS;
    expect(buf[off]).toBe(0);
    expect(buf[off + 1]).toBe(2 * 16);
    expect(buf[off + 2]).toBe(3 * 10);
  });

  it("draws search matches with the match color", () => {
    const cells = store(3, 1);
    const frame = makeFrame({
      store: cells,
      matches: [{ startRow: 0, startCol: 0, endRow: 0, endCol: 2 }],
    });
    const buf = new Float32Array(16 * RECT_FLOATS);
    const n = buildBgFrameRects(buf, frame, 3, 1, 0, M1, CURSOR_COLOR, true);
    expect(n).toBe(1);
    expect(buf[4]).toBeCloseTo(0xff / 255, 6);
    expect(buf[5]).toBeCloseTo(0xbf / 255, 6);
    expect(buf[6]).toBe(0);
  });

  it("adds the focused block-cursor fill and countFrameRects agrees", () => {
    const cells = store(3, 2);
    const frame = makeFrame({
      store: cells,
      cursor: { row: 1, col: 2, visible: true, shape: "block" },
    });
    expect(selRectCount(frame, 3, 2, true)).toBe(1);
    expect(selRectCount(frame, 3, 2, false)).toBe(0);
    expect(countFrameRects(frame, 3, 2, true)).toBe(1);
    expect(countFrameRects(frame, 3, 2, false)).toBe(0);
  });

  it("counts selection + matches so the renderer can size the region", () => {
    const cells = store(4, 3);
    const frame = makeFrame({
      store: cells,
      selection: { startRow: 0, startCol: 0, endRow: 2, endCol: 3 },
      matches: [{ startRow: 1, startCol: 0, endRow: 1, endCol: 1 }],
    });
    expect(countFrameRects(frame, 4, 3, true)).toBe(4); // 3 sel rows + 1 match + cursor? cursor hidden
  });
});

describe("cursor rects", () => {
  function cursorRects(opts: Partial<FrameCursor> & { row: number; col: number }, focused = true): { n: number; buf: Float32Array } {
    const cells = store(3, 2);
    const frame = makeFrame({ store: cells, cursor: opts });
    const buf = new Float32Array(8 * RECT_FLOATS);
    return { n: buildCursorRects(buf, frame, M1, focused, CURSOR_COLOR), buf };
  }

  it("emits nothing for a hidden cursor", () => {
    expect(cursorRects({ row: 0, col: 0, visible: false }).n).toBe(0);
    expect(cursorRects({ row: 0, col: 0, shape: "hidden", visible: true }).n).toBe(0);
  });

  it("emits a 2px beam bar and a thin underline bar", () => {
    const beam = cursorRects({ row: 1, col: 2, visible: true, shape: "beam" });
    expect(beam.n).toBe(1);
    expect(beam.buf[0]).toBe(2 * 10);
    expect(beam.buf[1]).toBe(16);
    expect(beam.buf[2]).toBe(2);
    expect(beam.buf[3]).toBe(16);

    const underline = cursorRects({ row: 0, col: 0, visible: true, shape: "underline" });
    const thin = Math.max(2, Math.round(16 * 0.12));
    expect(underline.buf[1]).toBe(16 - thin);
    expect(underline.buf[3]).toBe(thin);
  });

  it("emits a hollow ring for hollow and for an unfocused block", () => {
    const hollow = cursorRects({ row: 0, col: 1, visible: true, shape: "hollow" });
    expect(hollow.buf[8]).toBe(RECT_KIND_HOLLOW);
    expect(hollow.buf[2]).toBe(10);

    const unfocusedBlock = cursorRects({ row: 0, col: 1, visible: true, shape: "block" }, false);
    expect(unfocusedBlock.n).toBe(1);
    expect(unfocusedBlock.buf[8]).toBe(RECT_KIND_HOLLOW);

    // Focused block emits nothing here — its fill lives in the bg pass.
    expect(cursorRects({ row: 0, col: 1, visible: true, shape: "block" }, true).n).toBe(0);
  });
});

describe("blockCursorGlyph", () => {
  function cg(cursor: Partial<FrameCursor>, focused: boolean): { row: number; col: number } | null {
    const cells = store(3, 2);
    return blockCursorGlyph(
      makeFrame({ store: cells, cursor: { row: 1, col: 2, visible: true, shape: "block", ...cursor } }),
      focused,
    );
  }

  it("redraws the glyph only for a focused visible block", () => {
    expect(cg({}, true)).toEqual({ row: 1, col: 2 });
    expect(cg({}, false)).toBeNull();
    expect(cg({ shape: "beam" }, true)).toBeNull();
    expect(cg({ shape: "hidden", visible: true }, true)).toBeNull();
    expect(cg({ visible: false }, true)).toBeNull();
  });
});

describe("shader sources", () => {
  it("are stable snapshots (accidental-edit guard; real compile is host-side)", () => {
    expect(COLOR_VERT_SRC).toMatchSnapshot("color vertex");
    expect(COLOR_FRAG_SRC).toMatchSnapshot("color fragment");
    expect(GLYPH_VERT_SRC).toMatchSnapshot("glyph vertex");
    expect(GLYPH_FRAG_SRC).toMatchSnapshot("glyph fragment");
  });

  it("are GLSL ES 3.00 with the expected entrypoints", () => {
    for (const src of [COLOR_VERT_SRC, COLOR_FRAG_SRC, GLYPH_VERT_SRC, GLYPH_FRAG_SRC]) {
      expect(src.startsWith("#version 300 es")).toBe(true);
    }
    expect(COLOR_VERT_SRC).toContain("gl_VertexID");
    expect(GLYPH_FRAG_SRC).toContain("sampler2DArray");
    expect(GLYPH_FRAG_SRC).toContain("texture(u_atlas");
  });
});

// --- headless GL smoke test ------------------------------------------------
// jsdom has no WebGL2, so this file stubs the context to exercise the
// apply() → uploads → draw() path (draw counts, per-dirty-row bufferSubData
// granularity, atlas-miss rasterize). Real pixels are host-verified.

interface SubDataRecord {
  buffer: object;
  dstOffset: number;
  srcOffset: number;
  length: number;
}

let mockGl: Record<string, unknown>;
let mock2d: Record<string, unknown>;
let buffers: object[];

beforeEach(() => {
  buffers = [];
  const draws: { instanceCount: number }[] = [];
  const subData: SubDataRecord[] = [];
  const gl: Record<string, unknown> = {
    VERTEX_SHADER: 1,
    FRAGMENT_SHADER: 2,
    COMPILE_STATUS: 3,
    LINK_STATUS: 4,
    ARRAY_BUFFER: 5,
    ELEMENT_ARRAY_BUFFER: 6,
    DYNAMIC_DRAW: 7,
    FLOAT: 8,
    TEXTURE_2D_ARRAY: 9,
    RGBA8: 10,
    RGBA: 11,
    UNSIGNED_BYTE: 12,
    LINEAR: 13,
    CLAMP_TO_EDGE: 14,
    UNPACK_PREMULTIPLY_ALPHA_WEBGL: 15,
    TEXTURE0: 16,
    DEPTH_TEST: 17,
    CULL_FACE: 18,
    BLEND: 19,
    ONE: 20,
    ONE_MINUS_SRC_ALPHA: 21,
    TRIANGLE_STRIP: 22,
    TEXTURE_MIN_FILTER: 23,
    TEXTURE_MAG_FILTER: 24,
    TEXTURE_WRAP_S: 25,
    TEXTURE_WRAP_T: 26,
    disable: vi.fn(),
    enable: vi.fn(),
    blendFunc: vi.fn(),
    createShader: vi.fn(() => ({})),
    shaderSource: vi.fn(),
    compileShader: vi.fn(),
    getShaderParameter: vi.fn(() => true),
    getShaderInfoLog: vi.fn(() => ""),
    deleteShader: vi.fn(),
    createProgram: vi.fn(() => ({})),
    attachShader: vi.fn(),
    linkProgram: vi.fn(),
    getProgramParameter: vi.fn(() => true),
    getProgramInfoLog: vi.fn(() => ""),
    deleteProgram: vi.fn(),
    useProgram: vi.fn(),
    getUniformLocation: vi.fn(() => ({})),
    uniform2f: vi.fn(),
    uniform1f: vi.fn(),
    uniform1i: vi.fn(),
    createTexture: vi.fn(() => ({})),
    bindTexture: vi.fn(),
    deleteTexture: vi.fn(),
    activeTexture: vi.fn(),
    texStorage3D: vi.fn(),
    texParameteri: vi.fn(),
    pixelStorei: vi.fn(),
    texSubImage3D: vi.fn(),
    clearTexImage: vi.fn(),
    createVertexArray: vi.fn(() => ({})),
    bindVertexArray: vi.fn(),
    deleteVertexArray: vi.fn(),
    createBuffer: vi.fn(() => {
      const b: object = {};
      buffers.push(b);
      return b;
    }),
    bindBuffer: vi.fn(),
    bufferData: vi.fn(),
    bufferSubData: vi.fn((_t: number, dstOffset: number, _d: Float32Array, srcOffset: number, length: number) => {
      subData.push({ buffer: gl.__currentBuffer as object, dstOffset, srcOffset, length });
    }),
    deleteBuffer: vi.fn(),
    enableVertexAttribArray: vi.fn(),
    vertexAttribPointer: vi.fn(),
    vertexAttribDivisor: vi.fn(),
    viewport: vi.fn(),
    clearColor: vi.fn(),
    clear: vi.fn(),
    drawArraysInstanced: vi.fn((_m: number, _f: number, _c: number, instanceCount: number) => {
      draws.push({ instanceCount });
    }),
    __currentBuffer: null,
    __draws: draws,
    __subData: subData,
  };
  gl.bindBuffer = vi.fn((_t: number, b: object | null) => {
    gl.__currentBuffer = b;
  });
  mockGl = gl;

  mock2d = {
    fillStyle: "",
    font: "",
    textBaseline: "alphabetic",
    measureText: vi.fn(() => ({ width: 0 })),
    fillText: vi.fn(),
    clearRect: vi.fn(),
    strokeRect: vi.fn(),
    beginPath: vi.fn(),
    moveTo: vi.fn(),
    lineTo: vi.fn(),
    stroke: vi.fn(),
    getImageData: vi.fn((_x: number, _y: number, w: number, h: number) => ({
      data: new Uint8ClampedArray(w * h * 4),
    })),
  };

  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation((id: string) => {
    if (id === "webgl2") return mockGl as unknown as WebGL2RenderingContext;
    if (id === "2d") return mock2d as unknown as CanvasRenderingContext2D;
    return null;
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("WebGLRenderer (headless mock GL)", () => {
  it("throws cleanly when WebGL2 is unavailable and removes its canvas", () => {
    vi.restoreAllMocks(); // undo the mock so getContext("webgl2") returns null
    const container = document.createElement("div");
    expect(() => new WebGLRenderer({ container, cellW: 8, cellH: 14, dpr: 1, baseline: 11, fontFamily: "JetBrains Mono", fontSize: 14 })).toThrow(/WebGL2/);
    expect(container.children.length).toBe(0);
  });

  it("drives the three passes and uploads only dirty rows", () => {
    const container = document.createElement("div");
    const renderer = new WebGLRenderer({ container, cellW: 8, cellH: 14, dpr: 1, baseline: 11, fontFamily: "JetBrains Mono", fontSize: 14 });
    const draws = mockGl.__draws as { instanceCount: number }[];
    const subData = mockGl.__subData as SubDataRecord[];
    const bgGpu = buffers[0]; // createBuffer order: bg, decor, cursor, glyph, cursorGlyph

    const cells = store(4, 2);
    put(cells, 0, 0, 0x41, RED, RED); // glyph + non-default bg
    put(cells, 0, 1, 0x42, BLUE);
    put(cells, 1, 0, 0x1f600); // emoji → colored + atlas miss
    const frame = makeFrame({
      store: cells,
      kind: "full",
      rows: [{ row: 0, colStart: 0, cellCount: 4 }, { row: 1, colStart: 0, cellCount: 4 }],
      cursor: { row: 0, col: 2, visible: true, shape: "block" },
    });

    renderer.apply(frame, cells);

    // Pass 1 bg: 8 cells + 1 focused block fill = 9 instances.
    // Pass 2 glyph: 8. Pass 3a decor: 8. Pass 3b cursor rects: 0 (block
    // focused). Pass 3c inverse glyph: 1.
    expect(draws.map((d) => d.instanceCount)).toEqual([9, 8, 8, 1]);

    // Full frame → every row uploaded for bg/glyph/decor (row stride =
    // cols × floats).
    const bgRowStride = 4 * RECT_FLOATS;
    const bgSubData = subData.filter((s) => s.buffer === bgGpu);
    expect(bgSubData.some((s) => s.dstOffset === 0 * bgRowStride * 4 && s.length === bgRowStride)).toBe(true);
    expect(bgSubData.some((s) => s.dstOffset === 1 * bgRowStride * 4 && s.length === bgRowStride)).toBe(true);

    // Delta touching only row 1 → only row 1 re-uploaded (untouched rows
    // never re-sent).
    draws.length = 0;
    subData.length = 0;
    put(cells, 1, 1, 0x58, RED, BLUE);
    const delta = makeFrame({
      store: cells,
      kind: "delta",
      rows: [{ row: 1, colStart: 0, cellCount: 4 }],
      cursor: { row: 1, col: 1, visible: true, shape: "block" },
    });
    renderer.apply(delta, cells);
    const deltaBg = subData.filter((s) => s.buffer === bgGpu);
    expect(deltaBg.some((s) => s.dstOffset === 1 * bgRowStride * 4)).toBe(true);
    expect(deltaBg.some((s) => s.dstOffset === 0)).toBe(false);
    // No selection/matches → no frame-rect upload; bg count = 8 + fill(1).
    expect(draws[0].instanceCount).toBe(9);

    renderer.dispose();
  });

  it("rasterizes atlas misses through the mock canvas and patches their slots", () => {
    const container = document.createElement("div");
    const renderer = new WebGLRenderer({ container, cellW: 8, cellH: 14, dpr: 1, baseline: 11, fontFamily: "JetBrains Mono", fontSize: 14 });
    const cells = store(2, 1);
    put(cells, 0, 0, 0x41, RED);
    renderer.apply(
      makeFrame({ store: cells, kind: "full", rows: [{ row: 0, colStart: 0, cellCount: 2 }] }),
      cells,
    );
    // The miss was rasterized: the mock 2d context saw a draw call and the
    // GL context uploaded a dirty rect.
    expect(mock2d.measureText).toHaveBeenCalled();
    expect(mockGl.texSubImage3D).toHaveBeenCalled();
    renderer.dispose();
  });
});
