// Shared renderer interface.
//
// Both renderers implement this: the DOM renderer (the permanent
// correctness oracle) and the WebGL2 renderer. The panel owns the
// retained CellStore — `decodeFrame(buf, store)` applies each delta onto it in
// place — and passes the store alongside every frame, so a renderer must be a
// pure function of (frame, cells): same input in → same picture out. Rendering
// state a renderer keeps across frames (row caches, glyph atlas, instance
// buffers) is an internal optimization; it must never change what the frame
// says. Dirty-row incremental updates are exactly that kind of optimization:
// the panel's deltas only carry the damaged spans, so a renderer may repaint
// just those rows as long as the untouched rows keep showing their accumulated
// store content.

import type { CellStore, Frame } from "./protocol";

/** Cell/font metrics a renderer sizes itself to. All lengths in CSS px unless
 *  noted; the canvas backing store scales by `dpr`. */
export interface RendererMetrics {
  /** CSS px per cell. */
  cellW: number;
  /** CSS px per cell. */
  cellH: number;
  /** Device pixel ratio the backing store is scaled by. */
  dpr: number;
  /** CSS px from the cell top to the alphabetic baseline (glyph raster). */
  baseline: number;
  fontFamily: string;
  fontSize: number;
  /** CSS px added per character so DOM text advances land on the quantized
   *  cell grid (cellW − the font's natural advance; may be negative). Only
   *  the DOM renderer consumes it — GL positions quads on cellW directly. */
  letterSpacing?: number;
}

export interface TermRenderer {
  /**
   * Paint one decoded frame. `cells` is the panel's retained store: the
   * frame's damaged spans have already been applied onto it. Defaults to
   * `frame.cells`, which is the same object for panel-delivered frames (the
   * decoder writes in place), so `apply(frame)` is always valid.
   */
  apply(frame: Frame, cells?: CellStore): void;
  /** Font/cell metrics changed (font load, DPR change, resize): repaint. */
  setMetrics(m: RendererMetrics): void;
  /** Terminal focus state; block cursors render hollow when unfocused. */
  focus(focused: boolean): void;
  /** Release DOM/GPU resources and detach from the container. */
  dispose(): void;
}
