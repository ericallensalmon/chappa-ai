// The one way out of a page that was drilled into: a back arrow at the top
// left of the page's header, before its title. Every such page (a source
// view, a source's drill-in page, a work item's detail, the dashboard,
// Settings, Edit project, a scratchpad) builds its header through this
// helper, so the arrow looks, sizes, reads and answers the keyboard the same
// everywhere. The arrow is the only visible control; where it returns to is
// said in its tooltip and accessible name ("Back to …"), not as text, so a
// header never grows a second way out. Escape does what the arrow does: a
// page binds it with {@link isBackKey}.
//
// Dialogs, toasts, notification entries and the notification drawer are not
// pages and keep their own close controls.

export const BACK_ARROW_CLASS = "chappa-back-arrow";
export const PAGE_TITLE_CLASS = "chappa-page-title";

const CSS = `
.${BACK_ARROW_CLASS}{flex:none;display:inline-flex;align-items:center;justify-content:center;width:28px;height:28px;margin:0;padding:0;border:0;border-radius:6px;background:none;color:#8b949e;font:600 17px system-ui,sans-serif;line-height:1;cursor:pointer;}
.${BACK_ARROW_CLASS}:hover{background:#1d232b;color:#e6e8ec;}
.${BACK_ARROW_CLASS}:focus-visible{outline:1px solid #5ea6ff;outline-offset:1px;}
.${PAGE_TITLE_CLASS}{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#e6e8ec;font:600 15px system-ui,sans-serif;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = CSS;
  document.head.appendChild(el);
}

/** The arrow's accessible name: "Back to the workspace". */
export function backLabel(where: string): string {
  return `Back to ${where}`;
}

/** Whether a key press means "back": a plain Escape, not one that ends an
 *  IME composition. */
export function isBackKey(e: KeyboardEvent): boolean {
  return e.key === "Escape" && !e.isComposing && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
}

export interface PageHeaderOptions {
  /** The page's title, shown right after the arrow. */
  title: string;
  /** Where the arrow returns, finishing "Back to …" ("the workspace",
   *  "Work", "the terminal bash"). */
  backTo: string;
  onBack(): void;
  /** Extra classes for the arrow and the title (a page's own hooks). */
  arrowClass?: string;
  titleClass?: string;
}

/** The arrow and title of a page header; the page puts its other controls
 *  after them. */
export interface PageHeader {
  readonly arrow: HTMLButtonElement;
  readonly title: HTMLElement;
  setTitle(text: string): void;
  /** Where the arrow returns changed (the page was reached another way). */
  setBackTo(where: string): void;
  readonly backTo: string;
}

/**
 * Put the back arrow and the title at the start of `header` (before
 * anything already in it) and answer both. The arrow's tooltip and
 * accessible name say where it returns; Escape is the page's to bind.
 */
export function mountPageHeader(header: HTMLElement, opts: PageHeaderOptions): PageHeader {
  injectStyles();
  const arrow = document.createElement("button");
  arrow.type = "button";
  arrow.className = opts.arrowClass ? `${BACK_ARROW_CLASS} ${opts.arrowClass}` : BACK_ARROW_CLASS;
  arrow.dataset.back = "";
  const glyph = document.createElement("span");
  glyph.setAttribute("aria-hidden", "true");
  glyph.textContent = "←";
  arrow.appendChild(glyph);
  arrow.addEventListener("click", () => opts.onBack());
  const title = document.createElement("span");
  title.className = opts.titleClass ? `${PAGE_TITLE_CLASS} ${opts.titleClass}` : PAGE_TITLE_CLASS;
  title.textContent = opts.title;
  header.prepend(arrow, title);
  let backTo = "";
  const refs: PageHeader = {
    arrow,
    title,
    setTitle(text: string) {
      title.textContent = text;
    },
    setBackTo(where: string) {
      backTo = where;
      arrow.title = `${backLabel(where)} (Esc)`;
      arrow.setAttribute("aria-label", backLabel(where));
    },
    get backTo() {
      return backTo;
    },
  };
  refs.setBackTo(opts.backTo);
  return refs;
}
