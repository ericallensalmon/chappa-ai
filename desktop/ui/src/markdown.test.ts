// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { firstLine, MARKDOWN_CLASS, renderMarkdown } from "./markdown";

/** The rendered tree as compact tag/text tuples, for exact assertions. */
function shape(node: Node): unknown {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent;
  const el = node as Element;
  const kids = [...el.childNodes].map(shape);
  return [el.tagName.toLowerCase(), ...kids];
}

function md(src: string): unknown[] {
  const root = renderMarkdown(src);
  expect(root.className).toBe(MARKDOWN_CLASS);
  return [...root.childNodes].map(shape);
}

describe("renderMarkdown blocks", () => {
  it("headings of every level; trailing hashes are a closing sequence only after a space", () => {
    expect(md("# One\n## Two ##\n###### Six\n#NoSpace\n# C#")).toEqual([
      ["h1", "One"],
      ["h2", "Two"],
      ["h6", "Six"],
      ["p", "#NoSpace"],
      ["h1", "C#"],
    ]);
    expect(md("####### seven")).toEqual([["p", "####### seven"]]);
  });

  it("paragraphs keep each source line and split on blank lines", () => {
    expect(md("first line\nsecond line\n\nnext para")).toEqual([
      ["p", "first line", ["br"], "second line"],
      ["p", "next para"],
    ]);
  });

  it("bulleted and numbered lists, nested by indentation, with continuation lines", () => {
    expect(md("- a\n- b\n  - b1\n    continued\n  - b2\n- c")).toEqual([
      ["ul", ["li", "a"], ["li", "b", ["ul", ["li", "b1", ["br"], "continued"], ["li", "b2"]]], ["li", "c"]],
    ]);
    expect(md("* x\n+ y")).toEqual([["ul", ["li", "x"], ["li", "y"]]]);
    const ol = renderMarkdown("3. three\n4) four").firstElementChild!;
    expect(shape(ol)).toEqual(["ol", ["li", "three"], ["li", "four"]]);
    expect(ol.getAttribute("start")).toBe("3");
    expect(renderMarkdown("1. one").firstElementChild!.hasAttribute("start")).toBe(false);
  });

  it("a blank line between items keeps one list; a blank line before prose ends it", () => {
    expect(md("- a\n\n- b\n\nafter")).toEqual([["ul", ["li", "a"], ["li", "b"]], ["p", "after"]]);
    expect(md("intro\n- item")).toEqual([["p", "intro"], ["ul", ["li", "item"]]]);
  });

  it("not lists: emphasis-looking starts and rules", () => {
    expect(md("**bold** start\n---\n-no space")).toEqual([
      ["p", ["strong", "bold"], " start", ["br"], "---", ["br"], "-no space"],
    ]);
  });

  it("fenced code keeps its text verbatim, markup and all, with the language as data", () => {
    const root = renderMarkdown("before\n```ts\nconst a = **1**;\n  <b>x</b>\n```\nafter");
    expect([...root.childNodes].map(shape)).toEqual([
      ["p", "before"],
      ["pre", ["code", "const a = **1**;\n  <b>x</b>"]],
      ["p", "after"],
    ]);
    expect(root.querySelector("code")!.dataset.lang).toBe("ts");
    // Tilde fences; a shorter closing run does not close; unclosed runs to the end.
    expect(md("~~~~\n~~~\nstill code\n~~~~")).toEqual([["pre", ["code", "~~~\nstill code"]]]);
    expect(md("```\nopen")).toEqual([["pre", ["code", "open"]]]);
    expect(md("```\n```")).toEqual([["pre", ["code"]]]);
  });
});

describe("renderMarkdown inline", () => {
  it("code spans of any backtick run, bold, and escapes", () => {
    expect(md("run `make test` and ``a ` b`` now")).toEqual([
      ["p", "run ", ["code", "make test"], " and ", ["code", "a ` b"], " now"],
    ]);
    expect(md("**strong** and __also__ and **`code` inside**")).toEqual([
      ["p", ["strong", "strong"], " and ", ["strong", "also"], " and ", ["strong", ["code", "code"], " inside"]],
    ]);
    expect(md("\\*\\*not bold\\*\\* and \\`not code\\`")).toEqual([["p", "**not bold** and `not code`"]]);
    expect(md("inside code `**x**` stays")).toEqual([["p", "inside code ", ["code", "**x**"], " stays"]]);
  });

  it("unmatched markers are literal text", () => {
    expect(md("a ` b")).toEqual([["p", "a ` b"]]);
    expect(md("2 ** 3 and ** x **")).toEqual([["p", "2 ** 3 and ** x **"]]);
    expect(md("**")).toEqual([["p", "**"]]);
    expect(md("snake_case_name and a_b")).toEqual([["p", "snake_case_name and a_b"]]);
  });
});

describe("renderMarkdown never interprets HTML", () => {
  it("tags, entities and attributes stay text in every block kind", () => {
    const src = [
      "<script>window.__md = 1</script>",
      "# <img src=x onerror=\"window.__md = 2\">",
      "- <a href=\"javascript:alert(1)\">link</a>",
      "**<b>bold</b>** `<i>code</i>` &amp; &lt;",
      "```<svg onload=alert(1)>",
      "<iframe src=x></iframe>",
      "```",
    ].join("\n");
    const root = renderMarkdown(src);
    document.body.appendChild(root);
    expect(root.querySelector("script, img, a, b, i, svg, iframe")).toBeNull();
    const tags = [...root.querySelectorAll("*")].map((e) => e.tagName.toLowerCase());
    expect(new Set(tags)).toEqual(new Set(["p", "h1", "ul", "li", "strong", "code", "pre"]));
    expect(root.textContent).toContain("<script>window.__md = 1</script>");
    expect(root.textContent).toContain("&amp; &lt;");
    expect(root.querySelector("pre code")!.textContent).toBe("<iframe src=x></iframe>");
    expect(root.querySelector("pre code")!.getAttribute("data-lang")).toBe("<svg");
    expect((window as unknown as { __md?: number }).__md).toBeUndefined();
    root.remove();
  });
});

describe("firstLine", () => {
  it("is the first non-blank line as plain text", () => {
    expect(firstLine("\n\n## Build **ok**  \nmore")).toBe("Build ok");
    expect(firstLine("- `x` done\n- y")).toBe("x done");
    expect(firstLine("```\ncode\n```")).toBe("code");
    expect(firstLine("\r\nline one\r\nline two")).toBe("line one");
    expect(firstLine("  \n\t")).toBe("");
    expect(firstLine("<b>raw</b>")).toBe("<b>raw</b>");
  });
});
