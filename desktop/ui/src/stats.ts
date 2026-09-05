// Per-terminal process stats: the `term://stats` store plus the two
// tiny formatters the rail row and the terminal hint bar render from.
//
// Scope is those two readouts ONLY — there is no activity-monitor view, so
// no history, no series and no sampling
// here: Rust
// owns the poller and only tells us when something MATERIALLY changed
// (>0.5% cpu, >1MB rss, any subprocess-count delta).
//
// The store's own apply/ignore-unchanged gate is a second line of defence, not
// a duplicate of the Rust one: an exited terminal's final zero, a redelivered
// event and a re-emitted identical sample must all cost zero repaints.

import type { TerminalStatsEvent } from "./ipc";

/** One terminal's current cost. `cpuPct` is 0–100 across ALL cores. */
export interface TermStats {
  cpuPct: number;
  memBytes: number;
  /** Live processes under the shell, EXCLUDING the shell itself. */
  subprocCount: number;
}

export const ZERO_STATS: TermStats = { cpuPct: 0, memBytes: 0, subprocCount: 0 };

const MB = 1024 * 1024;
const GB = 1024 * MB;

/**
 * Memory for the rail tooltip: MB below a gigabyte, GB above, one decimal
 * either way. Deliberately never bytes/KB — a shell's rss is always MB-scale
 * and a unit that changes width every tick makes the tooltip jitter.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0.0 MB";
  return bytes >= GB ? `${(bytes / GB).toFixed(1)} GB` : `${(bytes / MB).toFixed(1)} MB`;
}

/**
 * The rail's `N%` chip, or "" when there is nothing worth showing.
 *
 * Hidden when cpu rounds to 0 and count is 0 — no noise on idle shells. A
 * shell with children is NOT idle even at 0% — the chip stays so
 * the tooltip explaining the subprocess count has something to hang on.
 */
export function statsChip(stats: TermStats): string {
  const cpu = Math.round(stats.cpuPct);
  if (cpu === 0 && stats.subprocCount === 0) return "";
  return `${cpu}%`;
}

/** The rail chip's tooltip: memory plus the subprocess count. */
export function statsTooltip(stats: TermStats): string {
  return `${formatBytes(stats.memBytes)} · ${subprocessLabel(stats.subprocCount)}`;
}

/** "1 subprocess" / "3 subprocesses" — the hint bar's text and the tooltip's
 *  tail. Pluralised: "1 subprocesses" is the kind of wrong that gets noticed. */
export function subprocessLabel(count: number): string {
  return `${count} ${count === 1 ? "subprocess" : "subprocesses"}`;
}

/** Whether two samples are the same reading (the ignore-unchanged gate). */
function same(a: TermStats, b: TermStats): boolean {
  return (
    a.cpuPct === b.cpuPct && a.memBytes === b.memBytes && a.subprocCount === b.subprocCount
  );
}

/**
 * The app's `term://stats` state, keyed by term id. Everything the rail and
 * the hint bar read comes from here; nothing else caches a sample.
 */
export class StatsStore {
  private readonly byTerm = new Map<number, TermStats>();

  /**
   * Adopt one `term://stats` payload. Returns true when the store CHANGED —
   * a false answer means the caller must not repaint. Missing/garbage numbers
   * decode to 0 rather than NaN, which would poison every later comparison
   * (NaN !== NaN would repaint forever).
   */
  apply(event: TerminalStatsEvent): boolean {
    const next: TermStats = {
      cpuPct: finite(event.cpu_pct),
      memBytes: finite(event.mem_bytes),
      subprocCount: finite(event.subproc_count),
    };
    const prev = this.byTerm.get(event.term_id);
    if (prev && same(prev, next)) return false;
    this.byTerm.set(event.term_id, next);
    return true;
  }

  /** This terminal's last sample, or null when it never reported one. */
  get(id: number): TermStats | null {
    return this.byTerm.get(id) ?? null;
  }

  /** Drop a closed terminal's sample (its row and panel are gone). */
  forget(id: number): void {
    this.byTerm.delete(id);
  }
}

function finite(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
