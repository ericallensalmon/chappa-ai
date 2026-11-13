// A small, safe markdown subset for text a process wrote about itself (run
// summaries today; any other process-authored prose later).
//
// Blocks: ATX headings (`#` … `######`), bulleted (`-` `*` `+`) and numbered
// (`1.` / `1)`) lists nested by indentation, fenced code (``` or ~~~, an
// optional language word), and paragraphs (each source line keeps its own
// line). Inline: code spans (`` `x` ``, any backtick run length), bold
// (`**x**` / `__x__`) and backslash escapes. Everything else — raw HTML,
// links, images, tables — stays literal text.
//
// The input is untrusted: it comes from whatever program ran in the
// terminal. So the renderer never parses HTML: every piece of source text
// reaches the DOM through `createTextNode` / `textContent`, and the only
// elements created are the fixed set below. There is no innerHTML anywhere
// in this module, and no attribute is ever taken from the input except a
// fence's language word, which goes into a `data-` attribute as plain text.

/** The class on the root a {@link renderMarkdown} call returns. */
export const MARKDOWN_CLASS = "chappa-md";

export const MARKDOWN_CSS = `
.chappa-md{font:13px system-ui,sans-serif;color:#d6d8dc;line-height:1.45;word-break:break-word;}
.chappa-md > :first-child{margin-top:0;}
.chappa-md > :last-child{margin-bottom:0;}
.chappa-md p{margin:0 0 8px;}
.chappa-md h1,.chappa-md h2,.chappa-md h3,.chappa-md h4,.chappa-md h5,.chappa-md h6{margin:10px 0 6px;font-weight:600;color:#e6e8eb;line-height:1.25;}
.chappa-md h1{font-size:16px;}
.chappa-md h2{font-size:15px;}
.chappa-md h3{font-size:14px;}
.chappa-md h4,.chappa-md h5,.chappa-md h6{font-size:13px;}
.chappa-md ul,.chappa-md ol{margin:0 0 8px;padding-left:20px;}
.chappa-md li > ul,.chappa-md li > ol{margin:2px 0 0;}
.chappa-md code{font:12px ui-monospace,monospace;background:#1a1d22;border:1px solid #23272e;border-radius:3px;padding:0 3px;}
.chappa-md pre{margin:0 0 8px;padding:6px 8px;background:#0f1114;border:1px solid #1c1f24;border-radius:5px;overflow:auto;}
.chappa-md pre code{background:none;border:0;padding:0;white-space:pre;color:#c9d1d9;}
.chappa-md strong{font-weight:600;color:#e6e8eb;}
`;

const FENCE = /^ {0,3}(`{3,}|~{3,})[ \t]*([^\s`]*)/;
const HEADING = /^ {0,3}(#{1,6})[ \t]+(.+?)(?:[ \t]+#+)?[ \t]*$/;
const LIST_ITEM = /^([ \t]*)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
const ESCAPABLE = /[!-/:-@[-`{-~]/;

/** Render `src` as a detached `div.chappa-md`. Pure apart from building
 *  DOM nodes; safe for any input (see the module comment). */
export function renderMarkdown(src: string): HTMLDivElement {
  const root = document.createElement("div");
  root.className = MARKDOWN_CLASS;
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      i++;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      i = renderFence(root, lines, i, fence[1], fence[2]);
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      const h = document.createElement(`h${heading[1].length}`);
      h.append(...inline(heading[2]));
      root.appendChild(h);
      i++;
      continue;
    }
    if (LIST_ITEM.test(line)) {
      i = renderList(root, lines, i);
      continue;
    }
    i = renderParagraph(root, lines, i);
  }
  return root;
}

/** The first line of a summary as plain text, block markers and inline
 *  markup removed — what a one-line strip shows. Empty for blank input. */
export function firstLine(src: string): string {
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  for (const line of lines) {
    if (line.trim() === "" || FENCE.test(line)) continue;
    const heading = HEADING.exec(line);
    const item = LIST_ITEM.exec(line);
    const text = heading ? heading[2] : item ? item[3] : line.trim();
    const span = document.createElement("span");
    span.append(...inline(text));
    const plain = (span.textContent ?? "").trim();
    if (plain) return plain;
  }
  return "";
}

/** A fenced block from line `start` to its closing fence (or the end of
 *  the text — an unclosed fence runs to the end, as CommonMark has it).
 *  Returns the index after the block. */
function renderFence(root: HTMLElement, lines: string[], start: number, marker: string, lang: string): number {
  const close = new RegExp(`^ {0,3}${marker[0] === "`" ? "`" : "~"}{${marker.length},}[ \\t]*$`);
  const body: string[] = [];
  let i = start + 1;
  while (i < lines.length && !close.test(lines[i])) {
    body.push(lines[i]);
    i++;
  }
  const pre = document.createElement("pre");
  const code = document.createElement("code");
  code.textContent = body.join("\n");
  if (lang) code.dataset.lang = lang;
  pre.appendChild(code);
  root.appendChild(pre);
  return Math.min(i + 1, lines.length);
}

interface ListLevel {
  indent: number;
  list: HTMLElement;
  lastItem: HTMLLIElement | null;
}

/** Consecutive list items (blank lines between items allowed) as nested
 *  lists: a deeper-indented item opens a list inside the previous item, a
 *  shallower one closes back out. A non-item line indented under an item
 *  continues that item's text on a new line. Returns the index after. */
function renderList(root: HTMLElement, lines: string[], start: number): number {
  const stack: ListLevel[] = [];
  let i = start;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      // A blank line ends the list unless another item follows it.
      let j = i + 1;
      while (j < lines.length && lines[j].trim() === "") j++;
      if (j < lines.length && LIST_ITEM.test(lines[j])) {
        i = j;
        continue;
      }
      break;
    }
    const item = LIST_ITEM.exec(line);
    if (!item) {
      const last = stack.length > 0 ? stack[stack.length - 1].lastItem : null;
      if (last && /^[ \t]{2,}/.test(line) && !FENCE.test(line) && !HEADING.test(line)) {
        last.append(document.createElement("br"), ...inline(line.trim()));
        i++;
        continue;
      }
      break;
    }
    const indent = widthOf(item[1]);
    const ordered = /\d/.test(item[2]);
    while (stack.length > 1 && indent < stack[stack.length - 1].indent) stack.pop();
    const top = stack[stack.length - 1];
    if (!top || (indent > top.indent && top.lastItem)) {
      const list = document.createElement(ordered ? "ol" : "ul");
      if (ordered) {
        const n = parseInt(item[2], 10);
        if (n !== 1) list.setAttribute("start", String(n));
      }
      (top?.lastItem ?? root).appendChild(list);
      stack.push({ indent, list, lastItem: null });
    }
    const level = stack[stack.length - 1];
    const li = document.createElement("li");
    li.append(...inline(item[3]));
    level.list.appendChild(li);
    level.lastItem = li;
    i++;
  }
  return i;
}

/** Lines up to the next blank line or block start, one line each. */
function renderParagraph(root: HTMLElement, lines: string[], start: number): number {
  const p = document.createElement("p");
  let i = start;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") break;
    if (i > start && (FENCE.test(line) || HEADING.test(line) || LIST_ITEM.test(line))) break;
    if (i > start) p.appendChild(document.createElement("br"));
    p.append(...inline(line.trim()));
    i++;
  }
  root.appendChild(p);
  return i;
}

/** Leading whitespace width, a tab counting as four columns. */
function widthOf(ws: string): number {
  let n = 0;
  for (const ch of ws) n += ch === "\t" ? 4 : 1;
  return n;
}

/** Inline markup: code spans, bold, backslash escapes; text otherwise. */
function inline(text: string): Node[] {
  const out: Node[] = [];
  let buf = "";
  const flush = (): void => {
    if (buf) out.push(document.createTextNode(buf));
    buf = "";
  };
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === "\\" && i + 1 < text.length && ESCAPABLE.test(text[i + 1])) {
      buf += text[i + 1];
      i += 2;
      continue;
    }
    if (ch === "`") {
      let n = 1;
      while (text[i + n] === "`") n++;
      const run = "`".repeat(n);
      const close = findRun(text, run, i + n);
      if (close < 0) {
        buf += run;
        i += n;
        continue;
      }
      flush();
      let body = text.slice(i + n, close);
      if (body.length > 1 && body.startsWith(" ") && body.endsWith(" ") && body.trim() !== "") body = body.slice(1, -1);
      const code = document.createElement("code");
      code.textContent = body;
      out.push(code);
      i = close + n;
      continue;
    }
    if ((ch === "*" || ch === "_") && text[i + 1] === ch) {
      const delim = ch + ch;
      const close = text.indexOf(delim, i + 2);
      const body = close > i + 2 ? text.slice(i + 2, close) : "";
      if (body.trim() === "" || /^\s/.test(body) || /\s$/.test(body)) {
        buf += delim;
        i += 2;
        continue;
      }
      flush();
      const strong = document.createElement("strong");
      strong.append(...inline(body));
      out.push(strong);
      i = close + 2;
      continue;
    }
    buf += ch;
    i++;
  }
  flush();
  return out;
}

/** The index of a backtick run of exactly `run.length` at or after `from`,
 *  or -1. A longer run does not close a shorter one. */
function findRun(text: string, run: string, from: number): number {
  let i = text.indexOf(run, from);
  while (i >= 0) {
    let end = i + run.length;
    const exact = text[i - 1] !== "`" && text[end] !== "`";
    if (exact) return i;
    while (text[end] === "`") end++;
    i = text.indexOf(run, end);
  }
  return -1;
}
