// The activity bar: a slim column on the window's far left with one tab per
// cross-project view (Work, Needs you, PRs, Environment) and one for the
// projects, plus a footer slot the app fills with Settings and the bell.
//
// The views span every open project, so they are not listed inside one
// project's sidebar; the Projects tab is where the project sidebar lives.
// The bar owns only its own DOM and the remembered choices; what a tab
// shows (and what Back returns to) stays with the app.

import { isMac, shortcutLabel } from "./platform";
import { navTabShortcut } from "./term/input";

/** The tabs, in bar order. View tabs use the rail view names. */
export const NAV_TABS = ["work", "attention", "prs", "environment", "projects"] as const;
export type NavTab = (typeof NAV_TABS)[number];
export type NavViewTab = Exclude<NavTab, "projects">;

/** The short label under each icon. */
export const NAV_LABELS: Record<NavTab, string> = {
  work: "Work",
  attention: "Needs you",
  prs: "PRs",
  environment: "Environment",
  projects: "Projects",
};

export function isNavTab(name: string): name is NavTab {
  return (NAV_TABS as readonly string[]).includes(name);
}

/** The tab a Mod+digit press selects, or null. */
export function navTabFor(e: KeyboardEvent): NavTab | null {
  const n = navTabShortcut(e);
  return n === null ? null : (NAV_TABS[n - 1] ?? null);
}

/** The chord that selects `tab`, as this platform spells it (Cmd+digit on
 *  macOS, Ctrl+Shift+digit elsewhere). */
export function navTabShortcutLabel(tab: NavTab): string {
  const n = NAV_TABS.indexOf(tab) + 1;
  return shortcutLabel(isMac() ? `Mod+${n}` : `Mod+Shift+${n}`);
}

// --- remembered choices -----------------------------------------------

const TAB_KEY = "chappa-ai.nav.tab";
const PROJECT_KEY = "chappa-ai.nav.project";
const SOURCE_PREFIX = "chappa-ai.nav.source.";

/** Every key the bar remembers under (tests clear them between runs). */
export const NAV_STORAGE_PREFIX = "chappa-ai.nav.";

/** localStorage, or null where there is none or touching it throws: a
 *  remembered tab is a convenience, never a reason to fail. */
function storage(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch {
    return null;
  }
}

function read(key: string): string | null {
  try {
    return storage()?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

function write(key: string, value: string | null): void {
  try {
    const s = storage();
    if (!s) return;
    if (value === null) s.removeItem(key);
    else s.setItem(key, value);
  } catch {
    // Storage full or disabled: the choice holds for this session only.
  }
}

/** The tab selected when the app last ran, or null. */
export function readNavTab(): NavTab | null {
  const v = read(TAB_KEY);
  return v !== null && isNavTab(v) ? v : null;
}

export function writeNavTab(tab: NavTab): void {
  write(TAB_KEY, tab);
}

/** The project expanded when the app last ran, or null. */
export function readNavProject(): number | null {
  const v = read(PROJECT_KEY);
  if (v === null || !/^\d+$/.test(v)) return null;
  return Number(v);
}

export function writeNavProject(id: number | null): void {
  write(PROJECT_KEY, id === null ? null : String(id));
}

/** The source (or stack) picked at the top of a view, or null for the
 *  default. */
export function readViewSource(view: string): string | null {
  return read(SOURCE_PREFIX + view);
}

export function writeViewSource(view: string, sourceId: string | null): void {
  write(SOURCE_PREFIX + view, sourceId);
}

// --- icons --------------------------------------------------------------

const ICONS: Record<NavTab, string[]> = {
  // A list: three lines with bullets.
  work: ["M2.5 4 H3.5", "M6 4 H13.5", "M2.5 8 H3.5", "M6 8 H13.5", "M2.5 12 H3.5", "M6 12 H13.5"],
  // An exclamation mark in a circle.
  attention: ["M8 1.75 A6.25 6.25 0 1 0 8 14.25 A6.25 6.25 0 1 0 8 1.75 Z", "M8 4.75 V8.75", "M8 11 V11.25"],
  // Two opposite arrows: a change offered and taken back for review.
  prs: ["M2.5 5 H12.5", "M10 2.5 L12.5 5 L10 7.5", "M13.5 11 H3.5", "M6 8.5 L3.5 11 L6 13.5"],
  // Three stacked servers.
  environment: ["M2.5 2 H13.5 V6 H2.5 Z", "M2.5 6 V10 H13.5 V6", "M2.5 10 V14 H13.5 V10", "M4.5 4 H5", "M4.5 8 H5", "M4.5 12 H5"],
  // A folder.
  projects: ["M1.75 4 V13 H14.25 V5.5 H7.5 L6 3.5 H1.75 Z"],
};

function navIcon(tab: NavTab): SVGSVGElement {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  svg.classList.add("chappa-nav-icon");
  for (const d of ICONS[tab]) {
    const path = document.createElementNS(ns, "path");
    path.setAttribute("d", d);
    path.setAttribute("fill", "none");
    path.setAttribute("stroke", "currentColor");
    path.setAttribute("stroke-width", "1.5");
    path.setAttribute("stroke-linecap", "round");
    path.setAttribute("stroke-linejoin", "round");
    svg.appendChild(path);
  }
  return svg;
}

// --- the bar --------------------------------------------------------------

/** What one tab shows. */
export interface NavTabState {
  /** Listed at all (a view switched off in Settings is not). */
  visible: boolean;
  /** The number on the tab (Needs you, PRs); 0 shows none. */
  count?: number;
  /** A dot instead of a number (Environment's alert). */
  dot?: "bad" | "warn" | null;
  /** No source serves the view yet: drawn quieter, still selectable. */
  idle?: boolean;
  /** The tooltip (the shortcut is appended). */
  title: string;
}

interface TabRefs {
  btn: HTMLButtonElement;
  badge: HTMLElement;
  dot: HTMLElement;
}

export class ActivityBar {
  readonly el: HTMLElement;
  /** Bottom of the bar, for Settings, the bell and the review hint. */
  readonly footer: HTMLElement;
  private readonly tabs = new Map<NavTab, TabRefs>();
  private selected: NavTab = "projects";
  private sig = "";

  constructor(private readonly onSelect: (tab: NavTab) => void) {
    this.el = document.createElement("nav");
    this.el.className = "chappa-activity-bar";
    this.el.setAttribute("aria-label", "Main");
    const list = document.createElement("div");
    list.className = "chappa-nav-tabs";
    list.setAttribute("role", "tablist");
    list.setAttribute("aria-orientation", "vertical");
    for (const tab of NAV_TABS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-nav-tab";
      btn.dataset.tab = tab;
      btn.setAttribute("role", "tab");
      const icon = document.createElement("span");
      icon.className = "chappa-nav-glyph";
      icon.appendChild(navIcon(tab));
      const badge = document.createElement("span");
      badge.className = "chappa-nav-badge";
      const dot = document.createElement("span");
      dot.className = "chappa-nav-dot";
      icon.append(badge, dot);
      const label = document.createElement("span");
      label.className = "chappa-nav-label";
      label.textContent = NAV_LABELS[tab];
      btn.append(icon, label);
      // Keep the terminal's focus on a press; the click still lands.
      btn.addEventListener("mousedown", (e) => e.preventDefault());
      btn.addEventListener("click", () => this.onSelect(tab));
      btn.addEventListener("keydown", (e) => this.onKey(e, tab));
      list.appendChild(btn);
      this.tabs.set(tab, { btn, badge, dot });
    }
    this.footer = document.createElement("div");
    this.footer.className = "chappa-rail-footer chappa-nav-footer";
    this.el.append(list, this.footer);
  }

  /** The tab's button (tests and focus moves). */
  button(tab: NavTab): HTMLButtonElement {
    return this.tabs.get(tab)!.btn;
  }

  get current(): NavTab {
    return this.selected;
  }

  /** Repaint every tab; untouched when nothing it shows changed. */
  update(selected: NavTab, states: Record<NavTab, NavTabState>): void {
    const sig = JSON.stringify([selected, states]);
    if (sig === this.sig) return;
    this.sig = sig;
    this.selected = selected;
    for (const tab of NAV_TABS) {
      const s = states[tab];
      const { btn, badge, dot } = this.tabs.get(tab)!;
      btn.hidden = !s.visible;
      const on = tab === selected;
      btn.classList.toggle("selected", on);
      btn.classList.toggle("idle", !!s.idle);
      btn.setAttribute("aria-selected", String(on));
      // One tab stop for the whole bar: arrows move inside it.
      btn.tabIndex = on ? 0 : -1;
      const n = s.count ?? 0;
      badge.textContent = n > 0 ? (n > 99 ? "99+" : String(n)) : "";
      if (s.dot) dot.dataset.level = s.dot;
      else delete dot.dataset.level;
      btn.title = `${s.title} (${navTabShortcutLabel(tab)})`;
      btn.setAttribute("aria-label", n > 0 ? `${NAV_LABELS[tab]}, ${n}` : s.dot ? `${NAV_LABELS[tab]}, ${s.dot === "bad" ? "alert" : "warning"}` : NAV_LABELS[tab]);
    }
    // The selected tab may have been hidden: keep one tab stop.
    if (!this.visibleTabs().includes(selected)) {
      const first = this.visibleTabs()[0];
      if (first) this.tabs.get(first)!.btn.tabIndex = 0;
    }
  }

  private visibleTabs(): NavTab[] {
    return NAV_TABS.filter((t) => !this.tabs.get(t)!.btn.hidden);
  }

  /** Arrow keys walk the tabs (focus only; Enter or Space selects). */
  private onKey(e: KeyboardEvent, from: NavTab): void {
    const tabs = this.visibleTabs();
    const i = tabs.indexOf(from);
    let next: NavTab | undefined;
    if (e.key === "ArrowDown" || e.key === "ArrowRight") next = tabs[(i + 1) % tabs.length];
    else if (e.key === "ArrowUp" || e.key === "ArrowLeft") next = tabs[(i - 1 + tabs.length) % tabs.length];
    else if (e.key === "Home") next = tabs[0];
    else if (e.key === "End") next = tabs[tabs.length - 1];
    if (!next) return;
    e.preventDefault();
    this.tabs.get(next)!.btn.focus();
  }
}

export const NAV_BAR_CSS = `
.chappa-activity-bar{flex:none;width:68px;display:flex;flex-direction:column;align-items:stretch;box-sizing:border-box;padding:8px 4px 6px;border-right:1px solid #22252a;background:#0b0c0e;}
.chappa-nav-tabs{display:flex;flex-direction:column;gap:4px;}
.chappa-nav-tab{display:flex;flex-direction:column;align-items:center;gap:3px;width:100%;padding:7px 2px 6px;border:0;border-radius:8px;background:none;color:#8b949e;font:10px system-ui,sans-serif;line-height:1.15;text-align:center;cursor:pointer;}
.chappa-nav-tab[hidden]{display:none;}
.chappa-nav-tab:hover{background:#171a1f;color:#d6d8dc;}
.chappa-nav-tab.selected{background:#1d232b;color:#e6e8ec;box-shadow:inset 2px 0 0 #5ea6ff;}
.chappa-nav-tab.idle:not(.selected){color:#5c636c;}
.chappa-nav-tab:focus-visible{outline:2px solid #5ea6ff;outline-offset:-2px;}
.chappa-nav-glyph{position:relative;display:block;width:20px;height:20px;}
.chappa-nav-icon{display:block;width:20px;height:20px;}
.chappa-nav-label{display:block;max-width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-nav-badge{position:absolute;top:-6px;left:12px;min-width:16px;height:16px;border-radius:8px;background:#f0b429;color:#0d0e10;font:600 10px ui-monospace,monospace;line-height:16px;text-align:center;padding:0 4px;box-sizing:border-box;}
.chappa-nav-badge:empty{display:none;}
.chappa-nav-dot{position:absolute;top:-3px;right:-4px;width:8px;height:8px;border-radius:50%;display:none;box-shadow:0 0 0 2px #0b0c0e;}
.chappa-nav-dot[data-level]{display:block;}
.chappa-nav-dot[data-level="bad"]{background:#f85149;}
.chappa-nav-dot[data-level="warn"]{background:#f0b429;}
.chappa-nav-footer{margin-top:auto;flex-direction:column;align-items:stretch;gap:2px;border-top:1px solid #1c1f24;padding:6px 0 0;}
.chappa-nav-footer .chappa-rail-gear{flex:none;flex-direction:column;justify-content:center;gap:2px;padding:6px 2px;font-size:10px;text-align:center;}
.chappa-nav-footer .chappa-rail-gear > span:first-child{font-size:16px;line-height:1;}
.chappa-nav-footer .chappa-notify-bell{width:100%;height:34px;font-size:16px;}
.chappa-nav-footer .chappa-notify-bell-count{right:12px;}
.chappa-nav-footer .chappa-views-hint{width:100%;padding:4px 2px;border:1px solid #3a3320;border-radius:6px;background:#1f1b10;color:#f0b429;font:10px system-ui,sans-serif;cursor:pointer;white-space:normal;}
.chappa-nav-footer .chappa-views-hint:hover{background:#2a2414;}
`;
