// What the source views share: the page frame (a main-area page with a
// back control, like the dashboard), the freshness line, chips, keyed-row
// placement and the action buttons.
//
// Action buttons never decide anything themselves: Rust judges every action
// (`source_actions`) and judges it again when it runs; a button shows that
// judgement through `actionButtonState` and runs through `runAction`, which
// asks whatever must be asked first.

import type * as ipc from "../ipc";
import { formatAgo } from "../record_pane";
import { actionButtonState, outcomeText, runAction } from "../source_actions";
import type { ActionTarget } from "./needs_you";
import type { Chip, FreshnessSummary, NeedsUpdate } from "./prs_model";

/** What a source view needs from the app. */
export interface SourceViewDeps {
  /** Rust's judged actions of one item (`source_actions`). */
  actions(sourceId: string, view: string, key: string): Promise<ipc.SourceActionDto[]>;
  /** `source_run_action`; only ever called through `runAction`. */
  run: typeof ipc.sourceRunAction;
  /** Open a source's link after Rust's https + trusted-host check. */
  openLink(sourceId: string, url: string): Promise<void>;
  /** The back control or Escape. */
  onClose(): void;
  /** One line for the user: an action's outcome or a failure. */
  say(text: string, kind?: "info" | "error"): void;
  now(): number;
}

export const SOURCE_VIEW_CSS = `
.chappa-panel-host.chappa-source-view{display:none;}
.chappa-panel-host.chappa-source-view.active{display:flex;flex-direction:column;outline:none;background:#0d0e10;}
.chappa-sv-header{flex:none;display:flex;align-items:center;gap:14px;padding:10px 20px;border-bottom:1px solid #1c1f24;}
.chappa-sv-back{display:inline-flex;align-items:center;gap:8px;border:0;background:none;color:#8b949e;font:600 15px system-ui,sans-serif;cursor:pointer;padding:2px 0;}
.chappa-sv-back > span:last-child{color:#d6d8dc;}
.chappa-sv-back:hover{color:#d6d8dc;}
.chappa-sv-tabs{display:flex;gap:4px;}
.chappa-sv-tab{border:1px solid #262a31;border-radius:6px;background:#16181d;color:#8b949e;font:12px system-ui,sans-serif;cursor:pointer;padding:3px 10px;}
.chappa-sv-tab.active{background:#1d232b;color:#e6e8ec;border-color:#353a43;}
.chappa-sv-fresh{margin-left:auto;font:11px system-ui,sans-serif;color:#6b7280;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;}
.chappa-sv-body{flex:1 1 auto;min-height:0;overflow-y:auto;padding:10px 20px 48px;}
.chappa-sv-notices,.chappa-sv-errors,.chappa-sv-banners{display:flex;flex-direction:column;gap:2px;margin-bottom:8px;}
.chappa-sv-notices:empty,.chappa-sv-errors:empty,.chappa-sv-banners:empty{display:none;}
.chappa-sv-notice{font:11px system-ui,sans-serif;color:#6b7280;}
.chappa-sv-error{font:12px system-ui,sans-serif;color:#f0b429;}
.chappa-sv-banner{padding:6px 10px;border:1px solid #5c4813;border-radius:6px;background:#1d1708;color:#f0d68a;font:12px system-ui,sans-serif;}
.chappa-sv-empty{padding:24px 0;color:#6b7280;font:13px system-ui,sans-serif;}
.chappa-sv-empty:empty{display:none;}
.chappa-sv-group{margin-bottom:14px;}
.chappa-sv-group-head{display:flex;align-items:center;gap:8px;width:100%;margin:4px 0 6px;padding:0;border:0;background:none;text-align:left;font:600 12px system-ui,sans-serif;color:#adb3bd;text-transform:none;cursor:default;}
button.chappa-sv-group-head{cursor:pointer;}
button.chappa-sv-group-head:hover{color:#e6e8ec;}
.chappa-sv-count{font:400 11px ui-monospace,monospace;color:#6b7280;}
.chappa-sv-rows{display:flex;flex-direction:column;gap:6px;}
.chappa-sv-rows.folded{display:none;}
.chappa-sv-row{display:flex;flex-direction:column;gap:3px;padding:7px 10px;border:1px solid #22252a;border-radius:7px;background:#121417;min-width:0;}
.chappa-sv-row[data-tone="bad"]{border-left:3px solid #f85149;}
.chappa-sv-row[data-tone="warn"]{border-left:3px solid #f0b429;}
.chappa-sv-row[data-tone="info"]{border-left:3px solid #5ea6ff;}
.chappa-sv-top{display:flex;align-items:center;gap:8px;min-width:0;}
.chappa-sv-title{flex:0 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;border:0;background:none;padding:0;color:#e6e8ec;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-sv-title:hover{text-decoration:underline;}
.chappa-sv-title:disabled{cursor:default;text-decoration:none;}
.chappa-sv-meta,.chappa-sv-detail{font:11px ui-monospace,monospace;color:#8b949e;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-sv-meta:empty,.chappa-sv-detail:empty,.chappa-sv-reasons:empty,.chappa-sv-chips:empty,.chappa-sv-actions:empty{display:none;}
.chappa-sv-reasons{display:flex;flex-direction:column;font:12px system-ui,sans-serif;color:#c9d1d9;}
.chappa-sv-reason{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-sv-chips{display:flex;flex-wrap:wrap;gap:4px;}
.chappa-sv-chip{padding:0 6px;border:1px solid #2a2e35;border-radius:8px;font:10px ui-monospace,monospace;color:#adb3bd;}
.chappa-sv-chip[data-tone="ok"]{color:#3fb950;border-color:#1f4d2a;}
.chappa-sv-chip[data-tone="warn"]{color:#f0b429;border-color:#5c4813;}
.chappa-sv-chip[data-tone="bad"]{color:#f85149;border-color:#5c1f1c;}
.chappa-sv-chip[data-tone="info"]{color:#5ea6ff;border-color:#1f3a5c;}
.chappa-sv-chip[data-tone="muted"]{color:#6b7280;}
.chappa-sv-actions{display:flex;flex-wrap:wrap;gap:4px;}
.chappa-sv-action{border:1px solid #262a31;border-radius:4px;background:#16181d;color:#d6d8dc;font:11px system-ui,sans-serif;cursor:pointer;padding:1px 7px;}
.chappa-sv-action:hover:not(:disabled){background:#1d232b;}
.chappa-sv-action:disabled{color:#6b7280;cursor:default;}
`;

let stylesInjected = false;

export function injectSourceViewStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = SOURCE_VIEW_CSS;
  document.head.appendChild(el);
}

/** "as of 3 min ago · refreshing", or why there is no as-of yet. */
export function freshnessText(f: FreshnessSummary, now: number): string {
  const ms = f.asOf ? Date.parse(f.asOf) : NaN;
  const parts = [Number.isFinite(ms) ? `as of ${formatAgo(ms, now)}` : "no data yet"];
  if (f.refreshing) parts.push("refreshing");
  return parts.join(" · ");
}

/** The "needs update" line for a source speaking another contract version. */
export function needsUpdateText(s: NeedsUpdate): string {
  return `${s.sourceLabel} needs an update: it speaks a different version of the source contract, so nothing from it is shown.`;
}

/** Put `el` at child position `i` of `parent`, moving it only when it is
 *  not already there (an unmoved node keeps its focus and scroll). */
export function placeAt(parent: HTMLElement, el: HTMLElement, i: number): void {
  const at = parent.children[i];
  if (at !== el) parent.insertBefore(el, at ?? null);
}

/** Drop the children of `parent` from position `n` on. */
export function truncate(parent: HTMLElement, n: number): void {
  while (parent.children.length > n) parent.lastElementChild!.remove();
}

export function chipEl(chip: Chip): HTMLElement {
  const el = document.createElement("span");
  el.className = "chappa-sv-chip";
  el.dataset.tone = chip.tone;
  el.textContent = chip.text;
  if (chip.title) el.title = chip.title;
  return el;
}

/** Replace `parent`'s children with one line element per text. */
export function fillLines(parent: HTMLElement, cls: string, lines: string[]): void {
  parent.textContent = "";
  for (const line of lines) {
    const el = document.createElement("div");
    el.className = cls;
    el.textContent = line;
    el.title = line;
    parent.appendChild(el);
  }
}

/** The page frame both views use: back control with the view's name, a
 *  header slot, the freshness line, then banners / errors / notices above
 *  the content and its empty state. */
export class ViewFrame {
  readonly host: HTMLDivElement;
  readonly headerSlot: HTMLElement;
  readonly fresh: HTMLElement;
  readonly banners: HTMLElement;
  readonly errors: HTMLElement;
  readonly notices: HTMLElement;
  readonly content: HTMLElement;
  readonly empty: HTMLElement;
  private sig = "";

  constructor(name: string, view: string, private readonly onClose: () => void) {
    injectSourceViewStyles();
    this.host = document.createElement("div");
    this.host.className = "chappa-panel-host chappa-source-view";
    this.host.dataset.view = view;
    this.host.tabIndex = -1;
    this.host.setAttribute("aria-label", name);
    this.host.addEventListener("keydown", this.onKeydown);
    const header = document.createElement("div");
    header.className = "chappa-sv-header";
    const back = document.createElement("button");
    back.type = "button";
    back.className = "chappa-sv-back";
    back.title = "Back (Esc)";
    const arrow = document.createElement("span");
    arrow.setAttribute("aria-hidden", "true");
    arrow.textContent = "←";
    const title = document.createElement("span");
    title.textContent = name;
    back.append(arrow, title);
    back.addEventListener("click", () => this.onClose());
    this.headerSlot = document.createElement("div");
    this.headerSlot.className = "chappa-sv-header-slot";
    this.fresh = document.createElement("span");
    this.fresh.className = "chappa-sv-fresh";
    header.append(back, this.headerSlot, this.fresh);
    const body = document.createElement("div");
    body.className = "chappa-sv-body";
    this.banners = document.createElement("div");
    this.banners.className = "chappa-sv-banners";
    this.errors = document.createElement("div");
    this.errors.className = "chappa-sv-errors";
    this.notices = document.createElement("div");
    this.notices.className = "chappa-sv-notices";
    this.content = document.createElement("div");
    this.content.className = "chappa-sv-content";
    this.empty = document.createElement("div");
    this.empty.className = "chappa-sv-empty";
    body.append(this.banners, this.errors, this.notices, this.content, this.empty);
    this.host.append(header, body);
  }

  /** The freshness line, its errors and notices, and the needs-update
   *  banners; untouched when nothing changed. */
  paintStatus(f: FreshnessSummary, needsUpdate: NeedsUpdate[], now: number): void {
    const text = freshnessText(f, now);
    const sig = JSON.stringify([text, f.errors, f.notices, needsUpdate]);
    if (sig === this.sig) return;
    this.sig = sig;
    this.fresh.textContent = text;
    this.fresh.title = f.asOf ? new Date(f.asOf).toLocaleString() : "";
    fillLines(this.banners, "chappa-sv-banner", needsUpdate.map(needsUpdateText));
    fillLines(this.errors, "chappa-sv-error", f.errors);
    fillLines(this.notices, "chappa-sv-notice", f.notices);
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
    if (e.key !== "Escape" || e.isComposing) return;
    e.preventDefault();
    this.onClose();
  };
}

/** The action buttons of one row, filled from Rust's verdicts. The row
 *  calls {@link ActionsCell.load} with its targets and a version; the same
 *  pair twice costs nothing, and a slow answer never paints over a newer
 *  load. */
export class ActionsCell {
  readonly el: HTMLElement;
  private sig = "\u0000never";
  private seq = 0;

  constructor(private readonly deps: SourceViewDeps) {
    this.el = document.createElement("div");
    this.el.className = "chappa-sv-actions";
  }

  load(targets: ActionTarget[], version: string): void {
    const sig = `${version}\u001e${JSON.stringify(targets)}`;
    if (sig === this.sig) return;
    this.sig = sig;
    const seq = ++this.seq;
    if (targets.length === 0) {
      this.el.textContent = "";
      return;
    }
    void Promise.all(
      targets.map((t) =>
        this.deps
          .actions(t.sourceId, t.view, t.key)
          .then((entries) => entries.map((entry) => ({ t, entry })))
          .catch(() => []),
      ),
    ).then((lists) => {
      if (seq !== this.seq) return;
      this.el.textContent = "";
      const links = new Set<string>();
      const many = targets.length > 1;
      for (const { t, entry } of lists.flat()) {
        // Two sources serving the same PR both offer "open it": one button.
        if (entry.action.kind === "link") {
          if (links.has(entry.action.url)) continue;
          links.add(entry.action.url);
        }
        const state = actionButtonState(entry);
        if (state.hidden) continue;
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "chappa-sv-action";
        btn.dataset.action = entry.action.id;
        btn.dataset.source = t.sourceId;
        btn.textContent = state.asksFirst ? `${state.label}…` : state.label;
        btn.title = many ? `${state.title} (${t.sourceLabel})` : state.title;
        btn.disabled = state.disabled;
        btn.addEventListener("mousedown", (e) => e.preventDefault());
        btn.addEventListener("click", () => void this.click(t, entry));
        this.el.appendChild(btn);
      }
    });
  }

  private async click(t: ActionTarget, entry: ipc.SourceActionDto): Promise<void> {
    try {
      const outcome = await runAction(t.sourceId, t.sourceLabel, t.view, t.key, entry, { run: this.deps.run });
      if (!outcome) return;
      const text = outcomeText(outcome);
      if (text) this.deps.say(text, outcome.kind === "uncertain" ? "error" : "info");
    } catch (e) {
      this.deps.say(`${entry.action.label}: ${e instanceof Error ? e.message : String(e)}`, "error");
    }
  }
}

/** Open a source's link; a refusal (host not trusted, not https) is said. */
export function openChecked(deps: SourceViewDeps, sourceId: string, url: string): void {
  void deps.openLink(sourceId, url).catch((e) => deps.say(e instanceof Error ? e.message : String(e), "error"));
}
