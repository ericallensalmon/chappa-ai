// The Environment page: one block per trusted source serving the view (each
// is its own stack), each with its header counts, banners and alerts, the
// components grouped as the source groups them, and its facts. Built like
// the PRs page: the app hands it a plain model on every change and only the
// rows whose content changed are repainted, so a poll does not flicker and
// focus inside the page survives.
//
// Components that are not deployed are hidden by default behind a toggle
// per source; the choice lasts as long as the page does (the session).
//
// Banners and facts are drawn by the shared section renderer, their actions
// judged as the view's own; a component's sections fold inside its row with
// their actions judged as the component's. A `panel` action opens the
// source's drill-in page over the view.

import type { ActionTarget } from "./needs_you";
import type { EnvAlert, EnvBlock, EnvGroup, EnvRow, EnvironmentModel } from "./environment_model";
import { withPanels } from "./panel_view";
import { SectionsBlock, paintBanners } from "./sections";
import { ActionsCell, ViewFrame, chipEl, openChecked, placeAt, truncate, type SourceViewDeps } from "./view_common";

export interface EnvironmentViewInput {
  model: EnvironmentModel;
  /** Labels of the trusted sources serving the view (empty: none does). */
  servedBy: string[];
  /** Bumped per source whenever its payload was re-read. */
  versions: Record<string, number>;
}

export const ENVIRONMENT_CSS = `
.chappa-env-block{margin-bottom:22px;}
.chappa-env-block-head{display:flex;align-items:baseline;gap:10px;margin:4px 0 8px;flex-wrap:wrap;}
.chappa-env-source{font:600 13px system-ui,sans-serif;color:#e6e8ec;}
.chappa-env-counts{font:12px system-ui,sans-serif;color:#8b949e;}
.chappa-env-alerts,.chappa-env-banners{display:flex;flex-direction:column;gap:4px;margin-bottom:10px;}
.chappa-env-facts{margin-bottom:10px;}
.chappa-env-alerts:empty,.chappa-env-banners:empty,.chappa-env-facts:empty,.chappa-env-wait:empty,.chappa-env-row .chappa-secs:empty{display:none;}
.chappa-env-row .chappa-sec{margin:2px 0 0;}
.chappa-env-alert{font:12px system-ui,sans-serif;padding:3px 8px;border-radius:5px;border:1px solid #5c4813;color:#f0d68a;background:#1d1708;}
.chappa-env-alert[data-severity="bad"]{border-color:#5c1f1c;color:#ffb3ad;background:#1f0d0c;}
.chappa-env-wait{font:12px system-ui,sans-serif;color:#6b7280;margin-bottom:8px;}
.chappa-env-dot{flex:none;width:8px;height:8px;border-radius:50%;background:#6b7280;}
.chappa-env-dot[data-tone="ok"]{background:#3fb950;}
.chappa-env-dot[data-tone="warn"]{background:#f0b429;}
.chappa-env-dot[data-tone="bad"]{background:#f85149;}
.chappa-env-label{font:13px system-ui,sans-serif;color:#e6e8ec;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-env-state{font:11px ui-monospace,monospace;color:#8b949e;}
.chappa-env-state[data-tone="ok"]{color:#3fb950;}
.chappa-env-state[data-tone="warn"]{color:#f0b429;}
.chappa-env-state[data-tone="bad"]{color:#f85149;}
.chappa-env-cells{display:flex;flex-wrap:wrap;gap:4px 14px;font:11px ui-monospace,monospace;color:#8b949e;}
.chappa-env-cells:empty,.chappa-env-links:empty,.chappa-env-hint:empty{display:none;}
.chappa-env-probe[data-up="true"]{color:#3fb950;}
.chappa-env-probe[data-up="false"]{color:#f85149;}
.chappa-env-links{display:flex;flex-wrap:wrap;gap:4px;}
.chappa-env-link{border:0;background:none;padding:0;color:#5ea6ff;font:11px system-ui,sans-serif;cursor:pointer;}
.chappa-env-link:hover{text-decoration:underline;}
.chappa-env-hint{font:12px system-ui,sans-serif;color:#adb3bd;}
.chappa-env-toggle{border:0;background:none;padding:2px 0;color:#8b949e;font:12px system-ui,sans-serif;cursor:pointer;margin-bottom:10px;}
.chappa-env-toggle:hover{color:#d6d8dc;}
.chappa-env-row.focused{outline:1px solid #5ea6ff;outline-offset:1px;}
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
export function notDeployedToggleText(n: number, shown: boolean): string {
  return `${shown ? "Hide" : "Show"} components that are not deployed (${n})`;
}

interface BlockRefs {
  el: HTMLElement;
  source: HTMLElement;
  counts: HTMLElement;
  wait: HTMLElement;
  banners: HTMLElement;
  alerts: HTMLElement;
  groups: HTMLElement;
  toggle: HTMLButtonElement;
  facts: SectionsBlock;
  sig: string;
}

interface GroupRefs {
  el: HTMLElement;
  label: HTMLElement;
  count: HTMLElement;
  rows: HTMLElement;
  sig: string;
}

interface RowRefs {
  el: HTMLElement;
  dot: HTMLElement;
  label: HTMLElement;
  kind: HTMLElement;
  state: HTMLElement;
  details: HTMLElement;
  cells: HTMLElement;
  links: HTMLElement;
  hint: HTMLElement;
  chips: HTMLElement;
  sections: SectionsBlock;
  actions: ActionsCell;
  sig: string;
  row: EnvRow | null;
}

const el = (tag: string, cls: string): HTMLElement => {
  const e = document.createElement(tag);
  e.className = cls;
  return e;
};

export class EnvironmentView {
  private readonly frame: ViewFrame;
  /** Per source: whether its not-deployed components are shown. */
  private readonly showNotDeployed = new Map<string, boolean>();
  private readonly blocks = new Map<string, BlockRefs>();
  private readonly groups = new Map<string, GroupRefs>();
  private readonly rows = new Map<string, RowRefs>();
  private input: EnvironmentViewInput | null = null;
  /** A focus asked for before the row exists (the first answer pending). */
  private pendingFocus: { id: string; sourceId?: string } | null = null;
  private focusedKey: string | null = null;
  /** The deps every part of the page uses: panels stack over this view. */
  private readonly ui: SourceViewDeps;

  constructor(private readonly deps: SourceViewDeps) {
    injectStyles();
    this.frame = new ViewFrame("Environment", "environment", () => deps.onClose());
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

  update(input: EnvironmentViewInput): void {
    this.input = input;
    this.paint();
    if (this.pendingFocus) this.focus(this.pendingFocus.id, this.pendingFocus.sourceId);
  }

  dispose(): void {
    this.frame.dispose();
  }

  /** Scroll to a component's row and mark it. A not-deployed component's
   *  block shows its hidden rows first. Without `sourceId` the first source
   *  with that component wins. Answers whether the row was found; a row not
   *  there yet is focused when it arrives. */
  focus(componentId: string, sourceId?: string): boolean {
    const row = this.input?.model.blocks
      .filter((b) => sourceId === undefined || b.sourceId === sourceId)
      .flatMap((b) => b.groups.flatMap((g) => g.rows))
      .find((r) => r.id === componentId);
    if (!row) {
      // Before the first answer the row may yet arrive; after it, it is gone.
      this.pendingFocus = this.input?.model.hasData ? null : { id: componentId, ...(sourceId !== undefined ? { sourceId } : {}) };
      return false;
    }
    this.pendingFocus = null;
    this.frame.clearSubpages();
    if (row.notDeployed && !this.showNotDeployed.get(row.sourceId)) {
      this.showNotDeployed.set(row.sourceId, true);
      this.paint();
    }
    if (this.focusedKey) this.rows.get(this.focusedKey)?.el.classList.remove("focused");
    const refs = this.rows.get(row.key);
    if (!refs) return false;
    this.focusedKey = row.key;
    refs.el.classList.add("focused");
    refs.el.scrollIntoView?.({ block: "center" });
    return true;
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
      const refs = this.blockRefs(b.sourceId);
      this.paintBlock(refs, b, model.blocks.length > 1, `${b.sourceId}:${input.versions[b.sourceId] ?? 0}`);
      placeAt(this.frame.content, refs.el, i);
      const shown = this.showNotDeployed.get(b.sourceId) ?? false;
      let gi = 0;
      for (const g of b.groups) {
        const rows = shown ? g.rows : g.rows.filter((r) => !r.notDeployed);
        if (rows.length === 0) continue;
        const gk = `${b.sourceId}\u001f${g.id}`;
        liveGroups.add(gk);
        const grefs = this.groupRefs(gk, g.id);
        this.paintGroup(grefs, g, rows.length);
        placeAt(refs.groups, grefs.el, gi++);
        rows.forEach((row, j) => {
          liveRows.add(row.key);
          const r = this.rowRefs(row.key);
          this.paintRow(r, row, input.versions);
          placeAt(grefs.rows, r.el, j);
        });
        truncate(grefs.rows, rows.length);
      }
      truncate(refs.groups, gi);
    });
    truncate(this.frame.content, model.blocks.length);
    for (const k of [...this.blocks.keys()]) if (!liveBlocks.has(k)) this.blocks.delete(k);
    for (const k of [...this.groups.keys()]) if (!liveGroups.has(k)) this.groups.delete(k);
    for (const k of [...this.rows.keys()]) if (!liveRows.has(k)) this.rows.delete(k);
    this.frame.empty.textContent = environmentEmptyText(input);
  }

  private blockRefs(sourceId: string): BlockRefs {
    let refs = this.blocks.get(sourceId);
    if (refs) return refs;
    const root = el("section", "chappa-env-block");
    root.dataset.source = sourceId;
    const head = el("div", "chappa-env-block-head");
    const source = el("span", "chappa-env-source");
    const counts = el("span", "chappa-env-counts");
    head.append(source, counts);
    const wait = el("div", "chappa-env-wait");
    const banners = el("div", "chappa-env-banners");
    const alerts = el("div", "chappa-env-alerts");
    const groups = el("div", "chappa-env-groups");
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "chappa-env-toggle";
    toggle.addEventListener("mousedown", (e) => e.preventDefault());
    toggle.addEventListener("click", () => {
      this.showNotDeployed.set(sourceId, !(this.showNotDeployed.get(sourceId) ?? false));
      this.paint();
    });
    const facts = new SectionsBlock(this.ui);
    facts.el.classList.add("chappa-env-facts");
    root.append(head, wait, banners, alerts, groups, toggle, facts.el);
    refs = { el: root, source, counts, wait, banners, alerts, groups, toggle, facts, sig: "" };
    this.blocks.set(sourceId, refs);
    return refs;
  }

  private paintBlock(refs: BlockRefs, b: EnvBlock, many: boolean, version: string): void {
    // The view's own target: its banners' and facts' actions (no key).
    const own: ActionTarget = { sourceId: b.sourceId, sourceLabel: b.sourceLabel, view: "environment", key: null };
    const ctx = { sourceId: b.sourceId, target: own, version };
    paintBanners(refs.banners, this.ui, b.banners.map((banner) => ({ banner, ctx })));
    refs.facts.update(b.facts, ctx);
    const shown = this.showNotDeployed.get(b.sourceId) ?? false;
    const sig = JSON.stringify([b.sourceLabel, b.hasData, b.header, b.alerts, b.notDeployed, shown, many]);
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.source.textContent = b.sourceLabel;
    refs.counts.textContent = b.header.join(" · ");
    refs.wait.textContent = b.hasData ? "" : `Waiting for the first answer from ${b.sourceLabel}.`;
    paintAlerts(refs.alerts, b.alerts);
    refs.toggle.hidden = b.notDeployed === 0;
    refs.toggle.textContent = notDeployedToggleText(b.notDeployed, shown);
    refs.toggle.setAttribute("aria-expanded", String(shown));
  }

  private groupRefs(key: string, id: string): GroupRefs {
    let refs = this.groups.get(key);
    if (refs) return refs;
    const root = el("section", "chappa-sv-group");
    root.dataset.group = id;
    const head = el("div", "chappa-sv-group-head");
    const label = el("span", "chappa-sv-group-label");
    const count = el("span", "chappa-sv-count");
    head.append(label, count);
    const rows = el("div", "chappa-sv-rows");
    root.append(head, rows);
    refs = { el: root, label, count, rows, sig: "" };
    this.groups.set(key, refs);
    return refs;
  }

  private paintGroup(refs: GroupRefs, g: EnvGroup, shown: number): void {
    const sig = `${g.label}\u001f${shown}`;
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.label.textContent = g.label;
    refs.count.textContent = String(shown);
  }

  private rowRefs(key: string): RowRefs {
    let refs = this.rows.get(key);
    if (refs) return refs;
    const root = el("div", "chappa-sv-row chappa-env-row");
    root.dataset.key = key;
    const top = el("div", "chappa-sv-top");
    const dot = el("span", "chappa-env-dot");
    const label = el("span", "chappa-env-label");
    const kind = el("span", "chappa-sv-chip chappa-env-kind");
    kind.dataset.tone = "muted";
    kind.textContent = "one-shot";
    kind.title = "Runs to completion (a migration, a seed job)";
    const state = el("span", "chappa-env-state");
    top.append(dot, label, kind, state);
    const details = el("div", "chappa-sv-detail chappa-env-details");
    const cells = el("div", "chappa-env-cells");
    const links = el("div", "chappa-env-links");
    const hint = el("div", "chappa-env-hint");
    const chips = el("div", "chappa-sv-chips");
    const sections = new SectionsBlock(this.ui, { collapsedByDefault: true });
    const actions = new ActionsCell(this.ui);
    root.append(top, details, cells, links, hint, chips, sections.el, actions.el);
    refs = { el: root, dot, label, kind, state, details, cells, links, hint, chips, sections, actions, sig: "", row: null };
    this.rows.set(key, refs);
    return refs;
  }

  private paintRow(r: RowRefs, row: EnvRow, versions: Record<string, number>): void {
    r.row = row;
    const target: ActionTarget = { sourceId: row.sourceId, sourceLabel: row.sourceLabel, view: "environment", key: row.id };
    const version = `${row.sourceId}:${versions[row.sourceId] ?? 0}`;
    // The sections draw their own actions; the row's cell draws the rest.
    const own = row.sections.some((s) => (s.actions ?? []).length > 0) ? row.actionIds : undefined;
    r.actions.load(row.hasActions && own?.length !== 0 ? [target] : [], version, own);
    r.sections.update(row.sections, { sourceId: row.sourceId, target, version });
    const sig = JSON.stringify([
      row.label, row.oneshot, row.state, row.tone, row.details, row.source, row.probe, row.links, row.hint, row.badges, row.alerts,
    ]);
    if (sig === r.sig) return;
    r.sig = sig;
    r.el.dataset.id = row.id;
    r.el.dataset.state = row.state;
    r.el.dataset.tone = row.tone;
    const worst = row.alerts.some((a) => a.severity === "bad") ? "bad" : row.alerts.length ? "warn" : "";
    if (worst) r.el.dataset.alert = worst;
    else delete r.el.dataset.alert;
    r.dot.dataset.tone = row.tone;
    r.label.textContent = row.label;
    r.label.title = row.id === row.label ? row.label : `${row.label} (${row.id})`;
    r.kind.hidden = !row.oneshot;
    r.state.dataset.tone = row.tone;
    r.state.textContent = row.stateText;
    r.state.title = row.stateTitle;
    r.details.textContent = row.details.join(" · ");
    r.cells.textContent = "";
    if (row.source) {
      const s = el("span", "chappa-env-source-cell");
      s.dataset.mode = row.source.mode;
      s.textContent = row.source.text;
      s.title = row.source.title;
      r.cells.appendChild(s);
    }
    if (row.probe) {
      const p = el("span", "chappa-env-probe");
      p.dataset.up = String(row.probe.up);
      p.textContent = row.probe.text;
      p.title = row.probe.title;
      r.cells.appendChild(p);
    }
    r.links.textContent = "";
    for (const link of row.links) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "chappa-env-link";
      b.dataset.url = link.url;
      if (link.browser) b.dataset.browser = "true";
      b.textContent = link.browser ? `${link.label} ↗` : link.label;
      b.title = link.browser ? `Open ${link.url} in the browser` : link.url;
      b.addEventListener("mousedown", (e) => e.preventDefault());
      b.addEventListener("click", () => openChecked(this.deps, row.sourceId, link.url));
      r.links.appendChild(b);
    }
    r.hint.textContent = row.hint;
    r.chips.textContent = "";
    for (const a of row.alerts) {
      const c = chipEl({ text: a.text, tone: a.severity, title: a.severity === "bad" ? "Alert" : "Warning" });
      c.classList.add("chappa-env-row-alert");
      r.chips.appendChild(c);
    }
    for (const chip of row.badges) r.chips.appendChild(chipEl(chip));
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
