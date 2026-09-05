// Debug HUD overlay. Ctrl+Shift+D toggles it (the shortcut is
// routed through `isAppShortcut` in input.ts so it never reaches the
// terminal; this controller just listens for it on window). Shows the
// actor-side `FrameStats` (polled via the `debug_stats(id)` command) overlaid
// with frontend-side counters: frames/s, MB/s received, damage rows/frame,
// resyncs (seq-gap/decode-failure FULL requests) and the rolling p95 rAF
// frame time.
//
// Rendering is one fixed div updated on a 1s cadence (and on toggle) — the
// per-frame record calls only bump counters.

export interface DebugStatsDto {
  /** Frames emitted by the actor. */
  framesSent: number;
  /** Wire bytes emitted by the actor. */
  bytesSent: number;
  /** Rows covered by the last emitted frame. */
  damageRowsLast: number;
  /** Ticks the actor withheld emission behind the ack gate. */
  coalescedTicks: number;
  /** Whether a frame is currently un-acked actor-side. */
  outstanding: boolean;
}

/** What the HUD reads on demand; the panel wires the real calls. */
export interface HudData {
  /** Resolve `debug_stats(id)`; null when the panel/term is gone. */
  getRemoteStats: () => Promise<DebugStatsDto | null>;
  /** Local resync counter (seq gaps + decode failures). */
  getResyncs: () => number;
}

// Bare declarations (no selector) — this is assigned to `style.cssText`,
// which silently drops anything that isn't `prop:value`. A selector-wrapped
// rule here loses `position:absolute` and the HUD paints UNDER the viewport.
const HUD_CSS =
  "position:absolute;top:4px;left:4px;z-index:20;pointer-events:none;" +
  "background:rgba(20,21,24,.85);color:#9aa0aa;font:10px ui-monospace,monospace;" +
  "padding:4px 6px;border-radius:4px;white-space:pre;line-height:1.5";

/** Rolling-window length for the rAF frame-time percentile (~4s @ 60fps). */
export const PAINT_WINDOW = 240;

/** How often the HUD recomputes rates and polls the actor. */
export const HUD_INTERVAL_MS = 1000;

/**
 * The p95 of a sample set — the frame time the slowest 5% of frames still
 * beat. Empty input → 0. Pure so tests can pin it.
 */
export function p95(values: number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const idx = Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95));
  return sorted[idx];
}

export class Hud {
  private readonly el: HTMLDivElement;
  private readonly data: HudData;
  private visible = false;
  private secFrames = 0;
  private secBytes = 0;
  private secDamageRows = 0;
  private lastDamageRows = 0;
  private readonly paint: number[] = [];
  private remote: DebugStatsDto | null = null;
  private timer: number | null = null;
  private readonly keyHandler: (e: KeyboardEvent) => void;

  constructor(container: HTMLElement, data: HudData) {
    this.data = data;
    this.el = document.createElement("div");
    this.el.className = "chappa-hud";
    this.el.style.cssText = HUD_CSS;
    this.el.style.display = "none";
    container.appendChild(this.el);

    this.keyHandler = (e) => {
      // e.repeat keeps Ctrl+Shift+D held from toggling every autorepeat.
      if (e.repeat) return;
      if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === "d") {
        e.preventDefault();
        this.toggle();
      }
    };
    window.addEventListener("keydown", this.keyHandler);
  }

  /** Detach the listener and drop the overlay (panel dispose). */
  dispose(): void {
    window.removeEventListener("keydown", this.keyHandler);
    if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }
    this.el.remove();
  }

  /** Flip visibility; rates are measured from the reveal moment. */
  toggle(): void {
    this.visible = !this.visible;
    this.el.style.display = this.visible ? "block" : "none";
    if (this.visible) {
      this.secFrames = 0;
      this.secBytes = 0;
      this.secDamageRows = 0;
      this.timer = window.setInterval(() => void this.tick(), HUD_INTERVAL_MS);
      void this.tick();
    } else if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Per-frame accounting from the panel's onFrame (cheap: counters only). */
  recordFrame(frameBytes: number, damageRows: number): void {
    this.secFrames += 1;
    this.secBytes += frameBytes;
    this.secDamageRows += damageRows;
    this.lastDamageRows = damageRows;
  }

  /** rAF paint duration (ms) — the render-time distribution. */
  recordPaint(frameMs: number): void {
    this.paint.push(frameMs);
    if (this.paint.length > PAINT_WINDOW) this.paint.shift();
  }

  /** True while the overlay is shown (tests + future settings UI). */
  isVisible(): boolean {
    return this.visible;
  }

  private async tick(): Promise<void> {
    // Snapshot the local counters FIRST: the remote poll suspends this
    // async, and the reveal-time tick must not clobber a fresher render.
    const fps = this.secFrames;
    const mbs = this.secBytes / 1e6;
    const dmgAvg = this.secFrames > 0 ? this.secDamageRows / this.secFrames : 0;
    this.secFrames = 0;
    this.secBytes = 0;
    this.secDamageRows = 0;

    this.remote = await this.data.getRemoteStats().catch(() => null);
    if (!this.visible) return;
    const r = this.remote;
    const actor = r
      ? `R ${r.framesSent} · ${(r.bytesSent / 1e6).toFixed(2)} MB\n` +
        `  gate ${r.outstanding ? "OUTSTANDING" : "acked"} · coalesced ${r.coalescedTicks}`
      : "R —";

    this.el.textContent =
      `${fps.toFixed(1)} fps · ${mbs.toFixed(2)} MB/s\n` +
      `dmg ${dmgAvg.toFixed(1)} rows/f · resyncs ${this.data.getResyncs()}\n` +
      `rAF p95 ${p95(this.paint).toFixed(2)} ms (last ${this.lastDamageRows} rows)\n` +
      actor;
  }
}
