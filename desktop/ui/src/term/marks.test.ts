// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { TerminalPanel } from "./panel";
import { MARKS_CAP, Marks } from "./marks";
import { frame, mockHostRect, stubApi } from "./test-utils";

// Rows are BUFFER-ABSOLUTE (index from the top of scrollback), matching what
// the actor emits: history_len + cursor row at mark time. The frame header's
// historyLen maps a stored row onto the current grid (grid = row - historyLen)
// — which is exactly how marks stay navigable while history keeps growing.

// --- the pure math ----------------------------------------------------------

describe("Marks.offsetForMark", () => {
  it("centers the mark at viewport row floor(rows/2)", () => {
    const m = new Marks();
    // 24-row viewport, 30 lines of history: mark at absolute row 20 sits at
    // grid -10 → offset 12-(-10) = 22.
    expect(m.offsetForMark(20, 30, 24)).toBe(22);
    // A mark already on screen (grid 5) scrolls up just enough to center it.
    expect(m.offsetForMark(35, 30, 24)).toBe(7);
    // A mark below the center needs no scroll (offset clamps at 0).
    expect(m.offsetForMark(50, 30, 24)).toBe(0);
    // The buffer-top mark clamps at the history limit.
    expect(m.offsetForMark(0, 30, 24)).toBe(30);
  });

  it("history growth between mark and jump shifts the offset", () => {
    // THE bug class the old grid-line scheme had: the same mark needs a
    // bigger offset after more output scrolled past.
    const m = new Marks();
    expect(m.offsetForMark(25, 30, 24)).toBe(17);
    expect(m.offsetForMark(25, 40, 24)).toBe(27);
  });
});

describe("Marks.nav", () => {
  it("prev = newest mark above the viewport center; next = oldest below", () => {
    const m = new Marks();
    m.add(10);
    m.add(20);
    m.add(25);
    // rows 24, history 30 → center absolute row = 30+12-offset.
    expect(m.nav("prev", 0, 24, 30)).toBe(25);
    expect(m.nav("next", 0, 24, 30)).toBeNull(); // nothing below the live view
    // At offset 22 (center 20): prev skips the centered 20 → 10; next → 25.
    expect(m.nav("prev", 22, 24, 30)).toBe(10);
    expect(m.nav("next", 22, 24, 30)).toBe(25);
  });

  it("returns null with no marks in that direction", () => {
    const m = new Marks();
    expect(m.nav("prev", 0, 24, 30)).toBeNull();
    expect(m.nav("next", 0, 24, 30)).toBeNull();
  });
});

describe("Marks.add", () => {
  it("ignores out-of-order/duplicate rows (stream order)", () => {
    const m = new Marks();
    m.add(10);
    m.add(10); // duplicate (re-emitted prompt on the same line)
    m.add(11);
    m.add(5); // older than the last — ignored
    expect(m.snapshot()).toEqual([10, 11]);
  });

  it("a synthetic and a real mark on the SAME row dedup to one", () => {
    // With `synthetic_prompt_marks` on, a plain shell that also emits OSC 133
    // produces two PromptStart events for one prompt: the Enter-derived
    // synthetic one and the scanner's real one, both carrying the same
    // buffer-absolute row. The monotonic guard makes the second a no-op —
    // this is what lets the two sources double up harmlessly.
    const m = new Marks();
    m.add(40); // synthetic (unmodified Enter, main screen)
    expect(m.snapshot()).toEqual([40]);
    m.add(40); // real OSC 133 PromptStart for the same line
    expect(m.snapshot()).toEqual([40]);
    // …and the guard is `<=`, so an out-of-order older row is dropped too.
    m.add(39);
    expect(m.snapshot()).toEqual([40]);
    m.add(41);
    expect(m.snapshot()).toEqual([40, 41]);
  });

  it("caps at MARKS_CAP, dropping the oldest", () => {
    const m = new Marks();
    for (let i = 0; i < MARKS_CAP + 5; i++) m.add(i);
    expect(m.snapshot().length).toBe(MARKS_CAP);
    expect(m.snapshot()[0]).toBe(5);
  });
});

// --- the panel journey ------------------------------------------------------

async function makeMarksJourney() {
  const { api, onFrame } = stubApi();
  const container = document.createElement("div");
  const panel = new TerminalPanel({ container, api, platform: "Linux" });
  mockHostRect(container, 800, 400);
  await panel.start();
  return { api, onFrame, container, panel };
}

/** A frame that actually covers all 5 rows (so the decoded store is 5 rows —
 *  an empty frame decodes to a 1-row grid and rows the nav math off). */
function frame5(opts: { seq: number; displayOffset?: number; historyLen?: number }): ArrayBuffer {
  return frame({
    cols: 30,
    rows: 5,
    seq: opts.seq,
    displayOffset: opts.displayOffset,
    historyLen: opts.historyLen,
    content: { 0: [], 1: [], 2: [], 3: [], 4: [] },
  });
}

describe("panel marks journey", () => {
  it("records marks, grows history, Ctrl+Alt+↑ jumps to the exact offset, ↓ returns", async () => {
    const { api, onFrame, panel } = await makeMarksJourney();
    // 5-row viewport, center row floor(5/2)=2. Marks at absolute rows
    // 10/20/25 while history was 30.
    onFrame()!(frame5({ seq: 1, displayOffset: 0, historyLen: 30 }));
    panel.handlePromptMark("prompt_start", 10);
    panel.handlePromptMark("prompt_start", 20);
    panel.handlePromptMark("prompt_start", 25);

    // History grows to 40 while the panel is open; the same marks now need
    // BIGGER offsets — the real-events drift the old grid-line scheme broke.
    onFrame()!(frame5({ seq: 2, displayOffset: 0, historyLen: 40 }));

    // Ctrl+Alt+↑ (prev): center abs 42 → newest above → 25 → offset
    // 2-(25-40) = 17.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    expect(api.setDisplayOffset).toHaveBeenCalledWith(42, 17);

    // A frame confirms the jump — the next prev computes from it.
    onFrame()!(frame5({ seq: 3, displayOffset: 17, historyLen: 40 }));
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    // Center abs 42-17 = 25 → prev skips the centered 25 → 20 → offset 22.
    expect(api.setDisplayOffset).toHaveBeenLastCalledWith(42, 22);

    // Ctrl+Alt+↓ (next): center abs 42-22 = 20 → next → 25 → back to 17.
    onFrame()!(frame5({ seq: 4, displayOffset: 22, historyLen: 40 }));
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowDown", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    expect(api.setDisplayOffset).toHaveBeenLastCalledWith(42, 17);

    // The keydown never reached the terminal.
    expect(api.writeKey).not.toHaveBeenCalled();

    panel.dispose();
  });

  it("a mark older than the reachable scrollback clamps to the buffer top", async () => {
    const { api, onFrame, panel } = await makeMarksJourney();
    onFrame()!(frame5({ seq: 1, displayOffset: 0, historyLen: 20 }));
    panel.handlePromptMark("prompt_start", 0); // the very top of the buffer

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    // Centering row 0 wants offset 2+20 = 22 → clamps to the 20 reachable.
    expect(api.setDisplayOffset).toHaveBeenCalledWith(42, 20);
    panel.dispose();
  });

  it("ignores non-prompt-start mark kinds", async () => {
    const { api, onFrame, panel } = await makeMarksJourney();
    onFrame()!(frame5({ seq: 1, displayOffset: 0, historyLen: 20 }));
    panel.handlePromptMark("prompt_end", 15);
    panel.handlePromptMark("command_start", 15);
    panel.handlePromptMark("command_end", 15);

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    expect(api.setDisplayOffset).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("nav with no marks is a no-op", async () => {
    const { api, onFrame, panel } = await makeMarksJourney();
    onFrame()!(frame5({ seq: 1, displayOffset: 0, historyLen: 20 }));
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    expect(api.setDisplayOffset).not.toHaveBeenCalled();
    panel.dispose();
  });
});
