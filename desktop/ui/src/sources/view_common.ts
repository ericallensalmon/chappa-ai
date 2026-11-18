// What the source views share: the page frame (a main-area page whose
// header starts with the shared back arrow, like the dashboard), the pages
// drilled into from it, the freshness line, chips, keyed-row placement and
// the action buttons.
//
// Action buttons never decide anything themselves: Rust judges every action
// (`source_actions`) and judges it again when it runs; a button shows that
// judgement through `actionButtonState` and runs through `runAction`, which
// asks whatever must be asked first.

import type * as ipc from "../ipc";
import { isBackKey, mountPageHeader, type PageHeader } from "../page_header";
import { formatAgo } from "../record_pane";
import { fieldsDialog } from "../dialog";
import { readCollapsed, writeCollapsed } from "../sidebar_sections";
import { actionButtonState, outcomeText, runAction, type ActionButtonState } from "../source_actions";
import type { ActionTarget } from "./needs_you";
import type { Chip, FreshnessSummary, NeedsUpdate } from "./prs_model";

/** What a source view needs from the app. */
export interface SourceViewDeps {
  /** Rust's judged actions of one item (`source_actions`); `key` null =
   *  the view's (or page's) own, which is where its banners' actions are.
   *  With `owner`: the item actions `sourceId` offers on the row `key` of
   *  that other source. */
  actions(sourceId: string, view: string, key: string | null, owner?: string): Promise<ipc.SourceActionDto[]>;
  /** `source_run_action`; only ever called through `runAction`. */
  run: typeof ipc.sourceRunAction;
  /** Open a source's link after Rust's trusted-host check (https, or plain
   *  http to this machine). */
  openLink(sourceId: string, url: string): Promise<void>;
  /** Fetch a panel's page again (`source_panel`), to re-poll it while it
   *  is open. Absent where no page can be shown. */
  panel?(sourceId: string, path: string): Promise<unknown>;
  /** A `panel` action ran and answered its first page: show it. Absent
   *  where the view has nowhere to show one. */
  openPanel?(origin: PanelOrigin, payload: unknown): void;
  /** The view's back arrow or Escape, with no page drilled into it left. */
  onClose(): void;
  /** Open Settings where sources are added and trusted (the empty state's
   *  link when no trusted source serves the view). Absent: no link. */
  openSourcesSettings?(): void;
  /** One line for the user: an action's outcome or a failure. */
  say(text: string, kind?: "info" | "error"): void;
  now(): number;
}

export const SOURCE_VIEW_CSS = `
.chappa-panel-host.chappa-source-view{display:none;}
.chappa-panel-host.chappa-source-view.active{display:flex;flex-direction:column;outline:none;background:#0d0e10;}
.chappa-sv-header{flex:none;display:flex;align-items:center;gap:10px;padding:8px 20px 8px 14px;border-bottom:1px solid #1c1f24;min-width:0;}
.chappa-sv-header > .chappa-sv-header-slot{margin-left:4px;}
.chappa-source-view.covered > .chappa-sv-header,.chappa-source-view.covered > .chappa-sv-body{display:none;}
.chappa-sv-subpage{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;}
.chappa-sv-subpage.under{display:none;}
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
.chappa-sv-empty-link{padding:0;border:0;background:none;color:#5ea6ff;font:inherit;cursor:pointer;text-decoration:underline;}
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
.chappa-dt{min-width:0;}
.chappa-dt-cols,.chappa-dt-row{display:grid;grid-template-columns:var(--dt-cols);column-gap:12px;}
.chappa-dt-cols{position:sticky;top:-10px;z-index:2;background:#0d0e10;padding:6px 8px;border-bottom:1px solid #22252a;font:600 10px ui-monospace,monospace;letter-spacing:.06em;color:#8b949e;}
.chappa-dt-group{margin-top:4px;}
.chappa-dt-group-head{display:flex;align-items:baseline;gap:6px;width:100%;margin:0;padding:8px 8px 4px;border:0;border-bottom:1px solid #22252a;background:none;text-align:left;font:600 12px system-ui,sans-serif;color:#adb3bd;cursor:pointer;}
.chappa-dt-group-head:hover{color:#e6e8ec;}
div.chappa-dt-group-head{cursor:default;}
.chappa-dt-group-count{font:400 11px ui-monospace,monospace;color:#6b7280;}
.chappa-dt-rows.folded{display:none;}
.chappa-dt-empty{padding:6px 8px;font:12px system-ui,sans-serif;color:#6b7280;}
.chappa-dt-empty:empty{display:none;}
.chappa-dt-row{position:relative;align-items:start;padding:7px 8px;border-bottom:1px solid #1a1d22;cursor:pointer;}
.chappa-dt-row:hover{background:#14171c;}
.chappa-dt-row.selected{background:#161a20;}
.chappa-dt-row.focused{outline:1px solid #5ea6ff;outline-offset:-1px;}
.chappa-dt-row[data-accent="active"]{box-shadow:inset 3px 0 0 #3fb950;}
.chappa-dt-row[data-accent="bad"]{box-shadow:inset 3px 0 0 #f85149;}
.chappa-dt-row[data-accent="warn"]{box-shadow:inset 3px 0 0 #f0b429;}
.chappa-dt-row[data-accent="info"]{box-shadow:inset 3px 0 0 #5ea6ff;}
.chappa-dt-cell{min-width:0;display:flex;flex-direction:column;align-items:flex-start;gap:2px;overflow-wrap:anywhere;font:12px system-ui,sans-serif;color:#d6d8dc;}
.chappa-dt-chips{display:flex;flex-wrap:wrap;gap:3px;}
.chappa-dt-chips:empty{display:none;}
.chappa-dt-main{display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden;border:0;background:none;padding:0;color:#e6e8ec;font:600 13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-dt-main:hover{text-decoration:underline;}
.chappa-dt-sub{font:11px system-ui,sans-serif;color:#8b949e;}
.chappa-dt-sub:empty{display:none;}
.chappa-dt-mono{font:11px ui-monospace,monospace;color:#adb3bd;}
.chappa-dt-num{font:600 12px ui-monospace,monospace;color:#5ea6ff;}
.chappa-dt-acts{flex-direction:row;align-items:center;justify-content:flex-end;gap:4px;}
.chappa-dt-open{visibility:hidden;}
.chappa-dt-row:hover .chappa-dt-open,.chappa-dt-row:focus-within .chappa-dt-open{visibility:visible;}
.chappa-dt-more{border:1px solid transparent;border-radius:4px;background:none;color:#8b949e;font:14px system-ui,sans-serif;line-height:1;padding:1px 5px;cursor:pointer;}
.chappa-dt-more:hover,.chappa-dt-more:focus-visible,.chappa-dt-more[aria-expanded="true"]{border-color:#2a2e35;color:#e6e8ec;}
.chappa-dt-menu{position:fixed;z-index:50;min-width:170px;max-width:320px;padding:6px;background:#16181d;border:1px solid #2a2e35;border-radius:6px;box-shadow:0 6px 18px rgba(0,0,0,.35);display:flex;flex-direction:column;gap:4px;}
.chappa-dt-menu .chappa-sv-actions{flex-direction:column;align-items:stretch;}
.chappa-dt-menu .chappa-sv-action{text-align:left;}
.chappa-dt-page-grid{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;margin:0 0 10px;font:12px system-ui,sans-serif;color:#d6d8dc;}
.chappa-dt-page-grid dt{color:#8b949e;}
.chappa-dt-page-grid dd{margin:0;min-width:0;overflow-wrap:anywhere;display:flex;flex-wrap:wrap;gap:4px;align-items:center;}
.chappa-dt-page-gone{font:12px system-ui,sans-serif;color:#f0b429;}
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

/** The empty state: its text and, when given, a link to Settings ▸ Sources
 *  (where a source for the view is added or trusted). Untouched when
 *  nothing changed, so a focused link keeps its focus across repaints. */
export function paintEmpty(el: HTMLElement, text: string, openSettings?: () => void): void {
  const sig = `${openSettings ? "1" : "0"}:${text}`;
  if (el.dataset.sig === sig) return;
  el.dataset.sig = sig;
  el.textContent = text;
  if (!text || !openSettings) return;
  const link = document.createElement("button");
  link.type = "button";
  link.className = "chappa-sv-empty-link";
  link.textContent = "Open Settings ▸ Sources";
  link.addEventListener("click", () => openSettings());
  el.append(" ", link);
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

/** Where a panel page came from: the source, and the `panel` action. */
export interface PanelOrigin {
  sourceId: string;
  sourceLabel: string;
  /** The action's label (the page's title until the page names its own). */
  label: string;
  /** The payload-named path the page is re-fetched from. */
  endpoint: string;
}

/** A page drilled into from a view (a source's panel page): it covers the
 *  view, header and all, and its own header's arrow steps back one page. */
export interface Subpage {
  readonly el: HTMLElement;
  /** What its header says (the page above it returns "to" this). */
  readonly title: string;
  setBackTo(where: string): void;
  dispose(): void;
  /** The source the page belongs to: a page whose source left is stale. */
  readonly sourceId?: string;
  /** What the page shows is no longer reported. */
  gone?(): boolean;
}

/** The page frame every view uses: the back arrow and the view's name, a
 *  header slot, the freshness line, then banners / errors / notices above
 *  the content and its empty state; and the stack of pages drilled into
 *  from it, the top one covering the view. */
export class ViewFrame {
  readonly host: HTMLDivElement;
  readonly header: PageHeader;
  readonly headerSlot: HTMLElement;
  readonly fresh: HTMLElement;
  readonly banners: HTMLElement;
  readonly errors: HTMLElement;
  readonly notices: HTMLElement;
  readonly content: HTMLElement;
  readonly empty: HTMLElement;
  /** Above everything else in the body: the app's source selector. */
  readonly picker: HTMLElement;
  private sig = "";
  private readonly pages: Subpage[] = [];
  /** A level inside the view itself (a work item's detail) that Escape
   *  closes before the view; answers whether it closed one. */
  private innerBack: (() => boolean) | null = null;

  constructor(readonly name: string, view: string, private readonly onClose: () => void) {
    injectSourceViewStyles();
    this.host = document.createElement("div");
    this.host.className = "chappa-panel-host chappa-source-view";
    this.host.dataset.view = view;
    this.host.tabIndex = -1;
    this.host.setAttribute("aria-label", name);
    this.host.addEventListener("keydown", this.onKeydown);
    const header = document.createElement("div");
    header.className = "chappa-sv-header";
    this.headerSlot = document.createElement("div");
    this.headerSlot.className = "chappa-sv-header-slot";
    this.fresh = document.createElement("span");
    this.fresh.className = "chappa-sv-fresh";
    header.append(this.headerSlot, this.fresh);
    this.header = mountPageHeader(header, { title: name, backTo: "the workspace", onBack: () => this.onClose() });
    const body = document.createElement("div");
    body.className = "chappa-sv-body";
    this.picker = document.createElement("div");
    this.picker.className = "chappa-sv-picker";
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
    body.append(this.picker, this.banners, this.errors, this.notices, this.content, this.empty);
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

  /** A view leaving the screen keeps the pages drilled into from it: its
   *  tab comes back where it was left. */
  setActive(active: boolean): void {
    this.host.classList.toggle("active", active);
    if (active) this.host.focus({ preventScroll: true });
  }

  get active(): boolean {
    return this.host.classList.contains("active");
  }

  /** Where the view's arrow returns (what it covered). */
  setBackTo(where: string): void {
    this.header.setBackTo(where);
  }

  setInnerBack(fn: (() => boolean) | null): void {
    this.innerBack = fn;
  }

  /** The pages drilled into, the top one last. */
  get subpages(): readonly Subpage[] {
    return this.pages;
  }

  /** Cover the view (or the page on top) with `page`; its arrow returns to
   *  what it covers. */
  pushSubpage(page: Subpage): void {
    const below = this.pages[this.pages.length - 1];
    below?.el.classList.add("under");
    page.setBackTo(below ? below.title : this.name);
    page.el.classList.add("chappa-sv-subpage");
    this.pages.push(page);
    this.host.classList.add("covered");
    this.host.appendChild(page.el);
    this.host.focus({ preventScroll: true });
  }

  /** Step back one page. Answers whether there was one. */
  popSubpage(): boolean {
    const page = this.pages.pop();
    if (!page) return false;
    page.dispose();
    const top = this.pages[this.pages.length - 1];
    top?.el.classList.remove("under");
    this.host.classList.toggle("covered", top !== undefined);
    this.host.focus({ preventScroll: true });
    return true;
  }

  clearSubpages(): void {
    while (this.pages.length) this.popSubpage();
  }

  /** Coming back to the view: when any page drilled into shows something
   *  that is gone (its item, or its source), fall back to the view's top. */
  dropStale(sourceLive: (sourceId: string) => boolean): void {
    if (this.pages.some((p) => p.gone?.() || (p.sourceId !== undefined && !sourceLive(p.sourceId)))) this.clearSubpages();
  }

  /** One step back: the top drilled-in page, else the view's own inner
   *  level, else the view itself. Escape lands here. */
  back(): void {
    if (this.popSubpage()) return;
    if (this.innerBack?.()) return;
    this.onClose();
  }

  dispose(): void {
    this.clearSubpages();
    this.host.removeEventListener("keydown", this.onKeydown);
    this.host.remove();
  }

  private onKeydown = (e: KeyboardEvent): void => {
    if (!isBackKey(e)) return;
    e.preventDefault();
    this.back();
  };
}

/** One action as a row draws it: whose it is, Rust's verdict, the button. */
interface Offer {
  t: ActionTarget;
  entry: ipc.SourceActionDto;
  state: ActionButtonState;
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

  /** `only`: draw just these action ids (a section's own actions out of
   *  its item's list), in the order Rust lists them; a target's own `only`
   *  takes its place for that target (rows fed by several sources, whose
   *  ids are each source's own). */
  load(targets: (ActionTarget & { only?: readonly string[] })[], version: string, only?: readonly string[]): void {
    const sig = `${version}\u001e${JSON.stringify(targets)}\u001e${only ? JSON.stringify(only) : ""}`;
    if (sig === this.sig) return;
    this.sig = sig;
    const seq = ++this.seq;
    if (targets.length === 0) {
      this.el.textContent = "";
      return;
    }
    void Promise.all(
      targets.map((t) =>
        (t.owner ? this.deps.actions(t.sourceId, t.view, t.key, t.owner) : this.deps.actions(t.sourceId, t.view, t.key))
          .then((entries) => entries.map((entry) => ({ t, entry })))
          .catch(() => []),
      ),
    ).then((lists) => {
      if (seq !== this.seq) return;
      this.el.textContent = "";
      const links = new Set<string>();
      const ids = new Set<string>();
      const many = targets.length > 1;
      // An action several sources offer on this row (each stack its own
      // "Review with environment") is one button; which source runs it is
      // asked when it is clicked.
      const offers = new Map<string, Offer[]>();
      const order: (Offer | Offer[])[] = [];
      for (const { t, entry } of lists.flat()) {
        const allowed = t.only ?? only;
        if (allowed && !allowed.includes(entry.action.id)) continue;
        // One target can carry an id twice (a page's own "Re-run" and its
        // banner's); Rust runs the first, so one button.
        const id = `${t.sourceId}\u001f${t.owner ?? ""}\u001f${t.key ?? ""}\u001f${entry.action.id}`;
        if (ids.has(id)) continue;
        ids.add(id);
        // Two sources serving the same PR both offer "open it": one button.
        if (entry.action.kind === "link") {
          if (links.has(entry.action.url)) continue;
          links.add(entry.action.url);
        }
        const state = actionButtonState(entry);
        if (state.hidden) continue;
        if (!t.owner) {
          order.push({ t, entry, state });
          continue;
        }
        const k = `${t.owner}\u001f${entry.action.id}`;
        const group = offers.get(k);
        if (group) group.push({ t, entry, state });
        else {
          const fresh = [{ t, entry, state }];
          offers.set(k, fresh);
          order.push(fresh);
        }
      }
      for (const item of order) {
        if (Array.isArray(item) && item.length > 1) {
          this.el.appendChild(this.offerButton(item));
          continue;
        }
        const { t, entry, state } = Array.isArray(item) ? item[0] : item;
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "chappa-sv-action";
        btn.dataset.action = entry.action.id;
        btn.dataset.source = t.sourceId;
        if (t.owner) btn.dataset.owner = t.owner;
        btn.textContent = state.asksFirst ? `${state.label}…` : state.label;
        btn.title = many ? `${state.title} (${t.sourceLabel})` : state.title;
        btn.disabled = state.disabled;
        btn.addEventListener("mousedown", (e) => e.preventDefault());
        btn.addEventListener("click", () => void this.click(t, entry, btn));
        this.el.appendChild(btn);
      }
    });
  }

  /** One button for the same action offered by several sources: a click
   *  asks which source runs it (only those whose verdict allows it), then
   *  runs that one as usual, with its own confirm. */
  private offerButton(variants: Offer[]): HTMLButtonElement {
    const first = variants[0];
    const live = variants.filter((v) => !v.state.disabled);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chappa-sv-action";
    btn.dataset.action = first.entry.action.id;
    btn.dataset.offers = variants.map((v) => v.t.sourceId).join(" ");
    if (first.t.owner) btn.dataset.owner = first.t.owner;
    btn.textContent = `${first.state.label}…`;
    btn.title = live.length
      ? `Offered by ${variants.map((v) => v.t.sourceLabel).join(", ")}; asks which one`
      : variants.map((v) => `${v.t.sourceLabel}: ${v.state.title}`).join("\n");
    btn.disabled = live.length === 0;
    btn.addEventListener("mousedown", (e) => e.preventDefault());
    btn.addEventListener("click", () => void this.pickOffer(first.state.label, live));
    return btn;
  }

  private async pickOffer(label: string, live: Offer[]): Promise<void> {
    if (live.length === 0) return;
    let chosen = live[0];
    if (live.length > 1) {
      const answer = await fieldsDialog(
        `${label}: which one?`,
        [{ name: "via", label: "With", kind: "choice", required: true, value: "0", options: live.map((v, i) => ({ value: String(i), label: v.t.sourceLabel })) }],
        { okLabel: "Next" },
      );
      const at = answer ? Number(answer.values.via) : NaN;
      if (!Number.isInteger(at) || !live[at]) return;
      chosen = live[at];
    }
    await this.click(chosen.t, chosen.entry);
  }

  /** One call at a time per button: it is disabled from the click until
   *  the call returns (a second click would only be refused as busy). */
  private async click(t: ActionTarget, entry: ipc.SourceActionDto, btn?: HTMLButtonElement): Promise<void> {
    if (btn?.disabled) return;
    if (btn) btn.disabled = true;
    try {
      const outcome = await runAction(t.sourceId, t.sourceLabel, t.view, t.key, entry, { run: this.deps.run }, t.owner ? { owner: t.owner } : {});
      if (!outcome) return;
      if (outcome.kind === "panel" && entry.action.kind === "panel") {
        this.deps.openPanel?.({ sourceId: t.sourceId, sourceLabel: t.sourceLabel, label: entry.action.label, endpoint: entry.action.endpoint }, outcome.payload);
        return;
      }
      const text = outcomeText(outcome);
      if (text) this.deps.say(text, outcome.kind === "uncertain" ? "error" : "info");
    } catch (e) {
      this.deps.say(`${entry.action.label}: ${e instanceof Error ? e.message : String(e)}`, "error");
    } finally {
      if (btn) btn.disabled = actionButtonState(entry).disabled;
    }
  }
}

/** Open a source's link; a refusal (host not trusted, plain http to
 *  another machine) is said. */
export function openChecked(deps: SourceViewDeps, sourceId: string, url: string): void {
  void deps.openLink(sourceId, url).catch((e) => deps.say(e instanceof Error ? e.message : String(e), "error"));
}

/** A group's fold, remembered in the same store the Environment view and
 *  the sidebar use. Only a choice away from the default is stored, so a
 *  group that starts folded (Done, Waiting on reviewers, a team) and one
 *  that starts open each leave no key until the user turns them. */
export function readFold(key: string, byDefault: boolean): boolean {
  return byDefault ? !readCollapsed(`${key}:open`) : readCollapsed(key);
}

export function writeFold(key: string, byDefault: boolean, folded: boolean): void {
  if (byDefault) writeCollapsed(`${key}:open`, !folded);
  else writeCollapsed(key, folded);
}

/** The table's column header row. */
export function columnsHead(labels: readonly string[]): HTMLElement {
  const head = document.createElement("div");
  head.className = "chappa-dt-cols";
  head.setAttribute("role", "row");
  for (const l of labels) {
    const c = document.createElement("div");
    c.setAttribute("role", "columnheader");
    c.textContent = l;
    head.appendChild(c);
  }
  return head;
}

/** One entry of a row's menu drawn by the app (not a source's action). */
export interface MenuEntry {
  label: string;
  run(): void;
  title?: string;
}

type MenuTarget = ActionTarget & { only?: readonly string[] };

/**
 * The one "⋯" menu of a table, moved to whichever row opened it: the app's
 * own entries (open the row's page, open its link), then the row's actions
 * as Rust judged them. The actions are only asked for when the menu opens,
 * so a list of fifty rows asks nothing until one is reached. Escape closes
 * it back to its button without leaving the view; a click outside closes it.
 */
export class RowMenu {
  private cur: { el: HTMLElement; cell: ActionsCell; key: string; opener: HTMLButtonElement } | null = null;

  constructor(
    private readonly host: HTMLElement,
    private readonly deps: SourceViewDeps,
  ) {
    host.addEventListener("keydown", this.onKey, true);
    document.addEventListener("mousedown", this.onOutside, true);
  }

  /** The row whose menu is open. */
  get key(): string | null {
    return this.cur?.key ?? null;
  }

  get el(): HTMLElement | null {
    return this.cur?.el ?? null;
  }

  /** A row's "⋯" button; `onClick` gets the button. */
  static button(onClick: (btn: HTMLButtonElement) => void): HTMLButtonElement {
    const more = document.createElement("button");
    more.type = "button";
    more.className = "chappa-dt-more";
    more.textContent = "⋯";
    more.title = "Actions";
    more.setAttribute("aria-label", "Actions");
    more.setAttribute("aria-haspopup", "menu");
    more.setAttribute("aria-expanded", "false");
    more.addEventListener("click", (e) => {
      e.stopPropagation();
      onClick(more);
    });
    return more;
  }

  toggle(key: string, opener: HTMLButtonElement, entries: MenuEntry[], targets: MenuTarget[], version: string, only?: readonly string[]): void {
    if (this.cur?.key === key) {
      this.close();
      return;
    }
    this.close();
    const menu = document.createElement("div");
    menu.className = "chappa-dt-menu";
    menu.setAttribute("role", "menu");
    menu.dataset.key = key;
    const own = document.createElement("div");
    own.className = "chappa-sv-actions chappa-dt-menu-own";
    for (const e of entries) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "chappa-sv-action chappa-dt-menu-entry";
      b.textContent = e.label;
      if (e.title) b.title = e.title;
      b.addEventListener("click", () => {
        this.close();
        e.run();
      });
      own.appendChild(b);
    }
    const cell = new ActionsCell(this.deps);
    // An action chosen from the menu closes it (its button ran first).
    cell.el.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest("button:not(:disabled)")) this.close();
    });
    menu.append(own, cell.el);
    menu.addEventListener("click", (e) => e.stopPropagation());
    const box = opener.getBoundingClientRect();
    menu.style.top = `${Math.round(box.bottom + 2)}px`;
    menu.style.right = `${Math.max(8, Math.round((window.innerWidth || 0) - box.right))}px`;
    this.host.appendChild(menu);
    this.cur = { el: menu, cell, key, opener };
    opener.setAttribute("aria-expanded", "true");
    cell.load(targets, version, only);
    (own.firstElementChild as HTMLElement | null)?.focus();
  }

  /** Keep an open menu's actions current with a poll. */
  refresh(key: string, targets: MenuTarget[], version: string, only?: readonly string[]): void {
    if (this.cur?.key === key) this.cur.cell.load(targets, version, only);
  }

  close(): void {
    if (!this.cur) return;
    this.cur.opener.setAttribute("aria-expanded", "false");
    this.cur.el.remove();
    this.cur = null;
  }

  dispose(): void {
    this.close();
    this.host.removeEventListener("keydown", this.onKey, true);
    document.removeEventListener("mousedown", this.onOutside, true);
  }

  private onKey = (e: KeyboardEvent): void => {
    if (!this.cur || !isBackKey(e)) return;
    e.preventDefault();
    e.stopPropagation();
    const opener = this.cur.opener;
    this.close();
    opener.focus();
  };

  private onOutside = (e: MouseEvent): void => {
    if (!this.cur) return;
    const t = e.target as Node;
    if (this.cur.el.contains(t) || this.cur.opener.contains(t)) return;
    this.close();
  };
}

/** A row's inline "Open" (shown on hover and focus): its link. */
export function openButton(title: string, run: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "chappa-sv-action chappa-dt-open";
  b.textContent = "Open";
  b.title = title;
  b.addEventListener("mousedown", (e) => e.preventDefault());
  b.addEventListener("click", (e) => {
    e.stopPropagation();
    run();
  });
  return b;
}

/** Whether a click on a row opens its page: anywhere but on one of its
 *  own controls (a chip, a link, a button other than its title). */
export function rowClickOpens(e: MouseEvent, title: HTMLElement): boolean {
  const control = (e.target as HTMLElement).closest("button,a,input");
  return !control || control === title;
}

/** A compact group header: a fold toggle with the label and its count
 *  (or, unfoldable, a plain heading). */
export function groupHead(foldable: boolean, onToggle: () => void): { el: HTMLElement; paint(label: string, count: number, folded: boolean): void } {
  const el = document.createElement(foldable ? "button" : "div");
  el.className = "chappa-dt-group-head";
  if (el instanceof HTMLButtonElement) {
    el.type = "button";
    el.addEventListener("click", onToggle);
  }
  const label = document.createElement("span");
  label.className = "chappa-dt-group-label";
  const count = document.createElement("span");
  count.className = "chappa-dt-group-count";
  el.append(label, count);
  return {
    el,
    paint(text, n, folded) {
      label.textContent = foldable ? `${folded ? "▸" : "▾"} ${text}` : text;
      count.textContent = String(n);
      if (foldable) el.setAttribute("aria-expanded", String(!folded));
    },
  };
}

/** A compact age for a table cell ("3 h", "2 d", "now"); null when the
 *  timestamp cannot be read. */
export function shortAge(utc: string | null | undefined, now: number): string | null {
  const ms = utc ? Date.parse(utc) : NaN;
  if (!Number.isFinite(ms)) return null;
  const text = formatAgo(ms, now);
  return text === "just now" ? "now" : text.replace(/ ago$/, "");
}
