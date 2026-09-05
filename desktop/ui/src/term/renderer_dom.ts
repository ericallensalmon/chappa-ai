// DOM renderer (correctness oracle).
//
// Renders a decoded `Frame` into a `<pre class="term">` of row divs whose
// children are coalesced `<span>` runs (adjacent cells with identical style
// share one span). Only rows listed in the frame's dirty spans are rebuilt —
// row nodes stay keyed by absolute viewport row so untouched rows keep their
// DOM. The cursor, selection/search overlays and the scrollbar are absolutely
// positioned layers over the pre.
//
// The renderer keeps its OWN cell buffer (a persistent CellStore) and copies
// the frame's covered cells into it, because delta frames only carry the
// damaged spans: repainting a whole row reads accumulated state, so a partial
// row patch doesn't clobber the untouched columns.
//
// Wide chars occupy two columns naturally (the mono font's advance for the
// glyph is expected to be 2 cells; the following spacer cell emits nothing);
// zero-width combiners are appended to their base cell's text.
//
// Cell colors arrive fully resolved (palette lookups happen Rust-side);
// `inverse` is already applied to the colors, so the DOM never double-inverts.

import { CELL_FLAGS, type Frame, type Range, type CellStore } from "./protocol";
import { cssFontFamily } from "./atlas";
import type { RendererMetrics, TermRenderer } from "./renderer";

export interface DomRendererOptions {
  /** The container is turned into the terminal viewport. */
  container: HTMLElement;
  /** CSS px per cell; measured from the loaded font. */
  cellW: number;
  cellH: number;
  fontFamily: string;
  /** CSS px font size. */
  fontSize: number;
  /**
   * Packed RGBA of the terminal's default background (0xRRGGBBAA). Cells
   * whose bg equals this skip the inline background so the pre's CSS shows
   * through; keeps uniform rows a single span. Defaults to the xterm-256color
   * default bg (the Rust DEFAULT_PALETTE slot 257, 0x181818).
   */
  defaultBg?: number;
  /** Packed RGBA default foreground; cells matching it still get color set
   *  (the pre text color is the fallback for genuinely empty cells). */
  cursorColor?: string;
  dimOpacity?: number;
  /** Thumb drag / track click on the scrollbar. Offset is an absolute
   *  scrollback position in lines. */
  onScrollbar?: (offset: number) => void;
}

export const TERM_DEFAULT_BG = 0x181818ff;

const CURSOR_DEFAULT = "#ffffff";

/** styles applied once to the whole document (idempotent) — the DOM
 *  renderer's own chrome, independent of any app stylesheet. */
const TERM_CSS = `
.chappa-term-viewport{position:relative;overflow:hidden;background:#181818;}
.chappa-term-viewport .term{position:absolute;inset:0;margin:0;white-space:pre;overflow:hidden;}
.chappa-term-viewport .term-row{position:absolute;left:0;right:0;white-space:pre;height:var(--chappa-cell-h,14px);line-height:var(--chappa-cell-h,14px);}
.chappa-term-viewport .term-row span{display:inline;white-space:pre;}
.chappa-term-viewport .cursor{position:absolute;top:0;left:0;background:#fff;mix-blend-mode:difference;pointer-events:none;will-change:transform;}
.chappa-term-viewport .cursor.hollow{background:transparent;mix-blend-mode:normal;box-sizing:border-box;}
.chappa-term-viewport .cursor.blink{animation:chappa-cursor-blink 1.06s steps(1) infinite;}
@keyframes chappa-cursor-blink{50%{opacity:0;}}
.chappa-term-overlay{position:absolute;top:0;left:0;pointer-events:none;}
.chappa-term-overlay .term-sel{position:absolute;background:var(--chappa-selection-bg,rgba(94,166,255,.3));}
.chappa-term-overlay .term-match{position:absolute;background:var(--chappa-match-bg,rgba(255,191,0,.28));}
.chappa-term-pill{position:absolute;top:4px;right:14px;padding:1px 8px;border-radius:10px;background:#2a2c31;color:#9aa0aa;font:11px ui-monospace,monospace;pointer-events:none;}
.chappa-term-scrollbar{position:absolute;top:0;right:0;width:10px;bottom:0;background:transparent;}
.chappa-term-scrollbar:hover{background:rgba(255,255,255,.04);}
.chappa-term-scrollbar .thumb{position:absolute;left:2px;right:2px;border-radius:4px;background:rgba(255,255,255,.22);cursor:pointer;}
.chappa-term-scrollbar .thumb:hover{background:rgba(255,255,255,.35);}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = TERM_CSS;
  document.head.appendChild(el);
}

/** Renderer-local style decision for one cell; equal values coalesce. */
interface RunStyle {
  color: string;
  bg: string | null;
  bold: boolean;
  italic: boolean;
  dim: boolean;
  /** CSS text-decoration-line value ("" when none). */
  decorLine: string;
  /** CSS text-decoration-style value ("" when none). */
  decorStyle: string;
  hidden: boolean;
}

function rgbaStr(packed: number): string {
  const r = (packed >>> 24) & 0xff;
  const g = (packed >>> 16) & 0xff;
  const b = (packed >>> 8) & 0xff;
  const a = packed & 0xff;
  return `rgba(${r},${g},${b},${a / 255})`;
}

function styleFromFlags(fg: number, bg: number, flags: number, opts: DomRendererOptions): RunStyle {
  let decorLine = "";
  let decorStyle = "";
  if (flags & CELL_FLAGS.underline) decorLine = "underline";
  if (flags & CELL_FLAGS.strikeout) decorLine = (decorLine ? `${decorLine} ` : "") + "line-through";
  if (flags & CELL_FLAGS.doubleUnderline) decorStyle = "double";
  else if (flags & CELL_FLAGS.undercurl) decorStyle = "wavy";
  else if (flags & CELL_FLAGS.dottedUnderline) decorStyle = "dotted";
  else if (flags & CELL_FLAGS.dashedUnderline) decorStyle = "dashed";
  else if (flags & CELL_FLAGS.underline) decorStyle = "solid";

  return {
    color: rgbaStr(fg),
    bg: bg === (opts.defaultBg ?? TERM_DEFAULT_BG) ? null : rgbaStr(bg),
    bold: (flags & CELL_FLAGS.bold) !== 0,
    italic: (flags & CELL_FLAGS.italic) !== 0,
    dim: (flags & CELL_FLAGS.dim) !== 0,
    decorLine,
    decorStyle,
    hidden: (flags & CELL_FLAGS.hidden) !== 0,
  };
}

function applyStyle(span: HTMLSpanElement, s: RunStyle, opts: DomRendererOptions): void {
  span.style.color = s.color;
  if (s.bg !== null) span.style.backgroundColor = s.bg;
  if (s.bold) span.style.fontWeight = "700";
  if (s.italic) span.style.fontStyle = "italic";
  if (s.dim) span.style.opacity = String(opts.dimOpacity ?? 0.6);
  if (s.decorLine) span.style.textDecorationLine = s.decorLine;
  if (s.decorStyle) span.style.textDecorationStyle = s.decorStyle;
  if (s.hidden) span.style.visibility = "hidden";
}

/** A horizontal strip the overlay layers draw. */
export interface OverlayRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export class DomRenderer implements TermRenderer {
  readonly container: HTMLElement;
  private readonly opts: DomRendererOptions;
  private pre: HTMLPreElement;
  private cursorEl: HTMLDivElement;
  private overlay: HTMLDivElement;
  private pill: HTMLDivElement;
  private scrollbar: HTMLDivElement;
  private thumb: HTMLDivElement;

  private store: CellStore = { cols: 0, rows: 0, ch: new Uint32Array(0), fg: new Uint32Array(0), bg: new Uint32Array(0), flags: new Uint16Array(0), link: new Uint16Array(0) };
  private rowEls: (HTMLDivElement | null)[] = [];
  private cellW: number;
  private cellH: number;
  private displayOffset = 0;
  private historyLen = 0;
  private scrollDrag = false;
  /** Terminal focus; an unfocused block cursor renders hollow. */
  private focused = true;
  /** Last frame, kept so focus() can repaint the cursor immediately. */
  private lastFrame: Frame | null = null;

  constructor(opts: DomRendererOptions) {
    this.opts = opts;
    this.cellW = opts.cellW;
    this.cellH = opts.cellH;
    injectStyles();

    const { container } = opts;
    this.container = container;
    container.classList.add("chappa-term-viewport");
    container.style.setProperty("--chappa-cell-h", `${opts.cellH}px`);
    container.textContent = "";

    this.pre = document.createElement("pre");
    this.pre.className = "term";
    this.pre.style.fontFamily = `${cssFontFamily(opts.fontFamily)}, monospace`;
    this.pre.style.fontSize = `${opts.fontSize}px`;

    this.cursorEl = document.createElement("div");
    this.cursorEl.className = "cursor";
    this.cursorEl.style.display = "none";

    this.overlay = document.createElement("div");
    this.overlay.className = "chappa-term-overlay";

    this.pill = document.createElement("div");
    this.pill.className = "chappa-term-pill";
    this.pill.style.display = "none";
    this.pill.textContent = "⏶ scrolled";

    this.scrollbar = document.createElement("div");
    this.scrollbar.className = "chappa-term-scrollbar";
    this.thumb = document.createElement("div");
    this.thumb.className = "thumb";
    this.scrollbar.appendChild(this.thumb);

    container.append(this.pre, this.cursorEl, this.overlay, this.pill, this.scrollbar);
    this.bindScrollbar();
  }

  /** Font/metrics changed (ResizeObserver); repaint everything next frame. */
  setCellMetrics(cellW: number, cellH: number): void {
    this.cellW = cellW;
    this.cellH = cellH;
    this.container.style.setProperty("--chappa-cell-h", `${cellH}px`);
    // Drop the row cache AND its DOM: orphaned divs would keep painting the
    // old content on top of (or displacing) everything rebuilt after them.
    this.pre.textContent = "";
    this.rowEls = [];
    this.store = { cols: 0, rows: 0, ch: new Uint32Array(0), fg: new Uint32Array(0), bg: new Uint32Array(0), flags: new Uint16Array(0), link: new Uint16Array(0) };
  }

  /** The row div for a viewport row, if it exists (tests query this). */
  rowElement(row: number): HTMLDivElement | null {
    return this.rowEls[row] ?? null;
  }

  render(frame: Frame): void {
    this.lastFrame = frame;
    this.displayOffset = frame.displayOffset;
    this.historyLen = frame.historyLen;

    this.ensureGrid(frame.cells.cols, frame.cells.rows);
    this.copyCells(frame);
    this.buildZerowidth(frame.zerowidth);
    for (const span of frame.rows) {
      this.rebuildRow(span.row);
    }

    this.renderCursor(frame);
    this.renderOverlays(frame);
    this.pill.style.display = frame.displayOffset > 0 ? "block" : "none";
    this.renderScrollbar();
  }

  dispose(): void {
    this.container.textContent = "";
    this.container.classList.remove("chappa-term-viewport");
    this.rowEls = [];
  }

  // --- TermRenderer interface ----------------------------------------------

  /** The shared interface entry point. The panel passes its retained store,
   *  but for panel-delivered frames `cells === frame.cells` (decodeFrame
   *  writes in place), so this is exactly `render(frame)` — and the DOM
   *  renderer keeps its own cell buffer anyway as the self-contained
   *  correctness oracle (see the copyCells doc comment). */
  apply(frame: Frame, _cells?: CellStore): void {
    this.render(frame);
  }

  /** Font/cell metrics changed: same path as the old setCellMetrics, plus
   *  the letter-spacing that lands natural text advances on the quantized
   *  cell grid (panel.probeCellMetrics; an xterm.js-derived technique) —
   *  without it DOM glyph positions drift off the shared grid the cursor,
   *  selection, and mouse math use. */
  setMetrics(m: RendererMetrics): void {
    if (m.letterSpacing !== undefined) {
      this.pre.style.letterSpacing = `${m.letterSpacing}px`;
    }
    this.setCellMetrics(m.cellW, m.cellH);
  }

  /** Track focus so a block cursor hollows out when the panel blurs. */
  focus(focused: boolean): void {
    if (focused === this.focused) return;
    this.focused = focused;
    // The cursor only changes with frames, so repaint the last one now —
    // the actor sends nothing when the webview merely loses focus.
    if (this.lastFrame) this.renderCursor(this.lastFrame);
  }

  // --- grid / rows ---------------------------------------------------------

  private ensureGrid(cols: number, rows: number): void {
    if (cols === this.store.cols && rows === this.store.rows) return;
    this.store = {
      cols,
      rows,
      ch: new Uint32Array(cols * rows),
      fg: new Uint32Array(cols * rows),
      bg: new Uint32Array(cols * rows),
      flags: new Uint16Array(cols * rows),
      link: new Uint16Array(cols * rows),
    };
    // Resize implies a full frame follows in Rust; drop cached rows AND
    // their DOM so stale content doesn't survive (or displace) the rebuild.
    this.pre.textContent = "";
    this.rowEls = [];
  }

  private copyCells(frame: Frame): void {
    const src = frame.cells;
    const { cols } = this.store;
    for (const span of frame.rows) {
      const base = span.row * cols + span.colStart;
      const sbase = span.row * src.cols + span.colStart;
      for (let j = 0; j < span.cellCount; j++) {
        const d = base + j;
        const s = sbase + j;
        this.store.ch[d] = src.ch[s];
        this.store.fg[d] = src.fg[s];
        this.store.bg[d] = src.bg[s];
        this.store.flags[d] = src.flags[s];
        this.store.link[d] = src.link[s];
      }
    }
  }

  /** Rebuild one row div from the accumulated store. Only called for rows in
   *  the frame's dirty spans. */
  private rebuildRow(row: number): void {
    let el = this.rowEls[row];
    if (!el) {
      el = document.createElement("div");
      el.className = "term-row";
      el.dataset.row = String(row);
      // Rows are absolutely positioned by index: delta frames create row divs
      // lazily and out of order, so document order can never be the layout.
      el.style.top = `${row * this.cellH}px`;
      this.pre.appendChild(el);
      this.rowEls[row] = el;
    }

    const frag = document.createDocumentFragment();
    const base = row * this.store.cols;
    let current: { span: HTMLSpanElement; style: RunStyle; text: string } | null = null;

    const flush = (): void => {
      if (current) {
        current.span.textContent = current.text;
        frag.appendChild(current.span);
        current = null;
      }
    };

    for (let col = 0; col < this.store.cols; col++) {
      const idx = base + col;
      if (this.store.flags[idx] & CELL_FLAGS.wideSpacer) continue;
      const s = styleFromFlags(this.store.fg[idx], this.store.bg[idx], this.store.flags[idx], this.opts);
      if (!current || !styleEquals(s, current.style)) {
        flush();
        const span = document.createElement("span");
        applyStyle(span, s, this.opts);
        current = { span, style: s, text: "" };
      }
      current.text += this.cellText(idx, row, col);
    }
    flush();

    el.textContent = "";
    el.appendChild(frag);
  }

  private cellText(idx: number, row: number, col: number): string {
    let ch = this.store.ch[idx];
    if (ch === 0) ch = 0x20;
    let s = String.fromCodePoint(ch);
    const zw = this.zerowidth.get(row);
    if (zw && zw.has(col)) {
      for (const cp of zw.get(col)!) s += String.fromCodePoint(cp);
    }
    return s;
  }

  /** Zero-width chars for the current frame, keyed row → col → chars. Built
   *  before rows are painted so combiners land on their base cells. */
  private zerowidth = new Map<number, Map<number, number[]>>();

  private buildZerowidth(entries: Frame["zerowidth"]): void {
    this.zerowidth = new Map();
    for (const zw of entries) {
      let rowMap = this.zerowidth.get(zw.row);
      if (!rowMap) {
        rowMap = new Map();
        this.zerowidth.set(zw.row, rowMap);
      }
      rowMap.set(zw.col, Array.from(zw.chars));
    }
  }

  // --- overlays ------------------------------------------------------------

  private renderCursor(frame: Frame): void {
    const { cursor } = frame;
    const visible = cursor.visible && cursor.shape !== "hidden";
    this.cursorEl.style.display = visible ? "block" : "none";
    if (!visible) return;

    const x = cursor.col * this.cellW;
    let y = cursor.row * this.cellH;
    let w = this.cellW;
    let h = this.cellH;
    let hollow = false;

    switch (cursor.shape) {
      case "beam":
        w = 2;
        break;
      case "underline":
        h = Math.max(2, Math.round(this.cellH * 0.12));
        y += this.cellH - h;
        break;
      case "hollow":
        hollow = true;
        break;
      case "block":
      default:
        // Unfocused block cursor hollows out (terminal convention); the
        // shared interface's focus() feeds this.
        if (!this.focused) hollow = true;
        break;
    }

    this.cursorEl.classList.toggle("hollow", hollow);
    // Focused solid cursors blink (terminal convention); an
    // unfocused hollow cursor holds steady.
    this.cursorEl.classList.toggle("blink", this.focused && !hollow);
    this.cursorEl.style.transform = `translate(${x}px, ${y}px)`;
    this.cursorEl.style.width = `${w}px`;
    this.cursorEl.style.height = `${h}px`;
    if (hollow) {
      this.cursorEl.style.background = "transparent";
      this.cursorEl.style.border = `1px solid ${this.opts.cursorColor ?? CURSOR_DEFAULT}`;
      this.cursorEl.style.mixBlendMode = "normal";
    } else {
      this.cursorEl.style.background = this.opts.cursorColor ?? CURSOR_DEFAULT;
      this.cursorEl.style.border = "none";
      this.cursorEl.style.mixBlendMode = "difference";
    }
  }

  private renderOverlays(frame: Frame): void {
    this.overlay.textContent = "";
    const frag = document.createDocumentFragment();
    if (frame.selection) {
      for (const rect of this.rangeRects(frame.selection)) {
        const el = document.createElement("div");
        el.className = "term-sel";
        this.placeRect(el, rect);
        frag.appendChild(el);
      }
    }
    for (const match of frame.matches) {
      for (const rect of this.rangeRects(match)) {
        const el = document.createElement("div");
        el.className = "term-match";
        this.placeRect(el, rect);
        frag.appendChild(el);
      }
    }
    this.overlay.appendChild(frag);
  }

  /** Inclusive start/end range → per-row overlay rects (exclusive end col). */
  private rangeRects(range: Range): OverlayRect[] {
    const rects: OverlayRect[] = [];
    const rows = this.store.rows;
    if (rows === 0) return rects;
    const startRow = Math.max(0, Math.min(range.startRow, rows - 1));
    const endRow = Math.max(0, Math.min(range.endRow, rows - 1));
    for (let r = startRow; r <= endRow; r++) {
      const startCol = r === startRow ? range.startCol : 0;
      const endCol = r === endRow ? range.endCol + 1 : this.store.cols; // exclusive
      if (endCol <= startCol) continue;
      rects.push({
        left: startCol * this.cellW,
        top: r * this.cellH,
        width: (endCol - startCol) * this.cellW,
        height: this.cellH,
      });
    }
    return rects;
  }

  private placeRect(el: HTMLDivElement, rect: OverlayRect): void {
    el.style.left = `${rect.left}px`;
    el.style.top = `${rect.top}px`;
    el.style.width = `${rect.width}px`;
    el.style.height = `${rect.height}px`;
  }

  // --- scrollbar -----------------------------------------------------------

  private renderScrollbar(): void {
    const { rows } = this.store;
    if (rows === 0 || this.historyLen === 0) {
      this.thumb.style.display = "none";
      return;
    }
    this.thumb.style.display = "block";
    const trackH = rows * this.cellH;
    const total = rows + this.historyLen;
    const thumbH = Math.max(24, (rows / total) * trackH);
    const travel = Math.max(1, trackH - thumbH);
    // displayOffset counts up from the bottom (0 = live/newest); the thumb's
    // top counts down from the top (0 = oldest history). Invert.
    const pos = ((this.historyLen - this.displayOffset) / this.historyLen) * travel;
    this.thumb.style.height = `${thumbH}px`;
    this.thumb.style.top = `${Math.max(0, Math.min(trackH - thumbH, pos))}px`;
  }

  private bindScrollbar(): void {
    const offsetFromEvent = (clientY: number): number => {
      const { rows } = this.store;
      if (rows === 0 || this.historyLen === 0) return 0;
      const trackH = rows * this.cellH;
      const rect = this.scrollbar.getBoundingClientRect();
      const frac = Math.min(1, Math.max(0, (clientY - rect.top) / trackH));
      // Track top = oldest history (max offset), bottom = live (offset 0).
      return Math.round((1 - frac) * this.historyLen);
    };

    this.thumb.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      this.scrollDrag = true;
      this.thumb.setPointerCapture(e.pointerId);
      this.opts.onScrollbar?.(offsetFromEvent(e.clientY));
    });
    this.thumb.addEventListener("pointermove", (e) => {
      if (!this.scrollDrag) return;
      this.opts.onScrollbar?.(offsetFromEvent(e.clientY));
    });
    this.thumb.addEventListener("pointerup", (e) => {
      this.scrollDrag = false;
      try {
        this.thumb.releasePointerCapture(e.pointerId);
      } catch {
        /* pointer already released */
      }
    });
    this.scrollbar.addEventListener("click", (e) => {
      if (e.target === this.thumb) return;
      this.opts.onScrollbar?.(offsetFromEvent(e.clientY));
    });
  }
}

function styleEquals(a: RunStyle, b: RunStyle): boolean {
  return (
    a.color === b.color &&
    a.bg === b.bg &&
    a.bold === b.bold &&
    a.italic === b.italic &&
    a.dim === b.dim &&
    a.decorLine === b.decorLine &&
    a.decorStyle === b.decorStyle &&
    a.hidden === b.hidden
  );
}
