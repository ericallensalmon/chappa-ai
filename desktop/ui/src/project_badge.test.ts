// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  PROJECT_COLORS,
  deriveColor,
  deriveInitials,
  isProjectColor,
  projectBadge,
  resolveIdentity,
} from "./project_badge";

describe("project badge", () => {
  it("derives initials from the first two words, or the first two letters of one", () => {
    expect(deriveInitials("chappa-ai")).toBe("CA");
    expect(deriveInitials("lab cluster")).toBe("LC");
    expect(deriveInitials("Demo Project")).toBe("DP");
    // A third word earns a third letter, so numbered siblings stay apart.
    expect(deriveInitials("Demo Project 2")).toBe("DP2");
    expect(deriveInitials("one two three four")).toBe("OTT");
    expect(deriveInitials("twofivesix")).toBe("TW");
    expect(deriveInitials("x")).toBe("X");
    expect(deriveInitials("---")).toBe("?");
  });

  it("badges a work-item key with as much of it as fits in five characters", () => {
    expect(deriveInitials("ca123")).toBe("CA123");
    expect(deriveInitials("ab-12")).toBe("AB12");
    expect(deriveInitials("vtc-1234")).toBe("V1234");
    expect(deriveInitials("vtc-12345")).toBe("12345");
    expect(deriveInitials("VTC_12345 checkout redesign")).toBe("12345");
    expect(deriveInitials("vtc-1234567")).toBe("34567");
    expect(deriveInitials("ca123-web")).toBe("CA123");
    // Not keys: letters run on after the digits, no digits, or words first.
    expect(deriveInitials("v2ray")).toBe("V2");
    expect(deriveInitials("chappa-ai")).toBe("CA");
    expect(deriveInitials("Demo Project 2")).toBe("DP2");
    // A typed badge still wins over a derived one.
    expect(resolveIdentity({ name: "vtc-12345", initials: "X" }).initials).toBe("X");
  });

  it("derives a stable palette colour, never gray", () => {
    const first = deriveColor("chappa-ai");
    expect(isProjectColor(first)).toBe(true);
    expect(first).not.toBe("gray");
    expect(deriveColor("chappa-ai")).toBe(first);
    // Different names spread across the palette rather than collapsing.
    const names = ["chappa-ai", "twofivesix", "lab cluster", "healthtracker", "Demo Project", "notes"];
    expect(new Set(names.map(deriveColor)).size).toBeGreaterThan(1);
  });

  it("stored overrides win over the derived halves, blanks and unknowns fall back", () => {
    const derived = resolveIdentity({ name: "chappa-ai" });
    expect(derived.initials).toBe("CA");
    const forced = resolveIdentity({ name: "chappa-ai", initials: "CH", color: "green" });
    expect(forced.initials).toBe("CH");
    expect(forced.color.key).toBe("green");
    const blank = resolveIdentity({ name: "chappa-ai", initials: "  ", color: "not-a-colour" });
    expect(blank.initials).toBe("CA");
    expect(blank.color.key).toBe(derived.color.key);
  });

  it("paints a badge with the resolved letters and colours inline, titled by the full name", () => {
    const el = projectBadge({ name: "lab cluster", color: "teal" });
    const teal = PROJECT_COLORS.find((c) => c.key === "teal")!;
    expect(el.className).toBe("chappa-project-badge");
    expect(el.textContent).toBe("LC");
    expect(el.title).toBe("lab cluster");
    expect(el.dataset.color).toBe("teal");
    expect(el.style.backgroundColor).not.toBe("");
    expect(el.style.color).not.toBe("");
    expect(teal.bg).toMatch(/^#/);
  });
});
