// The dashboard: every row of the open projects and the workspace as a grid
// of cards, grouped by project, in one page of the stack.
//
// A card is the record pane's card drawn small: the same `recordView` output
// (title, last run, summary, session line, start / resume actions) plus the
// live facts of a row whose terminal is attached (status, cpu / memory,
// attention, hidden output). It is a renderer over state the app already
// keeps — the rows, their records and the stats store — so it owns no data:
// the app hands it the inputs on every repaint, and the page diffs them
// per card and per group header, repainting only what changed in place (a
// status event touches one card, never the whole grid, and focus inside the
// page survives).
//
// The page has its own pinned header — title, a filter box, the stopped-rows
// toggle and the way back — above a column that scrolls on its own.

import type * as ipc from "./ipc";
import { firstLine, MARKDOWN_CSS, renderMarkdown } from "./markdown";
import {
  formatAgo,
  formatDuration,
  frozenStripText,
  lastRunText,
  recordView,
  runOutcome,
  runSummary,
  type PaneRun,
  type RecordPaneKind,
  type RunSummary,
  type SummaryView,
} from "./record_pane";
import { statsChip, statsTooltip, sumStats, totalsLabel, type TermStats } from "./stats";
import { renderEvents } from "./transcript";

/** A row's attention, as the rail derives it (typed waiting outranks a bell). */
export type CardAttention = null | "badge" | "agent-waiting";

/** The facts of a row whose terminal is attached in this window. */
export interface CardLive {
  status: string;
  exitCode: number | null;
  /** When the terminal was seen to die; null while it lives. */
  endedAtMs: number | null;
  attention: CardAttention;
  /** It produced output while hidden. */
  activity: boolean;
  /** The last stats sample, or null when it never reported. */
  stats: TermStats | null;
  /** What the current run posted about itself. */
  summary: RunSummary | null;
}

/** One card's data, as the app assembles it from a row. */
export interface CardInput {
  /** Stable across repaints: `cmd:<project>:<name>`, `ws:<name>`,
   *  `rec:<record id>` or `term:<term id>` for a row without a record. */
  key: string;
  kind: RecordPaneKind;
  name: string;
  /** The record behind the row (null for an ephemeral row): it has a run
   *  history to expand. */
  recordId: string | null;
  /** The record's newest run as last seen. */
  lastRun: PaneRun | null;
  canResume?: boolean;
  captures?: boolean;
  /** The attached terminal, or null when the row is stopped. */
  live: CardLive | null;
  /** The newest line the row produced, for a card without a summary (a
   *  json agent's last text event, or the stored tail's last line). */
  lastText?: string | null;
}

/** What a card can do from its hover actions. */
export type CardActionId = "start" | "resume" | "stop" | "restart";

export interface CardAction {
  id: CardActionId;
  label: string;
}

/** A card's content: plain data, so a repaint can compare it whole. */
export interface CardView {
  key: string;
  kind: RecordPaneKind;
  name: string;
  /** The status the dot shows (an attached terminal's, else the record's
   *  resting outcome). */
  status: string;
  /** Starting or running. */
  live: boolean;
  kindLabel: string;
  stats: string;
  statsTitle: string;
  attention: CardAttention;
  activity: boolean;
  /** The run line: an attached dead terminal's strip text, a stopped row's
   *  last run, nothing while live. */
  run: string | null;
  runTitle: string | null;
  summary: SummaryView | null;
  /** Shown only without a summary. */
  line: string | null;
  session: string | null;
  sessionTitle: string | null;
  actions: CardAction[];
  /** When the newest run ended (ordering of stopped cards). */
  endedAtMs: number | null;
  recorded: boolean;
}

const KIND_LABEL: Record<RecordPaneKind, string> = { agent: "agent", terminal: "shell", command: "command" };

function isLiveStatus(status: string): boolean {
  return status === "starting" || status === "running";
}

/** The card for one row: the record pane's view of it, plus the live facts
 *  while a terminal is attached. */
export function cardView(input: CardInput, now: number): CardView {
  const view = recordView(
    { kind: input.kind, name: input.name, lastRun: input.lastRun, canResume: input.canResume, captures: input.captures },
    now,
  );
  const live = input.live;
  const isLive = live !== null && isLiveStatus(live.status);
  let status: string;
  let run: string | null = null;
  let runTitle: string | null = null;
  let summary: SummaryView | null;
  let endedAtMs: number | null;
  let actions: CardAction[];
  if (live) {
    status = live.status;
    endedAtMs = live.endedAtMs;
    if (!isLive) {
      run = frozenStripText({ status: live.status, exitCode: live.exitCode, endedAtMs: live.endedAtMs }, now);
      runTitle = live.endedAtMs !== null ? new Date(live.endedAtMs).toLocaleString() : null;
    }
    // The attached terminal's current run: the stored head may be the
    // previous run's, so its summary is not this one's.
    summary = live.summary
      ? {
          ...live.summary,
          ended: live.endedAtMs !== null && !isLive ? formatAgo(live.endedAtMs, now) : null,
          endedTitle: runTitle,
        }
      : null;
    actions = isLive
      ? input.recordId !== null || input.kind === "command"
        ? [
            { id: "stop", label: "■ Stop" },
            { id: "restart", label: "⟲ Restart" },
          ]
        : [{ id: "stop", label: "■ Stop" }]
      : input.recordId !== null || input.kind === "command"
        ? view.actions.map((a) => ({ id: a.id, label: a.label }))
        : [];
  } else {
    status = input.lastRun ? restingStatus(input.lastRun) : "stopped";
    endedAtMs = input.lastRun?.ended_at_ms ?? null;
    if (view.lastRun) {
      run = `last run · ${lastRunText(view.lastRun)}`;
      runTitle = view.lastRun.endedTitle;
    }
    summary = view.summary;
    actions = view.actions.map((a) => ({ id: a.id, label: a.label }));
  }
  const stats = isLive && live?.stats ? statsChip(live.stats) : "";
  return {
    key: input.key,
    kind: input.kind,
    name: input.name.trim() || KIND_LABEL[input.kind],
    status,
    live: isLive,
    kindLabel: KIND_LABEL[input.kind],
    stats,
    statsTitle: stats && live?.stats ? statsTooltip(live.stats) : "",
    attention: isLive ? (live?.attention ?? null) : null,
    activity: live?.activity ?? false,
    run,
    runTitle,
    summary,
    line: summary ? null : (input.lastText?.trim() || null),
    session: view.session,
    sessionTitle: view.sessionTitle,
    actions,
    endedAtMs,
    recorded: input.recordId !== null,
  };
}

/** A stopped row's dot: how its last run ended. */
function restingStatus(run: PaneRun): string {
  if (run.status === "exited" || run.status === "failed" || run.status === "stopped") return run.status;
  // A run left "running" by a crash or a lost event is not running now.
  return "stopped";
}

/** Attention first (typed waiting above a bell), then live rows, then the
 *  rest by when they ended, newest first; the incoming order breaks ties. */
export function orderCards(views: CardView[]): CardView[] {
  const rank = (v: CardView): number =>
    v.attention === "agent-waiting" ? 0 : v.attention === "badge" ? 1 : v.live ? 2 : 3;
  return views
    .map((v, i) => ({ v, i }))
    .sort((a, b) => {
      const r = rank(a.v) - rank(b.v);
      if (r !== 0) return r;
      if (rank(a.v) === 3) {
        const ea = a.v.endedAtMs ?? -Infinity;
        const eb = b.v.endedAtMs ?? -Infinity;
        if (ea !== eb) return eb > ea ? 1 : -1;
      }
      return a.i - b.i;
    })
    .map((x) => x.v);
}

/** The filter: every whitespace-separated word must appear (any case) in the
 *  card's name, kind, status or its group's name. An empty filter keeps all. */
export function matchesFilter(view: CardView, groupName: string, filter: string): boolean {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const hay = `${view.name} ${view.kindLabel} ${view.status} ${groupName}`.toLowerCase();
  return words.every((w) => hay.includes(w));
}

/** Whether a card shows with the stopped rows hidden: it is live or wants
 *  attention. */
export function isActiveCard(view: CardView): boolean {
  return view.live || view.attention !== null;
}

/** The newest line a stored run left: a json agent's last text event (its
 *  first line), or the text tail's last non-blank line. */
export function tailLine(run: Pick<ipc.RunDto, "tail_kind" | "tail">): string | null {
  const tail = run.tail ?? [];
  for (let i = tail.length - 1; i >= 0; i--) {
    const item = tail[i];
    if (run.tail_kind === "events") {
      const e = item as { kind?: unknown; payload?: { text?: unknown; thinking?: unknown } } | null;
      if (e?.kind === "text" && typeof e.payload?.text === "string" && e.payload.thinking !== true) {
        const line = firstLine(e.payload.text).trim();
        if (line) return line;
      }
    } else if (typeof item === "string" && item.trim() !== "") {
      return item.trim();
    }
  }
  return null;
}

/** One ledger run as a history row: `2 h ago · ran 3m 4s · exit 1`. */
export function historyRowText(run: ipc.RunDto, now: number): string {
  const end = run.ended_at_ms ?? null;
  return [
    `started ${formatAgo(run.started_at_ms, now)}`,
    end !== null ? `ran ${formatDuration(end - run.started_at_ms)}` : "no end recorded",
    runOutcome(run),
  ].join(" · ");
}

/** One project's (or the workspace's) cards. */
export interface DashGroupInput {
  /** `p:<project id>` or `ws`. */
  key: string;
  name: string;
  /** The project's badge, built by the app; null for the workspace. */
  badge: (() => HTMLElement) | null;
  /** What the badge shows, so a repaint knows when to rebuild it. */
  badgeSig: string;
  cards: CardInput[];
}

export interface DashboardOptions {
  /** Click on a card: front its row (its pane, or its record card). */
  onJump: (key: string) => void;
  onAction: (key: string, action: CardActionId) => void;
  /** The back control or Escape. */
  onClose: () => void;
  /** A card's run history, newest first (null: none to read). */
  loadRuns: (key: string) => Promise<ipc.RunDto[] | null>;
}

export const DASHBOARD_CSS = `${MARKDOWN_CSS}
.chappa-panel-host.chappa-dashboard{display:none;}
.chappa-panel-host.chappa-dashboard.active{display:flex;flex-direction:column;outline:none;background:#0d0e10;}
.chappa-dash-header{flex:none;display:flex;align-items:center;gap:14px;padding:10px 20px;border-bottom:1px solid #1c1f24;}
.chappa-dash-back{display:inline-flex;align-items:center;gap:8px;border:0;background:none;color:#8b949e;font:600 15px system-ui,sans-serif;cursor:pointer;padding:2px 0;}
.chappa-dash-back > span:last-child{color:#d6d8dc;}
.chappa-dash-back:hover{color:#d6d8dc;}
.chappa-dash-filter{flex:0 1 240px;min-width:80px;padding:4px 8px;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#d6d8dc;font:12px system-ui,sans-serif;}
.chappa-dash-stopped{display:flex;align-items:center;gap:6px;font:12px system-ui,sans-serif;color:#8b949e;cursor:pointer;}
.chappa-dash-body{flex:1 1 auto;min-height:0;overflow-y:auto;padding:12px 20px 48px;}
.chappa-dash-empty{padding:24px 0;color:#6b7280;font:13px system-ui,sans-serif;}
.chappa-dash-group{margin-bottom:18px;}
.chappa-dash-group-head{display:flex;align-items:center;gap:8px;margin:4px 0 8px;font:600 13px system-ui,sans-serif;color:#d6d8dc;}
.chappa-dash-group-badge{display:flex;}
.chappa-dash-group-badge:empty{display:none;}
.chappa-dash-group-totals{font:400 11px ui-monospace,monospace;color:#6b7280;}
.chappa-dash-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:10px;}
.chappa-dash-card{position:relative;display:flex;flex-direction:column;gap:5px;min-width:0;padding:9px 11px;border:1px solid #22252a;border-radius:8px;background:#121417;cursor:pointer;outline:none;}
.chappa-dash-card:hover,.chappa-dash-card:focus-visible{border-color:#353a43;}
.chappa-dash-card.attention{box-shadow:inset 0 0 0 1px #f0b429;}
.chappa-dash-card.attention[data-attention="agent-waiting"]{box-shadow:inset 0 0 0 1px #5ea6ff;}
.chappa-dash-card.stopped .chappa-dash-name{color:#8b949e;}
.chappa-dash-top{display:flex;align-items:center;gap:6px;min-width:0;}
.chappa-dash-name{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:13px system-ui,sans-serif;}
.chappa-dash-kind{flex:none;padding:0 5px;border:1px solid #2a2e35;border-radius:8px;font:10px ui-monospace,monospace;color:#8b949e;}
.chappa-dash-stats{flex:none;font:11px ui-monospace,monospace;color:#6b7280;}
.chappa-dash-stats:empty,.chappa-dash-mark:empty{display:none;}
.chappa-dash-mark{flex:none;color:#f0b429;font:11px ui-monospace,monospace;}
.chappa-dash-activity{flex:none;width:7px;height:7px;border-radius:50%;background:#5ea6ff;}
.chappa-dash-run,.chappa-dash-session,.chappa-dash-line{font:11px ui-monospace,monospace;color:#8b949e;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-dash-session{color:#6b7280;}
.chappa-dash-run:empty,.chappa-dash-session:empty,.chappa-dash-line:empty,.chappa-dash-summary:empty{display:none;}
.chappa-dash-summary{max-height:8.5em;overflow:hidden;padding:5px 7px;border:1px solid #1c1f24;border-radius:5px;background:#0f1114;}
.chappa-dash-summary .chappa-md{font-size:12px;}
.chappa-dash-actions{position:absolute;top:6px;right:8px;display:none;gap:4px;padding-left:6px;background:#121417;}
.chappa-dash-card:hover .chappa-dash-actions,.chappa-dash-card:focus-within .chappa-dash-actions{display:flex;}
.chappa-dash-action{border:1px solid #262a31;border-radius:4px;background:#16181d;color:#d6d8dc;font:11px system-ui,sans-serif;cursor:pointer;padding:1px 7px;}
.chappa-dash-action:hover{background:#1d232b;}
.chappa-dash-history-toggle{align-self:flex-start;border:0;background:none;color:#6b7280;font:11px system-ui,sans-serif;cursor:pointer;padding:0;}
.chappa-dash-history-toggle:hover{color:#d6d8dc;}
.chappa-dash-history{display:flex;flex-direction:column;gap:2px;}
.chappa-dash-history:empty{display:none;}
.chappa-dash-history-note{font:11px system-ui,sans-serif;color:#6b7280;}
.chappa-dash-run-row{display:block;width:100%;text-align:left;border:0;border-radius:4px;background:none;color:#adb3bd;font:11px ui-monospace,monospace;cursor:pointer;padding:2px 4px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-dash-run-row:hover{background:#1a1d22;}
.chappa-dash-run-detail{margin:2px 0 6px 6px;padding:6px 8px;border-left:2px solid #22252a;cursor:default;}
.chappa-dash-run-tail{margin:6px 0 0;max-height:30vh;overflow:auto;white-space:pre-wrap;word-break:break-word;color:#c9d1d9;font:11px ui-monospace,monospace;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = DASHBOARD_CSS;
  document.head.appendChild(el);
}

interface GroupRefs {
  el: HTMLElement;
  badge: HTMLElement;
  name: HTMLElement;
  totals: HTMLElement;
  grid: HTMLElement;
  sig: string;
  badgeSig: string;
}

interface CardRefs {
  el: HTMLElement;
  dot: HTMLElement;
  name: HTMLElement;
  kind: HTMLElement;
  stats: HTMLElement;
  mark: HTMLElement;
  activity: HTMLElement;
  run: HTMLElement;
  summary: HTMLElement;
  line: HTMLElement;
  session: HTMLElement;
  actions: HTMLElement;
  historyToggle: HTMLButtonElement;
  history: HTMLElement;
  /** The view this card last painted (JSON), and the parts that are costly
   *  to rebuild. */
  sig: string;
  summaryText: string | null;
  actionsSig: string;
  historyOpen: boolean;
  /** Bumped per expansion, so a slow read never fills a closed list. */
  historySeq: number;
}

/** Put `el` at child position `i` of `parent`, moving it only when it is
 *  not already there (an unmoved node keeps its focus and scroll). */
function placeAt(parent: HTMLElement, el: HTMLElement, i: number): void {
  const at = parent.children[i];
  if (at !== el) parent.insertBefore(el, at ?? null);
}

/** Drop the children of `parent` from position `n` on. */
function truncate(parent: HTMLElement, n: number): void {
  while (parent.children.length > n) parent.lastElementChild!.remove();
}

/** The dashboard page. One instance, mounted in the stack; `update` repaints
 *  it from the app's current rows. */
export class Dashboard {
  readonly host: HTMLDivElement;
  private readonly opts: DashboardOptions;
  private readonly filterInput: HTMLInputElement;
  private readonly stoppedToggle: HTMLInputElement;
  private readonly list: HTMLElement;
  private readonly empty: HTMLElement;
  private readonly groups = new Map<string, GroupRefs>();
  private readonly cards = new Map<string, CardRefs>();
  private inputs: DashGroupInput[] = [];
  private now = 0;

  constructor(opts: DashboardOptions) {
    injectStyles();
    this.opts = opts;
    this.host = document.createElement("div");
    this.host.className = "chappa-panel-host chappa-dashboard";
    this.host.tabIndex = -1;
    this.host.setAttribute("aria-label", "Dashboard");
    this.host.addEventListener("keydown", this.onKeydown);

    const header = document.createElement("div");
    header.className = "chappa-dash-header";
    const back = document.createElement("button");
    back.type = "button";
    back.className = "chappa-dash-back";
    back.title = "Back (Esc)";
    const arrow = document.createElement("span");
    arrow.setAttribute("aria-hidden", "true");
    arrow.textContent = "←";
    const title = document.createElement("span");
    title.textContent = "Dashboard";
    back.append(arrow, title);
    back.addEventListener("click", () => this.opts.onClose());
    this.filterInput = document.createElement("input");
    this.filterInput.type = "search";
    this.filterInput.className = "chappa-dash-filter";
    this.filterInput.placeholder = "Filter";
    this.filterInput.setAttribute("aria-label", "Filter rows");
    this.filterInput.addEventListener("input", () => this.paint());
    const stopped = document.createElement("label");
    stopped.className = "chappa-dash-stopped";
    this.stoppedToggle = document.createElement("input");
    this.stoppedToggle.type = "checkbox";
    this.stoppedToggle.className = "chappa-dash-stopped-toggle";
    this.stoppedToggle.checked = true;
    this.stoppedToggle.addEventListener("change", () => this.paint());
    stopped.append(this.stoppedToggle, "Stopped rows");
    header.append(back, this.filterInput, stopped);

    const body = document.createElement("div");
    body.className = "chappa-dash-body";
    this.list = document.createElement("div");
    this.list.className = "chappa-dash-groups";
    this.empty = document.createElement("div");
    this.empty.className = "chappa-dash-empty";
    body.append(this.list, this.empty);
    this.host.append(header, body);
  }

  /** Repaint from the app's rows; only the cards and headers whose content
   *  changed are touched. */
  update(groups: DashGroupInput[], now: number): void {
    this.inputs = groups;
    this.now = now;
    this.paint();
  }

  setActive(active: boolean): void {
    this.host.classList.toggle("active", active);
    if (active) this.host.focus({ preventScroll: true });
  }

  get active(): boolean {
    return this.host.classList.contains("active");
  }

  dispose(): void {
    this.host.removeEventListener("keydown", this.onKeydown);
    this.host.remove();
  }

  private paint(): void {
    const filter = this.filterInput.value;
    const showStopped = this.stoppedToggle.checked;
    const liveKeys = new Set<string>();
    let g = 0;
    let anyCard = false;
    for (const group of this.inputs) {
      const views = group.cards.map((c) => cardView(c, this.now));
      for (const c of group.cards) liveKeys.add(c.key);
      const shown = orderCards(
        views.filter((v) => (showStopped || isActiveCard(v)) && matchesFilter(v, group.name, filter)),
      );
      if (shown.length === 0) continue;
      anyCard = true;
      const refs = this.groupRefs(group);
      const running = views.filter((v) => v.live).length;
      const sum = sumStats(group.cards.map((c) => (c.live && isLiveStatus(c.live.status) ? c.live.stats : null)));
      this.paintGroup(refs, group, totalsLabel(running, views.length, sum));
      placeAt(this.list, refs.el, g++);
      shown.forEach((view, i) => {
        const card = this.cardRefs(view.key);
        this.paintCard(card, view);
        placeAt(refs.grid, card.el, i);
      });
      truncate(refs.grid, shown.length);
    }
    truncate(this.list, g);
    // A row that is gone forgets its card (and its open history).
    for (const key of [...this.cards.keys()]) if (!liveKeys.has(key)) this.cards.delete(key);
    for (const key of [...this.groups.keys()]) if (!this.inputs.some((x) => x.key === key)) this.groups.delete(key);
    this.empty.textContent = anyCard
      ? ""
      : this.inputs.some((x) => x.cards.length > 0)
        ? "No rows match."
        : "Nothing here yet: open a project, or start a terminal or an agent.";
  }

  private groupRefs(group: DashGroupInput): GroupRefs {
    let refs = this.groups.get(group.key);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-dash-group";
    el.dataset.group = group.key;
    const head = document.createElement("div");
    head.className = "chappa-dash-group-head";
    const badge = document.createElement("span");
    badge.className = "chappa-dash-group-badge";
    const name = document.createElement("span");
    name.className = "chappa-dash-group-name";
    const totals = document.createElement("span");
    totals.className = "chappa-dash-group-totals";
    head.append(badge, name, totals);
    const grid = document.createElement("div");
    grid.className = "chappa-dash-grid";
    el.append(head, grid);
    refs = { el, badge, name, totals, grid, sig: "", badgeSig: "\u0000never" };
    this.groups.set(group.key, refs);
    return refs;
  }

  private paintGroup(refs: GroupRefs, group: DashGroupInput, totals: string): void {
    if (refs.badgeSig !== group.badgeSig) {
      refs.badgeSig = group.badgeSig;
      refs.badge.textContent = "";
      if (group.badge) refs.badge.appendChild(group.badge());
    }
    const sig = `${group.name}\u001f${totals}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.name.textContent = group.name;
    refs.totals.textContent = totals;
  }

  private cardRefs(key: string): CardRefs {
    let refs = this.cards.get(key);
    if (refs) return refs;
    const el = document.createElement("div");
    el.className = "chappa-dash-card";
    el.tabIndex = 0;
    el.setAttribute("role", "button");
    el.dataset.key = key;
    const top = document.createElement("div");
    top.className = "chappa-dash-top";
    const dot = document.createElement("span");
    const name = document.createElement("span");
    name.className = "chappa-dash-name";
    const kind = document.createElement("span");
    kind.className = "chappa-dash-kind";
    const stats = document.createElement("span");
    stats.className = "chappa-dash-stats";
    const activity = document.createElement("span");
    activity.className = "chappa-dash-activity";
    activity.title = "produced output while hidden";
    const mark = document.createElement("span");
    mark.className = "chappa-dash-mark";
    top.append(dot, name, kind, stats, activity, mark);
    const run = document.createElement("div");
    run.className = "chappa-dash-run";
    const summary = document.createElement("div");
    summary.className = "chappa-dash-summary";
    const line = document.createElement("div");
    line.className = "chappa-dash-line";
    const session = document.createElement("div");
    session.className = "chappa-dash-session";
    const actions = document.createElement("div");
    actions.className = "chappa-dash-actions";
    const historyToggle = document.createElement("button");
    historyToggle.type = "button";
    historyToggle.className = "chappa-dash-history-toggle";
    const history = document.createElement("div");
    history.className = "chappa-dash-history";
    el.append(top, run, summary, line, session, historyToggle, history, actions);
    refs = {
      el,
      dot,
      name,
      kind,
      stats,
      mark,
      activity,
      run,
      summary,
      line,
      session,
      actions,
      historyToggle,
      history,
      sig: "",
      summaryText: null,
      actionsSig: "\u0000never",
      historyOpen: false,
      historySeq: 0,
    };
    const card = refs;
    el.addEventListener("click", (e) => {
      // Buttons and the open history are their own targets, never a jump.
      if ((e.target as Element).closest("button, .chappa-dash-history")) return;
      this.opts.onJump(key);
    });
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && e.target === el) {
        e.preventDefault();
        this.opts.onJump(key);
      }
    });
    historyToggle.addEventListener("mousedown", (e) => e.preventDefault());
    historyToggle.addEventListener("click", () => this.toggleHistory(key, card));
    this.cards.set(key, refs);
    return refs;
  }

  /** Repaint one card in place from its view; a view equal to the last
   *  painted one costs nothing. */
  private paintCard(refs: CardRefs, view: CardView): void {
    const sig = JSON.stringify(view);
    if (sig === refs.sig) return;
    refs.sig = sig;
    const el = refs.el;
    el.classList.toggle("live", view.live);
    el.classList.toggle("stopped", !view.live);
    el.classList.toggle("attention", view.attention !== null);
    if (view.attention !== null) el.dataset.attention = view.attention;
    else delete el.dataset.attention;
    el.dataset.status = view.status;
    el.title = view.attention === "agent-waiting" ? `${view.name} — waiting for your input` : view.name;
    refs.dot.className = `chappa-status-dot ${view.status}${view.live ? "" : " dim"}`;
    refs.dot.title = view.status;
    refs.name.textContent = view.name;
    refs.kind.textContent = view.kindLabel;
    refs.stats.textContent = view.stats;
    refs.stats.title = view.statsTitle;
    refs.activity.style.display = view.activity ? "" : "none";
    refs.mark.textContent = view.attention === "agent-waiting" ? "⌨" : view.attention === "badge" ? "!" : "";
    refs.mark.title = view.attention === "agent-waiting" ? "waiting for your input" : view.attention ? "needs attention" : "";
    refs.run.textContent = view.run ?? "";
    refs.run.title = view.runTitle ?? "";
    const text = view.summary?.text ?? null;
    if (text !== refs.summaryText) {
      refs.summaryText = text;
      refs.summary.textContent = "";
      if (text !== null) refs.summary.appendChild(renderMarkdown(text));
    }
    refs.summary.title = view.summary
      ? [view.summary.source ? `summary · ${view.summary.source}` : "summary", view.summary.ended ? `ended ${view.summary.ended}` : ""]
          .filter(Boolean)
          .join(" · ")
      : "";
    refs.line.textContent = view.line ?? "";
    refs.line.title = view.line ?? "";
    refs.session.textContent = view.session ?? "";
    refs.session.title = view.sessionTitle ?? "";
    const actionsSig = view.actions.map((a) => `${a.id}:${a.label}`).join("|");
    if (actionsSig !== refs.actionsSig) {
      refs.actionsSig = actionsSig;
      refs.actions.textContent = "";
      for (const action of view.actions) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "chappa-dash-action";
        btn.dataset.action = action.id;
        btn.textContent = action.label;
        btn.addEventListener("mousedown", (e) => e.preventDefault());
        btn.addEventListener("click", () => this.opts.onAction(view.key, action.id));
        refs.actions.appendChild(btn);
      }
    }
    refs.historyToggle.style.display = view.recorded ? "" : "none";
    refs.historyToggle.textContent = refs.historyOpen ? "History ▾" : "History ▸";
  }

  /** Open or close a card's run history. Each opening reads the ledger
   *  once; a repaint never does. */
  private toggleHistory(key: string, refs: CardRefs): void {
    refs.historyOpen = !refs.historyOpen;
    refs.historyToggle.textContent = refs.historyOpen ? "History ▾" : "History ▸";
    const seq = ++refs.historySeq;
    refs.history.textContent = "";
    if (!refs.historyOpen) return;
    refs.history.appendChild(note("Loading…"));
    void this.opts
      .loadRuns(key)
      .catch(() => null)
      .then((runs) => {
        if (seq !== refs.historySeq || !refs.historyOpen) return;
        refs.history.textContent = "";
        if (!runs || runs.length === 0) {
          refs.history.appendChild(note("No runs recorded."));
          return;
        }
        for (const run of runs) refs.history.appendChild(this.historyRow(run));
      });
  }

  /** One run of the history: its one-line facts and summary's first line;
   *  a click unfolds the full summary and the stored tail. Read-only. */
  private historyRow(run: ipc.RunDto): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "chappa-dash-run-item";
    const row = document.createElement("button");
    row.type = "button";
    row.className = "chappa-dash-run-row";
    const posted = runSummary(run);
    const facts = historyRowText(run, this.now);
    row.textContent = posted ? `${facts} — ${firstLine(posted.text)}` : facts;
    row.title = new Date(run.started_at_ms).toLocaleString();
    row.addEventListener("mousedown", (e) => e.preventDefault());
    let detail: HTMLElement | null = null;
    row.addEventListener("click", () => {
      if (detail) {
        detail.remove();
        detail = null;
        return;
      }
      detail = document.createElement("div");
      detail.className = "chappa-dash-run-detail";
      if (posted) detail.appendChild(renderMarkdown(posted.text));
      const tail = run.tail ?? [];
      if (tail.length > 0) {
        if (run.tail_kind === "events") {
          const box = document.createElement("div");
          box.className = "chappa-dash-run-tail";
          box.append(...renderEvents(tail));
          detail.appendChild(box);
        } else {
          const pre = document.createElement("pre");
          pre.className = "chappa-dash-run-tail";
          pre.textContent = tail.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n");
          detail.appendChild(pre);
        }
      }
      if (!detail.firstChild) detail.appendChild(note("No summary or output was stored for this run."));
      wrap.appendChild(detail);
    });
    wrap.appendChild(row);
    return wrap;
  }

  private onKeydown = (e: KeyboardEvent): void => {
    if (e.key !== "Escape" || e.isComposing) return;
    // Escape in a non-empty filter clears it first; otherwise it leaves.
    if (e.target === this.filterInput && this.filterInput.value !== "") {
      this.filterInput.value = "";
      this.paint();
      e.preventDefault();
      return;
    }
    e.preventDefault();
    this.opts.onClose();
  };
}

function note(text: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "chappa-dash-history-note";
  el.textContent = text;
  return el;
}
