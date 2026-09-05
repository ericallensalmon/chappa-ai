// WebGL2 renderer.
//
// The renderer is split so the container can test everything that isn't GPU:
//
//  - PURE instance-list builders below (writeGlyphCells, writeBgCells,
//    writeDecorCells, buildBgFrameRects, buildCursorRects, blockCursorGlyph).
//    They take the retained CellStore + the frame's dirty spans and write
//    instanced-attribute data into caller-owned persistent Float32Arrays at
//    per-cell slots. Dirty rows update in place; untouched rows' regions stay
//    byte-identical, which the tests pin. No gl.* call anywhere in them.
//  - The WebGLRenderer class isolates every gl.* call. jsdom has no WebGL2, so
//    constructing it throws cleanly (the panel catches and falls back to the
//    DOM oracle); it is exercised host-side.
//
// Drawing is three passes over the retained store the panel passes to apply():
//   1. bg pass — per-cell backgrounds (cells whose bg equals the terminal
//      default become degenerate quads and draw nothing, letting the canvas's
//      transparent pixels show the container background), plus the frame's
//      selection/search rects and the focused block-cursor fill, drawn after
//      the cells with blending.
//   2. glyph pass — instanced quads sampling a TEXTURE_2D_ARRAY atlas (one
//      draw call across all pages = layers). White glyphs are tinted by their
//      fg in the fragment shader; color glyphs (emoji) skip the tint; dim
//      cells have the fg scaled 0.66× builder-side.
//   3. overlay pass — per-cell decorations (underlines/undercurl/strikeout,
//      procedural in the fragment shader) plus non-block cursor shapes, plus
//      the focused block cursor's inverse-glyph redraw (filled rect in the bg
//      pass + this glyph re-drawn tinted with the cell's bg color on top).
// The overlay pass is two instanced draws plus a single-glyph redraw, so the
// conceptual three passes run as up to five draws.
//
// Buffers are sized to the grid and never reallocated in the frame loop. Each
// pass uploads only its dirty rows via bufferSubData (in place, so untouched
// rows are never re-sent). Atlas misses are drained after the glyph list is
// built (rasterize → dirty texSubImage3D into the page layer); an eviction
// invalidates already-written slots, so the glyph list is rebuilt once
// (guarded). Context loss recreates the whole pipeline and asks the panel for
// a FULL frame via onRequestFull.
//
// An IME preview box border is deliberately not drawn here:
// IME composition stays a DOM overlay (the panel's `compose` div) in both
// renderers, so GL does not need to know about it.

import { CELL_FLAGS, type CellStore, type Frame, type Range, type RowSpan } from "./protocol";
import type { RendererMetrics, TermRenderer } from "./renderer";
import {
  ATLAS_DEFAULT_MAX_PAGES,
  ATLAS_DEFAULT_PAGE_SIZE,
  ATLAS_SLOT_PAD,
  AtlasIndex,
  cssFontFamily,
  GlyphRaster,
  isColorGlyph,
  packKey,
  type AtlasSlot,
} from "./atlas";
import { measureCellMetrics } from "./metrics";

/** Snap a CSS-px cell dimension so cell×dpr is an integer device-px count.
 *  Fractional device cells put every glyph quad off the pixel grid, and the
 *  atlas's LINEAR sampling then smears each glyph in both axes — text
 *  rendered noticeably fatter/fuzzier than the DOM renderer's native spans
 *  (host-run). All quad positions are col×(cellW×dpr), so one
 *  integral cell makes the whole grid pixel-exact. */
function quantizeCell(cssPx: number, dpr: number): number {
  return Math.max(1, Math.round(cssPx * dpr)) / dpr;
}

// --- shader sources --------------------------------------------------------
// Snapshot-tested (container) and compiled host-side. GLSL ES 3.00 (WebGL2).
// The fragment shaders output premultiplied colors and the context blends
// ONE, ONE_MINUS_SRC_ALPHA against the premultipliedAlpha:true backing store.

/** The rect/color pass (bg cells, selection, matches, cursor, decorations):
 *  one instance per rect; the unit-quad corners come from gl_VertexID. */
export const COLOR_VERT_SRC = `#version 300 es
// Color pass vertex: instanced rects in device px -> clip space.
layout(location = 0) in vec4 a_rect;    // x, y, w, h (device px)
layout(location = 1) in vec4 a_color;   // straight rgba 0..1
layout(location = 2) in float a_kind;   // RECT_KIND_* (plain/undercurl/...)
uniform vec2 u_viewport;                // device px canvas size
out vec4 v_color;
out vec4 v_rect;
flat out float f_kind;
void main() {
  vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
  vec2 pos = a_rect.xy + corner * a_rect.zw;
  vec2 clip = pos / u_viewport * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  v_color = a_color;
  v_rect = a_rect;
  f_kind = a_kind;
}
`;

/** Draws the rect content; underline/strikeout strokes are procedural. */
export const COLOR_FRAG_SRC = `#version 300 es
precision mediump float;
uniform float u_dpr;                    // scales decoration sizes from CSS px
// highp: must match the vertex stage's default precision or the link fails.
uniform highp vec2 u_viewport;          // device px canvas size (y-flip)
in vec4 v_color;
in vec4 v_rect;                         // device px, top-left origin
flat in float f_kind;
out vec4 fragColor;
void main() {
  // v_rect is x,y,w,h — in swizzle terms width is .z and height is .w.
  // Name them: 'v_rect.h' does not compile, and 'v_rect.w' compiles to the
  // HEIGHT, which is exactly the trap.
  float rw = v_rect.z;
  float rh = v_rect.w;
  // Local coordinates in device px, ly measured DOWN from the rect top —
  // gl_FragCoord is bottom-origin, so flip through the viewport height.
  float lx = gl_FragCoord.x - v_rect.x;
  float ly = (u_viewport.y - gl_FragCoord.y) - v_rect.y;
  float dpr = max(u_dpr, 1.0);
  float a = 1.0;
  if (f_kind > 0.5 && f_kind < 1.5) {
    // undercurl: 3px-amplitude sine, ~8px wavelength, 1px stroke.
    float y = rh - 2.0 * dpr - 3.0 * dpr * sin(lx * 0.785398163 / dpr);
    a *= step(abs(ly - y), 0.75 * dpr);
  } else if (f_kind > 1.5 && f_kind < 2.5) {
    // dashed underline: 4px on / 2px off.
    a *= step(0.6666667, fract(lx / (6.0 * dpr)));
  } else if (f_kind > 2.5 && f_kind < 3.5) {
    // dotted underline: 2px on / 2px off.
    a *= step(0.5, fract(lx / (4.0 * dpr)));
  } else if (f_kind > 3.5 && f_kind < 4.5) {
    // double underline: two 1px lines, 1px and 3.5px above the bottom.
    float y1 = rh - 1.0 * dpr;
    float y2 = rh - 3.5 * dpr;
    a *= max(step(abs(ly - y1), 0.5 * dpr), step(abs(ly - y2), 0.5 * dpr));
  } else if (f_kind > 4.5 && f_kind < 5.5) {
    // hollow cursor: 1px border ring.
    float left = step(lx, 1.0 * dpr);
    float right = step(rw - 1.0 * dpr, lx);
    float top = step(ly, 1.0 * dpr);
    float bottom = step(rh - 1.0 * dpr, ly);
    a *= max(max(left, right), max(top, bottom));
  } else if (f_kind > 5.5 && f_kind < 6.5) {
    // underline + strikeout: a bottom line and a line through the middle.
    float yb = rh - 1.0 * dpr;
    float ym = rh * 0.5;
    a *= max(step(abs(ly - yb), 0.5 * dpr), step(abs(ly - ym), 0.5 * dpr));
  }
  if (a <= 0.0) discard;
  // Premultiplied output for ONE, ONE_MINUS_SRC_ALPHA blending.
  fragColor = vec4(v_color.rgb * v_color.a, v_color.a) * a;
}
`;

/** Glyph pass vertex: instanced quads sampling the TEXTURE_2D_ARRAY atlas. */
export const GLYPH_VERT_SRC = `#version 300 es
layout(location = 0) in vec4 a_rect;    // x, y, w, h (device px)
layout(location = 1) in vec4 a_uv;      // u, v, uw, vh (fractions of a page)
layout(location = 2) in float a_page;   // texture array layer
layout(location = 3) in vec4 a_fg;      // straight rgba tint (dim applied)
layout(location = 4) in float a_flags;  // bit 0: colored glyph (skip tint)
uniform vec2 u_viewport;
out vec2 v_uv;
flat out float f_page;
flat out vec4 f_fg;
flat out float f_flags;
void main() {
  vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
  vec2 pos = a_rect.xy + corner * a_rect.zw;
  vec2 clip = pos / u_viewport * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  v_uv = a_uv.xy + corner * a_uv.zw;
  f_page = a_page;
  f_fg = a_fg;
  f_flags = a_flags;
}
`;

/** Samples the atlas. White glyphs are tinted by fg; emoji pass through. */
export const GLYPH_FRAG_SRC = `#version 300 es
precision mediump float;
// sampler2DArray has NO default precision in GLSL ES 3.00 — it must be
// qualified explicitly or the shader does not compile.
uniform mediump sampler2DArray u_atlas;
in vec2 v_uv;
flat in float f_page;
flat in vec4 f_fg;
flat in float f_flags;
out vec4 fragColor;
void main() {
  vec4 texel = texture(u_atlas, vec3(v_uv, f_page));
  float a = texel.a;
  if (a < 0.02) discard;
  if (f_flags > 0.5) {
    // Colored glyph (emoji): the atlas page carries its own colors, already
    // premultiplied by the rasterizer's canvas backing store.
    fragColor = texel;
  } else {
    // Grayscale-AA coverage composited with naive sRGB alpha blending makes
    // light-on-dark text bloom about a weight step bolder than the same
    // font in the DOM (whose text stack gamma-corrects). Sharpen the
    // coverage curve to compensate — the ghostty/kitty-style correction
    // (host-run: GL text read a weight step bolder than the same
    // font rendered by the DOM path).
    a = pow(a, 1.45);
    // The page holds premultiplied white (r=g=b=a), so premultiplying the
    // straight fg tint by the corrected coverage gives the tinted glyph.
    fragColor = vec4(f_fg.rgb * a, a);
  }
}
`;

// --- instance layout -------------------------------------------------------

/** Floats per color-pass instance: x y w h r g b a kind. */
export const RECT_FLOATS = 9;
/** Floats per glyph instance: x y w h u v uw vh page r g b a flags. */
export const GLYPH_FLOATS = 14;

/** Color-pass instance kinds — RECT_KIND_* drive the fragment shader. */
export const RECT_KIND_PLAIN = 0;
export const RECT_KIND_UNDERCURL = 1;
export const RECT_KIND_DASHED = 2;
export const RECT_KIND_DOTTED = 3;
export const RECT_KIND_DOUBLE = 4;
export const RECT_KIND_HOLLOW = 5;
export const RECT_KIND_STRIKE_UNDER = 6;

/** Glyph instance flags bit for "colored glyph, skip fg tint". */
export const GLYPH_COLORED = 1;

/** Dim cells render at this fraction of their fg. */
export const DIM_FACTOR = 0.66;

/** Overlay colors matching the DOM renderer's CSS (renderer_dom.ts). */
export const SELECTION_COLOR = 0x5ea6ff4d;
export const MATCH_COLOR = 0xffbf0047;
export const CURSOR_COLOR = 0xffffffff;

export interface GpuMetrics {
  /** CSS px. */
  cellW: number;
  /** CSS px. */
  cellH: number;
  dpr: number;
}

/** A glyph that was missing from the atlas while the glyph list was built.
 *  (row, col) lets the renderer patch the cell's slot once rasterized. */
export interface GlyphMiss {
  key: number;
  codepoint: number;
  wide: boolean;
  bold: boolean;
  italic: boolean;
  row: number;
  col: number;
}

function writeRectFloats(
  out: Float32Array,
  off: number,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
  g: number,
  b: number,
  a: number,
  kind: number,
): void {
  out[off] = x;
  out[off + 1] = y;
  out[off + 2] = w;
  out[off + 3] = h;
  out[off + 4] = r;
  out[off + 5] = g;
  out[off + 6] = b;
  out[off + 7] = a;
  out[off + 8] = kind;
}

function writeRect(
  out: Float32Array,
  off: number,
  x: number,
  y: number,
  w: number,
  h: number,
  packed: number,
  kind: number,
): void {
  writeRectFloats(
    out,
    off,
    x,
    y,
    w,
    h,
    ((packed >>> 24) & 0xff) / 255,
    ((packed >>> 16) & 0xff) / 255,
    ((packed >>> 8) & 0xff) / 255,
    (packed & 0xff) / 255,
    kind,
  );
}

/** A degenerate color-pass instance — zero area, rasterizes nothing. Used to
 *  remove a cell's instance (e.g. bg turned default) without resizing the
 *  persistent buffer. */
export function writeDegenerateRect(out: Float32Array, off: number): void {
  out[off + 2] = 0;
  out[off + 3] = 0;
}

/** Packed RRGGBBAA → 0..1 floats, dim scaled down when the flag is set. */
function fgFloats(packed: number, flags: number): [number, number, number, number] {
  let r = ((packed >>> 24) & 0xff) / 255;
  let g = ((packed >>> 16) & 0xff) / 255;
  let b = ((packed >>> 8) & 0xff) / 255;
  const a = (packed & 0xff) / 255;
  if (flags & CELL_FLAGS.dim) {
    r *= DIM_FACTOR;
    g *= DIM_FACTOR;
    b *= DIM_FACTOR;
  }
  return [r, g, b, a];
}

function writeGlyph(
  out: Float32Array,
  off: number,
  x: number,
  y: number,
  w: number,
  h: number,
  slot: AtlasSlot,
  pageSize: number,
  tint: [number, number, number, number],
  colored: number,
): void {
  out[off] = x;
  out[off + 1] = y;
  out[off + 2] = w;
  out[off + 3] = h;
  // UVs cover the slot's GLYPH region only — the 1px pad exists to stop
  // LINEAR-filter bleed between slots, not to be sampled. Mapping the padded
  // slot onto a cell-sized quad scaled every glyph by (cell+2)/cell and
  // shifted it half a pad — one more contributor to the fat/fuzzy GL text
  // (host-run). With quantized cells the glyph region equals the
  // quad exactly: 1:1 texels.
  out[off + 4] = (slot.x + ATLAS_SLOT_PAD) / pageSize;
  out[off + 5] = (slot.y + ATLAS_SLOT_PAD) / pageSize;
  out[off + 6] = (slot.w - 2 * ATLAS_SLOT_PAD) / pageSize;
  out[off + 7] = (slot.h - 2 * ATLAS_SLOT_PAD) / pageSize;
  out[off + 8] = slot.page;
  out[off + 9] = tint[0];
  out[off + 10] = tint[1];
  out[off + 11] = tint[2];
  out[off + 12] = tint[3];
  out[off + 13] = colored;
}

/** Write one cell's glyph instance into `out` at float offset `off`. A cell
 *  with no glyph (empty or wide spacer) or an atlas miss becomes a degenerate
 *  zero-area quad. `tint` overrides the fg (used by the cursor's inverse
 *  redraw); `misses` collects atlas misses unless null. */
function writeGlyphCell(
  out: Float32Array,
  off: number,
  cells: CellStore,
  idx: number,
  row: number,
  col: number,
  atlas: { lookup(key: number): AtlasSlot | null },
  pageSize: number,
  m: GpuMetrics,
  misses: GlyphMiss[] | null,
  tint?: [number, number, number, number],
): void {
  const flags = cells.flags[idx];
  const ch = cells.ch[idx];
  if ((flags & CELL_FLAGS.wideSpacer) !== 0 || ch === 0) {
    writeDegenerateRect(out, off);
    return;
  }
  const bold = (flags & CELL_FLAGS.bold) !== 0;
  const italic = (flags & CELL_FLAGS.italic) !== 0;
  const wide = (flags & CELL_FLAGS.wide) !== 0;
  const key = packKey(ch, bold, italic);
  const slot = atlas.lookup(key);
  if (!slot) {
    if (misses) {
      misses.push({ key, codepoint: ch, wide, bold, italic, row, col });
    }
    writeDegenerateRect(out, off);
    return;
  }
  const cellW = m.cellW * m.dpr;
  const cellH = m.cellH * m.dpr;
  writeGlyph(
    out,
    off,
    col * cellW,
    row * cellH,
    wide ? 2 * cellW : cellW,
    cellH,
    slot,
    pageSize,
    tint ?? fgFloats(cells.fg[idx], flags),
    isColorGlyph(ch) ? GLYPH_COLORED : 0,
  );
}

/**
 * Write glyph instances for the given dirty spans, one per cell slot. Cells
 * the frame does not touch keep whatever their slot held (untouched rows'
 * regions are byte-identical). Misses are collected into `misses` for the
 * renderer to rasterize and patch afterwards.
 */
export function writeGlyphCells(
  out: Float32Array,
  cells: CellStore,
  spans: RowSpan[],
  atlas: { lookup(key: number): AtlasSlot | null },
  pageSize: number,
  m: GpuMetrics,
  misses: GlyphMiss[],
): void {
  const { cols } = cells;
  for (const span of spans) {
    const base = span.row * cols + span.colStart;
    for (let j = 0; j < span.cellCount; j++) {
      const idx = base + j;
      writeGlyphCell(
        out,
        idx * GLYPH_FLOATS,
        cells,
        idx,
        span.row,
        span.colStart + j,
        atlas,
        pageSize,
        m,
        misses,
      );
    }
  }
}

/** Write bg-pass instances for the dirty spans: one rect per cell whose bg
 *  differs from the terminal default; everything else becomes a degenerate
 *  zero-area quad. Per-cell, merging nothing: a wide char's
 *  spacer cell carries its own bg and is drawn independently, which matches
 *  the DOM renderer's per-cell spans. */
export function writeBgCells(
  out: Float32Array,
  cells: CellStore,
  spans: RowSpan[],
  defaultBg: number,
  m: GpuMetrics,
): void {
  const cellW = m.cellW * m.dpr;
  const cellH = m.cellH * m.dpr;
  const { cols } = cells;
  for (const span of spans) {
    const base = span.row * cols + span.colStart;
    const y = span.row * cellH;
    for (let j = 0; j < span.cellCount; j++) {
      const idx = base + j;
      const off = idx * RECT_FLOATS;
      const bg = cells.bg[idx];
      if (bg === defaultBg) {
        writeDegenerateRect(out, off);
        continue;
      }
      writeRect(out, off, (span.colStart + j) * cellW, y, cellW, cellH, bg, RECT_KIND_PLAIN);
    }
  }
}

/** Map a cell's decoration flags onto a shader kind, or -1 for none. Kinds
 *  that draw multiple lines (undercurl, double, strike+underline) use a
 *  full-height rect and let the fragment shader place the strokes; plain
 *  underline/strikeout are thin rects positioned by the builder. A cell with
 *  double-underline AND strikeout renders the double pair (the shader's
 *  strike+underline kind is single-line); the combo is rare enough to lose. */
function decorKind(flags: number): number {
  if ((flags & CELL_FLAGS.doubleUnderline) !== 0) return RECT_KIND_DOUBLE;
  if ((flags & CELL_FLAGS.undercurl) !== 0) return RECT_KIND_UNDERCURL;
  if ((flags & CELL_FLAGS.dottedUnderline) !== 0) return RECT_KIND_DOTTED;
  if ((flags & CELL_FLAGS.dashedUnderline) !== 0) return RECT_KIND_DASHED;
  if ((flags & CELL_FLAGS.underline) !== 0 && (flags & CELL_FLAGS.strikeout) !== 0) {
    return RECT_KIND_STRIKE_UNDER;
  }
  if ((flags & CELL_FLAGS.underline) !== 0 || (flags & CELL_FLAGS.strikeout) !== 0) {
    return RECT_KIND_PLAIN;
  }
  return -1;
}

/** Write overlay decorations (underlines/undercurl/strikeout) for the dirty
 *  spans, one instance per decorated cell. Colors come from the cell's fg,
 *  dimmed like the glyph. */
export function writeDecorCells(
  out: Float32Array,
  cells: CellStore,
  spans: RowSpan[],
  m: GpuMetrics,
): void {
  const cellW = m.cellW * m.dpr;
  const cellH = m.cellH * m.dpr;
  const thin = Math.max(2, Math.round(m.cellH * 0.12)) * m.dpr;
  const { cols } = cells;
  for (const span of spans) {
    const base = span.row * cols + span.colStart;
    const y = span.row * cellH;
    for (let j = 0; j < span.cellCount; j++) {
      const idx = base + j;
      const off = idx * RECT_FLOATS;
      const flags = cells.flags[idx];
      const kind = decorKind(flags);
      if (kind < 0) {
        writeDegenerateRect(out, off);
        continue;
      }
      const wide = (flags & CELL_FLAGS.wide) !== 0;
      const x = (span.colStart + j) * cellW;
      const w = wide ? 2 * cellW : cellW;
      const [r, g, b, a] = fgFloats(cells.fg[idx], flags);
      if (kind === RECT_KIND_PLAIN) {
        // Position the thin stroke: mid line for strikeout, bottom for
        // underline.
        const isStrike = (flags & CELL_FLAGS.underline) === 0;
        const yy = isStrike ? y + (cellH - thin) / 2 : y + cellH - thin;
        writeRectFloats(out, off, x, yy, w, thin, r, g, b, a, RECT_KIND_PLAIN);
      } else {
        // Full-height rect; the fragment shader places the strokes.
        writeRectFloats(out, off, x, y, w, cellH, r, g, b, a, kind);
      }
    }
  }
}

/** A selection/match range expanded into per-row rects (exclusive end col),
 *  viewport-clipped — the same geometry the DOM overlay draws. */
function rangeRects(range: Range, cols: number, rows: number): { row: number; startCol: number; endCol: number }[] {
  const rects: { row: number; startCol: number; endCol: number }[] = [];
  if (rows === 0) return rects;
  const startRow = Math.max(0, Math.min(range.startRow, rows - 1));
  const endRow = Math.max(0, Math.min(range.endRow, rows - 1));
  for (let r = startRow; r <= endRow; r++) {
    const startCol = r === startRow ? Math.min(range.startCol, cols) : 0;
    const endCol = r === endRow ? Math.min(range.endCol + 1, cols) : cols;
    if (endCol <= startCol) continue;
    rects.push({ row: r, startCol, endCol });
  }
  return rects;
}

/** How many bg-pass frame rects a frame will emit — lets the renderer grow
 *  the variable region before writing (selection + matches can exceed the
 *  default capacity when a frame carries many search matches). */
export function countFrameRects(frame: Frame, cols: number, rows: number, focused: boolean): number {
  let n = 0;
  if (frame.selection) n += rangeRects(frame.selection, cols, rows).length;
  for (const match of frame.matches) n += rangeRects(match, cols, rows).length;
  if (focused && frame.cursor.visible && frame.cursor.shape === "block") n += 1;
  return n;
}

/**
 * Write the bg pass's per-frame rects (selection, search matches, focused
 * block-cursor fill) into `out` starting at float offset `off`; returns how
 * many instances were written. These live after the per-cell region of the bg
 * buffer, so they draw on top of the cell backgrounds. The cursor block fill
 * is a solid rect under the glyph pass — the inverse glyph itself is redrawn
 * in the overlay pass.
 */
export function buildBgFrameRects(
  out: Float32Array,
  frame: Frame,
  cols: number,
  rows: number,
  off: number,
  m: GpuMetrics,
  cursorColor: number,
  focused: boolean,
): number {
  const cellW = m.cellW * m.dpr;
  const cellH = m.cellH * m.dpr;
  let n = 0;
  const push = (x: number, y: number, w: number, h: number, packed: number, kind: number): void => {
    writeRect(out, off + n * RECT_FLOATS, x, y, w, h, packed, kind);
    n++;
  };
  if (frame.selection) {
    for (const r of rangeRects(frame.selection, cols, rows)) {
      push(r.startCol * cellW, r.row * cellH, (r.endCol - r.startCol) * cellW, cellH, SELECTION_COLOR, RECT_KIND_PLAIN);
    }
  }
  for (const match of frame.matches) {
    for (const r of rangeRects(match, cols, rows)) {
      push(r.startCol * cellW, r.row * cellH, (r.endCol - r.startCol) * cellW, cellH, MATCH_COLOR, RECT_KIND_PLAIN);
    }
  }
  const { cursor } = frame;
  if (focused && cursor.visible && cursor.shape === "block") {
    push(cursor.col * cellW, cursor.row * cellH, cellW, cellH, cursorColor, RECT_KIND_PLAIN);
  }
  return n;
}

/**
 * Write the overlay pass's cursor rects (beam/underline bars, hollow ring;
 * the focused block is handled by the bg-pass fill + glyph redraw). Returns
 * the instance count. Hidden cursors emit nothing.
 */
export function buildCursorRects(
  out: Float32Array,
  frame: Frame,
  m: GpuMetrics,
  focused: boolean,
  cursorColor: number,
): number {
  const { cursor } = frame;
  if (!cursor.visible || cursor.shape === "hidden") return 0;
  const cellW = m.cellW * m.dpr;
  const cellH = m.cellH * m.dpr;
  const x = cursor.col * cellW;
  const y = cursor.row * cellH;
  const thin = Math.max(2, Math.round(m.cellH * 0.12)) * m.dpr;
  let n = 0;
  const push = (xx: number, yy: number, w: number, h: number, kind: number): void => {
    writeRect(out, n * RECT_FLOATS, xx, yy, w, h, cursorColor, kind);
    n++;
  };
  switch (cursor.shape) {
    case "beam":
      push(x, y, Math.max(2, 2 * m.dpr), cellH, RECT_KIND_PLAIN);
      break;
    case "underline":
      push(x, y + cellH - thin, cellW, thin, RECT_KIND_PLAIN);
      break;
    case "hollow":
      push(x, y, cellW, cellH, RECT_KIND_HOLLOW);
      break;
    case "block":
      // Unfocused block cursor: a hollow ring over the (still normal) glyph.
      if (!focused) push(x, y, cellW, cellH, RECT_KIND_HOLLOW);
      break;
  }
  return n;
}

/** The cell the overlay must re-draw in inverse (bg) color: only a focused,
 *  visible block cursor. Null otherwise — the normal glyph pass is enough. */
export function blockCursorGlyph(frame: Frame, focused: boolean): { row: number; col: number } | null {
  const { cursor } = frame;
  if (!focused || !cursor.visible || cursor.shape !== "block") return null;
  return { row: cursor.row, col: cursor.col };
}

/** One span per row — the "everything dirty" set used for full frames,
 *  context-loss/metrics resets, and eviction rebuilds. */
export function allRowSpans(cols: number, rows: number): RowSpan[] {
  const spans = new Array<RowSpan>(rows);
  for (let r = 0; r < rows; r++) spans[r] = { row: r, colStart: 0, cellCount: cols };
  return spans;
}

/** Distinct dirty row indices from the frame's spans, in span order. */
function dirtyRows(spans: RowSpan[]): number[] {
  const rows: number[] = [];
  let last = -1;
  for (const s of spans) {
    if (s.row !== last) {
      rows.push(s.row);
      last = s.row;
    }
  }
  return rows;
}

// --- the GL pipeline -------------------------------------------------------

export interface WebGLRendererOptions {
  /** The terminal viewport the canvas fills (position:absolute host). */
  container: HTMLElement;
  /** CSS px; the panel's initial probe (may be 0 before the first measure). */
  cellW: number;
  cellH: number;
  dpr: number;
  /** CSS px from the cell top to the alphabetic baseline. */
  baseline: number;
  fontFamily: string;
  fontSize: number;
  /** Called when a FULL frame is wanted (context loss, metrics change). The
   *  panel routes it to `request_full(id)` — the only way out of the renderer. */
  onRequestFull?: () => void;
  /** Cursor fill color (packed RGBA); defaults to white. */
  cursorColor?: number;
  /** Packed RGBA of the terminal default background (0xRRGGBBAA); cells
   *  whose bg equals this skip the bg pass so the container shows through. */
  defaultBg?: number;
}

/**
 * The WebGL2 pipeline. Constructing throws when WebGL2 is unavailable (jsdom
 * has none); the panel catches and falls back to the DOM oracle. All gl.*
 * calls are contained here and exercised host-side.
 */
export class WebGLRenderer implements TermRenderer {
  private readonly canvas: HTMLCanvasElement;
  private readonly onRequestFull: () => void;
  private readonly defaultBg: number;
  private readonly cursorColor: number;
  private readonly pageSize: number;
  private readonly maxPages: number;

  private gl: WebGL2RenderingContext | null = null;
  private colorProg: WebGLProgram | null = null;
  private glyphProg: WebGLProgram | null = null;
  private atlasTex: WebGLTexture | null = null;
  private vaoBg: WebGLVertexArrayObject | null = null;
  private vaoGlyph: WebGLVertexArrayObject | null = null;
  private vaoDecor: WebGLVertexArrayObject | null = null;
  private vaoCursor: WebGLVertexArrayObject | null = null;
  private vaoCursorGlyph: WebGLVertexArrayObject | null = null;
  private bgGpu: WebGLBuffer | null = null;
  private glyphGpu: WebGLBuffer | null = null;
  private decorGpu: WebGLBuffer | null = null;
  private cursorGpu: WebGLBuffer | null = null;
  private cursorGlyphGpu: WebGLBuffer | null = null;

  private cellW: number;
  private cellH: number;
  private dpr: number;
  private baseline: number;
  private fontFamily: string;
  private fontSize: number;
  private cols = 0;
  private rows = 0;
  private backingW = 1;
  private backingH = 1;

  /** CPU-side instance buffers, sized to the grid in resizeForGrid. The bg
   *  buffer has a fixed per-cell region followed by a variable frame-rect
   *  region that grows on demand. */
  private bgBuf: Float32Array = new Float32Array(0);
  private glyphBuf: Float32Array = new Float32Array(0);
  private decorBuf: Float32Array = new Float32Array(0);
  private cursorBuf: Float32Array = new Float32Array(0);
  private cursorGlyphBuf: Float32Array = new Float32Array(0);

  /** Slot geometry derives from cell metrics, so the index is recreated
   *  together with the raster on every metrics change — an index built from
   *  stale (or the constructor's possibly-zero) cell dims allocates
   *  pad-only slivers and every glyph samples as empty. */
  private atlas!: AtlasIndex;
  private raster!: GlyphRaster;
  private readonly misses: GlyphMiss[] = [];

  private contextLost = false;
  private diagCount = 0;
  private focused = true;
  private fullRebuild = true;
  private disposed = false;
  private bgFrameOffset = 0;
  private bgFrameCapacity = 0;
  private bgDrawCount = 0;
  private glyphDrawCount = 0;
  private decorDrawCount = 0;
  private cursorDrawCount = 0;
  private cursorGlyphActive = false;
  /** Blink phase: cursor drawn when true. Frames reset it visible so the
   *  cursor holds solid while output/typing flows. */
  private blinkVisible = true;
  private blinkTimer: ReturnType<typeof setInterval> | null = null;
  private frameRectCount = 0;

  /** Rows whose bg/glyph/decor slots changed this frame — uploaded via
   *  bufferSubData. Glyph rows widen to ALL rows after an eviction rebuild. */
  private bgRows: number[] = [];
  private decorRows: number[] = [];
  private glyphRows: number[] = [];

  /** The store apply() was handed, stashed so rasterizeMisses can patch the
   *  miss cells' slots; cleared by draw() (apply is otherwise stateless). */
  private currentStore: CellStore | null = null;
  private lastFrame: Frame | null = null;
  private lastCells: CellStore | null = null;

  private readonly onLost: (e: Event) => void;
  private readonly onRestored: () => void;

  constructor(opts: WebGLRendererOptions) {
    this.onRequestFull = opts.onRequestFull ?? (() => {});
    this.defaultBg = opts.defaultBg ?? 0x181818ff;
    this.cursorColor = opts.cursorColor ?? CURSOR_COLOR;
    this.pageSize = ATLAS_DEFAULT_PAGE_SIZE;
    this.maxPages = ATLAS_DEFAULT_MAX_PAGES;
    this.cellW = quantizeCell(opts.cellW, opts.dpr);
    this.cellH = quantizeCell(opts.cellH, opts.dpr);
    this.dpr = opts.dpr;
    this.baseline = opts.baseline;
    this.fontFamily = opts.fontFamily;
    this.fontSize = opts.fontSize;

    this.canvas = document.createElement("canvas");
    this.canvas.style.position = "absolute";
    this.canvas.style.inset = "0";
    this.canvas.style.width = "0px";
    this.canvas.style.height = "0px";
    opts.container.appendChild(this.canvas);
    // Default cells are transparent here so the container's terminal
    // background (set on the panel's viewport) shows through — same as the
    // DOM renderer's pre background.
    opts.container.style.background = "#181818";

    this.onLost = (e: Event): void => {
      // Default behaviour is to lose the context permanently; prevent it so
      // webglcontextrestored can bring the pipeline back.
      e.preventDefault();
      this.contextLost = true;
    };
    this.onRestored = (): void => {
      this.contextLost = false;
      this.recreatePipeline();
      this.atlas.reset();
      this.raster.reset();
      this.fullRebuild = true;
      this.onRequestFull();
    };
    this.canvas.addEventListener("webglcontextlost", this.onLost, false);
    this.canvas.addEventListener("webglcontextrestored", this.onRestored, false);

    let gl: WebGL2RenderingContext | null = null;
    try {
      gl = this.canvas.getContext("webgl2", {
        premultipliedAlpha: true,
        alpha: true,
        antialias: false,
        depth: false,
        stencil: false,
        preserveDrawingBuffer: false,
      }) as WebGL2RenderingContext | null;
    } catch {
      gl = null;
    }
    if (!gl) {
      this.canvas.remove();
      throw new Error("WebGLRenderer: WebGL2 context unavailable");
    }
    this.gl = gl;
    this.rebuildAtlasAndRaster();
    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.CULL_FACE);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    this.initPipeline();

    // The atlas caches whatever fillText produced — if a bold/italic face
    // hasn't loaded when its first glyph rasterizes, the FALLBACK glyph is
    // cached forever (serif 'BOLD' in a mono terminal). Load every face this
    // renderer will ever ask for, then re-rasterize from scratch once.
    const fonts = typeof document !== "undefined" ? document.fonts : undefined;
    if (fonts?.load) {
      const fam = `${this.fontSize}px ${cssFontFamily(this.fontFamily)}`;
      void Promise.allSettled([
        fonts.load(fam),
        fonts.load(`700 ${fam}`),
        fonts.load(`italic ${fam}`),
        fonts.load(`italic 700 ${fam}`),
      ]).then(() => {
        if (this.disposed) return;
        this.resetGlyphState();
        this.onRequestFull();
      });
    }

    // Self-correct the glyph metrics once the real font is loaded: the panel's
    // DOM probe measures the line box, while glyph rasterization wants the
    // canvas baseline + the true devicePixelRatio. jsdom has no canvas, so
    // this resolves to the constructor heuristics there.
    let settled = false;
    measureCellMetrics(this.fontFamily, this.fontSize)
      .then((m) => {
        if (this.disposed || settled) return;
        settled = true;
        const changed =
          Math.abs(m.baseline - this.baseline) > 1e-6 || Math.abs(m.dpr - this.dpr) > 1e-6;
        this.baseline = m.baseline;
        this.dpr = m.dpr;
        if (changed) {
          // dpr feeds slot geometry and baseline feeds rasterization — both
          // factories must follow (see rebuildAtlasAndRaster).
          this.rebuildAtlasAndRaster();
          this.resizeCanvas();
          this.resetGlyphState();
          this.onRequestFull();
        }
      })
      .catch(() => {
        /* Canvas2D unavailable (jsdom) — keep the heuristic baseline/dpr. */
      });
  }

  apply(frame: Frame, cells?: CellStore): void {
    if (this.contextLost) return;
    const store = cells ?? frame.cells;
    this.currentStore = store;
    this.lastFrame = frame;
    this.lastCells = store;
    this.blinkVisible = true;
    if (store.cols !== this.cols || store.rows !== this.rows) {
      this.resizeForGrid(store.cols, store.rows);
    }
    const spans =
      this.fullRebuild || frame.kind === "full"
        ? allRowSpans(store.cols, store.rows)
        : frame.rows;
    this.fullRebuild = false;
    this.bgRows = dirtyRows(spans);
    this.decorRows = this.bgRows;

    // Glyph pass: per-cell slots for the dirty rows; rasterize misses, and if
    // an eviction invalidated already-written slots rebuild the whole list
    // once (the rebuild hits the atlas, so the guard terminates).
    this.misses.length = 0;
    writeGlyphCells(this.glyphBuf, store, spans, this.atlas, this.pageSize, this.gpuMetrics(), this.misses);
    this.glyphRows = this.bgRows;
    if (this.misses.length > 0) {
      let rebuilds = 0;
      for (;;) {
        const evicted = this.rasterizeMisses();
        if (!evicted || rebuilds >= 2) break;
        rebuilds++;
        const all = allRowSpans(store.cols, store.rows);
        this.misses.length = 0;
        writeGlyphCells(this.glyphBuf, store, all, this.atlas, this.pageSize, this.gpuMetrics(), this.misses);
        this.glyphRows = dirtyRows(all);
        if (this.misses.length === 0) break;
      }
      this.misses.length = 0;
    }
    this.glyphDrawCount = store.cols * store.rows;

    // Bg pass: cell backgrounds for the dirty rows + the per-frame rects.
    writeBgCells(this.bgBuf, store, spans, this.defaultBg, this.gpuMetrics());
    this.ensureFrameCapacity(countFrameRects(frame, store.cols, store.rows, this.focused));
    this.frameRectCount = buildBgFrameRects(
      this.bgBuf,
      frame,
      store.cols,
      store.rows,
      this.bgFrameOffset,
      this.gpuMetrics(),
      this.cursorColor,
      this.focused,
    );
    this.bgDrawCount = store.cols * store.rows + this.frameRectCount;

    // Overlay pass: decorations for dirty rows + non-block cursor + inverse
    // block glyph.
    writeDecorCells(this.decorBuf, store, spans, this.gpuMetrics());
    this.decorDrawCount = store.cols * store.rows;
    this.cursorDrawCount = buildCursorRects(this.cursorBuf, frame, this.gpuMetrics(), this.focused, this.cursorColor);
    this.cursorGlyphActive = blockCursorGlyph(frame, this.focused) !== null;
    this.writeCursorGlyph();

    this.uploadAll();
    this.draw();
  }

  setMetrics(m: RendererMetrics): void {
    const cellW = quantizeCell(m.cellW, m.dpr);
    const cellH = quantizeCell(m.cellH, m.dpr);
    const changed =
      Math.abs(cellW - this.cellW) > 1e-6 ||
      Math.abs(cellH - this.cellH) > 1e-6 ||
      Math.abs(m.dpr - this.dpr) > 1e-6 ||
      Math.abs(m.baseline - this.baseline) > 1e-6 ||
      m.fontFamily !== this.fontFamily ||
      m.fontSize !== this.fontSize;
    this.cellW = cellW;
    this.cellH = cellH;
    this.dpr = m.dpr;
    this.baseline = m.baseline;
    this.fontFamily = m.fontFamily;
    this.fontSize = m.fontSize;
    this.rebuildAtlasAndRaster();
    if (changed) {
      this.resizeCanvas();
      this.resetGlyphState();
      this.onRequestFull();
    }
  }

  focus(focused: boolean): void {
    if (focused === this.focused) return;
    this.focused = focused;
    // Blink only while focused (terminal convention; the unfocused hollow
    // cursor holds steady). Regaining focus restarts the phase visible.
    this.blinkVisible = true;
    if (focused) this.startBlink();
    else this.stopBlink();
    // The cursor shape only changes on frames, so repaint the last one with
    // the new focus state — a block cursor hollows out the moment the panel
    // blurs even if the actor sends nothing.
    this.repaintCursor();
  }

  /** The last frame with the cursor masked out during the blink-off phase —
   *  feeding this to the cursor builders hides all three cursor passes
   *  (bg-pass block rect, shape rects, inverse glyph) without touching their
   *  signatures. */
  private blinkFrame(): Frame | null {
    if (!this.lastFrame) return null;
    if (this.blinkVisible) return this.lastFrame;
    return { ...this.lastFrame, cursor: { ...this.lastFrame.cursor, visible: false } };
  }

  /** Rebuild + redraw only the cursor-bearing passes from the last frame
   *  (focus flips and blink ticks — the grid itself is unchanged). */
  private repaintCursor(): void {
    const frame = this.blinkFrame();
    if (!frame || !this.lastCells) return;
    const store = this.lastCells;
    this.ensureFrameCapacity(countFrameRects(frame, store.cols, store.rows, this.focused));
    this.frameRectCount = buildBgFrameRects(
      this.bgBuf,
      frame,
      store.cols,
      store.rows,
      this.bgFrameOffset,
      this.gpuMetrics(),
      this.cursorColor,
      this.focused,
    );
    this.bgDrawCount = store.cols * store.rows + this.frameRectCount;
    this.cursorDrawCount = buildCursorRects(this.cursorBuf, frame, this.gpuMetrics(), this.focused, this.cursorColor);
    this.cursorGlyphActive = blockCursorGlyph(frame, this.focused) !== null;
    this.writeCursorGlyph();
    this.uploadFrameRects();
    this.uploadCursor();
    this.draw();
  }

  private startBlink(): void {
    if (this.blinkTimer !== null) return;
    this.blinkTimer = setInterval(() => {
      this.blinkVisible = !this.blinkVisible;
      this.repaintCursor();
    }, 530);
  }

  private stopBlink(): void {
    if (this.blinkTimer !== null) {
      clearInterval(this.blinkTimer);
      this.blinkTimer = null;
    }
    this.blinkVisible = true;
  }

  dispose(): void {
    this.disposed = true;
    this.stopBlink();
    this.canvas.removeEventListener("webglcontextlost", this.onLost, false);
    this.canvas.removeEventListener("webglcontextrestored", this.onRestored, false);
    const gl = this.gl;
    if (gl) {
      for (const vao of [this.vaoBg, this.vaoGlyph, this.vaoDecor, this.vaoCursor, this.vaoCursorGlyph]) {
        if (vao) gl.deleteVertexArray(vao);
      }
      for (const buf of [this.bgGpu, this.glyphGpu, this.decorGpu, this.cursorGpu, this.cursorGlyphGpu]) {
        if (buf) gl.deleteBuffer(buf);
      }
      if (this.atlasTex) gl.deleteTexture(this.atlasTex);
      if (this.colorProg) gl.deleteProgram(this.colorProg);
      if (this.glyphProg) gl.deleteProgram(this.glyphProg);
    }
    this.canvas.remove();
  }

  // --- pipeline ------------------------------------------------------------

  private gpuMetrics(): GpuMetrics {
    return { cellW: this.cellW, cellH: this.cellH, dpr: this.dpr };
  }

  /** (Re)create the atlas index + raster from the current metrics — always
   *  together: index slot geometry (cellW/cellH/dpr) and rasterization
   *  (baseline/font/dpr) must agree, or slots and drawn glyphs diverge. */
  private rebuildAtlasAndRaster(): void {
    this.atlas = new AtlasIndex({
      cellW: this.cellW,
      cellH: this.cellH,
      dpr: this.dpr,
      pageSize: this.pageSize,
      maxPages: this.maxPages,
    });
    this.raster = new GlyphRaster({
      family: this.fontFamily,
      fontSize: this.fontSize,
      cellW: this.cellW,
      cellH: this.cellH,
      baseline: this.baseline,
      dpr: this.dpr,
    });
  }

  private initPipeline(): void {
    const gl = this.gl;
    if (!gl) return;
    this.colorProg = this.compile(COLOR_VERT_SRC, COLOR_FRAG_SRC);
    this.glyphProg = this.compile(GLYPH_VERT_SRC, GLYPH_FRAG_SRC);
    this.createTexture();
    this.setupColorVao("bg");
    this.setupColorVao("decor");
    this.setupColorVao("cursor");
    this.setupGlyphVao("glyph");
    this.setupGlyphVao("cursorGlyph");
    this.resizeCanvas();
  }

  private recreatePipeline(): void {
    const gl = this.gl;
    if (!gl) return;
    for (const vao of [this.vaoBg, this.vaoGlyph, this.vaoDecor, this.vaoCursor, this.vaoCursorGlyph]) {
      if (vao) gl.deleteVertexArray(vao);
    }
    for (const buf of [this.bgGpu, this.glyphGpu, this.decorGpu, this.cursorGpu, this.cursorGlyphGpu]) {
      if (buf) gl.deleteBuffer(buf);
    }
    if (this.atlasTex) gl.deleteTexture(this.atlasTex);
    if (this.colorProg) gl.deleteProgram(this.colorProg);
    if (this.glyphProg) gl.deleteProgram(this.glyphProg);
    this.vaoBg = this.vaoGlyph = this.vaoDecor = this.vaoCursor = this.vaoCursorGlyph = null;
    this.bgGpu = this.glyphGpu = this.decorGpu = this.cursorGpu = this.cursorGlyphGpu = null;
    this.initPipeline();
  }

  private compile(vert: string, frag: string): WebGLProgram {
    const gl = this.gl;
    if (!gl) throw new Error("WebGLRenderer: no context");
    const vs = gl.createShader(gl.VERTEX_SHADER);
    const fs = gl.createShader(gl.FRAGMENT_SHADER);
    if (!vs || !fs) throw new Error("WebGLRenderer: shader allocation failed");
    gl.shaderSource(vs, vert);
    gl.shaderSource(fs, frag);
    gl.compileShader(vs);
    gl.compileShader(fs);
    if (!gl.getShaderParameter(vs, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(vs);
      gl.deleteShader(vs);
      gl.deleteShader(fs);
      throw new Error(`WebGLRenderer: vertex shader compile failed: ${log}`);
    }
    if (!gl.getShaderParameter(fs, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(fs);
      gl.deleteShader(vs);
      gl.deleteShader(fs);
      throw new Error(`WebGLRenderer: fragment shader compile failed: ${log}`);
    }
    const prog = gl.createProgram();
    if (!prog) throw new Error("WebGLRenderer: program allocation failed");
    gl.attachShader(prog, vs);
    gl.attachShader(prog, fs);
    gl.linkProgram(prog);
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
      const log = gl.getProgramInfoLog(prog);
      gl.deleteProgram(prog);
      throw new Error(`WebGLRenderer: program link failed: ${log}`);
    }
    return prog;
  }

  /** Create + clear the TEXTURE_2D_ARRAY glyph atlas (pages = layers). */
  private createTexture(): void {
    const gl = this.gl;
    if (!gl) return;
    this.atlasTex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D_ARRAY, this.atlasTex);
    gl.texStorage3D(gl.TEXTURE_2D_ARRAY, 1, gl.RGBA8, this.pageSize, this.pageSize, this.maxPages);
    gl.texParameteri(gl.TEXTURE_2D_ARRAY, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D_ARRAY, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D_ARRAY, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D_ARRAY, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    for (let p = 0; p < this.maxPages; p++) this.clearLayer(p);
  }

  private setupColorVao(kind: "bg" | "decor" | "cursor"): void {
    const gl = this.gl;
    if (!gl || !this.colorProg) return;
    const vao = gl.createVertexArray();
    const buf = gl.createBuffer();
    if (!vao || !buf) throw new Error("WebGLRenderer: vertex array/buffer allocation failed");
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, RECT_FLOATS * 4, 0);
    gl.vertexAttribDivisor(0, 1);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 4, gl.FLOAT, false, RECT_FLOATS * 4, 4 * 4);
    gl.vertexAttribDivisor(1, 1);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribPointer(2, 1, gl.FLOAT, false, RECT_FLOATS * 4, 8 * 4);
    gl.vertexAttribDivisor(2, 1);
    gl.bindVertexArray(null);
    if (kind === "bg") {
      this.vaoBg = vao;
      this.bgGpu = buf;
    } else if (kind === "decor") {
      this.vaoDecor = vao;
      this.decorGpu = buf;
    } else {
      this.vaoCursor = vao;
      this.cursorGpu = buf;
    }
  }

  private setupGlyphVao(kind: "glyph" | "cursorGlyph"): void {
    const gl = this.gl;
    if (!gl || !this.glyphProg) return;
    const vao = gl.createVertexArray();
    const buf = gl.createBuffer();
    if (!vao || !buf) throw new Error("WebGLRenderer: vertex array/buffer allocation failed");
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    const stride = GLYPH_FLOATS * 4;
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, stride, 0);
    gl.vertexAttribDivisor(0, 1);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 4, gl.FLOAT, false, stride, 4 * 4);
    gl.vertexAttribDivisor(1, 1);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribPointer(2, 1, gl.FLOAT, false, stride, 8 * 4);
    gl.vertexAttribDivisor(2, 1);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 4, gl.FLOAT, false, stride, 9 * 4);
    gl.vertexAttribDivisor(3, 1);
    gl.enableVertexAttribArray(4);
    gl.vertexAttribPointer(4, 1, gl.FLOAT, false, stride, 13 * 4);
    gl.vertexAttribDivisor(4, 1);
    gl.bindVertexArray(null);
    if (kind === "glyph") {
      this.vaoGlyph = vao;
      this.glyphGpu = buf;
    } else {
      this.vaoCursorGlyph = vao;
      this.cursorGlyphGpu = buf;
    }
  }

  private resizeForGrid(cols: number, rows: number): void {
    this.cols = cols;
    this.rows = rows;
    const cellCount = cols * rows;
    this.bgFrameOffset = cellCount * RECT_FLOATS;
    this.bgFrameCapacity = rows * 2 + 8;
    this.bgBuf = new Float32Array((cellCount + this.bgFrameCapacity) * RECT_FLOATS);
    this.glyphBuf = new Float32Array(cellCount * GLYPH_FLOATS);
    this.decorBuf = new Float32Array(cellCount * RECT_FLOATS);
    this.cursorBuf = new Float32Array(8 * RECT_FLOATS);
    this.cursorGlyphBuf = new Float32Array(2 * GLYPH_FLOATS);
    this.resizeCanvas();
    this.reallocGpu();
  }

  /** Grow the bg buffer's variable frame-rect region when a frame carries
   *  more selection/match rects than the default capacity. */
  private ensureFrameCapacity(needed: number): void {
    if (needed <= this.bgFrameCapacity) return;
    this.bgFrameCapacity = needed;
    const cellFloats = this.cols * this.rows * RECT_FLOATS;
    const next = new Float32Array((this.cols * this.rows + needed) * RECT_FLOATS);
    next.set(this.bgBuf.subarray(0, cellFloats));
    this.bgBuf = next;
    const gl = this.gl;
    if (gl && this.bgGpu) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.bgGpu);
      gl.bufferData(gl.ARRAY_BUFFER, this.bgBuf, gl.DYNAMIC_DRAW);
    }
  }

  /** (Re)allocate every GPU instance buffer to match the current CPU arrays.
   *  bufferData (not subData) because storage grew or the grid changed. */
  private reallocGpu(): void {
    const gl = this.gl;
    if (!gl) return;
    const upload = (buf: WebGLBuffer | null, data: Float32Array): void => {
      if (!buf) return;
      gl.bindBuffer(gl.ARRAY_BUFFER, buf);
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.DYNAMIC_DRAW);
    };
    upload(this.bgGpu, this.bgBuf);
    upload(this.glyphGpu, this.glyphBuf);
    upload(this.decorGpu, this.decorBuf);
    upload(this.cursorGpu, this.cursorBuf);
    upload(this.cursorGlyphGpu, this.cursorGlyphBuf);
  }

  private resizeCanvas(): void {
    const cssW = this.cols * this.cellW;
    const cssH = this.rows * this.cellH;
    this.canvas.style.width = `${cssW}px`;
    this.canvas.style.height = `${cssH}px`;
    const bw = Math.max(1, Math.round(cssW * this.dpr));
    const bh = Math.max(1, Math.round(cssH * this.dpr));
    if (this.canvas.width !== bw) this.canvas.width = bw;
    if (this.canvas.height !== bh) this.canvas.height = bh;
    this.backingW = bw;
    this.backingH = bh;
  }

  private resetGlyphState(): void {
    this.atlas.reset();
    this.raster.reset();
    const gl = this.gl;
    if (gl && this.atlasTex) {
      for (let p = 0; p < this.maxPages; p++) this.clearLayer(p);
    }
    this.fullRebuild = true;
  }

  private clearLayer(page: number): void {
    const gl = this.gl;
    if (!gl || !this.atlasTex) return;
    const any = gl as WebGL2RenderingContext & {
      clearTexImage?: (t: WebGLTexture, l: number, f: number, t2: number, d: ArrayBufferView | null) => void;
    };
    if (typeof any.clearTexImage === "function") {
      gl.bindTexture(gl.TEXTURE_2D_ARRAY, this.atlasTex);
      any.clearTexImage(this.atlasTex, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
      return;
    }
    // Fallback: upload a cleared page canvas into the whole layer.
    const canvas = this.raster.getPage(page);
    const ctx = canvas.getContext("2d");
    ctx?.clearRect(0, 0, this.pageSize, this.pageSize);
    gl.bindTexture(gl.TEXTURE_2D_ARRAY, this.atlasTex);
    gl.texSubImage3D(gl.TEXTURE_2D_ARRAY, 0, 0, 0, page, this.pageSize, this.pageSize, 1, gl.RGBA, gl.UNSIGNED_BYTE, canvas);
  }

  private uploadDirty(page: number, dirty: { x: number; y: number; w: number; h: number }): void {
    const gl = this.gl;
    if (!gl || !this.atlasTex) return;
    const canvas = this.raster.getPage(page);
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    // getImageData returns straight alpha; the canvas backing store (and the
    // shader's premultiplied blending) is premultiplied, so ask the driver to
    // premultiply on upload. Canvas sources ignore this flag, hence the
    // dirty-rect path goes through getImageData instead of texSubImage3D(canvas).
    const data = ctx.getImageData(dirty.x, dirty.y, dirty.w, dirty.h);
    gl.bindTexture(gl.TEXTURE_2D_ARRAY, this.atlasTex);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
    gl.texSubImage3D(gl.TEXTURE_2D_ARRAY, 0, dirty.x, dirty.y, page, dirty.w, dirty.h, 1, gl.RGBA, gl.UNSIGNED_BYTE, data);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  }

  /** Rasterize every queued miss into the atlas page canvases and upload the
   *  dirty rects; returns true when an eviction invalidated glyph slots that
   *  were already written this frame (the caller rebuilds the glyph list). */
  private rasterizeMisses(): boolean {
    let evicted = false;
    const store = this.currentStore;
    for (const miss of this.misses) {
      const { slot, evictedPages } = this.atlas.insert(miss.key, miss.wide);
      for (const p of evictedPages) {
        this.raster.clearPage(p);
        this.clearLayer(p);
        evicted = true;
      }
      const res = this.raster.rasterize(slot, miss.codepoint, miss.wide, miss.bold, miss.italic);
      this.uploadDirty(slot.page, res.dirty);
      if (store) {
        writeGlyphCell(
          this.glyphBuf,
          (miss.row * store.cols + miss.col) * GLYPH_FLOATS,
          store,
          miss.row * store.cols + miss.col,
          miss.row,
          miss.col,
          this.atlas,
          this.pageSize,
          this.gpuMetrics(),
          null,
        );
      }
    }
    return evicted;
  }

  private writeCursorGlyph(): void {
    if (!this.lastFrame || !this.lastCells) return;
    const cg = blockCursorGlyph(this.lastFrame, this.focused);
    this.cursorGlyphActive = cg !== null;
    if (!cg) return;
    const store = this.lastCells;
    const idx = cg.row * store.cols + cg.col;
    writeGlyphCell(
      this.cursorGlyphBuf,
      0,
      store,
      idx,
      cg.row,
      cg.col,
      this.atlas,
      this.pageSize,
      this.gpuMetrics(),
      null,
      fgFloats(store.bg[idx], 0),
    );
  }

  // --- uploads + draws -----------------------------------------------------

  /** One bufferSubData per dirty row per pass — untouched rows are never
   *  re-sent, so damage cost is proportional to damage, not grid size. */
  private uploadRows(buf: WebGLBuffer | null, data: Float32Array, rows: number[], floatsPerCell: number): void {
    const gl = this.gl;
    if (!gl || !buf) return;
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    const stride = this.cols * floatsPerCell;
    for (const r of rows) {
      if (r < 0 || r >= this.rows) continue;
      gl.bufferSubData(gl.ARRAY_BUFFER, r * stride * 4, data, r * stride, stride);
    }
  }

  private uploadFrameRects(): void {
    const gl = this.gl;
    if (!gl || !this.bgGpu || this.frameRectCount === 0) return;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.bgGpu);
    gl.bufferSubData(
      gl.ARRAY_BUFFER,
      this.bgFrameOffset * 4,
      this.bgBuf,
      this.bgFrameOffset,
      this.frameRectCount * RECT_FLOATS,
    );
  }

  private uploadCursor(): void {
    const gl = this.gl;
    if (!gl) return;
    if (this.cursorGpu && this.cursorDrawCount > 0) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.cursorGpu);
      gl.bufferSubData(gl.ARRAY_BUFFER, 0, this.cursorBuf, 0, this.cursorDrawCount * RECT_FLOATS);
    }
    if (this.cursorGlyphGpu && this.cursorGlyphActive) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.cursorGlyphGpu);
      gl.bufferSubData(gl.ARRAY_BUFFER, 0, this.cursorGlyphBuf, 0, GLYPH_FLOATS);
    }
  }

  private uploadAll(): void {
    this.uploadRows(this.bgGpu, this.bgBuf, this.bgRows, RECT_FLOATS);
    this.uploadFrameRects();
    this.uploadRows(this.glyphGpu, this.glyphBuf, this.glyphRows, GLYPH_FLOATS);
    this.uploadRows(this.decorGpu, this.decorBuf, this.decorRows, RECT_FLOATS);
    this.uploadCursor();
  }

  private draw(): void {
    this.currentStore = null;
    const gl = this.gl;
    const colorProg = this.colorProg;
    const glyphProg = this.glyphProg;
    if (!gl || this.contextLost || !colorProg || !glyphProg) return;
    gl.viewport(0, 0, this.backingW, this.backingH);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);

    // Pass 1: backgrounds + selection/matches/cursor-fill.
    gl.useProgram(colorProg);
    gl.uniform2f(gl.getUniformLocation(colorProg, "u_viewport"), this.backingW, this.backingH);
    gl.uniform1f(gl.getUniformLocation(colorProg, "u_dpr"), this.dpr);
    gl.bindVertexArray(this.vaoBg);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.bgDrawCount);

    // Pass 2: glyphs.
    gl.useProgram(glyphProg);
    gl.uniform2f(gl.getUniformLocation(glyphProg, "u_viewport"), this.backingW, this.backingH);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D_ARRAY, this.atlasTex);
    gl.uniform1i(gl.getUniformLocation(glyphProg, "u_atlas"), 0);
    gl.bindVertexArray(this.vaoGlyph);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.glyphDrawCount);

    // Pass 3a: decorations (underlines/undercurl/strikeout).
    gl.useProgram(colorProg);
    gl.uniform2f(gl.getUniformLocation(colorProg, "u_viewport"), this.backingW, this.backingH);
    gl.uniform1f(gl.getUniformLocation(colorProg, "u_dpr"), this.dpr);
    gl.bindVertexArray(this.vaoDecor);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.decorDrawCount);

    // Pass 3b: non-block cursor shapes.
    gl.bindVertexArray(this.vaoCursor);
    if (this.cursorDrawCount > 0) {
      gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.cursorDrawCount);
    }

    // Pass 3c: inverse block-cursor glyph.
    if (this.cursorGlyphActive) {
      gl.useProgram(glyphProg);
      gl.bindVertexArray(this.vaoCursorGlyph);
      gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, 1);
    }

    if (GLDIAG && this.diagCount++ % 15 === 0) this.diag();
  }

  /** `?gldiag=1` state dump: everything needed to see why a frame is blank.
   *  Runs at the end of draw() — same task, so readPixels of the default
   *  framebuffer is valid despite preserveDrawingBuffer:false. */
  private diag(): void {
    const gl = this.gl;
    if (!gl) return;
    let live = -1;
    let sample = "";
    for (let i = 0; i < this.glyphDrawCount; i++) {
      const o = i * GLYPH_FLOATS;
      if (this.glyphBuf[o + 2] > 0) {
        live = i;
        sample = Array.from(this.glyphBuf.slice(o, o + GLYPH_FLOATS))
          .map((v) => +v.toFixed(4))
          .join(",");
        break;
      }
    }
    // What actually landed in the drawing buffer this frame.
    const rw = Math.min(this.backingW, 400);
    const rh = Math.min(this.backingH, 200);
    const px = new Uint8Array(rw * rh * 4);
    gl.readPixels(0, 0, rw, rh, gl.RGBA, gl.UNSIGNED_BYTE, px);
    let lit = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] + px[i + 1] + px[i + 2] > 24) lit++;
    // What the atlas texture layer 0 actually holds.
    const fb = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
    gl.framebufferTextureLayer(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, this.atlasTex, 0, 0);
    const fboOk = gl.checkFramebufferStatus(gl.FRAMEBUFFER) === gl.FRAMEBUFFER_COMPLETE;
    let atlasPx = 0;
    if (fboOk) {
      const ap = new Uint8Array(128 * 64 * 4);
      gl.readPixels(0, 0, 128, 64, gl.RGBA, gl.UNSIGNED_BYTE, ap);
      for (let i = 3; i < ap.length; i += 4) if (ap[i] > 8) atlasPx++;
    }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.deleteFramebuffer(fb);
    // Store-flag census: are decoration/wide flags even reaching the frontend?
    const UNDER_MASK =
      CELL_FLAGS.underline |
      CELL_FLAGS.doubleUnderline |
      CELL_FLAGS.undercurl |
      CELL_FLAGS.dottedUnderline |
      CELL_FLAGS.dashedUnderline;
    let under = 0;
    let wide = 0;
    let bold = 0;
    let decorLive = 0;
    const cells = this.lastCells;
    if (cells) {
      for (let i = 0; i < cells.flags.length; i++) {
        const f = cells.flags[i];
        if (f & UNDER_MASK) under++;
        if (f & CELL_FLAGS.wide) wide++;
        if (f & CELL_FLAGS.bold) bold++;
      }
    }
    for (let i = 0; i < this.decorDrawCount; i++) {
      if (this.decorBuf[i * RECT_FLOATS + 2] > 0) decorLive++;
    }
    console.log(
      `[gldiag] backing=${this.backingW}x${this.backingH} grid=${this.cols}x${this.rows} ` +
        `cell=${this.cellW}x${this.cellH}@dpr${this.dpr} baseline=${this.baseline} ` +
        `draws bg=${this.bgDrawCount} glyph=${this.glyphDrawCount} decor=${this.decorDrawCount} ` +
        `firstLiveGlyph=${live} [${sample}] ` +
        `canvasLit=${lit}/${rw * rh} atlas128x64=${atlasPx}px fboOk=${fboOk} glErr=${gl.getError()} ` +
        `storeFlags under=${under} wide=${wide} bold=${bold} decorLive=${decorLive}`,
    );
  }
}

/** Diagnostics flag — logs renderer state every 60 frames when the page URL
 *  carries `gldiag=1`. */
const GLDIAG = typeof window !== "undefined" && /[?&]gldiag=1/.test(window.location.search);
