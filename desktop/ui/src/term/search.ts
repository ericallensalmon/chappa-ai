// Search-bar overlay state machine. Owns the DOM (input + `.*`
// toggle + "N matches" label), the 150ms debounce, the literal-escaping of
// the default input, and the raw-regex toggle (invalid regex → red border,
// the search clears and the count flips to 0 — a search that can't run
// behaves like no search, per the host-run rule). Esc → onClose (the
// panel clears the search and returns typing to the terminal); Ctrl+F while
// open → refocus, never close.

/** Debounce window for search-as-you-type. */
export const SEARCH_DEBOUNCE_MS = 150;

/** Regex metacharacters escaped for a literal search (the default input
 *  mode). `1.5*2` → `1\.5\*2` — the terminal's regex search then matches it
 *  literally. */
export function escapeLiteral(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Whether `re` compiles as a JS regex (the frontend's validity proxy for the
 *  Rust regex syntax — close enough for the red-border gate). */
export function isValidRegex(re: string): boolean {
  try {
    new RegExp(re);
    return true;
  } catch {
    return false;
  }
}

export interface SearchBarOptions {
  /** The pane the bar anchors to (top-right). Positioned via CSS. */
  container: HTMLElement;
  /** Debounced search payload: an escaped literal or a raw regex. Null when
   *  the input is empty OR the raw regex is invalid — either way the live
   *  search is cleared. */
  onSearch: (regex: string | null) => void;
  /** Enter / Shift+Enter. */
  onNav: (dir: "next" | "prev") => void;
  /** Esc: the panel clears the search and refocuses the terminal. */
  onClose: () => void;
}

const BAR_CSS =
  "position:absolute;top:6px;right:14px;z-index:30;display:none;align-items:center;gap:6px;" +
  "background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px 8px;" +
  "box-shadow:0 4px 16px rgba(0,0,0,.5);";
const INPUT_CSS =
  "width:180px;background:#0d0e10;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;" +
  "padding:2px 6px;font:13px ui-monospace,monospace;outline:none;";
const INPUT_INVALID_CSS =
  "width:180px;background:#0d0e10;color:#d6d8dc;border:1px solid #f85149;border-radius:4px;" +
  "padding:2px 6px;font:13px ui-monospace,monospace;outline:none;";
const TOGGLE_CSS =
  "border:1px solid #2a2c31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:11px ui-monospace,monospace;cursor:pointer;padding:1px 6px;";
const COUNT_CSS = "font:11px ui-monospace,monospace;color:#8b949e;min-width:0;white-space:nowrap;";

export class SearchBar {
  private readonly opts: SearchBarOptions;
  private readonly wrap: HTMLDivElement;
  private readonly input: HTMLInputElement;
  private readonly toggle: HTMLButtonElement;
  private readonly count: HTMLSpanElement;

  private rawRegex = false;
  private opened = false;
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(opts: SearchBarOptions) {
    this.opts = opts;
    this.wrap = document.createElement("div");
    this.wrap.style.cssText = BAR_CSS;

    this.input = document.createElement("input");
    this.input.type = "text";
    this.input.placeholder = "Search…";
    this.input.style.cssText = INPUT_CSS;
    this.input.spellcheck = false;

    this.toggle = document.createElement("button");
    this.toggle.textContent = ".*";
    this.toggle.title = "Treat input as a raw regular expression";
    this.toggle.style.cssText = TOGGLE_CSS;

    this.count = document.createElement("span");
    this.count.style.cssText = COUNT_CSS;
    this.count.textContent = "";

    this.wrap.append(this.input, this.toggle, this.count);
    opts.container.appendChild(this.wrap);

    // Debounced search-as-you-type; the last keystroke wins.
    this.input.addEventListener("input", () => this.schedule());
    this.toggle.addEventListener("click", () => {
      this.rawRegex = !this.rawRegex;
      // The active-mode indicator is the border/color, not the label.
      this.toggle.style.borderColor = this.rawRegex ? "#5ea6ff" : "#2a2c31";
      this.toggle.style.color = this.rawRegex ? "#5ea6ff" : "#8b949e";
      // Toggle re-sends immediately (no debounce): the raw vs escaped form.
      this.flush();
      this.input.focus();
    });
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        this.opts.onNav(e.shiftKey ? "prev" : "next");
      } else if (e.key === "Escape") {
        e.preventDefault();
        this.opts.onClose();
      } else if (e.key.toLowerCase() === "f" && e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) {
        // Ctrl+F while the bar is open refocuses it — it never closes.
        e.preventDefault();
        this.input.focus();
        this.input.select();
      }
    });
  }

  /** Show (or, when already open, refocus) the bar. */
  open(): void {
    this.opened = true;
    this.wrap.style.display = "flex";
    this.input.focus();
    this.input.select();
  }

  /** Hide the bar. The panel sends search(None) separately via onClose. */
  close(): void {
    this.opened = false;
    this.wrap.style.display = "none";
    this.clearTimer();
  }

  isOpen(): boolean {
    return this.opened;
  }

  /** "N matches" from `term://search` (a scan-time snapshot; may go stale). */
  setCount(total: number): void {
    this.count.textContent = `${total} match${total === 1 ? "" : "es"}`;
  }

  /** Drive the state machine straight from the input element (tests). */
  get inputElement(): HTMLInputElement {
    return this.input;
  }

  dispose(): void {
    this.clearTimer();
    this.wrap.remove();
  }

  private schedule(): void {
    this.clearTimer();
    this.timer = setTimeout(() => this.flush(), SEARCH_DEBOUNCE_MS);
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  /** Send the current input under the active mode. Empty input clears the
   *  search and blanks the count; an invalid raw regex is a search that
   *  can't run — red border, search cleared, count flipped to 0 (no stale
   *  highlights or counts under a bad pattern). */
  private flush(): void {
    this.clearTimer();
    const text = this.input.value;
    if (text === "") {
      this.setValid(true);
      this.count.textContent = "";
      this.opts.onSearch(null);
      return;
    }
    if (this.rawRegex) {
      if (!isValidRegex(text)) {
        this.setValid(false);
        this.setCount(0);
        this.opts.onSearch(null);
        return;
      }
      this.setValid(true);
      this.opts.onSearch(text);
    } else {
      this.setValid(true);
      this.opts.onSearch(escapeLiteral(text));
    }
  }

  private setValid(valid: boolean): void {
    this.input.style.cssText = valid ? INPUT_CSS : INPUT_INVALID_CSS;
  }
}
