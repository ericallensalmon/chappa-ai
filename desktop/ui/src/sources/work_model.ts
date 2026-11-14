// The Work view's model: the items of every trusted source serving `work`,
// the one the environment is set up for pinned on top, the rest grouped by
// status category, and the two filters (assigned to me, text). Pure (no DOM,
// no ipc): the view and the tests feed it payloads.
//
// Keys are per source: two sources may both have an `ABC-1` and they stay
// two rows, each addressed as `{sourceId, key}` (the actions of a row run
// against its own source). A payload arrives exactly as the source sent it,
// so nothing here trusts its shape: an item without a usable key is skipped
// and counted, an optional field that is absent or of the wrong type reads
// as absent, unknown fields are ignored, and a payload speaking another
// contract version is left out with a needs-update banner.

import {
  itemsOf,
  oneLine,
  prActionKey,
  prId,
  readBadges,
  summarizeFreshness,
  wrongContract,
  type Chip,
  type FreshnessSummary,
  type NeedsUpdate,
  type Tone,
  type ViewPayload,
} from "./prs_model";
import { readBanners, readSections } from "./sections";
import type { Banner, LocalStatus, Section } from "./types";

export type WorkCategory = "doing" | "todo" | "done";

/** A linked PR as a chip: opens in the PRs view when that view holds it,
 *  else its link (checked like any other). */
export interface WorkPrChip {
  /** The PRs view's row id ({@link prId}). */
  id: string;
  /** The PRs view's key (`forge:repo:number`), what `openView("prs", …)` takes. */
  key: string;
  /** "owner/repo#12" */
  text: string;
  url: string | null;
  status: string | null;
  tone: Tone;
  /** The PRs view currently lists this PR. */
  loaded: boolean;
}

export interface WorkRow {
  /** Stable across polls: `<sourceId>|<key>`. */
  id: string;
  sourceId: string;
  sourceLabel: string;
  key: string;
  /** The item's title, or its key when it has none. */
  title: string;
  hasTitle: boolean;
  type: string | null;
  status: { label: string; category: WorkCategory | null } | null;
  /** The status chip (tone from the category). */
  statusChip: Chip | null;
  priority: string | null;
  labels: string[];
  parent: { key: string; title: string | null } | null;
  reporter: { name: string; isMe: boolean } | null;
  created: string | null;
  lastTouchedUtc: string | null;
  url: string | null;
  folder: string | null;
  assignedToMe: boolean;
  active: boolean;
  prs: WorkPrChip[];
  /** The source's badges. */
  badges: Chip[];
  localStatus: LocalStatus | null;
  sections: Section[];
  /** Ids of the item's own actions (not its sections'), in payload order. */
  actionIds: string[];
  /** The item or one of its sections carries actions: worth asking Rust. */
  hasActions: boolean;
}

export type WorkGroupId = "active" | WorkCategory | "other";

export interface WorkGroup {
  id: WorkGroupId;
  label: string;
  /** Collapsed by default (Done). */
  folded: boolean;
  rows: WorkRow[];
}

export interface WorkFilter {
  /** The user's choice for "Assigned to me"; null keeps the default (on
   *  when any item is assigned to the user). */
  assignedOnly: boolean | null;
  /** Free text matched against key, title and labels. */
  text: string;
}

export interface WorkModel {
  groups: WorkGroup[];
  /** Every readable row, filters ignored, in source then payload order. */
  all: WorkRow[];
  /** Rows the filters let through (the groups' rows). */
  shown: number;
  /** Whether "Assigned to me" is in effect (the choice, or the default). */
  assignedOnly: boolean;
  /** Some item says it is assigned to the user: the filter means something. */
  anyAssigned: boolean;
  freshness: FreshnessSummary;
  needsUpdate: NeedsUpdate[];
  /** The sources' banners, each with the source it came from. */
  banners: { sourceId: string; sourceLabel: string; banner: Banner }[];
  /** At least one source has served a payload. */
  hasData: boolean;
}

/** Group order and headings. Active leads: it is what the environment is
 *  set up for, whatever its status. */
export const WORK_GROUPS: { id: WorkGroupId; label: string; folded: boolean }[] = [
  { id: "active", label: "Active", folded: false },
  { id: "doing", label: "In progress", folded: false },
  { id: "todo", label: "To do", folded: false },
  { id: "done", label: "Done", folded: true },
  { id: "other", label: "Other", folded: false },
];

const CATEGORY_TONE: Record<WorkCategory | "other", Tone> = { doing: "info", todo: "muted", done: "ok", other: "muted" };

/** A linked PR's status as a tone. */
const PR_TONE: Record<string, Tone> = { open: "info", draft: "warn", merged: "ok", closed: "muted" };

const CATEGORIES: ReadonlySet<string> = new Set(["doing", "todo", "done"]);
const GATES: ReadonlySet<string> = new Set(["any", "human", "human-exit"]);

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const text = (v: unknown): string | null => {
  const s = typeof v === "string" ? oneLine(v) : "";
  return s === "" ? null : s;
};
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);
const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

/** An item's LocalStatus read leniently; null when absent or unusable (no
 *  id or label), which hides the status block. */
export function readLocalStatus(v: unknown): LocalStatus | null {
  const ls = obj(v);
  const id = str(ls?.id);
  const label = text(ls?.label);
  if (!ls || !id || !label) return null;
  const pending = obj(ls.pendingApproval);
  const gate = str(ls.gate);
  return {
    id,
    label,
    since: str(ls.since),
    actor: text(ls.actor),
    round: num(ls.round),
    gate: gate && GATES.has(gate) ? (gate as LocalStatus["gate"]) : null,
    pendingApproval:
      pending && text(pending.to)
        ? {
            to: text(pending.to)!,
            ...(text(pending.by) ? { by: text(pending.by)! } : {}),
            ...(text(pending.note) ? { note: text(pending.note)! } : {}),
          }
        : null,
    allowed: list(ls.allowed)
      .map(obj)
      .filter((a): a is Record<string, unknown> => a !== null && !!str(a.id) && !!text(a.label))
      .map((a) => ({
        id: a.id as string,
        label: text(a.label)!,
        humanOnly: a.humanOnly === true,
        requireNote: a.requireNote === true,
      })),
    history: list(ls.history)
      .map(obj)
      .filter((h): h is Record<string, unknown> => h !== null && !!str(h.at))
      .map((h) => ({
        at: h.at as string,
        ...(text(h.from) ? { from: text(h.from)! } : {}),
        ...(text(h.to) ? { to: text(h.to)! } : {}),
        ...(text(h.actor) ? { actor: text(h.actor)! } : {}),
        ...(text(h.note) ? { note: text(h.note)! } : {}),
        ...(text(h.override) ? { override: text(h.override)! } : {}),
      })),
  };
}

/** The ids of an `actions` array's usable entries. */
function actionIdsOf(v: unknown): string[] {
  return list(v)
    .map(obj)
    .map((a) => str(a?.id))
    .filter((id): id is string => !!id);
}

/** One item read leniently; null when it has no usable key. */
export function readWorkItem(raw: unknown, sourceId: string, sourceLabel: string, loadedPrs: ReadonlySet<string>): WorkRow | null {
  const w = obj(raw);
  const key = str(w?.key);
  if (!w || !key || key.trim() === "") return null;
  const status = obj(w.status);
  const statusLabel = text(status?.label);
  const category = str(status?.category);
  const cat = category && CATEGORIES.has(category) ? (category as WorkCategory) : null;
  const parent = obj(w.parent);
  const reporter = obj(w.reporter);
  const title = text(w.title);
  const sections = readSections(w.sections);
  const actionIds = actionIdsOf(w.actions);
  return {
    id: `${sourceId}|${key}`,
    sourceId,
    sourceLabel,
    key,
    title: title ?? key,
    hasTitle: title !== null,
    type: text(w.type),
    status: statusLabel ? { label: statusLabel, category: cat } : null,
    statusChip: statusLabel ? { text: statusLabel, tone: CATEGORY_TONE[cat ?? "other"] } : null,
    priority: text(w.priority),
    labels: list(w.labels).map(text).filter((l): l is string => l !== null),
    parent: parent && str(parent.key) ? { key: parent.key as string, title: text(parent.title) } : null,
    reporter: reporter && text(reporter.name) ? { name: text(reporter.name)!, isMe: reporter.isMe === true } : null,
    created: str(w.created),
    lastTouchedUtc: str(w.lastTouchedUtc),
    url: str(w.url),
    folder: str(w.folder),
    assignedToMe: w.assignedToMe === true,
    active: w.active === true,
    prs: list(w.prs)
      .map(obj)
      .filter((p): p is Record<string, unknown> => p !== null && !!str(p.forge) && !!str(p.repo) && !!str(p.number))
      .map((p) => {
        const [forge, repo, number] = [p.forge as string, p.repo as string, p.number as string];
        const id = prId(forge, repo, number);
        const status = text(p.status);
        return {
          id,
          key: prActionKey(forge, repo, number),
          text: `${repo}${number}`,
          url: str(p.url),
          status,
          tone: PR_TONE[status?.toLowerCase() ?? ""] ?? "muted",
          loaded: loadedPrs.has(id),
        };
      }),
    badges: readBadges(w.badges).map((b) => ({ text: b.text, tone: (b.tone ?? "info") as Tone, ...(b.title ? { title: b.title } : {}) })),
    localStatus: readLocalStatus(w.localStatus),
    sections,
    actionIds,
    hasActions: actionIds.length > 0 || sections.some((s) => (s.actions ?? []).length > 0),
  };
}

/** Whether a row passes the text filter: every word of it appears in the
 *  key, the title or a label (case-insensitive). */
export function matchesText(row: WorkRow, filter: string): boolean {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const hay = [row.key, row.hasTitle ? row.title : "", ...row.labels].join("\u0000").toLowerCase();
  return words.every((w) => hay.includes(w));
}

const plural = (n: number, one: string, many = `${one}s`): string => `${n} ${n === 1 ? one : many}`;

/**
 * The Work model of every trusted source's `work` payload. `loadedPrs` holds
 * the PRs view's row ids ({@link prId}): a PR chip it contains opens there.
 */
export function buildWorkModel(
  payloads: ViewPayload[],
  opts: { needsUpdate?: NeedsUpdate[]; filter?: WorkFilter; loadedPrs?: ReadonlySet<string> } = {},
): WorkModel {
  const stale: NeedsUpdate[] = [...(opts.needsUpdate ?? [])];
  const usable: ViewPayload[] = [];
  for (const p of payloads) {
    if (wrongContract(p.payload)) {
      if (!stale.some((s) => s.sourceId === p.sourceId)) stale.push({ sourceId: p.sourceId, sourceLabel: p.sourceLabel });
    } else {
      usable.push(p);
    }
  }
  const freshness = summarizeFreshness(usable);
  const loaded = opts.loadedPrs ?? new Set<string>();
  const all: WorkRow[] = [];
  const banners: WorkModel["banners"] = [];
  for (const p of usable) {
    if (p.payload === null || p.payload === undefined) continue;
    for (const banner of readBanners(obj(p.payload)?.banners)) banners.push({ sourceId: p.sourceId, sourceLabel: p.sourceLabel, banner });
    const seen = new Set<string>();
    let skipped = 0;
    let repeated = 0;
    for (const raw of itemsOf(p.payload)) {
      const row = readWorkItem(raw, p.sourceId, p.sourceLabel, loaded);
      if (!row) {
        skipped++;
        continue;
      }
      // A key the source repeats is one item (its actions are addressed by
      // the key): the first one stands.
      if (seen.has(row.key)) {
        repeated++;
        continue;
      }
      seen.add(row.key);
      all.push(row);
    }
    const say = (line: string): void => void freshness.errors.push(usable.length > 1 ? `${p.sourceLabel}: ${line}` : line);
    if (skipped > 0) say(`${plural(skipped, "work item")} could not be read`);
    if (repeated > 0) say(`${plural(repeated, "work item")} repeated a key and ${repeated === 1 ? "was" : "were"} left out`);
  }
  const anyAssigned = all.some((r) => r.assignedToMe);
  const assignedOnly = opts.filter?.assignedOnly ?? anyAssigned;
  const textFilter = opts.filter?.text ?? "";
  // The active item stays on top even when it is not assigned to the user:
  // it is what the environment is set up for. The text filter still applies.
  const passes = (r: WorkRow): boolean => (!assignedOnly || r.assignedToMe || r.active) && matchesText(r, textFilter);
  const shownRows = all.filter(passes);
  const groupOf = (r: WorkRow): WorkGroupId => (r.active ? "active" : (r.status?.category ?? "other"));
  const groups = WORK_GROUPS.map((g) => ({ ...g, rows: shownRows.filter((r) => groupOf(r) === g.id) })).filter((g) => g.rows.length > 0);
  return {
    groups,
    all,
    shown: shownRows.length,
    assignedOnly,
    anyAssigned,
    freshness,
    needsUpdate: stale,
    banners,
    hasData: usable.some((p) => p.payload !== null && p.payload !== undefined),
  };
}
