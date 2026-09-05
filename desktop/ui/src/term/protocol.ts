// Binary frame protocol decoder.
// Layout must mirror term-core/src/frame.rs exactly (v1, little-endian),
// and both sides validate against the same checked-in fixtures under
// term-core/tests/fixtures/frames/ (Rust writes *.bin, this decodes them).
//
// Wire layout (v1):
//   magic u8 = 0xD7 | version u8 = 1 | seq u32 | kind u8 |
//   flags u8 (bit 0 = selection_active, bit 1 = mouse_capture, bit 2 =
//   alt_screen; bits 3..=7 reserved) |
//   cursor_row u16 | cursor_col u16 | cursor_shape u8 | cursor_visible u8 |
//   display_offset u32 | history_len u32 |
//   selection: present u8, start_row i32, start_col u16, end_row i32, end_col u16 |
//   row_count u16 | rows: [row u16 | col_start u16 | cell_count u16 |
//   cells: cell_count × 16B (ch u32 | fg u32 | bg u32 | flags u16 | link_id u16)] |
//   zerowidth_count u16 | [row u16 | col u16 | n u8 | n × ch u32] |
//   match_count u16 | [start_row i32 | start_col u16 | end_row i32 | end_col u16]
//
// Colors are packed r<<24 | g<<16 | b<<8 | a so the LE bytes read back as
// RRGGBBAA. Cell `ch` is a Unicode scalar value.

export const FRAME_MAGIC = 0xd7;
export const FRAME_VERSION = 1;
/** Bytes per wire cell: ch u32 | fg u32 | bg u32 | flags u16 | link u16. */
export const CELL_BYTES = 16;
/** Bytes in the fixed header, through row_count (see layout above). */
export const FRAME_HEADER_LEN = 37;

/**
 * CellFlags bit assignments — MIRROR of `term-core/src/actor.rs`
 * (`actor::CellFlags`) exported onto the wire by
 * `term-core/src/frame.rs`. Underline kinds ride bits 3..=7. Keep in lockstep
 * with both Rust files.
 */
export const CELL_FLAGS = {
  bold: 1 << 0,
  italic: 1 << 1,
  dim: 1 << 2,
  underline: 1 << 3,
  doubleUnderline: 1 << 4,
  undercurl: 1 << 5,
  dottedUnderline: 1 << 6,
  dashedUnderline: 1 << 7,
  inverse: 1 << 8,
  strikeout: 1 << 9,
  hidden: 1 << 10,
  wide: 1 << 11,
  wideSpacer: 1 << 12,
} as const;

/** Cursor shape codes on the wire (alacritty's CursorShape variant order). */
export const CURSOR_SHAPES = ["block", "underline", "beam", "hollow", "hidden"] as const;
export type CursorShapeName = (typeof CURSOR_SHAPES)[number];

/** Viewport grid dimensions the cell store is sized to. */
export interface GridDims {
  cols: number;
  rows: number;
}

export interface FrameCursor {
  row: number;
  col: number;
  shape: CursorShapeName;
  visible: boolean;
}

/** A (start,end) range in viewport coordinates: selection or search match. */
export interface Range {
  startRow: number;
  startCol: number;
  endRow: number;
  endCol: number;
}

/** One dirtied row span, (row, colStart, cellCount) — what the renderer paints. */
export interface RowSpan {
  row: number;
  colStart: number;
  cellCount: number;
}

/** Zero-width combiners attached to a cell; chars are codepoints. */
export interface ZerowidthEntry {
  row: number;
  col: number;
  chars: Uint32Array;
}

/**
 * Flat cell storage written in place by (row, col): index `row * cols + col`.
 * No per-cell objects are allocated during a decode — the renderer keeps one
 * `CellStore` across frames (passing its dims in) and every decode writes
 * only the covered cells, leaving untouched cells intact for deltas.
 */
export interface CellStore {
  cols: number;
  rows: number;
  ch: Uint32Array;
  fg: Uint32Array;
  bg: Uint32Array;
  flags: Uint16Array;
  link: Uint16Array;
}

export interface Frame {
  seq: number;
  kind: "full" | "delta";
  cursor: FrameCursor;
  displayOffset: number;
  historyLen: number;
  /** Viewport-clipped selection for the overlay; null when nothing is
   *  selected OR the selection is scrolled fully out of the viewport. */
  selection: Range | null;
  /** A non-empty selection exists somewhere (even in scrollback) — wire
   *  flags bit 0. Gate copy shortcuts on THIS, not on `selection`. */
  selectionActive: boolean;
  /** Any mouse tracking protocol is active (xterm 1000/1002/1003) — wire
   *  flags bit 1. When set, local drag-selection and the wheel→scroll
   *  duplicate are suppressed unless Shift is held. */
  mouseCapture: boolean;
  /** The alternate screen is active (xterm 1049) — wire flags bit 2. */
  altScreen: boolean;
  matches: Range[];
  rows: RowSpan[];
  zerowidth: ZerowidthEntry[];
  cells: CellStore;
}

/**
 * Decode one binary frame.
 *
 * `store` optionally pins both the cell-store size AND the storage: when given,
 * the frame's cells are written IN PLACE into that store (delta application
 * onto retained arrays — cells the frame does not cover keep their previous
 * content). The renderer/panel keeps one store across frames and passes it in;
 * only a full frame with different dims (or a fresh panel) allocates a new one.
 *
 * When `store` is omitted the store is sized to the frame's own bounds, which
 * is correct for full frames and fixtures.
 *
 * Malformed input — wrong magic/version, truncated buffer, bad kind/shape, a
 * row span that overflows a pinned store — throws; the caller reacts by
 * requesting a FULL frame.
 */
export function decodeFrame(buf: ArrayBuffer, store?: CellStore): Frame {
  const view = new DataView(buf);
  const bytes = buf.byteLength;
  let pos = 0;

  const need = (n: number): void => {
    if (pos + n > bytes) {
      throw new Error(`decodeFrame: truncated at ${pos}, need ${n}, have ${bytes - pos}`);
    }
  };
  const u8 = (): number => {
    need(1);
    const v = view.getUint8(pos);
    pos += 1;
    return v;
  };
  const u16 = (): number => {
    need(2);
    const v = view.getUint16(pos, true);
    pos += 2;
    return v;
  };
  const u32 = (): number => {
    need(4);
    const v = view.getUint32(pos, true);
    pos += 4;
    return v;
  };
  const i32 = (): number => u32() | 0;
  const range = (): Range => {
    const startRow = i32();
    const startCol = u16();
    const endRow = i32();
    const endCol = u16();
    return { startRow, startCol, endRow, endCol };
  };

  // Validate the header bytes up front so a short buffer reports a clean
  // "truncated header" and a wrong magic/version reports what it is.
  if (bytes < 2) throw new Error("decodeFrame: truncated header (need magic+version)");
  if (view.getUint8(0) !== FRAME_MAGIC) throw new Error("decodeFrame: bad magic");
  if (view.getUint8(1) !== FRAME_VERSION) throw new Error("decodeFrame: bad version");
  pos = 2;

  const seq = u32();
  const kindCode = u8();
  if (kindCode > 1) throw new Error(`decodeFrame: bad kind ${kindCode}`);
  const kind = kindCode === 0 ? "full" : "delta";
  // flags: bit 0 = selection_active, bit 1 = mouse_capture, bit 2 =
  // alt_screen; bits 3..=7 reserved (a layout change bumps the version byte,
  // so unknown bits are malformed, not future).
  const flags = u8();
  if ((flags & ~0b111) !== 0) throw new Error(`decodeFrame: bad flags ${flags}`);
  const selectionActive = (flags & 1) !== 0;
  const mouseCapture = (flags & 2) !== 0;
  const altScreen = (flags & 4) !== 0;

  const cursor: FrameCursor = {
    row: u16(),
    col: u16(),
    shape: (() => {
      const code = u8();
      if (code >= CURSOR_SHAPES.length) throw new Error(`decodeFrame: bad cursor shape ${code}`);
      return CURSOR_SHAPES[code];
    })(),
    visible: u8() !== 0,
  };
  const displayOffset = u32();
  const historyLen = u32();

  // Selection slot is always 13 bytes; present=0 carries zeroed padding.
  const present = u8();
  if (present > 1) throw new Error(`decodeFrame: bad selection present ${present}`);
  const sel = range();
  const selection = present === 1 ? sel : null;

  // Compute the cell-store grid before allocating: a pinned store sizes the
  // grid (and IS the storage — see the doc comment). Without one, do a cheap
  // header-only pass over the rows section to find its extents, then rewind
  // and walk it for real below.
  let cols: number;
  let rows: number;
  let out: CellStore;
  if (store) {
    cols = store.cols;
    rows = store.rows;
    out = store;
  } else {
    let maxRow = 0;
    let maxCol = 0;
    const firstSpan = pos;
    need(2);
    const rowCount = u16();
    for (let i = 0; i < rowCount; i++) {
      need(6);
      const row = view.getUint16(pos, true);
      const colStart = view.getUint16(pos + 2, true);
      const cellCount = view.getUint16(pos + 4, true);
      pos += 6;
      if (row + 1 > maxRow) maxRow = row + 1;
      if (colStart + cellCount > maxCol) maxCol = colStart + cellCount;
      need(cellCount * CELL_BYTES);
      pos += cellCount * CELL_BYTES;
    }
    pos = firstSpan;
    cols = Math.max(maxCol, 1);
    rows = Math.max(maxRow, 1);
    const cellCount = rows * cols;
    out = {
      cols,
      rows,
      ch: new Uint32Array(cellCount),
      fg: new Uint32Array(cellCount),
      bg: new Uint32Array(cellCount),
      flags: new Uint16Array(cellCount),
      link: new Uint16Array(cellCount),
    };
  }
  const cellCount = cols * rows;

  const rowSpans: RowSpan[] = [];
  const rowCount = u16();
  for (let i = 0; i < rowCount; i++) {
    const row = u16();
    const colStart = u16();
    const cellCountThis = u16();
    const base = row * cols + colStart;
    for (let j = 0; j < cellCountThis; j++) {
      const idx = base + j;
      if (idx >= cellCount) throw new Error("decodeFrame: cell index out of grid");
      out.ch[idx] = u32();
      out.fg[idx] = u32();
      out.bg[idx] = u32();
      out.flags[idx] = u16();
      out.link[idx] = u16();
    }
    rowSpans.push({ row, colStart, cellCount: cellCountThis });
  }

  const zerowidth: ZerowidthEntry[] = [];
  const zwCount = u16();
  for (let i = 0; i < zwCount; i++) {
    const row = u16();
    const col = u16();
    const n = u8();
    const chars = new Uint32Array(n);
    for (let j = 0; j < n; j++) chars[j] = u32();
    zerowidth.push({ row, col, chars });
  }

  const matches: Range[] = [];
  const matchCount = u16();
  for (let i = 0; i < matchCount; i++) matches.push(range());

  return {
    seq,
    kind,
    cursor,
    displayOffset,
    historyLen,
    selection,
    selectionActive,
    mouseCapture,
    altScreen,
    matches,
    rows: rowSpans,
    zerowidth,
    cells: out,
  };
}
