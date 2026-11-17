// The PRs page: the merged pull requests of every trusted source serving
// the view, in three tabs: My PRs and Reviewing grouped by bucket, All
// grouped by how I am on each review (team groups start folded). Built like
// the dashboard: the app hands it a plain model on every change and the
// page repaints only the rows whose content changed (a poll that changed
// one PR touches one row, and focus inside the page survives). The
// sources' view-level banners sit above the tabs; a PR's sections are
// folded inside its row; a `panel` action opens the source's drill-in page
// over the view. A PR joined to work items carries a chip per item key
// that opens the item in Work, and the actions other sources offer on PR
// rows sit with the PR's own.

import type { ActionTarget } from "./needs_you";
import { withPanels } from "./panel_view";
import { prId, type PrGroup, type PrRow, type PrsModel, type PrsTab } from "./prs_model";
import { SectionsBlock, paintBanners } from "./sections";
import { prItemActionTargets, type JoinHow, type MergeSource } from "./work_merge";
import {
  ActionsCell,
  ViewFrame,
  chipEl,
  fillLines,
  openChecked,
  paintEmpty,
  placeAt,
  truncate,
  type SourceViewDeps,
} from "./view_common";

export type { PrsTab } from "./prs_model";

/** Tab order and labels. */
export const PRS_TABS: { id: PrsTab; label: string }[] = [
  { id: "mine", label: "My PRs" },
  { id: "reviewing", label: "Reviewing" },
  { id: "all", label: "All" },
];

/** What the page paints: the model plus what the app knows around it. */
export interface PrsViewInput {
  model: PrsModel;
  /** Labels of the trusted sources serving PRs (empty: none does). */
  servedBy: string[];
  /** Why no source serves it, when none does (`unservedText`). Absent:
   *  the general line. */
  unserved?: string;
  /** Bumped per source whenever its payload was re-read, so a row's
   *  actions are read again after a poll. */
  versions: Record<string, number>;
  /** Per PR id: the work items it joined (the row's item-key chips). */
  workItems?: ReadonlyMap<string, readonly { rowId: string; key: string; title: string; how: JoinHow }[]>;
  /** The listed sources: the item actions they offer on PR rows. */
  sources?: readonly MergeSource[];
}

/** What the PRs page needs beyond any view: opening an item in Work. */
export interface PrsViewDeps extends SourceViewDeps {
  /** Open another view at an item (an item-key chip → Work); false when
   *  that view is not available. */
  openView?(view: string, key: string): boolean;
}

/** An item-key chip's tooltip: the item, and how the PR joined it. */
export function itemChipTitle(item: { key: string; title: string; how: JoinHow }): string {
  const how = item.how === "linked" ? "linked by the item" : item.how === "branch" ? "matched by its branch" : `matched by ${item.how}`;
  return `${item.title === item.key ? item.key : `${item.key} ${item.title}`} (${how}); show it in Work`;
}

interface GroupRefs {
  el: HTMLElement;
  head: HTMLButtonElement;
  label: HTMLElement;
  count: HTMLElement;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  title: HTMLButtonElement;
  meta: HTMLElement;
  reasons: HTMLElement;
  chips: HTMLElement;
  sections: SectionsBlock;
  actions: ActionsCell;
  sig: string;
  row: PrRow | null;
}

/** The empty-state line: why there is nothing to show. */
export function prsEmptyText(input: PrsViewInput, tab: PrsTab): string {
  if (input.servedBy.length === 0) {
    return input.unserved ?? "No trusted source serves pull requests. Add one as a source command in a project's chappa.yml, or attach one in Settings ▸ Sources.";
  }
  if (!input.model.hasData) return `Waiting for the first answer from ${input.servedBy.join(", ")}.`;
  if (tab === "mine") return "None of your pull requests is open or recent.";
  if (tab === "reviewing") return "No review is required of you, and none you took part in is open.";
  return "No open pull request lists you as a reviewer.";
}

/** The action targets of a merged row: every origin that carried actions. */
export function prTargets(row: PrRow): ActionTarget[] {
  return row.origins
    .filter((o) => o.hasActions)
    .map((o) => ({ sourceId: o.sourceId, sourceLabel: o.sourceLabel, view: "prs", key: o.key }));
}

/** Whether `row` is the PR a `forge:repo:number` key names: one of its
 *  sources' keys exactly, or the same PR as forges compare them (forge and
 *  repo case folded). */
export function prMatchesKey(row: PrRow, key: string): boolean {
  if (row.origins.some((o) => o.key === key)) return true;
  const first = key.indexOf(":");
  const last = key.lastIndexOf(":");
  if (first <= 0 || last <= first) return false;
  return prId(key.slice(0, first), key.slice(first + 1, last), key.slice(last + 1)) === row.id;
}

/** Which tab shows a PR, looked for in this order. */
const FOCUS_TABS: PrsTab[] = ["reviewing", "mine", "all"];

const CSS = `
.chappa-pr-banners:empty{display:none;}
.chappa-pr-row.focused{box-shadow:0 0 0 1px #5ea6ff;}
.chappa-pr-row .chappa-secs:empty{display:none;}
.chappa-pr-row .chappa-sec{margin:2px 0 0;}
.chappa-pr-item{cursor:pointer;background:none;}
.chappa-pr-item:hover{border-color:#5ea6ff;}
.chappa-pr-item:disabled{cursor:default;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = CSS;
  document.head.appendChild(el);
}

export class PrsView {
  private readonly frame: ViewFrame;
  private readonly tabs: Record<PrsTab, HTMLButtonElement>;
  private tab: PrsTab = "mine";
  /** Groups the user unfolded or folded, per tab; others keep their
   *  default. Kept for as long as the page lives (the session), not saved. */
  private readonly foldChoice = new Map<string, boolean>();
  private readonly groups = new Map<string, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private readonly bannersEl: HTMLElement;
  private input: PrsViewInput | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: SourceViewDeps;
  private focused: string | null = null;
  /** A focus asked for before the data arrived. */
  private pendingFocus: string | null = null;

  constructor(private readonly deps: PrsViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("PRs", "prs", () => deps.onClose());
    this.ui = withPanels(() => this.frame, deps);
    this.bannersEl = document.createElement("div");
    this.bannersEl.className = "chappa-pr-banners";
    this.frame.banners.after(this.bannersEl);
    const tabs = document.createElement("div");
    tabs.className = "chappa-sv-tabs";
    tabs.setAttribute("role", "tablist");
    const mk = (id: PrsTab): HTMLButtonElement => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "chappa-sv-tab";
      b.dataset.tab = id;
      b.setAttribute("role", "tab");
      b.addEventListener("click", () => this.selectTab(id));
      tabs.appendChild(b);
      return b;
    };
    this.tabs = { mine: mk("mine"), reviewing: mk("reviewing"), all: mk("all") };
    this.frame.headerSlot.appendChild(tabs);
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

  get currentTab(): PrsTab {
    return this.tab;
  }

  setActive(active: boolean): void {
    this.frame.setActive(active);
  }

  /** Where the view's arrow returns. */
  setBackTo(where: string): void {
    this.frame.setBackTo(where);
  }

  /**
   * Bring the PR `key` (`forge:repo:number`) into view: the tab that holds
   * it (Reviewing first, then My PRs, then All), its group unfolded,
   * scrolled to and marked. Answers whether it was found; asked before the
   * first answer arrived, it waits for it (and answers true).
   */
  focus(key: string): boolean {
    const model = this.input?.model;
    if (!model?.hasData) {
      this.pendingFocus = key;
      return true;
    }
    this.pendingFocus = null;
    for (const tab of FOCUS_TABS) {
      const group = model[tab].find((g) => g.rows.some((r) => prMatchesKey(r, key)));
      if (!group) continue;
      const row = group.rows.find((r) => prMatchesKey(r, key))!;
      this.frame.clearSubpages();
      this.foldChoice.set(`${tab}\u001f${group.id}`, false);
      this.tab = tab;
      this.focused = row.id;
      this.paint();
      this.rows.get(row.id)?.el.scrollIntoView?.({ block: "center" });
      return true;
    }
    return false;
  }

  selectTab(tab: PrsTab): void {
    if (tab === this.tab) return;
    this.tab = tab;
    this.paint();
  }

  update(input: PrsViewInput): void {
    this.input = input;
    this.paint();
    if (this.pendingFocus !== null && input.model.hasData) {
      const key = this.pendingFocus;
      if (!this.focus(key)) this.deps.say(`${key} is not in the PRs list.`, "error");
    }
  }

  dispose(): void {
    this.frame.dispose();
  }

  private paint(): void {
    const input = this.input;
    if (!input) return;
    const { model } = input;
    this.frame.paintStatus(model.freshness, model.needsUpdate, this.deps.now());
    const version = (sourceId: string): string => `${sourceId}:${input.versions[sourceId] ?? 0}`;
    paintBanners(
      this.bannersEl,
      this.ui,
      model.banners.map(({ sourceId, sourceLabel, banner }) => ({
        banner,
        ctx: { sourceId, target: { sourceId, sourceLabel, view: "prs", key: null }, version: version(sourceId) },
      })),
    );
    for (const { id, label } of PRS_TABS) {
      this.tabs[id].textContent = `${label} (${model.counts[id]})`;
      this.tabs[id].classList.toggle("active", id === this.tab);
      this.tabs[id].setAttribute("aria-selected", String(id === this.tab));
    }
    const groups = model[this.tab];
    const live = new Set<string>();
    groups.forEach((g, i) => {
      const refs = this.groupRefs(this.tab, g);
      this.paintGroup(refs, g);
      placeAt(this.frame.content, refs.el, i);
      g.rows.forEach((row, j) => {
        live.add(row.id);
        const r = this.rowRefs(row.id);
        this.paintRow(r, row, input);
        placeAt(refs.rows, r.el, j);
      });
      truncate(refs.rows, g.rows.length);
    });
    truncate(this.frame.content, groups.length);
    // Rows of the other tabs stay cached; only rows gone from every tab are dropped.
    const all = new Set([...model.mine, ...model.reviewing, ...model.all].flatMap((g) => g.rows.map((r) => r.id)));
    for (const id of [...this.rows.keys()]) if (!all.has(id)) this.rows.delete(id);
    for (const key of [...this.groups.keys()]) {
      const [tab, id] = key.split("\u001f");
      if (!model[tab as PrsTab].some((g) => g.id === id)) this.groups.delete(key);
    }
    if (this.focused !== null && !all.has(this.focused)) this.focused = null;
    for (const [id, r] of this.rows) r.el.classList.toggle("focused", id === this.focused);
    paintEmpty(
      this.frame.empty,
      groups.length === 0 ? prsEmptyText(input, this.tab) : "",
      input.servedBy.length === 0 ? this.deps.openSourcesSettings : undefined,
    );
  }

  private folded(tab: PrsTab, g: PrGroup): boolean {
    return this.foldChoice.get(`${tab}\u001f${g.id}`) ?? g.folded;
  }

  private groupRefs(tab: PrsTab, g: PrGroup): GroupRefs {
    const key = `${tab}\u001f${g.id}`;
    let refs = this.groups.get(key);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-sv-group";
    el.dataset.group = g.id;
    if (g.bucket) el.dataset.bucket = g.bucket;
    const head = document.createElement("button");
    head.type = "button";
    head.className = "chappa-sv-group-head";
    const label = document.createElement("span");
    label.className = "chappa-sv-group-label";
    const count = document.createElement("span");
    count.className = "chappa-sv-count";
    head.append(label, count);
    const rows = document.createElement("div");
    rows.className = "chappa-sv-rows";
    el.append(head, rows);
    head.addEventListener("click", () => {
      const now = this.folded(tab, this.currentGroup(tab, g.id) ?? g);
      this.foldChoice.set(key, !now);
      this.paint();
    });
    refs = { el, head, label, count, rows, sig: "" };
    this.groups.set(key, refs);
    return refs;
  }

  private currentGroup(tab: PrsTab, id: string): PrGroup | undefined {
    return this.input?.model[tab].find((g) => g.id === id);
  }

  private paintGroup(refs: GroupRefs, g: PrGroup): void {
    const folded = this.folded(this.tab, g);
    const sig = `${g.label}\u001f${g.rows.length}\u001f${folded}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.label.textContent = `${folded ? "▸" : "▾"} ${g.label}`;
    refs.count.textContent = String(g.rows.length);
    refs.head.setAttribute("aria-expanded", String(!folded));
    refs.rows.classList.toggle("folded", folded);
    refs.el.classList.toggle("folded", folded);
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = document.createElement("div");
    el.className = "chappa-sv-row chappa-pr-row";
    el.dataset.id = id;
    const top = document.createElement("div");
    top.className = "chappa-sv-top";
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-sv-title";
    top.appendChild(title);
    const meta = document.createElement("div");
    meta.className = "chappa-sv-meta";
    const reasons = document.createElement("div");
    reasons.className = "chappa-sv-reasons";
    const chips = document.createElement("div");
    chips.className = "chappa-sv-chips";
    const sections = new SectionsBlock(this.ui, { collapsedByDefault: true });
    const actions = new ActionsCell(this.ui);
    el.append(top, meta, reasons, chips, sections.el, actions.el);
    const r: RowRefs = { el, title, meta, reasons, chips, sections, actions, sig: "", row: null };
    title.addEventListener("mousedown", (e) => e.preventDefault());
    title.addEventListener("click", () => {
      const row = r.row;
      if (row?.url) openChecked(this.deps, row.primary.sourceId, row.url);
    });
    this.rows.set(id, r);
    return r;
  }

  private paintRow(r: RowRefs, row: PrRow, input: PrsViewInput): void {
    const versions = input.versions;
    r.row = row;
    // The sections draw their own actions; the row's cell draws the rest,
    // then what other sources offer on the PR.
    const own = row.sections.some((s) => (s.actions ?? []).length > 0) ? [...new Set(row.origins.flatMap((o) => o.actionIds))] : undefined;
    const offered = input.sources ? prItemActionTargets(row, input.sources) : [];
    const targets = [...(own?.length === 0 ? [] : prTargets(row).map((t) => (own ? { ...t, only: own } : t))), ...offered];
    const asked = [...new Set([...targets.map((t) => t.sourceId), ...offered.map((t) => t.owner)])];
    r.actions.load(targets, asked.map((id) => `${id}:${versions[id] ?? 0}`).join(","));
    const p = row.primary;
    r.sections.update(row.sections, {
      sourceId: p.sourceId,
      target: { sourceId: p.sourceId, sourceLabel: p.sourceLabel, view: "prs", key: p.key },
      version: `${p.sourceId}:${versions[p.sourceId] ?? 0}`,
    });
    const items = input.workItems?.get(row.id) ?? [];
    const sig = JSON.stringify([row.title, row.url, row.repo, row.number, row.author, row.branch, row.reasons, row.chips, row.bucket, row.primary.sourceLabel, items]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.bucket = row.bucket;
    r.title.textContent = row.title;
    r.title.title = row.url ? `Open ${row.url}` : row.title;
    r.title.disabled = !row.url;
    r.meta.textContent = [`${row.repo}${row.number}`, row.author, row.branch].filter(Boolean).join(" · ");
    fillLines(r.reasons, "chappa-sv-reason", row.reasons);
    r.chips.textContent = "";
    for (const chip of row.chips) r.chips.appendChild(chipEl(chip));
    for (const item of items) r.chips.appendChild(this.itemChip(item));
  }

  /** A work item the PR joined: its key, opening the item in Work. */
  private itemChip(item: { rowId: string; key: string; title: string; how: JoinHow }): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chappa-sv-chip chappa-pr-item";
    btn.dataset.tone = "info";
    btn.dataset.item = item.rowId;
    btn.dataset.how = item.how;
    btn.textContent = item.key;
    btn.title = itemChipTitle(item);
    btn.disabled = !this.deps.openView;
    btn.addEventListener("mousedown", (e) => e.preventDefault());
    btn.addEventListener("click", () => {
      if (this.deps.openView && !this.deps.openView("work", item.rowId)) this.deps.say("Work is not available.", "error");
    });
    return btn;
  }
}
