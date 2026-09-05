// Stage-by-stage WebGL renderer debug harness — open /gldebug.html on the
// vite dev server in any browser (no Tauri needed). Each stage prints its
// own pass/fail evidence so a screenshot of the page is a full bug report.

import {
  AtlasIndex,
  GlyphRaster,
  packKey,
  ATLAS_DEFAULT_PAGE_SIZE,
  ATLAS_DEFAULT_MAX_PAGES,
} from "./term/atlas";
import { WebGLRenderer, writeGlyphCells, GLYPH_FLOATS, type GlyphMiss } from "./term/renderer_webgl";
import { CELL_FLAGS, type CellStore, type Frame } from "./term/protocol";

const CELL_W = 9;
const CELL_H = 18;
const BASELINE = 14;
const DPR = 1;
const FONT = "monospace";
const FONT_SIZE = 14;
const COLS = 40;
const ROWS = 5;

function out(id: string, text: string, bad = false): void {
  const el = document.getElementById(id)!;
  const line = document.createElement("span");
  line.className = bad ? "bad" : "ok";
  line.textContent = text + "\n";
  el.appendChild(line);
}

function makeStore(): CellStore {
  const n = COLS * ROWS;
  const store: CellStore = {
    cols: COLS,
    rows: ROWS,
    ch: new Uint32Array(n),
    fg: new Uint32Array(n),
    bg: new Uint32Array(n),
    flags: new Uint16Array(n),
    link: new Uint16Array(n),
  };
  store.fg.fill(0xd6d6d6ff);
  const put = (row: number, text: string, flags = 0): void => {
    let col = 0;
    for (const c of text) {
      const idx = row * COLS + col;
      store.ch[idx] = c.codePointAt(0)!;
      store.flags[idx] = flags;
      col++;
    }
  };
  put(0, "Hello WebGL renderer 0123456789");
  put(1, "underlined text", CELL_FLAGS.underline);
  put(2, "undercurled text", CELL_FLAGS.undercurl);
  put(3, "bold text", CELL_FLAGS.bold);
  // Wide CJK pair on row 4: wide cell + spacer.
  store.ch[4 * COLS] = "你".codePointAt(0)!;
  store.flags[4 * COLS] = CELL_FLAGS.wide;
  store.flags[4 * COLS + 1] = CELL_FLAGS.wideSpacer;
  // One red-bg cell so the bg pass shows up.
  store.bg[7] = 0xcc2222ff;
  return store;
}

function makeFrame(cells: CellStore): Frame {
  return {
    seq: 1,
    kind: "full",
    cursor: { row: 0, col: 5, shape: "block", visible: true },
    displayOffset: 0,
    historyLen: 0,
    selection: null,
    selectionActive: false,
    mouseCapture: false,
    altScreen: false,
    matches: [],
    rows: Array.from({ length: ROWS }, (_, r) => ({ row: r, colStart: 0, cellCount: COLS })),
    zerowidth: [],
    cells,
  };
}

// --- stage 1: raster + atlas ------------------------------------------------
function stage1(): { atlas: AtlasIndex; raster: GlyphRaster } {
  const atlas = new AtlasIndex({
    cellW: CELL_W,
    cellH: CELL_H,
    dpr: DPR,
    pageSize: ATLAS_DEFAULT_PAGE_SIZE,
    maxPages: ATLAS_DEFAULT_MAX_PAGES,
  });
  const raster = new GlyphRaster({
    family: FONT,
    fontSize: FONT_SIZE,
    cellW: CELL_W,
    cellH: CELL_H,
    baseline: BASELINE,
    dpr: DPR,
  });
  try {
    for (const c of "HeloWbGrnd你") {
      const cp = c.codePointAt(0)!;
      const wide = cp > 0xff;
      const { slot } = atlas.insert(packKey(cp), wide);
      const res = raster.rasterize(slot, cp, wide);
      out("r1", `'${c}' slot page=${slot.page} x=${slot.x} y=${slot.y} w=${slot.w} h=${slot.h} dirty=${JSON.stringify(res.dirty)}`,
        slot.w <= 2 || slot.h <= 2);
    }
    const page = raster.getPage(0);
    const ctx = page.getContext("2d")!;
    const probe = ctx.getImageData(0, 0, 256, 64);
    let nonzero = 0;
    for (let i = 3; i < probe.data.length; i += 4) if (probe.data[i] > 8) nonzero++;
    out("r1", `page0 top-left 256x64: ${nonzero} px with alpha>8 ${nonzero > 50 ? "(GLYPHS PRESENT)" : "(EMPTY — raster broken)"}`, nonzero <= 50);
    // Show the atlas corner, scaled up.
    const peek = document.createElement("canvas");
    peek.className = "peek";
    peek.width = 256;
    peek.height = 64;
    peek.style.width = "512px";
    peek.style.height = "128px";
    peek.getContext("2d")!.drawImage(page, 0, 0, 256, 64, 0, 0, 256, 64);
    document.getElementById("peek1")!.appendChild(peek);
  } catch (e) {
    out("r1", `THREW: ${e}`, true);
  }
  return { atlas, raster };
}

// --- stage 2: instance builder ----------------------------------------------
function stage2(atlas: AtlasIndex): void {
  try {
    const store = makeStore();
    const buf = new Float32Array(COLS * ROWS * GLYPH_FLOATS);
    const misses: GlyphMiss[] = [];
    const spans = Array.from({ length: ROWS }, (_, r) => ({ row: r, colStart: 0, cellCount: COLS }));
    writeGlyphCells(buf, store, spans, atlas, ATLAS_DEFAULT_PAGE_SIZE, { cellW: CELL_W, cellH: CELL_H, dpr: DPR }, misses);
    out("r2", `misses queued: ${misses.length} (cells with glyphs not yet in the atlas)`);
    const cell = (i: number): string => {
      const o = i * GLYPH_FLOATS;
      const f = Array.from(buf.slice(o, o + GLYPH_FLOATS)).map((v) => +v.toFixed(4));
      return `rect=(${f[0]},${f[1]},${f[2]},${f[3]}) uv=(${f[4]},${f[5]},${f[6]},${f[7]}) page=${f[8]} tint=(${f[9]},${f[10]},${f[11]},${f[12]}) colored=${f[13]}`;
    };
    out("r2", `cell[0] 'H' (pre-inserted): ${cell(0)}`, buf[2] === 0);
    out("r2", `cell[1] 'e' (pre-inserted): ${cell(1)}`, buf[GLYPH_FLOATS + 2] === 0);
    out("r2", `cell[5] ' ' (empty → degenerate expected): ${cell(5)}`);
  } catch (e) {
    out("r2", `THREW: ${e}`, true);
  }
}

// --- stage 3: the full renderer ---------------------------------------------
function stage3(): void {
  const container = document.getElementById("term")!;
  let renderer: WebGLRenderer;
  try {
    renderer = new WebGLRenderer({
      container,
      cellW: CELL_W,
      cellH: CELL_H,
      dpr: DPR,
      baseline: BASELINE,
      fontFamily: FONT,
      fontSize: FONT_SIZE,
    });
  } catch (e) {
    out("r3", `constructor THREW: ${e}`, true);
    return;
  }
  const store = makeStore();
  const frame = makeFrame(store);
  try {
    renderer.apply(frame, store);
  } catch (e) {
    out("r3", `apply THREW: ${e}`, true);
    return;
  }
  // Same-task readback: preserveDrawingBuffer is false, so the buffer is only
  // valid until this task yields — apply() draws synchronously.
  const canvas = container.querySelector("canvas") as HTMLCanvasElement;
  const gl = canvas.getContext("webgl2") as WebGL2RenderingContext;
  const dbg = gl.getExtension("WEBGL_debug_renderer_info");
  out("r3", `GPU: ${dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : "(masked)"}`);
  out("r3", `canvas backing ${canvas.width}x${canvas.height}, css ${canvas.style.width}/${canvas.style.height}`, canvas.width < 10);
  const px = new Uint8Array(canvas.width * canvas.height * 4);
  gl.readPixels(0, 0, canvas.width, canvas.height, gl.RGBA, gl.UNSIGNED_BYTE, px);
  let colored = 0;
  for (let i = 0; i < px.length; i += 4) if (px[i] + px[i + 1] + px[i + 2] > 24) colored++;
  const total = canvas.width * canvas.height;
  out(
    "r3",
    `readPixels: ${colored}/${total} non-dark px — ${colored > total * 0.005 ? "RENDERED CONTENT" : "BLANK (glyph pass dead)"}`,
    colored <= total * 0.005,
  );
  const err = gl.getError();
  out("r3", `gl.getError() = ${err}${err !== 0 ? " (GL ERROR!)" : ""}`, err !== 0);
  // Re-apply inside rAF so the drawn buffer composites and stays visible.
  requestAnimationFrame(() => renderer.apply(frame, store));
}

// --- stage 4: the panel's actual lifecycle ---------------------------------
// The app constructs with cellW/cellH/baseline 0 (metrics not probed yet),
// then calls setMetrics with real values + the display's devicePixelRatio.
function stage4(): void {
  const container = document.createElement("div");
  container.id = "term4";
  container.style.cssText = "position:relative;width:360px;height:90px;background:#181818;margin-top:6px";
  document.getElementById("term")!.after(container);
  let renderer: WebGLRenderer;
  try {
    renderer = new WebGLRenderer({
      container,
      cellW: 0,
      cellH: 0,
      dpr: 1,
      baseline: 0,
      fontFamily: FONT,
      fontSize: FONT_SIZE,
    });
    renderer.setMetrics({
      cellW: CELL_W,
      cellH: CELL_H,
      dpr: window.devicePixelRatio || 1,
      baseline: BASELINE,
      fontFamily: FONT,
      fontSize: FONT_SIZE,
    });
  } catch (e) {
    out("r4", `THREW: ${e}`, true);
    return;
  }
  const store = makeStore();
  const frame = makeFrame(store);
  try {
    renderer.apply(frame, store);
  } catch (e) {
    out("r4", `apply THREW: ${e}`, true);
    return;
  }
  const canvas = container.querySelector("canvas") as HTMLCanvasElement;
  const gl = canvas.getContext("webgl2") as WebGL2RenderingContext;
  out("r4", `dpr=${window.devicePixelRatio} canvas backing ${canvas.width}x${canvas.height}, css ${canvas.style.width}/${canvas.style.height}`, canvas.width < 10);
  const px = new Uint8Array(canvas.width * canvas.height * 4);
  gl.readPixels(0, 0, canvas.width, canvas.height, gl.RGBA, gl.UNSIGNED_BYTE, px);
  let colored = 0;
  for (let i = 0; i < px.length; i += 4) if (px[i] + px[i + 1] + px[i + 2] > 24) colored++;
  const total = canvas.width * canvas.height;
  out(
    "r4",
    `readPixels: ${colored}/${total} non-dark px — ${colored > total * 0.005 ? "RENDERED CONTENT" : "BLANK (panel path broken)"}`,
    colored <= total * 0.005,
  );
  out("r4", `gl.getError() = ${gl.getError()}`);
  requestAnimationFrame(() => renderer.apply(frame, store));
}

// --- stage 5: faithful app replay -------------------------------------------
// Exact in-app conditions from a real gldiag line: fractional cellW, dpr 1.5,
// heuristic baseline, JetBrains Mono, 0-metrics construction, then a FULL
// frame followed by DELTA frames that introduce new glyphs (what typing or
// pasting the decoration test does). onRequestFull is wired like the panel's:
// it re-applies the current full frame (the actor round-trip).
async function stage5(): Promise<void> {
  const A_CELL_W = 8.40625;
  const A_CELL_H = 18;
  const A_DPR = 1.5;
  const A_BASELINE = A_CELL_H * 0.8;
  const A_FONT = "JetBrains Mono";
  try {
    await document.fonts.load(`14px "${A_FONT}"`);
    out("r5", `font loaded: ${document.fonts.check(`14px "${A_FONT}"`)}`);
  } catch {
    out("r5", "font load failed (fallback in use)", true);
  }
  const container = document.createElement("div");
  container.style.cssText = "position:relative;width:840px;height:90px;background:#181818;margin-top:6px";
  document.getElementById("s5host")!.appendChild(container);

  const store = makeStore();
  const frame = makeFrame(store);
  let fullRequests = 0;
  let renderer: WebGLRenderer;
  try {
    renderer = new WebGLRenderer({
      container,
      cellW: 0,
      cellH: 0,
      dpr: 1,
      baseline: 0,
      fontFamily: A_FONT,
      fontSize: 14,
      onRequestFull: () => {
        fullRequests++;
        // The panel would round-trip to the actor; the actor answers with a
        // full frame — replay it on the next task like the channel would.
        setTimeout(() => renderer.apply(frame, store), 0);
      },
    });
    renderer.setMetrics({
      cellW: A_CELL_W,
      cellH: A_CELL_H,
      dpr: A_DPR,
      baseline: A_BASELINE,
      fontFamily: A_FONT,
      fontSize: 14,
    });
  } catch (e) {
    out("r5", `THREW: ${e}`, true);
    return;
  }

  const snap = (label: string): void => {
    const canvas = container.querySelector("canvas") as HTMLCanvasElement;
    const gl = canvas.getContext("webgl2") as WebGL2RenderingContext;
    const px = new Uint8Array(canvas.width * canvas.height * 4);
    gl.readPixels(0, 0, canvas.width, canvas.height, gl.RGBA, gl.UNSIGNED_BYTE, px);
    let lit = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] + px[i + 1] + px[i + 2] > 24) lit++;
    const total = canvas.width * canvas.height;
    out("r5", `${label}: backing=${canvas.width}x${canvas.height} lit=${lit}/${total}`, lit === 0);
    const copy = document.createElement("canvas");
    copy.className = "peek";
    copy.width = canvas.width;
    copy.height = canvas.height;
    copy.style.width = `${canvas.width / A_DPR}px`;
    copy.style.height = `${canvas.height / A_DPR}px`;
    copy.getContext("2d")!.drawImage(canvas, 0, 0);
    const cap = document.createElement("div");
    cap.textContent = label;
    document.getElementById("s5host")!.append(cap, copy);
  };

  frame.kind = "full";
  renderer.apply(frame, store);
  snap("after FULL frame");

  // Delta 1: overwrite row 0 with fresh, never-rasterized glyphs.
  const put = (row: number, text: string, flags = 0): void => {
    for (let col = 0; col < COLS; col++) {
      const idx = row * COLS + col;
      store.ch[idx] = 0;
      store.flags[idx] = 0;
    }
    let col = 0;
    for (const c of text) {
      const idx = row * COLS + col;
      store.ch[idx] = c.codePointAt(0)!;
      store.flags[idx] = flags;
      col++;
    }
  };
  put(1, "ZQXJK@#$%^&*()[]{}", CELL_FLAGS.undercurl);
  const delta = { ...frame, kind: "delta" as const, seq: 2, rows: [{ row: 1, colStart: 0, cellCount: COLS }] };
  renderer.apply(delta, store);
  snap("after DELTA with new glyphs (undercurl row)");

  // Let the renderer's async font-metric self-correction fire, then see what
  // the canvas holds — this is the window where the app looks blank.
  await new Promise((r) => setTimeout(r, 400));
  const canvas = container.querySelector("canvas") as HTMLCanvasElement;
  const gl = canvas.getContext("webgl2") as WebGL2RenderingContext;
  const px = new Uint8Array(canvas.width * canvas.height * 4);
  gl.readPixels(0, 0, canvas.width, canvas.height, gl.RGBA, gl.UNSIGNED_BYTE, px);
  let lit = 0;
  for (let i = 0; i < px.length; i += 4) if (px[i] + px[i + 1] + px[i + 2] > 24) lit++;
  out(
    "r5",
    `+400ms (post self-correction): backing=${canvas.width}x${canvas.height} readback lit=${lit} ` +
      `(NOTE: stale unless a draw ran this task) fullRequests=${fullRequests}`,
  );
  frame.kind = "full";
  renderer.apply(frame, store);
  snap("after settle + fresh full apply");
}

const { atlas } = stage1();
stage2(atlas);
stage3();
stage4();
void stage5();
