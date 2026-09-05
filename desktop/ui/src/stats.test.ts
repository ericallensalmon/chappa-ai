import { describe, expect, it } from "vitest";
import {
  formatBytes,
  statsChip,
  statsTooltip,
  StatsStore,
  subprocessLabel,
  ZERO_STATS,
  type TermStats,
} from "./stats";
import type { TerminalStatsEvent } from "./ipc";

function event(over: Partial<TerminalStatsEvent> = {}): TerminalStatsEvent {
  return {
    event: "term://stats",
    term_id: 1,
    seq: 1,
    cpu_pct: 12.4,
    mem_bytes: 50 * 1024 * 1024,
    subproc_count: 2,
    ...over,
  };
}

const MB = 1024 * 1024;
const GB = 1024 * MB;

describe("formatBytes", () => {
  it("renders MB below a gigabyte and GB above, always one decimal", () => {
    const table: Array<[number, string]> = [
      [0, "0.0 MB"],
      [512 * 1024, "0.5 MB"],
      [MB, "1.0 MB"],
      [50 * MB, "50.0 MB"],
      [1536 * MB, "1.5 GB"],
      [GB - 1, "1024.0 MB"], // the boundary belongs to MB
      [GB, "1.0 GB"],
      [3 * GB + 512 * MB, "3.5 GB"],
    ];
    for (const [bytes, expected] of table) {
      expect(formatBytes(bytes), `${bytes} bytes`).toBe(expected);
    }
  });

  it("degrades on garbage instead of printing NaN", () => {
    expect(formatBytes(Number.NaN)).toBe("0.0 MB");
    expect(formatBytes(-1)).toBe("0.0 MB");
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe("0.0 MB");
  });
});

describe("statsChip / statsTooltip", () => {
  const stats = (over: Partial<TermStats> = {}): TermStats => ({ ...ZERO_STATS, ...over });

  it("is empty for an idle shell and a whole percent otherwise", () => {
    // Hidden when cpu rounds to 0 AND the count is 0.
    expect(statsChip(stats())).toBe("");
    expect(statsChip(stats({ cpuPct: 0.4, memBytes: 40 * MB }))).toBe("");
    // Rounds, never truncates: 0.6% is visible activity.
    expect(statsChip(stats({ cpuPct: 0.6 }))).toBe("1%");
    expect(statsChip(stats({ cpuPct: 12.4 }))).toBe("12%");
    expect(statsChip(stats({ cpuPct: 99.7 }))).toBe("100%");
    // Children make a 0%-cpu shell non-idle: the chip stays so its tooltip
    // (which explains the subprocess count) has something to hang on.
    expect(statsChip(stats({ subprocCount: 1 }))).toBe("0%");
  });

  it("tooltips memory plus a correctly pluralised subprocess count", () => {
    expect(statsTooltip(stats({ memBytes: 50 * MB, subprocCount: 2 }))).toBe(
      "50.0 MB · 2 subprocesses",
    );
    expect(statsTooltip(stats({ memBytes: 1536 * MB, subprocCount: 1 }))).toBe(
      "1.5 GB · 1 subprocess",
    );
    expect(subprocessLabel(0)).toBe("0 subprocesses");
  });
});

describe("StatsStore", () => {
  it("adopts a new sample and IGNORES an unchanged one", () => {
    const store = new StatsStore();
    expect(store.get(1)).toBeNull();

    expect(store.apply(event())).toBe(true);
    expect(store.get(1)).toEqual({ cpuPct: 12.4, memBytes: 50 * MB, subprocCount: 2 });

    // Byte-identical redelivery: no change, so no repaint.
    expect(store.apply(event())).toBe(false);
    // A different seq is still the same READING — seq is envelope, not data.
    expect(store.apply(event({ seq: 9 }))).toBe(false);

    // Each field on its own is enough to count as changed.
    expect(store.apply(event({ cpu_pct: 12.5 }))).toBe(true);
    expect(store.apply(event({ cpu_pct: 12.5, mem_bytes: 51 * MB }))).toBe(true);
    expect(store.apply(event({ cpu_pct: 12.5, mem_bytes: 51 * MB, subproc_count: 3 }))).toBe(true);
  });

  it("keeps terminals apart and forgets a closed one", () => {
    const store = new StatsStore();
    store.apply(event({ term_id: 1, cpu_pct: 10 }));
    store.apply(event({ term_id: 2, cpu_pct: 20 }));
    expect(store.get(1)?.cpuPct).toBe(10);
    expect(store.get(2)?.cpuPct).toBe(20);

    store.forget(1);
    expect(store.get(1)).toBeNull();
    expect(store.get(2)?.cpuPct).toBe(20);
    // A reused term id starts clean, never inheriting the old row's reading.
    expect(store.apply(event({ term_id: 1, cpu_pct: 10 }))).toBe(true);
  });

  it("decodes a missing or non-numeric field as 0, never NaN", () => {
    // NaN !== NaN, so a poisoned sample would report "changed" on every tick
    // for the rest of the session.
    const store = new StatsStore();
    const broken = { ...event(), cpu_pct: undefined, mem_bytes: "lots" } as unknown;
    expect(store.apply(broken as TerminalStatsEvent)).toBe(true);
    expect(store.get(1)).toEqual({ cpuPct: 0, memBytes: 0, subprocCount: 2 });
    expect(store.apply(broken as TerminalStatsEvent)).toBe(false);
  });

  it("records an exit's final zero once, then treats it as unchanged", () => {
    const store = new StatsStore();
    store.apply(event());
    const zero = event({ cpu_pct: 0, mem_bytes: 0, subproc_count: 0, seq: 2 });
    expect(store.apply(zero)).toBe(true);
    expect(store.get(1)).toEqual(ZERO_STATS);
    expect(store.apply(zero)).toBe(false);
  });
});
