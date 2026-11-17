import { afterEach, describe, expect, it } from "vitest";
import { detectPlatform, hasAppMod, setPlatform, shortcutLabel } from "./platform";

afterEach(() => setPlatform(null));

describe("detectPlatform", () => {
  it("recognises macOS from any of the navigator hints", () => {
    expect(detectPlatform({ userAgentData: { platform: "macOS" } })).toBe("mac");
    expect(detectPlatform({ platform: "MacIntel" })).toBe("mac");
    expect(
      detectPlatform({ userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15" }),
    ).toBe("mac");
  });

  it("treats everything else as the Ctrl platform", () => {
    expect(detectPlatform({ platform: "Win32" })).toBe("other");
    expect(detectPlatform({ platform: "Linux x86_64" })).toBe("other");
    // The newer hint wins over the older one.
    expect(detectPlatform({ userAgentData: { platform: "Windows" }, platform: "MacIntel" })).toBe("other");
    expect(detectPlatform({})).toBe("other");
    expect(detectPlatform(undefined)).toBe("other");
  });
});

describe("hasAppMod", () => {
  it("is Cmd without Ctrl on macOS", () => {
    setPlatform("mac");
    expect(hasAppMod({ metaKey: true, ctrlKey: false })).toBe(true);
    expect(hasAppMod({ metaKey: false, ctrlKey: true })).toBe(false);
    expect(hasAppMod({ metaKey: true, ctrlKey: true })).toBe(false);
  });

  it("is Ctrl without Cmd/Win elsewhere", () => {
    setPlatform("other");
    expect(hasAppMod({ metaKey: false, ctrlKey: true })).toBe(true);
    expect(hasAppMod({ metaKey: true, ctrlKey: false })).toBe(false);
    expect(hasAppMod({ metaKey: true, ctrlKey: true })).toBe(false);
  });
});

describe("shortcutLabel", () => {
  it("spells shortcuts with Ctrl and plus signs elsewhere", () => {
    setPlatform("other");
    expect(shortcutLabel("Mod+T")).toBe("Ctrl+T");
    expect(shortcutLabel("Mod+Shift+T")).toBe("Ctrl+Shift+T");
    expect(shortcutLabel("Mod+Shift+S/A/P")).toBe("Ctrl+Shift+S/A/P");
    expect(shortcutLabel("Mod+Alt+ArrowUp")).toBe("Ctrl+Alt+↑");
    expect(shortcutLabel("Mod+PageDown")).toBe("Ctrl+PgDn");
  });

  it("spells shortcuts with the menu glyphs, in menu order, on macOS", () => {
    setPlatform("mac");
    expect(shortcutLabel("Mod+T")).toBe("⌘T");
    expect(shortcutLabel("Mod+Shift+T")).toBe("⇧⌘T");
    expect(shortcutLabel("Shift+Mod+T")).toBe("⇧⌘T");
    expect(shortcutLabel("Mod+Alt+ArrowUp")).toBe("⌥⌘↑");
    expect(shortcutLabel("Mod+Shift+S/A/P")).toBe("⇧⌘S/A/P");
    expect(shortcutLabel("Ctrl+C")).toBe("⌃C");
  });
});
