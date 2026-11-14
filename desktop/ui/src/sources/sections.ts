// The shared renderer of a source's `Section`s and `Banner`s: the titled
// label/value blocks an item, an environment or a drill-in page carries,
// and the coloured notes above a list. One place reads them leniently and
// draws them, so every view shows a source's sections the same way.
//
// A section's or banner's actions are judged by Rust like any other: they
// are read with what holds them (`source_actions` walks every `actions`
// array under the item's key; a view's or a drill-in page's own banners and
// sections are read with no key, as the view or page itself) and each block
// draws only its own ids. Where no target exists the buttons are drawn
// disabled with the reason.

import type { ActionTarget } from "./needs_you";
import { oneLine, readBadges, type Chip, type Tone } from "./prs_model";
import type { Action, Banner, Link, Section } from "./types";
import { ActionsCell, chipEl, openChecked, type SourceViewDeps } from "./view_common";

type Row = Section["rows"][number];

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const text = (v: unknown): string | null => {
  const s = typeof v === "string" ? oneLine(v) : "";
  return s === "" ? null : s;
};
const obj = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

const BANNER_TONES: ReadonlySet<string> = new Set(["info", "warn", "bad"]);

// ---- reading --------------------------------------------------------------

function readLinks(v: unknown): Link[] {
  return list(v)
    .map(obj)
    .filter((l): l is Record<string, unknown> => l !== null && !!str(l.url))
    .map((l) => ({ label: text(l.label) ?? (l.url as string), url: l.url as string }));
}

function readRows(v: unknown): Row[] {
  return list(v)
    .map(obj)
    .filter((r): r is Record<string, unknown> => r !== null && !!text(r.label))
    .map((r) => {
      const links = readLinks(r.links);
      const badges = readBadges(r.badges);
      const value = text(r.value);
      return {
        label: text(r.label)!,
        ...(value !== null ? { value } : {}),
        ...(links.length ? { links } : {}),
        ...(badges.length ? { badges } : {}),
      };
    });
}

/** The actions of a section or banner, kept as sent: Rust parses and
 *  judges them; only the id and label are read here. */
function readActions(v: unknown): Action[] {
  return list(v).filter((a) => !!str(obj(a)?.id)) as Action[];
}

/** One section read leniently: an id and a title are required, rows
 *  without a label are dropped, `more` needs a label and at least one row. */
export function readSection(v: unknown): Section | null {
  const s = obj(v);
  const id = str(s?.id);
  const title = text(s?.title);
  if (!s || !id || !title) return null;
  const more = obj(s.more);
  const moreRows = readRows(more?.rows);
  const actions = readActions(s.actions);
  return {
    id,
    title,
    collapsed: s.collapsed === true,
    rows: readRows(s.rows),
    ...(more && text(more.label) && moreRows.length ? { more: { label: text(more.label)!, rows: moreRows } } : {}),
    ...(actions.length ? { actions } : {}),
  };
}

/** A sections list read leniently; the first of two sections sharing an
 *  id stands (fold state is kept per id). */
export function readSections(v: unknown): Section[] {
  const out: Section[] = [];
  for (const raw of list(v)) {
    const s = readSection(raw);
    if (s && !out.some((x) => x.id === s.id)) out.push(s);
  }
  return out;
}

/** A banners list read leniently: an id and a title are required; an
 *  unknown tone reads as "info". */
export function readBanners(v: unknown): Banner[] {
  return list(v)
    .map(obj)
    .filter((b): b is Record<string, unknown> => b !== null && !!str(b.id) && !!text(b.title))
    .map((b) => {
      const actions = readActions(b.actions);
      return {
        id: b.id as string,
        tone: (str(b.tone) && BANNER_TONES.has(b.tone as string) ? b.tone : "info") as Banner["tone"],
        title: text(b.title)!,
        lines: list(b.lines).map(text).filter((l): l is string => l !== null),
        ...(actions.length ? { actions } : {}),
      };
    });
}

/** The ids of an actions list. */
export function actionIds(actions: readonly Action[] | undefined): string[] {
  return (actions ?? []).map((a) => a.id);
}

// ---- drawing ----------------------------------------------------------------

export const SECTIONS_CSS = `
.chappa-sec{border:1px solid #22252a;border-radius:7px;background:#121417;margin-bottom:8px;}
.chappa-sec-head{display:flex;align-items:center;gap:8px;width:100%;padding:6px 10px;border:0;background:none;color:#adb3bd;font:600 12px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-sec-head:hover{color:#e6e8ec;}
.chappa-sec-body{padding:0 10px 8px;display:flex;flex-direction:column;gap:3px;}
.chappa-sec.collapsed .chappa-sec-body{display:none;}
.chappa-sec-row{display:flex;flex-wrap:wrap;align-items:baseline;gap:4px 10px;font:12px system-ui,sans-serif;min-width:0;}
.chappa-sec-label{flex:0 0 140px;color:#8b949e;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-sec-value{color:#d6d8dc;min-width:0;overflow-wrap:anywhere;}
.chappa-sec-links{display:flex;flex-wrap:wrap;gap:6px;}
.chappa-sec-link{border:0;background:none;padding:0;color:#5ea6ff;font:12px system-ui,sans-serif;cursor:pointer;}
.chappa-sec-link:hover{text-decoration:underline;}
.chappa-sec-more{align-self:flex-start;border:0;background:none;padding:2px 0;color:#8b949e;font:11px system-ui,sans-serif;cursor:pointer;}
.chappa-sec-more:hover{color:#d6d8dc;}
.chappa-sec-more-rows{display:flex;flex-direction:column;gap:3px;}
.chappa-sec-more-rows.folded{display:none;}
.chappa-banner{padding:6px 10px;border:1px solid #1f3a5c;border-radius:6px;background:#0e1724;color:#b8d4f5;font:12px system-ui,sans-serif;margin-bottom:8px;display:flex;flex-direction:column;gap:3px;}
.chappa-banner[data-tone="warn"]{border-color:#5c4813;background:#1d1708;color:#f0d68a;}
.chappa-banner[data-tone="bad"]{border-color:#5c1f1c;background:#1f0d0c;color:#f5b3ae;}
.chappa-banner-title{font-weight:600;}
.chappa-banner-line{opacity:0.9;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = SECTIONS_CSS;
  document.head.appendChild(el);
}

/**
 * Where the drawn actions come from. With a `target`, each block's own
 * action ids are drawn out of Rust's judged list for that target (and run
 * through `runAction`); without one they are drawn disabled with `reason`.
 */
export interface SectionsContext {
  sourceId: string;
  target: ActionTarget | null;
  /** Bumped when the source's payload was re-read, so actions are re-judged. */
  version: string;
  /** Why the actions cannot run here (no target). */
  reason?: string;
}

/** Actions drawn without a verdict to run them: disabled, with why. */
export function inertActions(actions: readonly Action[], reason: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "chappa-sv-actions";
  for (const a of actions) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chappa-sv-action";
    btn.dataset.action = a.id;
    btn.textContent = (typeof a.label === "string" && a.label.trim()) || a.id;
    btn.disabled = true;
    btn.title = reason;
    el.appendChild(btn);
  }
  return el;
}

const NO_TARGET = "These actions cannot run here.";

interface SectionRefs {
  el: HTMLElement;
  head: HTMLButtonElement;
  body: HTMLElement;
  actions: ActionsCell;
  inert: HTMLElement | null;
  sig: string;
}

/**
 * A list of sections, drawn into {@link SectionsBlock.el} and repainted in
 * place: a section whose content did not change keeps its nodes, its
 * collapsed / "more" state as the user left it, and its action buttons.
 */
export class SectionsBlock {
  readonly el: HTMLElement;
  /** Fold choices per section id, for as long as the block lives. */
  private readonly collapsed = new Map<string, boolean>();
  private readonly moreOpen = new Set<string>();
  private readonly refs = new Map<string, SectionRefs>();
  private sections: Section[] = [];
  private ctx: SectionsContext | null = null;

  /** `collapsedByDefault`: every section starts folded whatever it says
   *  (sections inside a list row, where open ones would bury the list). */
  constructor(
    private readonly deps: SourceViewDeps,
    private readonly opts: { collapsedByDefault?: boolean } = {},
  ) {
    injectStyles();
    this.el = document.createElement("div");
    this.el.className = "chappa-secs";
  }

  update(sections: Section[], ctx: SectionsContext): void {
    this.sections = sections;
    this.ctx = ctx;
    this.paint();
  }

  private paint(): void {
    const ctx = this.ctx;
    if (!ctx) return;
    this.sections.forEach((s, i) => {
      const refs = this.sectionRefs(s.id);
      this.paintSection(refs, s, ctx);
      const at = this.el.children[i];
      if (at !== refs.el) this.el.insertBefore(refs.el, at ?? null);
    });
    while (this.el.children.length > this.sections.length) this.el.lastElementChild!.remove();
    for (const id of [...this.refs.keys()]) if (!this.sections.some((s) => s.id === id)) this.refs.delete(id);
  }

  private sectionRefs(id: string): SectionRefs {
    let refs = this.refs.get(id);
    if (refs) return refs;
    const el = document.createElement("section");
    el.className = "chappa-sec";
    el.dataset.section = id;
    const head = document.createElement("button");
    head.type = "button";
    head.className = "chappa-sec-head";
    const body = document.createElement("div");
    body.className = "chappa-sec-body";
    el.append(head, body);
    head.addEventListener("click", () => {
      const s = this.sections.find((x) => x.id === id);
      if (!s) return;
      this.collapsed.set(id, !this.isCollapsed(s));
      this.paint();
    });
    refs = { el, head, body, actions: new ActionsCell(this.deps), inert: null, sig: "" };
    this.refs.set(id, refs);
    return refs;
  }

  private isCollapsed(s: Section): boolean {
    return this.collapsed.get(s.id) ?? (this.opts.collapsedByDefault === true || s.collapsed === true);
  }

  private paintSection(refs: SectionRefs, s: Section, ctx: SectionsContext): void {
    const collapsed = this.isCollapsed(s);
    const open = this.moreOpen.has(s.id);
    const own = actionIds(s.actions);
    if (ctx.target && own.length) refs.actions.load([ctx.target], ctx.version, own);
    const sig = JSON.stringify([s, collapsed, open, ctx.sourceId, ctx.target === null, ctx.reason]);
    if (sig === refs.sig) return;
    refs.sig = sig;
    refs.el.classList.toggle("collapsed", collapsed);
    refs.head.textContent = `${collapsed ? "▸" : "▾"} ${s.title}`;
    refs.head.setAttribute("aria-expanded", String(!collapsed));
    refs.body.textContent = "";
    for (const row of s.rows) refs.body.appendChild(this.rowEl(row, ctx.sourceId));
    if (s.more) {
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "chappa-sec-more";
      toggle.textContent = `${open ? "▾" : "▸"} ${s.more.label}`;
      toggle.setAttribute("aria-expanded", String(open));
      toggle.addEventListener("click", () => {
        if (this.moreOpen.has(s.id)) this.moreOpen.delete(s.id);
        else this.moreOpen.add(s.id);
        this.paint();
      });
      const more = document.createElement("div");
      more.className = "chappa-sec-more-rows";
      more.classList.toggle("folded", !open);
      for (const row of s.more.rows) more.appendChild(this.rowEl(row, ctx.sourceId));
      refs.body.append(toggle, more);
    }
    if (own.length) {
      if (ctx.target) {
        refs.body.appendChild(refs.actions.el);
      } else {
        refs.body.appendChild(inertActions(s.actions!, ctx.reason ?? NO_TARGET));
      }
    }
  }

  private rowEl(row: Row, sourceId: string): HTMLElement {
    const el = document.createElement("div");
    el.className = "chappa-sec-row";
    const label = document.createElement("span");
    label.className = "chappa-sec-label";
    label.textContent = row.label;
    label.title = row.label;
    el.appendChild(label);
    if (row.value !== undefined) {
      const value = document.createElement("span");
      value.className = "chappa-sec-value";
      value.textContent = row.value;
      el.appendChild(value);
    }
    if (row.links?.length) {
      const links = document.createElement("span");
      links.className = "chappa-sec-links";
      for (const l of row.links) links.appendChild(linkButton(this.deps, sourceId, l));
      el.appendChild(links);
    }
    if (row.badges?.length) {
      const chips = document.createElement("span");
      chips.className = "chappa-sv-chips";
      for (const b of row.badges) chips.appendChild(chipEl(badgeChip(b)));
      el.appendChild(chips);
    }
    return el;
  }
}

/** A source's link as a button through the checked opener. */
export function linkButton(deps: SourceViewDeps, sourceId: string, link: Link): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "chappa-sec-link";
  btn.textContent = link.label;
  btn.title = `Open ${link.url}`;
  btn.addEventListener("mousedown", (e) => e.preventDefault());
  btn.addEventListener("click", () => openChecked(deps, sourceId, link.url));
  return btn;
}

const badgeChip = (b: { text: string; tone?: string; title?: string }): Chip => ({
  text: b.text,
  tone: (b.tone ?? "info") as Tone,
  ...(b.title ? { title: b.title } : {}),
});

/**
 * One banner: tone, title, lines, and its actions (judged for `target`:
 * a view's or page's own banner is the view or page itself, key null;
 * drawn disabled without one).
 */
export function bannerEl(deps: SourceViewDeps, b: Banner, ctx: SectionsContext): HTMLElement {
  injectStyles();
  const el = document.createElement("div");
  el.className = "chappa-banner";
  el.dataset.tone = b.tone;
  el.dataset.banner = b.id;
  const title = document.createElement("div");
  title.className = "chappa-banner-title";
  title.textContent = b.title;
  el.appendChild(title);
  for (const line of b.lines ?? []) {
    const l = document.createElement("div");
    l.className = "chappa-banner-line";
    l.textContent = line;
    el.appendChild(l);
  }
  const own = actionIds(b.actions);
  if (own.length) {
    if (ctx.target) {
      const cell = new ActionsCell(deps);
      cell.load([ctx.target], ctx.version, own);
      el.appendChild(cell.el);
    } else {
      el.appendChild(inertActions(b.actions!, ctx.reason ?? NO_TARGET));
    }
  }
  return el;
}

const bannerSigs = new WeakMap<HTMLElement, string>();

/** Banners drawn into `parent` behind a signature: unchanged banners are
 *  not redrawn (their buttons are not re-read). */
export function paintBanners(
  parent: HTMLElement,
  deps: SourceViewDeps,
  banners: { banner: Banner; ctx: SectionsContext }[],
): void {
  const sig = JSON.stringify(banners.map((b) => [b.banner, b.ctx]));
  if (bannerSigs.get(parent) === sig) return;
  bannerSigs.set(parent, sig);
  parent.textContent = "";
  for (const { banner, ctx } of banners) parent.appendChild(bannerEl(deps, banner, ctx));
}
