// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import type { CellStore } from "./protocol";
import { TerminalPanel } from "./panel";
import { ALLOWED_SCHEMES, LinkIndex, autolinkRun, linkAt, linkRunAt, rowText, schemeAllowed } from "./links";
import { frame, mockHostRect, paint, stubApi, type CellSpec } from "./test-utils";

// --- pure model -------------------------------------------------------------

describe("schemeAllowed", () => {
  it("opens http/https/mailto only", () => {
    expect(ALLOWED_SCHEMES).toEqual(["http:", "https:", "mailto:"]);
    expect(schemeAllowed("https://example.com/a?b=1")).toBe(true);
    expect(schemeAllowed("http://x")).toBe(true);
    expect(schemeAllowed("mailto:a@b.c")).toBe(true);
    expect(schemeAllowed("file:///c:/x.txt")).toBe(false);
    expect(schemeAllowed("ftp://x")).toBe(false);
    expect(schemeAllowed("javascript:alert(1)")).toBe(false);
    expect(schemeAllowed("ssh://x")).toBe(false);
  });
});

function store(cols: number, rows: number, rowSpecs: Record<number, CellSpec[] | string>): CellStore {
  const ch = new Uint32Array(cols * rows).fill(0x20);
  const fg = new Uint32Array(cols * rows).fill(0xd8d8d8ff);
  const bg = new Uint32Array(cols * rows).fill(0x181818ff);
  const flags = new Uint16Array(cols * rows);
  const link = new Uint16Array(cols * rows);
  for (const [rowStr, rowSpec] of Object.entries(rowSpecs)) {
    const row = Number(rowStr);
    const cells = typeof rowSpec === "string" ? Array.from(rowSpec) : rowSpec;
    for (let c = 0; c < cols; c++) {
      const s = cells[c] ?? "";
      const idx = row * cols + c;
      if (typeof s === "string") {
        ch[idx] = s.codePointAt(0) ?? 0x20;
      } else {
        ch[idx] = s.ch?.codePointAt(0) ?? 0x20;
        link[idx] = s.link ?? 0;
      }
    }
  }
  return { cols, rows, ch, fg, bg, flags, link };
}

describe("linkRunAt", () => {
  it("finds the contiguous same-id run containing the cell", () => {
    const s = store(6, 1, { 0: ["a", "b", "c", "d", "e", "f"] });
    for (let c = 0; c < 6; c++) s.link[c] = c < 4 ? 7 : 0;
    expect(linkRunAt(s, 0, 1)).toEqual({ colStart: 0, colEnd: 3, linkId: 7 });
    expect(linkRunAt(s, 0, 3)).toEqual({ colStart: 0, colEnd: 3, linkId: 7 });
    expect(linkRunAt(s, 0, 4)).toBeNull(); // id 0: not a link cell
    expect(linkRunAt(s, 0, 9)).toBeNull(); // out of range
  });

  it("stops the run at a neighbouring different id", () => {
    const s = store(6, 1, { 0: ["a", "b", "c", "d", "e", "f"] });
    s.link[1] = 1;
    s.link[2] = 1;
    s.link[3] = 2;
    expect(linkRunAt(s, 0, 2)).toEqual({ colStart: 1, colEnd: 2, linkId: 1 });
    expect(linkRunAt(s, 0, 3)).toEqual({ colStart: 3, colEnd: 3, linkId: 2 });
  });
});

describe("rowText", () => {
  it("flattens a row, reading blank cells as spaces", () => {
    const s = store(5, 1, { 0: ["h", "i", { ch: "" }] });
    s.ch[0] = 0x68;
    s.ch[1] = 0x69;
    s.ch[2] = 0; // blank
    s.ch[3] = 0x21;
    // Cell 4 keeps the store's default space — rows are full-width.
    expect(rowText(s, 0)).toBe("hi ! ");
  });
});

describe("autolinkRun", () => {
  it("detects a bare http URL containing the column", () => {
    const text = "see https://example.com/a?b=1 now";
    expect(autolinkRun(text, 6)).toEqual({
      colStart: 4,
      colEnd: 28,
      url: "https://example.com/a?b=1",
    });
  });

  it("trims trailing sentence punctuation from the URL", () => {
    const text = "visit https://x.test/y. now";
    expect(autolinkRun(text, 7)?.url).toBe("https://x.test/y");
  });

  it("returns null outside any URL", () => {
    const text = "x https://a.test y"; // URL spans cols 2..15
    expect(autolinkRun(text, 0)).toBeNull(); // the leading 'x'
    expect(autolinkRun(text, 17)).toBeNull(); // the trailing 'y'
    expect(autolinkRun(text, 2)?.url).toBe("https://a.test");
    expect(autolinkRun(text, 15)?.url).toBe("https://a.test");
  });

  it("matches only http(s) — mailto/file need OSC 8", () => {
    const text = "mailto:a@b.c";
    expect(autolinkRun(text, 1)).toBeNull();
  });
});

describe("linkAt", () => {
  it("prefers the OSC 8 id run over an autolink in the same row", () => {
    const s = store(24, 1, { 0: "https://example.com" });
    for (let c = 0; c < 18; c++) s.link[c] = 5;
    const index = new LinkIndex();
    index.merge([{ id: 5, uri: "https://osc8.example" }]);
    const run = linkAt(s, index, 0, 3);
    expect(run).toEqual({
      row: 0,
      colStart: 0,
      colEnd: 17,
      url: "https://osc8.example",
    });
  });

  it("falls back to the autolink when the id is unmapped", () => {
    const s = store(24, 1, { 0: "https://example.com" });
    const index = new LinkIndex(); // no ids merged
    expect(linkAt(s, index, 0, 6)?.url).toBe("https://example.com");
  });
});

// --- the panel journey ------------------------------------------------------

const CELL_W = 8; // jsdom probe fallback at fontSize 14
const CELL_H = 16.8; // line-height model: fontSize × 1.2

function makePanel(over: Partial<ConstructorParameters<typeof TerminalPanel>[0]> = {}) {
  const { api, onFrame } = stubApi();
  const container = document.createElement("div");
  const opened: string[] = [];
  const panel = new TerminalPanel({
    container,
    api,
    platform: "Linux",
    openUrl: (url) => {
      opened.push(url);
      return Promise.resolve();
    },
    ...over,
  });
  mockHostRect(container, 800, 400);
  return { api, onFrame, container, panel, opened };
}

/** A row of `text` where columns `[start, end)` carry OSC 8 link `id`. */
function linkedRow(text: string, id: number, start: number, end: number): CellSpec[] {
  const cells: CellSpec[] = Array.from(text).map((ch, i) =>
    i >= start && i < end ? { ch, link: id } : ch,
  );
  return cells;
}

const mo = (clientX: number, clientY: number): MouseEvent =>
  new MouseEvent("mousemove", { clientX, clientY, bubbles: true });

describe("panel link hover", () => {
  it("hovers an OSC 8 cell → underline run + URL bar", async () => {
    const { api, onFrame, container, panel } = makePanel();
    await panel.start();
    onFrame()!(
      frame({ cols: 30, rows: 5, seq: 1, content: { 2: linkedRow("open https://osc8.test/x now", 1, 5, 23) } }),
    );
    await paint();
    panel.handleLinks([{ id: 1, uri: "https://osc8.test/x" }]);

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(mo(6 * CELL_W + 4, 2 * CELL_H + 8));
    await FLUSH_PROMISE();

    const underline = container.querySelector<HTMLDivElement>(".chappa-link-underline")!;
    const bar = container.querySelector<HTMLDivElement>(".chappa-link-bar")!;
    expect(underline.style.display).toBe("block");
    // The run spans cells 5..22 (inclusive) — width 18 cells at 8px.
    expect(underline.style.left).toBe(`${5 * CELL_W}px`);
    expect(underline.style.width).toBe(`${18 * CELL_W}px`);
    expect(underline.style.top).toBe(`${(2 + 1) * CELL_H - 2}px`);
    expect(bar.style.display).toBe("block");
    expect(bar.textContent).toBe("https://osc8.test/x");
    expect(api.mouse).toHaveBeenCalledWith(
      42,
      expect.objectContaining({ kind: "move", row: 2, col: 6 }),
    );
    panel.dispose();
  });

  it("no link → no underline/bar", async () => {
    const { onFrame, container, panel } = makePanel();
    await panel.start();
    onFrame()!(frame({ cols: 30, rows: 5, seq: 1, content: { 2: "plain text row here" } }));
    await paint();
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(mo(3 * CELL_W, 2 * CELL_H));
    await FLUSH_PROMISE();
    const underline = container.querySelector<HTMLDivElement>(".chappa-link-underline")!;
    expect(underline.style.display).toBe("none");
    panel.dispose();
  });

  it("autolinks a bare http:// run at hover time", async () => {
    const { onFrame, container, panel } = makePanel();
    await panel.start();
    onFrame()!(frame({ cols: 30, rows: 5, seq: 1, content: { 1: "got https://auto.example/x here" } }));
    await paint();
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(mo(7 * CELL_W, 1 * CELL_H + 8));
    await FLUSH_PROMISE();
    const bar = container.querySelector<HTMLDivElement>(".chappa-link-bar")!;
    expect(bar.style.display).toBe("block");
    expect(bar.textContent).toBe("https://auto.example/x");
    panel.dispose();
  });

  it("hover after a scroll reads the shifted rows", async () => {
    // Two frames: first has a link at row 2, a scrolled frame puts different
    // content there. Hovering the same cell position shows the new link.
    const { onFrame, container, panel } = makePanel();
    await panel.start();
    onFrame()!(
      frame({ cols: 30, rows: 5, seq: 1, displayOffset: 0, historyLen: 10, content: { 2: linkedRow("first row link here", 1, 0, 10) } }),
    );
    await paint();
    panel.handleLinks([{ id: 1, uri: "https://first.test" }]);
    onFrame()!(
      frame({
        cols: 30,
        rows: 5,
        seq: 2,
        kind: 0,
        displayOffset: 4,
        historyLen: 10,
        content: { 2: linkedRow("second row link ok", 1, 0, 9) },
      }),
    );
    await paint();

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(mo(2 * CELL_W, 2 * CELL_H + 8));
    await FLUSH_PROMISE();
    const bar = container.querySelector<HTMLDivElement>(".chappa-link-bar")!;
    expect(bar.style.display).toBe("block");
    expect(bar.textContent).toBe("https://first.test");
    panel.dispose();
  });
});

describe("panel link click", () => {
  it("ctrl+click on an http link → exactly one openUrl, no terminal write", async () => {
    const { api, onFrame, container, panel, opened } = makePanel();
    await panel.start();
    onFrame()!(
      frame({ cols: 30, rows: 5, seq: 1, content: { 2: linkedRow("open https://osc8.test/x now", 1, 5, 23) } }),
    );
    await paint();
    panel.handleLinks([{ id: 1, uri: "https://osc8.test/x" }]);

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 6 * CELL_W + 4, clientY: 2 * CELL_H + 8, ctrlKey: true, bubbles: true }),
    );
    await FLUSH_PROMISE();

    expect(opened).toEqual(["https://osc8.test/x"]);
    // The consumed click never reaches the terminal, and never starts a
    // selection (ctrl+click is the link modifier, not a select gesture).
    expect(api.mouse).not.toHaveBeenCalledWith(42, expect.objectContaining({ kind: "press" }));
    expect(api.selection).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("ctrl+click on a file: URL → zero openUrl calls + status flash", async () => {
    const { api, onFrame, container, panel, opened } = makePanel();
    await panel.start();
    onFrame()!(
      frame({ cols: 30, rows: 5, seq: 1, content: { 1: linkedRow("local file:///c:/x.txt path", 1, 6, 20) } }),
    );
    await paint();
    panel.handleLinks([{ id: 1, uri: "file:///c:/x.txt" }]);

    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 7 * CELL_W + 4, clientY: 1 * CELL_H + 8, ctrlKey: true, bubbles: true }),
    );
    await FLUSH_PROMISE();

    expect(opened).toEqual([]);
    const flash = container.querySelector<HTMLDivElement>(".chappa-link-flash")!;
    expect(flash.style.display).toBe("block");
    expect(flash.textContent).toBe("not opening file:///c:/x.txt");
    expect(api.mouse).not.toHaveBeenCalledWith(42, expect.objectContaining({ kind: "press" }));
    panel.dispose();
  });

  it("a plain ctrl+click (no link) reaches the terminal", async () => {
    const { api, onFrame, container, panel, opened } = makePanel();
    await panel.start();
    onFrame()!(frame({ cols: 30, rows: 5, seq: 1, content: { 0: "no links anywhere" } }));
    await paint();
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 2 * CELL_W, clientY: 4, ctrlKey: true, bubbles: true }),
    );
    await FLUSH_PROMISE();
    expect(opened).toEqual([]);
    expect(api.mouse).toHaveBeenCalledWith(42, expect.objectContaining({ kind: "press", col: 2, row: 0 }));
    panel.dispose();
  });

  it("a ctrl+click on an OSC 8 id with an unmapped URL autolinks instead", async () => {
    const { onFrame, container, panel, opened } = makePanel();
    await panel.start();
    // Cell carries id 9 but no handleLinks ever maps it; the row's bare URL
    // is an autolink — ctrl+click opens that.
    onFrame()!(
      frame({ cols: 30, rows: 5, seq: 1, content: { 2: linkedRow("open https://auto.test/x now", 9, 5, 23) } }),
    );
    await paint();
    const viewport = container.querySelector(".chappa-term-viewport") as HTMLElement;
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 6 * CELL_W + 4, clientY: 2 * CELL_H + 8, ctrlKey: true, bubbles: true }),
    );
    await FLUSH_PROMISE();
    expect(opened).toEqual(["https://auto.test/x"]);
    panel.dispose();
  });
});

const FLUSH_PROMISE = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
