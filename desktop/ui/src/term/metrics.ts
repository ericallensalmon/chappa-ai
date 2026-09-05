// Cell metrics measured from a loaded web font.
//
// All measurements happen on a probe Canvas2D context whose font is set at
// `fontSize * dpr` px — the same scale GlyphRaster draws at — and are
// returned in CSS px (divided back down by dpr) so panel.ts can
// size the grid directly. The atlas rounds up to device px internally.
import { fontShorthand } from "./atlas";

export interface CellMetrics {
  /** CSS px. */
  cellW: number;
  /** CSS px. */
  cellH: number;
  /** CSS px from the cell top to the alphabetic baseline. */
  baseline: number;
  dpr: number;
}

export const DEFAULT_FONT_SIZE = 14;

/**
 * Wait for the face to be actually loaded before measuring, so the numbers
 * reflect the real font and not the generic fallback.
 * `document.fonts.load` resolves once the face is ready; if fonts are
 * unavailable we measure whatever the engine currently resolves.
 */
async function ensureFontLoaded(family: string, fontSize: number, dpr: number): Promise<void> {
  try {
    await document.fonts.load(`${fontSize * dpr}px ${family}`, "Mg");
  } catch {
    // document.fonts is not guaranteed everywhere — proceed anyway.
  }
}

function probeContext(): CanvasRenderingContext2D {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("measureCellMetrics: Canvas2D unavailable");
  return ctx;
}

function round3(x: number): number {
  return Math.round(x * 1000) / 1000;
}

function effectiveDpr(dpr?: number): number {
  return dpr ?? (typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1);
}

/**
 * Measure monospace cell geometry. Width is the advance of "M" (every glyph
 * shares it in a mono font); height and baseline come from the
 * actualBoundingBox ascent/descent of "Mg" — the tallest/deepest pair in
 * normal text. Fallbacks apply when actualBoundingBox is unavailable.
 */
export async function measureCellMetrics(
  family: string,
  fontSize = DEFAULT_FONT_SIZE,
  dpr?: number,
): Promise<CellMetrics> {
  const d = effectiveDpr(dpr);
  await ensureFontLoaded(family, fontSize, d);
  const ctx = probeContext();
  ctx.font = fontShorthand(family, fontSize, d);

  const box = ctx.measureText("Mg");
  const ascent = (box.actualBoundingBoxAscent || fontSize * 0.8) / d;
  const descent = (box.actualBoundingBoxDescent || fontSize * 0.2) / d;

  return {
    cellW: round3(ctx.measureText("M").width / d),
    cellH: round3(ascent + descent),
    baseline: round3(ascent),
    dpr: d,
  };
}

/**
 * Host-verification helper (acceptance): the advance width of any
 * codepoint in CSS px, for checking that a wide glyph's measureText width
 * really equals 2 × cellW with the bundled font.
 */
export async function measureGlyph(
  family: string,
  codepoint: number,
  fontSize = DEFAULT_FONT_SIZE,
  dpr?: number,
): Promise<number> {
  const d = effectiveDpr(dpr);
  await ensureFontLoaded(family, fontSize, d);
  const ctx = probeContext();
  ctx.font = fontShorthand(family, fontSize, d);
  return round3(ctx.measureText(String.fromCodePoint(codepoint)).width / d);
}
