// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { arrowsFor, makeHScroller, stepFor } from "./h_scroller";

/** jsdom lays nothing out: give the track the sizes a real layout would. */
function sized(track: HTMLElement, clientWidth: number, scrollWidth: number): void {
  Object.defineProperty(track, "clientWidth", { configurable: true, value: clientWidth });
  Object.defineProperty(track, "scrollWidth", { configurable: true, value: scrollWidth });
}

describe("horizontal scroller", () => {
  it("shows an arrow only where there is more to see", () => {
    expect(arrowsFor(0, 200, 200)).toEqual({ left: false, right: false });
    expect(arrowsFor(0, 200, 500)).toEqual({ left: false, right: true });
    expect(arrowsFor(150, 200, 500)).toEqual({ left: true, right: true });
    expect(arrowsFor(300, 200, 500)).toEqual({ left: true, right: false });
    // Fractional positions at the end read as the end.
    expect(arrowsFor(299.5, 200, 500)).toEqual({ left: true, right: false });
  });

  it("steps most of a visible width, never a sliver", () => {
    expect(stepFor(200)).toBe(160);
    expect(stepFor(10)).toBe(40);
  });

  it("hides both arrows while everything fits and shows the right one when it does not", () => {
    const s = makeHScroller("Projects");
    document.body.appendChild(s.el);
    sized(s.track, 200, 200);
    s.update();
    expect(s.left.hidden).toBe(true);
    expect(s.right.hidden).toBe(true);
    sized(s.track, 200, 600);
    s.update();
    expect(s.left.hidden).toBe(true);
    expect(s.right.hidden).toBe(false);
    s.el.remove();
  });

  it("scrolls by a step on an arrow press, and sideways on a vertical wheel", () => {
    const s = makeHScroller("Projects");
    document.body.appendChild(s.el);
    sized(s.track, 200, 600);
    const moves: number[] = [];
    s.track.scrollBy = ((opts: ScrollToOptions) => moves.push(opts.left ?? 0)) as typeof s.track.scrollBy;
    s.update();
    s.right.click();
    expect(moves).toEqual([160]);
    const wheel = new WheelEvent("wheel", { deltaY: 50, cancelable: true });
    s.track.dispatchEvent(wheel);
    expect(moves).toEqual([160, 50]);
    expect(wheel.defaultPrevented).toBe(true);
    s.el.remove();
  });
});
