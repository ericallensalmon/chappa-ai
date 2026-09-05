/// <reference types="node" />
import { describe, expect, it } from "vitest";
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  CELL_BYTES,
  CURSOR_SHAPES,
  FRAME_HEADER_LEN,
  FRAME_MAGIC,
  FRAME_VERSION,
  decodeFrame,
  type Frame,
} from "./protocol";

// Fixtures live in the Rust crate; both sides consume the exact same files.
const FIXTURES_DIR = fileURLToPath(
  new URL("../../../term-core/tests/fixtures/frames", import.meta.url),
);

interface RangeJson {
  startRow: number;
  startCol: number;
  endRow: number;
  endCol: number;
}
interface FixtureCell {
  ch: string;
  fg: string;
  bg: string;
  flags: string[];
  link: number;
}
interface FixtureRow {
  row: number;
  colStart: number;
  cells: FixtureCell[];
}
interface Fixture {
  name: string;
  seq: number;
  kind: "full" | "delta";
  cursor: { row: number; col: number; shape: string; visible: boolean };
  displayOffset: number;
  historyLen: number;
  selection: RangeJson | null;
  selectionActive?: boolean;
  /** Any mouse tracking protocol active (wire flags bit 1). */
  mouseCapture?: boolean;
  /** Alternate screen active (wire flags bit 2). */
  altScreen?: boolean;
  rows: FixtureRow[];
  zerowidth: { row: number; col: number; chars: string[] }[];
  matches: RangeJson[];
  truncateTo?: number;
}

/** "RRGGBBAA" → the wire u32 (r<<24|g<<16|b<<8|a), which parses identically. */
function colorFromHex(hex: string): number {
  return parseInt(hex, 16);
}

/** Fixture flags → wire bits, mirrored from `CELL_FLAGS` / frame.rs. */
function flagBits(name: string): number {
  const table: Record<string, number> = {
    bold: 1 << 0,
    italic: 1 << 1,
    dim: 1 << 2,
    underline: 1 << 3,
    double_underline: 1 << 4,
    undercurl: 1 << 5,
    dotted_underline: 1 << 6,
    dashed_underline: 1 << 7,
    inverse: 1 << 8,
    strikeout: 1 << 9,
    hidden: 1 << 10,
    wide: 1 << 11,
    wide_spacer: 1 << 12,
  };
  const bits = table[name];
  if (bits === undefined) throw new Error(`fixture flag ${name}`);
  return bits;
}

/** A fixture `ch` ("" means "no glyph"/wide spacer) → wire codepoint (space). */
function charCode(ch: string): number {
  if (ch === "") return 0x20;
  const cp = ch.codePointAt(0);
  if (cp === undefined) throw new Error(`fixture ch ${JSON.stringify(ch)}`);
  return cp;
}

function expectFrameMatchesJson(frame: Frame, fx: Fixture): void {
  expect(frame.seq).toBe(fx.seq);
  expect(frame.kind).toBe(fx.kind);
  expect(frame.cursor.row).toBe(fx.cursor.row);
  expect(frame.cursor.col).toBe(fx.cursor.col);
  expect(frame.cursor.shape).toBe(fx.cursor.shape);
  expect(CURSOR_SHAPES).toContain(fx.cursor.shape);
  expect(frame.cursor.visible).toBe(fx.cursor.visible);
  expect(frame.displayOffset).toBe(fx.displayOffset);
  expect(frame.historyLen).toBe(fx.historyLen);

  if (fx.selection === null) {
    expect(frame.selection).toBeNull();
  } else {
    expect(frame.selection).toEqual(fx.selection);
  }
  expect(frame.selectionActive).toBe(fx.selectionActive ?? fx.selection !== null);
  expect(frame.mouseCapture).toBe(fx.mouseCapture ?? false);
  expect(frame.altScreen).toBe(fx.altScreen ?? false);

  expect(frame.matches).toEqual(fx.matches);

  expect(frame.rows).toEqual(
    fx.rows.map((r) => ({ row: r.row, colStart: r.colStart, cellCount: r.cells.length })),
  );

  // Cell store: every fixture cell must land at (row, col) with the exact
  // ch/fg/bg/flags/link from the JSON.
  const { cells } = frame;
  expect(cells.rows * cells.cols).toBe(cells.ch.length);
  for (const r of fx.rows) {
    const base = r.row * cells.cols + r.colStart;
    r.cells.forEach((c, i) => {
      const idx = base + i;
      expect(cells.ch[idx]).toBe(charCode(c.ch));
      expect(cells.fg[idx]).toBe(colorFromHex(c.fg));
      expect(cells.bg[idx]).toBe(colorFromHex(c.bg));
      expect(cells.flags[idx]).toBe(c.flags.reduce((acc, f) => acc | flagBits(f), 0));
      expect(cells.link[idx]).toBe(c.link);
    });
  }

  // Zero-width table, chars as codepoints.
  expect(frame.zerowidth.length).toBe(fx.zerowidth.length);
  fx.zerowidth.forEach((z, i) => {
    expect(frame.zerowidth[i].row).toBe(z.row);
    expect(frame.zerowidth[i].col).toBe(z.col);
    expect(Array.from(frame.zerowidth[i].chars)).toEqual(z.chars.map(charCode));
  });
}

function readBin(path: string): ArrayBuffer {
  const buf = readFileSync(path);
  return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength) as ArrayBuffer;
}

/** Build a DELTA frame buffer covering the given spans (cells: packed
 *  {ch, fg, bg, flags, link} or defaults). Used to test retained-store
 *  application without shipping a fixture for every span. */
function deltaSpanBuffer(
  seq: number,
  spans: { row: number; colStart: number; cells: number[][] }[],
): ArrayBuffer {
  const cellCount = spans.reduce((n, s) => n + s.cells.length, 0);
  const buf = new ArrayBuffer(37 + spans.length * 6 + cellCount * CELL_BYTES + 2 + 2);
  const view = new DataView(buf);
  let p = 0;
  view.setUint8(p++, FRAME_MAGIC);
  view.setUint8(p++, FRAME_VERSION);
  view.setUint32(p, seq, true);
  p += 4;
  view.setUint8(p++, 1); // kind: delta
  view.setUint8(p++, 0); // flags
  view.setUint16(p, 0, true); // cursor row
  p += 2;
  view.setUint16(p, 0, true); // cursor col
  p += 2;
  view.setUint8(p++, 4); // cursor shape hidden
  view.setUint8(p++, 0); // cursor visible
  view.setUint32(p, 0, true); // display_offset
  p += 4;
  view.setUint32(p, 0, true); // history_len
  p += 4;
  view.setUint8(p++, 0); // selection present
  view.setUint32(p, 0, true);
  p += 4;
  view.setUint16(p, 0, true);
  p += 2;
  view.setUint32(p, 0, true);
  p += 4;
  view.setUint16(p, 0, true);
  p += 2;
  view.setUint16(p, spans.length, true); // row_count
  p += 2;
  for (const span of spans) {
    view.setUint16(p, span.row, true);
    p += 2;
    view.setUint16(p, span.colStart, true);
    p += 2;
    view.setUint16(p, span.cells.length, true);
    p += 2;
    for (const [ch, fg, bg, flags, link] of span.cells) {
      view.setUint32(p, ch, true);
      p += 4;
      view.setUint32(p, fg, true);
      p += 4;
      view.setUint32(p, bg, true);
      p += 4;
      view.setUint16(p, flags, true);
      p += 2;
      view.setUint16(p, link, true);
      p += 2;
    }
  }
  view.setUint16(p, 0, true); // zerowidth_count
  p += 2;
  view.setUint16(p, 0, true); // match_count
  p += 2;
  return buf;
}

describe("protocol constants", () => {
  it("matches the documented wire layout", () => {
    expect(CELL_BYTES).toBe(16);
    expect(FRAME_MAGIC).toBe(0xd7);
    expect(FRAME_VERSION).toBe(1);
  });
});

describe("decodeFrame", () => {
  const names = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith(".json"))
    .sort();
  expect(names.length).toBeGreaterThan(0);

  it.each(names)("decodes fixture %s", (name) => {
    const fx: Fixture = JSON.parse(readFileSync(`${FIXTURES_DIR}/${name}`, "utf8"));
    expect(fx.name).toBe(name.replace(/\.json$/, ""));
    const bin = readBin(`${FIXTURES_DIR}/${name.replace(/\.json$/, ".bin")}`);

    if (fx.truncateTo !== undefined) {
      expect(() => decodeFrame(bin)).toThrow();
      return;
    }

    const frame = decodeFrame(bin);
    expectFrameMatchesJson(frame, fx);
  });

  it("rejects an empty buffer", () => {
    expect(() => decodeFrame(new ArrayBuffer(0))).toThrow(/magic/);
  });

  it("rejects a wrong magic byte", () => {
    const bytes = new Uint8Array(FRAME_HEADER_LEN);
    bytes[0] = 0x00;
    expect(() => decodeFrame(bytes.buffer)).toThrow(/magic/);
  });

  it("rejects a wrong version", () => {
    const bytes = new Uint8Array(FRAME_HEADER_LEN);
    bytes[0] = FRAME_MAGIC;
    bytes[1] = 0x7f;
    expect(() => decodeFrame(bytes.buffer)).toThrow(/version/);
  });

  it("rejects reserved flag bits above bit 2 (wire rule)", () => {
    // Magic + version + seq(0) + kind full + flags byte at offset 7. A full
    // frame needs no rows, so a header-only buffer is otherwise decodable.
    const mk = (flags: number): ArrayBuffer => {
      // Header (through row_count) + zerowidth_count + match_count = 41B.
      const bytes = new Uint8Array(FRAME_HEADER_LEN + 4);
      bytes[0] = FRAME_MAGIC;
      bytes[1] = FRAME_VERSION;
      bytes[6] = 0; // kind: full
      bytes[7] = flags;
      return bytes.buffer;
    };
    for (const flags of [0x08, 0x10, 0x40, 0x80]) {
      expect(() => decodeFrame(mk(flags))).toThrow(/flags/);
    }
    // Bits 0..=2 are defined and decode.
    expect(decodeFrame(mk(0b111)).selectionActive).toBe(true);
    expect(decodeFrame(mk(0b111)).mouseCapture).toBe(true);
    expect(decodeFrame(mk(0b111)).altScreen).toBe(true);
  });

  it("rejects a header cut off mid-way", () => {
    const bin = readBin(`${FIXTURES_DIR}/seq-wraparound.bin`);
    // Valid full frame truncated to a header boundary.
    expect(() => decodeFrame(bin.slice(0, FRAME_HEADER_LEN - 3))).toThrow();
  });

  it("applies a DELTA onto a retained store (spans overwrite, rest untouched)", () => {
    const full = decodeFrame(readBin(`${FIXTURES_DIR}/full-80x24.bin`));
    const { cells } = full;
    expect(cells.cols).toBe(80);
    expect(cells.rows).toBe(24);
    const before = cells.ch.slice();

    // A delta overwriting row 5 columns 1..2 with distinct cells.
    const delta = deltaSpanBuffer(2, [
      {
        row: 5,
        colStart: 1,
        cells: [
          [0x51, 0xac4242ff, 0x181818ff, 1, 7], // 'Q', red fg, bold, link 7
          [0x52, 0xac4242ff, 0x181818ff, 0, 0], // 'R'
        ],
      },
    ]);
    const applied = decodeFrame(delta, cells);

    // Same storage back — retained, not a fresh allocation.
    expect(applied.cells).toBe(cells);
    expect(applied.kind).toBe("delta");
    expect(applied.cells.cols).toBe(80);
    expect(applied.cells.rows).toBe(24);

    const idx5_1 = 5 * 80 + 1;
    const idx5_2 = 5 * 80 + 2;
    expect(cells.ch[idx5_1]).toBe(0x51);
    expect(cells.fg[idx5_1]).toBe(0xac4242ff);
    expect(cells.flags[idx5_1]).toBe(1);
    expect(cells.link[idx5_1]).toBe(7);
    expect(cells.ch[idx5_2]).toBe(0x52);

    // Untouched cells keep their previous content — same row neighbours and
    // other rows are unchanged.
    expect(cells.ch[idx5_1 - 1]).toBe(before[idx5_1 - 1]);
    expect(cells.ch[idx5_1 + 2]).toBe(before[idx5_1 + 2]);
    expect(cells.ch[6 * 80]).toBe(before[6 * 80]);
    expect(cells.ch[5 * 80 + 3]).toBe(before[5 * 80 + 3]);
  });

  it("rejects a DELTA whose spans overflow a pinned store", () => {
    const full = decodeFrame(readBin(`${FIXTURES_DIR}/full-80x24.bin`));
    const delta = deltaSpanBuffer(2, [
      { row: 24, colStart: 0, cells: [[0x51, 0xac4242ff, 0x181818ff, 0, 0]] },
    ]);
    expect(() => decodeFrame(delta, full.cells)).toThrow(/out of grid/);
  });
});
