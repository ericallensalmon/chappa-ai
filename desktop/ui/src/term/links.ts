// Hyperlink model + detection. Per-terminal OSC 8 link table fed by
// `term://links`, hover-time run detection over the retained CellStore, and
// conservative bare-`http(s)://` autolink detection at hover time (no
// persistent index). The scheme gate lives HERE and is unit-tested — the
// opener plugin is never asked to filter.

import { CELL_FLAGS, type CellStore } from "./protocol";

/** Schemes the app opens on ctrl+click. Anything else — `file:`, `ssh:`,
 *  custom protocols — is ignored with a status flash: the terminal owns the
 *  rest of the URL space. */
export const ALLOWED_SCHEMES = ["http:", "https:", "mailto:"] as const;

/** Whether the app will open `url` (http/https/mailto only). */
export function schemeAllowed(url: string): boolean {
  return ALLOWED_SCHEMES.some((scheme) => url.startsWith(scheme));
}

/** A contiguous link run on one viewport row, `[colStart, colEnd]` inclusive. */
export interface LinkRun {
  row: number;
  colStart: number;
  colEnd: number;
  url: string;
}

/** Per-terminal OSC 8 hyperlink table. Link ids only grow while a terminal
 *  lives; the table is cleared on terminal close. */
export class LinkIndex {
  private readonly byId = new Map<number, string>();

  merge(entries: { id: number; uri: string }[]): void {
    for (const { id, uri } of entries) this.byId.set(id, uri);
  }

  url(id: number): string | undefined {
    return this.byId.get(id);
  }

  clear(): void {
    this.byId.clear();
  }
}

/** One viewport row flattened to text. The wide spacer carries the leading
 *  cell's glyph in alacritty; skipping it keeps the text honest. Blank cells
 *  (ch 0) read as spaces. */
export function rowText(store: CellStore, row: number): string {
  if (row < 0 || row >= store.rows) return "";
  const base = row * store.cols;
  let out = "";
  for (let col = 0; col < store.cols; col++) {
    const flags = store.flags[base + col];
    if (flags & CELL_FLAGS.wideSpacer) continue;
    const ch = store.ch[base + col];
    out += ch === 0 ? " " : String.fromCodePoint(ch);
  }
  return out;
}

/**
 * The contiguous same-`link_id` run containing cell (row, col), if any. The
 * run stops at the row edges: a wrapped link's continuation on the next row
 * is its own run (the renderer underlines per-row spans anyway).
 */
export function linkRunAt(
  store: CellStore,
  row: number,
  col: number,
): { colStart: number; colEnd: number; linkId: number } | null {
  if (row < 0 || row >= store.rows || col < 0 || col >= store.cols) return null;
  const base = row * store.cols;
  const id = store.link[base + col];
  if (id === 0) return null;
  let colStart = col;
  while (colStart > 0 && store.link[base + colStart - 1] === id) colStart--;
  let colEnd = col;
  while (colEnd + 1 < store.cols && store.link[base + colEnd + 1] === id) colEnd++;
  return { colStart, colEnd, linkId: id };
}

/** Conservative bare-URL matcher for autolinks: `http(s)://` followed by
 *  anything that is not whitespace or a shell/closing delimiter. Trailing
 *  sentence punctuation is trimmed from the URL. */
const AUTOLINK_RE = /https?:\/\/[^\s<>"'`{}[\]\\]+/g;

/** Detect a bare `http(s)://` URL in `text` that contains cell column `col`.
 *  Returns the URL run (text columns == cell columns for ASCII URLs) or null.
 */
export function autolinkRun(text: string, col: number): { colStart: number; colEnd: number; url: string } | null {
  if (col < 0 || col >= text.length) return null;
  AUTOLINK_RE.lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = AUTOLINK_RE.exec(text)) !== null) {
    const raw = match[0];
    const stripped = raw.replace(/[.,;:!?)\]}>]+$/, "");
    const colEnd = match.index + stripped.length - 1;
    if (colEnd < match.index) continue;
    if (col >= match.index && col <= colEnd) {
      return { colStart: match.index, colEnd, url: stripped };
    }
    if (match.index > col) break;
  }
  return null;
}

/**
 * The hover-time link at (row, col): an OSC 8 link first (cell `link_id` →
 * the table), then a bare-URL scan of the row's text. Returns null when the
 * cell is not part of any link.
 */
export function linkAt(store: CellStore, index: LinkIndex, row: number, col: number): LinkRun | null {
  const os8 = linkRunAt(store, row, col);
  if (os8) {
    const url = index.url(os8.linkId);
    if (url) return { row, colStart: os8.colStart, colEnd: os8.colEnd, url };
  }
  const auto = autolinkRun(rowText(store, row), col);
  if (auto) return { row, colStart: auto.colStart, colEnd: auto.colEnd, url: auto.url };
  return null;
}
