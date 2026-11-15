// One work item fed by several sources. Pure (no DOM, no ipc): the Work
// model and the PRs view read it, the tests feed it rows.
//
// A tracker owns an item's row; other sources add to it. A work entry with
// `ref: {source, key}` is such an addition (an overlay): it is merged into
// the row the source with manifest id `ref.source` lists under `ref.key`.
//  - The owner supplies what the tracker knows: key, title, type, status,
//    url, priority, labels, parent, reporter, created, assigned-to-me.
//  - Overlays supply what is local: folder, active, local status, branches.
//    An owner value for one of these counts only when no overlay sets it;
//    when two overlays set one differently, the first source in the
//    Settings order wins and the row carries a warning naming both.
//  - Badges, banners, sections, actions and linked PRs are appended from
//    every contributor (PRs deduped by forge, repo and number). Each
//    contribution stays tagged with its source: its actions run against
//    that source, judged by that source's trust, never the owner's.
//  - An overlay whose owner row is missing (the tracker is not set up here,
//    not trusted, down, or no longer lists the key) still shows, as its own
//    row with the overlay's cached title and url, and says why: a workspace
//    never disappears because a query changed.
//
// Untrusted sources feed nothing at all (the data layer reads trusted ones
// only), so an untrusted overlay never colours a trusted owner's row, and an
// untrusted owner never lends an overlay its fields.
//
// PRs join a work row by three rules, checked in order: the row's merged
// `prs` names it ("linked"); one of the row's `branches` is the PR's source
// branch on the same forge and repo ("branch"); or the owner's item key
// appears as a whole word in the PR's source branch or title ("key in
// branch" / "key in title"), found with the owner source's key pattern and
// joined only when the found text equals the key exactly.

import type * as ipc from "../ipc";
import { prId, type Chip, type PrRow } from "./prs_model";
import type { Banner } from "./types";
import type { WorkPrChip, WorkRow } from "./work_model";

/** The default pattern that finds candidate keys in a branch or title. */
export const DEFAULT_KEY_PATTERN = "[A-Z][A-Z0-9]+-\\d+";

/** A source's key-match choice, as Settings ▸ Sources keeps it. */
export interface KeyMatchRule {
  enabled: boolean;
  /** null = {@link DEFAULT_KEY_PATTERN}. */
  pattern: string | null;
}

/** How a PR came to sit on a work row. */
export type JoinHow = "linked" | "branch" | "key in branch" | "key in title";

/** What the merge needs to know about a listed source. */
export type MergeSource = Pick<ipc.SourceDto, "id" | "label" | "trusted" | "status" | "manifestId"> &
  Partial<Pick<ipc.SourceDto, "trust" | "keyMatch">>;

/** One work entry as read from one source's payload. */
export interface WorkEntry {
  sourceId: string;
  sourceLabel: string;
  /** Set on an overlay: what it adds to. */
  ref: { source: string; key: string } | null;
  row: WorkRow;
  /** Which of the local fields the entry set (present and not null). */
  sets: Record<LocalField, boolean>;
}

/** The fields an overlay supplies. */
export const LOCAL_FIELDS = ["folder", "active", "localStatus", "branches"] as const;
export type LocalField = (typeof LOCAL_FIELDS)[number];

/** The manifest id of a listed source (its registry id when unknown). */
function manifestIdOf(sourceId: string, sources: readonly MergeSource[] | undefined): string {
  return sources?.find((s) => s.id === sourceId)?.manifestId ?? sourceId;
}

/** Why an overlay's owner row is not there, as the row's badge. */
export function ownerMissingBadge(
  ref: { source: string; key: string },
  sources: readonly MergeSource[] | undefined,
  listedBy: ReadonlySet<string>,
): Chip {
  const owners = (sources ?? []).filter((s) => (s.manifestId ?? s.id) === ref.source);
  const owner = owners.find((s) => s.trusted) ?? owners[0];
  const label = owner?.label ?? ref.source;
  if (owner && listedBy.has(owner.id)) {
    return { text: `not listed by ${label}`, tone: "muted", title: `${label} no longer lists ${ref.key}; this is what other sources keep about it` };
  }
  const why = !owner
    ? `no source called ${ref.source} is set up here`
    : !owner.trusted
      ? `${label} needs review under Settings ▸ Sources`
      : owner.status === "down"
        ? `${label} is down`
        : `${label} has not answered yet`;
  return { text: `${label} unavailable`, tone: "warn", title: `${ref.key}: ${why}` };
}

const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);

const worst = (tones: Banner["tone"][]): Banner["tone"] | null =>
  tones.includes("bad") ? "bad" : tones.includes("warn") ? "warn" : tones.includes("info") ? "info" : null;

/**
 * Merge the work entries of every trusted source, given in the Settings
 * order (source by source, each in its payload's order), into rows.
 * `listedBy` holds the sources that answered with a usable payload (a source
 * that did, and lacks the key, "no longer lists" it).
 */
export function mergeWorkEntries(entries: WorkEntry[], sources: readonly MergeSource[] | undefined, listedBy: ReadonlySet<string>): WorkRow[] {
  const owners = new Map<string, WorkRow>();
  const rows: WorkRow[] = [];
  const overlays: WorkEntry[] = [];
  for (const e of entries) {
    if (e.ref) {
      overlays.push(e);
      continue;
    }
    const mid = manifestIdOf(e.sourceId, sources);
    const at = `${mid}\u001f${e.row.key}`;
    // Two sources with one manifest id: the first in the Settings order owns it.
    if (!owners.has(at)) owners.set(at, e.row);
    e.row.owner = { sourceId: e.sourceId, manifestId: mid };
    rows.push(e.row);
  }
  /** Per row: the overlays merged into it, in order. */
  const added = new Map<WorkRow, WorkEntry[]>();
  /** Rows made from an overlay whose owner is missing, by what they point at. */
  const orphans = new Map<string, WorkRow>();
  for (const e of overlays) {
    const ref = e.ref!;
    const at = `${ref.source}\u001f${ref.key}`;
    let row = owners.get(at) ?? orphans.get(at);
    if (!row) {
      row = orphanRow(e, sources, listedBy);
      orphans.set(at, row);
      rows.push(row);
      added.set(row, [e]);
      continue;
    }
    // A source adds to a row once; a repeat of its overlay is left out.
    const list = added.get(row) ?? [];
    if (list.some((x) => x.sourceId === e.sourceId) || row.owner?.sourceId === e.sourceId) continue;
    list.push(e);
    added.set(row, list);
  }
  for (const [row, list] of added) applyOverlays(row, list);
  return rows;
}

/** The row of an overlay whose owner is missing: its key, cached title and
 *  url, its own local fields; nothing else a tracker would say. */
function orphanRow(e: WorkEntry, sources: readonly MergeSource[] | undefined, listedBy: ReadonlySet<string>): WorkRow {
  const r = e.row;
  const ref = e.ref!;
  return {
    ...r,
    key: ref.key,
    id: `${e.sourceId}|${ref.key}`,
    type: null,
    status: null,
    statusChip: null,
    priority: null,
    labels: [],
    parent: null,
    reporter: null,
    created: null,
    assignedToMe: false,
    folder: null,
    active: false,
    localStatus: null,
    branches: [],
    prs: [],
    badges: [],
    banners: [],
    contributions: [],
    owner: null,
    ownerRef: ref,
    missingOwner: ownerMissingBadge(ref, sources, listedBy),
    keySourceId: (sources ?? []).find((s) => (s.manifestId ?? s.id) === ref.source)?.id ?? null,
    bannerTone: null,
  };
}

/** Fold the overlays into `row` (an owner's row, or an orphan's, whose own
 *  entry is the first overlay). */
function applyOverlays(row: WorkRow, list: WorkEntry[]): void {
  const ownerPart = row.contributions.filter((c) => c.role === "owner");
  const ownerPrs = row.prs;
  const ownerBadges = row.badges;
  const ownerBanners = row.banners;
  const conflicts = new Map<string, { winner: string; fields: LocalField[] }>();
  const from: Partial<Record<LocalField, WorkEntry>> = {};
  for (const e of list) {
    for (const f of LOCAL_FIELDS) {
      if (!e.sets[f]) continue;
      const first = from[f];
      if (!first) {
        from[f] = e;
        continue;
      }
      if (!same(localValue(first.row, f), localValue(e.row, f))) {
        const c = conflicts.get(e.sourceId) ?? { winner: first.sourceLabel, fields: [] };
        c.fields.push(f);
        conflicts.set(e.sourceId, c);
      }
    }
  }
  if (from.folder) {
    row.folder = from.folder.row.folder;
    row.folderFrom = from.folder.sourceLabel;
  }
  if (from.active) row.active = from.active.row.active;
  if (from.localStatus) {
    row.localStatus = from.localStatus.row.localStatus;
    row.localStatusFrom = { sourceId: from.localStatus.sourceId, sourceLabel: from.localStatus.sourceLabel, key: from.localStatus.row.key };
  }
  if (from.branches) row.branches = from.branches.row.branches;
  const prs: WorkPrChip[] = [...ownerPrs];
  for (const e of list) for (const p of e.row.prs) if (!prs.some((x) => x.id === p.id)) prs.push(p);
  row.prs = prs;
  const badges: Chip[] = [...ownerBadges, ...list.flatMap((e) => e.row.badges)];
  for (const [sourceId, c] of conflicts) {
    const loser = list.find((e) => e.sourceId === sourceId)!.sourceLabel;
    badges.push({
      text: `${c.winner} and ${loser} both set ${c.fields.map(fieldName).join(", ")}`,
      tone: "warn",
      title: `${c.winner} comes first in Settings ▸ Sources, so its ${c.fields.length === 1 ? "value is" : "values are"} used`,
    });
  }
  if (row.missingOwner) badges.push(row.missingOwner);
  row.badges = badges;
  row.banners = [...ownerBanners, ...list.flatMap((e) => e.row.banners)];
  row.bannerTone = worst(row.banners.map((b) => b.banner.tone));
  row.contributions = [...ownerPart, ...list.flatMap((e) => e.row.contributions)];
  row.hasActions = row.contributions.some((c) => c.hasActions);
  if (!row.owner) {
    // An orphan's primary contribution is its first overlay's.
    const first = list[0];
    row.sourceId = first.sourceId;
    row.sourceLabel = first.sourceLabel;
    row.actionIds = first.row.actionIds;
    row.sections = first.row.sections;
  }
}

function localValue(row: WorkRow, f: LocalField): unknown {
  return f === "folder" ? row.folder : f === "active" ? row.active : f === "localStatus" ? row.localStatus : row.branches;
}

function fieldName(f: LocalField): string {
  return f === "localStatus" ? "local status" : f;
}

// ---- item actions on other sources' rows ----------------------------------

/** One source's actions for a row (`source_actions`); `owner` set = the
 *  item actions `sourceId` offers on that other source's row. */
export interface ItemActionTarget {
  sourceId: string;
  sourceLabel: string;
  view: "work" | "prs";
  key: string;
  owner: string;
}

/** The trusted sources whose item actions are offered on a `view` row of
 *  the source `ownerId` (manifest id `ownerManifest`), other than the owner
 *  and `except` (sources that already add to the row bring their own). */
export function itemActionSources(
  view: "work" | "prs",
  ownerId: string,
  ownerManifest: string,
  sources: readonly MergeSource[],
  except: ReadonlySet<string> = new Set(),
): MergeSource[] {
  return sources.filter(
    (s) =>
      s.trusted &&
      s.id !== ownerId &&
      !except.has(s.id) &&
      (s.trust?.itemActions ?? []).some((a) => a.view === view && a.sources.some((x) => x === "*" || x === ownerManifest)),
  );
}

/** The item action targets of a work row: none on a row whose owner is
 *  missing (there is no owner row to act on). */
export function workItemActionTargets(row: WorkRow, sources: readonly MergeSource[]): ItemActionTarget[] {
  if (!row.owner) return [];
  const contributing = new Set(row.contributions.map((c) => c.sourceId));
  return itemActionSources("work", row.owner.sourceId, row.owner.manifestId, sources, contributing).map((s) => ({
    sourceId: s.id,
    sourceLabel: s.label,
    view: "work",
    key: row.key,
    owner: row.owner!.sourceId,
  }));
}

/** The item action targets of a PR row: for each declaring source, the
 *  first of the row's sources it offers them on. */
export function prItemActionTargets(row: PrRow, sources: readonly MergeSource[]): ItemActionTarget[] {
  const out: ItemActionTarget[] = [];
  for (const o of row.origins) {
    const mid = manifestIdOf(o.sourceId, sources);
    for (const s of itemActionSources("prs", o.sourceId, mid, sources)) {
      if (out.some((t) => t.sourceId === s.id) || row.origins.some((x) => x.sourceId === s.id)) continue;
      out.push({ sourceId: s.id, sourceLabel: s.label, view: "prs", key: o.key, owner: o.sourceId });
    }
  }
  return out;
}

// ---- PRs joined to work rows ------------------------------------------------

/** Whether a key pattern can be used, and why not. */
export function keyPatternError(pattern: string): string | null {
  if (pattern.trim() === "") return null;
  try {
    new RegExp(pattern, "g");
  } catch (e) {
    return `not a usable pattern: ${e instanceof Error ? e.message : String(e)}`;
  }
  return null;
}

/** The compiled rule, or null when key matching is off (or the stored
 *  pattern cannot be used). Case-sensitive. */
export function compileKeyMatch(rule: KeyMatchRule | null | undefined): RegExp | null {
  const r = rule ?? { enabled: true, pattern: null };
  if (!r.enabled) return null;
  const pattern = r.pattern && r.pattern.trim() !== "" ? r.pattern.trim() : DEFAULT_KEY_PATTERN;
  if (keyPatternError(pattern)) return null;
  return new RegExp(pattern, "g");
}

const WORD = /[A-Za-z0-9]/;

/** The whole-word texts `re` finds in `text`: a hit that runs on into a
 *  letter or digit (`ABC-1` inside `ABC-12`, `XABC-1`) is not a word. */
export function candidateKeys(text: string, re: RegExp): string[] {
  const out: string[] = [];
  const g = new RegExp(re.source, re.flags.includes("g") ? re.flags : `${re.flags}g`);
  for (let m = g.exec(text); m !== null; m = g.exec(text)) {
    if (m[0] === "") {
      g.lastIndex++;
      continue;
    }
    const before = text[m.index - 1] ?? "";
    const after = text[m.index + m[0].length] ?? "";
    if (!WORD.test(before) && !WORD.test(after)) out.push(m[0]);
  }
  return out;
}

/** A branch name as a forge's API and git both spell it. */
export const branchName = (b: string): string => b.replace(/^refs\/heads\//, "");

export interface JoinedPr {
  pr: PrRow;
  how: JoinHow;
}

export interface WorkJoins {
  /** Per work row id: the PRs it gained beyond its own linked list. */
  byRow: Map<string, JoinedPr[]>;
  /** Per PR id: the work rows it joined, in row order. */
  byPr: Map<string, { rowId: string; key: string; title: string; how: JoinHow }[]>;
}

/**
 * Join PR rows to work rows by the three rules. `keyMatch(sourceId)` is the
 * rule of the source whose keys a row carries (its owner, or for a row
 * whose owner is missing, the source its overlays point at when that one is
 * set up here); a row with no such source uses the default rule.
 */
export function joinPrs(rows: readonly WorkRow[], prs: readonly PrRow[], keyMatch: (sourceId: string | null) => KeyMatchRule | null | undefined): WorkJoins {
  const byRow = new Map<string, JoinedPr[]>();
  const byPr: WorkJoins["byPr"] = new Map();
  const compiled = new Map<string | null, RegExp | null>();
  const ruleOf = (sourceId: string | null): RegExp | null => {
    if (!compiled.has(sourceId)) compiled.set(sourceId, compileKeyMatch(keyMatch(sourceId)));
    return compiled.get(sourceId)!;
  };
  const found = new Map<string, { branch: Set<string>; title: Set<string> }>();
  const tokens = (pr: PrRow, sourceId: string | null) => {
    const at = `${pr.id}\u001f${sourceId ?? ""}`;
    let t = found.get(at);
    if (!t) {
      const re = ruleOf(sourceId);
      t = re
        ? { branch: new Set(candidateKeys(branchName(pr.sourceBranch ?? ""), re)), title: new Set(candidateKeys(pr.title, re)) }
        : { branch: new Set(), title: new Set() };
      found.set(at, t);
    }
    return t;
  };
  for (const row of rows) {
    const linked = new Set(row.prs.map((p) => p.id));
    const branches = row.branches.map((b) => ({ id: prId(b.forge, b.repo, ""), branch: branchName(b.branch) }));
    const keySource = row.owner?.sourceId ?? row.keySourceId;
    for (const pr of prs) {
      let how: JoinHow | null = null;
      if (linked.has(pr.id)) how = "linked";
      else if (pr.sourceBranch && branches.some((b) => b.id === prId(pr.forge, pr.repo, "") && b.branch === branchName(pr.sourceBranch!))) how = "branch";
      else {
        const t = tokens(pr, keySource);
        if (t.branch.has(row.key)) how = "key in branch";
        else if (t.title.has(row.key)) how = "key in title";
      }
      if (!how) continue;
      if (how !== "linked") {
        const list = byRow.get(row.id) ?? [];
        list.push({ pr, how });
        byRow.set(row.id, list);
      }
      const on = byPr.get(pr.id) ?? [];
      on.push({ rowId: row.id, key: row.key, title: row.hasTitle ? row.title : row.key, how });
      byPr.set(pr.id, on);
    }
  }
  return { byRow, byPr };
}
