// The status block of a Work item that carries a local workflow
// (`localStatus`): where the item stands, who may move it on, the move
// waiting for approval, the moves on offer as buttons, and the history.
//
// The workflow's rules stay in the source. A move is one of the item's
// endpoint actions, matched by id: Rust judges it like any other action and
// `runAction` asks its confirm text first. A move that requires a note
// always asks for one, even when the action itself made the note optional.
// A move the source lists but offers no action for is drawn disabled.

import { formatAgo } from "../record_pane";
import type * as ipc from "../ipc";
import { actionButtonState, outcomeText, runAction } from "../source_actions";
import type { ActionTarget } from "./needs_you";
import type { LocalStatus } from "./types";
import { chipEl, type SourceViewDeps } from "./view_common";

export const LOCAL_STATUS_CSS = `
.chappa-ls{border:1px solid #262a31;border-radius:7px;background:#121417;padding:8px 10px;margin-bottom:10px;display:flex;flex-direction:column;gap:6px;font:12px system-ui,sans-serif;color:#d6d8dc;}
.chappa-ls-head{display:flex;align-items:baseline;gap:8px;flex-wrap:wrap;}
.chappa-ls-label{font:600 13px system-ui,sans-serif;color:#e6e8ec;}
.chappa-ls-meta,.chappa-ls-gate{color:#8b949e;font-size:11px;}
.chappa-ls-meta:empty,.chappa-ls-gate:empty{display:none;}
.chappa-ls-pending{border:1px solid #5c4813;border-radius:6px;background:#1d1708;color:#f0d68a;padding:6px 8px;display:flex;flex-direction:column;gap:2px;}
.chappa-ls-pending-title{font-weight:600;}
.chappa-ls-pending-note{font-style:italic;overflow-wrap:anywhere;}
.chappa-ls-moves{display:flex;flex-wrap:wrap;gap:6px;align-items:center;}
.chappa-ls-moves:empty{display:none;}
.chappa-ls-move{display:inline-flex;align-items:center;gap:4px;}
.chappa-ls-history{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:2px;font:11px ui-monospace,monospace;color:#8b949e;}
.chappa-ls-history:empty{display:none;}
.chappa-ls-entry{display:flex;flex-wrap:wrap;gap:4px 8px;align-items:center;}
.chappa-ls-entry[data-override]{color:#f0b429;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = LOCAL_STATUS_CSS;
  document.head.appendChild(el);
}

/** What the current status's gate means for the user, or "" when anyone
 *  may move the item: `human` guards the way IN to the status (an agent
 *  asking to move an item there waits for a person's approval),
 *  `human-exit` the way OUT of it. */
export function gateText(gate: LocalStatus["gate"], label: string): string {
  if (gate === "human") return `Only a person can move it into ${label}.`;
  if (gate === "human-exit") return `Only a person can move it out of ${label}.`;
  return "";
}

/** "since 2 h ago · by Alex · round 2". */
export function statusMeta(ls: LocalStatus, now: number): string {
  const since = ls.since ? Date.parse(ls.since) : NaN;
  return [
    Number.isFinite(since) ? `since ${formatAgo(since, now)}` : null,
    ls.actor ? `by ${ls.actor}` : null,
    ls.round !== null && ls.round !== undefined ? `round ${ls.round}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

/** One history entry as a line: when, who, from → to, the note. */
export function historyLine(h: NonNullable<LocalStatus["history"]>[number], now: number): string {
  const at = Date.parse(h.at);
  const move = h.from || h.to ? `${h.from ?? "?"} → ${h.to ?? "?"}` : null;
  return [Number.isFinite(at) ? formatAgo(at, now) : h.at, h.actor ?? null, move, h.note ? `“${h.note}”` : null]
    .filter(Boolean)
    .join(" · ");
}

export class LocalStatusBlock {
  readonly el: HTMLElement;
  private readonly label: HTMLElement;
  private readonly meta: HTMLElement;
  private readonly gate: HTMLElement;
  private readonly pending: HTMLElement;
  private readonly moves: HTMLElement;
  private readonly history: HTMLOListElement;
  private sig = "";
  private movesSig = "";
  private seq = 0;

  constructor(private readonly deps: SourceViewDeps) {
    injectStyles();
    this.el = document.createElement("div");
    this.el.className = "chappa-ls";
    const head = document.createElement("div");
    head.className = "chappa-ls-head";
    this.label = document.createElement("span");
    this.label.className = "chappa-ls-label";
    this.meta = document.createElement("span");
    this.meta.className = "chappa-ls-meta";
    head.append(this.label, this.meta);
    this.gate = document.createElement("div");
    this.gate.className = "chappa-ls-gate";
    this.pending = document.createElement("div");
    this.pending.className = "chappa-ls-pending";
    this.moves = document.createElement("div");
    this.moves.className = "chappa-ls-moves";
    this.history = document.createElement("ol");
    this.history.className = "chappa-ls-history";
    this.el.append(head, this.gate, this.pending, this.moves, this.history);
    this.el.hidden = true;
  }

  /** Show `ls` for the item at `target`; null hides the block. `version`
   *  re-reads the moves' verdicts after the source's payload changed. */
  update(ls: LocalStatus | null, target: ActionTarget, version: string): void {
    this.el.hidden = ls === null;
    if (!ls) {
      this.sig = "";
      this.movesSig = "";
      this.moves.textContent = "";
      return;
    }
    const now = this.deps.now();
    const sig = JSON.stringify([ls.label, statusMeta(ls, now), ls.gate, ls.pendingApproval, ls.history, now - (now % 60_000)]);
    if (sig !== this.sig) {
      this.sig = sig;
      this.label.textContent = ls.label;
      this.meta.textContent = statusMeta(ls, now);
      this.gate.textContent = gateText(ls.gate, ls.label);
      this.paintPending(ls);
      this.paintHistory(ls, now);
    }
    const movesSig = JSON.stringify([ls.allowed, target, version]);
    if (movesSig !== this.movesSig) {
      this.movesSig = movesSig;
      void this.loadMoves(ls, target);
    }
  }

  private paintPending(ls: LocalStatus): void {
    const p = ls.pendingApproval;
    this.pending.hidden = !p;
    this.pending.textContent = "";
    if (!p) return;
    const title = document.createElement("div");
    title.className = "chappa-ls-pending-title";
    title.textContent = `Waiting for approval: move to ${p.to}`;
    this.pending.appendChild(title);
    if (p.by) {
      const by = document.createElement("div");
      by.className = "chappa-ls-pending-by";
      by.textContent = `asked by ${p.by}`;
      this.pending.appendChild(by);
    }
    if (p.note) {
      const note = document.createElement("div");
      note.className = "chappa-ls-pending-note";
      note.textContent = p.note;
      this.pending.appendChild(note);
    }
  }

  /** Newest first: the latest move is the one the user wants to see. */
  private paintHistory(ls: LocalStatus, now: number): void {
    this.history.textContent = "";
    for (const h of [...(ls.history ?? [])].reverse()) {
      const li = document.createElement("li");
      li.className = "chappa-ls-entry";
      const line = document.createElement("span");
      line.textContent = historyLine(h, now);
      line.title = new Date(h.at).toLocaleString();
      li.appendChild(line);
      if (h.override) {
        li.dataset.override = "";
        li.appendChild(chipEl({ text: `override: ${h.override}`, tone: "warn", title: "This move bypassed the workflow's rules" }));
      }
      this.history.appendChild(li);
    }
  }

  private async loadMoves(ls: LocalStatus, target: ActionTarget): Promise<void> {
    const seq = ++this.seq;
    let entries: ipc.SourceActionDto[] = [];
    if (ls.allowed.length) {
      try {
        entries = await this.deps.actions(target.sourceId, target.view, target.key);
      } catch {
        entries = [];
      }
    }
    if (seq !== this.seq) return;
    this.moves.textContent = "";
    for (const move of ls.allowed) {
      const entry = entries.find((e) => e.action.id === move.id);
      const wrap = document.createElement("span");
      wrap.className = "chappa-ls-move";
      wrap.dataset.move = move.id;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-sv-action";
      if (!entry) {
        btn.textContent = move.label;
        btn.disabled = true;
        btn.title = "The source offers no action for this move.";
      } else {
        const state = actionButtonState(entry);
        if (state.hidden) continue;
        btn.textContent = state.asksFirst || move.requireNote ? `${move.label}…` : move.label;
        btn.disabled = state.disabled;
        btn.title = state.disabled ? state.title : move.requireNote ? `${state.title}; a note is required` : state.title;
        btn.addEventListener("mousedown", (e) => e.preventDefault());
        btn.addEventListener("click", () => void this.run(target, entry, move.requireNote === true));
      }
      wrap.appendChild(btn);
      if (move.humanOnly) wrap.appendChild(chipEl({ text: "person only", tone: "muted", title: "Only a person may make this move, never an agent" }));
      this.moves.appendChild(wrap);
    }
  }

  private async run(target: ActionTarget, entry: ipc.SourceActionDto, requireNote: boolean): Promise<void> {
    // The workflow wants a note on this move: ask for it even when the
    // action's own note rule is optional or absent.
    const asked: ipc.SourceActionDto =
      requireNote && entry.action.kind === "endpoint" ? { ...entry, action: { ...entry.action, note: "required" } } : entry;
    try {
      const outcome = await runAction(target.sourceId, target.sourceLabel, target.view, target.key, asked, { run: this.deps.run });
      if (!outcome) return;
      const text = outcomeText(outcome);
      if (text) this.deps.say(text, outcome.kind === "uncertain" ? "error" : "info");
    } catch (e) {
      this.deps.say(`${entry.action.label}: ${e instanceof Error ? e.message : String(e)}`, "error");
    }
  }
}
