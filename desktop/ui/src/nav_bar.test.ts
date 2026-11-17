// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { setPlatform } from "./platform";
import { isAppChord } from "./term/input";
import {
  ActivityBar,
  NAV_TABS,
  navTabFor,
  readNavProject,
  readNavTab,
  readViewSource,
  writeNavProject,
  writeNavTab,
  writeViewSource,
  type NavTab,
  type NavTabState,
} from "./nav_bar";

const states = (over: Partial<Record<NavTab, Partial<NavTabState>>> = {}): Record<NavTab, NavTabState> => {
  const out = {} as Record<NavTab, NavTabState>;
  for (const t of NAV_TABS) out[t] = { visible: true, title: t, ...over[t] };
  return out;
};

const key = (init: KeyboardEventInit): KeyboardEvent => new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });

describe("ActivityBar", () => {
  it("lists the tabs in bar order with an icon and a short label, and the footer last", () => {
    const bar = new ActivityBar(() => {});
    const tabs = [...bar.el.querySelectorAll<HTMLButtonElement>(".chappa-nav-tab")];
    expect(tabs.map((t) => t.dataset.tab)).toEqual(["work", "attention", "prs", "environment", "projects"]);
    expect(tabs.map((t) => t.querySelector(".chappa-nav-label")?.textContent)).toEqual(["Work", "Needs you", "PRs", "Environment", "Projects"]);
    expect(tabs.every((t) => t.querySelector("svg") !== null)).toBe(true);
    expect(tabs.every((t) => t.getAttribute("role") === "tab")).toBe(true);
    expect(bar.el.lastElementChild).toBe(bar.footer);
  });

  it("marks the selected tab, keeps one tab stop, and appends the shortcut to the tooltip", () => {
    const bar = new ActivityBar(() => {});
    bar.update("prs", states({ prs: { title: "PRs from fx" } }));
    expect(bar.current).toBe("prs");
    expect(bar.button("prs").classList.contains("selected")).toBe(true);
    expect(bar.button("prs").getAttribute("aria-selected")).toBe("true");
    expect(bar.button("work").getAttribute("aria-selected")).toBe("false");
    expect(NAV_TABS.map((t) => bar.button(t).tabIndex)).toEqual([-1, -1, 0, -1, -1]);
    expect(bar.button("prs").title).toBe("PRs from fx (Ctrl+Shift+3)");
  });

  it("shows a count (capped at 99+), a dot by level, the quieter idle look, and hides a tab that is not visible", () => {
    const bar = new ActivityBar(() => {});
    bar.update(
      "projects",
      states({
        attention: { count: 7 },
        prs: { count: 140 },
        environment: { dot: "bad" },
        work: { idle: true },
        projects: { visible: true },
      }),
    );
    const badge = (t: NavTab): string => bar.button(t).querySelector(".chappa-nav-badge")?.textContent ?? "";
    const dot = (t: NavTab): string | undefined => bar.button(t).querySelector<HTMLElement>(".chappa-nav-dot")?.dataset.level;
    expect(badge("attention")).toBe("7");
    expect(badge("prs")).toBe("99+");
    expect(badge("environment")).toBe("");
    expect(dot("environment")).toBe("bad");
    expect(bar.button("attention").getAttribute("aria-label")).toBe("Needs you, 7");
    expect(bar.button("environment").getAttribute("aria-label")).toBe("Environment, alert");
    expect(bar.button("work").classList.contains("idle")).toBe(true);
    bar.update("projects", states({ attention: { count: 0 }, environment: { dot: "warn" }, prs: { visible: false } }));
    expect(badge("attention")).toBe("");
    expect(dot("environment")).toBe("warn");
    expect(bar.button("prs").hidden).toBe(true);
    expect(bar.button("work").classList.contains("idle")).toBe(false);
    bar.update("projects", states());
    expect(dot("environment")).toBeUndefined();
  });

  it("a click selects; arrows, Home and End walk the visible tabs without selecting", () => {
    const onSelect = vi.fn();
    const bar = new ActivityBar(onSelect);
    document.body.appendChild(bar.el);
    bar.update("work", states({ prs: { visible: false } }));
    bar.button("environment").click();
    expect(onSelect).toHaveBeenCalledWith("environment");
    onSelect.mockClear();
    bar.button("work").focus();
    bar.button("work").dispatchEvent(key({ key: "ArrowDown" }));
    expect(document.activeElement).toBe(bar.button("attention"));
    // PRs is hidden: the walk skips it.
    bar.button("attention").dispatchEvent(key({ key: "ArrowDown" }));
    expect(document.activeElement).toBe(bar.button("environment"));
    bar.button("environment").dispatchEvent(key({ key: "End" }));
    expect(document.activeElement).toBe(bar.button("projects"));
    bar.button("projects").dispatchEvent(key({ key: "ArrowDown" }));
    expect(document.activeElement).toBe(bar.button("work"));
    bar.button("work").dispatchEvent(key({ key: "ArrowUp" }));
    expect(document.activeElement).toBe(bar.button("projects"));
    bar.button("projects").dispatchEvent(key({ key: "Home" }));
    expect(document.activeElement).toBe(bar.button("work"));
    expect(onSelect).not.toHaveBeenCalled();
    bar.el.remove();
  });

  it("with the selected tab hidden, the first visible tab keeps the tab stop", () => {
    const bar = new ActivityBar(() => {});
    bar.update("attention", states({ attention: { visible: false } }));
    expect(bar.button("work").tabIndex).toBe(0);
  });
});

describe("navTabFor", () => {
  afterEach(() => setPlatform(null));

  it("maps Ctrl+Shift+1…5 to the tabs in bar order by the physical digit, and leaves plain Ctrl+digit to the terminal", () => {
    setPlatform("other");
    // With Shift held the key is the shifted character; the digit is in `code`.
    expect(navTabFor(key({ key: "!", code: "Digit1", ctrlKey: true, shiftKey: true }))).toBe("work");
    expect(navTabFor(key({ key: "@", code: "Digit2", ctrlKey: true, shiftKey: true }))).toBe("attention");
    expect(navTabFor(key({ key: "3", ctrlKey: true, shiftKey: true }))).toBe("prs");
    expect(navTabFor(key({ key: "$", code: "Digit4", ctrlKey: true, shiftKey: true }))).toBe("environment");
    expect(navTabFor(key({ key: "%", code: "Digit5", ctrlKey: true, shiftKey: true }))).toBe("projects");
    expect(navTabFor(key({ key: "^", code: "Digit6", ctrlKey: true, shiftKey: true }))).toBeNull();
    expect(navTabFor(key({ key: "1", code: "Digit1", ctrlKey: true }))).toBeNull();
    expect(navTabFor(key({ key: "1", code: "Digit1" }))).toBeNull();
    expect(navTabFor(key({ key: "!", code: "Digit1", ctrlKey: true, shiftKey: true, altKey: true }))).toBeNull();
    expect(navTabFor(key({ key: "1", code: "Digit1", metaKey: true }))).toBeNull();
    expect(isAppChord(key({ key: "1", code: "Digit1", ctrlKey: true }))).toBe(false);
    expect(isAppChord(key({ key: "!", code: "Digit1", ctrlKey: true, shiftKey: true }))).toBe(true);
  });

  it("uses Cmd+digit on macOS, leaving Ctrl+digit to the terminal", () => {
    setPlatform("mac");
    expect(navTabFor(key({ key: "2", code: "Digit2", metaKey: true }))).toBe("attention");
    expect(navTabFor(key({ key: "@", code: "Digit2", metaKey: true, shiftKey: true }))).toBeNull();
    expect(navTabFor(key({ key: "2", code: "Digit2", ctrlKey: true }))).toBeNull();
    const bar = new ActivityBar(() => {});
    bar.update("projects", states({ work: { title: "Work" } }));
    expect(bar.button("work").title).toBe("Work (⌘1)");
  });
});

describe("remembered navigation", () => {
  it("round-trips the tab, the project and a view's source, and ignores what it cannot read", () => {
    expect(readNavTab()).toBeNull();
    writeNavTab("environment");
    expect(readNavTab()).toBe("environment");
    window.localStorage.setItem("chappa-ai.nav.tab", "nonsense");
    expect(readNavTab()).toBeNull();

    expect(readNavProject()).toBeNull();
    writeNavProject(12);
    expect(readNavProject()).toBe(12);
    window.localStorage.setItem("chappa-ai.nav.project", "-1");
    expect(readNavProject()).toBeNull();
    writeNavProject(null);
    expect(window.localStorage.getItem("chappa-ai.nav.project")).toBeNull();

    expect(readViewSource("prs")).toBeNull();
    writeViewSource("prs", "fx");
    expect(readViewSource("prs")).toBe("fx");
    writeViewSource("prs", null);
    expect(readViewSource("prs")).toBeNull();
  });

  it("a storage that throws leaves every choice at its default", () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    const set = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(() => writeNavTab("prs")).not.toThrow();
    expect(readNavTab()).toBeNull();
    expect(readNavProject()).toBeNull();
    spy.mockRestore();
    set.mockRestore();
  });
});
