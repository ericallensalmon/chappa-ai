// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { SEGMENTED_ACCENT, SEGMENTED_MUTED, Segmented } from "./segmented";

function build(value = 3, onChange: (n: number) => void = () => {}): Segmented<number> {
  return new Segmented<number>({
    label: "Scroll wheel speed",
    options: [1, 2, 3, 4, 5, 6].map((n) => ({ value: n, label: `${n}x` })),
    value,
    onChange,
  });
}

function options(seg: Segmented<number>): HTMLButtonElement[] {
  return [...seg.element.querySelectorAll<HTMLButtonElement>("button")];
}

/** jsdom serializes inline colours as `rgb(r, g, b)`; the component writes
 *  hex. Compare in jsdom's form. */
function rgb(hex: string): string {
  const n = Number.parseInt(hex.slice(1), 16);
  return `rgb(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255})`;
}

const ACCENT = rgb(SEGMENTED_ACCENT);
const MUTED = rgb(SEGMENTED_MUTED);

describe("Segmented", () => {
  it("renders one text option per value in order", () => {
    const seg = build();
    expect(options(seg).map((b) => b.textContent)).toEqual(["1x", "2x", "3x", "4x", "5x", "6x"]);
    expect(seg.element.getAttribute("role")).toBe("radiogroup");
    expect(seg.element.getAttribute("aria-label")).toBe("Scroll wheel speed");
  });

  it("underlines exactly the active option in the accent colour", () => {
    // The active option is underlined in the accent colour.
    const seg = build(3);
    const btns = options(seg);
    expect(btns[2].style.borderBottomColor).toBe(ACCENT);
    expect(btns[2].style.color).toBe(ACCENT);
    expect(btns[2].getAttribute("aria-checked")).toBe("true");
    for (const inactive of [btns[0], btns[1], btns[3], btns[4], btns[5]]) {
      expect(inactive.style.borderBottomColor).toBe("transparent");
      expect(inactive.style.color).toBe(MUTED);
      expect(inactive.getAttribute("aria-checked")).toBe("false");
    }
  });

  it("clicking an option fires onChange once and moves the underline", () => {
    const onChange = vi.fn();
    const seg = build(3, onChange);
    options(seg)[5].click();
    expect(onChange).toHaveBeenCalledExactlyOnceWith(6);
    expect(seg.getValue()).toBe(6);
    expect(options(seg)[5].style.borderBottomColor).toBe(ACCENT);
    expect(options(seg)[2].style.borderBottomColor).toBe("transparent");
  });

  it("re-clicking the ACTIVE option writes nothing", () => {
    // Every change is a set_settings → a config write plus a broadcast to
    // every open actor; a redundant one is not free.
    const onChange = vi.fn();
    const seg = build(3, onChange);
    options(seg)[2].click();
    expect(onChange).not.toHaveBeenCalled();
  });

  it("options are keyboard-focusable buttons and ←/→ move the selection", () => {
    const onChange = vi.fn();
    const seg = build(3, onChange);
    document.body.appendChild(seg.element);
    const btns = options(seg);
    for (const b of btns) {
      expect(b.tagName).toBe("BUTTON");
      expect(b.type).toBe("button"); // never a form submit
      expect(b.tabIndex).toBe(0);
    }
    btns[2].focus();
    expect(document.activeElement).toBe(btns[2]);
    btns[2].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(document.activeElement).toBe(btns[3]);
    expect(onChange).toHaveBeenCalledWith(4);
    seg.dispose();
  });

  it("setValue adopts a canonical value WITHOUT firing onChange", () => {
    const onChange = vi.fn();
    const seg = build(3, onChange);
    seg.setValue(5);
    expect(seg.getValue()).toBe(5);
    expect(options(seg)[4].style.borderBottomColor).toBe(ACCENT);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("setValue ignores a value the row does not offer", () => {
    const seg = build(3);
    seg.setValue(99);
    expect(seg.getValue()).toBe(3);
  });

  it("dispose detaches the row from the document", () => {
    const seg = build();
    document.body.appendChild(seg.element);
    seg.dispose();
    expect(document.body.contains(seg.element)).toBe(false);
  });
});
