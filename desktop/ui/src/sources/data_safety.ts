// Data safety in the views: which Work rows a stack's data-safety block
// covers, the Needs you rows it raises, and the "held" marker of an agent
// whose deliveries wait. Pure (no DOM, no ipc).
//
// Rust enforces the block (no agent starts in a covered folder, deliveries to
// running agents there are held) and sends what is in force through
// `data_safety_state` / `term://data-safety`. The webview only shows it:
//  - the Work row of the bound item (or any row whose folder is covered) gets
//    a `bad` badge "agents blocked — data safety", and its detail a banner
//    that leads to Environment, where the block is acknowledged;
//  - Needs you gets one row per block (it opens Environment on the component
//    the reason is about, and carries the source's acknowledge action while
//    the source answers and its data is not clean) and one per running agent
//    inside a covered folder;
//  - an agent row with held deliveries says how many ("held: 2 messages").
// The agent buttons themselves are disabled by Rust's verdict, with the
// reason as their tooltip, like any other disabled action.

import type * as ipc from "../ipc";
import type { NeedsYouRow, NeedsYouSection } from "./needs_you";
import type { Chip } from "./prs_model";

export const EMPTY_DATA_SAFETY: ipc.DataSafetyStateDto = { blocks: [], agents: [], held: [] };

/** The badge a covered Work row wears. */
export const BLOCKED_BADGE_TEXT = "agents blocked — data safety";

/** A folder for comparison: forward slashes, no trailing slash, no Windows
 *  verbatim prefix, and lowercase when it is a Windows path (Rust compares
 *  those ignoring case; this only decides which rows wear a badge). */
export function normFolder(path: string): string {
  let p = path.replace(/^\\\\\?\\/, "").replace(/\\/g, "/");
  while (p.length > 1 && p.endsWith("/")) p = p.slice(0, -1);
  return /^[a-z]:/i.test(p) ? p.toLowerCase() : p;
}

/** `child` is `root` or below it, by whole path parts. */
export function folderInside(root: string, child: string): boolean {
  const r = normFolder(root);
  const c = normFolder(child);
  return c === r || c.startsWith(r.endsWith("/") ? r : `${r}/`);
}

/** What a Work row needs for the match. */
export interface RowRef {
  key: string;
  folder: string | null;
  owner: { manifestId: string } | null;
  ownerRef: { source: string; key: string } | null;
}

/** The block covering a Work row: the one bound to its item (by the owning
 *  source's manifest id and the key), else one naming its folder or a folder
 *  above it. Null when none does. */
export function blockForRow(row: RowRef, blocks: readonly ipc.DataSafetyBlockDto[]): ipc.DataSafetyBlockDto | null {
  const source = row.owner?.manifestId ?? row.ownerRef?.source ?? null;
  const key = row.ownerRef?.key ?? row.key;
  for (const b of blocks) {
    if (b.bound && b.bound.source !== null && b.bound.source === source && b.bound.key === key) return b;
  }
  if (row.folder) {
    for (const b of blocks) if (b.folders.some((f) => folderInside(f, row.folder!))) return b;
  }
  return null;
}

/** Mark the rows a block covers: each gets `dataSafety` (the block, or
 *  null) and, when covered, the badge after its own. In place: the model's
 *  groups hold the same row objects. */
export function markDataSafety(
  rows: (RowRef & { badges: Chip[]; dataSafety?: ipc.DataSafetyBlockDto | null })[],
  blocks: readonly ipc.DataSafetyBlockDto[],
): void {
  for (const row of rows) {
    const block = blockForRow(row, blocks);
    row.dataSafety = block;
    if (block && !row.badges.some((b) => b.text === BLOCKED_BADGE_TEXT)) row.badges = [...row.badges, blockedBadge(block)];
  }
}

/** The row's badge for a block: the reason is its tooltip. */
export function blockedBadge(block: ipc.DataSafetyBlockDto): Chip {
  return { text: BLOCKED_BADGE_TEXT, tone: "bad", title: block.reason };
}

/** The item panel's banner for a block. */
export function blockedBanner(block: ipc.DataSafetyBlockDto): { title: string; lines: string[] } {
  return {
    title: `Agents are blocked here: the data on ${block.instance} may be real`,
    lines: [block.reason, "No new agent starts in this item's folders until the check is clean or acknowledged in Environment."],
  };
}

/** "held: 1 message" / "held: 3 messages". */
export function heldText(count: number): string {
  return `held: ${count} message${count === 1 ? "" : "s"}`;
}

/** How many deliveries are held for a pane. */
export function heldCount(state: ipc.DataSafetyStateDto, termId: number): number {
  return state.held.find((h) => h.termId === termId)?.count ?? 0;
}

/** The acknowledge action of a block, as the Environment view offers it:
 *  the source's own view-level action, so it asks the same confirm (every
 *  finding listed) and the same required note, and Rust sends the person as
 *  `X-Chappa-Actor`. Only while the source answers and says `not-clean`. */
export function ackTargets(b: ipc.DataSafetyBlockDto): NeedsYouRow["actions"] {
  if (!b.ackActionId || !b.current || b.state !== "not-clean") return [];
  return [{ sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "environment", key: null, only: [b.ackActionId] }];
}

/** The Needs you section: one row per block, then one per running agent
 *  inside a covered folder. Null when nothing is blocked. */
export function dataSafetySection(state: ipc.DataSafetyStateDto): NeedsYouSection | null {
  const rows: NeedsYouRow[] = [];
  for (const b of state.blocks) {
    rows.push({
      id: `ds:${b.sourceId}|${b.instance}`,
      kind: "environment",
      title: `Agents blocked on ${b.instance}`,
      detail: b.reason,
      tone: "bad",
      folded: false,
      counted: true,
      sourceId: b.sourceId,
      sourceLabel: b.sourceLabel,
      actions: ackTargets(b),
      opens: { view: "environment", key: b.component ?? "" },
    });
  }
  for (const a of state.agents) {
    const held = heldCount(state, a.termId);
    rows.push({
      id: `ds-agent:${a.termId}`,
      kind: "terminal",
      title: a.name,
      detail: [`working next to unscanned/real data in ${a.instance}`, held ? heldText(held) : ""].filter(Boolean).join(" · "),
      tone: "bad",
      folded: false,
      counted: true,
      sourceId: null,
      sourceLabel: null,
      actions: [],
      terminalKey: `term:${a.termId}`,
      opens: null,
    });
  }
  return rows.length ? { id: "data-safety", title: "Data safety", rows } : null;
}
