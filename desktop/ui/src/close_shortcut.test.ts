// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { closeForCmdW, cmdWTarget } from "./close_shortcut";
import { mountPageHeader } from "./page_header";
import { promptDialog } from "./dialog";

afterEach(() => {
  document.body.textContent = "";
});

/** A page with a back arrow and a text field, appended to `parent`. */
function page(parent: HTMLElement, onBack: () => void): { root: HTMLElement; field: HTMLInputElement } {
  const root = document.createElement("div");
  const header = document.createElement("div");
  root.appendChild(header);
  mountPageHeader(header, { title: "Page", backTo: "the workspace", onBack });
  const field = document.createElement("input");
  field.type = "text";
  root.appendChild(field);
  parent.appendChild(root);
  return { root, field };
}

describe("what Cmd+W closes", () => {
  it("a page around the focus, by its back arrow", () => {
    const onBack = vi.fn();
    const { field } = page(document.body, onBack);
    const closePane = vi.fn();
    expect(closeForCmdW(field, closePane).kind).toBe("page");
    expect(onBack).toHaveBeenCalledTimes(1);
    expect(closePane).not.toHaveBeenCalled();
  });

  it("the innermost page when one is drilled into from another", () => {
    const outerBack = vi.fn();
    const innerBack = vi.fn();
    const outer = page(document.body, outerBack);
    const inner = page(outer.root, innerBack);
    closeForCmdW(inner.field, () => {});
    expect(innerBack).toHaveBeenCalledTimes(1);
    expect(outerBack).not.toHaveBeenCalled();
    closeForCmdW(outer.field, () => {});
    expect(outerBack).toHaveBeenCalledTimes(1);
  });

  it("a dialog the focus is in, as Escape does, even over a page", async () => {
    const onBack = vi.fn();
    const { root } = page(document.body, onBack);
    const answer = promptDialog("Name?", { container: root });
    const input = document.querySelector<HTMLInputElement>(".chappa-dialog-input")!;
    input.focus();
    const closePane = vi.fn();
    expect(closeForCmdW(document.activeElement, closePane).kind).toBe("dialog");
    expect(await answer).toBeNull();
    expect(onBack).not.toHaveBeenCalled();
    expect(closePane).not.toHaveBeenCalled();
  });

  it("the active pane when no page is on screen; a hidden page does not count", () => {
    const onBack = vi.fn();
    const { root } = page(document.body, onBack);
    root.style.display = "none";
    const main = document.createElement("div");
    const field = document.createElement("input");
    main.appendChild(field);
    document.body.appendChild(main);
    const closePane = vi.fn();
    expect(closeForCmdW(field, closePane).kind).toBe("pane");
    expect(closePane).toHaveBeenCalledTimes(1);
    expect(onBack).not.toHaveBeenCalled();
    expect(cmdWTarget(null).kind).toBe("pane");
  });

  it("a page on screen even when the focus sits outside it", () => {
    const onBack = vi.fn();
    page(document.body, onBack);
    const stray = document.createElement("input");
    document.body.appendChild(stray);
    expect(cmdWTarget(stray).kind).toBe("page");
    expect(cmdWTarget(null).kind).toBe("page");
  });
});
