// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { ALL_SOURCES, SourcePicker, filterPayloads, isAll, resolvePick } from "./source_picker";
import type { ViewPayload } from "./prs_model";

const opts = [
  { id: "a", label: "Alpha" },
  { id: "b", label: "Beta" },
];

describe("resolvePick", () => {
  it("keeps a remembered pick only while it is offered", () => {
    expect(resolvePick(opts, "b", true)).toBe("b");
    expect(resolvePick(opts, "gone", true)).toBe(ALL_SOURCES);
    expect(resolvePick(opts, ALL_SOURCES, true)).toBe(ALL_SOURCES);
  });

  it("without All (one stack at a time) defaults to the first source, or null with none", () => {
    expect(resolvePick(opts, null, false)).toBe("a");
    expect(resolvePick(opts, ALL_SOURCES, false)).toBe("a");
    expect(resolvePick(opts, "gone", false)).toBe("a");
    expect(resolvePick(opts, "b", false)).toBe("b");
    expect(resolvePick([], null, false)).toBeNull();
    expect(resolvePick([], null, true)).toBe(ALL_SOURCES);
  });
});

describe("filterPayloads", () => {
  const p = (sourceId: string): ViewPayload => ({ sourceId, sourceLabel: sourceId, payload: null });
  it("narrows to the picked source; All and null keep everything", () => {
    const all = [p("a"), p("b"), p("a")];
    expect(filterPayloads(all, "a").map((x) => x.sourceId)).toEqual(["a", "a"]);
    expect(filterPayloads(all, ALL_SOURCES)).toBe(all);
    expect(filterPayloads(all, null)).toBe(all);
    expect(isAll(null)).toBe(true);
    expect(isAll("a")).toBe(false);
  });
});

describe("SourcePicker", () => {
  const buttons = (p: SourcePicker): HTMLButtonElement[] => [...p.el.querySelectorAll<HTMLButtonElement>(".chappa-source-pick")];

  it("stays hidden with one source or none", () => {
    const picker = new SourcePicker("Source", () => {});
    picker.update([opts[0]], "a", true);
    expect(picker.el.hidden).toBe(true);
    expect(buttons(picker)).toEqual([]);
    picker.update([], null, true);
    expect(picker.el.hidden).toBe(true);
  });

  it("with several sources lists All first (when allowed), marks the pick, and reports a click on another one", () => {
    const onPick = vi.fn();
    const picker = new SourcePicker("Source", onPick);
    picker.update(opts, ALL_SOURCES, true);
    expect(picker.el.hidden).toBe(false);
    expect(picker.el.getAttribute("role")).toBe("radiogroup");
    expect(buttons(picker).map((b) => b.textContent)).toEqual(["All", "Alpha", "Beta"]);
    expect(buttons(picker).map((b) => b.getAttribute("aria-checked"))).toEqual(["true", "false", "false"]);
    expect(buttons(picker).map((b) => b.tabIndex)).toEqual([0, -1, -1]);
    buttons(picker)[0].click();
    expect(onPick).not.toHaveBeenCalled();
    buttons(picker)[2].click();
    expect(onPick).toHaveBeenCalledWith("b");
  });

  it("one stack at a time: no All entry", () => {
    const picker = new SourcePicker("Stack", () => {});
    picker.update(opts, "b", false);
    expect(buttons(picker).map((b) => b.textContent)).toEqual(["Alpha", "Beta"]);
    expect(picker.el.querySelector(".chappa-source-picker-label")?.textContent).toBe("Stack");
    expect(buttons(picker)[1].classList.contains("selected")).toBe(true);
  });

  it("arrow keys pick the neighbour, wrapping, and keep focus on the picked button", () => {
    let pick = "a";
    const picker: SourcePicker = new SourcePicker("Stack", (id) => {
      pick = id;
      picker.update(opts, pick, false);
    });
    document.body.appendChild(picker.el);
    picker.update(opts, pick, false);
    buttons(picker)[0].focus();
    buttons(picker)[0].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(pick).toBe("b");
    expect(document.activeElement?.textContent).toBe("Beta");
    buttons(picker)[1].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(pick).toBe("a");
    expect(document.activeElement?.textContent).toBe("Alpha");
    picker.el.remove();
  });
});
