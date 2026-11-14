// A drill-in page a source serves for a `panel` action: a title with a
// back arrow before it, the freshness line, banners and sections drawn by
// the shared renderer, and the page's actions. The page is fetched again
// while it is open, through Rust (`source_panel`), which only fetches a
// path one of the source's payloads named.
//
// A page's own actions are not in any payload Rust has cached, so Rust has
// no verdict for them and cannot run them: they are drawn disabled with the
// reason until pages get their own action lookup.

import { readFreshness, oneLine, wrongContract, type FreshnessSummary } from "./prs_model";
import { SectionsBlock, inertActions, paintBanners, readBanners, readSections, type SectionsContext } from "./sections";
import type { Action, Banner, Section } from "./types";
import { fillLines, freshnessText, type PanelOrigin, type SourceViewDeps } from "./view_common";

/** How often an open page is fetched again. */
export const PANEL_POLL_MS = 30_000;

/** Why a page's actions are drawn disabled. */
export const PANEL_ACTIONS_REASON = "Actions on a source's page cannot run yet; use the item's own actions.";

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

export class PanelPage {
  readonly el: HTMLElement;
  private readonly title: HTMLElement;
  private readonly fresh: HTMLElement;
  private readonly banners: HTMLElement;
  private readonly errors: HTMLElement;
  private readonly notices: HTMLElement;
  private readonly sections: SectionsBlock;
  private readonly actions: HTMLElement;
  private timer: ReturnType<typeof setInterval> | null = null;
  private payload: unknown;
  /** The last refresh failed: said beside the kept page. */
  private refreshError: string | null = null;
  private disposed = false;

  constructor(
    private readonly deps: SourceViewDeps,
    readonly origin: PanelOrigin,
    payload: unknown,
    private readonly onBack: () => void,
    pollMs = PANEL_POLL_MS,
  ) {
    this.payload = payload;
    this.el = document.createElement("div");
    this.el.className = "chappa-panel-page";
    this.el.dataset.endpoint = origin.endpoint;
    const head = document.createElement("div");
    head.className = "chappa-panel-head";
    const back = document.createElement("button");
    back.type = "button";
    back.className = "chappa-sv-back chappa-panel-back";
    back.title = "Back";
    const arrow = document.createElement("span");
    arrow.setAttribute("aria-hidden", "true");
    arrow.textContent = "←";
    this.title = document.createElement("span");
    this.title.className = "chappa-panel-title";
    back.append(arrow, this.title);
    back.addEventListener("click", () => this.onBack());
    this.fresh = document.createElement("span");
    this.fresh.className = "chappa-sv-fresh";
    head.append(back, this.fresh);
    this.banners = document.createElement("div");
    this.banners.className = "chappa-panel-banners";
    this.errors = document.createElement("div");
    this.errors.className = "chappa-sv-errors";
    this.notices = document.createElement("div");
    this.notices.className = "chappa-sv-notices";
    this.sections = new SectionsBlock(deps);
    this.actions = document.createElement("div");
    this.actions.className = "chappa-panel-actions";
    this.el.append(head, this.banners, this.errors, this.notices, this.sections.el, this.actions);
    this.paint();
    if (deps.panel && pollMs > 0) this.timer = setInterval(() => void this.refresh(), pollMs);
  }

  /** Fetch the page again; a failure keeps the page and says so. */
  async refresh(): Promise<void> {
    if (!this.deps.panel || this.disposed) return;
    try {
      const next = await this.deps.panel(this.origin.sourceId, this.origin.endpoint);
      if (this.disposed) return;
      this.payload = next;
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
    this.title.textContent = page.title;
    this.fresh.textContent = freshnessText(page.freshness, this.deps.now());
    const ctx: SectionsContext = { sourceId: this.origin.sourceId, target: null, version: "", reason: PANEL_ACTIONS_REASON };
    if (page.wrongContract) {
      paintBanners(this.banners, this.deps, []);
      fillLines(this.errors, "chappa-sv-error", [`${this.origin.sourceLabel} needs an update: this page speaks a different version of the source contract.`]);
      fillLines(this.notices, "chappa-sv-notice", []);
      this.sections.update([], ctx);
      this.actions.textContent = "";
      return;
    }
    paintBanners(this.banners, this.deps, page.banners.map((banner) => ({ banner, ctx })));
    fillLines(this.errors, "chappa-sv-error", [...(this.refreshError ? [this.refreshError] : []), ...page.freshness.errors]);
    fillLines(this.notices, "chappa-sv-notice", page.freshness.notices);
    this.sections.update(page.sections, ctx);
    this.actions.textContent = "";
    if (page.actions.length) this.actions.appendChild(inertActions(page.actions, PANEL_ACTIONS_REASON));
  }
}
