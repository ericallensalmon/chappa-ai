// KeyboardEvent -> semantic KeyEventDto capture.
//
// A hidden focused `<textarea>` overlay owns focus so the browser hands us
// keydown/paste/composition events; every keydown becomes a semantic
// `KeyEventDto` (the terminal-mode-aware encoding happens Rust-side in
// term-core keys.rs), pastes and IME commits arrive as `paste(text)`, and
// mouse/wheel events on the terminal element become `MouseEventDto` +
// scrollback `scroll(delta)` calls.
//
// Shortcut policy: a small allowlist is kept local — F11/F12 stay with the
// browser, the focus nav (Ctrl+←/↑/↓) and Ctrl/Cmd+C-with-a-
// selection are app-level — everything else goes to the terminal. There
// are two SETTINGS-GATED app shortcuts (both default ON, read LIVE from the
// settings store by the controller — the pane's toggle changes an open
// panel's behavior without a rebuild): Ctrl+V pastes the clipboard (OFF =
// today's 0x16 passthrough) and Ctrl+C becomes copy-only — a no-op without a
// selection, so 0x03 never reaches the pty (OFF = today's rule). Both are
// deliberate departures from terminal convention; the predicates below
// document the consequences.
//
// Mouse protocol: the frame header now carries the terminal's
// mouse/alt-screen modes (flags bits 1/2), so a mouse-capturing TUI (vim,
// htop) suppresses the local drag-selection and the wheel→scroll duplicate —
// only the mouse DTO goes to the app. Shift is the standard override that
// restores local selection/scroll.

import {
  MOD_ALT,
  MOD_CTRL,
  MOD_SHIFT,
  MOD_SUPER,
  type KeyDto,
  type KeyEventDto,
  type MouseEventDto,
  type SelectionKindDto,
  type SelectionOpDto,
} from "../ipc";

// Scroll wheel speed (1x…6x, default 3x) lives ENTIRELY outside
// this file: the multiplier is applied Rust-side in
// `term-core/src/keys.rs::alt_screen_wheel` (the encode paths), fed by
// `ActorConfig.wheel_speed` at spawn plus a live control on every open actor,
// and the user-facing knob is `Settings.scrollWheelSpeed` (src/settings.ts →
// `set_settings`). There is deliberately no TS constant to keep in lockstep —
// the declaration-only `WHEEL_TUI_MULTIPLIER` was removed, since no code
// path ever read it.

export interface InputControllerOptions {
  /** The terminal element mouse/wheel events are read from. */
  element: HTMLElement;
  /** The hidden textarea holding keyboard/paste/composition focus. */
  textarea: HTMLTextAreaElement;
  /** IME preview div; shown during composition and cleared on commit. */
  compose: HTMLElement;
  /** CSS px cell width/height. */
  cellW: () => number;
  cellH: () => number;
  /** Current grid size (for clamping mouse coordinates). */
  cols: () => number;
  rows: () => number;
  /** Cursor cell in viewport coords, for anchoring the IME preview. */
  cursorCell: () => { row: number; col: number } | null;
  onKey: (ev: KeyEventDto) => void;
  onPaste: (text: string) => void;
  onMouse: (ev: MouseEventDto) => void;
  /** Positive scrolls up into history (TermHandle::scroll semantics). */
  onScroll: (deltaLines: number) => void;
  onSelection: (op: SelectionOpDto) => void;
  onCopy: () => void;
  /** A left-button drag-selection just ended (mouseup). The panel's
   *  copy-on-select hook — it fires for a bare click too, since a
   *  click also starts and ends a selection; the copy path's
   *  skip-whitespace-only flag is what makes that harmless. */
  onSelectionEnd?: () => void;
  /** Whether a terminal selection is currently active (gates Ctrl/Cmd+C). */
  hasSelection: () => boolean;
  /** Whether the frame header says the TUI has captured the mouse (wire
   *  flags bit 1). Suppresses local drag-selection and the wheel→scroll
   *  duplicate unless Shift is held. */
  hasMouseCapture: () => boolean;
  /** The clipboard-key settings, read LIVE on every keydown so a
   *  pane toggle changes this panel's behavior without a rebuild (the same
   *  pattern as the copy-on-select UI half, which reads the store in
   *  `TerminalPanel.copyOnSelect`). Omitted = both ON (the defaults). */
  clipboardSettings?: () => { ctrlVPastes: boolean; ctrlCCopyOnly: boolean };
  /** `ctrl_v_pastes`: fetch the OS clipboard and push it through
   *  the EXISTING paste path (the same `paste(text)` route a textarea
   *  paste event uses — bracketed-paste handling stays Rust-side). */
  onClipboardPaste?: () => void;
  /** Override the app-shortcut predicate (default: {@link isAppShortcut}). */
  shortcuts?: (e: KeyboardEvent, hasSelection: boolean) => boolean;
}

/** Modifier bits from a DOM event. */
export function modsFromEvent(
  e: Pick<KeyboardEvent, "shiftKey" | "altKey" | "ctrlKey" | "metaKey">,
): number {
  return (
    (e.shiftKey ? MOD_SHIFT : 0) |
    (e.altKey ? MOD_ALT : 0) |
    (e.ctrlKey ? MOD_CTRL : 0) |
    (e.metaKey ? MOD_SUPER : 0)
  );
}

const FUNCTION_KEY_RE = /^F(\d{1,2})$/;

/**
 * Map a DOM KeyboardEvent to a semantic KeyEventDto. Returns null for events
 * the terminal must not see: bare modifier presses, unencodable keys
 * (F13+, PrintScreen…), IME-in-progress, and dead keys (left to composition).
 */
export function dtoFromKeyboardEvent(e: KeyboardEvent): KeyEventDto | null {
  if (e.isComposing) return null;
  const mods = modsFromEvent(e);
  const { key } = e;

  const named: Record<string, KeyDto> = {
    Enter: { kind: "enter" },
    Tab: { kind: "tab" },
    Backspace: { kind: "backspace" },
    Escape: { kind: "escape" },
    ArrowUp: { kind: "up" },
    ArrowDown: { kind: "down" },
    ArrowLeft: { kind: "left" },
    ArrowRight: { kind: "right" },
    Home: { kind: "home" },
    End: { kind: "end" },
    PageUp: { kind: "page_up" },
    PageDown: { kind: "page_down" },
    Insert: { kind: "insert" },
    Delete: { kind: "delete" },
  };
  if (key in named) return { key: named[key], mods };

  const fn = FUNCTION_KEY_RE.exec(key);
  if (fn) {
    const n = Number(fn[1]);
    if (n >= 1 && n <= 12) return { key: { kind: "f", n }, mods };
    return null; // F13+: no mapping (Rust f_key returns None for them)
  }

  switch (key) {
    case "Shift":
    case "Control":
    case "Alt":
    case "Meta":
    case "AltGraph":
    case "CapsLock":
    case "NumLock":
    case "ScrollLock":
    case "PrintScreen":
    case "Pause":
    case "Dead":
    case "Unidentified":
    case "ContextMenu":
      return null;
  }

  if (key.length === 1) {
    return { key: { kind: "char", ch: key }, mods };
  }
  return null;
}

/**
 * Browser-reserved / app-level shortcuts that must NOT reach the terminal.
 * True → the controller keeps the event local (and the caller decides
 * whether to preventDefault). The split:
 * - F11/F12 stay with the browser (fullscreen/devtools) — no preventDefault.
 * - Ctrl+←/↑/↓ are the focus/nav ring (`Ctrl+← Unfocus · Ctrl+↑ Prev ·
 *   Ctrl+↓ Next`) — not terminal keys.
 * - Ctrl/Cmd+C while a selection exists → copy_selection.
 * - Ctrl+Shift+D → debug HUD toggle (hud.ts).
 * - Ctrl+PgUp/PgDn → panel cycle, Ctrl+Shift+T → new shell, Ctrl+Shift+W →
 *   close panel — the terminal never sees them.
 * - Ctrl+F → per-terminal search overlay, Ctrl+Alt+↑/↓ → prompt-mark nav
 * — the panel performs them.
 * Ctrl+Shift+arrows deliberately fall through to the terminal (word-select
 * sequences in TUIs).
 *
 * Clipboard keys (settings, both default ON — see {@link ClipboardShortcutSettings}):
 * - `ctrl_c_copy_only` on: the Ctrl+C reflex WITHOUT a selection is an
 *   app-level no-op — 0x03 never reaches the pty; stopping a process is a
 *   deliberate act on its rail row. With a selection the existing copy
 *   shortcut above still wins. OFF = today's rule (copy iff selection, else
 *   interrupt). Cmd+C (mac) keeps today's behavior in BOTH modes — the
 *   setting is about the Windows/Linux Ctrl reflex.
 * - `ctrl_v_pastes` on: Ctrl+V (no alt/shift/meta) is an app shortcut that
 *   pastes the clipboard through the existing paste path. OFF = today's
 *   passthrough (0x16 — vim's literal-next, etc.).
 *
 * Deliberate consequences (documented, not behavior): while `ctrl_v_pastes`
 * is on, ^V no longer reaches TUIs (vim literal-insert); while
 * `ctrl_c_copy_only` is on, ^C no longer reaches TUIs/agent CLIs — INCLUDING
 * their own cancel/exit gestures. Escape and the rail stop are the remaining
 * cancel paths.
 */
export interface ClipboardShortcutSettings {
  /** `ctrl_v_pastes`: Ctrl+V (no alt/shift/meta) pastes the clipboard. */
  ctrlVPastes: boolean;
  /** `ctrl_c_copy_only`: Ctrl+C without a selection is a no-op (0x03 never
   *  reaches the pty). */
  ctrlCCopyOnly: boolean;
}

/** The defaults: both ON. Deliberate — existing settings.json files
 *  must gain the behavior on upgrade, not keep the legacy passthrough. */
export const DEFAULT_CLIPBOARD_SHORTCUTS: ClipboardShortcutSettings = {
  ctrlVPastes: true,
  ctrlCCopyOnly: true,
};

export function isAppShortcut(
  e: KeyboardEvent,
  hasSelection: boolean,
  clipboard: ClipboardShortcutSettings = DEFAULT_CLIPBOARD_SHORTCUTS,
): boolean {
  if (e.key === "F11" || e.key === "F12") return true;
  if (
    e.ctrlKey &&
    !e.altKey &&
    !e.shiftKey &&
    !e.metaKey &&
    (e.key === "ArrowLeft" || e.key === "ArrowUp" || e.key === "ArrowDown")
  ) {
    return true;
  }
  if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "c" && hasSelection) {
    return true;
  }
  if (clipboard.ctrlCCopyOnly && !hasSelection && isCtrlCCopyOnlyShortcut(e)) return true;
  if (clipboard.ctrlVPastes && isCtrlVPasteShortcut(e)) return true;
  if (isSearchShortcut(e)) return true;
  if (isMarkNavShortcut(e)) return true;
  if (isHudShortcut(e)) return true;
  if (isPanelShortcut(e)) return true;
  if (isProjectShortcut(e)) return true;
  return false;
}

/**
 * Ctrl+F — the search overlay. Recognized so the terminal never sees
 * it; the panel's document-level listener performs the open/refocus.
 */
export function isSearchShortcut(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): boolean {
  return e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && e.key.toLowerCase() === "f";
}

/**
 * Ctrl+Alt+↑/↓ — prompt-mark navigation. Same reserved-for-the-panel
 * pattern as the search shortcut.
 */
export function isMarkNavShortcut(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): boolean {
  return (
    e.ctrlKey &&
    e.altKey &&
    !e.shiftKey &&
    !e.metaKey &&
    (e.key === "ArrowUp" || e.key === "ArrowDown")
  );
}

/**
 * The panel chords: Ctrl+PgUp/PgDn (cycle), Ctrl+Shift+T (new shell),
 * Ctrl+Shift+W (close active panel). Recognized here so the terminal never
 * sees them; the App listens on window for the same combos to perform the
 * action (same pattern as the HUD and its Ctrl+Shift+D).
 */
export function isPanelShortcut(e: Pick<KeyboardEvent, "key" | "ctrlKey" | "shiftKey" | "altKey" | "metaKey">): boolean {
  if (!e.ctrlKey || e.altKey || e.metaKey) return false;
  if (e.shiftKey) {
    const k = e.key.toLowerCase();
    return k === "t" || k === "w";
  }
  return e.key === "PageUp" || e.key === "PageDown";
}

/**
 * The project chords:
 * Ctrl+Shift+S (start auto-starting), Ctrl+Shift+A (start all), Ctrl+Shift+P
 * (stop all).
 * Same shape as {@link isPanelShortcut} so the terminal never sees them; the
 * App listens on window for the same combos to perform the action.
 */
export function isProjectShortcut(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "shiftKey" | "altKey" | "metaKey">,
): boolean {
  if (!e.ctrlKey || !e.shiftKey || e.altKey || e.metaKey) return false;
  const k = e.key.toLowerCase();
  return k === "s" || k === "a" || k === "p";
}

/**
 * True for the HUD toggle specifically: Ctrl+Shift+D (no alt/meta). Kept in
 * `isAppShortcut` so the terminal never sees it; the HUD listens for the same
 * combo on window (hud.ts) to flip its overlay.
 */
export function isHudShortcut(e: Pick<KeyboardEvent, "key" | "ctrlKey" | "shiftKey" | "altKey" | "metaKey">): boolean {
  return e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === "d";
}

/** True for the copy shortcut specifically (Ctrl/Cmd+C with a selection). */
export function isCopyShortcut(e: KeyboardEvent, hasSelection: boolean): boolean {
  return (e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "c" && hasSelection;
}

/**
 * The Ctrl+C reflex itself — Ctrl held, no Alt — that can encode a
 * raw 0x03. Shift/Meta variants are INCLUDED on purpose: with kitty
 * disambiguation off (a plain shell negotiates none), Ctrl+Shift+C and
 * Ctrl+Meta+C encode the SAME 0x03 byte, so a bare-Ctrl+C-only guard would
 * leave a keyboard-interrupt path and break the "0x03 never reaches the pty
 * while ON" contract. Alt is EXCLUDED: Alt+Ctrl+C carries an ESC prefix
 * (a different gesture) and Alt combos stay unchanged. Cmd-without-
 * Ctrl (the mac Cmd+C) never sets the ctrl bit and so is untouched in both
 * modes. The controller turns this into a no-op only while `ctrlCCopyOnly`
 * is on AND no selection is active (with a selection, the copy shortcut wins).
 */
export function isCtrlCCopyOnlyShortcut(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): boolean {
  return e.ctrlKey && !e.altKey && e.key.toLowerCase() === "c";
}

/**
 * Ctrl+V with no alt/shift/meta — the paste-the-clipboard shortcut.
 * The setting is checked by the caller (it reads the live settings store,
 * exactly the way the copy-on-select UI half does); this predicate is the
 * pure modifier discipline. Cmd+V (mac) is NOT covered — this is about the
 * Windows/Linux reflex — and Ctrl+Shift+V / Alt variants reach the terminal
 * untouched, as today.
 */
export function isCtrlVPasteShortcut(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): boolean {
  return (
    e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && e.key.toLowerCase() === "v"
  );
}

/**
 * Fractional wheel lines for one event, deltaMode-aware (0 pixels, 1 lines,
 * 2 pages). NOT rounded: precision trackpads deliver sub-cell deltas that
 * must accumulate (a lesson learned the hard way) — see `wheelQuanta`.
 */
export function rawWheelLines(e: WheelEvent, cellH: number, rows: number): number {
  switch (e.deltaMode) {
    case 1:
      return e.deltaY;
    case 2:
      return e.deltaY * rows;
    default:
      return e.deltaY / cellH;
  }
}

/** Accumulated wheel state: fractional lines carry their remainder forward. */
export interface WheelState {
  remainder: number;
}

/**
 * Push one event's fractional lines and return the whole-cell quanta to emit
 * (truncated toward zero), keeping the remainder in `state`. Sub-cell ticks
 * accumulate until they cross a whole cell — a trackpad must not fire one
 * scroll/mouse event per notch.
 */
export function wheelQuanta(state: WheelState, raw: number): number {
  state.remainder += raw;
  const q = Math.trunc(state.remainder);
  state.remainder -= q;
  // Math.trunc(-0.5) is -0; normalize so "no whole cell crossed" is always 0.
  return q === 0 ? 0 : q;
}

/**
 * Quote one OS file path for pasting into a shell: double-quoted with inner
 * backslashes and double quotes escaped. Multiple paths are space-separated.
 */
export function quotePath(path: string): string {
  return `"${path.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/** The paste payload for an OS file drop: every path quoted, space-separated. */
export function quotePaths(paths: string[]): string {
  return paths.map(quotePath).join(" ");
}

export class InputController {
  private readonly opts: InputControllerOptions;
  private readonly keydown: (e: KeyboardEvent) => void;
  private readonly paste: (e: ClipboardEvent) => void;
  private readonly compStart: () => void;
  private readonly compUpdate: (e: CompositionEvent) => void;
  private readonly compEnd: (e: CompositionEvent) => void;
  private readonly mouseDown: (e: MouseEvent) => void;
  private readonly mouseMove: (e: MouseEvent) => void;
  private readonly mouseUp: (e: MouseEvent) => void;
  private readonly wheel: (e: WheelEvent) => void;
  private readonly context: (e: MouseEvent) => void;
  private readonly dragover: (e: DragEvent) => void;
  private readonly drop: (e: DragEvent) => void;

  private selecting = false;
  private selKind: SelectionKindDto = "simple";
  private readonly wheelState: WheelState = { remainder: 0 };

  constructor(opts: InputControllerOptions) {
    this.opts = opts;

    this.keydown = (e) => this.onKeydown(e);
    this.paste = (e) => this.onPaste(e);
    this.compStart = () => this.onCompositionStart();
    this.compUpdate = (e) => this.onCompositionUpdate(e);
    this.compEnd = (e) => this.onCompositionEnd(e);
    this.mouseDown = (e) => this.onMouseDown(e);
    this.mouseMove = (e) => this.onMouseMove(e);
    this.mouseUp = (e) => this.onMouseUp(e);
    this.wheel = (e) => this.onWheel(e);
    this.context = (e) => e.preventDefault();
    this.dragover = (e) => e.preventDefault();
    this.drop = (e) => this.onDrop(e);

    opts.textarea.addEventListener("keydown", this.keydown);
    opts.textarea.addEventListener("paste", this.paste);
    opts.textarea.addEventListener("compositionstart", this.compStart);
    opts.textarea.addEventListener("compositionupdate", this.compUpdate);
    opts.textarea.addEventListener("compositionend", this.compEnd);
    opts.element.addEventListener("mousedown", this.mouseDown);
    opts.element.addEventListener("mousemove", this.mouseMove);
    opts.element.addEventListener("mouseup", this.mouseUp);
    opts.element.addEventListener("wheel", this.wheel, { passive: false });
    opts.element.addEventListener("contextmenu", this.context);
    // OS file drops paste the dropped paths; both preventDefault so the
    // webview never navigates to the file: an easy regression to reintroduce,
    // so the guard is explicit.
    opts.element.addEventListener("dragover", this.dragover);
    opts.element.addEventListener("drop", this.drop);

    opts.compose.style.position = "absolute";
    opts.compose.style.visibility = "hidden";
    opts.compose.className = "chappa-compose";
  }

  destroy(): void {
    const { textarea, element } = this.opts;
    textarea.removeEventListener("keydown", this.keydown);
    textarea.removeEventListener("paste", this.paste);
    textarea.removeEventListener("compositionstart", this.compStart);
    textarea.removeEventListener("compositionupdate", this.compUpdate);
    textarea.removeEventListener("compositionend", this.compEnd);
    element.removeEventListener("mousedown", this.mouseDown);
    element.removeEventListener("mousemove", this.mouseMove);
    element.removeEventListener("mouseup", this.mouseUp);
    element.removeEventListener("wheel", this.wheel);
    element.removeEventListener("contextmenu", this.context);
    element.removeEventListener("dragover", this.dragover);
    element.removeEventListener("drop", this.drop);
  }

  /** Focus the hidden textarea (e.g. when the panel regains focus). */
  focus(): void {
    this.opts.textarea.focus();
  }

  private onKeydown(e: KeyboardEvent): void {
    const hasSelection = this.opts.hasSelection();
    // The clipboard-key settings are read LIVE here (the pane's
    // toggle must change an open panel without a rebuild). Omitted options
    // stand for the defaults — both ON.
    const clipboard =
      this.opts.clipboardSettings?.() ?? DEFAULT_CLIPBOARD_SHORTCUTS;
    const isShortcut =
      this.opts.shortcuts?.(e, hasSelection) ??
      isAppShortcut(e, hasSelection, clipboard);
    if (isShortcut) {
      if (isCopyShortcut(e, hasSelection)) {
        e.preventDefault();
        this.opts.onCopy();
      } else if (clipboard.ctrlVPastes && isCtrlVPasteShortcut(e)) {
        // `ctrl_v_pastes`: pull the clipboard and push it through the
        // EXISTING paste path (the panel reads navigator.clipboard and calls
        // its `paste(text)` — bracketed-paste handling stays Rust-side).
        e.preventDefault();
        this.opts.onClipboardPaste?.();
      } else if (clipboard.ctrlCCopyOnly && !hasSelection && isCtrlCCopyOnlyShortcut(e)) {
        // `ctrl_c_copy_only`: the reflex ^C is a deliberate no-op —
        // 0x03 never reaches the pty; stopping a process is a deliberate act
        // on its rail row.
        e.preventDefault();
      } else if (e.key === "F11" || e.key === "F12") {
        // Browser keeps these; no preventDefault, no terminal.
      } else {
        e.preventDefault(); // app-level (focus nav): keep the caret put
      }
      return;
    }
    const dto = dtoFromKeyboardEvent(e);
    if (!dto) return; // dead key / modifier / unknown: leave to the browser
    e.preventDefault();
    this.opts.onKey(dto);
  }

  private onPaste(e: ClipboardEvent): void {
    const text = e.clipboardData?.getData("text") ?? "";
    e.preventDefault();
    if (text) this.opts.onPaste(text);
  }

  private onCompositionStart(): void {
    const { compose } = this.opts;
    compose.textContent = "";
    const cursor = this.opts.cursorCell();
    if (cursor) {
      compose.style.left = `${cursor.col * this.opts.cellW()}px`;
      compose.style.top = `${cursor.row * this.opts.cellH()}px`;
    } else {
      compose.style.left = "0px";
      compose.style.top = "0px";
    }
    compose.style.visibility = "visible";
  }

  private onCompositionUpdate(e: CompositionEvent): void {
    this.opts.compose.textContent = e.data ?? "";
  }

  private onCompositionEnd(e: CompositionEvent): void {
    this.opts.compose.style.visibility = "hidden";
    const text = e.data ?? "";
    if (text) this.opts.onPaste(text); // commit as paste: simpler + mode-correct
  }

  private cellFromEvent(e: MouseEvent): { row: number; col: number } {
    const rect = this.opts.element.getBoundingClientRect();
    const col = Math.floor((e.clientX - rect.left) / this.opts.cellW());
    const row = Math.floor((e.clientY - rect.top) / this.opts.cellH());
    return {
      col: clamp(col, 0, Math.max(0, this.opts.cols() - 1)),
      row: clamp(row, 0, Math.max(0, this.opts.rows() - 1)),
    };
  }

  private onMouseDown(e: MouseEvent): void {
    // Reclaim keyboard focus for the hidden textarea: preventDefault below
    // suppresses the browser's native focus-on-click, so without this a
    // terminal that lost focus (devtools, another widget) is permanently
    // deaf to the keyboard — clicks LOOK like they focus it but don't.
    this.opts.textarea.focus();
    const { col, row } = this.cellFromEvent(e);
    // A mouse-capturing TUI (vim) owns the press; Shift is the standard
    // override that restores local selection.
    const captured = this.opts.hasMouseCapture() && !e.shiftKey;
    this.opts.onMouse({ kind: "press", button: e.button, col, row, mods: modsFromEvent(e) });
    if (e.button === 0 && !captured) {
      // Ctrl/Cmd+click is the link opener — never also a selection.
      const linkClick = (modsFromEvent(e) & (MOD_CTRL | MOD_SUPER)) !== 0;
      // e.detail is the click count (1/2/3) → simple/word/line selection.
      if (!linkClick) {
        this.selecting = true;
        this.selKind = e.detail >= 3 ? "lines" : e.detail === 2 ? "semantic" : "simple";
        this.opts.onSelection({ op: "start", point: { row, col }, kind: this.selKind });
      }
    }
    e.preventDefault();
  }

  private onMouseMove(e: MouseEvent): void {
    const { col, row } = this.cellFromEvent(e);
    if (this.opts.hasMouseCapture() && !e.shiftKey) {
      // TUI owns the drag: forward motion only, never a local selection op.
      this.opts.onMouse({
        kind: (e.buttons & 1) !== 0 ? "drag" : "move",
        button: 0,
        col,
        row,
        mods: modsFromEvent(e),
      });
      return;
    }
    if ((e.buttons & 1) !== 0 && this.selecting) {
      this.opts.onMouse({ kind: "drag", button: 0, col, row, mods: modsFromEvent(e) });
      this.opts.onSelection({ op: "update", point: { row, col }, kind: this.selKind });
    } else {
      this.opts.onMouse({ kind: "move", button: 0, col, row, mods: modsFromEvent(e) });
    }
  }

  private onMouseUp(e: MouseEvent): void {
    const { col, row } = this.cellFromEvent(e);
    this.opts.onMouse({ kind: "release", button: e.button, col, row, mods: modsFromEvent(e) });
    if (e.button === 0) {
      const wasSelecting = this.selecting;
      this.selecting = false;
      // Selection finished: the copy-on-select hook. Only when a local
      // selection was actually in progress — a mouse-capturing TUI owns the
      // drag and never started one.
      if (wasSelecting) this.opts.onSelectionEnd?.();
    }
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    // Accumulate fractional/high-res deltas into whole-cell quanta BEFORE any
    // emit (scroll command or mouse DTO); sub-cell ticks carry their remainder.
    const raw = rawWheelLines(e, this.opts.cellH(), this.opts.rows());
    const quanta = wheelQuanta(this.wheelState, raw);
    if (quanta === 0) return;
    const captured = this.opts.hasMouseCapture() && !e.shiftKey;
    this.opts.onMouse({
      kind: quanta > 0 ? "wheel_down" : "wheel_up",
      button: 0,
      col: 0,
      row: 0,
      mods: modsFromEvent(e),
    });
    // Mouse-captured TUI (vim/htop): the wheel belongs to the app only —
    // ALSO scrolling scrollback locally is the known double-handling. Shift
    // is the override that restores local scroll.
    if (!captured) this.opts.onScroll(-quanta);
  }

  private onDrop(e: DragEvent): void {
    // One code path: dropped paths become a quoted paste payload through the
    // normal paste route (so the bracketed-paste guard applies). Never a
    // browser fallback — an easy regression to reintroduce, so it is
    // guarded explicitly.
    // NOTE: under Tauri's default drag-drop interception this handler never
    // fires (and HTML5 File objects carry no OS path in WebView2 anyway) —
    // the production route is App.handleFileDrop over `tauri://drag-drop`,
    // which uses the same quotePaths→paste path. This stays as the
    // suppress-navigation guard + fallback for interception-off configs.
    e.preventDefault();
    const files = e.dataTransfer?.files;
    if (!files || files.length === 0) return;
    const paths: string[] = [];
    for (const file of Array.from(files)) {
      const path = (file as File & { path?: string }).path;
      if (typeof path === "string" && path.length > 0) paths.push(path);
    }
    if (paths.length > 0) this.opts.onPaste(quotePaths(paths));
  }
}

function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}
