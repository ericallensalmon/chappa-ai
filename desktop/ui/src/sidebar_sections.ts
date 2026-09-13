// Collapsible sidebar sections: a chevron + icon on each section header, a
// click that folds the section away, and per-section memory of that choice.
//
// The sidebar's sections (TERMINALS, AGENTS, COMMANDS, SCRATCHPADS) already
// share one header shape (`chappa-subsection-header`: label, count, action
// buttons). This module decorates such a header in place — it prepends a
// chevron and a section icon, makes the header a toggle, and hides the body
// when collapsed — so the sections read as nested under their project and can
// be folded to cut clutter. Kept out of app.ts because scratchpads_section.ts
// builds its own header and needs the same treatment; a shared module beats a
// circular import.

/** localStorage, or null where there is none or where touching it throws
 *  (a webview with storage disabled, a private window, jsdom). Mirrors the
 *  app's own guard so a section's fold state is best-effort, never fatal. */
function safeStorage(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch {
    return null;
  }
}

const COLLAPSE_PREFIX = "chappa-ai.section-collapsed.";

/** Whether a section was left collapsed. Absent key = expanded (the default:
 *  a first-time sidebar shows everything). */
export function readCollapsed(key: string): boolean {
  try {
    return safeStorage()?.getItem(COLLAPSE_PREFIX + key) === "1";
  } catch {
    return false;
  }
}

/** Remember a section's fold state. Stores only the collapsed case, so a
 *  never-touched or re-expanded section leaves no key behind. */
export function writeCollapsed(key: string, collapsed: boolean): void {
  try {
    const storage = safeStorage();
    if (!storage) return;
    if (collapsed) storage.setItem(COLLAPSE_PREFIX + key, "1");
    else storage.removeItem(COLLAPSE_PREFIX + key);
  } catch {
    // Storage full or disabled: folding still works this session, it just
    // will not be remembered. Not worth surfacing.
  }
}

/** The section-icon set. One 16-box, 1.5px stroke, `currentColor` so the icon
 *  tracks the header's muted grey and its hover brightening for free. */
export type SectionIconName = "terminal" | "agents" | "commands" | "scratchpads";

const ICON_PATHS: Record<SectionIconName, string[]> = {
  // A prompt: the `>` caret and the input underline.
  terminal: ["M3.5 4.5 L7 8 L3.5 11.5", "M8.5 11.5 H13"],
  // A four-point spark — the agent/AI mark used on the spawner too.
  agents: ["M8 2 L9.3 6.7 L14 8 L9.3 9.3 L8 14 L6.7 9.3 L2 8 L6.7 6.7 Z"],
  // Stacked layers — a command is one of several defined in the project file.
  commands: ["M8 2 L14 5.2 L8 8.4 L2 5.2 Z", "M2 8.2 L8 11.4 L14 8.2", "M2 11 L8 14.2 L14 11"],
  // A page with a folded corner and two text lines.
  scratchpads: ["M4 2 H9.5 L13 5.5 V14 H4 Z", "M9.5 2 V5.5 H13", "M6 9 H11", "M6 11.5 H11"],
};

/** Build a section icon as an inline SVG (CSP-safe, no font or asset load). */
export function sectionIcon(name: SectionIconName): SVGSVGElement {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const d of ICON_PATHS[name]) {
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

export interface CollapsibleSectionOptions {
  /** The `chappa-subsection-header` (or peer) element to turn into a toggle. */
  header: HTMLElement;
  /** The body the toggle folds away (the row list). Hidden via `.hidden`. */
  body: HTMLElement;
  /** Stable per-section key for remembering the fold state. */
  storageKey: string;
  /** Which icon to prepend. */
  icon: SectionIconName;
}

/** What a decorated section exposes back to its owner. */
export interface CollapsibleSectionHandle {
  /** Fold or unfold programmatically (used by tests; the header click is the
   *  user path). */
  setCollapsed(collapsed: boolean): void;
  /** Current fold state. */
  isCollapsed(): boolean;
}

/**
 * Decorate an existing section header so it collapses its body on click and
 * carries a chevron + icon. The header keeps whatever label, count and action
 * buttons it already has; the chevron and icon are prepended before them.
 *
 * Clicks on an actual control in the header (an add button, a menu, an input)
 * do NOT toggle — only the chevron, icon, label and empty header space do — so
 * the `+`/`▾`/`New agent ▸` affordances keep working as before.
 */
export function makeCollapsibleSection(
  opts: CollapsibleSectionOptions,
): CollapsibleSectionHandle {
  const chevron = document.createElement("span");
  chevron.className = "chappa-sub-chevron";
  chevron.setAttribute("aria-hidden", "true");

  const icon = sectionIcon(opts.icon);
  icon.classList.add("chappa-sub-icon");

  // Prepend icon then chevron, so the final order is [chevron][icon][label…].
  opts.header.insertBefore(icon, opts.header.firstChild);
  opts.header.insertBefore(chevron, opts.header.firstChild);
  opts.header.classList.add("chappa-sub-toggle");

  let collapsed = readCollapsed(opts.storageKey);

  const apply = (): void => {
    opts.body.hidden = collapsed;
    chevron.textContent = collapsed ? "▸" : "▾"; // ▸ / ▾
    opts.header.classList.toggle("chappa-sub-collapsed", collapsed);
    opts.header.setAttribute("aria-expanded", String(!collapsed));
  };
  apply();

  const toggle = (): void => {
    collapsed = !collapsed;
    writeCollapsed(opts.storageKey, collapsed);
    apply();
  };

  opts.header.addEventListener("click", (event) => {
    // A click on a real control (add button, profile menu, spawner, an input)
    // is that control's, never a fold. The chevron/icon/label are plain
    // spans, so they fall through to the toggle.
    const target = event.target as HTMLElement | null;
    if (target && target.closest("button, input, select, textarea, a")) return;
    toggle();
  });

  return {
    setCollapsed(next: boolean): void {
      if (next === collapsed) return;
      collapsed = next;
      writeCollapsed(opts.storageKey, collapsed);
      apply();
    },
    isCollapsed(): boolean {
      return collapsed;
    },
  };
}

/** The CSS these sections need. Appended to the app's stylesheet by app.ts so
 *  there is one injection point. */
export const SIDEBAR_SECTIONS_CSS = `
.chappa-sub-toggle{cursor:pointer;user-select:none;}
.chappa-sub-toggle:hover{color:#9aa3ad;}
.chappa-sub-chevron{flex:none;display:inline-flex;justify-content:center;width:10px;font-size:9px;line-height:1;color:#6b7280;}
.chappa-sub-toggle:hover .chappa-sub-chevron{color:#9aa3ad;}
.chappa-sub-icon{flex:none;width:13px;height:13px;display:block;}
/* Nest the rows under their section: the list indents so a row's dot lines up
   past the section icon rather than under the chevron. */
.chappa-project-term-list,.chappa-workspace-cmd-list,.chappa-scratchpad-list{padding-left:10px;}
`;
