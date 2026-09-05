import { describe, expect, it } from "vitest";
import {
  decide,
  isNumericOnlyBody,
  LEVELS,
  LEVEL_LABELS,
  parseLevel,
  resolveLevel,
  type Decision,
  type EventKind,
  type Level,
} from "./notifications";

/** One matrix row, written out longhand — the FULL table (3 levels × 2
 *  kinds × focus states = 24 rows) is table-driven, so no row is generated
 *  from the rule it is meant to pin. */
interface Row {
  kind: EventKind;
  level: Level;
  isActive: boolean;
  windowFocused: boolean;
  want: Decision;
}

const N = { osNotify: false, badge: false };
const BADGE = { osNotify: false, badge: true };
const BOTH = { osNotify: true, badge: true };

const MATRIX: Row[] = [
  // --- All: "OSC notify → OS notification; bell → OS notification too;
  //     both also badge the rail." -------------------------------------------
  { kind: "bell", level: "all", isActive: true, windowFocused: true, want: N },
  { kind: "bell", level: "all", isActive: true, windowFocused: false, want: BOTH },
  { kind: "bell", level: "all", isActive: false, windowFocused: true, want: BOTH },
  { kind: "bell", level: "all", isActive: false, windowFocused: false, want: BOTH },
  { kind: "notify", level: "all", isActive: true, windowFocused: true, want: N },
  { kind: "notify", level: "all", isActive: true, windowFocused: false, want: BOTH },
  { kind: "notify", level: "all", isActive: false, windowFocused: true, want: BOTH },
  { kind: "notify", level: "all", isActive: false, windowFocused: false, want: BOTH },

  // --- Important: "OSC notify → OS notification; bell → badge only." --------
  { kind: "bell", level: "important", isActive: true, windowFocused: true, want: N },
  { kind: "bell", level: "important", isActive: true, windowFocused: false, want: BADGE },
  { kind: "bell", level: "important", isActive: false, windowFocused: true, want: BADGE },
  { kind: "bell", level: "important", isActive: false, windowFocused: false, want: BADGE },
  { kind: "notify", level: "important", isActive: true, windowFocused: true, want: N },
  { kind: "notify", level: "important", isActive: true, windowFocused: false, want: BOTH },
  { kind: "notify", level: "important", isActive: false, windowFocused: true, want: BOTH },
  { kind: "notify", level: "important", isActive: false, windowFocused: false, want: BOTH },

  // --- None: "everything badges silently; no OS notifications." -------------
  { kind: "bell", level: "none", isActive: true, windowFocused: true, want: N },
  { kind: "bell", level: "none", isActive: true, windowFocused: false, want: BADGE },
  { kind: "bell", level: "none", isActive: false, windowFocused: true, want: BADGE },
  { kind: "bell", level: "none", isActive: false, windowFocused: false, want: BADGE },
  { kind: "notify", level: "none", isActive: true, windowFocused: true, want: N },
  { kind: "notify", level: "none", isActive: true, windowFocused: false, want: BADGE },
  { kind: "notify", level: "none", isActive: false, windowFocused: true, want: BADGE },
  { kind: "notify", level: "none", isActive: false, windowFocused: false, want: BADGE },
];

describe("decide — the 24-row matrix", () => {
  it("covers every level × kind × focus combination exactly once", () => {
    expect(MATRIX.length).toBe(24);
    const keys = new Set(
      MATRIX.map((r) => `${r.level}/${r.kind}/${r.isActive}/${r.windowFocused}`),
    );
    expect(keys.size).toBe(24);
  });

  for (const r of MATRIX) {
    const name =
      `${r.level} · ${r.kind} · ${r.isActive ? "active" : "hidden"} · ` +
      `${r.windowFocused ? "window focused" : "window unfocused"}`;
    it(name, () => {
      expect(decide(r.kind, r.level, r.isActive, r.windowFocused)).toEqual(r.want);
    });
  }

  it("suppresses ENTIRELY when active AND focused, at every level (focused rule)", () => {
    // "if the terminal is the active panel and the window is focused, suppress
    // ENTIRELY — no OS notification AND no badge".
    for (const level of LEVELS) {
      for (const kind of ["bell", "notify"] as EventKind[]) {
        expect(decide(kind, level, true, true)).toEqual({ osNotify: false, badge: false });
      }
    }
  });

  it("still notifies when active-but-window-unfocused or hidden-but-focused", () => {
    // Only the CONJUNCTION suppresses; either half alone must not.
    expect(decide("notify", "important", true, false).badge).toBe(true);
    expect(decide("notify", "important", false, true).badge).toBe(true);
    expect(decide("notify", "important", true, false).osNotify).toBe(true);
    expect(decide("notify", "important", false, true).osNotify).toBe(true);
  });
});

describe("decide — numeric-only bodies", () => {
  const SPAM = ["42", "42%", "3/10", "1:23:45", "  87 % ", "(7/9)", "12.5", "-3", "+5", "1,024"];
  const NOT_SPAM = ["build 42", "7 of 9", "done", "---", "", "   ", "100 tests passed"];

  it("classifies progress counters as numeric-only", () => {
    for (const body of SPAM) expect(isNumericOnlyBody(body)).toBe(true);
  });

  it("does not classify text-bearing, digitless, or empty bodies as numeric-only", () => {
    for (const body of NOT_SPAM) expect(isNumericOnlyBody(body)).toBe(false);
    expect(isNumericOnlyBody(undefined)).toBe(false);
    expect(isNumericOnlyBody(null)).toBe(false);
  });

  it("badges but NEVER OS-notifies a numeric body, at EVERY level", () => {
    // "Numeric-only notification bodies (progress-counter spam) → badge only,
    // never an OS notification, at EVERY level.
    for (const level of LEVELS) {
      for (const body of SPAM) {
        expect(decide("notify", level, false, true, body)).toEqual({
          osNotify: false,
          badge: true,
        });
      }
    }
  });

  it("leaves a text body notifying per level", () => {
    expect(decide("notify", "all", false, true, "build 42")).toEqual(BOTH);
    expect(decide("notify", "important", false, true, "done")).toEqual(BOTH);
    expect(decide("notify", "none", false, true, "done")).toEqual(BADGE);
    // Empty/whitespace body is NOT numeric-only: a bodyless notify still toasts.
    expect(decide("notify", "all", false, true, "").osNotify).toBe(true);
    expect(decide("notify", "all", false, true, "   ").osNotify).toBe(true);
  });

  it("never applies the rule to bells (they carry no body)", () => {
    // A bell's body argument is simply absent — the numeric gate is a no-op.
    expect(decide("bell", "all", false, true)).toEqual(BOTH);
    // Even if a caller passed a numeric body, a level-all bell is not spam-
    // shaped in practice; but the gate is body-driven, so it WOULD suppress.
    // Pinning the real call shape: app.ts passes no body for bells.
    expect(decide("bell", "all", false, true, undefined)).toEqual(BOTH);
  });

  it("a numeric body cannot resurrect a suppressed focused terminal", () => {
    expect(decide("notify", "all", true, true, "42%")).toEqual(N);
  });
});

describe("resolveLevel — override → project default → 'all'", () => {
  it("prefers the per-process override", () => {
    expect(resolveLevel("none", "all")).toBe("none");
    expect(resolveLevel("important", null)).toBe("important");
    expect(resolveLevel("all", "none")).toBe("all");
  });

  it("falls through to the project default when there is no override", () => {
    expect(resolveLevel(null, "important")).toBe("important");
    expect(resolveLevel(undefined, "none")).toBe("none");
  });

  it("falls all the way to 'all' when neither tier is set", () => {
    expect(resolveLevel(null, null)).toBe("all");
    expect(resolveLevel(undefined, undefined)).toBe("all");
  });

  it("degrades garbage at either tier to the NEXT tier, never guesses", () => {
    expect(resolveLevel("loud", "none")).toBe("none");
    expect(resolveLevel("", "important")).toBe("important");
    expect(resolveLevel("ALL", "none")).toBe("none"); // case-sensitive wire value
    expect(resolveLevel("nope", "also-nope")).toBe("all");
    expect(resolveLevel(null, "quiet")).toBe("all");
  });

  it("parseLevel answers null for anything off the wire vocabulary", () => {
    expect(parseLevel("all")).toBe("all");
    expect(parseLevel("important")).toBe("important");
    expect(parseLevel("none")).toBe("none");
    expect(parseLevel("All")).toBeNull();
    expect(parseLevel(null)).toBeNull();
    expect(parseLevel(undefined)).toBeNull();
  });

  it("labels every level for the picker and the submenu", () => {
    expect(LEVELS.map((l) => LEVEL_LABELS[l])).toEqual(["All", "Important", "None"]);
  });
});
