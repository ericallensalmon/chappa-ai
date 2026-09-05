// Segmented value picker: a horizontal row of text options with the active
// one underlined in the accent colour. Used
// for the notification level (All | Important | None) and for scroll speed —
// hence a standalone reusable component rather than a settings-pane local.
//
// Structure/style conventions follow search.ts: module-level CSS consts, a
// plain class owning its DOM, dispose() detaches it. The active underline is
// set INLINE (not by a stylesheet rule) so it is observable without a layout
// engine — jsdom resolves inline styles only.

/** Accent (active) and muted (inactive) — the house palette. */
export const SEGMENTED_ACCENT = "#5ea6ff";
export const SEGMENTED_MUTED = "#8b949e";

// FLAT — a row of text options with the active one underlined, and no card
// chrome around it. An earlier
// bg+border wrapper made the picker read as a stuck-open dropdown in the
// narrow project rail (seen on the host).
const WRAP_CSS = "display:inline-flex;align-items:stretch;gap:2px;";
const OPTION_CSS =
  "background:none;border:0;border-bottom:2px solid transparent;border-radius:4px 4px 0 0;" +
  "padding:3px 9px 2px;font:12px ui-monospace,monospace;cursor:pointer;color:" +
  SEGMENTED_MUTED +
  ";";

export interface SegmentedOption<T> {
  value: T;
  label: string;
}

export interface SegmentedOptions<T> {
  options: SegmentedOption<T>[];
  /** The active value. Matched by `Object.is`, so use primitives. */
  value: T;
  onChange: (value: T) => void;
  /** Accessible name for the group (e.g. "Scroll wheel speed"). */
  label?: string;
}

/**
 * A row of text options with exactly one active. Options are real
 * `<button>`s: natively focusable and Enter/Space-activatable, with ←/→
 * moving focus across the row (the radiogroup convention).
 */
export class Segmented<T> {
  readonly element: HTMLDivElement;
  private readonly buttons: HTMLButtonElement[] = [];
  private readonly values: T[];
  private readonly onChange: (value: T) => void;
  private value: T;

  constructor(opts: SegmentedOptions<T>) {
    this.values = opts.options.map((o) => o.value);
    this.onChange = opts.onChange;
    this.value = opts.value;

    this.element = document.createElement("div");
    this.element.className = "chappa-segmented";
    this.element.style.cssText = WRAP_CSS;
    this.element.setAttribute("role", "radiogroup");
    if (opts.label) this.element.setAttribute("aria-label", opts.label);

    opts.options.forEach((opt, i) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-segmented-option";
      btn.textContent = opt.label;
      btn.style.cssText = OPTION_CSS;
      btn.setAttribute("role", "radio");
      // Keep the terminal's hidden textarea from losing focus to a press is
      // NOT wanted here (the pane is a real focus target) — but the click must
      // still land, so no preventDefault games.
      btn.addEventListener("click", () => this.select(opt.value));
      btn.addEventListener("keydown", (e) => this.onKeydown(e, i));
      this.buttons.push(btn);
      this.element.appendChild(btn);
    });
    this.paint();
  }

  /** The active value. */
  getValue(): T {
    return this.value;
  }

  /** Set the active value WITHOUT firing onChange (external adoption of the
   *  canonical settings value). Unknown values leave the row unchanged. */
  setValue(value: T): void {
    if (!this.values.some((v) => Object.is(v, value))) return;
    this.value = value;
    this.paint();
  }

  dispose(): void {
    this.element.remove();
  }

  private select(value: T): void {
    // Re-clicking the active option is a no-op: the store's write path is
    // "every change persists", and a redundant set_settings is still a disk
    // write plus a broadcast to every actor.
    if (Object.is(value, this.value)) return;
    this.value = value;
    this.paint();
    this.onChange(value);
  }

  private onKeydown(e: KeyboardEvent, index: number): void {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const dir = e.key === "ArrowRight" ? 1 : -1;
    const next = (index + dir + this.buttons.length) % this.buttons.length;
    this.buttons[next].focus();
    this.select(this.values[next]);
  }

  private paint(): void {
    this.buttons.forEach((btn, i) => {
      const active = Object.is(this.values[i], this.value);
      btn.style.borderBottomColor = active ? SEGMENTED_ACCENT : "transparent";
      btn.style.color = active ? SEGMENTED_ACCENT : SEGMENTED_MUTED;
      btn.setAttribute("aria-checked", active ? "true" : "false");
      btn.classList.toggle("active", active);
    });
  }
}
