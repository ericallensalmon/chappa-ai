// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  MOD_ALT,
  MOD_CTRL,
  MOD_SHIFT,
  MOD_SUPER,
  type KeyEventDto,
  type MouseEventDto,
  type SelectionOpDto,
} from "../ipc";
import {
  InputController,
  DEFAULT_CLIPBOARD_SHORTCUTS,
  dtoFromKeyboardEvent,
  isAppShortcut,
  isCopyShortcut,
  isCtrlCCopyOnlyShortcut,
  isCtrlVPasteShortcut,
  isMarkNavShortcut,
  isPanelShortcut,
  isProjectShortcut,
  isSearchShortcut,
  modsFromEvent,
  quotePath,
  quotePaths,
  rawWheelLines,
  wheelQuanta,
  type InputControllerOptions,
  type WheelState,
} from "./input";

// --- pure mapping tables ---------------------------------------------------

describe("dtoFromKeyboardEvent", () => {
  const cases: Array<[Record<string, unknown>, KeyEventDto | null]> = [
    // Plain chars.
    [{ key: "a" }, { key: { kind: "char", ch: "a" }, mods: 0 }],
    [{ key: "A", shiftKey: true }, { key: { kind: "char", ch: "A" }, mods: MOD_SHIFT }],
    [{ key: " " }, { key: { kind: "char", ch: " " }, mods: 0 }],
    [{ key: "+" }, { key: { kind: "char", ch: "+" }, mods: 0 }],
    // Modifier combos.
    [{ key: "a", ctrlKey: true }, { key: { kind: "char", ch: "a" }, mods: MOD_CTRL }],
    [{ key: "a", altKey: true }, { key: { kind: "char", ch: "a" }, mods: MOD_ALT }],
    [{ key: "a", altKey: true, ctrlKey: true }, { key: { kind: "char", ch: "a" }, mods: MOD_ALT | MOD_CTRL }],
    [{ key: "a", metaKey: true }, { key: { kind: "char", ch: "a" }, mods: MOD_SUPER }],
    // Special keys.
    [{ key: "Enter" }, { key: { kind: "enter" }, mods: 0 }],
    [{ key: "Enter", altKey: true }, { key: { kind: "enter" }, mods: MOD_ALT }],
    [{ key: "Tab" }, { key: { kind: "tab" }, mods: 0 }],
    [{ key: "Backspace" }, { key: { kind: "backspace" }, mods: 0 }],
    [{ key: "Backspace", ctrlKey: true }, { key: { kind: "backspace" }, mods: MOD_CTRL }],
    [{ key: "Escape" }, { key: { kind: "escape" }, mods: 0 }],
    [{ key: "ArrowUp" }, { key: { kind: "up" }, mods: 0 }],
    [{ key: "ArrowDown" }, { key: { kind: "down" }, mods: 0 }],
    [{ key: "ArrowLeft" }, { key: { kind: "left" }, mods: 0 }],
    [{ key: "ArrowRight" }, { key: { kind: "right" }, mods: 0 }],
    // Ctrl+Shift+arrows reach the terminal (modifier form, not the allowlist).
    [
      { key: "ArrowLeft", ctrlKey: true, shiftKey: true },
      { key: { kind: "left" }, mods: MOD_CTRL | MOD_SHIFT },
    ],
    [
      { key: "ArrowRight", ctrlKey: true, shiftKey: true, altKey: true },
      { key: { kind: "right" }, mods: MOD_CTRL | MOD_SHIFT | MOD_ALT },
    ],
    [{ key: "Home" }, { key: { kind: "home" }, mods: 0 }],
    [{ key: "End" }, { key: { kind: "end" }, mods: 0 }],
    [{ key: "PageUp" }, { key: { kind: "page_up" }, mods: 0 }],
    [{ key: "PageDown" }, { key: { kind: "page_down" }, mods: 0 }],
    [{ key: "Insert" }, { key: { kind: "insert" }, mods: 0 }],
    [{ key: "Delete" }, { key: { kind: "delete" }, mods: 0 }],
    // Function keys.
    [{ key: "F1" }, { key: { kind: "f", n: 1 }, mods: 0 }],
    [{ key: "F5", shiftKey: true }, { key: { kind: "f", n: 5 }, mods: MOD_SHIFT }],
    [{ key: "F12" }, { key: { kind: "f", n: 12 }, mods: 0 }],
    // Unencodable / not terminal keys.
    [{ key: "F13" }, null],
    [{ key: "F24" }, null],
    [{ key: "Shift" }, null],
    [{ key: "Control" }, null],
    [{ key: "Alt" }, null],
    [{ key: "Meta" }, null],
    [{ key: "CapsLock" }, null],
    [{ key: "PrintScreen" }, null],
    [{ key: "Pause" }, null],
    [{ key: "Dead" }, null],
    [{ key: "Unidentified" }, null],
    [{ key: "ContextMenu" }, null],
    // IME in progress is left to composition.
    [{ key: "a", isComposing: true }, null],
  ];

  it.each(cases)("maps %o", (init, expected) => {
    const ev = new KeyboardEvent("keydown", init);
    expect(dtoFromKeyboardEvent(ev)).toEqual(expected);
  });
});

describe("modsFromEvent", () => {
  it("packs modifier bits in xterm order", () => {
    expect(
      modsFromEvent({ shiftKey: true, altKey: true, ctrlKey: true, metaKey: true }),
    ).toBe(MOD_SHIFT | MOD_ALT | MOD_CTRL | MOD_SUPER);
    expect(modsFromEvent({ shiftKey: false, altKey: false, ctrlKey: false, metaKey: false })).toBe(0);
  });
});

describe("app shortcut allowlist", () => {
  it("keeps F11/F12 for the browser", () => {
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "F11" }), false)).toBe(true);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "F12" }), true)).toBe(true);
  });

  it("reserves Ctrl+←/↑/↓ for focus nav", () => {
    for (const key of ["ArrowLeft", "ArrowUp", "ArrowDown"]) {
      expect(isAppShortcut(new KeyboardEvent("keydown", { key, ctrlKey: true }), false)).toBe(true);
    }
    // Not reserved: right arrow, and any shift/meta variants go to the terminal.
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "ArrowRight", ctrlKey: true }), false)).toBe(false);
    expect(
      isAppShortcut(new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, shiftKey: true }), false),
    ).toBe(false);
    expect(
      isAppShortcut(new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, metaKey: true }), false),
    ).toBe(false);
  });

  it("maps Ctrl/Cmd+C with a selection to copy (in both modes)", () => {
    expect(isCopyShortcut(new KeyboardEvent("keydown", { key: "c", ctrlKey: true }), true)).toBe(true);
    expect(isCopyShortcut(new KeyboardEvent("keydown", { key: "c", metaKey: true }), true)).toBe(true);
    expect(isCopyShortcut(new KeyboardEvent("keydown", { key: "c", ctrlKey: true }), false)).toBe(false);
    expect(isCopyShortcut(new KeyboardEvent("keydown", { key: "c" }), true)).toBe(false);
    // With a selection the copy shortcut wins with ctrlCCopyOnly on OR off —
    // the existing rule is unchanged in both modes.
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "c", ctrlKey: true }), true)).toBe(true);
    expect(
      isAppShortcut(new KeyboardEvent("keydown", { key: "c", ctrlKey: true }), true, {
        ctrlVPastes: false,
        ctrlCCopyOnly: false,
      }),
    ).toBe(true);
    // Default (setting ON, omitted flag): Ctrl+C WITHOUT a selection
    // is now an app-level no-op — the 0x03 passthrough is the OFF mode only.
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "c", ctrlKey: true }), false)).toBe(true);
  });

  it("lets ordinary keys through", () => {
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "a" }), false)).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "Enter" }), false)).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "Escape", altKey: true }), false)).toBe(false);
  });

  it("reserves Ctrl+Shift+D for the debug HUD (never reaches the terminal)", () => {
    const ev = new KeyboardEvent("keydown", { key: "D", ctrlKey: true, shiftKey: true, cancelable: true, bubbles: true });
    expect(isAppShortcut(ev, false)).toBe(true);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "d", ctrlKey: true, shiftKey: true }), true)).toBe(true);
    // Alt/meta variants and bare 'd' are terminal keys.
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "d", ctrlKey: true, shiftKey: true, altKey: true }), false)).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "d", ctrlKey: true }), false)).toBe(false);
  });

  it("reserves the panel chords (Ctrl+PgUp/PgDn, Ctrl+Shift+T, Ctrl+Shift+W)", () => {
    // Cycle / new shell / close panel never reach the terminal.
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "PageUp", ctrlKey: true }))).toBe(true);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "PageDown", ctrlKey: true }))).toBe(true);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "t", ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "w", ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "PageUp", ctrlKey: true }), false)).toBe(true);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "T", ctrlKey: true, shiftKey: true }), false)).toBe(true);
    // Everything else still goes to the terminal:
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "PageUp" }))).toBe(false);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "PageUp", ctrlKey: true, altKey: true }))).toBe(false);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "PageUp", ctrlKey: true, shiftKey: true }))).toBe(false);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "t", ctrlKey: true }))).toBe(false);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "t", shiftKey: true }))).toBe(false);
    expect(isPanelShortcut(new KeyboardEvent("keydown", { key: "d", ctrlKey: true, shiftKey: true }))).toBe(false);
  });

  it("never forwards Ctrl+Shift+D from the controller", () => {
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "D", ctrlKey: true, shiftKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("reserves the project chords (Ctrl+Shift+S/A/P) for the app", () => {
    // Project-header keys: S start auto-starting,
    // A start all, P stop all. The terminal never sees them.
    for (const key of ["s", "S", "a", "A", "p", "P"]) {
      expect(isProjectShortcut(new KeyboardEvent("keydown", { key, ctrlKey: true, shiftKey: true }))).toBe(true);
      expect(isAppShortcut(new KeyboardEvent("keydown", { key, ctrlKey: true, shiftKey: true }), false)).toBe(true);
    }
    // Alt/meta variants, bare letters, and missing shift/ctrl go to the terminal.
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "s", ctrlKey: true, shiftKey: true, altKey: true }))).toBe(false);
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "s", ctrlKey: true, shiftKey: true, metaKey: true }))).toBe(false);
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "s", ctrlKey: true }))).toBe(false);
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "s", shiftKey: true }))).toBe(false);
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "s" }))).toBe(false);
    expect(isProjectShortcut(new KeyboardEvent("keydown", { key: "d", ctrlKey: true, shiftKey: true }))).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "s", ctrlKey: true }), false)).toBe(false);
  });

  it("reserves Ctrl+F for the search overlay", () => {
    const ev = new KeyboardEvent("keydown", { key: "f", ctrlKey: true, cancelable: true, bubbles: true });
    expect(isSearchShortcut(ev)).toBe(true);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "F", ctrlKey: true }), false)).toBe(true);
    // Alt/shift/meta variants and bare 'f' are terminal keys.
    expect(isSearchShortcut(new KeyboardEvent("keydown", { key: "f", ctrlKey: true, altKey: true }))).toBe(false);
    expect(isSearchShortcut(new KeyboardEvent("keydown", { key: "f", ctrlKey: true, shiftKey: true }))).toBe(false);
    expect(isSearchShortcut(new KeyboardEvent("keydown", { key: "f", ctrlKey: true, metaKey: true }))).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "f" }), false)).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "f", ctrlKey: true, altKey: true }), false)).toBe(false);
  });

  it("reserves Ctrl+Alt+↑/↓ for prompt-mark nav", () => {
    for (const key of ["ArrowUp", "ArrowDown"]) {
      expect(isMarkNavShortcut(new KeyboardEvent("keydown", { key, ctrlKey: true, altKey: true }))).toBe(true);
      expect(isAppShortcut(new KeyboardEvent("keydown", { key, ctrlKey: true, altKey: true }), false)).toBe(true);
    }
    // Ctrl+↑ alone stays the focus-nav ring, not mark nav.
    expect(isMarkNavShortcut(new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true }))).toBe(false);
    expect(isMarkNavShortcut(new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, altKey: true }))).toBe(false);
    expect(isMarkNavShortcut(new KeyboardEvent("keydown", { key: "ArrowUp", ctrlKey: true, altKey: true, shiftKey: true }))).toBe(false);
    expect(isAppShortcut(new KeyboardEvent("keydown", { key: "ArrowUp" }), false)).toBe(false);
  });

  it("never forwards Ctrl+F from the controller", () => {
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
  });
});

describe("the clipboard-key settings (predicate level)", () => {
  const OFF = { ctrlVPastes: false, ctrlCCopyOnly: false };
  const ON = { ctrlVPastes: true, ctrlCCopyOnly: true };

  // All four setting combinations at the boundaries: the (setting ON/OFF) ×
  // (selection yes/no) matrix for Ctrl+C, both modes for Ctrl+V, and the
  // modifier-variant discipline (Ctrl+Shift+V and Alt combos unchanged).

  it("defaults are both ON and match the Rust serde defaults", () => {
    expect(DEFAULT_CLIPBOARD_SHORTCUTS).toEqual({ ctrlVPastes: true, ctrlCCopyOnly: true });
  });

  it("Ctrl+C WITHOUT a selection: no-op while ON, 0x03 passthrough while OFF", () => {
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true });
    expect(isCtrlCCopyOnlyShortcut(ev)).toBe(true);
    expect(isAppShortcut(ev, false, ON)).toBe(true); // app shortcut → no-op
    expect(isAppShortcut(ev, false, OFF)).toBe(false); // terminal → 0x03
    // A 2-arg call stands for the defaults (ON) — the upgrade path.
    expect(isAppShortcut(ev, false)).toBe(true);
  });

  it("Ctrl+C WITH a selection copies in both modes (existing rule)", () => {
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true });
    expect(isAppShortcut(ev, true, ON)).toBe(true);
    expect(isAppShortcut(ev, true, OFF)).toBe(true);
    expect(isCopyShortcut(ev, true)).toBe(true);
  });

  it("the copy-only guard keeps Alt combos unchanged (ESC-prefix gestures)", () => {
    // Alt+Ctrl+C carries an ESC prefix — a different gesture, terminal key
    // in BOTH modes (Alt combos are unchanged).
    const alt = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, altKey: true });
    expect(isCtrlCCopyOnlyShortcut(alt)).toBe(false);
    expect(isAppShortcut(alt, false, ON)).toBe(false);
    expect(isAppShortcut(alt, false, OFF)).toBe(false);
    // Cmd-without-Ctrl (mac Cmd+C) never sets the ctrl bit: untouched in
    // both modes, with or without a selection (no selection → terminal key).
    const cmd = new KeyboardEvent("keydown", { key: "c", metaKey: true });
    expect(isCtrlCCopyOnlyShortcut(cmd)).toBe(false);
    expect(isAppShortcut(cmd, false, ON)).toBe(false);
    expect(isAppShortcut(cmd, false, OFF)).toBe(false);
  });

  it("Ctrl+Shift+C / Ctrl+Meta+C share the no-op while ON (same 0x03 byte)", () => {
    // With kitty disambiguation off (a plain shell negotiates none), these
    // encode the SAME raw 0x03 as Ctrl+C — the "0x03 never reaches the pty
    // while ON" contract covers them too.
    const shift = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, shiftKey: true });
    expect(isCtrlCCopyOnlyShortcut(shift)).toBe(true);
    expect(isAppShortcut(shift, false, ON)).toBe(true);
    expect(isAppShortcut(shift, false, OFF)).toBe(false); // OFF = today's rule
    const meta = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, metaKey: true });
    expect(isCtrlCCopyOnlyShortcut(meta)).toBe(true);
    expect(isAppShortcut(meta, false, ON)).toBe(true);
    expect(isAppShortcut(meta, false, OFF)).toBe(false);
    // …but with a selection they still copy (the existing copy rule has no
    // shift exclusion — unchanged in both modes).
    expect(isAppShortcut(shift, true, ON)).toBe(true);
    expect(isCopyShortcut(shift, true)).toBe(true);
  });

  it("Ctrl+V: paste shortcut while ON, 0x16 passthrough while OFF", () => {
    const ev = new KeyboardEvent("keydown", { key: "v", ctrlKey: true });
    expect(isCtrlVPasteShortcut(ev)).toBe(true);
    expect(isAppShortcut(ev, false, ON)).toBe(true); // app shortcut → paste
    expect(isAppShortcut(ev, false, OFF)).toBe(false); // terminal → 0x16
    // Ctrl+V is independent of any selection.
    expect(isAppShortcut(ev, true, ON)).toBe(true);
    expect(isAppShortcut(ev, true, OFF)).toBe(false);
  });

  it("modifier variants of Ctrl+V are unchanged (Ctrl+Shift+V, Alt, Cmd)", () => {
    // The setting is about the bare Ctrl+V reflex: every other variant keeps
    // reaching the terminal in both modes.
    for (const init of [
      { key: "v", ctrlKey: true, shiftKey: true },
      { key: "v", ctrlKey: true, altKey: true },
      { key: "v", ctrlKey: true, metaKey: true },
      { key: "v", ctrlKey: true, shiftKey: true, altKey: true },
      { key: "v", metaKey: true },
      { key: "v" },
    ]) {
      const ev = new KeyboardEvent("keydown", init);
      expect(isCtrlVPasteShortcut(ev), JSON.stringify(init)).toBe(false);
      expect(isAppShortcut(ev, false, ON), JSON.stringify(init)).toBe(false);
      expect(isAppShortcut(ev, false, OFF), JSON.stringify(init)).toBe(false);
    }
    // Uppercase "V" (as some layouts/engines report it) is the same key.
    const upper = new KeyboardEvent("keydown", { key: "V", ctrlKey: true, shiftKey: true });
    expect(isCtrlVPasteShortcut(upper)).toBe(false); // shift held → not the reflex
    const upperBare = new KeyboardEvent("keydown", { key: "V", ctrlKey: true });
    expect(isCtrlVPasteShortcut(upperBare)).toBe(true); // toLowerCase() catches it
  });
});

describe("wheel quantization", () => {
  it("converts pixel deltas to fractional lines (deltaMode-aware, unrounded)", () => {
    expect(rawWheelLines(new WheelEvent("wheel", { deltaY: 100, deltaMode: 0 }), 20, 24)).toBe(5);
    expect(rawWheelLines(new WheelEvent("wheel", { deltaY: -50, deltaMode: 0 }), 20, 24)).toBe(-2.5);
    const lines = new WheelEvent("wheel", { deltaY: 3, deltaMode: 1 });
    expect(rawWheelLines(lines, 20, 24)).toBe(3);
    const pages = new WheelEvent("wheel", { deltaY: 0.5, deltaMode: 2 });
    expect(rawWheelLines(pages, 20, 24)).toBe(12);
  });

  it("accumulates fractional deltas and carries the remainder", () => {
    const state: WheelState = { remainder: 0 };
    // Precision trackpad: 0.3-cell ticks. First three emit nothing; the
    // fourth crosses 1.0 and emits a single whole cell.
    expect(wheelQuanta(state, 0.3)).toBe(0);
    expect(wheelQuanta(state, 0.3)).toBe(0);
    expect(wheelQuanta(state, 0.3)).toBe(0);
    expect(wheelQuanta(state, 0.3)).toBe(1);
    expect(state.remainder).toBeCloseTo(0.2);
  });

  it("truncates toward zero and keeps negative remainders", () => {
    const state: WheelState = { remainder: 0 };
    expect(wheelQuanta(state, -0.5)).toBe(0);
    expect(state.remainder).toBeCloseTo(-0.5);
    expect(wheelQuanta(state, -0.7)).toBe(-1);
    expect(state.remainder).toBeCloseTo(-0.2);
  });
});

describe("drop → paste quoting", () => {
  it("quotes a path with spaces and escapes inner quotes/backslashes", () => {
    // Each `\` in the path is escaped to `\\`, inner `"` to `\"`.
    expect(quotePath("C:\\Users\\a b.txt")).toBe('"C:\\\\Users\\\\a b.txt"');
    expect(quotePath("/tmp/x\"y")).toBe('"/tmp/x\\"y"');
    // A backslash immediately before a quote: both get escaped.
    expect(quotePath('C:\\"q\\"')).toBe('"C:\\\\\\"q\\\\\\""');
  });

  it("joins multiple paths space-separated", () => {
    expect(quotePaths(["C:\\a.txt", "/tmp/b c.txt"])).toBe('"C:\\\\a.txt" "/tmp/b c.txt"');
  });
});

// --- controller behaviour --------------------------------------------------

function mockRect(el: Element, rect: { left: number; top: number; width: number; height: number }) {
  el.getBoundingClientRect = () =>
    ({
      left: rect.left,
      top: rect.top,
      right: rect.left + rect.width,
      bottom: rect.top + rect.height,
      width: rect.width,
      height: rect.height,
      x: rect.left,
      y: rect.top,
      toJSON: () => ({}),
    }) as DOMRect;
}

interface Harness {
  element: HTMLElement;
  textarea: HTMLTextAreaElement;
  compose: HTMLElement;
  onKey: KeyEventDto[];
  onPaste: string[];
  onMouse: MouseEventDto[];
  onScroll: number[];
  onSelection: SelectionOpDto[];
  onCopy: number;
  /** `ctrl_v_pastes`: how many times the clipboard-paste action fired. */
  onClipboardPaste: number;
  controller: InputController;
}

function makeHarness(opts: Partial<InputControllerOptions> = {}): Harness {
  const element = document.createElement("div");
  const textarea = document.createElement("textarea");
  const compose = document.createElement("div");
  document.body.append(element, textarea, compose);
  mockRect(element, { left: 0, top: 0, width: 400, height: 300 });

  const harness: Harness = {
    element,
    textarea,
    compose,
    onKey: [],
    onPaste: [],
    onMouse: [],
    onScroll: [],
    onSelection: [],
    onCopy: 0,
    onClipboardPaste: 0,
    controller: null as unknown as InputController,
  };

  harness.controller = new InputController({
    element,
    textarea,
    compose,
    cellW: () => 10,
    cellH: () => 20,
    cols: () => 40,
    rows: () => 15,
    cursorCell: () => ({ row: 1, col: 2 }),
    onKey: (ev) => harness.onKey.push(ev),
    onPaste: (t) => harness.onPaste.push(t),
    onMouse: (ev) => harness.onMouse.push(ev),
    onScroll: (d) => harness.onScroll.push(d),
    onSelection: (op) => harness.onSelection.push(op),
    onCopy: () => harness.onCopy++,
    onClipboardPaste: () => harness.onClipboardPaste++,
    hasSelection: () => false,
    hasMouseCapture: () => false,
    ...opts,
  });
  return harness;
}

describe("InputController focus reclaim", () => {
  it("a click on the terminal refocuses the hidden textarea", () => {
    // Regression: onMouseDown preventDefault suppresses native
    // focus-on-click, so a terminal whose textarea lost focus (devtools,
    // another widget) went permanently deaf without this explicit focus().
    const h = makeHarness();
    h.textarea.blur();
    expect(document.activeElement).not.toBe(h.textarea);
    h.element.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 }));
    expect(document.activeElement).toBe(h.textarea);
    h.controller.destroy();
  });
});

describe("InputController key handling", () => {
  it("forwards a plain key to the terminal and prevents the browser default", () => {
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "a", bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onKey).toEqual([{ key: { kind: "char", ch: "a" }, mods: 0 }]);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("sends Alt+key and Ctrl+Shift+arrow through", () => {
    const h = makeHarness();
    h.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "a", altKey: true, bubbles: true }));
    h.textarea.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, shiftKey: true, bubbles: true }),
    );
    expect(h.onKey[0]).toEqual({ key: { kind: "char", ch: "a" }, mods: MOD_ALT });
    expect(h.onKey[1]).toEqual({ key: { kind: "left" }, mods: MOD_CTRL | MOD_SHIFT });
  });

  it("keeps F11/F12 local without preventing the default", () => {
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "F12", bubbles: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(false);
  });

  it("keeps Ctrl+← local (app focus nav), nothing to the terminal", () => {
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("copies when Ctrl+C is pressed over a selection (both modes)", () => {
    const h = makeHarness({ hasSelection: () => true });
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onCopy).toBe(1);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
    h.controller.destroy();

    // Explicit OFF mode: the existing copy rule still wins.
    const hOff = makeHarness({
      hasSelection: () => true,
      clipboardSettings: () => ({ ctrlVPastes: false, ctrlCCopyOnly: false }),
    });
    hOff.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true, cancelable: true }));
    expect(hOff.onCopy).toBe(1);
    expect(hOff.onKey.length).toBe(0);
    hOff.controller.destroy();
  });

  it("Ctrl+C with nothing selected: no-op while ON, 0x03 to the terminal while OFF", () => {
    // Default options = both settings ON: the reflex ^C is a deliberate
    // no-op — preventDefault (the browser never tries its own thing) and
    // NOTHING reaches the terminal.
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onCopy).toBe(0);
    expect(h.onKey.length).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
    h.controller.destroy();

    // Explicit OFF mode = today's rule: 0x03 passthrough.
    const hOff = makeHarness({
      clipboardSettings: () => ({ ctrlVPastes: false, ctrlCCopyOnly: false }),
    });
    hOff.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true }));
    expect(hOff.onCopy).toBe(0);
    expect(hOff.onKey).toEqual([{ key: { kind: "char", ch: "c" }, mods: MOD_CTRL }]);
    hOff.controller.destroy();

    // Alt combos stay terminal keys even while ON (the ESC-prefix gesture).
    const hAlt = makeHarness();
    hAlt.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "c", ctrlKey: true, altKey: true, bubbles: true }));
    expect(hAlt.onKey).toEqual([{ key: { kind: "char", ch: "c" }, mods: MOD_CTRL | MOD_ALT }]);
    hAlt.controller.destroy();
  });

  it("Ctrl+V: fires the clipboard-paste action while ON, sends 0x16 while OFF", () => {
    // Default options = ON: the action fires and the terminal sees nothing.
    const h = makeHarness();
    const ev = new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true, cancelable: true });
    h.textarea.dispatchEvent(ev);
    expect(h.onClipboardPaste).toBe(1);
    expect(h.onKey.length).toBe(0);
    expect(h.onCopy).toBe(0);
    expect(ev.defaultPrevented).toBe(true);
    h.controller.destroy();

    // Explicit OFF = today's passthrough: raw 0x16 (ctrl char "v").
    const hOff = makeHarness({
      clipboardSettings: () => ({ ctrlVPastes: false, ctrlCCopyOnly: false }),
    });
    hOff.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "v", ctrlKey: true, bubbles: true }));
    expect(hOff.onClipboardPaste).toBe(0);
    expect(hOff.onKey).toEqual([{ key: { kind: "char", ch: "v" }, mods: MOD_CTRL }]);
    hOff.controller.destroy();

    // Ctrl+Shift+V is NOT the reflex: passthrough in both modes.
    const hShift = makeHarness();
    hShift.textarea.dispatchEvent(
      new KeyboardEvent("keydown", { key: "V", ctrlKey: true, shiftKey: true, bubbles: true }),
    );
    expect(hShift.onClipboardPaste).toBe(0);
    expect(hShift.onKey).toEqual([{ key: { kind: "char", ch: "V" }, mods: MOD_CTRL | MOD_SHIFT }]);
    hShift.controller.destroy();
  });

  it("ignores modifier-only and dead keys", () => {
    const h = makeHarness();
    h.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "Shift", bubbles: true }));
    h.textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "Dead", bubbles: true }));
    expect(h.onKey.length).toBe(0);
  });
});

describe("InputController paste + composition", () => {
  it("routes clipboard text to paste and prevents the default", () => {
    const h = makeHarness();
    // jsdom has no ClipboardEvent/DataTransfer; a bare Event with a minimal
    // clipboardData stand-in is all the controller reads.
    const ev = new Event("paste", { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(ev, "clipboardData", {
      value: { getData: (type: string) => (type === "text" ? "hello\nworld" : "") },
    });
    h.textarea.dispatchEvent(ev);
    expect(h.onPaste).toEqual(["hello\nworld"]);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("shows the IME preview and commits the composed string as paste", () => {
    const h = makeHarness();
    expect(h.compose.style.visibility).toBe("hidden");
    h.textarea.dispatchEvent(new CompositionEvent("compositionstart"));
    expect(h.compose.style.visibility).toBe("visible");
    expect(h.compose.style.left).toBe("20px");
    expect(h.compose.style.top).toBe("20px");
    h.textarea.dispatchEvent(new CompositionEvent("compositionupdate", { data: "héllo" }));
    expect(h.compose.textContent).toBe("héllo");
    h.textarea.dispatchEvent(new CompositionEvent("compositionend", { data: "héllo" }));
    expect(h.compose.style.visibility).toBe("hidden");
    expect(h.onPaste).toEqual(["héllo"]);
  });
});

describe("InputController mouse", () => {
  it("computes cell coords from the element rect at integer metrics", () => {
    const h = makeHarness();
    const ev = new MouseEvent("mousedown", { clientX: 25, clientY: 45, button: 0, bubbles: true });
    h.element.dispatchEvent(ev);
    expect(h.onMouse).toEqual([{ kind: "press", button: 0, col: 2, row: 2, mods: 0 }]);
    expect(h.onSelection[0]).toEqual({ op: "start", point: { row: 2, col: 2 }, kind: "simple" });
  });

  it("suppresses local drag-selection while the TUI captures the mouse", () => {
    const h = makeHarness({ hasMouseCapture: () => true });
    h.element.dispatchEvent(new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, bubbles: true }));
    expect(h.onMouse[0]).toEqual({ kind: "press", button: 0, col: 0, row: 0, mods: 0 });
    expect(h.onSelection.length).toBe(0);
    // Motion with a button held forwards a drag DTO but never a selection op.
    h.element.dispatchEvent(new MouseEvent("mousemove", { clientX: 55, clientY: 45, buttons: 1, button: 0, bubbles: true }));
    expect(h.onMouse[1]).toEqual({ kind: "drag", button: 0, col: 5, row: 2, mods: 0 });
    expect(h.onSelection.length).toBe(0);
  });

  it("Shift restores local selection over a mouse-capturing TUI", () => {
    const h = makeHarness({ hasMouseCapture: () => true });
    const ev = new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, shiftKey: true, bubbles: true });
    h.element.dispatchEvent(ev);
    expect(h.onSelection[0]).toEqual({ op: "start", point: { row: 0, col: 0 }, kind: "simple" });
  });

  it("computes cell coords at fractional DPR metrics and rect offsets", () => {
    const h = makeHarness({
      cellW: () => 8.4,
      cellH: () => 18.2,
    });
    mockRect(h.element, { left: 2, top: 3, width: 400, height: 300 });
    const ev = new MouseEvent("mousedown", { clientX: 12.8, clientY: 45.4, button: 0, bubbles: true });
    h.element.dispatchEvent(ev);
    // (12.8-2)/8.4 = 1.29 → 1 ; (45.4-3)/18.2 = 2.33 → 2
    expect(h.onMouse[0]).toEqual({ kind: "press", button: 0, col: 1, row: 2, mods: 0 });
  });

  it("clamps coordinates to the grid", () => {
    const h = makeHarness();
    const ev = new MouseEvent("mousedown", { clientX: 9999, clientY: 9999, button: 0, bubbles: true });
    h.element.dispatchEvent(ev);
    expect(h.onMouse[0].col).toBe(39);
    expect(h.onMouse[0].row).toBe(14);
  });

  it("starts word selection on double-click and line selection on triple-click", () => {
    const h = makeHarness();
    h.element.dispatchEvent(new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, detail: 2, bubbles: true }));
    expect(h.onSelection[0]).toEqual({ op: "start", point: { row: 0, col: 0 }, kind: "semantic" });
    h.element.dispatchEvent(new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, detail: 3, bubbles: true }));
    expect(h.onSelection[1]).toEqual({ op: "start", point: { row: 0, col: 0 }, kind: "lines" });
  });

  it("a ctrl/cmd+click is the link opener — never starts a selection", () => {
    // Ctrl+click (and cmd+click on mac) on a link cell opens it; the
    // press still reaches the terminal, but no selection may start.
    const h = makeHarness();
    h.element.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 25, clientY: 25, button: 0, ctrlKey: true, bubbles: true }),
    );
    expect(h.onSelection.length).toBe(0);
    expect(h.onMouse[0]).toEqual({ kind: "press", button: 0, col: 2, row: 1, mods: MOD_CTRL });

    h.element.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, metaKey: true, bubbles: true }),
    );
    expect(h.onSelection.length).toBe(0);

    // A plain left-click still selects.
    h.element.dispatchEvent(new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, bubbles: true }));
    expect(h.onSelection.length).toBe(1);
  });

  it("drives selection drag/update and stops on mouseup", () => {
    const h = makeHarness();
    h.element.dispatchEvent(new MouseEvent("mousedown", { clientX: 5, clientY: 5, button: 0, bubbles: true }));
    h.element.dispatchEvent(new MouseEvent("mousemove", { clientX: 55, clientY: 45, buttons: 1, button: 0, bubbles: true }));
    expect(h.onMouse[1]).toEqual({ kind: "drag", button: 0, col: 5, row: 2, mods: 0 });
    expect(h.onSelection[1]).toEqual({ op: "update", point: { row: 2, col: 5 }, kind: "simple" });
    h.element.dispatchEvent(new MouseEvent("mouseup", { clientX: 55, clientY: 45, button: 0, bubbles: true }));
    expect(h.onMouse[2].kind).toBe("release");
    // After mouseup, moves are plain motion again, no selection updates.
    h.element.dispatchEvent(new MouseEvent("mousemove", { clientX: 65, clientY: 45, buttons: 0, bubbles: true }));
    expect(h.onMouse[3]).toEqual({ kind: "move", button: 0, col: 6, row: 2, mods: 0 });
    expect(h.onSelection.length).toBe(2);
  });
});

describe("InputController wheel", () => {
  // (The `WHEEL_TUI_MULTIPLIER` lockstep assertion lived here until the multiplier moved Rust-side.
  // The constant was declaration-only — the ×3 is applied in term-core's
  // keys.rs and is now the `scrollWheelSpeed` setting — so the constant and
  // its test went away together.)

  it("scrolls scrollback and forwards a wheel mouse event", () => {
    const h = makeHarness();
    const ev = new WheelEvent("wheel", { deltaY: 100, deltaMode: 0, bubbles: true, cancelable: true });
    h.element.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(h.onScroll).toEqual([-5]); // wheel down → scroll down the page
    expect(h.onMouse[0]).toEqual({ kind: "wheel_down", button: 0, col: 0, row: 0, mods: 0 });
  });

  it("scrolls up into history on wheel-up", () => {
    const h = makeHarness();
    h.element.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, deltaMode: 0, bubbles: true }));
    expect(h.onScroll).toEqual([5]);
    expect(h.onMouse[0].kind).toBe("wheel_up");
  });

  it("does nothing for a zero delta", () => {
    const h = makeHarness();
    h.element.dispatchEvent(new WheelEvent("wheel", { deltaY: 0, deltaMode: 0, bubbles: true }));
    expect(h.onScroll.length).toBe(0);
    expect(h.onMouse.length).toBe(0);
  });

  it("accumulates sub-cell trackpad ticks before emitting", () => {
    const h = makeHarness();
    // deltaY 5 / cellH 20 = exactly 0.25 cells/tick (binary-exact).
    const tick = (): void => {
      h.element.dispatchEvent(new WheelEvent("wheel", { deltaY: 5, deltaMode: 0, bubbles: true }));
    };
    for (let i = 0; i < 9; i++) tick();
    // Nine sub-cell ticks cross 1.0 twice → exactly two whole-cell emissions,
    // never nine per-notch events.
    expect(h.onScroll).toEqual([-1, -1]);
    expect(h.onMouse.length).toBe(2);
    // The remainder (0.25) carries: one more tick is still sub-cell…
    tick();
    expect(h.onScroll).toEqual([-1, -1]);
    // …and three more cross 1.0 again.
    tick();
    tick();
    tick();
    expect(h.onScroll).toEqual([-1, -1, -1]);
    expect(h.onMouse.length).toBe(3);
  });

  it("sends only the mouse DTO (no local scroll) when a TUI captures the wheel", () => {
    const h = makeHarness({ hasMouseCapture: () => true });
    h.element.dispatchEvent(new WheelEvent("wheel", { deltaY: 100, deltaMode: 0, bubbles: true }));
    expect(h.onMouse).toEqual([{ kind: "wheel_down", button: 0, col: 0, row: 0, mods: 0 }]);
    expect(h.onScroll.length).toBe(0);
  });

  it("Shift restores local scroll over a mouse-capturing TUI", () => {
    const h = makeHarness({ hasMouseCapture: () => true });
    h.element.dispatchEvent(
      new WheelEvent("wheel", { deltaY: 100, deltaMode: 0, shiftKey: true, bubbles: true }),
    );
    expect(h.onMouse.length).toBe(1);
    expect(h.onScroll).toEqual([-5]);
  });
});

describe("InputController drag-drop", () => {
  it("suppresses the webview's default dragover/drop navigation", () => {
    const h = makeHarness();
    const over = new Event("dragover", { bubbles: true, cancelable: true }) as DragEvent;
    h.element.dispatchEvent(over);
    expect(over.defaultPrevented).toBe(true);
    const drop = new Event("drop", { bubbles: true, cancelable: true }) as DragEvent;
    Object.defineProperty(drop, "dataTransfer", { value: { files: [] } });
    h.element.dispatchEvent(drop);
    expect(drop.defaultPrevented).toBe(true);
  });

  it("pastes quoted dropped file paths through the normal paste route", () => {
    const h = makeHarness();
    const drop = new Event("drop", { bubbles: true, cancelable: true }) as DragEvent;
    Object.defineProperty(drop, "dataTransfer", {
      value: { files: [{ path: "C:\\Users\\a b.txt" }, { path: "/tmp/x\"y" }] },
    });
    h.element.dispatchEvent(drop);
    expect(h.onPaste).toEqual([`"C:\\\\Users\\\\a b.txt" "/tmp/x\\"y"`]);
  });

  it("ignores a drop with no file paths", () => {
    const h = makeHarness();
    const drop = new Event("drop", { bubbles: true, cancelable: true }) as DragEvent;
    Object.defineProperty(drop, "dataTransfer", { value: { files: [{ path: "" }] } });
    h.element.dispatchEvent(drop);
    expect(h.onPaste.length).toBe(0);
  });
});
