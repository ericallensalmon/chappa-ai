// The Work page: the items of every trusted source serving `work`, the
// active one pinned on top, the rest grouped by status (Done folded), with
// an "Assigned to me" switch and a text filter. Each item is one row of an
// aligned table (key, title, tracker status, the local workflow's status,
// branches and PRs, reporter and age); cells wrap rather than truncate, a
// group's header is one compact line whose fold is remembered, and a row's
// only inline control is "Open" (its link) beside a "⋯" menu with every
// action Rust judged for it, asked for when the menu opens. Clicking a row
// opens its detail beside the list on a wide window (the table keeps its
// key, title and status columns), or in place of the list on a
// narrow one: every field, the item's actions, its local workflow block and
// its sections. The detail's header starts with the shared back arrow; on a
// narrow window it is the page's only header (the view's own is hidden), so
// there is one way back. A `panel` action opens the source's drill-in page
// over the whole view.
//
// One item can be fed by several sources (a tracker owns the row, a
// workspace source adds its folder, status and actions): the row merges
// them (`work_merge.ts`), and the detail shows each source's actions and
// sections under its own name, after the item's banners. Actions other
// sources offer on the row ("Start work") show there too, under theirs.
// A row with banners carries a dot of their worst tone.
//
// A row a stack's data-safety block covers (its bound item, or any row whose
// folder is inside a covered folder) wears an "agents blocked — data safety"
// badge, and its detail opens with a banner that leads to Environment, where
// the block is checked again or acknowledged. Its agent buttons are disabled
// by Rust's own verdict, with the reason.
//
// Built like the other source pages: the app hands it the payloads on every
// change, and rows are keyed and repainted only when their content changed,
// so a poll that changed one item touches one row and focus survives.

import type * as ipc from "../ipc";
import { mountPageHeader } from "../page_header";
import { blockedBanner, markDataSafety } from "./data_safety";
import { formatAgo } from "../record_pane";
import { LocalStatusBlock } from "./local_status";
import type { ActionTarget } from "./needs_you";
import { withPanels, type PanelPage } from "./panel_view";
import type { Chip, NeedsUpdate, PrRow, ViewPayload } from "./prs_model";
import { SectionsBlock, paintBanners, type SectionsContext } from "./sections";
import {
  ActionsCell,
  RowMenu,
  ViewFrame,
  chipEl,
  columnsHead,
  groupHead,
  openButton,
  openChecked,
  paintEmpty,
  placeAt,
  readFold,
  rowClickOpens,
  shortAge,
  truncate,
  writeFold,
  type SourceViewDeps,
} from "./view_common";
import { workItemActionTargets, type ItemActionTarget, type KeyMatchRule, type MergeSource } from "./work_merge";
import { buildWorkModel, type WorkContribution, type WorkFilter, type WorkGroup, type WorkGroupId, type WorkModel, type WorkPrChip, type WorkRow } from "./work_model";

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
  /** Why no source serves it, when none does (`unservedText`). Absent:
   *  the general line. */
  unserved?: string;
  /** Bumped per source whenever its payload was re-read. */
  versions: Record<string, number>;
  /** The listed sources in the Settings order: whose `ref` is whose, why an
   *  owner is missing, the item actions they offer, their key-match rules.
   *  Absent: every source stands alone and offers nothing on others' rows. */
  sources?: readonly MergeSource[];
  /** The PRs view's rows, joined to work rows by branch and key. */
  prRows?: readonly PrRow[];
  /** The data-safety blocks in force (Rust's `data_safety_state`). */
  dataSafety?: readonly ipc.DataSafetyBlockDto[];
}

/** Each source's key-match rule, by registry id (absent = the default). */
export function keyMatchOf(sources: readonly MergeSource[] | undefined): (sourceId: string | null) => KeyMatchRule | null {
  return (sourceId) => (sourceId === null ? null : (sources?.find((s) => s.id === sourceId)?.keyMatch ?? null));
}

export const WORK_VIEW_CSS = `
.chappa-work-filters{display:flex;align-items:center;gap:8px;}
.chappa-work-assigned{border:1px solid #262a31;border-radius:6px;background:#16181d;color:#8b949e;font:12px system-ui,sans-serif;cursor:pointer;padding:3px 10px;}
.chappa-work-assigned[aria-pressed="true"]{background:#1d232b;color:#e6e8ec;border-color:#353a43;}
.chappa-work-assigned:disabled{opacity:0.5;cursor:default;}
.chappa-work-search{width:200px;border:1px solid #262a31;border-radius:6px;background:#0f1114;color:#d6d8dc;font:12px system-ui,sans-serif;padding:3px 8px;}
.chappa-work-split{display:flex;gap:16px;min-height:0;}
.chappa-work-list{flex:1 1 auto;min-width:0;}
.chappa-work-detail{display:none;flex:0 0 42%;min-width:0;border-left:1px solid #1c1f24;padding-left:16px;}
.chappa-work-split.has-detail .chappa-work-detail{display:block;}
@media (max-width: 900px){
  .chappa-work-split.has-detail .chappa-work-list{display:none;}
  .chappa-work-split.has-detail .chappa-work-detail{flex:1 1 auto;border-left:0;padding-left:0;}
  .chappa-source-view.detail-open > .chappa-sv-header{display:none;}
  .chappa-source-view.detail-open .chappa-sv-banners,.chappa-source-view.detail-open .chappa-sv-errors,
  .chappa-source-view.detail-open .chappa-sv-notices,.chappa-source-view.detail-open .chappa-work-banners{display:none;}
}
.chappa-source-view[data-view="work"] .chappa-dt{--dt-cols:minmax(78px,.8fr) minmax(200px,3fr) minmax(80px,.9fr) minmax(90px,1fr) minmax(120px,1.6fr) minmax(64px,.7fr) 74px;}
.chappa-work-split.has-detail .chappa-dt{--dt-cols:minmax(70px,.8fr) minmax(160px,3fr) minmax(76px,.9fr) 74px;}
.chappa-work-split.has-detail .chappa-work-c-flow,.chappa-work-split.has-detail .chappa-work-c-branch,.chappa-work-split.has-detail .chappa-work-c-from{display:none;}
.chappa-work-keyline{display:flex;align-items:center;gap:5px;}
.chappa-work-key{font:600 11px ui-monospace,monospace;color:#adb3bd;}
.chappa-work-active{text-transform:uppercase;}
.chappa-work-active[hidden]{display:none;}
.chappa-work-branches:empty{display:none;}
.chappa-work-pr{cursor:pointer;background:none;}
.chappa-work-pr:hover{border-color:#5ea6ff;}
.chappa-work-detail-head{display:flex;align-items:center;gap:8px;margin:2px 0 10px 0;}
.chappa-work-detail-title{font:600 14px system-ui,sans-serif;color:#e6e8ec;white-space:normal;overflow-wrap:anywhere;}
.chappa-work-fields{display:grid;grid-template-columns:max-content 1fr;gap:3px 12px;margin-bottom:10px;font:12px system-ui,sans-serif;}
.chappa-work-field-name{color:#8b949e;}
.chappa-work-field-value{color:#d6d8dc;min-width:0;overflow-wrap:anywhere;display:flex;flex-wrap:wrap;gap:4px;align-items:center;}
.chappa-work-detail .chappa-sv-actions{margin-bottom:10px;}
.chappa-work-dot{flex:none;width:8px;height:8px;border-radius:50%;background:#5ea6ff;}
.chappa-work-dot[data-tone="warn"]{background:#f0b429;}
.chappa-work-dot[data-tone="bad"]{background:#f85149;}
.chappa-work-dot[hidden]{display:none;}
.chappa-work-detail-banners:empty{display:none;}
.chappa-work-detail-safety:empty{display:none;}
.chappa-work-safety-open{align-self:flex-start;margin-top:4px;border:1px solid #5c1f1c;border-radius:5px;background:#2a1110;color:#f5b3ae;font:12px system-ui,sans-serif;cursor:pointer;padding:2px 10px;}
.chappa-work-detail-banners{margin-bottom:10px;}
.chappa-work-from{margin:12px 0 6px;font:600 11px system-ui,sans-serif;color:#8b949e;text-transform:uppercase;letter-spacing:.04em;}
.chappa-work-from[hidden]{display:none;}
.chappa-work-pr[data-how="branch"],.chappa-work-pr[data-how="key in branch"],.chappa-work-pr[data-how="key in title"]{border-style:dashed;}
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
    return input.unserved ?? "No trusted source serves work items. Add one as a source command in a project's chappa.yml, or attach one in Settings ▸ Sources.";
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

/** A contributor's actions: under its own key, only its own ids. */
const contributionTarget = (c: WorkContribution, ids: readonly string[] = c.actionIds): ActionTarget & { only: readonly string[] } => ({
  sourceId: c.sourceId,
  sourceLabel: c.sourceLabel,
  view: "work",
  key: c.key,
  only: ids,
});

/** A contributor's action ids without the moves its local status block
 *  draws (`localStatus.allowed`), when the status is that contributor's:
 *  a move shows once, in the status block. */
export function idsWithoutMoves(row: WorkRow, c: WorkContribution): string[] {
  const from = row.localStatusFrom?.sourceId ?? row.sourceId;
  if (c.sourceId !== from) return c.actionIds;
  const moves = new Set((row.localStatus?.allowed ?? []).map((m) => m.id));
  return c.actionIds.filter((id) => !moves.has(id));
}

/** Every target of a row's action cell: each contributor's own actions
 *  (without the moves of the status block), then the item actions other
 *  sources offer on it. */
export function rowTargets(row: WorkRow, sources: readonly MergeSource[] | undefined): (ActionTarget & { only?: readonly string[] })[] {
  const own = row.contributions
    .map((c) => ({ c, ids: idsWithoutMoves(row, c) }))
    .filter(({ ids }) => ids.length > 0)
    .map(({ c, ids }) => contributionTarget(c, ids));
  return [...own, ...(sources ? workItemActionTargets(row, sources) : [])];
}

/** The version a row's actions are cached under: every source it asks. */
function versionOf(sourceIds: readonly string[], versions: Record<string, number>): string {
  return [...new Set(sourceIds)].map((id) => `${id}:${versions[id] ?? 0}`).join(",");
}

interface GroupRefs {
  el: HTMLElement;
  head: ReturnType<typeof groupHead>;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  dot: HTMLElement;
  key: HTMLElement;
  active: HTMLElement;
  title: HTMLButtonElement;
  meta: HTMLElement;
  chips: HTMLElement;
  status: HTMLElement;
  flow: HTMLElement;
  flowFrom: HTMLElement;
  branches: HTMLElement;
  prs: HTMLElement;
  from: HTMLElement;
  age: HTMLElement;
  open: HTMLButtonElement;
  more: HTMLButtonElement;
  sig: string;
  row: WorkRow | null;
}

/** The table's columns, in order (the last, unlabelled, holds the row's
 *  Open and its "⋯"). */
export const WORK_COLUMNS = ["KEY", "TITLE", "STATUS", "WORKFLOW", "BRANCH / REPOS", "FROM", ""];

/** Where a group's fold is remembered. */
export const workGroupFoldKey = (groupId: string): string => `work-group:${groupId}`;

export class WorkView {
  private readonly frame: ViewFrame;
  private readonly assignedBtn: HTMLButtonElement;
  private readonly search: HTMLInputElement;
  private readonly bannersEl: HTMLElement;
  private readonly split: HTMLElement;
  private readonly list: HTMLElement;
  private readonly detail: WorkDetail;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: WorkViewDeps;
  private readonly groups = new Map<WorkGroupId, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private readonly table: HTMLElement;
  private readonly body: HTMLElement;
  private readonly menu: RowMenu;
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
    this.ui = withPanels(() => this.frame, deps);
    // Escape closes an open detail before it leaves the view.
    this.frame.setInnerBack(() => {
      if (this.selected === null) return false;
      this.select(null);
      return true;
    });
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
    this.table = document.createElement("div");
    this.table.className = "chappa-dt";
    this.table.setAttribute("role", "table");
    this.body = document.createElement("div");
    this.body.className = "chappa-dt-body";
    this.table.append(columnsHead(WORK_COLUMNS), this.body);
    this.list.appendChild(this.table);
    this.menu = new RowMenu(this.frame.host, this.ui);
    this.detail = new WorkDetail(this.ui, () => this.select(null));
    this.split.append(this.list, this.detail.el);
    this.frame.content.append(this.bannersEl, this.split);
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
    if (this.selected !== null) this.select(null);
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
    const pages = this.frame.subpages;
    return (pages[pages.length - 1] as PanelPage | undefined) ?? null;
  }

  setActive(active: boolean): void {
    if (!active) this.menu.close();
    this.frame.setActive(active);
  }

  /** Where the view's arrow returns. */
  setBackTo(where: string): void {
    this.frame.setBackTo(where);
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
    // A row id (`<sourceId>|<key>`) names one row when two sources share a key.
    const row = model.all.find((r) => r.id === key) ?? model.all.find((r) => r.key === key);
    if (!row) return false;
    this.frame.clearSubpages();
    const shown = model.groups.some((g) => g.rows.some((r) => r.id === row.id));
    if (!shown) {
      this.filter.text = "";
      this.search.value = "";
      if (model.assignedOnly && !row.assignedToMe) this.filter.assignedOnly = false;
    }
    const groupId: WorkGroupId = row.active ? "active" : (row.status?.category ?? "other");
    const group = model.groups.find((g) => g.id === groupId);
    writeFold(workGroupFoldKey(groupId), group?.folded ?? groupId === "done", false);
    this.focused = row.id;
    this.select(row.id);
    const el = this.rows.get(row.id)?.el;
    el?.scrollIntoView?.({ block: "nearest" });
    return true;
  }

  dispose(): void {
    this.menu.dispose();
    this.frame.dispose();
  }

  private select(id: string | null): void {
    this.menu.close();
    this.selected = id;
    if (id === null) this.focused = null;
    this.paint();
  }

  private paint(): void {
    const input = this.input;
    if (!input) return;
    const model = buildWorkModel(input.payloads, {
      needsUpdate: input.needsUpdate,
      filter: this.filter,
      loadedPrs: input.loadedPrs,
      sources: input.sources,
      prRows: input.prRows,
      keyMatch: keyMatchOf(input.sources),
    });
    markDataSafety(model.all, input.dataSafety ?? []);
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
      this.ui,
      model.banners.map(({ sourceId, sourceLabel, banner }) => ({
        banner,
        ctx: { sourceId, target: { sourceId, sourceLabel, view: "work", key: null }, version: version(sourceId) },
      })),
    );
    model.groups.forEach((g, i) => {
      const refs = this.groupRefs(g);
      this.paintGroup(refs, g);
      placeAt(this.body, refs.el, i);
      g.rows.forEach((row, j) => {
        const r = this.rowRefs(row.id);
        this.paintRow(r, row, input);
        placeAt(refs.rows, r.el, j);
      });
      truncate(refs.rows, g.rows.length);
    });
    truncate(this.body, model.groups.length);
    this.table.hidden = model.groups.length === 0;
    if (this.menu.key !== null && !model.groups.some((g) => g.rows.some((r) => r.id === this.menu.key))) this.menu.close();
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
    this.frame.host.classList.toggle("detail-open", sel !== null);
    if (sel) this.detail.show(sel, input);
    else this.detail.hide();
    for (const [id, r] of this.rows) {
      r.el.classList.toggle("selected", id === this.selected);
      r.el.classList.toggle("focused", id === this.focused);
      r.title.setAttribute("aria-current", String(id === this.selected));
    }
    paintEmpty(
      this.frame.empty,
      model.groups.length === 0 ? workEmptyText(input, model, this.filter) : "",
      input.servedBy.length === 0 ? this.deps.openSourcesSettings : undefined,
    );
  }

  private folded(g: WorkGroup): boolean {
    return readFold(workGroupFoldKey(g.id), g.folded);
  }

  private groupRefs(g: WorkGroup): GroupRefs {
    let refs = this.groups.get(g.id);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-dt-group";
    el.dataset.group = g.id;
    const head = groupHead(true, () => {
      const now = this.model?.groups.find((x) => x.id === g.id) ?? g;
      writeFold(workGroupFoldKey(g.id), now.folded, !this.folded(now));
      this.paint();
    });
    const rows = document.createElement("div");
    rows.className = "chappa-dt-rows";
    rows.setAttribute("role", "rowgroup");
    el.append(head.el, rows);
    refs = { el, head, rows, sig: "" };
    this.groups.set(g.id, refs);
    return refs;
  }

  private paintGroup(refs: GroupRefs, g: WorkGroup): void {
    const folded = this.folded(g);
    const sig = `${g.label}\u001f${g.rows.length}\u001f${folded}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.head.paint(g.label, g.rows.length, folded);
    refs.rows.classList.toggle("folded", folded);
    refs.el.classList.toggle("folded", folded);
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = document.createElement("div");
    el.className = "chappa-dt-row chappa-work-row";
    el.dataset.id = id;
    el.setAttribute("role", "row");
    const cell = (cls: string): HTMLElement => {
      const c = document.createElement("div");
      c.className = `chappa-dt-cell ${cls}`;
      c.setAttribute("role", "cell");
      el.appendChild(c);
      return c;
    };
    const keyCell = cell("chappa-work-c-key");
    const keyLine = document.createElement("div");
    keyLine.className = "chappa-work-keyline";
    const dot = document.createElement("span");
    dot.className = "chappa-work-dot";
    dot.hidden = true;
    const key = document.createElement("span");
    key.className = "chappa-work-key";
    keyLine.append(key, dot);
    const active = chipEl({ text: "active", tone: "ok", title: "The environment is set up for this item" });
    active.classList.add("chappa-work-active");
    keyCell.append(keyLine, active);
    const titleCell = cell("chappa-work-c-title");
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-dt-main chappa-sv-title";
    const meta = document.createElement("div");
    meta.className = "chappa-dt-sub chappa-sv-meta";
    const chips = document.createElement("div");
    chips.className = "chappa-dt-chips chappa-sv-chips";
    titleCell.append(title, meta, chips);
    const status = cell("chappa-work-c-status chappa-work-status");
    const flowCell = cell("chappa-work-c-flow");
    const flow = document.createElement("div");
    flow.className = "chappa-work-flow";
    const flowFrom = document.createElement("div");
    flowFrom.className = "chappa-dt-sub";
    flowCell.append(flow, flowFrom);
    const branchCell = cell("chappa-work-c-branch");
    const branches = document.createElement("div");
    branches.className = "chappa-dt-mono chappa-work-branches";
    const prs = document.createElement("div");
    prs.className = "chappa-dt-chips";
    branchCell.append(branches, prs);
    const fromCell = cell("chappa-work-c-from");
    const from = document.createElement("div");
    from.className = "chappa-work-from-name";
    const age = document.createElement("div");
    age.className = "chappa-dt-sub";
    fromCell.append(from, age);
    const acts = cell("chappa-dt-acts");
    const r = {} as RowRefs;
    const open = openButton("", () => {
      if (r.row?.url) openChecked(this.deps, r.row.sourceId, r.row.url);
    });
    const more = RowMenu.button((btn) => this.toggleMenu(r, btn));
    acts.append(open, more);
    Object.assign(r, { el, dot, key, active, title, meta, chips, status, flow, flowFrom, branches, prs, from, age, open, more, sig: "", row: null });
    el.addEventListener("click", (e) => {
      if (r.row && rowClickOpens(e, title)) this.select(this.selected === r.row.id ? null : r.row.id);
    });
    this.rows.set(id, r);
    return r;
  }

  private toggleMenu(r: RowRefs, btn: HTMLButtonElement): void {
    const row = r.row;
    const input = this.input;
    if (!row || !input) return;
    const targets = rowTargets(row, input.sources);
    const entries = [{ label: "Open details", run: () => this.select(row.id) }];
    this.menu.toggle(row.id, btn, entries, targets, versionOf(targets.flatMap(asked), input.versions));
  }

  private paintRow(r: RowRefs, row: WorkRow, input: WorkViewInput): void {
    r.row = row;
    if (this.menu.key === row.id) {
      const targets = rowTargets(row, input.sources);
      this.menu.refresh(row.id, targets, versionOf(targets.flatMap(asked), input.versions));
    }
    const now = this.deps.now();
    const meta = workMeta(row, now);
    const age = shortAge(row.lastTouchedUtc ?? row.created, now);
    const flow = workFlow(row);
    const sig = JSON.stringify([row.key, row.title, row.statusChip, meta, row.labels, row.badges, row.prs, row.active, row.sourceLabel, row.banners, row.branches, row.reporter, age, flow, row.url]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.dot.hidden = row.bannerTone === null;
    r.dot.dataset.tone = row.bannerTone ?? "";
    r.dot.title = row.banners.map((b) => b.banner.title).join("\n");
    r.el.dataset.key = row.key;
    if (row.active) r.el.dataset.accent = "active";
    else if (row.bannerTone) r.el.dataset.accent = row.bannerTone;
    else delete r.el.dataset.accent;
    r.active.hidden = !row.active;
    r.key.textContent = row.key;
    r.title.textContent = row.hasTitle ? row.title : "(no title)";
    r.title.title = `Show ${row.key}`;
    r.meta.textContent = meta;
    r.meta.title = meta;
    r.chips.textContent = "";
    for (const l of row.labels) r.chips.appendChild(chipEl({ text: l, tone: "muted" }));
    for (const b of row.badges) r.chips.appendChild(chipEl(b));
    r.status.textContent = "";
    if (row.statusChip) r.status.appendChild(chipEl(row.statusChip));
    r.flow.textContent = "";
    if (flow) r.flow.appendChild(chipEl(flow.chip));
    r.flowFrom.textContent = flow?.from ?? "";
    r.branches.textContent = "";
    for (const b of row.branches) {
      const line = document.createElement("div");
      line.textContent = `${b.repo}: ${b.branch}`;
      line.title = `${b.forge} ${b.repo}: ${b.branch}`;
      r.branches.appendChild(line);
    }
    r.prs.textContent = "";
    for (const p of row.prs) r.prs.appendChild(prChipEl(p, row.sourceId, this.ui));
    r.from.textContent = row.reporter ? (row.reporter.isMe ? "me" : row.reporter.name) : "";
    r.age.textContent = age ?? "";
    r.age.title = row.lastTouchedUtc ? `touched ${new Date(row.lastTouchedUtc).toLocaleString()}` : row.created ? `created ${new Date(row.created).toLocaleString()}` : "";
    r.open.hidden = !row.url;
    r.open.title = row.url ? `Open ${row.url}` : "";
  }
}

/** Every source a target's actions are asked of. */
const asked = (t: ActionTarget & { owner?: string }): string[] => (t.owner ? [t.sourceId, t.owner] : [t.sourceId]);

/** The WORKFLOW cell: the item's local status as a chip (warn while it
 *  waits on a person), and whose workflow it is when another source than
 *  the tracker keeps it. Null when the item has none. */
export function workFlow(row: WorkRow): { chip: Chip; from: string | null } | null {
  const ls = row.localStatus;
  if (!ls) return null;
  const waits = !!ls.pendingApproval || ls.gate === "human-exit";
  const title = ls.pendingApproval ? `waiting for approval of the move to ${ls.pendingApproval.to}` : ls.gate === "human-exit" ? "only a person can move it out" : ls.label;
  const from = row.localStatusFrom && row.localStatusFrom.sourceLabel !== row.sourceLabel ? row.localStatusFrom.sourceLabel : null;
  return { chip: { text: ls.label, tone: waits ? "warn" : "info", title }, from };
}

/** A linked PR: opens in the PRs view when that view lists it, else its
 *  link through the checked opener. */
export function prChipEl(p: WorkPrChip, sourceId: string, deps: WorkViewDeps): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "chappa-sv-chip chappa-work-pr";
  btn.dataset.tone = p.tone;
  btn.dataset.pr = p.key;
  btn.dataset.how = p.how;
  // A joined PR says how it joined, so a wrong join is visible at a glance.
  const how = p.how === "linked" ? "" : ` · ${p.how}`;
  btn.textContent = `${p.status ? `${p.text} ${p.status}` : p.text}${how}`;
  const what = p.loaded ? `Show ${p.text} in PRs` : p.url ? `Open ${p.url}` : p.text;
  btn.title = p.how === "linked" ? what : `${what} (matched by ${p.how === "branch" ? "its branch" : p.how})`;
  btn.disabled = !p.loaded && !p.url;
  btn.addEventListener("mousedown", (e) => e.preventDefault());
  btn.addEventListener("click", () => {
    if (p.loaded && deps.openView("prs", p.key)) return;
    if (p.url) openChecked(deps, sourceId, p.url);
  });
  return btn;
}

/** One more source's part of the detail: its name, its actions, and (for
 *  a source feeding the row) its sections. */
interface ExtraBlock {
  el: HTMLElement;
  head: HTMLElement;
  actions: ActionsCell;
  sections: SectionsBlock;
}

/** The detail pane of the selected item. */
class WorkDetail {
  readonly el: HTMLElement;
  private readonly title: HTMLElement;
  private readonly banners: HTMLElement;
  /** The data-safety banner, when a block covers the item. */
  private readonly safety: HTMLElement;
  private safetySig = "";
  private readonly fields: HTMLElement;
  /** The first source's name, shown when more than one feeds the item. */
  private readonly from: HTMLElement;
  private readonly actions: ActionsCell;
  private readonly status: LocalStatusBlock;
  private readonly sections: SectionsBlock;
  private readonly extras: HTMLElement;
  private readonly extraBlocks = new Map<string, ExtraBlock>();
  private sig = "";
  private shownId: string | null = null;

  constructor(private readonly deps: WorkViewDeps, onBack: () => void) {
    this.el = document.createElement("aside");
    this.el.className = "chappa-work-detail";
    this.el.setAttribute("aria-label", "Work item");
    const head = document.createElement("div");
    head.className = "chappa-work-detail-head";
    this.title = mountPageHeader(head, {
      title: "",
      backTo: "the Work list",
      onBack,
      arrowClass: "chappa-work-detail-back",
      titleClass: "chappa-work-detail-title",
    }).title;
    this.banners = document.createElement("div");
    this.banners.className = "chappa-work-detail-banners";
    this.safety = document.createElement("div");
    this.safety.className = "chappa-work-detail-safety";
    this.fields = document.createElement("div");
    this.fields.className = "chappa-work-fields";
    this.from = document.createElement("div");
    this.from.className = "chappa-work-from";
    this.from.hidden = true;
    this.actions = new ActionsCell(deps);
    this.status = new LocalStatusBlock(deps);
    this.sections = new SectionsBlock(deps);
    this.extras = document.createElement("div");
    this.extras.className = "chappa-work-extras";
    this.el.append(head, this.safety, this.banners, this.fields, this.from, this.actions.el, this.status.el, this.sections.el, this.extras);
  }

  show(row: WorkRow, input: WorkViewInput): void {
    const v = (id: string): string => versionOf([id], input.versions);
    const primary = row.contributions[0];
    const target = targetOf(row);
    // The moves are drawn by the status block, against the source whose
    // status it is: that source's general buttons skip them.
    const statusFrom = row.localStatusFrom ?? { sourceId: row.sourceId, sourceLabel: row.sourceLabel, key: row.key };
    const statusTarget: ActionTarget = { ...statusFrom, view: "work" };
    this.paintSafety(row.dataSafety ?? null);
    const ownIds = (c: WorkContribution): string[] => idsWithoutMoves(row, c);
    paintBanners(
      this.banners,
      this.deps,
      row.banners.map((b) => ({
        banner: b.banner,
        ctx: { sourceId: b.sourceId, target: { sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "work", key: b.key }, version: v(b.sourceId) },
      })),
    );
    const offered = input.sources ? workItemActionTargets(row, input.sources) : [];
    const many = row.contributions.length + offered.length > 1;
    this.from.hidden = !many || !primary;
    this.from.textContent = primary ? primary.sourceLabel : "";
    const own = primary ? ownIds(primary) : [];
    if (primary && own.length) this.actions.load([contributionTarget(primary, own)], v(primary.sourceId));
    else this.actions.load([], v(row.sourceId));
    this.status.update(row.localStatus, statusTarget, v(statusFrom.sourceId));
    const primaryTarget: ActionTarget = primary ? { sourceId: primary.sourceId, sourceLabel: primary.sourceLabel, view: "work", key: primary.key } : target;
    const ctx: SectionsContext = { sourceId: primaryTarget.sourceId, target: primaryTarget, version: v(primaryTarget.sourceId) };
    this.sections.update(primary ? primary.sections : row.sections, ctx);
    this.paintExtras(row.contributions.slice(1), offered, ownIds, v);
    const now = this.deps.now();
    const sig = JSON.stringify([row, Math.floor(now / 60_000)]);
    if (sig === this.sig && this.shownId === row.id) return;
    this.sig = sig;
    this.shownId = row.id;
    this.el.dataset.id = row.id;
    this.title.textContent = row.hasTitle ? `${row.key} ${row.title}` : row.key;
    this.paintFields(row, now);
  }

  /** The data-safety banner: why agents are kept out of this item and the
   *  way to Environment, where the block is dealt with. */
  private paintSafety(block: ipc.DataSafetyBlockDto | null): void {
    const sig = JSON.stringify(block);
    if (sig === this.safetySig) return;
    this.safetySig = sig;
    this.safety.textContent = "";
    if (!block) return;
    const { title, lines } = blockedBanner(block);
    const el = document.createElement("div");
    el.className = "chappa-banner";
    el.dataset.tone = "bad";
    el.dataset.banner = "data-safety";
    const head = document.createElement("div");
    head.className = "chappa-banner-title";
    head.textContent = title;
    el.appendChild(head);
    for (const line of lines) {
      const l = document.createElement("div");
      l.className = "chappa-banner-line";
      l.textContent = line;
      el.appendChild(l);
    }
    const open = document.createElement("button");
    open.type = "button";
    open.className = "chappa-work-safety-open";
    open.textContent = "Open Environment";
    open.addEventListener("click", () => {
      if (!this.deps.openView("environment", block.component ?? "")) this.deps.say("The Environment view is not available.", "error");
    });
    el.appendChild(open);
    this.safety.appendChild(el);
  }

  hide(): void {
    this.shownId = null;
    this.sig = "";
    delete this.el.dataset.id;
  }

  /** The blocks after the first source's: each further source feeding the
   *  row (its actions and sections), then each source offering actions on
   *  it, under their names. */
  private paintExtras(
    rest: WorkContribution[],
    offered: ItemActionTarget[],
    ownIds: (c: WorkContribution) => string[],
    v: (id: string) => string,
  ): void {
    const wanted: string[] = [];
    const block = (id: string, label: string, role: string): ExtraBlock => {
      let b = this.extraBlocks.get(id);
      if (!b) {
        const el = document.createElement("div");
        el.className = "chappa-work-extra";
        const head = document.createElement("div");
        head.className = "chappa-work-from";
        const actions = new ActionsCell(this.deps);
        const sections = new SectionsBlock(this.deps);
        el.append(head, actions.el, sections.el);
        b = { el, head, actions, sections };
        this.extraBlocks.set(id, b);
      }
      b.el.dataset.source = id.slice(id.indexOf(":") + 1);
      b.el.dataset.role = role;
      b.head.textContent = label;
      wanted.push(id);
      return b;
    };
    for (const c of rest) {
      const b = block(`from:${c.sourceId}`, c.sourceLabel, "contributor");
      const ids = ownIds(c);
      b.actions.load(ids.length ? [contributionTarget(c, ids)] : [], v(c.sourceId));
      const t: ActionTarget = { sourceId: c.sourceId, sourceLabel: c.sourceLabel, view: "work", key: c.key };
      b.sections.update(c.sections, { sourceId: c.sourceId, target: t, version: v(c.sourceId) });
    }
    for (const t of offered) {
      const b = block(`offers:${t.sourceId}`, t.sourceLabel, "offers");
      b.actions.load([t], `${v(t.sourceId)},${v(t.owner)}`);
      b.sections.update([], { sourceId: t.sourceId, target: null, version: v(t.sourceId) });
    }
    wanted.forEach((id, i) => placeAt(this.extras, this.extraBlocks.get(id)!.el, i));
    truncate(this.extras, wanted.length);
    for (const id of [...this.extraBlocks.keys()]) if (!wanted.includes(id)) this.extraBlocks.delete(id);
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
    add("Folder", row.folder && row.folderFrom && row.folderFrom !== row.sourceLabel ? `${row.folder} (from ${row.folderFrom})` : row.folder);
    add("Branches", row.branches.length ? row.branches.map((b) => `${b.repo}: ${b.branch}`).join(", ") : null);
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
    add("Source", row.contributions.length > 1 ? row.contributions.map((c) => c.sourceLabel).join(", ") : row.sourceLabel);
  }
}
