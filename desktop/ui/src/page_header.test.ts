// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { BACK_ARROW_CLASS, PAGE_TITLE_CLASS, backLabel, isBackKey, mountPageHeader } from "./page_header";

describe("mountPageHeader", () => {
  it("puts the arrow first and the title right after it, before what the header already holds", () => {
    const header = document.createElement("div");
    const tools = document.createElement("input");
    header.appendChild(tools);
    const onBack = vi.fn();
    const h = mountPageHeader(header, { title: "Work", backTo: "the workspace", onBack, arrowClass: "x-back", titleClass: "x-title" });
    expect([...header.children]).toEqual([h.arrow, h.title, tools]);
    expect(h.arrow.classList.contains(BACK_ARROW_CLASS) && h.arrow.classList.contains("x-back")).toBe(true);
    expect(h.title.classList.contains(PAGE_TITLE_CLASS) && h.title.classList.contains("x-title")).toBe(true);
    // The arrow is the only visible text of the control; where it returns
    // is its tooltip and accessible name.
    expect(h.arrow.textContent).toBe("←");
    expect(h.arrow.type).toBe("button");
    expect(h.arrow.getAttribute("aria-label")).toBe("Back to the workspace");
    expect(h.arrow.title).toBe("Back to the workspace (Esc)");
    expect(h.title.textContent).toBe("Work");
    h.arrow.click();
    expect(onBack).toHaveBeenCalledTimes(1);
    h.setBackTo("Needs you");
    expect(h.backTo).toBe("Needs you");
    expect(h.arrow.getAttribute("aria-label")).toBe(backLabel("Needs you"));
    h.setTitle("Work items");
    expect(h.title.textContent).toBe("Work items");
  });

  it("Escape alone is the back key; a composing or modified Escape is not", () => {
    const ev = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);
    expect(isBackKey(ev({ key: "Escape" }))).toBe(true);
    expect(isBackKey(ev({ key: "Escape", isComposing: true }))).toBe(false);
    expect(isBackKey(ev({ key: "Escape", ctrlKey: true }))).toBe(false);
    expect(isBackKey(ev({ key: "Escape", shiftKey: true }))).toBe(false);
    expect(isBackKey(ev({ key: "Backspace" }))).toBe(false);
  });
});
