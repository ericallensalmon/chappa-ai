// The compact source selector at the top of a view. A view merges every
// trusted source that serves it across the open projects; when there is more
// than one, this row narrows the page to one of them. Environment is one
// stack at a time (each stack is its own environment), so its row has no
// "All" entry; the other views default to "All".

import type { ViewPayload } from "./prs_model";

/** The "every source" choice. */
export const ALL_SOURCES = "*";

export interface PickerOption {
  id: string;
  label: string;
}

/** The choice in force: the remembered one while it is still offered, else
 *  "All" (or, without it, the first source). Null when nothing is offered. */
export function resolvePick(options: readonly PickerOption[], stored: string | null, allowAll: boolean): string | null {
  if (stored !== null && (options.some((o) => o.id === stored) || (allowAll && stored === ALL_SOURCES))) return stored;
  if (allowAll) return ALL_SOURCES;
  return options[0]?.id ?? null;
}

/** Whether a choice keeps every source. */
export function isAll(pick: string | null): boolean {
  return pick === null || pick === ALL_SOURCES;
}

/** The payloads the page shows under `pick`. */
export function filterPayloads(payloads: ViewPayload[], pick: string | null): ViewPayload[] {
  return isAll(pick) ? payloads : payloads.filter((p) => p.sourceId === pick);
}

/** One row of buttons, one per source (plus "All"), hidden while there is
 *  at most one source to choose from. Arrow keys move along the row and
 *  select, as in any radio group. */
export class SourcePicker {
  readonly el: HTMLElement;
  private sig = "";
  private buttons: { id: string; btn: HTMLButtonElement }[] = [];

  constructor(
    private readonly name: string,
    private readonly onPick: (id: string) => void,
  ) {
    this.el = document.createElement("div");
    this.el.className = "chappa-source-picker";
    this.el.setAttribute("role", "radiogroup");
    this.el.setAttribute("aria-label", name);
    this.el.hidden = true;
  }

  update(options: readonly PickerOption[], pick: string | null, allowAll: boolean): void {
    const shown = options.length > 1;
    const sig = JSON.stringify([options, pick, allowAll, shown]);
    if (sig === this.sig) return;
    this.sig = sig;
    this.el.hidden = !shown;
    this.el.textContent = "";
    this.buttons = [];
    if (!shown) return;
    const label = document.createElement("span");
    label.className = "chappa-source-picker-label";
    label.textContent = this.name;
    this.el.appendChild(label);
    const all: PickerOption[] = allowAll ? [{ id: ALL_SOURCES, label: "All" }, ...options] : [...options];
    all.forEach((o, i) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-source-pick";
      btn.dataset.source = o.id;
      btn.textContent = o.label;
      btn.title = o.id === ALL_SOURCES ? "Every source" : o.label;
      btn.setAttribute("role", "radio");
      const on = o.id === pick;
      btn.classList.toggle("selected", on);
      btn.setAttribute("aria-checked", String(on));
      btn.tabIndex = on ? 0 : -1;
      btn.addEventListener("click", () => {
        if (o.id !== pick) this.onPick(o.id);
      });
      btn.addEventListener("keydown", (e) => this.onKey(e, i));
      this.el.appendChild(btn);
      this.buttons.push({ id: o.id, btn });
    });
  }

  private onKey(e: KeyboardEvent, i: number): void {
    const n = this.buttons.length;
    let j: number | null = null;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") j = (i + 1) % n;
    else if (e.key === "ArrowLeft" || e.key === "ArrowUp") j = (i - 1 + n) % n;
    if (j === null) return;
    e.preventDefault();
    const next = this.buttons[j];
    this.onPick(next.id);
    // The row was rebuilt by the pick: focus the new button for that source.
    this.buttons.find((b) => b.id === next.id)?.btn.focus();
  }
}

export const SOURCE_PICKER_CSS = `
.chappa-source-picker{display:flex;flex-wrap:wrap;align-items:center;gap:4px;margin:0 0 10px;}
.chappa-source-picker[hidden]{display:none;}
.chappa-source-picker-label{margin-right:4px;color:#8b949e;font:11px system-ui,sans-serif;}
.chappa-source-pick{padding:3px 10px;border:1px solid #262a31;border-radius:12px;background:#16181d;color:#aab1ba;font:12px system-ui,sans-serif;cursor:pointer;max-width:220px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-source-pick:hover{background:#1d2127;color:#d6d8dc;}
.chappa-source-pick.selected{border-color:#5ea6ff;background:#1d2a3a;color:#e6e8ec;}
.chappa-source-pick:focus-visible{outline:2px solid #5ea6ff;outline-offset:1px;}
`;
