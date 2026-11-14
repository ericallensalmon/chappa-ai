// The PRs page: the merged pull requests of every trusted source serving
// the view, in three tabs: My PRs and Reviewing grouped by bucket, All
// grouped by how I am on each review (team groups start folded). Built like
// the dashboard: the app hands it a plain model on every change and the
// page repaints only the rows whose content changed (a poll that changed
// one PR touches one row, and focus inside the page survives).

import type { ActionTarget } from "./needs_you";
import type { PrGroup, PrRow, PrsModel, PrsTab } from "./prs_model";
import {
  ActionsCell,
  ViewFrame,
  chipEl,
  fillLines,
  openChecked,
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
  /** Bumped per source whenever its payload was re-read, so a row's
   *  actions are read again after a poll. */
  versions: Record<string, number>;
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
  actions: ActionsCell;
  sig: string;
  row: PrRow | null;
}

/** The empty-state line: why there is nothing to show. */
export function prsEmptyText(input: PrsViewInput, tab: PrsTab): string {
  if (input.servedBy.length === 0) {
    return "No trusted source serves pull requests. Add one as a source command in a project's chappa.yml, or attach one in Settings ▸ Sources.";
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

export class PrsView {
  private readonly frame: ViewFrame;
  private readonly tabs: Record<PrsTab, HTMLButtonElement>;
  private tab: PrsTab = "mine";
  /** Groups the user unfolded or folded, per tab; others keep their
   *  default. Kept for as long as the page lives (the session), not saved. */
  private readonly foldChoice = new Map<string, boolean>();
  private readonly groups = new Map<string, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private input: PrsViewInput | null = null;

  constructor(private readonly deps: SourceViewDeps) {
    this.frame = new ViewFrame("PRs", "prs", () => deps.onClose());
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

  get active(): boolean {
    return this.frame.active;
  }

  get currentTab(): PrsTab {
    return this.tab;
  }

  setActive(active: boolean): void {
    this.frame.setActive(active);
  }

  selectTab(tab: PrsTab): void {
    if (tab === this.tab) return;
    this.tab = tab;
    this.paint();
  }

  update(input: PrsViewInput): void {
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
        this.paintRow(r, row, input.versions);
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
    this.frame.empty.textContent = groups.length === 0 ? prsEmptyText(input, this.tab) : "";
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
    const actions = new ActionsCell(this.deps);
    el.append(top, meta, reasons, chips, actions.el);
    const r: RowRefs = { el, title, meta, reasons, chips, actions, sig: "", row: null };
    title.addEventListener("mousedown", (e) => e.preventDefault());
    title.addEventListener("click", () => {
      const row = r.row;
      if (row?.url) openChecked(this.deps, row.primary.sourceId, row.url);
    });
    this.rows.set(id, r);
    return r;
  }

  private paintRow(r: RowRefs, row: PrRow, versions: Record<string, number>): void {
    r.row = row;
    const targets = prTargets(row);
    r.actions.load(targets, targets.map((t) => `${t.sourceId}:${versions[t.sourceId] ?? 0}`).join(","));
    const sig = JSON.stringify([row.title, row.url, row.repo, row.number, row.author, row.branch, row.reasons, row.chips, row.bucket, row.primary.sourceLabel]);
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
  }
}
