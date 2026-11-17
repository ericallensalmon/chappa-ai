// The Environment page: one block per trusted source serving the view (each
// is its own stack). A block is a header (the stack's name, its counts as
// toned chips, its facts as one compact line, its banners and alerts, and
// the stack-wide actions) over a dense table: one line per component,
// grouped as the source groups them, each group folding away under a header
// that says how many of its components are up and carries the group's own
// actions. The app hands the page a plain model on every change and only
// the rows whose content changed are repainted, so a poll does not flicker
// and focus inside the page survives.
//
// A row is kept light so a stack of sixty components stays smooth: its
// actions are only asked for when the pointer or the keyboard reaches it
// (they show on hover and in its "⋯" menu), and everything else about the
// component (hint, badges, restarts, exit code, every link, its sections,
// every action) is on the page it opens when clicked, which leaves through
// the shared back arrow.
//
// Components that are not deployed are hidden behind a toggle per stack;
// shown, they sit dimmed in their own groups. The toggle and each group's
// fold are remembered per stack.
//
// Banners are drawn by the shared section renderer with their actions
// judged as the view's own; the facts' sections, with their actions, are on
// the "Details" page the compact line opens.

import { isBackKey, mountPageHeader, type PageHeader } from "../page_header";
import { readCollapsed, writeCollapsed } from "../sidebar_sections";
import type { ActionTarget } from "./needs_you";
import { groupCountText, type EnvAlert, type EnvBlock, type EnvGroup, type EnvRow, type EnvironmentModel } from "./environment_model";
import { withPanels } from "./panel_view";
import { SectionsBlock, paintBanners } from "./sections";
import type { Section } from "./types";
import { ActionsCell, ViewFrame, chipEl, openChecked, paintEmpty, placeAt, truncate, type SourceViewDeps, type Subpage } from "./view_common";

export interface EnvironmentViewInput {
  model: EnvironmentModel;
  /** Labels of the trusted sources serving the view (empty: none does). */
  servedBy: string[];
  /** Bumped per source whenever its payload was re-read. */
  versions: Record<string, number>;
}

/** The table's columns, in order (the last, unlabelled, holds the row's
 *  actions). */
export const ENV_COLUMNS = ["STATE", "COMPONENT", "MODE / SOURCE", "UPTIME", "ENDPOINT", "LINKS", ""];

export const ENVIRONMENT_CSS = `
.chappa-source-view[data-view="environment"]{
  --env-fg:#e6e8ec;--env-muted:#8b949e;--env-faint:#6b7280;--env-line:#22252a;--env-head-bg:#151820;
  --env-row-hover:#14171c;--env-bg:#0d0e10;--env-pop-bg:#16181d;--env-ok:#3fb950;--env-warn:#f0b429;--env-bad:#f85149;--env-link:#5ea6ff;
}
[data-theme="light"] .chappa-source-view[data-view="environment"]{
  --env-fg:#1f2328;--env-muted:#59636e;--env-faint:#818b98;--env-line:#d1d9e0;--env-head-bg:#eef1f4;
  --env-row-hover:#f3f5f7;--env-bg:#ffffff;--env-pop-bg:#ffffff;--env-ok:#1a7f37;--env-warn:#9a6700;--env-bad:#cf222e;--env-link:#0969da;
}
.chappa-env-block{margin-bottom:22px;}
.chappa-env-head{display:flex;flex-direction:column;gap:6px;margin:4px 0 8px;}
.chappa-env-titleline{display:flex;align-items:center;gap:8px;flex-wrap:wrap;min-width:0;}
.chappa-env-source{font:600 14px system-ui,sans-serif;color:var(--env-fg);}
.chappa-env-chips{display:flex;flex-wrap:wrap;gap:4px;}
.chappa-env-chips .chappa-sv-chip{font-size:11px;padding:1px 8px;}
.chappa-env-stack-actions{margin-left:auto;}
.chappa-env-factsline{display:flex;flex-wrap:wrap;gap:2px 14px;align-items:baseline;font:12px system-ui,sans-serif;color:var(--env-muted);}
.chappa-env-factsline:empty{display:none;}
.chappa-env-fact b{font-weight:600;color:var(--env-fg);margin-right:4px;}
.chappa-env-details-link{border:0;background:none;padding:0;color:var(--env-link);font:12px system-ui,sans-serif;cursor:pointer;}
.chappa-env-alerts,.chappa-env-banners{display:flex;flex-direction:column;gap:4px;}
.chappa-env-alerts:empty,.chappa-env-banners:empty,.chappa-env-wait:empty{display:none;}
.chappa-env-alert{font:12px system-ui,sans-serif;padding:3px 8px;border-radius:5px;border:1px solid #5c4813;color:#f0d68a;background:#1d1708;}
.chappa-env-alert[data-severity="bad"]{border-color:#5c1f1c;color:#ffb3ad;background:#1f0d0c;}
.chappa-env-wait{font:12px system-ui,sans-serif;color:var(--env-faint);}
.chappa-env-toggle{display:flex;align-items:center;gap:6px;border:0;background:none;padding:2px 0;color:var(--env-muted);font:12px system-ui,sans-serif;cursor:pointer;}
.chappa-env-toggle::before{content:"";width:11px;height:11px;border:1px solid var(--env-muted);border-radius:2px;}
.chappa-env-toggle[aria-pressed="true"]::before{background:var(--env-link);border-color:var(--env-link);}
.chappa-env-toggle:hover{color:var(--env-fg);}
.chappa-env-grid{display:grid;grid-template-columns:118px minmax(160px,2.4fr) minmax(110px,1.3fr) 72px 92px minmax(110px,1.6fr) 28px;align-items:center;column-gap:10px;}
.chappa-env-cols{position:sticky;top:-10px;z-index:2;background:var(--env-bg);padding:6px 8px;border-bottom:1px solid var(--env-line);font:600 10px ui-monospace,monospace;letter-spacing:.06em;color:var(--env-muted);}
.chappa-env-group{margin-top:6px;}
.chappa-env-group-head{display:flex;align-items:center;gap:8px;padding:4px 8px;background:var(--env-head-bg);border-left:3px solid var(--env-link);border-radius:3px;}
.chappa-env-group-toggle{border:0;background:none;padding:0;color:var(--env-fg);font:600 13px system-ui,sans-serif;cursor:pointer;text-align:left;}
.chappa-env-group-count{font-weight:400;color:var(--env-muted);}
.chappa-env-group-actions{margin-left:auto;}
.chappa-env-group.folded .chappa-env-rows{display:none;}
.chappa-env-row{position:relative;padding:5px 8px;border-bottom:1px solid var(--env-line);cursor:pointer;min-height:34px;}
.chappa-env-row:hover{background:var(--env-row-hover);}
.chappa-env-row.dim{opacity:.55;}
.chappa-env-row.focused{outline:1px solid var(--env-link);outline-offset:-1px;}
.chappa-env-row[data-alert="bad"]{box-shadow:inset 3px 0 0 var(--env-bad);}
.chappa-env-row[data-alert="warn"]{box-shadow:inset 3px 0 0 var(--env-warn);}
.chappa-env-cell{min-width:0;overflow:hidden;}
.chappa-env-line{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-env-pill{display:inline-block;max-width:100%;box-sizing:border-box;padding:1px 8px;border:1px solid var(--env-faint);border-radius:10px;font:600 10px ui-monospace,monospace;text-transform:uppercase;color:var(--env-muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;vertical-align:middle;}
.chappa-env-pill[data-tone="ok"]{color:var(--env-ok);border-color:var(--env-ok);}
.chappa-env-pill[data-tone="warn"]{color:var(--env-warn);border-color:var(--env-warn);}
.chappa-env-pill[data-tone="bad"]{color:var(--env-bad);border-color:var(--env-bad);}
.chappa-env-pill[data-tone="muted"]{border-style:dashed;}
.chappa-env-namebar{display:flex;align-items:center;gap:6px;min-width:0;}
.chappa-env-name{flex:0 1 auto;min-width:0;border:0;background:none;padding:0;color:var(--env-fg);font:600 13px ui-monospace,monospace;text-align:left;cursor:pointer;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-env-name:focus-visible{outline:1px solid var(--env-link);}
.chappa-env-badges{display:flex;gap:4px;flex:none;}
.chappa-env-badges:empty{display:none;}
.chappa-env-sub{font:11px system-ui,sans-serif;color:var(--env-muted);}
.chappa-env-mode{font:600 11px ui-monospace,monospace;color:var(--env-fg);}
.chappa-env-ref{font:10px ui-monospace,monospace;color:var(--env-muted);}
.chappa-env-uptime{font:11px ui-monospace,monospace;color:var(--env-fg);}
.chappa-env-links{display:flex;flex-direction:column;gap:1px;}
.chappa-env-link{display:block;max-width:100%;border:0;background:none;padding:0;color:var(--env-link);font:10px ui-monospace,monospace;cursor:pointer;text-align:left;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-env-link:hover{text-decoration:underline;}
.chappa-env-more{border:1px solid transparent;border-radius:4px;background:none;color:var(--env-muted);font:14px system-ui,sans-serif;line-height:1;padding:1px 5px;cursor:pointer;}
.chappa-env-more:hover,.chappa-env-more:focus-visible,.chappa-env-more[aria-expanded="true"]{border-color:var(--env-line);color:var(--env-fg);}
.chappa-env-inline{position:absolute;right:40px;top:50%;transform:translateY(-50%);display:none;background:var(--env-pop-bg);padding:2px 4px;border:1px solid var(--env-line);border-radius:5px;}
.chappa-env-row:hover .chappa-env-inline,.chappa-env-row:focus-within .chappa-env-inline{display:block;}
.chappa-env-inline .chappa-sv-actions:empty{display:none;}
.chappa-env-menu{position:fixed;z-index:50;min-width:150px;padding:6px;background:var(--env-pop-bg);border:1px solid var(--env-line);border-radius:6px;box-shadow:0 6px 18px rgba(0,0,0,.35);}
.chappa-env-menu .chappa-sv-actions{flex-direction:column;align-items:stretch;}
.chappa-env-menu-empty{font:12px system-ui,sans-serif;color:var(--env-faint);padding:2px 4px;}
.chappa-env-page-body{display:flex;flex-direction:column;gap:10px;}
.chappa-env-page-state{display:flex;align-items:center;gap:8px;font:12px system-ui,sans-serif;color:var(--env-muted);}
.chappa-env-page-grid{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;font:12px system-ui,sans-serif;color:var(--env-fg);}
.chappa-env-page-grid dt{color:var(--env-muted);}
.chappa-env-page-grid dd{margin:0;overflow-wrap:anywhere;}
.chappa-env-page-links{display:flex;flex-wrap:wrap;gap:6px;}
.chappa-env-page-links .chappa-env-link{font-size:12px;}
.chappa-env-gone{font:12px system-ui,sans-serif;color:var(--env-warn);}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = ENVIRONMENT_CSS;
  document.head.appendChild(el);
}

/** The empty-state line: why there is nothing to show. */
export function environmentEmptyText(input: EnvironmentViewInput): string {
  if (input.servedBy.length === 0) {
    return "No trusted source serves an environment. Add one as a source command in a project's chappa.yml, or attach one in Settings ▸ Sources.";
  }
  if (!input.model.hasData) return `Waiting for the first answer from ${input.servedBy.join(", ")}.`;
  return "";
}

/** The toggle's text for a block with `n` components not deployed. */
export function notDeployedToggleText(n: number): string {
  return `Show components that are not deployed (${n})`;
}

/** Where a stack's choices are remembered (the same store the sidebar's
 *  folds use): whether its not-deployed components show, and which of its
 *  groups are folded. */
export const notDeployedKey = (sourceId: string): string => `env-not-deployed:${sourceId}`;
export const groupFoldKey = (sourceId: string, groupId: string): string => `env-group:${sourceId}:${groupId}`;

interface BlockRefs {
  el: HTMLElement;
  source: HTMLElement;
  chips: HTMLElement;
  actions: ActionsCell;
  facts: HTMLElement;
  wait: HTMLElement;
  banners: HTMLElement;
  alerts: HTMLElement;
  toggle: HTMLButtonElement;
  table: HTMLElement;
  groups: HTMLElement;
  sig: string;
  block: EnvBlock | null;
}

interface GroupRefs {
  el: HTMLElement;
  toggle: HTMLButtonElement;
  actions: ActionsCell;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  pill: HTMLElement;
  name: HTMLButtonElement;
  badges: HTMLElement;
  sub: HTMLElement;
  mode: HTMLElement;
  ref: HTMLElement;
  uptime: HTMLElement;
  endpoint: HTMLElement;
  links: HTMLElement;
  more: HTMLButtonElement;
  /** Created on the first hover or focus: most rows are never reached. */
  inline: ActionsCell | null;
  sig: string;
  row: EnvRow | null;
  version: string;
}

const el = (tag: string, cls: string): HTMLElement => {
  const e = document.createElement(tag);
  e.className = cls;
  return e;
};

const rowTarget = (row: EnvRow): ActionTarget => ({ sourceId: row.sourceId, sourceLabel: row.sourceLabel, view: "environment", key: row.id });

/** A row's own actions: its `actions` (a section's are on its page). */
const rowOnly = (row: EnvRow): readonly string[] | undefined =>
  row.sections.some((s) => (s.actions ?? []).length > 0) ? row.actionIds : undefined;

export class EnvironmentView {
  private readonly frame: ViewFrame;
  private readonly blocks = new Map<string, BlockRefs>();
  private readonly groups = new Map<string, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  /** Per source, read from the store on first sight. */
  private readonly showNotDeployed = new Map<string, boolean>();
  private input: EnvironmentViewInput | null = null;
  /** A focus asked for before the row exists (the first answer pending). */
  private pendingFocus: { id: string; sourceId?: string } | null = null;
  private focusedKey: string | null = null;
  /** The one "⋯" menu, moved to whichever row opened it. */
  private menu: { el: HTMLElement; cell: ActionsCell; key: string; opener: HTMLButtonElement } | null = null;
  /** The component page open over the view, if any. */
  private detail: ComponentPage | null = null;
  private factsPage: FactsPage | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: SourceViewDeps;

  constructor(private readonly deps: SourceViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("Environment", "environment", () => deps.onClose());
    this.ui = withPanels(() => this.frame, deps);
    this.frame.host.addEventListener("keydown", this.onMenuKey, true);
    document.addEventListener("mousedown", this.onOutside, true);
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
    if (!active) this.closeMenu();
    this.frame.setActive(active);
  }

  /** Where the view's arrow returns. */
  setBackTo(where: string): void {
    this.frame.setBackTo(where);
  }

  update(input: EnvironmentViewInput): void {
    this.input = input;
    this.paint();
    this.repaintPages();
    if (this.pendingFocus) this.focus(this.pendingFocus.id, this.pendingFocus.sourceId);
  }

  dispose(): void {
    this.closeMenu();
    document.removeEventListener("mousedown", this.onOutside, true);
    this.frame.dispose();
  }

  /** Scroll to a component's row and mark it. Its group unfolds and, for a
   *  not-deployed one, its block shows those first. Without `sourceId` the
   *  first source with that component wins. Answers whether the row was
   *  found; a row not there yet is focused when it arrives. */
  focus(componentId: string, sourceId?: string): boolean {
    const blocks = this.input?.model.blocks.filter((b) => sourceId === undefined || b.sourceId === sourceId) ?? [];
    let found: { row: EnvRow; group: EnvGroup } | null = null;
    for (const b of blocks) {
      for (const g of b.groups) {
        const row = g.rows.find((r) => r.id === componentId);
        if (row) {
          found = { row, group: g };
          break;
        }
      }
      if (found) break;
    }
    if (!found) {
      // Before the first answer the row may yet arrive; after it, it is gone.
      this.pendingFocus = this.input?.model.hasData ? null : { id: componentId, ...(sourceId !== undefined ? { sourceId } : {}) };
      return false;
    }
    const { row, group } = found;
    this.pendingFocus = null;
    this.frame.clearSubpages();
    let repaint = false;
    if (row.notDeployed && !this.notDeployedShown(row.sourceId)) {
      this.setNotDeployed(row.sourceId, true);
      repaint = true;
    }
    if (readCollapsed(groupFoldKey(row.sourceId, group.id))) {
      writeCollapsed(groupFoldKey(row.sourceId, group.id), false);
      repaint = true;
    }
    if (repaint) this.paint();
    if (this.focusedKey) this.rows.get(this.focusedKey)?.el.classList.remove("focused");
    const refs = this.rows.get(row.key);
    if (!refs) return false;
    this.focusedKey = row.key;
    refs.el.classList.add("focused");
    refs.el.scrollIntoView?.({ block: "center" });
    return true;
  }

  private notDeployedShown(sourceId: string): boolean {
    let shown = this.showNotDeployed.get(sourceId);
    if (shown === undefined) {
      shown = readCollapsed(notDeployedKey(sourceId));
      this.showNotDeployed.set(sourceId, shown);
    }
    return shown;
  }

  private setNotDeployed(sourceId: string, shown: boolean): void {
    this.showNotDeployed.set(sourceId, shown);
    writeCollapsed(notDeployedKey(sourceId), shown);
  }

  private paint(): void {
    const input = this.input;
    if (!input) return;
    const { model } = input;
    this.frame.paintStatus(model.freshness, model.needsUpdate, this.deps.now());
    const liveBlocks = new Set<string>();
    const liveGroups = new Set<string>();
    const liveRows = new Set<string>();
    model.blocks.forEach((b, i) => {
      liveBlocks.add(b.sourceId);
      const version = `${b.sourceId}:${input.versions[b.sourceId] ?? 0}`;
      const refs = this.blockRefs(b.sourceId);
      this.paintBlock(refs, b, version);
      placeAt(this.frame.content, refs.el, i);
      const shown = this.notDeployedShown(b.sourceId);
      let gi = 0;
      for (const g of b.groups) {
        const rows = shown ? g.rows : g.rows.filter((r) => !r.notDeployed);
        if (rows.length === 0) continue;
        const gk = `${b.sourceId}\u001f${g.id}`;
        liveGroups.add(gk);
        const grefs = this.groupRefs(gk, b.sourceId, g.id);
        this.paintGroup(grefs, b, g, version);
        placeAt(refs.groups, grefs.el, gi++);
        rows.forEach((row, j) => {
          liveRows.add(row.key);
          const r = this.rowRefs(row.key);
          this.paintRow(r, row, version);
          placeAt(grefs.rows, r.el, j);
        });
        truncate(grefs.rows, rows.length);
      }
      truncate(refs.groups, gi);
      refs.table.hidden = gi === 0;
    });
    truncate(this.frame.content, model.blocks.length);
    for (const k of [...this.blocks.keys()]) if (!liveBlocks.has(k)) this.blocks.delete(k);
    for (const k of [...this.groups.keys()]) if (!liveGroups.has(k)) this.groups.delete(k);
    for (const k of [...this.rows.keys()]) if (!liveRows.has(k)) this.rows.delete(k);
    if (this.menu && !liveRows.has(this.menu.key)) this.closeMenu();
    paintEmpty(this.frame.empty, environmentEmptyText(input), input.servedBy.length === 0 ? this.deps.openSourcesSettings : undefined);
  }

  private blockRefs(sourceId: string): BlockRefs {
    let refs = this.blocks.get(sourceId);
    if (refs) return refs;
    const root = el("section", "chappa-env-block");
    root.dataset.source = sourceId;
    const head = el("div", "chappa-env-head");
    const titleline = el("div", "chappa-env-titleline");
    const source = el("span", "chappa-env-source");
    const chips = el("span", "chappa-env-chips");
    const actions = new ActionsCell(this.ui);
    actions.el.classList.add("chappa-env-stack-actions");
    titleline.append(source, chips, actions.el);
    const facts = el("div", "chappa-env-factsline");
    const wait = el("div", "chappa-env-wait");
    const banners = el("div", "chappa-env-banners");
    const alerts = el("div", "chappa-env-alerts");
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "chappa-env-toggle";
    toggle.addEventListener("mousedown", (e) => e.preventDefault());
    toggle.addEventListener("click", () => {
      this.setNotDeployed(sourceId, !this.notDeployedShown(sourceId));
      this.paint();
    });
    head.append(titleline, facts, wait, banners, alerts, toggle);
    const table = el("div", "chappa-env-table");
    table.setAttribute("role", "table");
    const cols = el("div", "chappa-env-cols chappa-env-grid");
    cols.setAttribute("role", "row");
    for (const name of ENV_COLUMNS) {
      const c = el("span", "chappa-env-col");
      c.setAttribute("role", "columnheader");
      c.textContent = name;
      cols.appendChild(c);
    }
    const groups = el("div", "chappa-env-groups");
    table.append(cols, groups);
    root.append(head, table);
    refs = { el: root, source, chips, actions, facts, wait, banners, alerts, toggle, table, groups, sig: "", block: null };
    this.blocks.set(sourceId, refs);
    return refs;
  }

  private paintBlock(refs: BlockRefs, b: EnvBlock, version: string): void {
    refs.block = b;
    // The view's own target: the stack-wide actions and its banners' (no key).
    const own: ActionTarget = { sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "environment", key: null };
    refs.actions.load(b.actionIds.length ? [own] : [], version, b.actionIds);
    paintBanners(refs.banners, this.ui, b.banners.map((banner) => ({ banner, ctx: { sourceId: b.sourceId, target: own, version } })));
    const shown = this.notDeployedShown(b.sourceId);
    const sig = JSON.stringify([b.sourceLabel, b.hasData, b.chips, b.factsLine, b.facts.length, b.alerts, b.notDeployed, shown]);
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.source.textContent = b.sourceLabel;
    refs.chips.textContent = "";
    for (const c of b.chips) refs.chips.appendChild(chipEl(c));
    refs.facts.textContent = "";
    for (const f of b.factsLine) {
      const pair = el("span", "chappa-env-fact");
      const label = document.createElement("b");
      label.textContent = `${f.label}:`;
      pair.append(label, f.value);
      pair.title = `${f.section}: ${f.label} ${f.value}`;
      refs.facts.appendChild(pair);
    }
    if (b.facts.length) {
      const more = document.createElement("button");
      more.type = "button";
      more.className = "chappa-env-details-link";
      more.textContent = "Details";
      more.title = "Every fact of the stack, with its actions";
      more.addEventListener("click", () => this.openFacts(b.sourceId));
      refs.facts.appendChild(more);
    }
    refs.wait.textContent = b.hasData ? "" : `Waiting for the first answer from ${b.sourceLabel}.`;
    paintAlerts(refs.alerts, b.alerts);
    refs.toggle.hidden = b.notDeployed === 0;
    refs.toggle.textContent = notDeployedToggleText(b.notDeployed);
    refs.toggle.setAttribute("aria-pressed", String(shown));
  }

  private groupRefs(key: string, sourceId: string, id: string): GroupRefs {
    let refs = this.groups.get(key);
    if (refs) return refs;
    const root = el("section", "chappa-env-group");
    root.dataset.group = id;
    const head = el("div", "chappa-env-group-head");
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "chappa-env-group-toggle";
    toggle.addEventListener("click", () => {
      writeCollapsed(groupFoldKey(sourceId, id), !readCollapsed(groupFoldKey(sourceId, id)));
      this.paint();
    });
    const actions = new ActionsCell(this.ui);
    actions.el.classList.add("chappa-env-group-actions");
    head.append(toggle, actions.el);
    const rows = el("div", "chappa-env-rows");
    rows.setAttribute("role", "rowgroup");
    root.append(head, rows);
    refs = { el: root, toggle, actions, rows, sig: "" };
    this.groups.set(key, refs);
    return refs;
  }

  private paintGroup(refs: GroupRefs, b: EnvBlock, g: EnvGroup, version: string): void {
    const target: ActionTarget = { sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "environment", key: g.id };
    refs.actions.load(g.actionIds.length ? [target] : [], version, g.actionIds);
    const folded = readCollapsed(groupFoldKey(b.sourceId, g.id));
    const count = groupCountText(g.rows);
    const sig = `${g.label}\u001f${count}\u001f${folded}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.toggle.textContent = "";
    const count_ = el("span", "chappa-env-group-count");
    count_.textContent = count;
    refs.toggle.append(`${folded ? "▸" : "▾"} ${g.label} `, count_);
    refs.toggle.setAttribute("aria-expanded", String(!folded));
    refs.el.classList.toggle("folded", folded);
  }

  private rowRefs(key: string): RowRefs {
    let refs = this.rows.get(key);
    if (refs) return refs;
    const root = el("div", "chappa-env-row chappa-env-grid");
    root.dataset.key = key;
    root.setAttribute("role", "row");
    const cell = (cls: string): HTMLElement => {
      const c = el("div", `chappa-env-cell ${cls}`);
      c.setAttribute("role", "cell");
      root.appendChild(c);
      return c;
    };
    const pill = el("span", "chappa-env-pill");
    cell("chappa-env-c-state").appendChild(pill);
    const comp = cell("chappa-env-c-component");
    const namebar = el("div", "chappa-env-namebar");
    const name = document.createElement("button");
    name.type = "button";
    name.className = "chappa-env-name";
    const badges = el("span", "chappa-env-badges");
    namebar.append(name, badges);
    const sub = el("span", "chappa-env-line chappa-env-sub");
    comp.append(namebar, sub);
    const srcCell = cell("chappa-env-c-source");
    const mode = el("span", "chappa-env-line chappa-env-mode");
    const ref = el("span", "chappa-env-line chappa-env-ref");
    srcCell.append(mode, ref);
    const uptime = el("span", "chappa-env-line chappa-env-uptime");
    cell("chappa-env-c-uptime").appendChild(uptime);
    const endpoint = el("span", "chappa-env-pill chappa-env-endpoint");
    cell("chappa-env-c-endpoint").appendChild(endpoint);
    const links = el("div", "chappa-env-links");
    cell("chappa-env-c-links").appendChild(links);
    const more = document.createElement("button");
    more.type = "button";
    more.className = "chappa-env-more";
    more.textContent = "⋯";
    more.setAttribute("aria-haspopup", "menu");
    more.setAttribute("aria-expanded", "false");
    cell("chappa-env-c-actions").appendChild(more);
    refs = { el: root, pill, name, badges, sub, mode, ref, uptime, endpoint, links, more, inline: null, sig: "", row: null, version: "" };
    const r = refs;
    // The row opens its page; its buttons do their own thing.
    root.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest("button") && e.target !== name && !name.contains(e.target as Node)) return;
      if (r.row) this.openDetail(r.row.key);
    });
    const reach = (): void => this.loadInline(r);
    root.addEventListener("mouseenter", reach);
    root.addEventListener("focusin", reach);
    more.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleMenu(r);
    });
    this.rows.set(key, refs);
    return refs;
  }

  /** The hover actions, asked for the first time the row is reached. */
  private loadInline(r: RowRefs): void {
    if (!r.row) return;
    if (!r.inline) {
      r.inline = new ActionsCell(this.ui);
      const wrap = el("div", "chappa-env-inline");
      wrap.appendChild(r.inline.el);
      wrap.addEventListener("click", (e) => e.stopPropagation());
      r.el.appendChild(wrap);
    }
    r.inline.load(r.row.hasActions ? [rowTarget(r.row)] : [], r.version, rowOnly(r.row));
  }

  private paintRow(r: RowRefs, row: EnvRow, version: string): void {
    r.row = row;
    r.version = version;
    // Rows already reached keep their actions current.
    if (r.inline) this.loadInline(r);
    if (this.menu?.key === row.key) this.menu.cell.load(row.hasActions ? [rowTarget(row)] : [], version, rowOnly(row));
    const sig = JSON.stringify([
      row.label, row.oneshot, row.state, row.tone, row.hint, row.uptime, row.source, row.sourceLine, row.endpoint, row.links, row.badges, row.alerts, row.notDeployed,
    ]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.id = row.id;
    r.el.dataset.state = row.state;
    r.el.dataset.tone = row.tone;
    r.el.classList.toggle("dim", row.notDeployed);
    const worst = row.alerts.some((a) => a.severity === "bad") ? "bad" : row.alerts.length ? "warn" : "";
    if (worst) r.el.dataset.alert = worst;
    else delete r.el.dataset.alert;
    r.pill.dataset.tone = row.tone;
    r.pill.textContent = row.stateText;
    r.pill.title = row.stateTitle;
    r.name.textContent = row.label;
    r.name.title = `${row.id === row.label ? row.label : `${row.label} (${row.id})`}: open its page`;
    r.badges.textContent = "";
    if (row.oneshot) r.badges.appendChild(chipEl({ text: "one-shot", tone: "muted", title: "Runs to completion (a migration, a seed job)" }));
    for (const a of row.alerts) {
      const c = chipEl({ text: a.severity === "bad" ? "alert" : "warning", tone: a.severity, title: a.text });
      c.classList.add("chappa-env-row-alert");
      r.badges.appendChild(c);
    }
    for (const chip of row.badges) r.badges.appendChild(chipEl(chip));
    r.sub.textContent = row.hint;
    r.sub.title = row.hint;
    r.mode.textContent = row.source?.mode ?? "";
    r.mode.title = row.source?.title ?? "";
    r.ref.textContent = row.sourceLine;
    r.ref.title = row.source?.title ?? "";
    r.uptime.textContent = row.uptime;
    r.uptime.title = row.details.join(" · ");
    r.endpoint.textContent = row.endpoint.text;
    r.endpoint.dataset.tone = row.endpoint.tone;
    r.endpoint.title = row.endpoint.title;
    r.links.textContent = "";
    for (const link of row.links) r.links.appendChild(this.linkEl(row.sourceId, link.url, link.url, link.browser));
  }

  private linkEl(sourceId: string, url: string, text: string, browser: boolean): HTMLButtonElement {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "chappa-env-link";
    b.dataset.url = url;
    if (browser) b.dataset.browser = "true";
    b.textContent = text;
    b.title = browser ? `Open ${url} in the browser` : `Open ${url}`;
    b.addEventListener("mousedown", (e) => e.preventDefault());
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      openChecked(this.deps, sourceId, url);
    });
    return b;
  }

  // ---- the "⋯" menu -------------------------------------------------

  private toggleMenu(r: RowRefs): void {
    if (this.menu?.key === r.row?.key) {
      this.closeMenu();
      return;
    }
    this.closeMenu();
    const row = r.row;
    if (!row) return;
    const menuEl = el("div", "chappa-env-menu");
    menuEl.setAttribute("role", "menu");
    menuEl.dataset.key = row.key;
    const cell = new ActionsCell(this.ui);
    const open = document.createElement("button");
    open.type = "button";
    open.className = "chappa-sv-action chappa-env-menu-open";
    open.textContent = "Open details";
    open.addEventListener("click", () => {
      this.closeMenu();
      this.openDetail(row.key);
    });
    const actions = el("div", "chappa-sv-actions");
    actions.appendChild(open);
    menuEl.append(actions, cell.el);
    menuEl.addEventListener("click", (e) => e.stopPropagation());
    const box = r.more.getBoundingClientRect();
    menuEl.style.top = `${Math.round(box.bottom + 2)}px`;
    menuEl.style.right = `${Math.max(8, Math.round((window.innerWidth || 0) - box.right))}px`;
    this.frame.host.appendChild(menuEl);
    this.menu = { el: menuEl, cell, key: row.key, opener: r.more };
    r.more.setAttribute("aria-expanded", "true");
    cell.load(row.hasActions ? [rowTarget(row)] : [], r.version, rowOnly(row));
    open.focus();
  }

  private closeMenu(): void {
    if (!this.menu) return;
    this.menu.opener.setAttribute("aria-expanded", "false");
    this.menu.el.remove();
    this.menu = null;
  }

  /** Escape closes the menu before it would leave the view. */
  private onMenuKey = (e: KeyboardEvent): void => {
    if (!this.menu || !isBackKey(e)) return;
    e.preventDefault();
    e.stopPropagation();
    const opener = this.menu.opener;
    this.closeMenu();
    opener.focus();
  };

  private onOutside = (e: MouseEvent): void => {
    if (!this.menu) return;
    const t = e.target as Node;
    if (this.menu.el.contains(t) || this.menu.opener.contains(t)) return;
    this.closeMenu();
  };

  // ---- pages ----------------------------------------------------------

  private findRow(key: string): EnvRow | null {
    for (const b of this.input?.model.blocks ?? []) {
      for (const g of b.groups) {
        const row = g.rows.find((r) => r.key === key);
        if (row) return row;
      }
    }
    return null;
  }

  private openDetail(key: string): void {
    const row = this.findRow(key);
    if (!row) return;
    this.closeMenu();
    this.frame.clearSubpages();
    const page = new ComponentPage(this.ui, this, row, () => this.frame.popSubpage());
    this.detail = page;
    this.frame.pushSubpage(page);
    page.update(row, this.versionOf(row.sourceId));
  }

  private openFacts(sourceId: string): void {
    const b = this.input?.model.blocks.find((x) => x.sourceId === sourceId);
    if (!b) return;
    this.frame.clearSubpages();
    const page = new FactsPage(this.ui, b, () => this.frame.popSubpage());
    this.factsPage = page;
    this.frame.pushSubpage(page);
    page.update(b, this.versionOf(sourceId));
  }

  private versionOf(sourceId: string): string {
    return `${sourceId}:${this.input?.versions[sourceId] ?? 0}`;
  }

  /** Keep an open page current with each poll. */
  private repaintPages(): void {
    if (this.detail && this.frame.subpages.includes(this.detail)) {
      this.detail.update(this.findRow(this.detail.key), this.versionOf(this.detail.sourceId));
    } else this.detail = null;
    if (this.factsPage && this.frame.subpages.includes(this.factsPage)) {
      const b = this.input?.model.blocks.find((x) => x.sourceId === this.factsPage!.sourceId) ?? null;
      this.factsPage.update(b, this.versionOf(this.factsPage.sourceId));
    } else this.factsPage = null;
  }

  /** Open a link of a component's page. */
  link(sourceId: string, url: string, text: string, browser: boolean): HTMLButtonElement {
    return this.linkEl(sourceId, url, text, browser);
  }
}

/** A component's page: everything about it, and all its actions. */
class ComponentPage implements Subpage {
  readonly el: HTMLElement;
  readonly key: string;
  readonly sourceId: string;
  private readonly header: PageHeader;
  private readonly body: HTMLElement;
  private readonly sections: SectionsBlock;
  private readonly actions: ActionsCell;
  private readonly info: HTMLElement;
  private sig = "";

  constructor(
    deps: SourceViewDeps,
    private readonly view: EnvironmentView,
    row: EnvRow,
    onBack: () => void,
  ) {
    this.key = row.key;
    this.sourceId = row.sourceId;
    this.el = el("div", "chappa-env-page");
    this.el.dataset.key = row.key;
    const head = el("div", "chappa-sv-header");
    this.header = mountPageHeader(head, { title: row.label, backTo: "Environment", onBack });
    this.body = el("div", "chappa-sv-body chappa-env-page-body");
    this.info = el("div", "chappa-env-page-info");
    this.actions = new ActionsCell(deps);
    this.actions.el.classList.add("chappa-env-page-actions");
    this.sections = new SectionsBlock(deps);
    this.body.append(this.info, this.actions.el, this.sections.el);
    this.el.append(head, this.body);
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

  /** Show `row` as of the last poll; `null` once it is no longer reported. */
  update(row: EnvRow | null, version: string): void {
    if (!row) {
      if (this.sig === "gone") return;
      this.sig = "gone";
      this.info.textContent = "";
      const gone = el("div", "chappa-env-gone");
      gone.textContent = "This component is no longer reported.";
      this.info.appendChild(gone);
      this.actions.load([], version);
      this.sections.update([], { sourceId: this.sourceId, target: null, version });
      return;
    }
    const target = rowTarget(row);
    this.actions.load(row.hasActions ? [target] : [], version, rowOnly(row));
    this.sections.update(row.sections, { sourceId: row.sourceId, target, version });
    const sig = JSON.stringify([row.label, row.state, row.hint, row.details, row.source, row.probe, row.links, row.badges, row.alerts, row.oneshot]);
    if (sig === this.sig) return;
    this.sig = sig;
    this.header.setTitle(row.label);
    this.info.textContent = "";
    const state = el("div", "chappa-env-page-state");
    const pill = el("span", "chappa-env-pill");
    pill.dataset.tone = row.tone;
    pill.textContent = row.stateText;
    const means = el("span", "");
    means.textContent = row.stateTitle;
    state.append(pill, means);
    if (row.oneshot) state.appendChild(chipEl({ text: "one-shot", tone: "muted" }));
    for (const c of row.badges) state.appendChild(chipEl(c));
    this.info.appendChild(state);
    const grid = document.createElement("dl");
    grid.className = "chappa-env-page-grid";
    const add = (label: string, value: string): void => {
      if (!value) return;
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      grid.append(dt, dd);
    };
    add("Id", row.id === row.label ? "" : row.id);
    add("Hint", row.hint);
    add("Details", row.details.join(" · "));
    add("Source", row.source ? `${row.source.text}\n${row.source.title}` : "");
    add("Endpoint", row.probe ? `${row.probe.text} (${row.probe.url})` : "");
    for (const a of row.alerts) add(a.severity === "bad" ? "Alert" : "Warning", a.text);
    this.info.appendChild(grid);
    if (row.links.length) {
      const links = el("div", "chappa-env-page-links");
      for (const l of row.links) links.appendChild(this.view.link(row.sourceId, l.url, l.browser ? `${l.label} ↗` : l.label, l.browser));
      this.info.appendChild(links);
    }
  }
}

/** Every fact section of a stack, with the sections' actions. */
class FactsPage implements Subpage {
  readonly el: HTMLElement;
  readonly sourceId: string;
  private missing = false;
  private readonly header: PageHeader;
  private readonly sections: SectionsBlock;

  constructor(deps: SourceViewDeps, b: EnvBlock, onBack: () => void) {
    this.sourceId = b.sourceId;
    this.el = el("div", "chappa-env-page chappa-env-facts-page");
    const head = el("div", "chappa-sv-header");
    this.header = mountPageHeader(head, { title: `${b.sourceLabel}: details`, backTo: "Environment", onBack });
    const body = el("div", "chappa-sv-body chappa-env-page-body");
    this.sections = new SectionsBlock(deps);
    body.appendChild(this.sections.el);
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
    return this.missing;
  }

  update(b: EnvBlock | null, version: string): void {
    this.missing = b === null;
    const sections: Section[] = b?.facts ?? [];
    const target: ActionTarget | null = b ? { sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "environment", key: null } : null;
    this.sections.update(sections, { sourceId: this.sourceId, target, version });
  }
}

function paintAlerts(parent: HTMLElement, alerts: EnvAlert[]): void {
  parent.textContent = "";
  for (const a of alerts) {
    const e = el("div", "chappa-env-alert");
    e.dataset.severity = a.severity;
    if (a.componentId) e.dataset.component = a.componentId;
    e.textContent = a.text;
    parent.appendChild(e);
  }
}
