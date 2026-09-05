// The transcript view: what a json-transport agent renders
// INSTEAD of a terminal. Same panel host + activation/focus discipline as
// TerminalPanel (it implements `PanelHost`), no renderer, no GL budget.
//
// Content: one block per typed `agent://event` —
//   text          → a paragraph (thinking blocks muted + collapsed)
//   tool_call     → "▸ name" with collapsed args (call) / result (result)
//   usage         → a muted line: in/out tokens, context %, cost
//   turn_started  → a turn separator (turn N · model)
//   turn_ended    → (nothing of its own; the next separator closes the turn)
//   awaiting_input→ the input box gains the "waiting" ring
//   compaction    → a separator-style note
//   error         → a red block
//   raw           → collapsed JSON, muted (nothing is dropped)
// plus an input box at the bottom: Enter sends (`send_agent_input`),
// Shift+Enter inserts a newline. The user's own messages are echoed as
// "you" blocks the moment they are sent, so the transcript reads as a
// conversation even before the CLI acknowledges.
//
// Events arrive two ways: the App forwards live `agent://event`s to
// `apply()`, and `start()` replays the ring (`get_agent_events`) so a panel
// created for an agent that already spoke is not empty. Both paths go
// through the same seq guard, so a replayed + live duplicate renders once.

import type * as ipc from "./ipc";
import type { PanelHost, TerminalApi } from "./term/panel";

export interface TranscriptPanelOptions {
  container: HTMLElement;
  api: Pick<TerminalApi, "sendAgentInput" | "getAgentEvents">;
  /** Spawns the agent (the App's spawn seam) and returns its term id. */
  spawn: () => Promise<number>;
  /** Fired when the id is known (rail row before any event races it). */
  onCreated?: (id: number) => void;
  /** The App's "input was sent" hook: clears the agent-waiting attention
   *  locally (the CLI's turn_started confirms it later). */
  onSent?: (id: number, text: string) => void;
  active?: boolean;
}

const ROOT_CSS =
  "display:flex;flex-direction:column;height:100%;min-height:0;background:#0f1114;color:#d6d8dc;font:13px system-ui,sans-serif;";
const LOG_CSS = "flex:1 1 auto;min-height:0;overflow-y:auto;padding:10px 14px;display:flex;flex-direction:column;gap:6px;";
const FORM_CSS = "flex:none;display:flex;gap:8px;padding:8px 10px;border-top:1px solid #1c1f24;background:#121417;";
const INPUT_CSS =
  "flex:1 1 auto;min-height:38px;max-height:160px;resize:vertical;background:#0f1114;color:#d6d8dc;border:1px solid #262a31;" +
  "border-radius:6px;padding:7px 9px;font:13px system-ui,sans-serif;outline:none;";
const SEND_CSS =
  "flex:none;align-self:flex-end;padding:7px 12px;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#d6d8dc;font:13px system-ui,sans-serif;cursor:pointer;";
const SEP_CSS = "display:flex;align-items:center;gap:8px;margin:8px 0 2px;color:#6b7280;font:11px ui-monospace,monospace;letter-spacing:.04em;";
const RULE_CSS = "flex:1 1 auto;height:1px;background:#1c1f24;";
const TEXT_CSS = "white-space:pre-wrap;word-break:break-word;line-height:1.45;";
const THINKING_CSS = "color:#6b7280;font-style:italic;";
const YOU_CSS = "align-self:flex-end;max-width:85%;background:#1d232b;border-radius:8px;padding:6px 10px;white-space:pre-wrap;word-break:break-word;";
const TOOL_CSS = "border:1px solid #262a31;border-radius:6px;background:#121417;padding:4px 8px;font:12px ui-monospace,monospace;";
const TOOL_SUMMARY_CSS = "cursor:pointer;color:#8b949e;list-style:none;";
const PRE_CSS = "margin:6px 0 2px;white-space:pre-wrap;word-break:break-word;color:#c9d1d9;font:12px ui-monospace,monospace;max-height:280px;overflow:auto;";
const USAGE_CSS = "color:#6b7280;font:11px ui-monospace,monospace;";
const ERROR_CSS = "border:1px solid #5c2f2f;border-radius:6px;background:#2a1717;color:#f0a0a0;padding:6px 10px;white-space:pre-wrap;word-break:break-word;";
const RAW_CSS = "color:#565b64;font:11px ui-monospace,monospace;";
const WAITING_STATUS_CSS = "flex:none;padding:0 10px 6px;color:#f0b429;font:11px ui-monospace,monospace;letter-spacing:.04em;";

/** One event's content as a string (tool args / results / raw objects). */
function pretty(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    // claude tool_result content: [{type: "text", text}] → the texts.
    const texts = value.map((v) =>
      v && typeof v === "object" && typeof (v as { text?: unknown }).text === "string"
        ? (v as { text: string }).text
        : JSON.stringify(v, null, 2),
    );
    return texts.join("\n");
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** The usage line: `in 1.2k · out 64 · ctx 3.4% · $0.0150`. */
export function usageLine(p: Record<string, unknown>): string {
  const n = (k: string): number | null => (typeof p[k] === "number" ? (p[k] as number) : null);
  const fmt = (v: number): string => (v >= 1_000 ? `${(v / 1000).toFixed(v >= 100_000 ? 0 : 1)}k` : String(v));
  const parts: string[] = [];
  const input = n("input_tokens");
  const output = n("output_tokens");
  if (input !== null) parts.push(`in ${fmt(input)}`);
  if (output !== null) parts.push(`out ${fmt(output)}`);
  const ctx = n("context_tokens");
  const pct = n("context_pct");
  if (pct !== null) parts.push(`ctx ${pct}%`);
  else if (ctx !== null) parts.push(`ctx ${fmt(ctx)}`);
  const cost = n("total_cost_usd");
  if (cost !== null && cost > 0) parts.push(`$${cost.toFixed(4)}`);
  return parts.join(" · ");
}

/** The replayed ring + the live events held during the replay, in seq
 *  order, one per seq (a replayed + live duplicate renders once). */
export function mergeBySeq(replay: ipc.AgentEventDto[], live: ipc.AgentEventDto[]): ipc.AgentEventDto[] {
  const bySeq = new Map<number, ipc.AgentEventDto>();
  for (const e of replay) bySeq.set(e.seq, e);
  for (const e of live) if (!bySeq.has(e.seq)) bySeq.set(e.seq, e);
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

export class TranscriptPanel implements PanelHost {
  readonly host: HTMLElement;
  readonly glLive = false;
  private readonly api: TranscriptPanelOptions["api"];
  private readonly opts: TranscriptPanelOptions;
  private readonly log: HTMLDivElement;
  private readonly input: HTMLTextAreaElement;
  private readonly sendBtn: HTMLButtonElement;
  private readonly waiting: HTMLDivElement;
  private termId = -1;
  private lastSeq = 0;
  private turns = 0;
  /** True between `onCreated` and the ring's answer: live events wait. */
  private replaying = false;
  private pending: ipc.AgentEventDto[] = [];
  private active: boolean;
  private disposed = false;
  private onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      void this.send();
    }
  };

  constructor(opts: TranscriptPanelOptions) {
    this.opts = opts;
    this.api = opts.api;
    this.active = opts.active ?? true;
    this.host = opts.container;
    this.host.classList.add("chappa-transcript-host");
    const root = document.createElement("div");
    root.className = "chappa-transcript";
    root.style.cssText = ROOT_CSS;
    this.log = document.createElement("div");
    this.log.className = "chappa-transcript-log";
    this.log.style.cssText = LOG_CSS;
    this.waiting = document.createElement("div");
    this.waiting.className = "chappa-transcript-waiting";
    this.waiting.style.cssText = WAITING_STATUS_CSS;
    this.waiting.textContent = "waiting for your input";
    this.waiting.style.display = "none";
    const form = document.createElement("div");
    form.className = "chappa-transcript-form";
    form.style.cssText = FORM_CSS;
    this.input = document.createElement("textarea");
    this.input.className = "chappa-transcript-input";
    this.input.style.cssText = INPUT_CSS;
    this.input.placeholder = "Message the agent — Enter sends, Shift+Enter for a newline";
    this.input.spellcheck = false;
    this.input.rows = 2;
    this.input.addEventListener("keydown", this.onKeydown);
    this.sendBtn = document.createElement("button");
    this.sendBtn.type = "button";
    this.sendBtn.className = "chappa-transcript-send";
    this.sendBtn.style.cssText = SEND_CSS;
    this.sendBtn.textContent = "Send";
    this.sendBtn.addEventListener("click", () => void this.send());
    form.append(this.input, this.sendBtn);
    root.append(this.log, this.waiting, form);
    this.host.appendChild(root);
  }

  /** Spawn, then replay the ring (events that landed before the panel
   *  listened). Idempotent.
   *
   *  `onCreated` fires BEFORE the replay await, so the App can forward live
   *  events from the first moment — those are held in `pending` until the
   *  ring answers, then merged by seq with the replay (deduped, in order)
   *  and rendered as one batch. Applying a live event mid-replay would
   *  advance `lastSeq` past the history and silently drop it. */
  async start(): Promise<number> {
    if (this.termId >= 0) return this.termId;
    const id = await this.opts.spawn();
    this.termId = id;
    this.replaying = true;
    this.opts.onCreated?.(id);
    let replay: ipc.AgentEventDto[] = [];
    try {
      replay = await this.api.getAgentEvents(id, 0);
    } catch {
      /* not in tauri, or the agent is gone — the live events still land */
    }
    const pending = this.pending;
    this.pending = [];
    this.replaying = false;
    this.applyBatch(mergeBySeq(replay, pending));
    if (this.active) this.focusInput();
    return id;
  }

  get id(): number {
    return this.termId;
  }

  /** Render one typed event (live or replayed); duplicates by seq are
   *  ignored, out-of-order older events too. During the replay await a
   *  live event is held, not dropped (see `start`). */
  apply(e: ipc.AgentEventDto): void {
    if (this.disposed) return;
    if (this.replaying) {
      this.pending.push(e);
      return;
    }
    const el = this.render(e);
    if (el) this.append(el);
  }

  /** Render many in seq order into ONE DocumentFragment, with a single
   *  scroll fixup — the replay of a long ring must not reflow per event. */
  private applyBatch(events: ipc.AgentEventDto[]): void {
    if (this.disposed || events.length === 0) return;
    const fragment = document.createDocumentFragment();
    for (const e of events) {
      const el = this.render(e);
      if (el) fragment.appendChild(el);
    }
    if (fragment.childNodes.length === 0) return;
    const atBottom = this.isAtBottom();
    this.log.appendChild(fragment);
    if (atBottom) this.log.scrollTop = this.log.scrollHeight;
  }

  /** One event → its block (null for the kinds that only flip state).
   *  Advances the seq guard. */
  private render(e: ipc.AgentEventDto): HTMLElement | null {
    if (e.seq <= this.lastSeq) return null;
    this.lastSeq = e.seq;
    const p = e.payload ?? {};
    let el: HTMLElement | null = null;
    switch (e.kind) {
      case "turn_started": {
        this.turns += 1;
        this.setWaiting(false);
        const model = typeof p.model === "string" ? ` · ${p.model}` : "";
        el = this.separator(`turn ${this.turns}${model}`);
        el.dataset.kind = "turn_started";
        break;
      }
      case "turn_ended":
        return null; // the next separator closes it visually
      case "text": {
        el = document.createElement("div");
        el.className = "chappa-transcript-text";
        el.style.cssText = TEXT_CSS;
        if (p.thinking === true) {
          const details = document.createElement("details");
          details.className = "chappa-transcript-thinking";
          details.style.cssText = THINKING_CSS;
          const summary = document.createElement("summary");
          summary.style.cssText = TOOL_SUMMARY_CSS;
          summary.textContent = "thinking";
          const body = document.createElement("div");
          body.style.cssText = TEXT_CSS;
          body.textContent = pretty(p.text);
          details.append(summary, body);
          el.appendChild(details);
        } else {
          el.textContent = pretty(p.text);
        }
        break;
      }
      case "tool_call": {
        const phase = p.phase === "result" ? "result" : "call";
        const details = document.createElement("details");
        details.className = "chappa-transcript-tool";
        details.dataset.phase = phase;
        details.style.cssText = TOOL_CSS;
        const summary = document.createElement("summary");
        summary.style.cssText = TOOL_SUMMARY_CSS;
        const name = typeof p.name === "string" ? p.name : phase === "result" ? "result" : "tool";
        const flag = p.is_error === true ? " · error" : "";
        summary.textContent = phase === "result" ? `◂ ${name}${flag}` : `▸ ${name}`;
        const pre = document.createElement("pre");
        pre.style.cssText = PRE_CSS;
        pre.textContent = pretty(phase === "result" ? p.content : p.input);
        details.append(summary, pre);
        el = details;
        break;
      }
      case "usage": {
        el = document.createElement("div");
        el.className = "chappa-transcript-usage";
        el.style.cssText = USAGE_CSS;
        el.textContent = usageLine(p);
        break;
      }
      case "awaiting_input":
        this.setWaiting(true);
        return null;
      case "compaction":
        el = this.separator("context compacted");
        el.dataset.kind = "compaction";
        break;
      case "error": {
        el = document.createElement("div");
        el.className = "chappa-transcript-error";
        el.style.cssText = ERROR_CSS;
        const message = typeof p.message === "string" ? p.message : pretty(p.message ?? p);
        el.textContent = message || "error";
        break;
      }
      case "raw":
      default: {
        const details = document.createElement("details");
        details.className = "chappa-transcript-raw";
        details.style.cssText = RAW_CSS;
        const summary = document.createElement("summary");
        summary.style.cssText = TOOL_SUMMARY_CSS;
        const label =
          typeof p.stderr === "string"
            ? "stderr"
            : typeof p.line === "string"
              ? "line"
              : typeof p.type === "string"
                ? `raw · ${p.type}${typeof p.subtype === "string" ? `/${p.subtype}` : ""}`
                : "raw";
        summary.textContent = label;
        const pre = document.createElement("pre");
        pre.style.cssText = PRE_CSS;
        pre.textContent = typeof p.stderr === "string" ? p.stderr : typeof p.line === "string" ? p.line : pretty(p);
        details.append(summary, pre);
        el = details;
        break;
      }
    }
    if (el) el.dataset.seq = String(e.seq);
    return el;
  }

  /** Whether the agent is waiting for input (the box's ring + status line). */
  get waitingForInput(): boolean {
    return this.waiting.style.display !== "none";
  }

  private setWaiting(on: boolean): void {
    this.waiting.style.display = on ? "" : "none";
    this.input.classList.toggle("waiting", on);
    this.input.style.borderColor = on ? "#f0b429" : "#262a31";
  }

  private separator(label: string): HTMLElement {
    const sep = document.createElement("div");
    sep.className = "chappa-transcript-sep";
    sep.style.cssText = SEP_CSS;
    const left = document.createElement("span");
    left.style.cssText = RULE_CSS;
    const text = document.createElement("span");
    text.textContent = label;
    const right = document.createElement("span");
    right.style.cssText = RULE_CSS;
    sep.append(left, text, right);
    return sep;
  }

  private isAtBottom(): boolean {
    return this.log.scrollHeight - this.log.scrollTop - this.log.clientHeight < 40;
  }

  private append(el: HTMLElement): void {
    const atBottom = this.isAtBottom();
    this.log.appendChild(el);
    if (atBottom) this.log.scrollTop = this.log.scrollHeight;
  }

  /** Send the box's text as one user message. The echo block appears at
   *  once; the receipt's `delivered` is informational (a timeout means
   *  written-but-unacknowledged, which the next turn separator settles). */
  async send(): Promise<void> {
    const text = this.input.value.replace(/\r\n/g, "\n").trim();
    if (text === "" || this.termId < 0) return;
    this.input.value = "";
    const you = document.createElement("div");
    you.className = "chappa-transcript-you";
    you.style.cssText = YOU_CSS;
    you.textContent = text;
    this.append(you);
    this.setWaiting(false);
    this.opts.onSent?.(this.termId, text);
    try {
      const receipt = await this.api.sendAgentInput(this.termId, text);
      if (!receipt.delivered && receipt.reason && receipt.reason !== "timeout") {
        const note = document.createElement("div");
        note.className = "chappa-transcript-error";
        note.style.cssText = ERROR_CSS;
        note.textContent = `not delivered: ${receipt.reason}`;
        this.append(note);
      }
    } catch (err) {
      const note = document.createElement("div");
      note.className = "chappa-transcript-error";
      note.style.cssText = ERROR_CSS;
      note.textContent = `send failed: ${err instanceof Error ? err.message : String(err)}`;
      this.append(note);
    }
  }

  // --- PanelHost ------------------------------------------------------------

  setActive(active: boolean): void {
    if (this.active === active) return;
    this.active = active;
    if (active) this.focusInput();
  }

  focusInput(): void {
    this.input.focus();
  }

  /** A dropped file path / pasted text lands in the box (never auto-sent). */
  pasteText(text: string): void {
    if (!this.active) return;
    const start = this.input.selectionStart ?? this.input.value.length;
    const end = this.input.selectionEnd ?? start;
    this.input.value = this.input.value.slice(0, start) + text + this.input.value.slice(end);
    this.input.selectionStart = this.input.selectionEnd = start + text.length;
  }

  async setFontMetrics(_family: string, _size: number, _lineHeight: number): Promise<void> {
    /* not a grid: the transcript uses the UI font */
  }

  setSubprocessCount(_count: number): void {
    /* no hint bar */
  }

  releaseGlContext(): void {
    /* no GL */
  }

  restoreGlContext(): void {
    /* no GL */
  }

  dispose(_closeTerminal = true): void {
    // The App closes the agent itself (closeTerminal false on the agent
    // path); a transcript never issues the Rust close on its own.
    this.disposed = true;
    this.input.removeEventListener("keydown", this.onKeydown);
    this.host.textContent = "";
  }
}
