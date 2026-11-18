// The PRs page: the merged pull requests of every trusted source serving
// the view, in three tabs: My PRs and Reviewing grouped by bucket, All
// grouped by how I am on each review (team groups start folded). Each PR is
// one row of an aligned table (repo, number, title with author and branch,
// status chips, votes, comments, the work items it joined, age); cells wrap
// to a few lines rather than truncate, and rows are separated by rules, not
// boxed. A group's header is one compact line whose fold is remembered.
//
// A row is kept light: its only inline control is "Open" (its link, shown
// on hover and focus) and a "⋯" menu holding its page, its link and every
// action Rust judged for it, asked for only when the menu opens. Clicking
// the row opens the PR's page (everything about it, its sections and all
// its actions), which leaves through the shared back arrow.
//
// Built like the dashboard: the app hands it a plain model on every change
// and the page repaints only the rows whose content changed (a poll that
// changed one PR touches one row, and focus inside the page survives). The
// sources' view-level banners sit above the tabs; a `panel` action opens
// the source's drill-in page over the view. A PR joined to work items
// carries a chip per item key that opens the item in Work, and the actions
// other sources offer on PR rows sit with the PR's own.

import { mountPageHeader, type PageHeader } from "../page_header";
import type { ActionTarget } from "./needs_you";
import { withPanels } from "./panel_view";
import { prId, type PrGroup, type PrRow, type PrsModel, type PrsTab } from "./prs_model";
import { SectionsBlock, paintBanners } from "./sections";
import { prItemActionTargets, type JoinHow, type MergeSource } from "./work_merge";
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
  type Subpage,
} from "./view_common";

export type { PrsTab } from "./prs_model";

/** Tab order and labels. */
export const PRS_TABS: { id: PrsTab; label: string }[] = [
  { id: "mine", label: "My PRs" },
  { id: "reviewing", label: "Reviewing" },
  { id: "all", label: "All" },
];

/** The table's columns, in order (the last, unlabelled, holds the row's
 *  Open and its "⋯"). */
export const PR_COLUMNS = ["REPO", "#", "TITLE", "STATUS", "VOTES", "COMMENTS", "ITEMS", "AGE", ""];

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
  workItems?: ReadonlyMap<string, readonly WorkItemLink[]>;
  /** The listed sources: the item actions they offer on PR rows. */
  sources?: readonly MergeSource[];
}

type WorkItemLink = { rowId: string; key: string; title: string; how: JoinHow };

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

/** The muted second line of the REPO cell: the forge, and the project
 *  above the repo where the forge has one. */
export function prForgeLine(row: PrRow): string {
  return row.project ? `${row.forge} | ${row.project}` : row.forge;
}

/** The muted line under a PR's title: who opened it and its branches. */
export function prByLine(row: PrRow): string {
  return [row.author, row.branch].filter(Boolean).join(" | ");
}

interface GroupRefs {
  el: HTMLElement;
  head: ReturnType<typeof groupHead>;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  repo: HTMLElement;
  forge: HTMLElement;
  number: HTMLElement;
  title: HTMLButtonElement;
  by: HTMLElement;
  why: HTMLElement;
  status: HTMLElement;
  votes: HTMLElement;
  me: HTMLElement;
  comments: HTMLElement;
  items: HTMLElement;
  age: HTMLElement;
  open: HTMLButtonElement;
  more: HTMLButtonElement;
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

/** Every target of a PR's actions: its origins' own (without the actions
 *  its sections draw, when it has sections with actions and `withSections`
 *  is off), then what other sources offer on it. */
export function prAllTargets(row: PrRow, sources: readonly MergeSource[] | undefined, withSections: boolean): (ActionTarget & { only?: readonly string[] })[] {
  const own = !withSections && row.sections.some((s) => (s.actions ?? []).length > 0) ? [...new Set(row.origins.flatMap((o) => o.actionIds))] : undefined;
  const offered = sources ? prItemActionTargets(row, sources) : [];
  return [...(own?.length === 0 ? [] : prTargets(row).map((t) => (own ? { ...t, only: own } : t))), ...offered];
}

/** The version a PR's actions are cached under: every source asked. */
function versionOf(targets: (ActionTarget & { owner?: string })[], versions: Record<string, number>): string {
  const asked = [...new Set(targets.flatMap((t) => (t.owner ? [t.sourceId, t.owner] : [t.sourceId])))];
  return asked.map((id) => `${id}:${versions[id] ?? 0}`).join(",");
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

/** Where a group's fold is remembered. */
export const prGroupFoldKey = (tab: PrsTab, groupId: string): string => `prs-group:${tab}:${groupId}`;

const CSS = `
.chappa-pr-banners:empty{display:none;}
.chappa-source-view[data-view="prs"] .chappa-dt{--dt-cols:minmax(100px,1.1fr) 52px minmax(180px,3fr) minmax(100px,1.2fr) minmax(96px,1fr) 92px minmax(56px,.7fr) 44px 74px;}
.chappa-pr-item{cursor:pointer;background:none;}
.chappa-pr-item:hover{border-color:#5ea6ff;}
.chappa-pr-item:disabled{cursor:default;}
.chappa-pr-why{font:11px system-ui,sans-serif;color:#c9a35a;}
.chappa-pr-why:empty{display:none;}
.chappa-pr-votes[data-tone="ok"]{color:#3fb950;}
.chappa-pr-votes[data-tone="bad"]{color:#f85149;}
.chappa-pr-votes[data-tone="info"]{color:#5ea6ff;}
.chappa-pr-comments[data-for-me]{color:#f0b429;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = CSS;
  document.head.appendChild(el);
}

const div = (cls: string): HTMLElement => {
  const e = document.createElement("div");
  e.className = cls;
  return e;
};

export class PrsView {
  private readonly frame: ViewFrame;
  private readonly tabs: Record<PrsTab, HTMLButtonElement>;
  private tab: PrsTab = "mine";
  private readonly groups = new Map<string, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private readonly bannersEl: HTMLElement;
  private readonly table: HTMLElement;
  private readonly body: HTMLElement;
  private readonly menu: RowMenu;
  private input: PrsViewInput | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: PrsViewDeps;
  private focused: string | null = null;
  /** A focus asked for before the data arrived. */
  private pendingFocus: string | null = null;
  /** The PR page open over the view, if any. */
  private page: PrPage | null = null;

  constructor(private readonly deps: PrsViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("PRs", "prs", () => deps.onClose());
    this.ui = withPanels(() => this.frame, deps);
    this.menu = new RowMenu(this.frame.host, this.ui);
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
    this.table = div("chappa-dt");
    this.table.setAttribute("role", "table");
    this.body = div("chappa-dt-body");
    this.table.append(columnsHead(PR_COLUMNS), this.body);
    this.frame.content.appendChild(this.table);
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
    if (!active) this.menu.close();
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
      writeFold(prGroupFoldKey(tab, group.id), group.folded, false);
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
    this.menu.close();
    this.paint();
  }

  update(input: PrsViewInput): void {
    this.input = input;
    this.paint();
    this.repaintPage();
    if (this.pendingFocus !== null && input.model.hasData) {
      const key = this.pendingFocus;
      if (!this.focus(key)) this.deps.say(`${key} is not in the PRs list.`, "error");
    }
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
    groups.forEach((g, i) => {
      const refs = this.groupRefs(this.tab, g);
      this.paintGroup(refs, g);
      placeAt(this.body, refs.el, i);
      g.rows.forEach((row, j) => {
        const r = this.rowRefs(row.id);
        this.paintRow(r, row, input);
        placeAt(refs.rows, r.el, j);
      });
      truncate(refs.rows, g.rows.length);
    });
    truncate(this.body, groups.length);
    this.table.hidden = groups.length === 0;
    // Rows of the other tabs stay cached; only rows gone from every tab are dropped.
    const all = new Set([...model.mine, ...model.reviewing, ...model.all].flatMap((g) => g.rows.map((r) => r.id)));
    for (const id of [...this.rows.keys()]) if (!all.has(id)) this.rows.delete(id);
    for (const key of [...this.groups.keys()]) {
      const [tab, id] = key.split("\u001f");
      if (!model[tab as PrsTab].some((g) => g.id === id)) this.groups.delete(key);
    }
    if (this.menu.key !== null && !groups.some((g) => g.rows.some((r) => r.id === this.menu.key))) this.menu.close();
    if (this.focused !== null && !all.has(this.focused)) this.focused = null;
    for (const [id, r] of this.rows) r.el.classList.toggle("focused", id === this.focused);
    paintEmpty(
      this.frame.empty,
      groups.length === 0 ? prsEmptyText(input, this.tab) : "",
      input.servedBy.length === 0 ? this.deps.openSourcesSettings : undefined,
    );
  }

  private folded(tab: PrsTab, g: PrGroup): boolean {
    return readFold(prGroupFoldKey(tab, g.id), g.folded);
  }

  private groupRefs(tab: PrsTab, g: PrGroup): GroupRefs {
    const key = `${tab}\u001f${g.id}`;
    let refs = this.groups.get(key);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-dt-group";
    el.dataset.group = g.id;
    if (g.bucket) el.dataset.bucket = g.bucket;
    const head = groupHead(true, () => {
      const now = this.currentGroup(tab, g.id) ?? g;
      writeFold(prGroupFoldKey(tab, g.id), now.folded, !this.folded(tab, now));
      this.paint();
    });
    const rows = div("chappa-dt-rows");
    rows.setAttribute("role", "rowgroup");
    el.append(head.el, rows);
    refs = { el, head, rows, sig: "" };
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
    refs.head.paint(g.label, g.rows.length, folded);
    refs.rows.classList.toggle("folded", folded);
    refs.el.classList.toggle("folded", folded);
  }

  private rowRefs(id: string): RowRefs {
    let refs = this.rows.get(id);
    if (refs) return refs;
    const el = div("chappa-dt-row chappa-pr-row");
    el.dataset.id = id;
    el.setAttribute("role", "row");
    const cell = (cls: string): HTMLElement => {
      const c = div(`chappa-dt-cell ${cls}`);
      c.setAttribute("role", "cell");
      el.appendChild(c);
      return c;
    };
    const repoCell = cell("chappa-pr-c-repo");
    const repo = div("chappa-pr-repo");
    const forge = div("chappa-dt-sub");
    repoCell.append(repo, forge);
    const number = div("chappa-dt-num");
    cell("chappa-pr-c-number").appendChild(number);
    const titleCell = cell("chappa-pr-c-title");
    const title = document.createElement("button");
    title.type = "button";
    title.className = "chappa-dt-main chappa-sv-title";
    const by = div("chappa-dt-sub chappa-pr-by");
    const why = div("chappa-pr-why");
    titleCell.append(title, by, why);
    const status = div("chappa-dt-chips chappa-sv-chips");
    cell("chappa-pr-c-status").appendChild(status);
    const votesCell = cell("chappa-pr-c-votes");
    const votes = div("chappa-pr-votes");
    const me = div("chappa-dt-sub");
    votesCell.append(votes, me);
    const comments = div("chappa-pr-comments");
    cell("chappa-pr-c-comments").appendChild(comments);
    const items = div("chappa-dt-chips");
    cell("chappa-pr-c-items").appendChild(items);
    const age = div("chappa-dt-sub");
    cell("chappa-pr-c-age").appendChild(age);
    const acts = cell("chappa-dt-acts");
    const r = {} as RowRefs;
    const open = openButton("", () => {
      if (r.row?.url) openChecked(this.deps, r.row.primary.sourceId, r.row.url);
    });
    const more = RowMenu.button((btn) => this.toggleMenu(r, btn));
    acts.append(open, more);
    Object.assign(r, { el, repo, forge, number, title, by, why, status, votes, me, comments, items, age, open, more, sig: "", row: null });
    el.addEventListener("click", (e) => {
      if (r.row && rowClickOpens(e, title)) this.openPage(r.row.id);
    });
    this.rows.set(id, r);
    return r;
  }

  private toggleMenu(r: RowRefs, btn: HTMLButtonElement): void {
    const row = r.row;
    const input = this.input;
    if (!row || !input) return;
    const targets = prAllTargets(row, input.sources, false);
    const entries = [{ label: "Open details", run: () => this.openPage(row.id) }];
    this.menu.toggle(row.id, btn, entries, targets, versionOf(targets, input.versions));
  }

  private paintRow(r: RowRefs, row: PrRow, input: PrsViewInput): void {
    r.row = row;
    if (this.menu.key === row.id) {
      const targets = prAllTargets(row, input.sources, false);
      this.menu.refresh(row.id, targets, versionOf(targets, input.versions));
    }
    const items = input.workItems?.get(row.id) ?? [];
    const age = shortAge(row.ageUtc, this.deps.now());
    const sig = JSON.stringify([row.title, row.url, row.repo, row.forge, row.project, row.number, row.author, row.branch, row.why, row.status, row.votes, row.comments, row.bucket, items, age]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.bucket = row.bucket;
    r.repo.textContent = row.repo;
    r.forge.textContent = prForgeLine(row);
    r.number.textContent = row.number;
    r.title.textContent = row.title;
    r.title.title = `Show ${row.repo}${row.number}`;
    r.by.textContent = prByLine(row);
    r.why.textContent = row.why.join(" · ");
    r.status.textContent = "";
    for (const c of row.status) r.status.appendChild(chipEl(c));
    r.votes.textContent = row.votes.summary;
    r.votes.dataset.tone = row.votes.tone;
    r.votes.title = row.votes.title;
    r.me.textContent = row.votes.me ?? "";
    r.comments.textContent = row.comments ? row.comments.text : "";
    if (row.comments?.forMe) {
      r.comments.dataset.forMe = String(row.comments.forMe);
      r.comments.title = `${row.comments.forMe} waiting for your reply`;
    } else {
      delete r.comments.dataset.forMe;
      r.comments.title = "";
    }
    r.items.textContent = "";
    for (const item of items) r.items.appendChild(this.itemChip(item));
    r.age.textContent = age ?? "";
    r.age.title = row.ageUtc ? new Date(row.ageUtc).toLocaleString() : "";
    r.open.hidden = !row.url;
    r.open.title = row.url ? `Open ${row.url}` : "";
  }

  /** A work item the PR joined: its key, opening the item in Work. */
  itemChip(item: WorkItemLink): HTMLButtonElement {
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
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (this.deps.openView && !this.deps.openView("work", item.rowId)) this.deps.say("Work is not available.", "error");
    });
    return btn;
  }

  // ---- the PR's page ------------------------------------------------

  private findRow(id: string): PrRow | null {
    const model = this.input?.model;
    if (!model) return null;
    for (const g of [...model.mine, ...model.reviewing, ...model.all]) {
      const row = g.rows.find((r) => r.id === id);
      if (row) return row;
    }
    return null;
  }

  /** The PR page on screen, or null. */
  get openPrPage(): PrPage | null {
    return this.page && this.frame.subpages.includes(this.page) ? this.page : null;
  }

  openPage(id: string): void {
    const row = this.findRow(id);
    if (!row) return;
    this.menu.close();
    this.frame.clearSubpages();
    const page = new PrPage(this.ui, this, row, () => this.frame.popSubpage());
    this.page = page;
    this.frame.pushSubpage(page);
    this.repaintPage();
  }

  private repaintPage(): void {
    const page = this.openPrPage;
    if (!page) {
      this.page = null;
      return;
    }
    const input = this.input;
    if (!input) return;
    page.update(this.findRow(page.id), input);
  }

  /** A PR link or the PR's page link, through the checked opener. */
  open(sourceId: string, url: string): void {
    openChecked(this.deps, sourceId, url);
  }
}

/** A PR's page: every fact about it, its sections unfolded, all its
 *  actions (its own, its sections', and what other sources offer). */
class PrPage implements Subpage {
  readonly el: HTMLElement;
  readonly id: string;
  readonly sourceId: string;
  private readonly header: PageHeader;
  private readonly info: HTMLElement;
  private readonly actions: ActionsCell;
  private readonly sections: SectionsBlock;
  private sig = "";

  constructor(
    deps: SourceViewDeps,
    private readonly view: PrsView,
    row: PrRow,
    onBack: () => void,
  ) {
    this.id = row.id;
    this.sourceId = row.primary.sourceId;
    this.el = div("chappa-pr-page");
    this.el.dataset.id = row.id;
    const head = div("chappa-sv-header");
    this.header = mountPageHeader(head, { title: `${row.number} ${row.title}`, backTo: "PRs", onBack });
    const body = div("chappa-sv-body chappa-pr-page-body");
    this.info = div("chappa-pr-page-info");
    this.actions = new ActionsCell(deps);
    this.actions.el.classList.add("chappa-pr-page-actions");
    this.sections = new SectionsBlock(deps);
    body.append(this.info, this.actions.el, this.sections.el);
    this.el.append(head, body);
  }

  get title(): string {
    return this.header.title.textContent ?? "";
  }

  setBackTo(where: string): void {
    this.header.setBackTo(where);
  }

  dispose(): void {
    this.el.remove();
  }

  gone(): boolean {
    return this.sig === "gone";
  }

  update(row: PrRow | null, input: PrsViewInput): void {
    const version = (id: string): string => `${id}:${input.versions[id] ?? 0}`;
    if (!row) {
      if (this.sig === "gone") return;
      this.sig = "gone";
      this.info.textContent = "";
      const gone = div("chappa-dt-page-gone");
      gone.textContent = "This pull request is no longer listed.";
      this.info.appendChild(gone);
      this.actions.load([], version(this.sourceId));
      this.sections.update([], { sourceId: this.sourceId, target: null, version: version(this.sourceId) });
      return;
    }
    const targets = prAllTargets(row, input.sources, false);
    this.actions.load(targets, versionOf(targets, input.versions));
    const p = row.primary;
    this.sections.update(row.sections, {
      sourceId: p.sourceId,
      target: { sourceId: p.sourceId, sourceLabel: p.sourceLabel, view: "prs", key: p.key },
      version: version(p.sourceId),
    });
    const items = input.workItems?.get(row.id) ?? [];
    const sig = JSON.stringify([row, items]);
    if (sig === this.sig) return;
    this.sig = sig;
    this.header.setTitle(`${row.number} ${row.title}`);
    this.info.textContent = "";
    const grid = document.createElement("dl");
    grid.className = "chappa-dt-page-grid";
    const add = (label: string, value: string | HTMLElement[]): void => {
      if (value === "" || (Array.isArray(value) && value.length === 0)) return;
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.dataset.field = label;
      if (typeof value === "string") dd.textContent = value;
      else dd.append(...value);
      grid.append(dt, dd);
    };
    add("Repo", `${row.repo} (${prForgeLine(row)})`);
    add("Number", row.number);
    add("Author", row.author);
    add("Branch", row.branch ?? "");
    add("Status", row.status.map(chipEl));
    add("Why", row.why.join("\n"));
    add("Votes", [row.votes.summary, row.votes.me].filter(Boolean).join("; "));
    add("Reviewers", row.reviewers.map((r) => `${r.name}${r.isGroup ? " (team)" : ""}: ${r.vote === "none" ? "no vote" : r.vote}${r.required ? ", required" : ""}`).join("\n"));
    add("Comments", row.comments ? `${row.comments.text}${row.comments.forMe ? `, ${row.comments.forMe} for me` : ""}` : "");
    add("Work items", items.map((i) => this.view.itemChip(i)));
    add("Updated", row.ageUtc ? new Date(row.ageUtc).toLocaleString() : "");
    if (row.url) {
      const link = document.createElement("button");
      link.type = "button";
      link.className = "chappa-sec-link";
      link.textContent = row.url;
      link.title = `Open ${row.url}`;
      link.addEventListener("click", () => this.view.open(row.primary.sourceId, row.url));
      add("Link", [link]);
    }
    add("Source", row.origins.map((o) => o.sourceLabel).join(", "));
    this.info.appendChild(grid);
  }
}
