// Prompt-mark navigation. Stores PromptStart rows per terminal and
// converts "previous/next mark relative to the current viewport" into an
// absolute display_offset.
//
// Rows are BUFFER-ABSOLUTE: a line's index from the top of scrollback
// (history above the screen + viewport row), as emitted by the actor at mark
// time — the one coordinate that stays put as output scrolls (grid lines
// shift with every rotation; the original grid-line scheme pinned every mark
// to ~the prompt row and nav went nowhere — host-run bug). A frame header's
// `historyLen` maps a stored row onto today's grid: grid = row - historyLen.
//
// Honest limit: once the scrollback cap starts dropping top lines, absolute
// indices of surviving lines drift downward while stored rows stay put, so
// marks older than the cap jump approximately (clamped to the buffer top).

/** Cap on stored marks per terminal. Oldest are dropped first. */
export const MARKS_CAP = 1000;

export class Marks {
  /** PromptStart buffer-absolute rows, ascending (oldest first). */
  private rows: number[] = [];

  /** Record a prompt-start row. Rows arrive in stream order and strictly
   *  grow; duplicates (a re-emitted prompt on the same line) are ignored. */
  add(row: number): void {
    const last = this.rows[this.rows.length - 1];
    if (this.rows.length > 0 && row <= last) return;
    this.rows.push(row);
    if (this.rows.length > MARKS_CAP) this.rows.shift();
  }

  clear(): void {
    this.rows = [];
  }

  /** The display_offset that puts the mark at `row` center-ish (viewport row
   *  rows/2), clamped to the reachable scrollback. */
  offsetForMark(row: number, historyLen: number, rows: number): number {
    const centerRow = Math.floor(rows / 2);
    const grid = row - historyLen;
    return Math.max(0, Math.min(historyLen, centerRow - grid));
  }

  /** The previous/next mark row relative to the current viewport's center.
   *  The center's buffer-absolute row is `historyLen + rows/2 - offset`.
   *  Prev = the newest mark above the center; next = the oldest mark below
   *  it. null when there is none in that direction. */
  nav(
    dir: "prev" | "next",
    displayOffset: number,
    rows: number,
    historyLen: number,
  ): number | null {
    const center = historyLen + Math.floor(rows / 2) - displayOffset;
    if (dir === "prev") {
      for (let i = this.rows.length - 1; i >= 0; i--) {
        if (this.rows[i] < center) return this.rows[i];
      }
    } else {
      for (const row of this.rows) {
        if (row > center) return row;
      }
    }
    return null;
  }

  /** The stored rows (tests inspect the history). */
  snapshot(): number[] {
    return [...this.rows];
  }
}
