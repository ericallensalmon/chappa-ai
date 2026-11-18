// The PRs view's model: every trusted source's PRs payload merged into one
// list, split into three tabs, and the rule for which PRs need the user.
// Pure (no DOM, no ipc): the view, the Needs you model and the rail count
// all read it, and the tests feed it the fixture payloads.
//
// The tabs:
//  - My PRs: every PR I authored, grouped by bucket;
//  - Reviewing: the reviews I am actively on: required of me by name, or
//    ones I already engaged with (replied, mythreads, voted); open or draft;
//  - All: every open or draft review, grouped by how I am on it (required
//    by name, optional, or one group per team that brought it to me): the
//    inbox to pull from when there is time.
// A PR may sit in Reviewing and All both; its bucket chip shows its state
// in either.
//
// A payload arrives exactly as the source sent it, so nothing here trusts its
// shape: a row missing a required field is skipped and counted, an optional
// field that is absent or of the wrong type reads as absent, and unknown
// fields are ignored.

import { readBanners, readSections } from "./sections";
import type { Badge, Banner, Bucket, Freshness, Pr, Section } from "./types";

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
  /** Whether this source's row carried any actions at all (its sections'
   *  included). */
  hasActions: boolean;
  /** The ids of the row's own actions (not its sections'). */
  actionIds: string[];
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
  /** The source branch as the source sent it (what work rows join on). */
  sourceBranch: string | null;
  state: Pr["state"];
  role: Pr["role"];
  bucket: Bucket | "other";
  /** One line each, markdown links reduced to their text. */
  reasons: string[];
  chips: Chip[];
  /** My review is required by name (`me.required`), as the fresher
   *  payload says: two sources that disagree do not merge their `me`. */
  required: boolean;
  /** The teams (`reviewers[].isGroup`) on the PR, in the source's order. */
  teams: string[];
  /** Whether it needs the user: {@link attentionOf}. */
  attention: "yes" | "folded" | "no";
  /** A grouping above the repo (forges that have one), or null. */
  project: string | null;
  /** The STATUS column: the bucket chip, then the PR's state, checks,
   *  conflicts and the source's own badges (votes and threads have their
   *  own columns). */
  status: Chip[];
  /** The VOTES column. */
  votes: PrVotes;
  /** The COMMENTS column, or null when the source sent no thread counts. */
  comments: { text: string; forMe: number } | null;
  /** The reasons that say something no chip already says: a reason that
   *  only restates a badge ("still a draft", "conflicts with main") is
   *  left to the badge. */
  why: string[];
  /** When the PR last moved (else was opened, else closed): its age. */
  ageUtc: string | null;
  /** The reviewers, people and teams, with their votes. */
  reviewers: { name: string; vote: string; required: boolean; isGroup: boolean }[];
  /** The primary source's sections; their actions are the primary's. */
  sections: Section[];
  /** The source whose payload supplied the fields (the fresher one). */
  primary: PrOrigin;
  /** Every source that served the row, the primary first. */
  origins: PrOrigin[];
}

/** What the VOTES column says. */
export interface PrVotes {
  /** "2 approved · 1 rejected", or "0 approved". */
  summary: string;
  /** My own vote on a review ("me: no vote (required)"); null on mine. */
  me: string | null;
  tone: Tone;
  /** Who voted what, one per line. */
  title: string;
}

export interface PrGroup {
  /** Stable within its tab: the bucket for bucket groups; `required`,
   *  `optional`, `team:<name, case folded>` or `team` in the All tab. */
  id: string;
  /** Set on bucket groups (My PRs, Reviewing). */
  bucket?: Bucket | "other";
  label: string;
  /** Collapsed by default with its count (the `waiting` bucket, and the
   *  team groups of the All tab). */
  folded: boolean;
  rows: PrRow[];
}

export type PrsTab = "mine" | "reviewing" | "all";

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
  all: PrGroup[];
  /** Rows per tab (the tab labels show them). */
  counts: Record<PrsTab, number>;
  freshness: FreshnessSummary;
  needsUpdate: NeedsUpdate[];
  /** The view-level banners, per source. */
  banners: { sourceId: string; sourceLabel: string; banner: Banner }[];
  /** At least one source has served a payload. */
  hasData: boolean;
}

/** Whether a bucket needs the user: always, shown but not counted, only
 *  while my review is required by name, or never. */
export type BucketNeed = "yes" | "folded" | "required" | "no";

export interface BucketInfo {
  bucket: Bucket;
  /** The group heading. */
  label: string;
  needsYou: BucketNeed;
  /** The chip every row in the bucket carries, so its state shows in any tab. */
  chip: Chip;
}

/** Bucket order, labels and chips per role ("needs me" buckets first). */
export const BUCKETS: Record<Pr["role"], BucketInfo[]> = {
  mine: [
    { bucket: "action", label: "Blocked on me", needsYou: "yes", chip: { text: "blocked on me", tone: "bad" } },
    { bucket: "draft", label: "Still a draft", needsYou: "yes", chip: { text: "draft", tone: "warn" } },
    { bucket: "ready", label: "Approved and green", needsYou: "yes", chip: { text: "ready to merge", tone: "ok" } },
    { bucket: "waiting", label: "Waiting on reviewers", needsYou: "folded", chip: { text: "waiting on reviewers", tone: "muted" } },
    { bucket: "recent", label: "Merged or closed lately", needsYou: "no", chip: { text: "closed lately", tone: "muted" } },
  ],
  reviewing: [
    { bucket: "replied", label: "The author answered my thread", needsYou: "yes", chip: { text: "author replied", tone: "warn" } },
    { bucket: "vote", label: "My vote is asked for", needsYou: "required", chip: { text: "vote asked", tone: "info" } },
    { bucket: "mythreads", label: "I commented; nothing new", needsYou: "no", chip: { text: "you commented", tone: "muted" } },
    { bucket: "voted", label: "I voted; nothing new", needsYou: "no", chip: { text: "you voted", tone: "muted" } },
    { bucket: "group", label: "Only via a team", needsYou: "no", chip: { text: "via a team", tone: "muted" } },
  ],
};

/** The reviewing buckets that mean I already engaged with the PR. */
const ENGAGED: ReadonlySet<string> = new Set(["replied", "mythreads", "voted"]);

/** Buckets that fold by default: present, counted, out of the way. */
const FOLDED: ReadonlySet<string> = new Set(["waiting"]);

/** Where a bucket stands for Needs you, given whether my review is
 *  required by name. A bucket this app does not know (a later contract
 *  addition) or one in the wrong role is "no". */
export function needsYouOf(role: Pr["role"], bucket: string, required = false): "yes" | "folded" | "no" {
  const need = BUCKETS[role].find((b) => b.bucket === bucket)?.needsYou ?? "no";
  if (need === "required") return required ? "yes" : "no";
  return need;
}

const isOpen = (state: Pr["state"]): boolean => state === "open" || state === "draft";

/**
 * Whether a PR needs the user (Needs you, its badge, the PRs rail count,
 * notifications):
 *  - mine: `action`, `draft`, `ready`; `waiting` is shown folded, never
 *    counted; `recent` never;
 *  - reviewing, open or draft only: `replied` (the author answered me), or
 *    `vote` while my review is required by name. `mythreads` / `voted` mean
 *    the ball is with the author. An optional or team-only review never
 *    needs the user: it waits in the All tab.
 */
export function attentionOf(pr: { role: Pr["role"]; bucket: string; required: boolean; state: Pr["state"] }): "yes" | "folded" | "no" {
  if (pr.role === "reviewing" && !isOpen(pr.state)) return "no";
  return needsYouOf(pr.role, pr.bucket, pr.required);
}

/** Whether a review belongs in the Reviewing tab: open or draft, and
 *  required of me by name or already engaged with. */
export function inReviewing(row: Pick<PrRow, "role" | "state" | "required" | "bucket">): boolean {
  return row.role === "reviewing" && isOpen(row.state) && (row.required || ENGAGED.has(row.bucket));
}

/** Whether a review belongs in the All tab: every open or draft one. */
export function inAll(row: Pick<PrRow, "role" | "state">): boolean {
  return row.role === "reviewing" && isOpen(row.state);
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
    created: str(p.created),
    lastTouchedUtc: str(p.lastTouchedUtc),
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
    sections: readSections(p.sections),
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
  const { facts, threads, conflicts, votes, badges } = chipParts(pr);
  return [...facts, ...threads, ...conflicts, ...votes, ...badges];
}

/** The row's chips by where the table shows them: the STATUS column
 *  (state, checks, conflicts, the source's badges), COMMENTS (threads)
 *  and VOTES. A badge repeating a chip's text is dropped. */
export function chipParts(pr: Pr): { facts: Chip[]; threads: Chip[]; conflicts: Chip[]; votes: Chip[]; badges: Chip[] } {
  const facts: Chip[] = [];
  const threads: Chip[] = [];
  const conflicts: Chip[] = [];
  const votes: Chip[] = [];
  const badges: Chip[] = [];
  const chips = facts;
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
  if (t && t.needsMe > 0) threads.push({ text: `${plural(t.needsMe, "thread")} for me`, tone: "warn", title: `${t.active} open, ${t.resolved} resolved` });
  else if (t && t.active > 0) threads.push({ text: `${plural(t.active, "open thread")}`, tone: "muted", title: `${t.resolved} resolved` });
  if (pr.mergeable === "conflicts") conflicts.push({ text: "conflicts", tone: "bad" });
  const vote = pr.me?.vote;
  if (pr.role === "reviewing" && pr.me?.required && (vote === "none" || vote === undefined)) votes.push({ text: "required", tone: "info", title: "your review is required" });
  if (vote === "approved") votes.push({ text: "you approved", tone: "ok" });
  else if (vote === "approved-with-suggestions") votes.push({ text: "you approved with suggestions", tone: "ok" });
  else if (vote === "waiting") votes.push({ text: "you voted: wait for author", tone: "warn" });
  else if (vote === "rejected") votes.push({ text: "you rejected", tone: "bad" });
  const others = (pr.reviewers ?? []).filter((r) => !r.isGroup);
  const count = (v: string): string[] => others.filter((r) => r.vote === v).map((r) => r.name);
  const approved = [...count("approved"), ...count("approved-with-suggestions")];
  const rejected = count("rejected");
  const waiting = count("waiting");
  if (approved.length) votes.push({ text: `${approved.length} approved`, tone: "ok", title: approved.join(", ") });
  if (rejected.length) votes.push({ text: `${rejected.length} rejected`, tone: "bad", title: rejected.join(", ") });
  if (waiting.length) votes.push({ text: `${waiting.length} waiting for author`, tone: "warn", title: waiting.join(", ") });
  const seen = new Set([...facts, ...threads, ...conflicts, ...votes].map((x) => x.text.toLowerCase()));
  for (const b of pr.badges ?? []) {
    if (seen.has(b.text.toLowerCase())) continue;
    seen.add(b.text.toLowerCase());
    badges.push({ text: b.text, tone: (b.tone ?? "info") as Tone, ...(b.title ? { title: b.title } : {}) });
  }
  return { facts, threads, conflicts, votes, badges };
}

/** The facts a line of text speaks of, as words a chip would use too. */
const FACT_WORDS: [RegExp, string][] = [
  [/\bdraft\b/i, "draft"],
  [/conflict/i, "conflicts"],
  [/\bchecks?\b|\bpolic(y|ies)\b|\bbuilds?\b/i, "checks"],
  [/thread|repl(y|ied|ies)|answer|comment/i, "threads"],
  [/approv/i, "approved"],
  [/reject|changes requested|requested changes/i, "rejected"],
  [/\bvot(e|ed|es)\b|required/i, "vote"],
  [/\bmerged\b|\bclosed\b|abandon/i, "closed"],
  [/\bteams?\b|\bvia\b|\bthrough\b/i, "team"],
  [/\bwait(s|ing)?\b/i, "waiting"],
];

export function factsOf(text: string): Set<string> {
  return new Set(FACT_WORDS.filter(([re]) => re.test(text)).map(([, f]) => f));
}

/** Whether a reason only restates what the chips already show: every fact
 *  it speaks of is one of theirs (a reason with nothing recognisable, or
 *  with a fact no chip has, is kept). */
export function restatesChips(reason: string, chips: readonly Chip[]): boolean {
  const said = factsOf(reason);
  if (said.size === 0) return false;
  const shown = factsOf(chips.map((c) => c.text).join(" · "));
  return [...said].every((f) => shown.has(f));
}

const VOTE_TEXT: Record<string, string> = {
  approved: "approved",
  "approved-with-suggestions": "approved with suggestions",
  waiting: "waiting for author",
  rejected: "rejected",
  none: "no vote",
};

/** The VOTES column of a PR. */
export function votesOf(pr: Pr): PrVotes {
  const people = (pr.reviewers ?? []).filter((r) => !r.isGroup);
  const by = (v: string): number => people.filter((r) => r.vote === v).length;
  const approved = by("approved") + by("approved-with-suggestions");
  const parts = [`${approved} approved`];
  if (by("rejected")) parts.push(`${by("rejected")} rejected`);
  if (by("waiting")) parts.push(`${by("waiting")} waiting`);
  const required = people.filter((r) => r.required).length;
  if (required) parts.push(`${required} required`);
  const mine = pr.role === "reviewing" && pr.me ? pr.me : null;
  const myVote = mine?.vote ?? "none";
  const me = mine ? `me: ${VOTE_TEXT[myVote] ?? myVote}${mine.required ? " (required)" : ""}` : null;
  const tone: Tone = by("rejected") || myVote === "rejected" ? "bad" : mine?.required && myVote === "none" ? "info" : approved ? "ok" : "muted";
  const title = (pr.reviewers ?? []).map((r) => `${r.name}${r.isGroup ? " (team)" : ""}: ${VOTE_TEXT[r.vote ?? "none"] ?? r.vote}${r.required ? ", required" : ""}`).join("\n");
  return { summary: parts.join(" · "), me, tone, title };
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
  const banners: PrsModel["banners"] = [];
  for (const p of usable) {
    if (p.payload === null || p.payload === undefined) continue;
    for (const banner of readBanners(obj(p.payload)?.banners)) banners.push({ sourceId: p.sourceId, sourceLabel: p.sourceLabel, banner });
    const asOf = asOfMs(readFreshness(obj(p.payload)?.freshness).asOf ?? p.freshness?.asOf);
    let skipped = 0;
    for (const raw of itemsOf(p.payload)) {
      const pr = readPr(raw);
      if (!pr) {
        skipped++;
        continue;
      }
      const id = prId(pr.forge, pr.repo, pr.number);
      const actionIds = (pr.actions ?? []).map((a) => str(obj(a)?.id)).filter((id): id is string => !!id);
      const origin: PrOrigin = {
        sourceId: p.sourceId,
        sourceLabel: p.sourceLabel,
        key: prActionKey(pr.forge, pr.repo, pr.number),
        hasActions: actionIds.length > 0 || (pr.sections ?? []).some((s) => (s.actions ?? []).length > 0),
        actionIds,
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
  const merged = [...rows.values()].map((r) => r.row);
  const mineRows = merged.filter((r) => r.role === "mine");
  const reviewingRows = merged.filter(inReviewing);
  const allRows = merged.filter(inAll);
  return {
    mine: groupsFor("mine", mineRows),
    reviewing: groupsFor("reviewing", reviewingRows),
    all: groupsForAll(allRows),
    counts: { mine: mineRows.length, reviewing: reviewingRows.length, all: allRows.length },
    freshness,
    needsUpdate: stale,
    banners,
    hasData: usable.some((p) => p.payload !== null && p.payload !== undefined),
  };
}

function toRow(id: string, pr: Pr, primary: PrOrigin, origins: PrOrigin[]): PrRow {
  const info = BUCKETS[pr.role].find((b) => b.bucket === pr.bucket);
  const bucket: PrRow["bucket"] = info ? pr.bucket : "other";
  const branch =
    pr.sourceBranch || pr.targetBranch ? `${pr.sourceBranch ?? "?"} → ${pr.targetBranch ?? "?"}` : null;
  const required = pr.me?.required === true;
  const teams = uniqueFolded((pr.reviewers ?? []).filter((r) => r.isGroup).map((r) => oneLine(r.name)).filter(Boolean));
  // The bucket chip leads (an unknown bucket shows its raw name); a fact
  // chip with the same text, such as a draft's state, is not repeated. A
  // PR reached only through several teams is listed under the first; the
  // others show as chips.
  const lead: Chip[] = info ? [{ ...info.chip, title: info.label }] : oneLine(pr.bucket) ? [{ text: oneLine(pr.bucket), tone: "muted" }] : [];
  if (bucket === "group" && !required) for (const t of teams.slice(1)) lead.push({ text: `also via ${t}`, tone: "muted" });
  const seen = new Set(lead.map((c) => c.text.toLowerCase()));
  const fresh = (c: Chip): boolean => !seen.has(c.text.toLowerCase());
  const chips = [...lead, ...chipsFor(pr).filter(fresh)];
  const parts = chipParts(pr);
  const status = [...lead, ...[...parts.facts, ...parts.conflicts, ...parts.badges].filter(fresh)];
  const t = pr.threads;
  const comments = t ? { text: `${t.active} active / ${t.resolved} resolved`, forMe: t.needsMe } : null;
  const reasons = pr.reasons.map(oneLine).filter(Boolean);
  return {
    id,
    forge: pr.forge,
    repo: pr.repo,
    number: pr.number,
    url: pr.url,
    title: oneLine(pr.title) || `${pr.repo}${pr.number}`,
    author: pr.author.name,
    branch,
    sourceBranch: pr.sourceBranch ?? null,
    state: pr.state,
    role: pr.role,
    bucket,
    reasons,
    chips,
    required,
    teams,
    attention: attentionOf({ role: pr.role, bucket, required, state: pr.state }),
    project: pr.project ? oneLine(pr.project) || null : null,
    status,
    votes: votesOf(pr),
    comments,
    why: reasons.filter((r) => !restatesChips(r, chips)),
    ageUtc: pr.lastTouchedUtc ?? pr.created ?? pr.closedUtc ?? null,
    reviewers: (pr.reviewers ?? []).map((r) => ({ name: oneLine(r.name), vote: r.vote ?? "none", required: r.required === true, isGroup: r.isGroup === true })),
    sections: pr.sections ?? [],
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
    if (inBucket.length) groups.push({ id: b.bucket, bucket: b.bucket, label: b.label, folded: FOLDED.has(b.bucket), rows: inBucket });
  }
  const other = rows.filter((r) => r.bucket === "other");
  if (other.length) groups.push({ id: "other", bucket: "other", label: "Other", folded: false, rows: other });
  return groups;
}

/** Case-folded duplicates dropped, the first spelling kept. */
function uniqueFolded(names: string[]): string[] {
  const seen = new Set<string>();
  return names.filter((n) => {
    const k = n.toLowerCase();
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}

/** Where a review sits in the All tab: required by name, optional (named
 *  but not required, not team-only), or under the first team that brought
 *  it to me ("Through a team" when the source named none). */
function allGroupOf(row: PrRow): { id: string; label: string; team: boolean } {
  if (row.required) return { id: "required", label: "Required by name", team: false };
  if (row.bucket !== "group") return { id: "optional", label: "Optional", team: false };
  const team = row.teams[0];
  return team ? { id: `team:${team.toLowerCase()}`, label: team, team: true } : { id: "team", label: "Through a team", team: true };
}

/** The All tab's groups: Required by name, Optional, then one per team in
 *  name order, "Through a team" last. Team groups start folded: they are
 *  the long tail. Rows within a group keep bucket order, so the ones that
 *  need me come first. */
function groupsForAll(rows: PrRow[]): PrGroup[] {
  const order: string[] = BUCKETS.reviewing.map((b) => b.bucket);
  const rank = (r: PrRow): number => {
    const i = order.indexOf(r.bucket);
    return i < 0 ? order.length : i;
  };
  const byId = new Map<string, PrGroup>();
  for (const row of [...rows].sort((a, b) => rank(a) - rank(b))) {
    const g = allGroupOf(row);
    const at = byId.get(g.id);
    if (at) at.rows.push(row);
    else byId.set(g.id, { id: g.id, label: g.label, folded: g.team, rows: [row] });
  }
  const place = (g: PrGroup): string =>
    g.id === "required" ? "0" : g.id === "optional" ? "1" : g.id === "team" ? "3" : `2${g.label.toLowerCase()}`;
  return [...byId.values()].sort((a, b) => (place(a) < place(b) ? -1 : place(a) > place(b) ? 1 : 0));
}

/** The rows of a model that need the user ({@link attentionOf} "yes":
 *  Needs you, its badge, the PRs rail count, notifications), and the folded
 *  ones (shown, not counted). Every review that needs the user is in the
 *  Reviewing tab, so the two bucket-grouped tabs hold them all, in display
 *  order. */
export function prsNeedingYou(model: PrsModel): { yes: PrRow[]; folded: PrRow[] } {
  const yes: PrRow[] = [];
  const folded: PrRow[] = [];
  for (const g of [...model.mine, ...model.reviewing]) {
    for (const r of g.rows) {
      if (r.attention === "yes") yes.push(r);
      else if (r.attention === "folded") folded.push(r);
    }
  }
  return { yes, folded };
}
