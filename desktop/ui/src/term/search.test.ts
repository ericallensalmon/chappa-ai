// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { TerminalPanel } from "./panel";
import { SearchBar, SEARCH_DEBOUNCE_MS, escapeLiteral, isValidRegex } from "./search";
import { frame, mockHostRect, stubApi } from "./test-utils";

afterEach(() => {
  vi.useRealTimers();
});

// --- pure helpers -----------------------------------------------------------

describe("escapeLiteral", () => {
  it("escapes regex metacharacters for a literal search", () => {
    expect(escapeLiteral("1.5*2")).toBe("1\\.5\\*2");
    expect(escapeLiteral("plain")).toBe("plain");
    expect(escapeLiteral("a+b?c(d)[e]{f}")).toBe("a\\+b\\?c\\(d\\)\\[e\\]\\{f\\}");
  });
});

describe("isValidRegex", () => {
  it("accepts real regexes and rejects unbalanced groups", () => {
    expect(isValidRegex(".*error.*")).toBe(true);
    expect(isValidRegex("foo|bar")).toBe(true);
    expect(isValidRegex("(")).toBe(false);
    expect(isValidRegex("[")).toBe(false);
  });
});

// --- the SearchBar state machine ----------------------------------------------

function makeBar() {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const calls: { search: (string | null)[]; nav: ("next" | "prev")[]; close: number } = {
    search: [],
    nav: [],
    close: 0,
  };
  const bar = new SearchBar({
    container,
    onSearch: (re) => calls.search.push(re),
    onNav: (dir) => calls.nav.push(dir),
    onClose: () => {
      calls.close++;
    },
  });
  return { bar, container, calls };
}

describe("SearchBar state machine", () => {
  it("types a literal → exactly one debounced escaped search", () => {
    vi.useFakeTimers();
    const { bar, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.value = "1.5*2";
    input.dispatchEvent(new Event("input"));
    // Six keystrokes but only the settled value is sent.
    expect(calls.search).toEqual([]);
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["1\\.5\\*2"]);
  });

  it("resets the debounce on each keystroke", () => {
    vi.useFakeTimers();
    const { bar, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.value = "er";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS - 1);
    input.value = "err";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(1);
    expect(calls.search).toEqual([]); // the first keystroke's timer was reset
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["err"]);
  });

  it("clears the search when the input empties", () => {
    vi.useFakeTimers();
    const { bar, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.value = "x";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["x"]);
    input.value = "";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["x", null]);
  });

  it("`.*` toggle re-sends the input as a raw regex immediately", () => {
    vi.useFakeTimers();
    const { bar, container, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.value = "1.5*2";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["1\\.5\\*2"]);
    (container.querySelector("button") as HTMLButtonElement).click();
    expect(calls.search).toEqual(["1\\.5\\*2", "1.5*2"]);
  });

  it("an invalid raw regex clears the search, zeroes the count, shows the red border", () => {
    // HOST-RUN RULE: A SEARCH that can't run behaves like no search —
    // no stale highlights or counts survive under a bad pattern.
    vi.useFakeTimers();
    const { bar, container, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.value = "a";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["a"]);
    bar.setCount(12); // a live count from term://search…
    (container.querySelector("button") as HTMLButtonElement).click(); // raw mode
    expect(calls.search).toEqual(["a", "a"]);
    input.value = "(";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["a", "a", null]); // …cleared, not kept
    expect(container.textContent).toContain("0 matches");
    expect(input.style.cssText).toContain("rgb(248, 81, 73)"); // red border
    // Recovery clears the border and sends again.
    input.value = "ok";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(calls.search).toEqual(["a", "a", null, "ok"]);
    expect(input.style.cssText).not.toContain("rgb(248, 81, 73)");
  });

  it("Enter navigates next, Shift+Enter previous, Esc closes", () => {
    const { bar, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(calls.nav).toEqual(["next"]);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true }));
    expect(calls.nav).toEqual(["next", "prev"]);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(calls.close).toBe(1);
  });

  it("Ctrl+F while open refocuses the input, never closes it", () => {
    const { bar, calls } = makeBar();
    bar.open();
    const input = bar.inputElement;
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    expect(calls.close).toBe(0);
    expect(bar.isOpen()).toBe(true);
    expect(document.activeElement).toBe(input);
  });

  it("setCount renders the N-matches label", () => {
    const { bar, container } = makeBar();
    bar.open();
    bar.setCount(0);
    expect(container.textContent).toContain("0 matches");
    bar.setCount(1);
    expect(container.textContent).toContain("1 match");
    bar.setCount(42);
    expect(container.textContent).toContain("42 matches");
  });
});

// --- the panel journey -------------------------------------------------------

async function makePanelJourney() {
  vi.useFakeTimers();
  const { api, onFrame } = stubApi();
  const container = document.createElement("div");
  document.body.appendChild(container);
  const panel = new TerminalPanel({ container, api, platform: "Linux" });
  mockHostRect(container, 800, 400);
  await panel.start();
  onFrame()!(frame({ cols: 30, rows: 5, seq: 1 }));
  return { api, container, panel };
}

describe("panel search journey", () => {
  it("Ctrl+F opens → debounced literal search → count → Enter nav → .* toggle → Esc", async () => {
    const { api, container, panel } = await makePanelJourney();

    // Ctrl+F opens the overlay and focuses its input.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    const input = container.querySelector<HTMLInputElement>("input")!;
    const bar = input.closest("div") as HTMLDivElement;
    expect(bar.style.display).toBe("flex");
    expect(document.activeElement).toBe(input);

    // Type 1.5*2 → exactly ONE debounced search call, escaped as a literal.
    input.value = "1.5*2";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(api.search).toHaveBeenCalledTimes(1);
    expect(api.search).toHaveBeenCalledWith(42, "1\\.5\\*2");

    // A SearchStatus arrival renders the count.
    panel.handleSearchStatus(7);
    expect(container.textContent).toContain("7 matches");

    // Enter navigates next; Shift+Enter previous.
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(api.searchNav).toHaveBeenCalledWith(42, "next");
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true }));
    expect(api.searchNav).toHaveBeenCalledWith(42, "prev");

    // The .* toggle re-sends the current input as a raw regex (immediate).
    (bar.querySelector("button") as HTMLButtonElement).click();
    expect(api.search).toHaveBeenCalledTimes(2);
    expect(api.search).toHaveBeenLastCalledWith(42, "1.5*2");

    // Esc → search(None), overlay closes, the terminal regains typing.
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(api.search).toHaveBeenLastCalledWith(42, null);
    expect(bar.style.display).toBe("none");
    const textarea = container.querySelector<HTMLTextAreaElement>(".chappa-textarea")!;
    expect(document.activeElement).toBe(textarea);

    panel.dispose();
  });

  it("Ctrl+F while open REFOCUSES, never closes", async () => {
    const { api, container, panel } = await makePanelJourney();

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    const input = container.querySelector<HTMLInputElement>("input")!;
    input.blur();
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    const bar = input.closest("div") as HTMLDivElement;
    expect(bar.style.display).toBe("flex");
    expect(document.activeElement).toBe(input);
    expect(api.search).not.toHaveBeenCalledWith(42, null); // never closed

    panel.dispose();
  });

  it("invalid raw regex clears the search, zeroes the count, red border", async () => {
    const { api, container, panel } = await makePanelJourney();

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    const input = container.querySelector<HTMLInputElement>("input")!;
    const bar = input.closest("div") as HTMLDivElement;
    // A valid literal first, then switch to raw mode.
    input.value = "abc";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(api.search).toHaveBeenLastCalledWith(42, "abc");
    panel.handleSearchStatus(9);
    expect(container.textContent).toContain("9 matches");
    (bar.querySelector("button") as HTMLButtonElement).click();
    expect(api.search).toHaveBeenLastCalledWith(42, "abc"); // re-sent raw
    input.value = "(";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    // A search that can't run is no search: cleared + count 0 + red border.
    expect(api.search).toHaveBeenLastCalledWith(42, null);
    expect(container.textContent).toContain("0 matches");
    expect(input.style.cssText).toContain("rgb(248, 81, 73)");

    panel.dispose();
  });

  it("clicking the terminal returns typing but the search bar STAYS open", async () => {
    // Host-run verdict: the click only moves focus — the bar and
    // its live search survive. Esc is the close-and-clear path.
    const { api, container, panel } = await makePanelJourney();

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true }),
    );
    const input = container.querySelector<HTMLInputElement>("input")!;
    const bar = input.closest("div") as HTMLDivElement;
    expect(bar.style.display).toBe("flex");
    input.value = "abc";
    input.dispatchEvent(new Event("input"));
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS);
    expect(api.search).toHaveBeenLastCalledWith(42, "abc");

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, clientX: 4, clientY: 4 }));
    await vi.advanceTimersByTimeAsync(0);

    expect(bar.style.display).toBe("flex");
    expect(api.search).not.toHaveBeenCalledWith(42, null); // search stays live
    const textarea = container.querySelector<HTMLTextAreaElement>(".chappa-textarea")!;
    expect(document.activeElement).toBe(textarea);

    // Esc from the bar still closes and clears.
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(bar.style.display).toBe("none");
    expect(api.search).toHaveBeenLastCalledWith(42, null);

    panel.dispose();
  });

  it("Ctrl+Alt+↑/↓ reserved for marks never opens or types in the terminal", async () => {
    const { api, container, panel } = await makePanelJourney();

    const textarea = container.querySelector<HTMLTextAreaElement>(".chappa-textarea")!;
    textarea.focus();
    textarea.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, bubbles: true, cancelable: true }),
    );
    expect(api.writeKey).not.toHaveBeenCalled();
    const bar = container.querySelector<HTMLInputElement>("input")!.closest("div") as HTMLDivElement;
    expect(bar.style.display).toBe("none"); // search stays closed

    panel.dispose();
  });
});
