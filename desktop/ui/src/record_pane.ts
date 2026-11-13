// The record pane: what a rail row shows when no terminal is attached to it.
//
// A rail row's pane is exactly one of three things (`paneState`):
//   live   — the terminal panel, fed by frames;
//   frozen — an exited terminal whose panel is still attached: its output
//            stays readable, and a one-line strip ABOVE it (never over it)
//            offers the start action — plus, when the run posted a summary,
//            its first line, which expands OVER the output on click (the
//            output stays underneath);
//   record — nothing attached: the row's stored record renders as a centered
//            card ("<name> is stopped", a hint, the actions, keycap hints)
//            with the last run's outcome, its summary (markdown) and its
//            stored tail underneath.
//
// One RecordPane instance is mounted in the stack and re-rendered per
// activation. The strings and the view are built by pure functions so they
// are testable without a DOM; the class only draws them.

import type * as ipc from "./ipc";
import { firstLine, MARKDOWN_CSS, renderMarkdown } from "./markdown";
import { renderEvents } from "./transcript";

/** The kinds a record pane renders: the record kinds. */
export type RecordPaneKind = ipc.RecordKind;

/** The minimum a pane-state derivation needs from a rail entry. */
export interface PaneEntry {
  status: string;
  exitCode: number | null;
}

/** The minimum a pane-state derivation needs from a record. */
export interface PaneRecord {
  termId: number | null;
}

/** Which pane a row shows. `frozen` carries what the strip says. */
export type PaneState =
  | { kind: "live" }
  | { kind: "frozen"; status: string; exitCode: number | null }
  | { kind: "record" };

/** A row's pane, from its record (null for a row without one) and its
 *  attached entry (null when no terminal is attached in this window). An
 *  attached entry always wins: a live terminal is live, a dead one keeps its
 *  output (frozen). With nothing attached the record renders, whatever its
 *  stored status says — a record whose terminal lives in another window has
 *  nothing to draw here either. */
export function paneState(_record: PaneRecord | null, entry: PaneEntry | null): PaneState {
  if (!entry) return { kind: "record" };
  if (entry.status === "starting" || entry.status === "running") return { kind: "live" };
  return { kind: "frozen", status: entry.status, exitCode: entry.exitCode };
}

/** A run as the pane reads it: the head (`last_run`) or a full run. */
export interface PaneRun {
  /** Which run this is, when known (a summary event names its run). */
  run_uuid?: string;
  status: string;
  started_at_ms: number;
  ended_at_ms?: number | null;
  exit_code?: number | null;
  /** The provider session the run saved, when one was captured. */
  session?: ipc.SessionDto | null;
  /** What the process said its run was about (markdown), and how. */
  summary?: string | null;
  summary_source?: ipc.SummarySource | null;
}

/** A run summary as posted: the markdown and how it arrived. */
export interface RunSummary {
  text: string;
  source: ipc.SummarySource | null;
}

/** The card's summary block: the text plus its header facts. */
export interface SummaryView extends RunSummary {
  /** `2 min ago` — when the run ended; null while it has no end. */
  ended: string | null;
  endedTitle: string | null;
}

/** A run's summary, or null when it has none (or an empty one). */
export function runSummary(run: PaneRun | null | undefined): RunSummary | null {
  const text = run?.summary;
  if (typeof text !== "string" || text.trim() === "") return null;
  return { text, source: run?.summary_source ?? null };
}

/** The strip's collapsed line: `Summary ▸ <first line>`. */
export function summaryStripText(summary: RunSummary, expanded: boolean): string {
  return `Summary ${expanded ? "▾" : "▸"} ${firstLine(summary.text)}`;
}

export interface RecordPaneInput {
  kind: RecordPaneKind;
  name: string;
  lastRun: PaneRun | null;
  /** The agent's tool has a way to continue a saved session. */
  canResume?: boolean;
  /** The agent's tool is set up to capture session ids at all. */
  captures?: boolean;
}

/** One card action. Ordered: the first is the primary (S / Enter). */
export interface RecordAction {
  id: "start" | "resume";
  label: string;
}

export interface LastRunView {
  /** `exit 1`, `stopped`, `failed`, … */
  outcome: string;
  /** `3m 12s`, or null while the run has no end. */
  duration: string | null;
  /** `2 min ago`, or null while the run has no end. */
  ended: string | null;
  /** The absolute end time for the tooltip. */
  endedTitle: string | null;
}

export interface RecordView {
  title: string;
  hint: string;
  actions: RecordAction[];
  lastRun: LastRunView | null;
  /** Agents only: `Session saved · claude · …a1b2` or why resume is off. */
  session: string | null;
  /** The full session id, for the line's tooltip. */
  sessionTitle: string | null;
  /** The last run's summary, rendered above the tail. */
  summary: SummaryView | null;
}

/** The session line of an agent card. A saved id names the provider and
 *  its last four characters; without one the card says resume is off, and
 *  why when the tool was set up to capture. */
export function sessionLine(run: PaneRun | null, captures: boolean): string {
  const s = run?.session;
  if (s) {
    const from = s.resumed_from && s.resumed_from !== s.session_id ? ` (resumed from …${shortId(s.resumed_from)})` : "";
    return `Session saved · ${s.provider} · …${shortId(s.session_id)}${from}`;
  }
  return captures ? "Resume unavailable (no session id seen)" : "Resume unavailable";
}

function shortId(id: string): string {
  return id.length <= 4 ? id : id.slice(-4);
}

const GENERIC_TITLE: Record<RecordPaneKind, string> = {
  agent: "Agent is stopped",
  terminal: "Terminal is stopped",
  command: "Process is stopped",
};

const HINT: Record<RecordPaneKind, string> = {
  agent: "Start it to begin a new session.",
  terminal: "Start it to resume this shell.",
  command: "Start it to run this process.",
};

/** The start label: an agent always begins a NEW session. */
export function startLabel(kind: RecordPaneKind): string {
  return kind === "agent" ? "▶ Start new session" : "▶ Start";
}

/** `12s`, `3m 4s`, `1h 2m`. */
export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

/** `just now`, `2 min ago`, `3 h ago`, `4 d ago`. */
export function formatAgo(endedAtMs: number, now: number): string {
  const s = Math.max(0, Math.floor((now - endedAtMs) / 1000));
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.max(1, Math.floor(s / 60))} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

/** How a run ended, in a word or two. */
export function runOutcome(run: PaneRun): string {
  if (run.status === "stopped") return "stopped";
  if (run.exit_code !== null && run.exit_code !== undefined) return `exit ${run.exit_code}`;
  return run.status;
}

/** The card's content, from the record and its newest run. */
export function recordView(input: RecordPaneInput, now: number): RecordView {
  const name = input.name.trim();
  const run = input.lastRun;
  let lastRun: LastRunView | null = null;
  const posted = runSummary(run);
  let summary: SummaryView | null = null;
  if (run) {
    const end = run.ended_at_ms ?? null;
    lastRun = {
      outcome: runOutcome(run),
      duration: end !== null ? formatDuration(end - run.started_at_ms) : null,
      ended: end !== null ? formatAgo(end, now) : null,
      endedTitle: end !== null ? new Date(end).toLocaleString() : null,
    };
    if (posted) summary = { ...posted, ended: lastRun.ended, endedTitle: lastRun.endedTitle };
  }
  const actions: RecordAction[] = [];
  // Resuming comes first: the primary key continues the conversation when
  // one was saved and the tool can pick it up; a fresh start sends nothing.
  if (input.kind === "agent" && run?.session && input.canResume) {
    actions.push({ id: "resume", label: "↻ Resume last session" });
  }
  actions.push({ id: "start", label: startLabel(input.kind) });
  return {
    title: name ? `${name} is stopped` : GENERIC_TITLE[input.kind],
    hint: HINT[input.kind],
    actions,
    lastRun,
    session: input.kind === "agent" ? sessionLine(run, input.captures ?? false) : null,
    sessionTitle: input.kind === "agent" && run?.session ? run.session.session_id : null,
    summary,
  };
}

/** A last run in one line: `exit 1 · ran 3m 12s · ended 2 min ago` (the
 *  duration and the age drop out while the run has no end). */
export function lastRunText(run: LastRunView): string {
  const parts = [run.outcome];
  if (run.duration) parts.push(`ran ${run.duration}`);
  if (run.ended) parts.push(`ended ${run.ended}`);
  return parts.join(" · ");
}

/** The frozen strip's words: `exited · code 1 · 2 min ago`. The code is
 *  left out when none was observed, the age when the end time is unknown. */
export function frozenStripText(
  state: { status: string; exitCode: number | null; endedAtMs: number | null },
  now: number,
): string {
  const parts = [state.status];
  if (state.exitCode !== null) parts.push(`code ${state.exitCode}`);
  if (state.endedAtMs !== null) parts.push(formatAgo(state.endedAtMs, now));
  return parts.join(" · ");
}

/** A stored tail: screen lines, or the typed events of a json agent. */
export interface PaneTail {
  kind: ipc.TailKind;
  lines: readonly unknown[];
}

export const RECORD_PANE_CSS = `${MARKDOWN_CSS}
.chappa-panel-host.chappa-record-pane{display:none;}
.chappa-panel-host.chappa-record-pane.active{display:flex;flex-direction:column;align-items:center;overflow:auto;outline:none;padding:0 16px;box-sizing:border-box;}
.chappa-record-card{margin-top:18vh;display:flex;flex-direction:column;align-items:center;gap:8px;max-width:520px;}
.chappa-record-title{font:600 15px system-ui,sans-serif;color:#d6d8dc;text-align:center;word-break:break-word;}
.chappa-record-hint{font:13px system-ui,sans-serif;color:#8b949e;}
.chappa-record-session{font:12px ui-monospace,monospace;color:#6b7280;}
.chappa-record-session:empty{display:none;}
.chappa-record-failure{font:13px system-ui,sans-serif;color:#f0883e;text-align:center;max-width:520px;word-break:break-word;}
.chappa-record-failure:empty{display:none;}
.chappa-record-actions{display:flex;flex-direction:column;align-items:stretch;gap:6px;margin-top:4px;}
.chappa-record-action{padding:6px 14px;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#d6d8dc;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-record-action:hover{background:#1d232b;}
.chappa-record-keys{align-self:stretch;display:grid;grid-template-columns:auto auto;justify-content:end;gap:4px 10px;margin-top:6px;font:12px system-ui,sans-serif;color:#6b7280;}
.chappa-record-keys > span:nth-child(odd){text-align:right;}
.chappa-keycap{display:inline-block;min-width:14px;padding:0 5px;border:1px solid #2a2e35;border-bottom-width:2px;border-radius:4px;background:#14161a;color:#adb3bd;font:11px ui-monospace,monospace;line-height:16px;text-align:center;}
.chappa-record-run{margin-top:22px;font:12px ui-monospace,monospace;color:#8b949e;}
.chappa-record-summary{align-self:stretch;margin:10px 0 0;border:1px solid #1c1f24;border-radius:6px;background:#121417;padding:8px 10px;}
.chappa-record-summary:empty{display:none;}
.chappa-summary-head{display:flex;align-items:center;gap:8px;margin-bottom:6px;font:12px system-ui,sans-serif;color:#8b949e;}
.chappa-summary-label{font-weight:600;color:#adb3bd;}
.chappa-summary-source{padding:0 5px;border:1px solid #2a2e35;border-radius:8px;font:11px ui-monospace,monospace;color:#8b949e;}
.chappa-summary-source:empty{display:none;}
.chappa-summary-ended:empty{display:none;}
.chappa-record-tail{align-self:stretch;margin:10px 0 16px;max-height:45vh;overflow:auto;border:1px solid #1c1f24;border-radius:6px;background:#0f1114;padding:8px 10px;}
.chappa-record-tail:empty{display:none;}
.chappa-record-tail-text{margin:0;white-space:pre-wrap;word-break:break-word;color:#c9d1d9;font:12px ui-monospace,monospace;}
.chappa-record-tail-events{display:flex;flex-direction:column;gap:6px;font:13px system-ui,sans-serif;color:#d6d8dc;}
.chappa-frozen-strip{flex:none;display:none;position:relative;align-items:center;gap:10px;padding:4px 12px;border-bottom:1px solid #1c1f24;background:#121417;color:#8b949e;font:12px ui-monospace,monospace;}
.chappa-frozen-strip.shown{display:flex;}
.chappa-frozen-strip-text{flex:none;}
.chappa-frozen-strip-summary{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;text-align:left;border:0;background:none;color:#adb3bd;font:12px ui-monospace,monospace;cursor:pointer;padding:0;}
.chappa-frozen-strip-summary:hover{color:#d6d8dc;}
.chappa-frozen-strip-start{margin-left:auto;}
.chappa-frozen-summary{display:none;position:absolute;top:100%;left:0;right:0;z-index:5;max-height:60vh;overflow:auto;padding:10px 14px;background:#121417;border-bottom:1px solid #262a31;box-shadow:0 6px 16px rgba(0,0,0,.45);}
.chappa-frozen-strip.expanded .chappa-frozen-summary{display:block;}
.chappa-frozen-strip-start{border:1px solid #262a31;border-radius:4px;background:#16181d;color:#d6d8dc;font:12px system-ui,sans-serif;cursor:pointer;padding:2px 8px;}
.chappa-frozen-strip-start:hover{background:#1d232b;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = RECORD_PANE_CSS;
  document.head.appendChild(el);
}

function keycap(text: string): HTMLElement {
  const k = document.createElement("kbd");
  k.className = "chappa-keycap";
  k.textContent = text;
  return k;
}

export interface RecordPaneOptions {
  /** A card action was chosen (button, or S / Enter for the first one). */
  onAction: (id: RecordAction["id"]) => void;
  /** Ctrl+T on the focused pane: a new agent in this row's scope. */
  onNewAgent: () => void;
}

/** The stopped-row card. One instance; `show` re-renders it in place. */
export class RecordPane {
  readonly host: HTMLDivElement;
  private readonly opts: RecordPaneOptions;
  private readonly title: HTMLElement;
  private readonly hint: HTMLElement;
  private readonly session: HTMLElement;
  private readonly failure: HTMLElement;
  private readonly actions: HTMLElement;
  private readonly run: HTMLElement;
  private readonly summary: HTMLElement;
  private readonly tail: HTMLElement;
  private view: RecordView | null = null;

  constructor(opts: RecordPaneOptions) {
    injectStyles();
    this.opts = opts;
    this.host = document.createElement("div");
    this.host.className = "chappa-panel-host chappa-record-pane";
    this.host.tabIndex = -1;
    this.host.addEventListener("keydown", this.onKeydown);
    const card = document.createElement("div");
    card.className = "chappa-record-card";
    this.title = document.createElement("div");
    this.title.className = "chappa-record-title";
    this.hint = document.createElement("div");
    this.hint.className = "chappa-record-hint";
    this.session = document.createElement("div");
    this.session.className = "chappa-record-session";
    this.failure = document.createElement("div");
    this.failure.className = "chappa-record-failure";
    this.actions = document.createElement("div");
    this.actions.className = "chappa-record-actions";
    const keys = document.createElement("div");
    keys.className = "chappa-record-keys";
    const startKeys = document.createElement("span");
    startKeys.append(keycap("S"), " or ", keycap("Enter"));
    const newKeys = document.createElement("span");
    newKeys.append(keycap("Ctrl+T"));
    keys.append(label("Start"), startKeys, label("New agent"), newKeys);
    card.append(this.title, this.hint, this.session, this.failure, this.actions, keys);
    this.run = document.createElement("div");
    this.run.className = "chappa-record-run";
    // The run's summary, above the tail; empty (and hidden) without one.
    this.summary = document.createElement("div");
    this.summary.className = "chappa-record-summary";
    this.tail = document.createElement("div");
    this.tail.className = "chappa-record-tail";
    this.host.append(card, this.run, this.summary, this.tail);
  }

  /** Render a view; the tail empties until `setTail` delivers one. */
  show(view: RecordView): void {
    this.view = view;
    this.title.textContent = view.title;
    this.hint.textContent = view.hint;
    this.session.textContent = view.session ?? "";
    this.session.title = view.sessionTitle ?? "";
    this.failure.textContent = "";
    this.actions.textContent = "";
    for (const action of view.actions) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-record-action";
      btn.dataset.action = action.id;
      btn.textContent = action.label;
      btn.addEventListener("mousedown", (e) => e.preventDefault());
      btn.addEventListener("click", () => this.opts.onAction(action.id));
      this.actions.appendChild(btn);
    }
    this.setRun(view.lastRun);
    this.setSummary(view.summary);
    this.tail.textContent = "";
  }

  /** Replace the summary block: a `Summary` header with the source chip and
   *  the run's end, then the markdown. Null empties (and hides) it. */
  setSummary(summary: SummaryView | null): void {
    this.summary.textContent = "";
    if (!summary) return;
    const head = document.createElement("div");
    head.className = "chappa-summary-head";
    const label = document.createElement("span");
    label.className = "chappa-summary-label";
    label.textContent = "Summary";
    const source = document.createElement("span");
    source.className = "chappa-summary-source";
    source.textContent = summary.source ?? "";
    source.title = summary.source === "mcp" ? "posted with set_summary" : summary.source === "osc" ? "printed by the process" : "";
    const ended = document.createElement("span");
    ended.className = "chappa-summary-ended";
    ended.textContent = summary.ended ? `ended ${summary.ended}` : "";
    ended.title = summary.endedTitle ?? "";
    head.append(label, source, ended);
    this.summary.append(head, renderMarkdown(summary.text));
  }

  /** A resume the provider refused: say so on the card, above the actions,
   *  which still offer a fresh start. Cleared by the next `show`. */
  setFailure(text: string | null): void {
    this.failure.textContent = text ?? "";
  }

  /** Replace the last-run line (a fresher run arrived). */
  setRun(run: LastRunView | null): void {
    this.run.textContent = "";
    this.run.title = "";
    if (!run) return;
    this.run.textContent = `last run · ${lastRunText(run)}`;
    if (run.endedTitle) this.run.title = run.endedTitle;
  }

  /** Draw the stored tail under the card: monospace lines, or the
   *  transcript's own blocks for an events tail. */
  setTail(tail: PaneTail | null): void {
    this.tail.textContent = "";
    if (!tail || tail.lines.length === 0) return;
    if (tail.kind === "events") {
      const box = document.createElement("div");
      box.className = "chappa-record-tail-events";
      box.append(...renderEvents(tail.lines));
      this.tail.appendChild(box);
    } else {
      const pre = document.createElement("pre");
      pre.className = "chappa-record-tail-text";
      pre.textContent = tail.lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n");
      this.tail.appendChild(pre);
    }
    this.tail.scrollTop = this.tail.scrollHeight;
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

  private onKeydown = (e: KeyboardEvent): void => {
    if (e.isComposing) return;
    const plain = !e.ctrlKey && !e.metaKey && !e.altKey;
    if (plain && !e.shiftKey && (e.key === "s" || e.key === "S" || e.key === "Enter")) {
      const primary = this.view?.actions[0];
      if (!primary) return;
      e.preventDefault();
      this.opts.onAction(primary.id);
    } else if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "t") {
      e.preventDefault();
      this.opts.onNewAgent();
    }
  };
}

function label(text: string): HTMLElement {
  const s = document.createElement("span");
  s.textContent = text;
  return s;
}

/** The one-line strip above a frozen panel: what happened, the run's
 *  summary collapsed to its first line (click to expand it over the output,
 *  which stays underneath), and the start action when the row has
 *  something to start. */
export class FrozenStrip {
  readonly el: HTMLDivElement;
  private readonly text: HTMLSpanElement;
  private readonly summaryToggle: HTMLButtonElement;
  private readonly summaryBody: HTMLDivElement;
  private readonly start: HTMLButtonElement;
  private onStart: (() => void) | null = null;
  private summary: RunSummary | null = null;

  constructor() {
    injectStyles();
    this.el = document.createElement("div");
    this.el.className = "chappa-frozen-strip";
    this.text = document.createElement("span");
    this.text.className = "chappa-frozen-strip-text";
    this.summaryToggle = document.createElement("button");
    this.summaryToggle.type = "button";
    this.summaryToggle.className = "chappa-frozen-strip-summary";
    this.summaryToggle.style.display = "none";
    this.summaryToggle.addEventListener("mousedown", (e) => e.preventDefault());
    this.summaryToggle.addEventListener("click", () => this.setExpanded(!this.expanded));
    this.summaryBody = document.createElement("div");
    this.summaryBody.className = "chappa-frozen-summary";
    this.start = document.createElement("button");
    this.start.type = "button";
    this.start.className = "chappa-frozen-strip-start";
    this.start.addEventListener("mousedown", (e) => e.preventDefault());
    this.start.addEventListener("click", () => this.onStart?.());
    this.el.append(this.text, this.summaryToggle, this.start, this.summaryBody);
  }

  /** Show the strip; `start` null = nothing to start (an ephemeral row),
   *  `summary` null = the run posted none. Re-showing the same summary
   *  keeps it as it was (expanded or not); a different one starts
   *  collapsed. */
  show(text: string, start: { label: string; run: () => void } | null, summary: RunSummary | null = null): void {
    this.text.textContent = text;
    this.onStart = start?.run ?? null;
    this.start.style.display = start ? "" : "none";
    this.start.textContent = start?.label ?? "";
    if (summary?.text !== this.summary?.text || summary?.source !== this.summary?.source) {
      this.summary = summary;
      this.summaryBody.textContent = "";
      if (summary) this.summaryBody.appendChild(renderMarkdown(summary.text));
      this.setExpanded(false);
    }
    this.summaryToggle.style.display = summary ? "" : "none";
    this.el.classList.add("shown");
  }

  hide(): void {
    this.onStart = null;
    this.setExpanded(false);
    this.el.classList.remove("shown");
  }

  /** Whether the summary is open over the output. */
  get expanded(): boolean {
    return this.el.classList.contains("expanded");
  }

  setExpanded(expanded: boolean): void {
    const open = expanded && this.summary !== null;
    this.el.classList.toggle("expanded", open);
    this.summaryToggle.textContent = this.summary ? summaryStripText(this.summary, open) : "";
    this.summaryToggle.title = this.summary ? (open ? "Hide the run summary" : "Show the run summary") : "";
    this.summaryToggle.setAttribute("aria-expanded", String(open));
  }

  get shown(): boolean {
    return this.el.classList.contains("shown");
  }
}
