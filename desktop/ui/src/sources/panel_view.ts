// A drill-in page a source serves for a `panel` action: a header that
// starts with the shared back arrow and the page's title, the freshness
// line, banners and sections drawn by the shared renderer, and the page's
// own actions. The page is fetched again while it is open, through Rust
// (`source_panel`), which only fetches a path one of the source's view
// payloads named.
//
// Rust keeps the last page of each panel, so the page's actions (its own,
// its banners' and its sections') are judged and run like a view's, under
// the view name `panel:<endpoint>` with no key. A page opened from a page
// stacks over it in the view that opened the first one; each arrow steps
// back one page.

import { mountPageHeader, type PageHeader } from "../page_header";
import type { ActionTarget } from "./needs_you";
import { readFreshness, oneLine, wrongContract, type FreshnessSummary } from "./prs_model";
import { SectionsBlock, actionIds, paintBanners, readBanners, readSections, type SectionsContext } from "./sections";
import type { Action, Banner, Section } from "./types";
import { ActionsCell, fillLines, freshnessText, type PanelOrigin, type SourceViewDeps, type Subpage, type ViewFrame } from "./view_common";

/** How often an open page is fetched again. */
export const PANEL_POLL_MS = 30_000;

/** The view name Rust addresses a fetched page's actions by. */
export function panelView(endpoint: string): string {
  return `panel:${endpoint}`;
}

export interface PanelPageData {
  title: string;
  freshness: FreshnessSummary;
  banners: Banner[];
  sections: Section[];
  actions: Action[];
  /** The page speaks another contract version. */
  wrongContract: boolean;
}

const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;

/** A page read leniently; the action's label stands in for a missing title. */
export function readPanel(v: unknown, fallbackTitle: string): PanelPageData {
  const p = obj(v) ?? {};
  const f = readFreshness(p.freshness);
  const title = typeof p.title === "string" ? oneLine(p.title) : "";
  return {
    title: title || fallbackTitle,
    freshness: {
      asOf: f.asOf,
      refreshing: f.refreshing === true,
      notices: (f.notices ?? []).map(oneLine),
      errors: (f.errors ?? []).map((e) => oneLine(e.message)),
    },
    banners: readBanners(p.banners),
    sections: readSections(p.sections),
    actions: (Array.isArray(p.actions) ? p.actions : []).filter((a) => typeof obj(a)?.id === "string") as Action[],
    wrongContract: wrongContract(v),
  };
}

export class PanelPage implements Subpage {
  readonly el: HTMLElement;
  private readonly header: PageHeader;
  private readonly fresh: HTMLElement;
  private readonly banners: HTMLElement;
  private readonly errors: HTMLElement;
  private readonly notices: HTMLElement;
  private readonly sections: SectionsBlock;
  private readonly actions: ActionsCell;
  private timer: ReturnType<typeof setInterval> | null = null;
  private payload: unknown;
  /** Bumped on every fetch, so the actions are judged again. */
  private fetches = 0;
  /** The last refresh failed: said beside the kept page. */
  private refreshError: string | null = null;
  private disposed = false;

  constructor(
    private readonly deps: SourceViewDeps,
    readonly origin: PanelOrigin,
    payload: unknown,
    onBack: () => void,
    pollMs = PANEL_POLL_MS,
  ) {
    this.payload = payload;
    this.el = document.createElement("div");
    this.el.className = "chappa-panel-page";
    this.el.dataset.endpoint = origin.endpoint;
    const head = document.createElement("div");
    head.className = "chappa-sv-header chappa-panel-head";
    this.fresh = document.createElement("span");
    this.fresh.className = "chappa-sv-fresh";
    head.append(this.fresh);
    this.header = mountPageHeader(head, { title: origin.label, backTo: origin.sourceLabel, onBack, titleClass: "chappa-panel-title" });
    const body = document.createElement("div");
    body.className = "chappa-sv-body chappa-panel-body";
    this.banners = document.createElement("div");
    this.banners.className = "chappa-panel-banners";
    this.errors = document.createElement("div");
    this.errors.className = "chappa-sv-errors";
    this.notices = document.createElement("div");
    this.notices.className = "chappa-sv-notices";
    this.sections = new SectionsBlock(deps);
    this.actions = new ActionsCell(deps);
    this.actions.el.classList.add("chappa-panel-actions");
    body.append(this.banners, this.errors, this.notices, this.sections.el, this.actions.el);
    this.el.append(head, body);
    this.paint();
    if (deps.panel && pollMs > 0) this.timer = setInterval(() => void this.refresh(), pollMs);
  }

  /** The page's title as its header shows it. */
  get title(): string {
    return this.header.title.textContent ?? this.origin.label;
  }

  setBackTo(where: string): void {
    this.header.setBackTo(where);
  }

  /** Who the page's actions are asked for: the page itself, no key. */
  get target(): ActionTarget {
    return { sourceId: this.origin.sourceId, sourceLabel: this.origin.sourceLabel, view: panelView(this.origin.endpoint), key: null };
  }

  /** Fetch the page again; a failure keeps the page and says so. */
  async refresh(): Promise<void> {
    if (!this.deps.panel || this.disposed) return;
    try {
      const next = await this.deps.panel(this.origin.sourceId, this.origin.endpoint);
      if (this.disposed) return;
      this.payload = next;
      this.fetches++;
      this.refreshError = null;
    } catch (e) {
      if (this.disposed) return;
      this.refreshError = `Could not refresh this page: ${e instanceof Error ? e.message : String(e)}`;
    }
    this.paint();
  }

  dispose(): void {
    this.disposed = true;
    if (this.timer !== null) clearInterval(this.timer);
    this.timer = null;
    this.el.remove();
  }

  private paint(): void {
    const page = readPanel(this.payload, this.origin.label);
    this.header.setTitle(page.title);
    this.fresh.textContent = freshnessText(page.freshness, this.deps.now());
    const version = `${this.origin.sourceId}:page:${this.fetches}`;
    const ctx: SectionsContext = { sourceId: this.origin.sourceId, target: this.target, version };
    if (page.wrongContract) {
      paintBanners(this.banners, this.deps, []);
      fillLines(this.errors, "chappa-sv-error", [`${this.origin.sourceLabel} needs an update: this page speaks a different version of the source contract.`]);
      fillLines(this.notices, "chappa-sv-notice", []);
      this.sections.update([], ctx);
      this.actions.load([], version);
      return;
    }
    paintBanners(this.banners, this.deps, page.banners.map((banner) => ({ banner, ctx })));
    fillLines(this.errors, "chappa-sv-error", [...(this.refreshError ? [this.refreshError] : []), ...page.freshness.errors]);
    fillLines(this.notices, "chappa-sv-notice", page.freshness.notices);
    this.sections.update(page.sections, ctx);
    const own = actionIds(page.actions);
    if (own.length) this.actions.load([this.target], version, own);
    else this.actions.load([], version);
  }
}

/**
 * Open a panel's first page over `frame` (over the page on top of it, when a
 * page opened it). `deps.openPanel` should land here again for the same
 * frame, so a page's own panel actions stack in the same view.
 */
export function openPanelIn(frame: ViewFrame, deps: SourceViewDeps, origin: PanelOrigin, payload: unknown, pollMs = PANEL_POLL_MS): PanelPage {
  const page = new PanelPage(deps, origin, payload, () => frame.popSubpage(), pollMs);
  frame.pushSubpage(page);
  return page;
}

/** A view's deps with `openPanel` stacking pages over `frame`. */
export function withPanels<D extends SourceViewDeps>(frame: () => ViewFrame, deps: D): D {
  const out: D = {
    ...deps,
    openPanel: (origin, payload) => void openPanelIn(frame(), out, origin, payload),
  };
  return out;
}
