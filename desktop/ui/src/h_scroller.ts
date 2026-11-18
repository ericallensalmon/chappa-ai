// A one-line strip that scrolls sideways, with an arrow at each end that
// shows only while there is more to see in that direction. Used for the
// project tiles: they stay on one line however many projects are open, and
// the arrows (or a mouse wheel over the strip) bring the rest into view.

export const H_SCROLLER_CSS = `
.chappa-hscroll{display:flex;align-items:center;gap:2px;flex:1 1 auto;min-width:0;}
.chappa-hscroll-track{display:flex;align-items:center;gap:8px;flex:1 1 auto;min-width:0;overflow-x:auto;overflow-y:hidden;scrollbar-width:none;scroll-behavior:smooth;}
.chappa-hscroll-track::-webkit-scrollbar{display:none;}
.chappa-hscroll-arrow{flex:none;width:16px;height:32px;border:0;border-radius:6px;background:none;color:#8b949e;font:14px system-ui,sans-serif;line-height:1;cursor:pointer;padding:0;}
.chappa-hscroll-arrow:hover{background:#1d2127;color:#e6e8ec;}
.chappa-hscroll-arrow[hidden]{display:none;}
`;

let stylesInjected = false;

function injectStyles(): void {
  if (stylesInjected || typeof document === "undefined") return;
  stylesInjected = true;
  const el = document.createElement("style");
  el.textContent = H_SCROLLER_CSS;
  document.head.appendChild(el);
}

/** What one arrow press moves: most of the visible width, so the next
 *  tiles come into view with a little overlap. */
export function stepFor(visibleWidth: number): number {
  return Math.max(40, Math.round(visibleWidth * 0.8));
}

/** Which arrows a strip needs: more to the left, more to the right. A pixel
 *  of slack absorbs fractional scroll positions. */
export function arrowsFor(scrollLeft: number, clientWidth: number, scrollWidth: number): { left: boolean; right: boolean } {
  return { left: scrollLeft > 1, right: scrollLeft + clientWidth < scrollWidth - 1 };
}

export interface HScroller {
  /** The strip: left arrow, track, right arrow. */
  el: HTMLElement;
  /** Where the items go. */
  track: HTMLElement;
  left: HTMLButtonElement;
  right: HTMLButtonElement;
  /** Re-read the sizes and show or hide the arrows (after the items change). */
  update(): void;
  /** Scroll `item` into view (the selected tile). */
  reveal(item: HTMLElement): void;
}

export function makeHScroller(label: string): HScroller {
  injectStyles();
  const el = document.createElement("div");
  el.className = "chappa-hscroll";
  const track = document.createElement("div");
  track.className = "chappa-hscroll-track";
  track.setAttribute("role", "group");
  track.setAttribute("aria-label", label);
  const arrow = (glyph: string, title: string, dir: -1 | 1): HTMLButtonElement => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "chappa-hscroll-arrow";
    b.textContent = glyph;
    b.title = title;
    b.hidden = true;
    // Keep the terminal's focus; the click still lands.
    b.addEventListener("mousedown", (e) => e.preventDefault());
    b.addEventListener("click", () => {
      track.scrollBy({ left: dir * stepFor(track.clientWidth) });
      // Smooth scrolling ends later; the scroll event updates again then.
      update();
    });
    return b;
  };
  const left = arrow("‹", "Scroll left", -1);
  const right = arrow("›", "Scroll right", 1);
  el.append(left, track, right);

  const update = (): void => {
    const need = arrowsFor(track.scrollLeft, track.clientWidth, track.scrollWidth);
    left.hidden = !need.left;
    right.hidden = !need.right;
  };
  track.addEventListener("scroll", update, { passive: true });
  // A vertical wheel over the strip scrolls it sideways (most mice have no
  // horizontal wheel).
  track.addEventListener(
    "wheel",
    (e) => {
      if (Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return;
      if (track.scrollWidth <= track.clientWidth) return;
      e.preventDefault();
      track.scrollBy({ left: e.deltaY });
    },
    { passive: false },
  );
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(update).observe(track);

  const reveal = (item: HTMLElement): void => {
    if (typeof item.scrollIntoView === "function") item.scrollIntoView({ block: "nearest", inline: "nearest" });
    update();
  };
  return { el, track, left, right, update, reveal };
}
