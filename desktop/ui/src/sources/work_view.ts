// The Work page: the items of every trusted source serving `work`, the
// active one pinned on top, the rest grouped by status (Done folded), with
// an "Assigned to me" switch and a text filter. Selecting a row opens its
// detail beside the list on a wide window, or in place of the list (with a
// back arrow) on a narrow one: every field, the item's actions, its local
// workflow block and its sections. A `panel` action opens the source's
// drill-in page over both.
//
// Built like the other source pages: the app hands it the payloads on every
// change, and rows are keyed and repainted only when their content changed,
// so a poll that changed one item touches one row and focus survives.

import { formatAgo } from "../record_pane";
import { LocalStatusBlock } from "./local_status";
import type { ActionTarget } from "./needs_you";
import { PanelPage } from "./panel_view";
import type { NeedsUpdate, ViewPayload } from "./prs_model";
import { SectionsBlock, paintBanners, type SectionsContext } from "./sections";
import {
  ActionsCell,
  ViewFrame,
  chipEl,
  openChecked,
  placeAt,
  truncate,
  type PanelOrigin,
  type SourceViewDeps,
} from "./view_common";
import { buildWorkModel, type WorkFilter, type WorkGroup, type WorkGroupId, type WorkModel, type WorkPrChip, type WorkRow } from "./work_model";

export interface WorkViewDeps extends SourceViewDeps {
  /** Open another view at an item (a PR chip → the PRs view); false when
   *  that view is not available. */
  openView(view: string, key: string): boolean;
}

export interface WorkViewInput {
  /** The `work` payloads of the trusted sources serving it. */
  payloads: ViewPayload[];
  needsUpdate: NeedsUpdate[];
  /** The PRs view's row ids: a PR chip found here opens in that view. */
  loadedPrs: ReadonlySet<string>;
  /** Labels of the trusted sources serving Work (empty: none does). */
  servedBy: string[];
  /** Bumped per source whenever its payload was re-read. */
  versions: Record<string, number>;
}

export const WORK_VIEW_CSS = `
.chappa-work-filters{display:flex;align-items:center;gap:8px;}
.chappa-work-assigned{border:1px solid #262a31;border-radius:6px;background:#16181d;color:#8b949e;font:12px system-ui,sans-serif;cursor:pointer;padding:3px 10px;}
.chappa-work-assigned[aria-pressed="true"]{background:#1d232b;color:#e6e8ec;border-color:#353a43;}
.chappa-work-assigned:disabled{opacity:0.5;cursor:default;}
.chappa-work-search{width:200px;border:1px solid #262a31;border-radius:6px;background:#0f1114;color:#d6d8dc;font:12px system-ui,sans-serif;padding:3px 8px;}
.chappa-work-split{display:flex;gap:16px;min-height:0;}
.chappa-work-list{flex:1 1 auto;min-width:0;}
.chappa-work-detail{display:none;flex:0 0 46%;min-width:0;border-left:1px solid #1c1f24;padding-left:16px;}
.chappa-work-split.has-detail .chappa-work-detail{display:block;}
.chappa-work-split.hidden,.chappa-work-banners.hidden{display:none;}
@media (max-width: 900px){
  .chappa-work-split.has-detail .chappa-work-list{display:none;}
  .chappa-work-split.has-detail .chappa-work-detail{flex:1 1 auto;border-left:0;padding-left:0;}
}
.chappa-work-row{cursor:default;}
.chappa-work-row.selected{border-color:#353a43;background:#161a20;}
.chappa-work-row.focused{box-shadow:0 0 0 1px #5ea6ff;}
.chappa-work-key{flex:none;font:11px ui-monospace,monospace;color:#8b949e;}
.chappa-work-pr{cursor:pointer;background:none;}
.chappa-work-pr:hover{border-color:#5ea6ff;}
.chappa-work-detail-head{display:flex;align-items:center;gap:8px;margin:2px 0 10px;}
.chappa-work-detail-title{font:600 14px system-ui,sans-serif;color:#e6e8ec;overflow-wrap:anywhere;}
.chappa-work-fields{display:grid;grid-template-columns:max-content 1fr;gap:3px 12px;margin-bottom:10px;font:12px system-ui,sans-serif;}
.chappa-work-field-name{color:#8b949e;}
.chappa-work-field-value{color:#d6d8dc;min-width:0;overflow-wrap:anywhere;display:flex;flex-wrap:wrap;gap:4px;align-items:center;}
.chappa-work-detail .chappa-sv-actions{margin-bottom:10px;}
.chappa-panel-page{display:flex;flex-direction:column;gap:6px;}
.chappa-panel-head{display:flex;align-items:center;gap:14px;margin-bottom:6px;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = WORK_VIEW_CSS;
  document.head.appendChild(el);
}

/** The empty-state line: why there is nothing to show. */
export function workEmptyText(input: WorkViewInput, model: WorkModel, filter: WorkFilter): string {
  if (input.servedBy.length === 0) {
    return "No trusted source serves work items. Add one as a source command in a project's chappa.yml, or attach one in Settings ▸ Sources.";
  }
  if (!model.hasData) return `Waiting for the first answer from ${input.servedBy.join(", ")}.`;
  if (model.all.length === 0) return "No work items.";
  if (filter.text.trim() !== "") return "No item matches the filter.";
  return "Nothing is assigned to you. Switch off “Assigned to me” to see every item.";
}

/** The age of a timestamp ("3 h ago"), or null when it cannot be read. */
function ago(utc: string | null, now: number): string | null {
  const ms = utc ? Date.parse(utc) : NaN;
  return Number.isFinite(ms) ? formatAgo(ms, now) : null;
}

/** The row's second line: type · priority · parent · reporter · touched. */
export function workMeta(row: WorkRow, now: number): string {
  const touched = ago(row.lastTouchedUtc, now);
  return [
    row.type,
    row.priority ? `${row.priority} priority` : null,
    row.parent ? `in ${row.parent.key}` : null,
    row.reporter ? `from ${row.reporter.isMe ? "me" : row.reporter.name}` : null,
    touched ? `touched ${touched}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

const targetOf = (row: WorkRow): ActionTarget => ({ sourceId: row.sourceId, sourceLabel: row.sourceLabel, view: "work", key: row.key });

interface GroupRefs {
  el: HTMLElement;
  head: HTMLButtonElement;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  key: HTMLElement;
  title: HTMLButtonElement;
  status: HTMLElement;
  meta: HTMLElement;
  chips: HTMLElement;
  actions: ActionsCell;
  sig: string;
  row: WorkRow | null;
}

export class WorkView {
  private readonly frame: ViewFrame;
  private readonly assignedBtn: HTMLButtonElement;
  private readonly search: HTMLInputElement;
  private readonly bannersEl: HTMLElement;
  private readonly split: HTMLElement;
  private readonly list: HTMLElement;
  private readonly detail: WorkDetail;
  private readonly panelHost: HTMLElement;
  private panel: PanelPage | null = null;
  private readonly groups = new Map<WorkGroupId, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  /** Fold choices per group, for as long as the page lives (the session). */
  private readonly foldChoice = new Map<WorkGroupId, boolean>();
  /** The filters, kept for the session: null = the default. */
  private readonly filter: WorkFilter = { assignedOnly: null, text: "" };
  private selected: string | null = null;
  private focused: string | null = null;
  /** A focus asked for before the data arrived. */
  private pendingFocus: string | null = null;
  private input: WorkViewInput | null = null;
  private model: WorkModel | null = null;

  constructor(private readonly deps: WorkViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("Work", "work", () => deps.onClose());
    const filters = document.createElement("div");
    filters.className = "chappa-work-filters";
    this.assignedBtn = document.createElement("button");
    this.assignedBtn.type = "button";
    this.assignedBtn.className = "chappa-work-assigned";
    this.assignedBtn.textContent = "Assigned to me";
    this.assignedBtn.addEventListener("click", () => {
      this.filter.assignedOnly = !(this.model?.assignedOnly ?? false);
      this.paint();
    });
    this.search = document.createElement("input");
    this.search.type = "search";
    this.search.className = "chappa-work-search";
    this.search.placeholder = "Filter by key, title or label";
    this.search.setAttribute("aria-label", "Filter work items");
    this.search.addEventListener("input", () => {
      this.filter.text = this.search.value;
      this.paint();
    });
    // Escape in the box clears it rather than leaving the page.
    this.search.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.search.value !== "") {
        e.stopPropagation();
        this.search.value = "";
        this.filter.text = "";
        this.paint();
      }
    });
    filters.append(this.assignedBtn, this.search);
    this.frame.headerSlot.appendChild(filters);
    this.bannersEl = document.createElement("div");
    this.bannersEl.className = "chappa-work-banners";
    this.split = document.createElement("div");
    this.split.className = "chappa-work-split";
    this.list = document.createElement("div");
    this.list.className = "chappa-work-list";
    this.detail = new WorkDetail({ ...deps, openPanel: (o, p) => this.openPanel(o, p) }, () => this.select(null));
    this.split.append(this.list, this.detail.el);
    this.panelHost = document.createElement("div");
    this.panelHost.className = "chappa-work-panel";
    this.frame.content.append(this.bannersEl, this.split, this.panelHost);
  }

  get host(): HTMLElement {
    return this.frame.host;
  }

  get active(): boolean {
    return this.frame.active;
  }

  /** The row whose detail is open (`<sourceId>|<key>`), or null. */
  get selectedId(): string | null {
    return this.selected;
  }

  /** The panel page on screen, or null. */
  get openPanelPage(): PanelPage | null {
    return this.panel;
  }

  setActive(active: boolean): void {
    this.frame.setActive(active);
  }

  update(input: WorkViewInput): void {
    this.input = input;
    this.paint();
    if (this.pendingFocus !== null && this.model?.hasData) {
      const key = this.pendingFocus;
      this.pendingFocus = null;
      if (!this.focus(key)) this.deps.say(`${key} is not in the Work list.`, "error");
    }
  }

  /**
   * Bring the item `key` into view: clear what filters hide it, unfold its
   * group, open its detail, scroll to it and mark it. Answers whether it was
   * found; asked before the first payload arrived, it waits for it (and
   * answers true).
   */
  focus(key: string): boolean {
    const model = this.model;
    if (!model?.hasData) {
      this.pendingFocus = key;
      return true;
    }
    const row = model.all.find((r) => r.key === key);
    if (!row) return false;
    this.closePanel();
    const shown = model.groups.some((g) => g.rows.some((r) => r.id === row.id));
    if (!shown) {
      this.filter.text = "";
      this.search.value = "";
      if (model.assignedOnly && !row.assignedToMe) this.filter.assignedOnly = false;
    }
    const group: WorkGroupId = row.active ? "active" : (row.status?.category ?? "other");
    this.foldChoice.set(group, false);
    this.focused = row.id;
    this.select(row.id);
    const el = this.rows.get(row.id)?.el;
    el?.scrollIntoView?.({ block: "nearest" });
    return true;
  }

  dispose(): void {
    this.closePanel();
    this.frame.dispose();
  }

  private select(id: string | null): void {
    this.selected = id;
    if (id === null) this.focused = null;
    this.paint();
  }

  private openPanel(origin: PanelOrigin, payload: unknown): void {
    this.closePanel();
    this.panel = new PanelPage(this.deps, origin, payload, () => this.closePanel());
    this.panelHost.appendChild(this.panel.el);
    this.split.classList.add("hidden");
    this.bannersEl.classList.add("hidden");
    this.paint();
  }

  private closePanel(): void {
    if (!this.panel) return;
    this.panel.dispose();
    this.panel = null;
    this.split.classList.remove("hidden");
    this.bannersEl.classList.remove("hidden");
    this.paint();
  }

  private paint(): void {
    const input = this.input;
    if (!input) return;
    const model = buildWorkModel(input.payloads, { needsUpdate: input.needsUpdate, filter: this.filter, loadedPrs: input.loadedPrs });
    this.model = model;
    this.frame.paintStatus(model.freshness, model.needsUpdate, this.deps.now());
    this.assignedBtn.setAttribute("aria-pressed", String(model.assignedOnly));
    this.assignedBtn.disabled = !model.anyAssigned && !model.assignedOnly;
    this.assignedBtn.title = model.anyAssigned
      ? "Show only the items assigned to you (the active item always shows)"
      : "No item says it is assigned to you";
    const version = (sourceId: string): string => `${sourceId}:${input.versions[sourceId] ?? 0}`;
    paintBanners(
      this.bannersEl,
      this.deps,
      model.banners.map(({ sourceId, sourceLabel, banner }) => ({
        banner,
        ctx: { sourceId, target: { sourceId, sourceLabel, view: "work", key: banner.id }, version: version(sourceId) },
      })),
    );
    model.groups.forEach((g, i) => {
      const refs = this.groupRefs(g);
      this.paintGroup(refs, g);
      placeAt(this.list, refs.el, i);
      g.rows.forEach((row, j) => {
        const r = this.rowRefs(row.id);
        this.paintRow(r, row, version(row.sourceId));
        placeAt(refs.rows, r.el, j);
      });
      truncate(refs.rows, g.rows.length);
    });
    truncate(this.list, model.groups.length);
    const live = new Set(model.all.map((r) => r.id));
    for (const id of [...this.rows.keys()]) if (!live.has(id)) this.rows.delete(id);
    for (const id of [...this.groups.keys()]) if (!model.groups.some((g) => g.id === id)) this.groups.delete(id);
    // The detail follows its item across polls; an item that left closes it.
    const sel = this.selected ? model.all.find((r) => r.id === this.selected) ?? null : null;
    if (this.selected && !sel) {
      this.selected = null;
      this.focused = null;
    }
    this.split.classList.toggle("has-detail", sel !== null);
    if (sel) this.detail.show(sel, version(sel.sourceId));
    else this.detail.hide();
    for (const [id, r] of this.rows) {
      r.el.classList.toggle("selected", id === this.selected);
      r.el.classList.toggle("focused", id === this.focused);
      r.title.setAttribute("aria-current", String(id === this.selected));
    }
    this.frame.empty.textContent = model.groups.length === 0 && !this.panel ? workEmptyText(input, model, this.filter) : "";
  }

  private folded(g: WorkGroup): boolean {
    return this.foldChoice.get(g.id) ?? g.folded;
  }

  private groupRefs(g: WorkGroup): GroupRefs {
    let refs = this.groups.get(g.id);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-sv-group";
    el.dataset.group = g.id;
    const head = document.createElement("button");
    head.type = "button";
    head.className = "chappa-sv-group-head";
    const rows = document.createElement("div");
    rows.className = "chappa-sv-rows";
    el.append(head, rows);
    head.addEventListener("click", () => {
      const now = this.model?.groups.find((x) => x.id === g.id) ?? g;
      this.foldChoice.set(g.id, !this.folded(now));
      this.paint();
    });
    refs = { el, head, rows, sig: "" };
    this.groups.set(g.id, refs);
    return refs;
  }

  private paintGroup(refs: GroupRefs, g: WorkGroup): void {
    const folded = this.folded(g);
    const sig = `${g.label}\u001f${g.rows.length}\u001f${folded}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.head.textContent = "";
    const label = document.createElement("span");
    label.className = "chappa-sv-group-label";
    label.textContent = `${folded ? "▸" : "▾"} ${g.label}`;
    const count = document.createElement("span");
    count.className = "chappa-sv-count";
    count.textContent = String(g.rows.length);
    refs.head.append(label, count);
    refs.head.setAttribute("aria-expanded", String(!folded));
    refs.rows.classList.toggle("folded", folded);
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = document.createElement("div");
    el.className = "chappa-sv-row chappa-work-row";
    el.dataset.id = id;
    const top = document.createElement("div");
    top.className = "chappa-sv-top";
    const key = document.createElement("span");
    key.className = "chappa-work-key";
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-sv-title";
    const status = document.createElement("span");
    status.className = "chappa-work-status";
    top.append(key, title, status);
    const meta = document.createElement("div");
    meta.className = "chappa-sv-meta";
    const chips = document.createElement("div");
    chips.className = "chappa-sv-chips";
    const actions = new ActionsCell({ ...this.deps, openPanel: (o, p) => this.openPanel(o, p) });
    el.append(top, meta, chips, actions.el);
    const r: RowRefs = { el, key, title, status, meta, chips, actions, sig: "", row: null };
    title.addEventListener("click", () => {
      if (r.row) this.select(this.selected === r.row.id ? null : r.row.id);
    });
    this.rows.set(id, r);
    return r;
  }

  private paintRow(r: RowRefs, row: WorkRow, version: string): void {
    r.row = row;
    if (row.hasActions && row.actionIds.length) r.actions.load([targetOf(row)], version, row.actionIds);
    else r.actions.load([], version);
    const now = this.deps.now();
    const meta = workMeta(row, now);
    const sig = JSON.stringify([row.key, row.title, row.statusChip, meta, row.labels, row.badges, row.prs, row.active, row.sourceLabel]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.key = row.key;
    r.el.dataset.tone = row.active ? "info" : "";
    r.key.textContent = row.key;
    r.title.textContent = row.hasTitle ? row.title : "(no title)";
    r.title.title = `Show ${row.key}`;
    r.status.textContent = "";
    if (row.statusChip) r.status.appendChild(chipEl(row.statusChip));
    r.meta.textContent = meta;
    r.meta.title = meta;
    fillChips(r.chips, row, this.deps);
  }
}

/** Labels (muted), the source's badges, then the linked PRs as buttons. */
function fillChips(parent: HTMLElement, row: WorkRow, deps: WorkViewDeps): void {
  parent.textContent = "";
  for (const l of row.labels) parent.appendChild(chipEl({ text: l, tone: "muted" }));
  for (const b of row.badges) parent.appendChild(chipEl(b));
  for (const p of row.prs) parent.appendChild(prChipEl(p, row.sourceId, deps));
}

/** A linked PR: opens in the PRs view when that view lists it, else its
 *  link through the checked opener. */
export function prChipEl(p: WorkPrChip, sourceId: string, deps: WorkViewDeps): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "chappa-sv-chip chappa-work-pr";
  btn.dataset.tone = p.tone;
  btn.dataset.pr = p.key;
  btn.textContent = p.status ? `${p.text} ${p.status}` : p.text;
  btn.title = p.loaded ? `Show ${p.text} in PRs` : p.url ? `Open ${p.url}` : p.text;
  btn.disabled = !p.loaded && !p.url;
  btn.addEventListener("mousedown", (e) => e.preventDefault());
  btn.addEventListener("click", () => {
    if (p.loaded && deps.openView("prs", p.key)) return;
    if (p.url) openChecked(deps, sourceId, p.url);
  });
  return btn;
}

/** The detail pane of the selected item. */
class WorkDetail {
  readonly el: HTMLElement;
  private readonly title: HTMLElement;
  private readonly fields: HTMLElement;
  private readonly actions: ActionsCell;
  private readonly status: LocalStatusBlock;
  private readonly sections: SectionsBlock;
  private sig = "";
  private shownId: string | null = null;

  constructor(private readonly deps: WorkViewDeps, onBack: () => void) {
    this.el = document.createElement("aside");
    this.el.className = "chappa-work-detail";
    this.el.setAttribute("aria-label", "Work item");
    const head = document.createElement("div");
    head.className = "chappa-work-detail-head";
    const back = document.createElement("button");
    back.type = "button";
    back.className = "chappa-sv-back chappa-work-detail-back";
    back.title = "Back to the list";
    const arrow = document.createElement("span");
    arrow.setAttribute("aria-hidden", "true");
    arrow.textContent = "←";
    back.appendChild(arrow);
    back.addEventListener("click", onBack);
    this.title = document.createElement("span");
    this.title.className = "chappa-work-detail-title";
    head.append(back, this.title);
    this.fields = document.createElement("div");
    this.fields.className = "chappa-work-fields";
    this.actions = new ActionsCell(deps);
    this.status = new LocalStatusBlock(deps);
    this.sections = new SectionsBlock(deps);
    this.el.append(head, this.fields, this.actions.el, this.status.el, this.sections.el);
  }

  show(row: WorkRow, version: string): void {
    const target = targetOf(row);
    // The moves are drawn by the status block: the general buttons skip them.
    const moves = new Set((row.localStatus?.allowed ?? []).map((m) => m.id));
    const own = row.actionIds.filter((id) => !moves.has(id));
    if (own.length) this.actions.load([target], version, own);
    else this.actions.load([], version);
    this.status.update(row.localStatus, target, version);
    const ctx: SectionsContext = { sourceId: row.sourceId, target, version };
    this.sections.update(row.sections, ctx);
    const now = this.deps.now();
    const sig = JSON.stringify([row, Math.floor(now / 60_000)]);
    if (sig === this.sig && this.shownId === row.id) return;
    this.sig = sig;
    this.shownId = row.id;
    this.el.dataset.id = row.id;
    this.title.textContent = row.hasTitle ? `${row.key} ${row.title}` : row.key;
    this.paintFields(row, now);
  }

  hide(): void {
    this.shownId = null;
    this.sig = "";
    delete this.el.dataset.id;
  }

  private paintFields(row: WorkRow, now: number): void {
    this.fields.textContent = "";
    const add = (name: string, value: string | HTMLElement[] | null): void => {
      if (value === null || (Array.isArray(value) && value.length === 0) || value === "") return;
      const n = document.createElement("div");
      n.className = "chappa-work-field-name";
      n.textContent = name;
      const v = document.createElement("div");
      v.className = "chappa-work-field-value";
      v.dataset.field = name;
      if (typeof value === "string") v.textContent = value;
      else v.append(...value);
      this.fields.append(n, v);
    };
    const when = (utc: string | null): string | null => {
      const a = ago(utc, now);
      return a && utc ? `${a} (${new Date(utc).toLocaleString()})` : null;
    };
    add("Key", row.key);
    add("Status", row.statusChip ? [chipEl(row.statusChip)] : null);
    add("Type", row.type);
    add("Priority", row.priority);
    add("Assigned to me", row.assignedToMe ? "yes" : null);
    add("Active", row.active ? "the environment is set up for this item" : null);
    add("Labels", row.labels.map((l) => chipEl({ text: l, tone: "muted" })));
    add("Parent", row.parent ? (row.parent.title ? `${row.parent.key} ${row.parent.title}` : row.parent.key) : null);
    add("Reporter", row.reporter ? (row.reporter.isMe ? `${row.reporter.name} (me)` : row.reporter.name) : null);
    add("Created", when(row.created));
    add("Last touched", when(row.lastTouchedUtc));
    add("Folder", row.folder);
    add("Pull requests", row.prs.map((p) => prChipEl(p, row.sourceId, this.deps)));
    add("Badges", row.badges.map((b) => chipEl(b)));
    if (row.url) {
      const link = document.createElement("button");
      link.type = "button";
      link.className = "chappa-sec-link";
      link.textContent = row.url;
      link.title = `Open ${row.url}`;
      link.addEventListener("click", () => openChecked(this.deps, row.sourceId, row.url!));
      add("Link", [link]);
    }
    add("Source", row.sourceLabel);
  }
}
