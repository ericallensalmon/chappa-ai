// What Cmd+W closes on macOS. The rule: it never closes something the person
// cannot see. With a dialog or a page (Settings, a view, a drill-in page) in
// front, the key closes that, the way its Escape or back arrow does; only
// with the panes on screen does it close the active pane. The other systems'
// close-pane chord (Ctrl+Shift+W) does not come through here.

import { BACK_ARROW_CLASS } from "./page_header";

/** What one Cmd+W closes. */
export type CloseTarget =
  /** A dialog the focus is in: closed as Escape closes it. */
  | { kind: "dialog"; element: HTMLElement }
  /** A page on screen around the focus: closed by its back arrow. */
  | { kind: "page"; arrow: HTMLElement }
  /** Nothing in front of the panes: the active pane. */
  | { kind: "pane" };

/** Whether `el` is on screen: attached, and neither it nor an ancestor is
 *  hidden. */
function isShown(el: HTMLElement): boolean {
  if (!el.isConnected) return false;
  for (let node: HTMLElement | null = el; node; node = node.parentElement) {
    if (node.hidden) return false;
    const style = getComputedStyle(node);
    if (style.display === "none" || style.visibility === "hidden") return false;
  }
  return true;
}

/** The first back arrow on screen inside `el`, if any. */
function shownArrow(el: Element): HTMLElement | null {
  for (const arrow of el.querySelectorAll<HTMLElement>(`.${BACK_ARROW_CLASS}`)) {
    if (isShown(arrow)) return arrow;
  }
  return null;
}

/**
 * The innermost dialog or page around `focus`: walking out from it, the first
 * element that is a dialog without a back arrow of its own, or that holds a
 * back arrow on screen. Walking out to the document means a page on screen
 * anywhere is in front of the panes, so it is closed rather than the pane
 * behind it. `null` focus starts at the body.
 */
export function cmdWTarget(focus: Element | null): CloseTarget {
  const start = focus ?? document.body;
  for (let el: Element | null = start; el && el !== document.documentElement; el = el.parentElement) {
    const arrow = shownArrow(el);
    if (el.getAttribute("role") === "dialog" && el instanceof HTMLElement && arrow === null) {
      return { kind: "dialog", element: el };
    }
    if (arrow) return { kind: "page", arrow };
  }
  return { kind: "pane" };
}

/**
 * Close what Cmd+W means for `focus`: a dialog by an Escape delivered where
 * the focus is (each dialog's own Escape handling runs), a page by its back
 * arrow, else `closePane`.
 */
export function closeForCmdW(focus: Element | null, closePane: () => void): CloseTarget {
  const target = cmdWTarget(focus);
  if (target.kind === "dialog") {
    const at = focus && target.element.contains(focus) ? focus : target.element;
    at.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  } else if (target.kind === "page") {
    target.arrow.click();
  } else {
    closePane();
  }
  return target;
}
