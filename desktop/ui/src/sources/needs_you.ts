// The Needs you view's model, and the rules for when a source's news becomes
// a notification. Pure (no DOM, no ipc).
//
// Needs you is built by the app, not served by any one source. Its rows:
//  - terminals and agents that want the user: the attention the rail and the
//    dashboard already derive (an agent waiting or a program reporting
//    `blocked` outranks a bell; `done` / `failed` sit at bell rank until the
//    row is viewed), handed in as the rail computed it, never re-derived;
//  - Work items with an approval pending, or whose current status may only be
//    left by a human;
//  - PRs that need the user (`attentionOf` in the PRs model: my PRs in
//    action / draft / ready, reviews the author answered, and required
//    reviews not yet voted; my PRs waiting on reviewers are shown folded,
//    not counted; optional and team-only reviews never appear);
//  - Environment components in a red state (unhealthy, crash loop, deployed
//    but not running, a failed one-shot): something the user brought up is
//    broken;
//  - the attention items sources serve, grouped under their own headings;
//  - data safety, first: each stack whose data-safety block keeps agents out
//    (it opens Environment, where the block is dealt with), and each running
//    agent working inside a blocked folder (its deliveries are held).
// The rail badge counts the rows that wait on a decision: agents waiting
// and programs blocked, Work approvals, PRs, red components and source
// attention. A
// terminal at bell rank (a bell, or a program that reported done / failed)
// is listed but not counted: a stray `\a` should not keep a number on the
// rail.

import { attentionText, type CardAttention } from "../dashboard";
import type * as ipc from "../ipc";
import { dataSafetySection } from "./data_safety";
import { buildEnvironmentModel, environmentNews } from "./environment_model";
import {
  itemsOf,
  oneLine,
  prsNeedingYou,
  readFreshness,
  summarizeFreshness,
  wrongContract,
  type FreshnessSummary,
  type NeedsUpdate,
  type PrRow,
  type PrsModel,
  type Tone,
  type ViewPayload,
} from "./prs_model";

/** A terminal or agent row the rail says wants the user. */
export interface TerminalAttention {
  /** Stable while the row lives (`cmd:<project>:<name>` / `term:<id>`). */
  key: string;
  name: string;
  /** The owning project's name, or null for the workspace. */
  project: string | null;
  attention: Exclude<CardAttention, null>;
  programStatus: ipc.ProgramStatus | null;
}

/** One source's actions for a row: `source_actions(sourceId, view, key)`. */
export interface ActionTarget {
  sourceId: string;
  sourceLabel: string;
  view: string;
  /** The item's key; null for the view's (or page's) own actions. */
  key: string | null;
  /** Set when the actions are `sourceId`'s item actions on the row `key`
   *  of this OTHER source (a "Start work" on a tracker's row). */
  owner?: string;
}

/** Where an attention item's title leads. */
export type Opens = { view: "work" | "prs" | "environment"; key: string } | { url: string } | null;

export interface NeedsYouRow {
  /** Stable across polls: `term:…`, `work:<source>|<key>`, `pr:<id>`,
   *  `env:<source>|<component id>`, `att:<source>|<id>`. */
  id: string;
  kind: "terminal" | "work" | "pr" | "environment" | "attention";
  /** Leads with the item or process name; markdown links stripped. */
  title: string;
  detail: string;
  tone: Tone;
  /** A folded row: listed under its section's fold, not counted. */
  folded: boolean;
  /** Counted in the rail badge: false for folded rows and for terminals at
   *  bell rank, which are listed but do not wait on a decision. */
  counted: boolean;
  /** The source the row came from (null for terminals); a link it opens is
   *  checked against this source's trusted hosts. */
  sourceId: string | null;
  sourceLabel: string | null;
  /** Where its actions are read from: each run against its own source;
   *  `only` draws just those ids of the target's list. */
  actions: (ActionTarget & { only?: readonly string[] })[];
  /** Terminal rows: the rail key a click selects. */
  terminalKey?: string;
  opens: Opens;
  /** An app-side action (not a source's), drawn as one button. */
  local?: { label: string; run: () => void };
}

export interface NeedsYouSection {
  id: string;
  title: string;
  rows: NeedsYouRow[];
}

export interface NeedsYouModel {
  sections: NeedsYouSection[];
  /** The counted rows: the rail badge. */
  count: number;
  freshness: FreshnessSummary;
  needsUpdate: NeedsUpdate[];
}

export interface NeedsYouInput {
  terminals: TerminalAttention[];
  /** The `work` payloads of the trusted sources serving it. */
  work: ViewPayload[];
  /** The merged PRs model, or null when the PRs view is not in play. */
  prs: PrsModel | null;
  /** The `attention` payloads of the trusted sources serving it. */
  attention: ViewPayload[];
  /** The `environment` payloads of the trusted sources serving it (absent
   *  or empty when the view is not in play). */
  environment?: ViewPayload[];
  needsUpdate?: NeedsUpdate[];
  /** What Rust enforces for data safety (absent: nothing is blocked). */
  dataSafety?: ipc.DataSafetyStateDto;
  /** Agent settings files git would commit, with the app's own action. */
  agentSettings?: AgentSettingsWarning[];
}

/** One agent settings file warning as Needs you lists it. */
export interface AgentSettingsWarning {
  projectRoot: string;
  file: ipc.AgentSettingsFileDto;
  /** "Add to .gitignore" (absent for a tracked file, which needs a person). */
  addToGitignore?: () => void;
}

/** The standing warnings as the app keeps them, by file path, each with
 *  "Add to .gitignore" when the file is not tracked and inside its project
 *  (`add` runs the confirm-gated action). Replaces whatever was listed:
 *  the app's list is the one truth, so a resolved warning goes away. */
export function standingAgentSettings(
  all: ipc.AgentSettingsWarningDto[],
  add: (projectRoot: string, path: string, line: string) => void,
): Map<string, AgentSettingsWarning> {
  const out = new Map<string, AgentSettingsWarning>();
  for (const w of all) {
    const warning: AgentSettingsWarning = { projectRoot: w.projectRoot, file: w.file };
    const line = w.file.ignoreLine;
    if (w.file.exposure === "not-ignored" && line) {
      warning.addToGitignore = () => add(w.projectRoot, w.file.path, line);
    }
    out.set(w.file.path, warning);
  }
  return out;
}

/** The section for agent settings files git would commit. A tracked file
 *  is `bad` and has no action: it must come out of the index and its
 *  secrets be rotated, which is not automated. */
export function agentSettingsSection(warnings: AgentSettingsWarning[]): NeedsYouSection | null {
  if (!warnings.length) return null;
  return {
    id: "agent-settings",
    title: "Agent settings files",
    rows: warnings.map((w) => {
      const tracked = w.file.exposure === "tracked";
      return {
        id: `agent-settings:${w.file.path}`,
        kind: "attention",
        title: w.file.path,
        detail: w.file.message,
        tone: tracked ? "bad" : "warn",
        folded: false,
        counted: true,
        sourceId: null,
        sourceLabel: null,
        actions: [],
        opens: null,
        ...(!tracked && w.addToGitignore ? { local: { label: "Add to .gitignore", run: w.addToGitignore } } : {}),
      };
    }),
  };
}

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;

const SEVERITY_TONE: Record<string, Tone> = { info: "info", warn: "warn", bad: "bad" };

/** A Work item's reason to need the user, or null when it has none. */
export function workAttention(raw: unknown): { key: string; title: string; detail: string; url: string | null } | null {
  const item = obj(raw);
  const key = str(item?.key);
  const ls = obj(item?.localStatus);
  if (!item || !key || !ls) return null;
  const name = oneLine(str(item.title) ?? "");
  const title = name ? `${key} ${name}` : key;
  const url = str(item.url);
  const pending = obj(ls.pendingApproval);
  if (pending && str(pending.to)) {
    const by = str(pending.by);
    const note = str(pending.note);
    const detail = `approve the move to ${pending.to as string}${by ? `, asked by ${by}` : ""}${note ? `: ${oneLine(note)}` : ""}`;
    return { key, title, detail, url };
  }
  if (ls.gate === "human-exit") {
    const label = str(ls.label) ?? str(ls.id) ?? "this status";
    return { key, title, detail: `${oneLine(label)}: only a person can move it out`, url };
  }
  return null;
}

/** The detail line of a PR row: where it is and the first reason. */
function prDetail(row: PrRow): string {
  const where = `${row.repo}${row.number}`;
  return row.reasons.length ? `${where} · ${row.reasons[0]}` : where;
}

function readOpens(v: unknown): Opens {
  const o = obj(v);
  if (!o) return null;
  const url = str(o.url);
  if (url) return { url };
  const view = str(o.view);
  const key = str(o.key);
  if (key && (view === "work" || view === "prs" || view === "environment")) return { view, key };
  return null;
}

/** Build the Needs you sections and the badge count. */
export function buildNeedsYou(input: NeedsYouInput): NeedsYouModel {
  const sections: NeedsYouSection[] = [];
  const stale: NeedsUpdate[] = [];
  const noteStale = (s: NeedsUpdate): void => {
    if (!stale.some((x) => x.sourceId === s.sourceId)) stale.push({ sourceId: s.sourceId, sourceLabel: s.sourceLabel });
  };
  for (const s of [...(input.needsUpdate ?? []), ...(input.prs?.needsUpdate ?? [])]) noteStale(s);
  const addStale = (p: ViewPayload): boolean => {
    if (!wrongContract(p.payload)) return false;
    noteStale(p);
    return true;
  };

  // Data safety leads: agents are being kept from data that may be real.
  const safety = input.dataSafety ? dataSafetySection(input.dataSafety) : null;
  if (safety) sections.push(safety);
  const settingsFiles = agentSettingsSection(input.agentSettings ?? []);
  if (settingsFiles) sections.push(settingsFiles);

  // Terminals and agents: waiting / blocked first, then bell rank, each in
  // the rail's own order.
  const terms = [
    ...input.terminals.filter((t) => t.attention === "agent-waiting"),
    ...input.terminals.filter((t) => t.attention === "badge"),
  ];
  if (terms.length) {
    sections.push({
      id: "terminals",
      title: "Terminals and agents",
      rows: terms.map((t) => ({
        id: `term:${t.key}`,
        kind: "terminal",
        title: t.name,
        detail: [attentionText({ attention: t.attention, programStatus: t.programStatus }), t.project].filter(Boolean).join(" · "),
        tone: t.attention === "agent-waiting" ? "info" : "warn",
        folded: false,
        counted: t.attention === "agent-waiting",
        sourceId: null,
        sourceLabel: null,
        actions: [],
        terminalKey: t.key,
        opens: null,
      })),
    });
  }

  const workRows: NeedsYouRow[] = [];
  const workUsable = input.work.filter((p) => !addStale(p));
  for (const p of workUsable) {
    for (const raw of itemsOf(p.payload)) {
      const w = workAttention(raw);
      if (!w) continue;
      workRows.push({
        id: `work:${p.sourceId}|${w.key}`,
        kind: "work",
        title: w.title,
        detail: w.detail,
        tone: "warn",
        folded: false,
        counted: true,
        sourceId: p.sourceId,
        sourceLabel: p.sourceLabel,
        actions: Array.isArray(obj(raw)?.actions) && (obj(raw)!.actions as unknown[]).length > 0
          ? [{ sourceId: p.sourceId, sourceLabel: p.sourceLabel, view: "work", key: w.key }]
          : [],
        opens: w.url ? { url: w.url } : null,
      });
    }
  }
  if (workRows.length) sections.push({ id: "work", title: "Work", rows: workRows });

  if (input.prs) {
    const { yes, folded } = prsNeedingYou(input.prs);
    const prRow = (r: PrRow, isFolded: boolean): NeedsYouRow => ({
      id: `pr:${r.id}`,
      kind: "pr",
      title: r.title,
      detail: prDetail(r),
      tone: r.bucket === "action" ? "bad" : isFolded ? "muted" : "warn",
      folded: isFolded,
      counted: !isFolded,
      sourceId: r.primary.sourceId,
      sourceLabel: r.primary.sourceLabel,
      actions: r.origins
        .filter((o) => o.hasActions)
        .map((o) => ({ sourceId: o.sourceId, sourceLabel: o.sourceLabel, view: "prs", key: o.key })),
      // The PRs view shows it with its reasons and chips; that view is in
      // the rail whenever a PR row is here (a hidden view feeds no rows).
      opens: { view: "prs", key: r.primary.key },
    });
    const rows = [...yes.map((r) => prRow(r, false)), ...folded.map((r) => prRow(r, true))];
    if (rows.length) sections.push({ id: "prs", title: "Pull requests", rows });
  }

  // Red Environment components: each opens the Environment view on its row.
  const envUsable = (input.environment ?? []).filter((p) => !addStale(p));
  const envRows: NeedsYouRow[] = buildEnvironmentModel(envUsable).red.map((r) => ({
    id: `env:${r.key}`,
    kind: "environment",
    title: r.label,
    detail: [r.stateText, r.hint || r.details.join(", "), envUsable.length > 1 ? r.sourceLabel : ""].filter(Boolean).join(" · "),
    tone: "bad",
    folded: false,
    counted: true,
    sourceId: r.sourceId,
    sourceLabel: r.sourceLabel,
    actions: r.hasActions ? [{ sourceId: r.sourceId, sourceLabel: r.sourceLabel, view: "environment", key: r.id }] : [],
    opens: { view: "environment", key: r.id },
  }));
  if (envRows.length) sections.push({ id: "environment", title: "Environment", rows: envRows });

  // Source attention, under each item's own group, groups in first-seen order.
  const attentionUsable = input.attention.filter((p) => !addStale(p));
  const groups = new Map<string, NeedsYouRow[]>();
  for (const p of attentionUsable) {
    for (const raw of itemsOf(p.payload)) {
      const a = obj(raw);
      const id = str(a?.id);
      const title = oneLine(str(a?.title) ?? "");
      if (!a || !id || !title) continue;
      const group = oneLine(str(a.group) ?? "") || "Other";
      const row: NeedsYouRow = {
        id: `att:${p.sourceId}|${id}`,
        kind: "attention",
        title,
        detail: oneLine(str(a.detail) ?? ""),
        tone: SEVERITY_TONE[str(a.severity) ?? ""] ?? "info",
        folded: false,
        counted: true,
        sourceId: p.sourceId,
        sourceLabel: p.sourceLabel,
        actions:
          Array.isArray(a.actions) && a.actions.length > 0
            ? [{ sourceId: p.sourceId, sourceLabel: p.sourceLabel, view: "attention", key: id }]
            : [],
        opens: readOpens(a.opens),
      };
      const at = groups.get(group);
      if (at) at.push(row);
      else groups.set(group, [row]);
    }
  }
  for (const [group, rows] of groups) sections.push({ id: `group:${group}`, title: group, rows });

  const fed = [...workUsable, ...envUsable, ...attentionUsable];
  const freshness = summarizeFreshness(fed);
  if (input.prs) {
    freshness.errors.push(...input.prs.freshness.errors);
    freshness.refreshing ||= input.prs.freshness.refreshing;
  }
  const count = sections.reduce((n, s) => n + s.rows.filter((r) => r.counted).length, 0);
  return { sections, count, freshness, needsUpdate: stale };
}

// ---- notification rules ---------------------------------------------------

/** One item a source's poll reports as needing the user. */
export interface NewsItem {
  /** Stable key: a PR's forge/repo/number, an attention item's `id`, a Work
   *  item's `key`. */
  id: string;
  /** What the notification leads with: the item's name. */
  name: string;
  body: string;
}

/** The outcome of one poll of one (source, view). */
export type PollResult =
  | { ok: true; items: NewsItem[]; partial?: boolean }
  | { ok: false };

export interface NewsNotification {
  title: string;
  body: string;
  /** The items it announces (one, or the whole batch of a summary). */
  ids: string[];
}

/** New items in one update at or above this many collapse into one summary. */
export const SUMMARY_AT = 5;

/**
 * Seen-id baselines per (source, view), and what a poll result turns into:
 * - the first successful poll sets the baseline silently;
 * - a failed poll keeps the previous ids (and says nothing);
 * - a partial poll (the source reported errors beside what it served) adds
 *   its ids but forgets none, so an item a failing search dropped does not
 *   announce itself again when the search recovers;
 * - ids not in the baseline are news: one notification each, or one summary
 *   when there are {@link SUMMARY_AT} or more;
 * - a notification leads with the item's name, never the source's label.
 */
export class SeenBaselines {
  private readonly seen = new Map<string, Set<string>>();

  /** Apply one poll result; answers the notifications it warrants. An item
   *  `known` says the user already heard about (another source serving the
   *  same PR) joins the baseline without a notification. */
  update(sourceId: string, view: string, result: PollResult, known: (id: string) => boolean = () => false): NewsNotification[] {
    if (!result.ok) return [];
    const k = `${sourceId}\u001f${view}`;
    const before = this.seen.get(k);
    const ids = new Set(result.items.map((i) => i.id));
    if (!before) {
      this.seen.set(k, ids);
      return [];
    }
    const fresh = result.items.filter(
      (i, at) => !before.has(i.id) && !known(i.id) && result.items.findIndex((j) => j.id === i.id) === at,
    );
    this.seen.set(k, result.partial ? new Set([...before, ...ids]) : ids);
    return newsFor(fresh);
  }

  /** Forget a source (it went away); its next poll is a first one again. */
  forget(sourceId: string): void {
    for (const k of [...this.seen.keys()]) if (k.startsWith(`${sourceId}\u001f`)) this.seen.delete(k);
  }

  /** Whether a source other than `sourceId` already has `id` in its
   *  baseline of `view`. */
  seenElsewhere(view: string, id: string, sourceId: string): boolean {
    for (const [k, ids] of this.seen) {
      const [s, v] = k.split("\u001f");
      if (v === view && s !== sourceId && ids.has(id)) return true;
    }
    return false;
  }

  /** Whether (source, view) has a baseline yet. */
  has(sourceId: string, view: string): boolean {
    return this.seen.has(`${sourceId}\u001f${view}`);
  }
}

/** The notifications for a batch of new items. */
export function newsFor(items: NewsItem[]): NewsNotification[] {
  if (items.length === 0) return [];
  if (items.length >= SUMMARY_AT) {
    const names = items.map((i) => i.name);
    return [
      {
        title: `${names[0]} and ${items.length - 1} more need you`,
        body: names.slice(1, 4).join(", ") + (items.length > 4 ? ", …" : ""),
        ids: items.map((i) => i.id),
      },
    ];
  }
  return items.map((i) => ({ title: i.name, body: i.body, ids: [i.id] }));
}

/** What one source's poll of `view` reports as needing the user, ready for
 *  {@link SeenBaselines.update}. `prs` must be the model of THIS source's
 *  payload alone (ids are per source). A poll the app saw fail (its own
 *  freshness carries an error) or with no payload yet is a failure. */
export function pollResult(view: "work" | "prs" | "attention" | "environment", p: ViewPayload, prs?: PrsModel): PollResult {
  if (p.payload === null || p.payload === undefined || (p.freshness?.errors?.length ?? 0) > 0 || wrongContract(p.payload)) {
    return { ok: false };
  }
  const partial = (readFreshness(obj(p.payload)?.freshness).errors?.length ?? 0) > 0;
  const items: NewsItem[] = [];
  if (view === "work") {
    for (const raw of itemsOf(p.payload)) {
      const w = workAttention(raw);
      if (w) items.push({ id: w.key, name: w.title, body: w.detail });
    }
  } else if (view === "prs") {
    for (const r of prs ? prsNeedingYou(prs).yes : []) items.push({ id: r.id, name: r.title, body: prDetail(r) });
  } else if (view === "environment") {
    // Red components and bad alerts; a recovery or a warning is never news.
    items.push(...environmentNews(p));
  } else {
    for (const raw of itemsOf(p.payload)) {
      const a = obj(raw);
      const id = str(a?.id);
      const title = oneLine(str(a?.title) ?? "");
      if (id && title) items.push({ id, name: title, body: oneLine(str(a?.detail) ?? "") });
    }
  }
  return { ok: true, items, ...(partial ? { partial } : {}) };
}
