// Notification levels — the single decision point.
//
// ONE side owns the decision and everything routes through it: a pure
// decision function in the ui, since focus state lives there. This is that
// side. The Rust pump no longer fires OS notifications
// itself; it emits raw `term://bell` / `term://notify` events and the UI
// decides, so a level change costs zero Rust churn.
//
// Nothing here touches the DOM or ipc — app.ts feeds it (kind, level,
// is-active, window-focused, body) and acts on the answer.

/** The three levels, exactly the wire vocabulary (`notification_level`). */
export type Level = "all" | "important" | "none";

/** What arrived: a BEL (`term://bell`) or an OSC 9/777/99 notify
 *  (`term://notify`). */
export type EventKind = "bell" | "notify";

/** The answer: fire an OS notification, badge the rail/card, or neither. */
export interface Decision {
  osNotify: boolean;
  badge: boolean;
}

/** Human labels for the segmented picker and the context submenu. */
export const LEVEL_LABELS: Record<Level, string> = {
  all: "All",
  important: "Important",
  none: "None",
};

/** The picker/submenu option order. */
export const LEVELS: Level[] = ["all", "important", "none"];

/** A wire string → Level, or null when it is absent/unknown. Unknown strings
 *  are NOT guessed at: `resolveLevel` degrades them to the next tier. */
export function parseLevel(value: string | null | undefined): Level | null {
  return value === "all" || value === "important" || value === "none" ? value : null;
}

/**
 * Per terminal, resolved as: per-process override → project default →
 * 'all'.
 *
 * An unknown/garbage string at either tier is treated as absent, so it falls
 * through to the next tier rather than poisoning the resolution.
 */
export function resolveLevel(
  override: string | null | undefined,
  projectDefault: string | null | undefined,
): Level {
  return parseLevel(override) ?? parseLevel(projectDefault) ?? "all";
}

/**
 * Numeric-only notification bodies are progress-counter spam (a build/test
 * runner re-emitting OSC 9 per percent). They badge only, never an OS
 * notification, at EVERY level.
 *
 * Definition of numeric-only used here:
 * the TRIMMED body is non-empty, contains at least one digit, and consists
 * solely of digits, whitespace and the punctuation `. , : % / ( ) - +`.
 *
 * Numeric-only: `42%`, `3/10`, `1:23:45`, `(7/9)`, `12.5 %`.
 * NOT numeric-only: `build 42` and `7 of 9` (letters), `---` (no digit),
 * `""` / `"   "` (a bodyless notify must still be able to toast).
 *
 * Bells carry no body, so this rule never applies to them.
 */
export function isNumericOnlyBody(body: string | null | undefined): boolean {
  if (typeof body !== "string") return false;
  const trimmed = body.trim();
  if (trimmed === "") return false;
  if (!/[0-9]/.test(trimmed)) return false;
  // Printable-escape only: the class is spelled out literally, no control
  // bytes and no \x.. escapes.
  return /^[0-9\s.,:%/()+-]+$/.test(trimmed);
}

/**
 * The whole policy, in one pure function.
 *
 * Focused rule, quoted verbatim: "if the terminal is
 * the active panel and the window is focused, suppress ENTIRELY — no OS
 * notification AND no badge". A badge means "something happened while you
 * weren't looking"; focused means you were looking. Active-but-window-
 * unfocused and hidden-but-window-focused both still notify per level.
 *
 * Per level:
 * - All: OSC 9/777/99 notify → OS notification; bell → OS notification too;
 *   both also badge the rail.
 * - Important: OSC notify → OS notification; bell → badge only.
 * - None: "everything badges silently; no OS notifications."
 */
export function decide(
  kind: EventKind,
  level: Level,
  isActive: boolean,
  windowFocused: boolean,
  body?: string | null,
): Decision {
  if (isActive && windowFocused) return { osNotify: false, badge: false };
  const osNotify =
    level !== "none" &&
    !isNumericOnlyBody(body) &&
    (kind === "notify" || level === "all");
  return { osNotify, badge: true };
}
