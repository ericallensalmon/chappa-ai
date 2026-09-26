// The settings pane's shared building blocks: a titled SECTION whose rows sit
// in one bordered card, and a ROW that carries its title and explanation on
// the left with the control on the right. Every tab (and the sections that
// live in their own modules) composes these, so the pane reads as one system
// instead of three styles of row.
//
// Styles are inline where a test needs to observe them without a layout
// engine (jsdom resolves inline styles only); the sibling divider between
// rows and the hover/active states come from one injected stylesheet, since
// `:first-child` and `:hover` cannot be expressed inline.

export const INPUT_CSS =
  "background:#0d0e10;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:4px 7px;" +
  "font:12px ui-monospace,monospace;outline:none;min-width:0;";
export const SELECT_CSS =
  "background:#16181d;color:#d6d8dc;border:1px solid #2a2c31;border-radius:6px;padding:5px 8px;" +
  "font:13px system-ui,sans-serif;outline:none;";
export const SMALL_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:11px system-ui,sans-serif;cursor:pointer;padding:3px 8px;";
export const CHECKBOX_CSS = "flex:none;width:16px;height:16px;margin:0;accent-color:#5ea6ff;";
export const HINT_CSS = "font:12px system-ui,sans-serif;color:#8b949e;line-height:1.45;padding:2px 0 6px;";

const SECTION_CSS = "padding:18px 0 4px;";
const SECTION_LABEL_CSS =
  "font:600 12px system-ui,sans-serif;color:#adb3bd;letter-spacing:.06em;text-transform:uppercase;padding-bottom:2px;";
const SECTION_DESC_CSS = "font:12px system-ui,sans-serif;color:#8b949e;line-height:1.45;padding-bottom:10px;";
const CARD_CSS = "border:1px solid #22252a;border-radius:8px;background:#101216;overflow:hidden;";
// `flex-wrap` + the text block's basis is the label-overlap guard: the text
// owns a stable 240px basis, the control right-aligns via margin-left:auto,
// and a control too wide for the remainder (the 9-option line-height picker)
// wraps to its own line rather than sliding over the words.
const ROW_CSS = "display:flex;align-items:center;flex-wrap:wrap;gap:8px 16px;padding:12px 16px;";
const ROW_TEXT_CSS = "flex:1 1 240px;min-width:0;display:flex;flex-direction:column;gap:3px;";
const ROW_TITLE_CSS = "font:13px system-ui,sans-serif;color:#d6d8dc;";
const ROW_DESC_CSS = "font:12px system-ui,sans-serif;color:#8b949e;line-height:1.45;";
const ROW_CONTROL_CSS = "flex:0 1 auto;margin-left:auto;display:flex;align-items:center;";
const BLOCK_CSS = "padding:10px 16px 12px;";

const SHEET = `
.chappa-settings-row + .chappa-settings-row,
.chappa-settings-block + .chappa-settings-row,
.chappa-settings-row + .chappa-settings-block,
.chappa-settings-block + .chappa-settings-block{border-top:1px solid #1c1f24;}
.chappa-settings-close:hover{color:#d6d8dc;}
.chappa-settings-tab:hover{color:#d6d8dc;}
.chappa-settings-tab[aria-selected="true"]{background:#24272e;color:#e6e8ec;}
.chappa-settings-tab:focus-visible{outline:1px solid #5ea6ff;outline-offset:1px;}
`;

let injected = false;

/** Append the settings stylesheet once per document. */
export function injectSettingsStyles(): void {
  if (injected || typeof document === "undefined") return;
  injected = true;
  const el = document.createElement("style");
  el.textContent = SHEET;
  document.head.appendChild(el);
}

/** Unique-id source for the `<label for>` associations (a pane can be built
 *  more than once per document in tests). */
let rowIds = 0;

export interface SettingsSection {
  /** The whole section: label, optional description, card. */
  element: HTMLDivElement;
  /** The card the rows go into. */
  card: HTMLDivElement;
}

/** A titled section: uppercase label, optional one-line description, and the
 *  card that holds its rows. */
export function settingsSection(label: string, description?: string): SettingsSection {
  const element = document.createElement("div");
  element.className = "chappa-settings-section";
  element.style.cssText = SECTION_CSS;
  const head = document.createElement("div");
  head.className = "chappa-settings-section-label";
  head.style.cssText = SECTION_LABEL_CSS;
  head.textContent = label;
  element.appendChild(head);
  if (description) {
    const desc = document.createElement("div");
    desc.style.cssText = SECTION_DESC_CSS;
    desc.textContent = description;
    element.appendChild(desc);
  }
  const card = document.createElement("div");
  card.className = "chappa-settings-card";
  card.style.cssText = CARD_CSS;
  element.appendChild(card);
  return { element, card };
}

/**
 * One setting: title (+ optional explanation) on the left, the control on
 * the right. The title is a `<label for=…>` (never a wrapping `<label>`: a
 * wrapper forwards its click to the first labelable descendant, which for
 * the segmented pickers and the font-size stepper would silently press their
 * first BUTTON — clicking the words "Scroll wheel speed" would set 1x).
 */
export function settingsRow(label: string, control: HTMLElement, description?: string): HTMLDivElement {
  const row = document.createElement("div");
  row.className = "chappa-settings-row";
  row.style.cssText = ROW_CSS;
  const text = document.createElement("div");
  text.className = "chappa-settings-row-text";
  text.style.cssText = ROW_TEXT_CSS;
  const title = document.createElement("label");
  title.style.cssText = ROW_TITLE_CSS;
  title.textContent = label;
  if (!control.id) control.id = `chappa-set-${++rowIds}`;
  title.htmlFor = control.id;
  text.appendChild(title);
  if (description) {
    const desc = document.createElement("div");
    desc.style.cssText = ROW_DESC_CSS;
    desc.textContent = description;
    text.appendChild(desc);
  }
  const slot = document.createElement("div");
  slot.style.cssText = ROW_CONTROL_CSS;
  slot.appendChild(control);
  row.append(text, slot);
  return row;
}

/** A free-form card member (a list, an add line) with the row padding, so it
 *  sits flush with the rows around it and shares their divider. */
export function settingsBlock(...children: HTMLElement[]): HTMLDivElement {
  const block = document.createElement("div");
  block.className = "chappa-settings-block";
  block.style.cssText = BLOCK_CSS;
  block.append(...children);
  return block;
}

/** A muted explanatory line, for the places a row description cannot go. */
export function hint(text: string): HTMLDivElement {
  const el = document.createElement("div");
  el.style.cssText = HINT_CSS;
  el.textContent = text;
  return el;
}

/** A checkbox bound to one boolean setting key, committing on change. */
export function checkbox(): HTMLInputElement {
  const input = document.createElement("input");
  input.type = "checkbox";
  input.style.cssText = CHECKBOX_CSS;
  return input;
}
