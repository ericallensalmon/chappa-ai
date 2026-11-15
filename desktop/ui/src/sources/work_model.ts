// The Work view's model: the items of every trusted source serving `work`,
// the one the environment is set up for pinned on top, the rest grouped by
// status category, and the two filters (assigned to me, text). Pure (no DOM,
// no ipc): the view and the tests feed it payloads.
//
// Keys are per source: two sources may both have an `ABC-1` and they stay
// two rows, each addressed as `{sourceId, key}` (the actions of a row run
// against its own source). The exception is an entry that says it adds to
// another source's item (`ref`): it is merged into that item's row, as
// `work_merge.ts` describes, and PRs from the PRs view join rows by the
// rules there. A payload arrives exactly as the source sent it,
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
import type { PrRow } from "./prs_model";
import { readBanners, readSections } from "./sections";
import type { Banner, LocalStatus, Section } from "./types";
import {
  joinPrs,
  mergeWorkEntries,
  type JoinHow,
  type KeyMatchRule,
  type LocalField,
  type MergeSource,
  type WorkEntry,
  type WorkJoins,
} from "./work_merge";

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
  /** How it came to be on the row: listed by a source, or joined. */
  how: JoinHow;
}

/** What one source contributes to a row: its actions run against it, under
 *  its own key, judged by its own trust. */
export interface WorkContribution {
  sourceId: string;
  sourceLabel: string;
  /** The tracker that owns the item, or a source adding to it. */
  role: "owner" | "overlay";
  /** The key its actions are filed under in its payload. */
  key: string;
  badges: Chip[];
  sections: Section[];
  /** Ids of its own actions (not its sections'). */
  actionIds: string[];
  hasActions: boolean;
}

/** A banner on a row, with the source and key its actions belong to. */
export interface WorkRowBanner {
  sourceId: string;
  sourceLabel: string;
  key: string;
  banner: Banner;
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
  /** Branches of the item, in the PRs' own terms (forge, repo, branch). */
  branches: { forge: string; repo: string; branch: string }[];
  /** The row's banners from every contributor. */
  banners: WorkRowBanner[];
  /** The worst tone among the banners (the row's dot), or null. */
  bannerTone: Banner["tone"] | null;
  /** Every source feeding the row: the owner first, then the sources
   *  adding to it in the Settings order. */
  contributions: WorkContribution[];
  /** The source that lists the item as its own; null when only sources
   *  adding to it do (the owner is missing). */
  owner: { sourceId: string; manifestId: string } | null;
  /** What the row's entries point at, when they add to another source's
   *  item (an overlay read on its own, or a row whose owner is missing). */
  ownerRef: { source: string; key: string } | null;
  /** Why the owner row is missing (also among the badges). */
  missingOwner?: Chip;
  /** The source whose key-match rule applies (its keys are the row's). */
  keySourceId: string | null;
  /** The source the folder came from, when a source adding to the row set it. */
  folderFrom?: string;
  /** The source whose local status the row shows (its moves run there). */
  localStatusFrom?: { sourceId: string; sourceLabel: string; key: string };
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
  /** The PRs joined to rows (by row) and the rows each PR joined (by PR). */
  joins: WorkJoins;
}

/** The PR state of a joined PR row as a chip tone. */
const STATE_TONE: Record<string, Tone> = { open: "info", draft: "warn", merged: "ok", closed: "muted" };

/** A PR row joined to a work row, as the row's chip. */
function joinedChip(pr: PrRow, how: JoinHow): WorkPrChip {
  return { id: pr.id, key: pr.primary.key, text: `${pr.repo}${pr.number}`, url: pr.url || null, status: pr.state, tone: STATE_TONE[pr.state] ?? "muted", loaded: true, how };
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

/** An overlay's `ref`, or null for an entry its source owns. */
export function readRef(v: unknown): { source: string; key: string } | null {
  const r = obj(v);
  const source = str(r?.source);
  const key = str(r?.key);
  return source && source.trim() !== "" && key && key.trim() !== "" ? { source, key } : null;
}

/** An item's `branches`, leniently: entries missing a part are dropped. */
function readBranches(v: unknown): WorkRow["branches"] {
  return list(v)
    .map(obj)
    .filter((b): b is Record<string, unknown> => b !== null && !!str(b.forge) && !!str(b.repo) && !!text(b.branch))
    .map((b) => ({ forge: b.forge as string, repo: b.repo as string, branch: (b.branch as string).trim() }));
}

const TONE_RANK = { info: 1, warn: 2, bad: 3 } as const;

/** One item read leniently; null when it has no usable key. An entry adding
 *  to another source's item may leave its key out: it is `ref.key`. */
export function readWorkItem(raw: unknown, sourceId: string, sourceLabel: string, loadedPrs: ReadonlySet<string>): WorkRow | null {
  const w = obj(raw);
  const ref = readRef(w?.ref);
  const key = str(w?.key) ?? ref?.key ?? null;
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
  const badges = readBadges(w.badges).map((b) => ({ text: b.text, tone: (b.tone ?? "info") as Tone, ...(b.title ? { title: b.title } : {}) }));
  const banners = readBanners(w.banners).map((banner) => ({ sourceId, sourceLabel, key, banner }));
  const hasActions = actionIds.length > 0 || sections.some((s) => (s.actions ?? []).length > 0) || banners.some((b) => (b.banner.actions ?? []).length > 0);
  const bannerTone = banners.reduce<Banner["tone"] | null>((t, b) => (t === null || TONE_RANK[b.banner.tone] > TONE_RANK[t] ? b.banner.tone : t), null);
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
          how: "linked" as const,
        };
      }),
    badges,
    localStatus: readLocalStatus(w.localStatus),
    sections,
    actionIds,
    hasActions,
    branches: readBranches(w.branches),
    banners,
    bannerTone,
    contributions: [{ sourceId, sourceLabel, role: ref ? "overlay" : "owner", key, badges, sections, actionIds, hasActions }],
    owner: null,
    ownerRef: ref,
    keySourceId: ref ? null : sourceId,
  };
}

/** Which local fields an entry sets: present and not null. */
function localSets(raw: unknown): Record<LocalField, boolean> {
  const w = obj(raw) ?? {};
  const set = (k: string): boolean => w[k] !== undefined && w[k] !== null;
  return { folder: set("folder"), active: set("active"), localStatus: set("localStatus"), branches: set("branches") };
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
  opts: {
    needsUpdate?: NeedsUpdate[];
    filter?: WorkFilter;
    loadedPrs?: ReadonlySet<string>;
    /** The listed sources, in the Settings order: who is who (manifest
     *  ids, for `ref`), and why an owner is missing. */
    sources?: readonly MergeSource[];
    /** The PRs view's rows, to join to work rows. */
    prRows?: readonly PrRow[];
    /** A source's key-match rule (default: on, with the default pattern). */
    keyMatch?: (sourceId: string | null) => KeyMatchRule | null | undefined;
  } = {},
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
  const entries: WorkEntry[] = [];
  const listedBy = new Set<string>();
  const banners: WorkModel["banners"] = [];
  for (const p of usable) {
    if (p.payload === null || p.payload === undefined) continue;
    listedBy.add(p.sourceId);
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
      entries.push({ sourceId: p.sourceId, sourceLabel: p.sourceLabel, ref: row.ownerRef, row, sets: localSets(raw) });
    }
    const say = (line: string): void => void freshness.errors.push(usable.length > 1 ? `${p.sourceLabel}: ${line}` : line);
    if (skipped > 0) say(`${plural(skipped, "work item")} could not be read`);
    if (repeated > 0) say(`${plural(repeated, "work item")} repeated a key and ${repeated === 1 ? "was" : "were"} left out`);
  }
  const all = mergeWorkEntries(entries, opts.sources, listedBy);
  const joins = joinPrs(all, opts.prRows ?? [], opts.keyMatch ?? (() => null));
  for (const row of all) {
    const extra = joins.byRow.get(row.id);
    if (extra) row.prs = [...row.prs, ...extra.filter((j) => !row.prs.some((p) => p.id === j.pr.id)).map((j) => joinedChip(j.pr, j.how))];
  }
  const anyAssigned = all.some((r) => r.assignedToMe);
  const assignedOnly = opts.filter?.assignedOnly ?? anyAssigned;
  const textFilter = opts.filter?.text ?? "";
  // The active item stays on top even when it is not assigned to the user:
  // it is what the environment is set up for. A row whose tracker does not
  // list it (or is not here) cannot say whether it is assigned, so it stays
  // too: a workspace never disappears because a query changed. The text
  // filter still applies.
  const passes = (r: WorkRow): boolean => (!assignedOnly || r.assignedToMe || r.active || r.owner === null) && matchesText(r, textFilter);
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
    joins,
  };
}
