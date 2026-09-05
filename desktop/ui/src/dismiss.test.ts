// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { registerDismiss } from "./dismiss";

function press(el: Node): void {
  el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
}

function escape(): void {
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
}

function surface(): { menu: HTMLElement; anchor: HTMLElement; outside: HTMLElement } {
  const menu = document.createElement("div");
  const inner = document.createElement("button");
  menu.appendChild(inner);
  const anchor = document.createElement("button");
  const outside = document.createElement("div");
  document.body.append(menu, anchor, outside);
  return { menu, anchor, outside };
}

describe("registerDismiss", () => {
  it("dismisses on an OUTSIDE capture-phase press, only while open", () => {
    const { menu, outside } = surface();
    let open = true;
    const dismiss = vi.fn(() => {
      open = false;
    });
    const unbind = registerDismiss({ isOpen: () => open, dismiss, inside: [menu] });

    press(outside);
    expect(dismiss).toHaveBeenCalledTimes(1);
    // Closed now: further outside presses are no-ops (the copies all guarded
    // on `display !== "none"`).
    press(outside);
    expect(dismiss).toHaveBeenCalledTimes(1);
    unbind();
    menu.remove();
    outside.remove();
  });

  it("a press INSIDE the surface (descendants included) never dismisses", () => {
    // The rule: capture-phase fires BEFORE the item's click
    // can land — hiding the button mid-gesture swallows its click.
    const { menu, outside } = surface();
    const dismiss = vi.fn();
    const unbind = registerDismiss({ isOpen: () => true, dismiss, inside: [menu] });

    press(menu);
    press(menu.querySelector("button")!);
    expect(dismiss).not.toHaveBeenCalled();
    unbind();
    menu.remove();
    outside.remove();
  });

  it("an anchor node is excluded like the surface (the ▾-toggle case)", () => {
    const { menu, anchor, outside } = surface();
    const dismiss = vi.fn();
    const unbind = registerDismiss({
      isOpen: () => true,
      dismiss,
      inside: [menu, anchor],
    });

    press(anchor);
    expect(dismiss).not.toHaveBeenCalled();
    press(outside);
    expect(dismiss).toHaveBeenCalledTimes(1);
    unbind();
    menu.remove();
    anchor.remove();
    outside.remove();
  });

  it("Escape dismisses only when opted in (the preserved drift)", () => {
    const { menu } = surface();
    const noEscape = vi.fn();
    const withEscape = vi.fn();
    const unbindA = registerDismiss({ isOpen: () => true, dismiss: noEscape, inside: [menu] });
    const unbindB = registerDismiss({
      isOpen: () => true,
      dismiss: withEscape,
      inside: [menu],
      escape: true,
    });

    escape();
    expect(noEscape).not.toHaveBeenCalled();
    expect(withEscape).toHaveBeenCalledTimes(1);
    // Other keys never dismiss.
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(withEscape).toHaveBeenCalledTimes(1);
    unbindA();
    unbindB();
    menu.remove();
  });

  it("unbind removes both listeners", () => {
    const { menu, outside } = surface();
    const dismiss = vi.fn();
    const unbind = registerDismiss({
      isOpen: () => true,
      dismiss,
      inside: [menu],
      escape: true,
    });
    unbind();
    press(outside);
    escape();
    expect(dismiss).not.toHaveBeenCalled();
    menu.remove();
    outside.remove();
  });
});
