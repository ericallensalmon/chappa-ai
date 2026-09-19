// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";
import {
  makeCollapsibleSection,
  readCollapsed,
  sectionIcon,
  writeCollapsed,
} from "./sidebar_sections";

/** Build a header shaped like the real ones: label, count, and an action
 *  button, so the click-guard is exercised against a real control. */
function makeHeader(): { header: HTMLElement; button: HTMLButtonElement } {
  const header = document.createElement("div");
  header.className = "chappa-subsection-header";
  const label = document.createElement("span");
  label.className = "chappa-subsection-label";
  label.textContent = "TERMINALS";
  const count = document.createElement("span");
  count.textContent = "2";
  const button = document.createElement("button");
  button.textContent = "＋";
  header.append(label, count, button);
  return { header, button };
}

describe("sectionIcon", () => {
  it("builds an inline SVG that inherits color and has drawable paths", () => {
    const svg = sectionIcon("terminal");
    expect(svg.tagName.toLowerCase()).toBe("svg");
    expect(svg.getAttribute("aria-hidden")).toBe("true");
    const paths = svg.querySelectorAll("path");
    expect(paths.length).toBeGreaterThan(0);
    for (const p of paths) {
      expect(p.getAttribute("stroke")).toBe("currentColor");
      expect(p.getAttribute("d")).toBeTruthy();
    }
  });

  it("covers every section name", () => {
    for (const name of ["terminal", "agents", "commands", "scratchpads", "active"] as const) {
      expect(sectionIcon(name).querySelectorAll("path").length).toBeGreaterThan(0);
    }
  });
});

describe("collapse persistence", () => {
  beforeEach(() => localStorage.clear());

  it("defaults to expanded and stores only the collapsed case", () => {
    expect(readCollapsed("x")).toBe(false);
    writeCollapsed("x", true);
    expect(readCollapsed("x")).toBe(true);
    // Re-expanding removes the key rather than storing "0".
    writeCollapsed("x", false);
    expect(readCollapsed("x")).toBe(false);
    expect(localStorage.getItem("chappa-ai.section-collapsed.x")).toBeNull();
  });
});

describe("makeCollapsibleSection", () => {
  beforeEach(() => localStorage.clear());

  it("prepends a chevron and icon before the existing header content", () => {
    const { header } = makeHeader();
    makeCollapsibleSection({
      header,
      body: document.createElement("div"),
      storageKey: "s",
      icon: "terminal",
    });
    expect(header.children[0].classList.contains("chappa-sub-chevron")).toBe(true);
    expect(header.children[1].classList.contains("chappa-sub-icon")).toBe(true);
    // The original label survives after the two prepended nodes.
    expect(header.children[2].textContent).toBe("TERMINALS");
    expect(header.classList.contains("chappa-sub-toggle")).toBe(true);
  });

  it("folds the body on a header click and remembers it", () => {
    const { header } = makeHeader();
    const body = document.createElement("div");
    const handle = makeCollapsibleSection({ header, body, storageKey: "s", icon: "terminal" });

    expect(body.hidden).toBe(false);
    header.click();
    expect(body.hidden).toBe(true);
    expect(handle.isCollapsed()).toBe(true);
    expect(readCollapsed("s")).toBe(true);

    header.click();
    expect(body.hidden).toBe(false);
    expect(readCollapsed("s")).toBe(false);
  });

  it("restores a remembered collapsed state on build", () => {
    writeCollapsed("s", true);
    const { header } = makeHeader();
    const body = document.createElement("div");
    makeCollapsibleSection({ header, body, storageKey: "s", icon: "agents" });
    expect(body.hidden).toBe(true);
    expect(header.getAttribute("aria-expanded")).toBe("false");
  });

  it("does NOT fold when an action button in the header is clicked", () => {
    const { header, button } = makeHeader();
    const body = document.createElement("div");
    let clicks = 0;
    button.addEventListener("click", () => (clicks += 1));
    makeCollapsibleSection({ header, body, storageKey: "s", icon: "commands" });

    button.click();
    expect(clicks).toBe(1);
    expect(body.hidden).toBe(false); // the add button did its job, no fold

    // The chevron itself still folds.
    (header.querySelector(".chappa-sub-chevron") as HTMLElement).click();
    expect(body.hidden).toBe(true);
  });

  it("setCollapsed toggles programmatically and is idempotent", () => {
    const { header } = makeHeader();
    const body = document.createElement("div");
    const handle = makeCollapsibleSection({ header, body, storageKey: "s", icon: "scratchpads" });
    handle.setCollapsed(true);
    expect(body.hidden).toBe(true);
    handle.setCollapsed(true); // no-op
    expect(body.hidden).toBe(true);
    handle.setCollapsed(false);
    expect(body.hidden).toBe(false);
  });
});
