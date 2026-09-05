// Glyph atlas: AtlasIndex (pure packing logic) + GlyphRaster (browser-only
// Canvas2D shell).
//
// The atlas is deliberately renderer-agnostic: it hands back a page index and
// a slot rect; only the WebGL renderer talks to WebGL textures. AtlasIndex is pure
// TypeScript and fully unit-tested; GlyphRaster depends on Canvas2D and is
// host-verified instead (jsdom has no canvas).

/** Device-pixel padding on each side of a glyph slot (1px left/right/top/bottom). */
export const ATLAS_SLOT_PAD = 1;
export const ATLAS_DEFAULT_PAGE_SIZE = 2048;
export const ATLAS_DEFAULT_MAX_PAGES = 8;

// --- key packing ----------------------------------------------------------
// A glyph key is one number: codepoint plus style bits, so Map lookups are a
// single integer compare and keys can pass through frames untouched. Bit
// layout (LSB first):
//   bits 0..20   codepoint — 21 bits cover the full Unicode range (0x10FFFF),
//                including astral-plane emoji
//   bit  21      bold
//   bit  22      italic
// Bits 23+ reserved.
const KEY_CODEPOINT_BITS = 21;
const KEY_BOLD_BIT = 1 << KEY_CODEPOINT_BITS;
const KEY_ITALIC_BIT = 1 << (KEY_CODEPOINT_BITS + 1);
const CODEPOINT_MASK = (1 << KEY_CODEPOINT_BITS) - 1;

export function packKey(codepoint: number, bold = false, italic = false): number {
  return (codepoint & CODEPOINT_MASK) | (bold ? KEY_BOLD_BIT : 0) | (italic ? KEY_ITALIC_BIT : 0);
}

export function unpackKey(key: number): { codepoint: number; bold: boolean; italic: boolean } {
  return {
    codepoint: key & CODEPOINT_MASK,
    bold: (key & KEY_BOLD_BIT) !== 0,
    italic: (key & KEY_ITALIC_BIT) !== 0,
  };
}

// --- slot sizing ----------------------------------------------------------
/** Slot size in device px for a glyph of the given width class. `round`, not
 *  `ceil`: the renderer quantizes cell metrics so cell×dpr is integral, and
 *  round keeps FP epsilon (17.000000000000002) from inflating the slot by a
 *  pixel — the glyph region must be EXACTLY the device cell for the 1:1
 *  quad↔texel mapping. */
export function slotDims(
  cellW: number,
  cellH: number,
  dpr: number,
  wide = false,
): { w: number; h: number } {
  const gw = Math.round(cellW * dpr);
  const gh = Math.round(cellH * dpr);
  return {
    w: (wide ? 2 * gw : gw) + 2 * ATLAS_SLOT_PAD,
    h: gh + 2 * ATLAS_SLOT_PAD,
  };
}

// --- block elements (geometric) --------------------------------------------

/** Geometry for U+2580–U+259F block elements in a `w`×`h` device-px cell, or
 *  null for anything else. These are drawn as rects instead of font glyphs
 *  (the ghostty/kitty approach): fonts give shades a coarse stipple, give
 *  blocks side bearings, and often lack the quadrants entirely (fallback
 *  font at the wrong size — Claude Code's block-art mascot rendered as a
 *  mismatched patchwork, host-run). Shades are full-cell fills at
 *  25/50/75% alpha; everything else is fractional rects on ONE shared grid:
 *  every boundary is `frac8(k)` measured from the top/left, and regions are
 *  [boundary, boundary) intervals — so ▀ under ▄, or ▐ beside ▙, meet at
 *  exactly the same pixel even when the cell is odd-sized (independent
 *  rounding notched the seams by 1px). */
export function blockElementRects(
  cp: number,
  w: number,
  h: number,
): { x: number; y: number; w: number; h: number; alpha: number }[] | null {
  const frac8 = (n: number, total: number) => Math.round((total * n) / 8);
  const rect = (x: number, y: number, rw: number, rh: number) => ({ x, y, w: rw, h: rh, alpha: 1 });
  // A horizontal band [frac8(top), frac8(bot)) and vertical band for columns.
  const band = (top: number, bot: number) => rect(0, frac8(top, h), w, frac8(bot, h) - frac8(top, h));
  const col = (left: number, right: number) => rect(frac8(left, w), 0, frac8(right, w) - frac8(left, w), h);

  if (cp === 0x2580) return [band(0, 4)]; // ▀
  if (cp >= 0x2581 && cp <= 0x2588) return [band(8 - (cp - 0x2580), 8)]; // ▁…█ lower n/8
  if (cp >= 0x2589 && cp <= 0x258f) return [col(0, 0x2590 - cp)]; // ▉…▏ left n/8
  if (cp === 0x2590) return [col(4, 8)]; // ▐
  if (cp === 0x2591) return [{ x: 0, y: 0, w, h, alpha: 0.25 }]; // ░
  if (cp === 0x2592) return [{ x: 0, y: 0, w, h, alpha: 0.5 }]; // ▒
  if (cp === 0x2593) return [{ x: 0, y: 0, w, h, alpha: 0.75 }]; // ▓
  if (cp === 0x2594) return [band(0, 1)]; // ▔
  if (cp === 0x2595) return [col(7, 8)]; // ▕

  if (cp >= 0x2596 && cp <= 0x259f) {
    // Quadrants: the four quarter-cells on the same shared grid.
    const wl = frac8(4, w);
    const hu = frac8(4, h);
    const UL = rect(0, 0, wl, hu);
    const UR = rect(wl, 0, w - wl, hu);
    const LL = rect(0, hu, wl, h - hu);
    const LR = rect(wl, hu, w - wl, h - hu);
    switch (cp) {
      case 0x2596: return [LL]; // ▖
      case 0x2597: return [LR]; // ▗
      case 0x2598: return [UL]; // ▘
      case 0x2599: return [UL, LL, LR]; // ▙
      case 0x259a: return [UL, LR]; // ▚
      case 0x259b: return [UL, UR, LL]; // ▛
      case 0x259c: return [UL, UR, LR]; // ▜
      case 0x259d: return [UR]; // ▝
      case 0x259e: return [UR, LL]; // ▞
      default: return [UR, LL, LR]; // ▟ (0x259f)
    }
  }
  return null;
}

// --- AtlasIndex -----------------------------------------------------------
export interface AtlasIndexOptions {
  /** Cell width, CSS px. */
  cellW: number;
  /** Cell height, CSS px. */
  cellH: number;
  dpr: number;
  /** Page edge length, device px. */
  pageSize?: number;
  /** Maximum number of atlas pages. */
  maxPages?: number;
}

export interface AtlasSlot {
  page: number;
  /** Slot origin in device px. */
  x: number;
  y: number;
  /** Slot size in device px, padding included. */
  w: number;
  h: number;
}

export interface InsertResult {
  slot: AtlasSlot;
  /** Pages whose content was dropped (version bumped): re-upload + clear. */
  evictedPages: number[];
}

interface Page {
  index: number;
  /** Bumped on every eviction/reset so the renderer re-uploads the page. */
  version: number;
  /** Monotonic access clock value — the LRU recency key. */
  lastUsed: number;
  /** Next free x within the current shelf. */
  x: number;
  /** Top of the current shelf. */
  y: number;
}

/**
 * Shelf-packing glyph allocator over N square pages.
 *
 * Glyphs live in rows of uniform height (the slot height); a glyph is one
 * cell wide or two (wide chars / emoji). Pages fill top-to-bottom,
 * left-to-right, so at any moment at most one page still has room. When
 * every page is full the least-recently-USED page is evicted wholesale:
 * its version bumps (the renderer re-uploads the texture), its entries drop
 * (survivors re-enter lazily through future misses), and the freed page
 * becomes the working page.
 */
export class AtlasIndex {
  private readonly pageSize: number;
  private readonly maxPages: number;
  private readonly cellW: number;
  private readonly cellH: number;
  private readonly dpr: number;
  private readonly pages: Page[];
  private readonly entries = new Map<number, AtlasSlot>();
  private clock = 0;
  private misses = 0;
  private evictionCount = 0;

  constructor(opts: AtlasIndexOptions) {
    this.cellW = opts.cellW;
    this.cellH = opts.cellH;
    this.dpr = opts.dpr;
    this.pageSize = opts.pageSize ?? ATLAS_DEFAULT_PAGE_SIZE;
    this.maxPages = opts.maxPages ?? ATLAS_DEFAULT_MAX_PAGES;
    this.pages = [];
    for (let i = 0; i < this.maxPages; i++) {
      this.pages.push({ index: i, version: 0, lastUsed: 0, x: 0, y: 0 });
    }
  }

  get missCount(): number {
    return this.misses;
  }

  get evictions(): number {
    return this.evictionCount;
  }

  get pageCount(): number {
    return this.pages.length;
  }

  /** Monotonic version of a page; the renderer re-uploads when it changes. */
  pageVersion(page: number): number {
    return this.pages[page].version;
  }

  /**
   * Returns the slot for `key` or null on a miss. A hit bumps the page's
   * recency; a miss just counts — the renderer drains misses each frame by
   * rasterizing and re-inserting.
   */
  lookup(key: number): AtlasSlot | null {
    const slot = this.entries.get(key);
    if (!slot) {
      this.misses++;
      return null;
    }
    this.touch(this.pages[slot.page]);
    return slot;
  }

  /** Allocates a slot for `key` (idempotent: a duplicate insert returns the existing slot). */
  insert(key: number, wide = false): InsertResult {
    const existing = this.entries.get(key);
    if (existing) {
      this.touch(this.pages[existing.page]);
      return { slot: existing, evictedPages: [] };
    }

    const { w, h } = slotDims(this.cellW, this.cellH, this.dpr, wide);
    let fit = this.pages.find((p) => this.canAllocate(p, w, h));
    if (fit) {
      const slot = this.allocate(fit, w, h);
      this.entries.set(key, slot);
      this.touch(fit);
      return { slot, evictedPages: [] };
    }

    const victim = this.lruPage();
    this.evict(victim);
    const slot = this.allocate(victim, w, h);
    this.entries.set(key, slot);
    this.touch(victim);
    return { slot, evictedPages: [victim.index] };
  }

  /** DPR or font change: drop every entry, wipe packing state, bump versions. */
  reset(): void {
    this.entries.clear();
    for (const p of this.pages) {
      p.version++;
      p.x = 0;
      p.y = 0;
      p.lastUsed = 0;
    }
    this.clock = 0;
    this.misses = 0;
    this.evictionCount = 0;
  }

  /** Room in the current shelf, or room to start a fresh uniform-height shelf. */
  private canAllocate(p: Page, w: number, h: number): boolean {
    if (p.x + w <= this.pageSize) return true;
    return p.y + h + h <= this.pageSize;
  }

  private allocate(p: Page, w: number, h: number): AtlasSlot {
    if (p.x + w <= this.pageSize) {
      const slot = { page: p.index, x: p.x, y: p.y, w, h };
      p.x += w;
      return slot;
    }
    p.y += h;
    p.x = 0;
    const slot = { page: p.index, x: 0, y: p.y, w, h };
    p.x = w;
    return slot;
  }

  private lruPage(): Page {
    let victim = this.pages[0];
    for (const p of this.pages) {
      if (p.lastUsed < victim.lastUsed) victim = p;
    }
    return victim;
  }

  private evict(p: Page): void {
    for (const [key, slot] of this.entries) {
      if (slot.page === p.index) this.entries.delete(key);
    }
    p.version++;
    p.x = 0;
    p.y = 0;
    this.evictionCount++;
  }

  private touch(p: Page): void {
    p.lastUsed = ++this.clock;
  }
}

// --- color-glyph detection ------------------------------------------------
// BMP entries follow Unicode's Emoji_Presentation property (default-EMOJI
// glyphs) — NOT Extended_Pictographic. The difference matters in a terminal:
// text-presentation symbols (✔ ✻ ▶ © ↔ …) must be fg-TINTED like any other
// glyph, and the old Extended_Pictographic sweep of 2600–27BF made Claude
// Code's ✻/✽/✶ spinners render untinted white and skip the gamma correction
// (host-run). A default-emoji char classified here renders with the
// emoji font's own colors; everything else is a tinted silhouette — standard
// terminal behavior.
const COLOR_RANGES: ReadonlyArray<readonly [number, number]> = [
  [0x231a, 0x231b], // ⌚ ⌛
  [0x23e9, 0x23ec], // ⏩ ⏪ ⏫ ⏬
  [0x23f0, 0x23f0], // ⏰
  [0x23f3, 0x23f3], // ⏳
  [0x25fd, 0x25fe], // ◽ ◾
  [0x2614, 0x2615], // ☔ ☕
  [0x2648, 0x2653], // ♈–♓ zodiac
  [0x267f, 0x267f], // ♿
  [0x2693, 0x2693], // ⚓
  [0x26a1, 0x26a1], // ⚡
  [0x26aa, 0x26ab], // ⚪ ⚫
  [0x26bd, 0x26be], // ⚽ ⚾
  [0x26c4, 0x26c5], // ⛄ ⛅
  [0x26ce, 0x26ce], // ⛎
  [0x26d4, 0x26d4], // ⛔
  [0x26ea, 0x26ea], // ⛪
  [0x26f2, 0x26f3], // ⛲ ⛳
  [0x26f5, 0x26f5], // ⛵
  [0x26fa, 0x26fa], // ⛺
  [0x26fd, 0x26fd], // ⛽
  [0x2705, 0x2705], // ✅
  [0x270a, 0x270b], // ✊ ✋
  [0x2728, 0x2728], // ✨
  [0x274c, 0x274c], // ❌
  [0x274e, 0x274e], // ❎
  [0x2753, 0x2755], // ❓ ❔ ❕
  [0x2757, 0x2757], // ❗
  [0x2795, 0x2797], // ➕ ➖ ➗
  [0x27b0, 0x27b0], // ➰
  [0x27bf, 0x27bf], // ➿
  [0x2b1b, 0x2b1c], // ⬛ ⬜
  [0x2b50, 0x2b50], // ⭐
  [0x2b55, 0x2b55], // ⭕
  [0x1f000, 0x1f0ff], // mahjong / domino / playing cards
  [0x1f100, 0x1f1ff], // enclosed alphanumerics incl. regional indicators
  [0x1f200, 0x1f2ff], // enclosed ideographic
  [0x1f300, 0x1f5ff], // misc symbols + pictographs (weather, transport…)
  [0x1f600, 0x1f64f], // emoticons
  [0x1f680, 0x1f6ff], // transport + map symbols
  [0x1f700, 0x1f77f], // alchemical
  [0x1f780, 0x1f7ff], // geometric shapes
  [0x1f800, 0x1f8ff], // supplemental arrows
  [0x1f900, 0x1f9ff], // supplemental symbols + pictographs
  [0x1fa70, 0x1faff], // symbols and pictographs extended-A
];

export function isColorGlyph(codepoint: number): boolean {
  let lo = 0;
  let hi = COLOR_RANGES.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [start, end] = COLOR_RANGES[mid];
    if (codepoint < start) {
      hi = mid - 1;
    } else if (codepoint > end) {
      lo = mid + 1;
    } else {
      return true;
    }
  }
  return false;
}

// --- GlyphRaster ----------------------------------------------------------
/* v8 ignore start -- browser-only Canvas2D shell, host-verified (no canvas in jsdom) */

export interface GlyphRasterOptions {
  family: string;
  /** CSS px. */
  fontSize: number;
  /** CSS px. */
  cellW: number;
  /** CSS px. */
  cellH: number;
  /** CSS px from the cell top to the alphabetic baseline. */
  baseline: number;
  dpr: number;
  pageSize?: number;
}

export interface RasterResult {
  /** True when the glyph carries its own colors — skip shader tinting. */
  colored: boolean;
  /** Dirty rect in device px — the exact region to texSubImage2D. */
  dirty: { x: number; y: number; w: number; h: number };
}

export function fontShorthand(
  family: string,
  fontSize: number,
  dpr: number,
  bold = false,
  italic = false,
): string {
  return `${italic ? "italic" : "normal"} ${bold ? "700" : "400"} ${fontSize * dpr}px ${cssFontFamily(family)}`;
}

/** A family value safe to interpolate into CSS `font-family` or a canvas
 *  font shorthand: bare names get quoted; anything already carrying quotes
 *  or a comma (a font STACK like `"Geist Mono", "JetBrains Mono", monospace`)
 *  passes through untouched. Double-wrapping a stack in quotes makes the
 *  whole declaration invalid CSS, which silently drops it — a
 *  real run measured cells in the body's proportional font and drew the
 *  terminal with huge letter gaps. */
export function cssFontFamily(family: string): string {
  return /[,"']/.test(family) ? family : `"${family}"`;
}

/**
 * Rasterizes glyphs into offscreen page canvases via Canvas2D fillText.
 * Output is the canvas itself plus the dirty rect, so the WebGL renderer can
 * texSubImage2D straight from the page.
 */
export class GlyphRaster {
  private readonly family: string;
  private readonly fontSize: number;
  private readonly cellW: number;
  private readonly cellH: number;
  private readonly baseline: number;
  private readonly dpr: number;
  private readonly pageSize: number;
  private readonly pages: HTMLCanvasElement[] = [];

  constructor(opts: GlyphRasterOptions) {
    this.family = opts.family;
    this.fontSize = opts.fontSize;
    this.cellW = opts.cellW;
    this.cellH = opts.cellH;
    this.baseline = opts.baseline;
    this.dpr = opts.dpr;
    this.pageSize = opts.pageSize ?? ATLAS_DEFAULT_PAGE_SIZE;
  }

  /** The page canvas, created on first use. */
  getPage(page: number): HTMLCanvasElement {
    return this.context(page).canvas;
  }

  /**
   * Draw one glyph into its slot. Draws at integer pixel origins (slot origin
   * plus the 1px pad) with an alphabetic baseline; wide glyphs fill the
   * two-cell slot. A glyph the font cannot produce (zero advance) is drawn
   * as a procedural notdef box rather than trusting fillText's fallback.
   */
  rasterize(
    slot: AtlasSlot,
    codepoint: number,
    wide: boolean,
    bold = false,
    italic = false,
  ): RasterResult {
    const { ctx } = this.context(slot.page);
    const { w, h } = slotDims(this.cellW, this.cellH, this.dpr, wide);
    const glyphW = w - 2 * ATLAS_SLOT_PAD;
    const glyphH = h - 2 * ATLAS_SLOT_PAD;
    const ox = slot.x + ATLAS_SLOT_PAD;
    const oy = slot.y + ATLAS_SLOT_PAD;
    const dirty = { x: slot.x, y: slot.y, w, h };

    // Always start from a blank slot: fillText composites over whatever the
    // slot last held, and any reuse path that redraws without a wipe
    // composites two glyphs into one (a stale `─` under an F reads as a
    // struck-through letter — host-run artifact).
    ctx.clearRect(slot.x, slot.y, w, h);

    ctx.fillStyle = "#fff"; // text glyphs tinted by the shader; color glyphs ignore it
    ctx.font = fontShorthand(this.family, this.fontSize, this.dpr, bold, italic);
    ctx.textBaseline = "alphabetic";

    // Block elements are drawn geometrically, never from the font — exact
    // full-bleed rects tile seamlessly across cells (the bar-slab look).
    const blocks = blockElementRects(codepoint, glyphW, glyphH);
    if (blocks) {
      for (const r of blocks) {
        ctx.globalAlpha = r.alpha;
        ctx.fillRect(ox + r.x, oy + r.y, r.w, r.h);
      }
      ctx.globalAlpha = 1;
      return { colored: false, dirty };
    }

    if (codepoint > 0x10ffff || (codepoint >= 0xd800 && codepoint <= 0xdfff)) {
      return this.notdef(ctx, ox, oy, glyphW, glyphH, slot, dirty);
    }

    const text = String.fromCodePoint(codepoint);
    const advance = ctx.measureText(text).width;
    if (advance === 0) {
      return this.notdef(ctx, ox, oy, glyphW, glyphH, slot, dirty);
    }

    if (advance > glyphW + 0.5) {
      // A fallback glyph the mono font lacks (Claude Code's ✻/✽ spinners in
      // a symbol font) can be wider than the cell; the slot would clip it to
      // a permanently half-drawn glyph (host-run). Scale-to-fit
      // around the baseline instead — the kitty behavior.
      const s = glyphW / advance;
      ctx.save();
      ctx.translate(ox, oy + this.baseline * this.dpr);
      ctx.scale(s, s);
      ctx.fillText(text, 0, 0);
      ctx.restore();
    } else {
      ctx.fillText(text, ox, oy + this.baseline * this.dpr);
    }
    return { colored: isColorGlyph(codepoint), dirty };
  }

  /** Wipe a page canvas — call after an atlas eviction (version bump). */
  clearPage(page: number): void {
    const canvas = this.pages[page];
    if (canvas) {
      canvas.getContext("2d")?.clearRect(0, 0, this.pageSize, this.pageSize);
    }
  }

  /** Drop all page canvases (DPR/font change). */
  reset(): void {
    this.pages.length = 0;
  }

  private notdef(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    w: number,
    h: number,
    slot: AtlasSlot,
    dirty: { x: number; y: number; w: number; h: number },
  ): RasterResult {
    ctx.clearRect(slot.x, slot.y, slot.w, slot.h);
    ctx.strokeStyle = "#fff";
    ctx.lineWidth = Math.max(1, Math.round(this.dpr));
    ctx.strokeRect(x + 0.5, y + 0.5, w - 1, h - 1);
    ctx.beginPath();
    ctx.moveTo(x, y);
    ctx.lineTo(x + w, y + h);
    ctx.moveTo(x + w, y);
    ctx.lineTo(x, y + h);
    ctx.stroke();
    return { colored: false, dirty };
  }

  private context(page: number): { canvas: HTMLCanvasElement; ctx: CanvasRenderingContext2D } {
    let canvas = this.pages[page];
    if (!canvas) {
      canvas = document.createElement("canvas");
      canvas.width = this.pageSize;
      canvas.height = this.pageSize;
      this.pages[page] = canvas;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("GlyphRaster: Canvas2D unavailable");
    return { canvas, ctx };
  }
}

/* v8 ignore stop */
