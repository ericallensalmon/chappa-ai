// The Needs you page: everything that waits on the user, across terminals,
// agents and sources, one section per kind (source attention under each
// item's own heading). Built like the dashboard: repainted from a plain
// model, rows keyed by id, only changed rows touched. A `panel` action
// opens the source's drill-in page over the view; a row that opens another
// view drills into it (its arrow comes back here).

import type { NeedsYouModel, NeedsYouRow, NeedsYouSection, Opens } from "./needs_you";
import { withPanels } from "./panel_view";
import { ActionsCell, ViewFrame, openChecked, placeAt, truncate, type SourceViewDeps } from "./view_common";

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
  label: HTMLElement;
  count: HTMLElement;
  rows: HTMLElement;
  /** The folded rows' toggle and list (PRs waiting on reviewers). */
  foldHead: HTMLButtonElement;
  foldRows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  title: HTMLButtonElement;
  detail: HTMLElement;
  actions: ActionsCell;
  sig: string;
  row: NeedsYouRow | null;
}

/** The empty state: nothing waits, and what would show up here. */
export const NEEDS_YOU_EMPTY =
  "Nothing needs you. Agents waiting for input, programs that report they are blocked, terminals that rang, " +
  "approvals, pull requests on your plate and alerts from your sources show up here.";

/** What a row's title does on click, as a tooltip. */
export function opensTitle(row: NeedsYouRow): string {
  if (row.kind === "terminal") return `Go to ${row.title}`;
  const o = row.opens;
  if (!o) return row.title;
  if ("url" in o) return `Open ${o.url}`;
  return `Show ${o.key} in ${o.view}`;
}

export class NeedsYouView {
  private readonly frame: ViewFrame;
  private readonly sections = new Map<string, SectionRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private readonly unfolded = new Set<string>();
  private input: NeedsYouViewInput | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: NeedsYouViewDeps;

  constructor(private readonly deps: NeedsYouViewDeps) {
    this.frame = new ViewFrame("Needs you", "attention", () => deps.onClose());
    this.ui = withPanels(() => this.frame, deps);
  }

  get host(): HTMLElement {
    return this.frame.host;
  }

  get active(): boolean {
    return this.frame.active;
  }

  setActive(active: boolean): void {
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
      const open = this.unfolded.has(s.id);
      const sig = `${s.title}\u001f${shown.length}\u001f${folded.length}\u001f${open}`;
      if (sig !== refs.sig) {
        refs.sig = sig;
        refs.label.textContent = s.title;
        refs.count.textContent = String(shown.length);
        refs.foldHead.style.display = folded.length ? "" : "none";
        refs.foldHead.textContent = `${open ? "▾" : "▸"} Waiting on others (${folded.length})`;
        refs.foldHead.setAttribute("aria-expanded", String(open));
        refs.foldRows.classList.toggle("folded", !open);
      }
      placeAt(this.frame.content, refs.el, i);
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
    truncate(this.frame.content, model.sections.length);
    for (const id of [...this.rows.keys()]) if (!live.has(id)) this.rows.delete(id);
    for (const id of [...this.sections.keys()]) if (!model.sections.some((s) => s.id === id)) this.sections.delete(id);
    this.frame.empty.textContent = model.sections.length === 0 ? NEEDS_YOU_EMPTY : "";
  }

  private sectionRefs(s: NeedsYouSection): SectionRefs {
    let refs = this.sections.get(s.id);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-sv-group";
    el.dataset.section = s.id;
    const head = document.createElement("div");
    head.className = "chappa-sv-group-head";
    const label = document.createElement("span");
    label.className = "chappa-sv-group-label";
    const count = document.createElement("span");
    count.className = "chappa-sv-count";
    head.append(label, count);
    const rows = document.createElement("div");
    rows.className = "chappa-sv-rows";
    const foldHead = document.createElement("button");
    foldHead.type = "button";
    foldHead.className = "chappa-sv-group-head chappa-sv-fold";
    const foldRows = document.createElement("div");
    foldRows.className = "chappa-sv-rows chappa-sv-fold-rows folded";
    foldHead.addEventListener("click", () => {
      if (this.unfolded.has(s.id)) this.unfolded.delete(s.id);
      else this.unfolded.add(s.id);
      this.paint();
    });
    el.append(head, rows, foldHead, foldRows);
    refs = { el, label, count, rows, foldHead, foldRows, sig: "" };
    this.sections.set(s.id, refs);
    return refs;
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = document.createElement("div");
    el.className = "chappa-sv-row chappa-needs-row";
    el.dataset.id = id;
    const top = document.createElement("div");
    top.className = "chappa-sv-top";
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-sv-title";
    top.appendChild(title);
    const detail = document.createElement("div");
    detail.className = "chappa-sv-detail";
    const actions = new ActionsCell(this.ui);
    el.append(top, detail, actions.el);
    const r: RowRefs = { el, title, detail, actions, sig: "", row: null };
    title.addEventListener("mousedown", (e) => e.preventDefault());
    title.addEventListener("click", () => {
      if (r.row) this.open(r.row);
    });
    this.rows.set(id, r);
    return r;
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
    r.actions.load(row.actions, row.actions.map((t) => `${t.sourceId}:${versions[t.sourceId] ?? 0}`).join(","));
    const sig = JSON.stringify([row.title, row.detail, row.tone, row.opens, row.kind]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.kind = row.kind;
    r.el.dataset.tone = row.tone;
    r.title.textContent = row.title;
    r.title.title = opensTitle(row);
    r.title.disabled = row.kind !== "terminal" && row.opens === null;
    r.detail.textContent = row.detail;
    r.detail.title = row.detail;
  }
}
