// The PRs view's model: every trusted source's PRs payload merged into one
// list, split into the My / Reviewing tabs and grouped by bucket. Pure (no
// DOM, no ipc): the view and the Needs you model both read it, and the tests
// feed it the fixture payloads.
//
// A payload arrives exactly as the source sent it, so nothing here trusts its
// shape: a row missing a required field is skipped and counted, an optional
// field that is absent or of the wrong type reads as absent, and unknown
// fields are ignored.

import type { Badge, Bucket, Freshness, Pr } from "./types";

/** The contract version these views speak. */
export const CONTRACT = 1;

export type Tone = "info" | "ok" | "warn" | "bad" | "muted";

/** A small coloured chip on a row. */
export interface Chip {
  text: string;
  tone: Tone;
  title?: string;
}

/** One source's cached payload of a view, as the data layer hands it over. */
export interface ViewPayload {
  sourceId: string;
  sourceLabel: string;
  /** The raw body (null until the first successful poll). */
  payload: unknown;
  /** The app's own poll bookkeeping: `errors` holds the last poll's failure. */
  freshness?: Freshness | null;
}

/** A source whose contract this app does not speak. */
export interface NeedsUpdate {
  sourceId: string;
  sourceLabel: string;
}

/** Where a merged row's actions come from: run each against its own source. */
export interface PrOrigin {
  sourceId: string;
  sourceLabel: string;
  /** The key the source's actions are addressed by (see {@link prActionKey}). */
  key: string;
  /** Whether this source's row carried any actions at all. */
  hasActions: boolean;
}

export interface PrRow {
  /** Stable across polls and sources: {@link prId}. */
  id: string;
  forge: string;
  repo: string;
  number: string;
  url: string;
  title: string;
  author: string;
  /** "feature → main", or null when the source sent neither branch. */
  branch: string | null;
  state: Pr["state"];
  role: Pr["role"];
  bucket: Bucket | "other";
  /** One line each, markdown links reduced to their text. */
  reasons: string[];
  chips: Chip[];
  /** The source whose payload supplied the fields (the fresher one). */
  primary: PrOrigin;
  /** Every source that served the row, the primary first. */
  origins: PrOrigin[];
}

export interface PrGroup {
  bucket: Bucket | "other";
  label: string;
  /** Collapsed by default with its count (the `waiting` bucket). */
  folded: boolean;
  rows: PrRow[];
}

/** What the freshness line shows for a view fed by several sources. */
export interface FreshnessSummary {
  /** The OLDEST as-of among the sources that have data: the list is at
   *  least this current. Null when none has data yet. */
  asOf: string | null;
  refreshing: boolean;
  /** Quiet lines ("showing 50 of 212"). */
  notices: string[];
  /** Warning lines: a poll that failed or a source's partial failure. */
  errors: string[];
}

export interface PrsModel {
  mine: PrGroup[];
  reviewing: PrGroup[];
  /** Rows per tab (the tab labels show them). */
  counts: { mine: number; reviewing: number };
  freshness: FreshnessSummary;
  needsUpdate: NeedsUpdate[];
  /** At least one source has served a payload. */
  hasData: boolean;
}

/** Bucket order and labels per role, as the source contract's table lists them. */
export const BUCKETS: Record<Pr["role"], { bucket: Bucket; label: string; needsYou: "yes" | "folded" | "no" }[]> = {
  mine: [
    { bucket: "action", label: "Blocked on me", needsYou: "yes" },
    { bucket: "draft", label: "Still a draft", needsYou: "yes" },
    { bucket: "ready", label: "Approved and green", needsYou: "yes" },
    { bucket: "waiting", label: "Waiting on reviewers", needsYou: "folded" },
    { bucket: "recent", label: "Merged or closed lately", needsYou: "no" },
  ],
  reviewing: [
    { bucket: "replied", label: "The author answered my thread", needsYou: "yes" },
    { bucket: "vote", label: "Required reviewer, not voted", needsYou: "yes" },
    { bucket: "mythreads", label: "I commented; nothing new", needsYou: "no" },
    { bucket: "voted", label: "I voted; nothing new", needsYou: "no" },
    { bucket: "group", label: "Only via a group", needsYou: "no" },
  ],
};

/** Buckets that fold by default: present, counted, out of the way. */
const FOLDED: ReadonlySet<string> = new Set(["waiting"]);

/** Where a PR stands for Needs you. A bucket this app does not know (a later
 *  contract addition) or one in the wrong role is "no". */
export function needsYouOf(role: Pr["role"], bucket: string): "yes" | "folded" | "no" {
  return BUCKETS[role].find((b) => b.bucket === bucket)?.needsYou ?? "no";
}

/** The identity two sources agree on: forge and repo (case folded, as
 *  forges treat them) and the number as shown. */
export function prId(forge: string, repo: string, number: string): string {
  return `${forge.toLowerCase()}/${repo.toLowerCase()}/${number}`;
}

/** The key the app addresses a PR's actions by: `forge:repo:number`, the
 *  fields exactly as the source sent them (a bare number repeats across
 *  repositories). */
export function prActionKey(forge: string, repo: string, number: string): string {
  return `${forge}:${repo}:${number}`;
}

/** `[text](url)` becomes `text`; images become their alt text; `<url>`
 *  autolinks become the url. A source's text is shown as plain text, so the
 *  markup would only be noise (and a link in it would not be checked). */
export function stripMarkdownLinks(text: string): string {
  return text
    .replace(/!?\[([^\]]*)\]\((?:[^()\s]|\([^()\s]*\))*(?:\s+"[^"]*")?\)/g, "$1")
    .replace(/<((?:https?|mailto):[^>\s]+)>/g, "$1");
}

/** One display line: markdown links stripped, line breaks and runs of
 *  whitespace collapsed. */
export function oneLine(text: string): string {
  return stripMarkdownLinks(text).replace(/\s+/g, " ").trim();
}

// ---- reading a payload --------------------------------------------------

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);
const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

/** The payload's own freshness, read leniently. */
export function readFreshness(v: unknown): Freshness {
  const f = obj(v) ?? {};
  const errors = list(f.errors)
    .map(obj)
    .filter((e): e is Record<string, unknown> => e !== null && typeof e.message === "string")
    .map((e) => ({ source: str(e.source) ?? "", message: e.message as string }));
  const notices = list(f.notices).filter((n): n is string => typeof n === "string" && n.trim() !== "");
  return {
    asOf: str(f.asOf),
    refreshing: f.refreshing === true,
    nextUtc: str(f.nextUtc),
    ...(errors.length ? { errors } : {}),
    ...(notices.length ? { notices } : {}),
  };
}

/** Whether a payload declares a contract this app does not speak. A payload
 *  without the field is fine (the manifest carries the version). */
export function wrongContract(payload: unknown): boolean {
  const p = obj(payload);
  return p !== null && "contract" in p && p.contract !== CONTRACT;
}

/** The items of a list payload (an absent or broken list reads as empty). */
export function itemsOf(payload: unknown): unknown[] {
  return list(obj(payload)?.items);
}

const STATES = new Set(["open", "draft", "merged", "closed"]);
const ROLES = new Set(["mine", "reviewing"]);
const TONES = new Set(["info", "ok", "warn", "bad", "muted"]);
const VOTES = new Set(["approved", "approved-with-suggestions", "waiting", "rejected", "none"]);
const CHECKS = new Set(["passing", "failing", "pending", "none"]);
const MERGEABLE = new Set(["yes", "conflicts", "unknown"]);

/** One PR read leniently: the fields a row cannot do without (forge, repo,
 *  number, title, role) must be strings of the right kind, or the row is
 *  skipped; the rest default. Null for an unusable entry. */
export function readPr(raw: unknown): Pr | null {
  const p = obj(raw);
  if (!p) return null;
  const forge = str(p.forge);
  const repo = str(p.repo);
  const number = str(p.number);
  const title = str(p.title);
  const role = str(p.role);
  if (!forge || !repo || !number || title === null || !role || !ROLES.has(role)) return null;
  const author = obj(p.author);
  const me = obj(p.me);
  const checks = obj(p.checks);
  const threads = obj(p.threads);
  const state = str(p.state);
  const mergeable = str(p.mergeable);
  return {
    forge,
    repo,
    project: str(p.project),
    number,
    url: str(p.url) ?? "",
    title,
    author: { name: str(author?.name) ?? "", isMe: author?.isMe === true, ...(str(author?.id) ? { id: str(author?.id)! } : {}) },
    ...(str(p.sourceBranch) !== null ? { sourceBranch: str(p.sourceBranch)! } : {}),
    ...(str(p.targetBranch) !== null ? { targetBranch: str(p.targetBranch)! } : {}),
    state: (state && STATES.has(state) ? state : "open") as Pr["state"],
    closedUtc: str(p.closedUtc),
    role: role as Pr["role"],
    bucket: (str(p.bucket) ?? "") as Bucket,
    reasons: list(p.reasons).filter((r): r is string => typeof r === "string"),
    me: me
      ? {
          required: me.required === true,
          vote: (str(me.vote) && VOTES.has(me.vote as string) ? me.vote : "none") as NonNullable<Pr["me"]>["vote"],
        }
      : null,
    reviewers: list(p.reviewers)
      .map(obj)
      .filter((r): r is Record<string, unknown> => r !== null && typeof r.name === "string")
      .map((r) => ({
        name: r.name as string,
        required: r.required === true,
        isGroup: r.isGroup === true,
        ...(str(r.vote) ? { vote: r.vote as string } : {}),
      })),
    checks:
      checks && str(checks.state) && CHECKS.has(checks.state as string)
        ? {
            state: checks.state as NonNullable<Pr["checks"]>["state"],
            failing: list(checks.failing).filter((x): x is string => typeof x === "string"),
            pending: list(checks.pending).filter((x): x is string => typeof x === "string"),
          }
        : null,
    threads: threads
      ? { active: num(threads.active) ?? 0, resolved: num(threads.resolved) ?? 0, needsMe: num(threads.needsMe) ?? 0 }
      : null,
    mergeable: (mergeable && MERGEABLE.has(mergeable) ? mergeable : null) as Pr["mergeable"],
    workKeys: list(p.workKeys).filter((k): k is string => typeof k === "string"),
    badges: readBadges(p.badges),
    actions: list(p.actions) as Pr["actions"],
  };
}

/** A source's badges, leniently: an unknown tone reads as "info". */
export function readBadges(v: unknown): Badge[] {
  return list(v)
    .map(obj)
    .filter((b): b is Record<string, unknown> => b !== null && typeof b.text === "string" && b.text.trim() !== "")
    .map((b) => ({
      text: b.text as string,
      tone: (str(b.tone) && TONES.has(b.tone as string) ? b.tone : "info") as Tone,
      ...(str(b.title) ? { title: b.title as string } : {}),
    }));
}

// ---- chips --------------------------------------------------------------

const plural = (n: number, one: string, many = `${one}s`): string => `${n} ${n === 1 ? one : many}`;

/** The chips a row shows, from the facts the source reported: checks,
 *  threads, mergeability, votes, the state when not open, then the source's
 *  own badges (a badge repeating a chip's text is dropped). */
export function chipsFor(pr: Pr): Chip[] {
  const chips: Chip[] = [];
  if (pr.state !== "open") chips.push({ text: pr.state, tone: "muted" });
  const c = pr.checks;
  if (c?.state === "failing") {
    const names = c.failing ?? [];
    chips.push({ text: names.length ? `${plural(names.length, "check")} failing` : "checks failing", tone: "bad", ...(names.length ? { title: names.join(", ") } : {}) });
  } else if (c?.state === "pending") {
    const names = c.pending ?? [];
    chips.push({ text: "checks pending", tone: "warn", ...(names.length ? { title: names.join(", ") } : {}) });
  } else if (c?.state === "passing") {
    chips.push({ text: "checks passing", tone: "ok" });
  }
  const t = pr.threads;
  if (t && t.needsMe > 0) chips.push({ text: `${plural(t.needsMe, "thread")} for me`, tone: "warn", title: `${t.active} open, ${t.resolved} resolved` });
  else if (t && t.active > 0) chips.push({ text: `${plural(t.active, "open thread")}`, tone: "muted", title: `${t.resolved} resolved` });
  if (pr.mergeable === "conflicts") chips.push({ text: "conflicts", tone: "bad" });
  const vote = pr.me?.vote;
  if (pr.role === "reviewing" && pr.me?.required && (vote === "none" || vote === undefined)) chips.push({ text: "required", tone: "info", title: "your review is required" });
  if (vote === "approved") chips.push({ text: "you approved", tone: "ok" });
  else if (vote === "approved-with-suggestions") chips.push({ text: "you approved with suggestions", tone: "ok" });
  else if (vote === "waiting") chips.push({ text: "you voted: wait for author", tone: "warn" });
  else if (vote === "rejected") chips.push({ text: "you rejected", tone: "bad" });
  const others = (pr.reviewers ?? []).filter((r) => !r.isGroup);
  const count = (v: string): string[] => others.filter((r) => r.vote === v).map((r) => r.name);
  const approved = [...count("approved"), ...count("approved-with-suggestions")];
  const rejected = count("rejected");
  const waiting = count("waiting");
  if (approved.length) chips.push({ text: `${approved.length} approved`, tone: "ok", title: approved.join(", ") });
  if (rejected.length) chips.push({ text: `${rejected.length} rejected`, tone: "bad", title: rejected.join(", ") });
  if (waiting.length) chips.push({ text: `${waiting.length} waiting for author`, tone: "warn", title: waiting.join(", ") });
  const seen = new Set(chips.map((x) => x.text.toLowerCase()));
  for (const b of pr.badges ?? []) {
    if (seen.has(b.text.toLowerCase())) continue;
    seen.add(b.text.toLowerCase());
    chips.push({ text: b.text, tone: (b.tone ?? "info") as Tone, ...(b.title ? { title: b.title } : {}) });
  }
  return chips;
}

// ---- merging ------------------------------------------------------------

/** Milliseconds of an as-of; an absent or unreadable one is the oldest. */
function asOfMs(asOf: string | null | undefined): number {
  const ms = asOf ? Date.parse(asOf) : NaN;
  return Number.isFinite(ms) ? ms : -Infinity;
}

/** Fold the per-source freshness into one line's worth. With more than one
 *  source, each line is prefixed with its source's label. */
export function summarizeFreshness(payloads: ViewPayload[]): FreshnessSummary {
  const prefix = (label: string, text: string): string => (payloads.length > 1 ? `${label}: ${text}` : text);
  let oldest: number | null = null;
  let asOf: string | null = null;
  let refreshing = false;
  const notices: string[] = [];
  const errors: string[] = [];
  for (const p of payloads) {
    const app = p.freshness ?? null;
    for (const e of app?.errors ?? []) errors.push(prefix(p.sourceLabel, oneLine(e.message)));
    if (app?.refreshing) refreshing = true;
    if (p.payload === null || p.payload === undefined) continue;
    const own = readFreshness(obj(p.payload)?.freshness);
    if (own.refreshing) refreshing = true;
    for (const n of own.notices ?? []) notices.push(prefix(p.sourceLabel, oneLine(n)));
    for (const e of own.errors ?? []) errors.push(prefix(p.sourceLabel, oneLine(e.message)));
    const ms = asOfMs(own.asOf ?? app?.asOf);
    if (ms !== -Infinity && (oldest === null || ms < oldest)) {
      oldest = ms;
      asOf = own.asOf ?? app?.asOf ?? null;
    }
  }
  return { asOf, refreshing, notices, errors };
}

/** Rows that could not be read, as one warning line per source. */
function skippedLine(label: string, n: number, many: boolean): string {
  const text = `${plural(n, "pull request")} could not be read`;
  return many ? `${label}: ${text}` : text;
}

/**
 * Merge the PRs payloads of every trusted source serving the view. The same
 * PR from two sources is one row: the payload with the fresher
 * `freshness.asOf` supplies its fields (the first source wins a tie), and
 * every source that served it is an origin whose actions the row offers.
 */
export function buildPrsModel(payloads: ViewPayload[], needsUpdate: NeedsUpdate[] = []): PrsModel {
  const stale: NeedsUpdate[] = [...needsUpdate];
  const usable: ViewPayload[] = [];
  for (const p of payloads) {
    if (wrongContract(p.payload)) {
      if (!stale.some((s) => s.sourceId === p.sourceId)) stale.push({ sourceId: p.sourceId, sourceLabel: p.sourceLabel });
    } else {
      usable.push(p);
    }
  }
  const freshness = summarizeFreshness(usable);
  const rows = new Map<string, { row: PrRow; asOf: number }>();
  for (const p of usable) {
    if (p.payload === null || p.payload === undefined) continue;
    const asOf = asOfMs(readFreshness(obj(p.payload)?.freshness).asOf ?? p.freshness?.asOf);
    let skipped = 0;
    for (const raw of itemsOf(p.payload)) {
      const pr = readPr(raw);
      if (!pr) {
        skipped++;
        continue;
      }
      const id = prId(pr.forge, pr.repo, pr.number);
      const origin: PrOrigin = {
        sourceId: p.sourceId,
        sourceLabel: p.sourceLabel,
        key: prActionKey(pr.forge, pr.repo, pr.number),
        hasActions: (pr.actions ?? []).length > 0,
      };
      const seen = rows.get(id);
      if (!seen) {
        rows.set(id, { row: toRow(id, pr, origin, [origin]), asOf });
        continue;
      }
      // The same source serving the PR twice (both roles) keeps its first.
      if (seen.row.origins.some((o) => o.sourceId === p.sourceId)) continue;
      if (asOf > seen.asOf) {
        rows.set(id, { row: toRow(id, pr, origin, [origin, ...seen.row.origins]), asOf });
      } else {
        seen.row.origins.push(origin);
      }
    }
    if (skipped > 0) freshness.errors.push(skippedLine(p.sourceLabel, skipped, usable.length > 1));
  }
  const all = [...rows.values()].map((r) => r.row);
  const mine = groupsFor("mine", all.filter((r) => r.role === "mine"));
  const reviewing = groupsFor("reviewing", all.filter((r) => r.role === "reviewing"));
  return {
    mine,
    reviewing,
    counts: { mine: all.filter((r) => r.role === "mine").length, reviewing: all.filter((r) => r.role === "reviewing").length },
    freshness,
    needsUpdate: stale,
    hasData: usable.some((p) => p.payload !== null && p.payload !== undefined),
  };
}

function toRow(id: string, pr: Pr, primary: PrOrigin, origins: PrOrigin[]): PrRow {
  const known = BUCKETS[pr.role].some((b) => b.bucket === pr.bucket);
  const branch =
    pr.sourceBranch || pr.targetBranch ? `${pr.sourceBranch ?? "?"} → ${pr.targetBranch ?? "?"}` : null;
  return {
    id,
    forge: pr.forge,
    repo: pr.repo,
    number: pr.number,
    url: pr.url,
    title: oneLine(pr.title) || `${pr.repo}${pr.number}`,
    author: pr.author.name,
    branch,
    state: pr.state,
    role: pr.role,
    bucket: known ? pr.bucket : "other",
    reasons: pr.reasons.map(oneLine).filter(Boolean),
    chips: chipsFor(pr),
    primary,
    origins,
  };
}

/** A role's rows grouped in table order; empty groups are left out, and a
 *  bucket this app does not know lands in a trailing "Other" group. */
function groupsFor(role: Pr["role"], rows: PrRow[]): PrGroup[] {
  const groups: PrGroup[] = [];
  for (const b of BUCKETS[role]) {
    const inBucket = rows.filter((r) => r.bucket === b.bucket);
    if (inBucket.length) groups.push({ bucket: b.bucket, label: b.label, folded: FOLDED.has(b.bucket), rows: inBucket });
  }
  const other = rows.filter((r) => r.bucket === "other");
  if (other.length) groups.push({ bucket: "other", label: "Other", folded: false, rows: other });
  return groups;
}

/** The rows of a model that need the user ("yes" buckets), and the folded
 *  ones (shown, not counted). */
export function prsNeedingYou(model: PrsModel): { yes: PrRow[]; folded: PrRow[] } {
  const yes: PrRow[] = [];
  const folded: PrRow[] = [];
  for (const g of [...model.mine, ...model.reviewing]) {
    for (const r of g.rows) {
      if (r.bucket === "other") continue;
      const n = needsYouOf(r.role, r.bucket);
      if (n === "yes") yes.push(r);
      else if (n === "folded") folded.push(r);
    }
  }
  return { yes, folded };
}
