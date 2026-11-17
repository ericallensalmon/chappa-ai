// The keyboard platform: which modifier the app's own shortcuts use, and how
// shortcuts are spelled in the UI.
//
// macOS reserves Cmd for applications and leaves Ctrl to the terminal, so
// there every app shortcut is a Cmd chord and Ctrl+<key> always reaches the
// pty (Ctrl+C stays an interrupt). Elsewhere Ctrl is the app modifier, as
// it always was. Every shortcut predicate asks `hasAppMod` instead of
// reading ctrlKey/metaKey itself, and every label goes through
// `shortcutLabel`, so the two never disagree.

export type Platform = "mac" | "other";

/** The parts of `navigator` the detection reads. */
export interface NavigatorLike {
  platform?: string;
  userAgent?: string;
  userAgentData?: { platform?: string };
}

/** macOS when the webview says so (iPad user agents count: same keys). */
export function detectPlatform(nav: NavigatorLike | undefined): Platform {
  if (!nav) return "other";
  const hint = nav.userAgentData?.platform || nav.platform || nav.userAgent || "";
  return /mac|iphone|ipad/i.test(hint) ? "mac" : "other";
}

let current: Platform | null = null;

/** This webview's platform, detected once. */
export function platform(): Platform {
  if (current === null) {
    current = detectPlatform(typeof navigator === "undefined" ? undefined : navigator);
  }
  return current;
}

/** Pin the platform (tests); `null` detects it again on the next call. */
export function setPlatform(p: Platform | null): void {
  current = p;
}

export function isMac(): boolean {
  return platform() === "mac";
}

/**
 * The app modifier is held and the other one is not: Cmd without Ctrl on
 * macOS, Ctrl without Cmd/Win elsewhere. Shift and Alt are the caller's to
 * check.
 */
export function hasAppMod(e: Pick<KeyboardEvent, "ctrlKey" | "metaKey">): boolean {
  return isMac() ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
}

const MAC_GLYPHS: Record<string, string> = {
  Ctrl: "⌃",
  Alt: "⌥",
  Shift: "⇧",
  Mod: "⌘",
};
/** The order macOS menus print modifiers in. */
const MAC_ORDER = ["Ctrl", "Alt", "Shift", "Mod"];
const KEY_NAMES: Record<string, string> = {
  ArrowLeft: "←",
  ArrowUp: "↑",
  ArrowRight: "→",
  ArrowDown: "↓",
  PageUp: "PgUp",
  PageDown: "PgDn",
};

/**
 * A shortcut as this platform spells it. `spec` is `+`-separated with `Mod`
 * for the app modifier, e.g. `Mod+Shift+T`: `Ctrl+Shift+T` elsewhere, `⇧⌘T`
 * on macOS.
 */
export function shortcutLabel(spec: string): string {
  const parts = spec.split("+");
  const key = parts.pop() ?? "";
  const shown = KEY_NAMES[key] ?? key;
  if (!isMac()) {
    return [...parts.map((p) => (p === "Mod" ? "Ctrl" : p)), shown].join("+");
  }
  const mods = MAC_ORDER.filter((m) => parts.includes(m)).map((m) => MAC_GLYPHS[m]);
  return mods.join("") + shown;
}
