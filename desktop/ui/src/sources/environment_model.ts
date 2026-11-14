// The Environment view's model: one block per trusted source serving the
// view (each source is its own stack, so nothing merges across sources),
// its components grouped, every state mapped to one tone, and the level of
// the rail's alert dot. Pure (no DOM, no ipc): the page, the rail, Needs you
// and the notification rules all read it, and the tests feed it the fixture.
//
// What decides a component's colour is its state alone, never its exit code:
// a source tracks whether the component is WANTED running (desired state)
// and reports `stopped-problem` or `stopped-idle` from that, so a 137 and a 0
// look the same here and the code is only shown in the row's details.
//
// A payload arrives exactly as the source sent it, so nothing here trusts its
// shape: a component without an id is skipped and counted, an optional field
// of the wrong type reads as absent, an unknown state reads as a muted
// "unknown", and unknown fields are ignored.

import { readBanners, readSections } from "./sections";
import type { Banner, ComponentState, Section } from "./types";
import {
  oneLine,
  readBadges,
  summarizeFreshness,
  wrongContract,
  type Chip,
  type FreshnessSummary,
  type NeedsUpdate,
  type Tone,
  type ViewPayload,
} from "./prs_model";

/** What the page shows for each state, in the contract's table order. */
export interface StateInfo {
  tone: Tone;
  /** The state as the row's text says it. */
  text: string;
  /** The phrase of the header count ("2 deployed but not running"). */
  count: (n: number) => string;
  /** Counts toward the alert dot, Needs you and notifications. */
  red: boolean;
  /** What the state means, for the tooltip. */
  title: string;
}

const n = (k: number): string => String(k);

export const STATES: Record<ComponentState, StateInfo> = {
  up: { tone: "ok", text: "up", count: (k) => `${n(k)} up`, red: false, title: "Running; its probe passes, or it has none" },
  starting: { tone: "warn", text: "starting", count: (k) => `${n(k)} starting`, red: false, title: "Running; its health check has not passed yet" },
  unhealthy: { tone: "bad", text: "unhealthy", count: (k) => `${n(k)} unhealthy`, red: true, title: "Running, but its probe or health check keeps failing" },
  crashloop: { tone: "bad", text: "crash loop", count: (k) => `${n(k)} in a crash loop`, red: true, title: "Restarting now, or restarted three or more times" },
  "stopped-problem": {
    tone: "bad",
    text: "deployed but not running",
    count: (k) => `${n(k)} deployed but not running`,
    red: true,
    title: "Not running, though it is wanted running: it was brought up and no stop was asked for since",
  },
  "stopped-idle": {
    tone: "muted",
    text: "stopped",
    count: (k) => `${n(k)} stopped`,
    red: false,
    title: "Not running and not wanted running: stopped on request, or started on demand and stopped again",
  },
  "oneshot-ok": { tone: "muted", text: "ran ok", count: (k) => `${n(k)} ran ok`, red: false, title: "A one-shot job that ran to exit 0" },
  "oneshot-failed": { tone: "bad", text: "failed", count: (k) => `${n(k)} failed`, red: true, title: "A one-shot job that exited non-zero" },
  "not-deployed": {
    tone: "muted",
    text: "not deployed",
    count: (k) => `${n(k)} not deployed`,
    red: false,
    title: "Never started, or not part of what this stack brings up",
  },
};

/** The nine states in table order. */
export const STATE_ORDER = Object.keys(STATES) as ComponentState[];

/** What a state the app does not know reads as. */
export const UNKNOWN_STATE: Omit<StateInfo, "count"> = {
  tone: "muted",
  text: "unknown",
  red: false,
  title: "The source reported a state this app does not know",
};

const isState = (s: string): s is ComponentState => Object.prototype.hasOwnProperty.call(STATES, s);

/** A state's info; an unknown state reads as muted "unknown". */
export function stateInfo(state: string): Omit<StateInfo, "count"> {
  return isState(state) ? STATES[state] : UNKNOWN_STATE;
}

/** What each source mode means, for the source cell's tooltip. */
export const MODE_TITLES: Record<string, string> = {
  prebuilt: "prebuilt: runs a saved image, as built elsewhere",
  source: "source: runs an image built here from the repository at this ref",
  dev: "dev: runs as a process from your working copy, so your edits show",
};

/** What each probe rule passes on. */
export const PROBE_RULES: Record<string, string> = {
  health: "passes on 2xx only",
  web: "passes on 2xx or 3xx",
  any: "passes on any HTTP answer",
};

export interface SourceCell {
  mode: string;
  /** "dev · abc-1-health @ 1a2b3c4". */
  text: string;
  title: string;
}

export interface ProbeCell {
  up: boolean;
  rule: string;
  url: string;
  status: number | null;
  /** "health up 200", or "health down: no response". */
  text: string;
  /** The rule, the url, and why it is down. */
  title: string;
}

export interface EnvLink {
  label: string;
  url: string;
  /** The one to open in a browser (the component's UI). */
  browser: boolean;
}

export interface EnvAlert {
  text: string;
  severity: "warn" | "bad";
  componentId: string | null;
}

export interface EnvRow {
  /** The component's id (unique within its source). */
  id: string;
  /** Stable across polls and sources: `<sourceId>|<id>`. */
  key: string;
  sourceId: string;
  sourceLabel: string;
  label: string;
  group: string;
  oneshot: boolean;
  /** The raw state as sent. */
  state: string;
  stateText: string;
  stateTitle: string;
  tone: Tone;
  red: boolean;
  notDeployed: boolean;
  /** Uptime, restarts, exit code: shown, never deciding the colour. */
  details: string[];
  source: SourceCell | null;
  probe: ProbeCell | null;
  links: EnvLink[];
  hint: string;
  badges: Chip[];
  /** Alerts that name this component. */
  alerts: EnvAlert[];
  /** Folded in the row; their actions are the component's. */
  sections: Section[];
  /** The ids of the component's own actions (not its sections'). */
  actionIds: string[];
  /** Any action at all, its sections' included. */
  hasActions: boolean;
}

export interface EnvGroup {
  id: string;
  label: string;
  rows: EnvRow[];
}

/** One source's stack. */
export interface EnvBlock {
  sourceId: string;
  sourceLabel: string;
  /** The source has served a payload. */
  hasData: boolean;
  /** The non-zero counts, table order ("1 up", "2 stopped"). */
  header: string[];
  groups: EnvGroup[];
  /** How many components are `not-deployed` (hidden behind the toggle). */
  notDeployed: number;
  alerts: EnvAlert[];
  /** Drawn by the shared section renderer; their actions are the view's. */
  facts: Section[];
  /** The view-level banners; their actions are the view's own (no key). */
  banners: Banner[];
}

export type AlertLevel = "bad" | "warn" | null;

export interface EnvironmentModel {
  blocks: EnvBlock[];
  freshness: FreshnessSummary;
  needsUpdate: NeedsUpdate[];
  hasData: boolean;
  /** The rail's dot: the worst of the alerts and the red components. */
  alertLevel: AlertLevel;
  /** Every component in a red state, in block order. */
  red: EnvRow[];
}

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);
const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const text = (v: unknown): string => oneLine(str(v) ?? "");

/** The short form of a commit: the first seven characters. */
export const shortCommit = (sha: string): string => sha.slice(0, 7);

export function readSourceCell(v: unknown): SourceCell | null {
  const s = obj(v);
  const mode = text(s?.mode);
  if (!s || !mode) return null;
  const ref = text(s.ref);
  const commit = text(s.commit);
  const where = [ref, commit ? `@ ${shortCommit(commit)}` : ""].filter(Boolean).join(" ");
  const title = [MODE_TITLES[mode] ?? mode, ref ? `ref ${ref}` : "", commit ? `commit ${commit}` : ""].filter(Boolean).join("\n");
  return { mode, text: where ? `${mode} · ${where}` : mode, title };
}

export function readProbe(v: unknown): ProbeCell | null {
  const p = obj(v);
  const url = text(p?.url);
  if (!p || !url || typeof p.up !== "boolean") return null;
  // `health` is the default and the strict one.
  const rule = text(p.rule) || "health";
  const status = num(p.status);
  const up = p.up;
  let why = "";
  if (!up) {
    if (status === null) why = "no response";
    else if (rule === "any") why = `answered ${status}, but reported down`;
    else why = `${status} fails the ${rule} rule`;
  }
  const shown = up ? `${rule} up${status !== null ? ` ${status}` : ""}` : `${rule} down: ${why}`;
  const ruleText = PROBE_RULES[rule] ? `${rule} ${PROBE_RULES[rule]}` : rule;
  return { up, rule, url, status, text: shown, title: [url, ruleText, why].filter(Boolean).join("\n") };
}

function readLinks(v: unknown): EnvLink[] {
  const links = list(v)
    .map(obj)
    .filter((l): l is Record<string, unknown> => l !== null && !!text(l.url))
    .map((l) => ({ label: text(l.label) || text(l.url), url: text(l.url), browser: l.browser === true }));
  // The browser link leads.
  return [...links.filter((l) => l.browser), ...links.filter((l) => !l.browser)];
}

function readAlerts(v: unknown): EnvAlert[] {
  return list(v)
    .map(obj)
    .filter((a): a is Record<string, unknown> => a !== null && !!text(a.text))
    .map((a) => ({
      text: text(a.text),
      // An unreadable severity is the milder one.
      severity: a.severity === "bad" ? ("bad" as const) : ("warn" as const),
      componentId: text(a.componentId) || null,
    }));
}

const chips = (v: unknown): Chip[] => readBadges(v).map((b) => ({ text: b.text, tone: (b.tone ?? "info") as Tone, ...(b.title ? { title: b.title } : {}) }));

const plural = (k: number, one: string, many = `${one}s`): string => `${k} ${k === 1 ? one : many}`;

function details(c: Record<string, unknown>): string[] {
  const out: string[] = [];
  const uptime = text(c.uptime);
  if (uptime) out.push(`up ${uptime}`);
  const restarts = num(c.restarts);
  if (restarts !== null && restarts > 0) out.push(plural(restarts, "restart"));
  const exit = num(c.exitCode);
  if (exit !== null) out.push(`exit ${exit}`);
  return out;
}

/** One component as a row, or null when it has no id. */
export function readComponent(raw: unknown, sourceId: string, sourceLabel: string, alerts: EnvAlert[] = []): EnvRow | null {
  const c = obj(raw);
  const id = text(c?.id);
  if (!c || !id) return null;
  const state = text(c.state) || "unknown";
  const info = stateInfo(state);
  const kindOneshot = c.kind === "oneshot" || state === "oneshot-ok" || state === "oneshot-failed";
  const sections = readSections(c.sections);
  const actionIds = list(c.actions).map((a) => str(obj(a)?.id)).filter((a): a is string => !!a);
  return {
    id,
    key: `${sourceId}|${id}`,
    sourceId,
    sourceLabel,
    label: text(c.label) || id,
    group: text(c.group),
    oneshot: kindOneshot,
    state,
    stateText: info.text,
    stateTitle: isState(state) ? info.title : `${info.title}: "${state}"`,
    tone: info.tone,
    red: info.red,
    notDeployed: state === "not-deployed",
    details: details(c),
    source: readSourceCell(c.source),
    probe: readProbe(c.probe),
    links: readLinks(c.links),
    hint: text(c.hint),
    badges: chips(c.badges),
    alerts: alerts.filter((a) => a.componentId === id),
    sections,
    actionIds,
    hasActions: actionIds.length > 0 || sections.some((s) => (s.actions ?? []).length > 0),
  };
}

/** The header counts: the source's `summary` when it sent one, else counted
 *  from the components. Non-zero only, table order. */
export function headerCounts(summary: unknown, rows: EnvRow[]): string[] {
  const s = obj(summary);
  const counts = new Map<ComponentState, number>();
  if (s) {
    for (const state of STATE_ORDER) {
      const k = num(s[state]);
      if (k !== null && k > 0) counts.set(state, Math.floor(k));
    }
  } else {
    for (const r of rows) if (isState(r.state)) counts.set(r.state, (counts.get(r.state) ?? 0) + 1);
  }
  return STATE_ORDER.filter((st) => (counts.get(st) ?? 0) > 0).map((st) => STATES[st].count(counts.get(st)!));
}

function block(p: ViewPayload): { block: EnvBlock; skipped: number } {
  const body = obj(p.payload);
  if (!body) {
    return {
      block: { sourceId: p.sourceId, sourceLabel: p.sourceLabel, hasData: false, header: [], groups: [], notDeployed: 0, alerts: [], facts: [], banners: [] },
      skipped: 0,
    };
  }
  const alerts = readAlerts(body.alerts);
  const rows: EnvRow[] = [];
  let skipped = 0;
  const seen = new Set<string>();
  for (const raw of list(body.components)) {
    const r = readComponent(raw, p.sourceId, p.sourceLabel, alerts);
    if (!r || seen.has(r.id)) {
      skipped++;
      continue;
    }
    seen.add(r.id);
    rows.push(r);
  }
  // Groups in the source's order; a component naming no known group goes
  // under a trailing "Other".
  const groups: EnvGroup[] = [];
  const byId = new Map<string, EnvGroup>();
  for (const raw of list(body.groups)) {
    const g = obj(raw);
    const id = text(g?.id);
    if (!id || byId.has(id)) continue;
    const group = { id, label: text(g?.label) || id, rows: [] };
    byId.set(id, group);
    groups.push(group);
  }
  const other: EnvGroup = { id: "\u0000other", label: "Other", rows: [] };
  for (const r of rows) (byId.get(r.group) ?? other).rows.push(r);
  if (other.rows.length) groups.push(other);
  return {
    block: {
      sourceId: p.sourceId,
      sourceLabel: p.sourceLabel,
      hasData: true,
      header: headerCounts(body.summary, rows),
      groups: groups.filter((g) => g.rows.length > 0),
      notDeployed: rows.filter((r) => r.notDeployed).length,
      alerts,
      facts: readSections(body.facts),
      banners: readBanners(body.banners),
    },
    skipped,
  };
}

/** The worst of two dot levels. */
export function worse(a: AlertLevel, b: AlertLevel): AlertLevel {
  if (a === "bad" || b === "bad") return "bad";
  return a ?? b;
}

/** Build the page's model from every trusted source serving the view. */
export function buildEnvironmentModel(payloads: ViewPayload[], needsUpdate: NeedsUpdate[] = []): EnvironmentModel {
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
  const blocks: EnvBlock[] = [];
  let level: AlertLevel = null;
  const red: EnvRow[] = [];
  for (const p of usable) {
    const { block: b, skipped } = block(p);
    if (skipped > 0) {
      const line = `${plural(skipped, "component")} could not be read`;
      freshness.errors.push(usable.length > 1 ? `${p.sourceLabel}: ${line}` : line);
    }
    for (const a of b.alerts) level = worse(level, a.severity);
    for (const g of b.groups) {
      for (const r of g.rows) {
        if (!r.red) continue;
        red.push(r);
        level = "bad";
      }
    }
    blocks.push(b);
  }
  return { blocks, freshness, needsUpdate: stale, hasData: blocks.some((b) => b.hasData), alertLevel: level, red };
}

/** The ids the notification rules track for one source's payload: a
 *  component entering a red state (`component:<id>:<state>`, so a move from
 *  one red state to another is news too) and each `bad` alert
 *  (`alert:<componentId?>:<text>`). Warnings and recoveries are never news. */
export function environmentNews(p: ViewPayload): { id: string; name: string; body: string }[] {
  const model = buildEnvironmentModel([p]);
  const b = model.blocks[0];
  if (!b) return [];
  const labelOf = new Map(b.groups.flatMap((g) => g.rows.map((r) => [r.id, r.label] as const)));
  const items = model.red.map((r) => ({
    id: `component:${r.id}:${r.state}`,
    name: r.label,
    body: [r.stateText, r.hint || r.details.join(", ")].filter(Boolean).join(": "),
  }));
  for (const a of b.alerts) {
    if (a.severity !== "bad") continue;
    items.push({
      id: `alert:${a.componentId ?? ""}:${a.text}`,
      name: a.componentId ? (labelOf.get(a.componentId) ?? a.componentId) : a.text,
      body: a.componentId ? a.text : `${b.sourceLabel} environment`,
    });
  }
  return items;
}
