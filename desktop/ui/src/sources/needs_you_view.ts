// The Needs you page: everything that waits on the person, across
// terminals, agents and sources, one section per kind (source attention
// under each item's own heading). Work items, terminals and pull requests
// keep their section while their view is in play, with a line saying when
// nothing waits there. Each section's header is one compact line whose
// fold is remembered.
//
// A row is one line of an aligned list: the item's number or key, its title
// over a muted line (repo or project, and what the badge does not already
// say), and at its end the reason as one small badge. Clicking it opens
// what it is about (the terminal, the item in its view, the link); its
// actions sit in a "⋯" menu, asked for only when the menu opens.
//
// Built like the dashboard: repainted from a plain model, rows keyed by id,
// only changed rows touched. A `panel` action opens the source's drill-in
// page over the view; a row that opens another view drills into it (its
// arrow comes back here).

import type { NeedsYouModel, NeedsYouRow, NeedsYouSection, Opens } from "./needs_you";
import { withPanels } from "./panel_view";
import {
  RowMenu,
  ViewFrame,
  chipEl,
  groupHead,
  openChecked,
  placeAt,
  readFold,
  rowClickOpens,
  truncate,
  writeFold,
  type MenuEntry,
  type SourceViewDeps,
} from "./view_common";

export interface NeedsYouViewDeps extends SourceViewDeps {
  /** Front the terminal / agent / command row behind a terminal key. */
  selectTerminal(key: string): void;
  /** Open another view at an item of `sourceId` (when the row has one);
   *  false when that view is not available. */
  openView(view: string, key: string, sourceId?: string): boolean;
}

export interface NeedsYouViewInput {
  model: NeedsYouModel;
  /** Bumped per source whenever its payloads were re-read. */
  versions: Record<string, number>;
}

interface SectionRefs {
  el: HTMLElement;
  head: ReturnType<typeof groupHead>;
  body: HTMLElement;
  rows: HTMLElement;
  empty: HTMLElement;
  /** The folded rows' toggle and list (PRs waiting on reviewers). */
  foldHead: HTMLButtonElement;
  foldRows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  lead: HTMLElement;
  title: HTMLButtonElement;
  detail: HTMLElement;
  badge: HTMLElement;
  more: HTMLButtonElement;
  sig: string;
  row: NeedsYouRow | null;
}

/** The empty state: nothing waits, and what would show up here. */
export const NEEDS_YOU_EMPTY =
  "Nothing needs you. Agents waiting for input, programs that report they are blocked, terminals that rang, " +
  "approvals, pull requests on your plate and alerts from your sources show up here.";

/** Where a section's fold (and its "Waiting on others" fold) is remembered. */
export const needsFoldKey = (sectionId: string): string => `needs-group:${sectionId}`;

const CSS = `
.chappa-source-view[data-view="attention"] .chappa-dt{--dt-cols:72px minmax(0,1fr) auto 28px;}
.chappa-needs-section .chappa-dt-group-label{text-transform:uppercase;letter-spacing:.05em;font-size:11px;}
.chappa-needs-row .chappa-dt-main{-webkit-line-clamp:2;}
.chappa-needs-badge{flex-direction:row;justify-content:flex-end;}
.chappa-needs-more[hidden]{display:inline-block;visibility:hidden;}
.chappa-needs-row[data-tone="bad"] .chappa-dt-num{color:#f85149;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = CSS;
  document.head.appendChild(el);
}

/** What a row's title does on click, as a tooltip. */
export function opensTitle(row: NeedsYouRow): string {
  if (row.kind === "terminal") return `Go to ${row.title}`;
  const o = row.opens;
  if (!o) return row.title;
  if ("url" in o) return `Open ${o.url}`;
  return `Show ${o.key} in ${o.view}`;
}

const div = (cls: string): HTMLElement => {
  const e = document.createElement("div");
  e.className = cls;
  return e;
};

export class NeedsYouView {
  private readonly frame: ViewFrame;
  private readonly sections = new Map<string, SectionRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private readonly list: HTMLElement;
  private readonly menu: RowMenu;
  private input: NeedsYouViewInput | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: NeedsYouViewDeps;

  constructor(private readonly deps: NeedsYouViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("Needs you", "attention", () => deps.onClose());
    this.ui = withPanels(() => this.frame, deps);
    this.menu = new RowMenu(this.frame.host, this.ui);
    this.list = div("chappa-dt");
    this.list.setAttribute("role", "table");
    this.frame.content.appendChild(this.list);
  }

  get host(): HTMLElement {
    return this.frame.host;
  }

  /** Where the app puts its source selector. */
  get pickerSlot(): HTMLElement {
    return this.frame.picker;
  }

  /** Back on screen: pages drilled into that show something gone (or a
   *  source that left) give way to the view's top. */
  dropStale(sourceLive: (sourceId: string) => boolean): void {
    this.frame.dropStale(sourceLive);
  }

  /** Forget where the view was left: its top shows next time. */
  resetPlace(): void {
    this.frame.clearSubpages();
  }

  get active(): boolean {
    return this.frame.active;
  }

  setActive(active: boolean): void {
    if (!active) this.menu.close();
    this.frame.setActive(active);
  }

  /** Where the view's arrow returns. */
  setBackTo(where: string): void {
    this.frame.setBackTo(where);
  }

  update(input: NeedsYouViewInput): void {
    this.input = input;
    this.paint();
  }

  dispose(): void {
    this.menu.dispose();
    this.frame.dispose();
  }

  private paint(): void {
    const input = this.input;
    if (!input) return;
    const { model } = input;
    this.frame.paintStatus(model.freshness, model.needsUpdate, this.deps.now());
    const live = new Set<string>();
    model.sections.forEach((s, i) => {
      const refs = this.sectionRefs(s);
      const shown = s.rows.filter((r) => !r.folded);
      const folded = s.rows.filter((r) => r.folded);
      const closed = readFold(needsFoldKey(s.id), false);
      const waitingOpen = !readFold(`${needsFoldKey(s.id)}:waiting`, true);
      const sig = `${s.title}\u001f${shown.length}\u001f${folded.length}\u001f${closed}\u001f${waitingOpen}\u001f${s.empty ?? ""}`;
      if (sig !== refs.sig) {
        refs.sig = sig;
        refs.head.paint(s.title, shown.length, closed);
        refs.body.classList.toggle("folded", closed);
        refs.el.classList.toggle("folded", closed);
        refs.empty.textContent = s.rows.length === 0 ? (s.empty ?? "") : "";
        refs.foldHead.style.display = folded.length ? "" : "none";
        refs.foldHead.textContent = `${waitingOpen ? "▾" : "▸"} Waiting on others (${folded.length})`;
        refs.foldHead.setAttribute("aria-expanded", String(waitingOpen));
        refs.foldRows.classList.toggle("folded", !waitingOpen);
      }
      placeAt(this.list, refs.el, i);
      const place = (list: HTMLElement, rows: NeedsYouRow[]): void => {
        rows.forEach((row, j) => {
          live.add(row.id);
          const r = this.rowRefs(row.id);
          this.paintRow(r, row, input.versions);
          placeAt(list, r.el, j);
        });
        truncate(list, rows.length);
      };
      place(refs.rows, shown);
      place(refs.foldRows, folded);
    });
    truncate(this.list, model.sections.length);
    for (const id of [...this.rows.keys()]) if (!live.has(id)) this.rows.delete(id);
    for (const id of [...this.sections.keys()]) if (!model.sections.some((s) => s.id === id)) this.sections.delete(id);
    if (this.menu.key !== null && !live.has(this.menu.key)) this.menu.close();
    this.frame.empty.textContent = model.sections.length === 0 ? NEEDS_YOU_EMPTY : "";
  }

  private sectionRefs(s: NeedsYouSection): SectionRefs {
    let refs = this.sections.get(s.id);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-dt-group chappa-needs-section";
    el.dataset.section = s.id;
    const head = groupHead(true, () => {
      writeFold(needsFoldKey(s.id), false, !readFold(needsFoldKey(s.id), false));
      this.paint();
    });
    const body = div("chappa-dt-rows chappa-needs-body");
    const rows = div("chappa-dt-rows");
    rows.setAttribute("role", "rowgroup");
    const empty = div("chappa-dt-empty");
    const foldHead = document.createElement("button");
    foldHead.type = "button";
    foldHead.className = "chappa-dt-group-head chappa-sv-fold";
    const foldRows = div("chappa-dt-rows chappa-sv-fold-rows folded");
    foldHead.addEventListener("click", () => {
      const key = `${needsFoldKey(s.id)}:waiting`;
      writeFold(key, true, !readFold(key, true));
      this.paint();
    });
    body.append(rows, empty, foldHead, foldRows);
    el.append(head.el, body);
    refs = { el, head, body, rows, empty, foldHead, foldRows, sig: "" };
    this.sections.set(s.id, refs);
    return refs;
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = div("chappa-dt-row chappa-needs-row");
    el.dataset.id = id;
    el.setAttribute("role", "row");
    const cell = (cls: string): HTMLElement => {
      const c = div(`chappa-dt-cell ${cls}`);
      c.setAttribute("role", "cell");
      el.appendChild(c);
      return c;
    };
    const lead = div("chappa-dt-num");
    cell("chappa-needs-c-lead").appendChild(lead);
    const main = cell("chappa-needs-c-title");
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-dt-main chappa-sv-title";
    const detail = div("chappa-dt-sub chappa-sv-detail");
    main.append(title, detail);
    const badge = cell("chappa-needs-badge");
    const r = {} as RowRefs;
    const more = RowMenu.button((btn) => this.toggleMenu(r, btn));
    more.classList.add("chappa-needs-more");
    cell("chappa-dt-acts").appendChild(more);
    Object.assign(r, { el, lead, title, detail, badge, more, sig: "", row: null });
    title.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", (e) => {
      if (r.row && rowClickOpens(e, title)) this.open(r.row);
    });
    this.rows.set(id, r);
    return r;
  }

  private canOpen(row: NeedsYouRow): boolean {
    return row.kind === "terminal" ? !!row.terminalKey : row.opens !== null && !!row.sourceId;
  }

  private toggleMenu(r: RowRefs, btn: HTMLButtonElement): void {
    const row = r.row;
    const input = this.input;
    if (!row || !input) return;
    const entries: MenuEntry[] = [];
    if (this.canOpen(row)) entries.push({ label: "Open", title: opensTitle(row), run: () => this.open(row) });
    if (row.local) entries.push({ label: row.local.label, run: row.local.run });
    this.menu.toggle(row.id, btn, entries, row.actions, versionOf(row, input.versions));
  }

  private open(row: NeedsYouRow): void {
    if (row.kind === "terminal" && row.terminalKey) {
      this.deps.selectTerminal(row.terminalKey);
      return;
    }
    const o: Opens = row.opens;
    if (!o || !row.sourceId) return;
    if ("url" in o) {
      openChecked(this.deps, row.sourceId, o.url);
      return;
    }
    if (!this.deps.openView(o.view, o.key, row.sourceId)) this.deps.say(`No ${o.view} view is available to show ${o.key}.`, "error");
  }

  private paintRow(r: RowRefs, row: NeedsYouRow, versions: Record<string, number>): void {
    r.row = row;
    this.menu.refresh(row.id, row.actions, versionOf(row, versions));
    const sig = JSON.stringify([row.title, row.lead, row.detail, row.badge, row.tone, row.opens, row.kind, row.actions.length, !!row.local]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.kind = row.kind;
    r.el.dataset.tone = row.tone;
    r.el.dataset.accent = row.tone === "bad" ? "bad" : "";
    r.lead.textContent = row.lead ?? "";
    r.title.textContent = row.title;
    r.title.title = opensTitle(row);
    r.title.disabled = !this.canOpen(row);
    r.detail.textContent = row.detail;
    r.detail.title = row.detail;
    r.badge.textContent = "";
    if (row.badge) r.badge.appendChild(chipEl(row.badge));
    // Nothing to put in the menu: its button keeps its place, unseen.
    r.more.hidden = row.actions.length === 0 && !row.local;
  }
}

const versionOf = (row: NeedsYouRow, versions: Record<string, number>): string =>
  row.actions.map((t) => `${t.sourceId}:${versions[t.sourceId] ?? 0}`).join(",");
