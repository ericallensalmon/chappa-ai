// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { TerminalPanel, type OpenPathTarget } from "./panel";
import {
  baseFor,
  describeTarget,
  hostMountSource,
  joinPath,
  linkifyPaths,
  mapThroughMounts,
  pathRunAt,
  pathRuns,
  resolvePath,
  PATH_LINK_CLASS,
  type PathBase,
  type PathRun,
} from "./paths";
import { frame, mockHostRect, paint, stubApi, type CellSpec } from "./test-utils";

// --- detection --------------------------------------------------------------

/** Hover `needle` (its first character) on `row` and expect `want`: the run's
 *  text as printed plus the parsed path and location. */
interface DetectRow {
  name: string;
  row: string;
  needle: string;
  want: { text: string; path: string; line?: number; col?: number } | null;
}

const DETECT: DetectRow[] = [
  // cargo / rustc
  {
    name: "rustc arrow line",
    row: "  --> src/main.rs:12:5",
    needle: "src/",
    want: { text: "src/main.rs:12:5", path: "src/main.rs", line: 12, col: 5 },
  },
  {
    name: "cargo test panic location with trailing colon",
    row: "thread 'tests::adds' panicked at src/lib.rs:10:9:",
    needle: "lib",
    want: { text: "src/lib.rs:10:9", path: "src/lib.rs", line: 10, col: 9 },
  },
  {
    name: "rustc on Windows keeps the drive colon",
    row: "  --> C:\\proj\\src\\main.rs:12:5",
    needle: "proj",
    want: { text: "C:\\proj\\src\\main.rs:12:5", path: "C:\\proj\\src\\main.rs", line: 12, col: 5 },
  },
  // tsc
  {
    name: "tsc plain output, paren location",
    row: "src/app.ts(10,3): error TS2322: Type 'string' is not assignable to type 'number'.",
    needle: "app",
    want: { text: "src/app.ts(10,3)", path: "src/app.ts", line: 10, col: 3 },
  },
  {
    name: "tsc paren location on a bare file name",
    row: "app.ts(10,3): error TS2304: Cannot find name 'x'.",
    needle: "app",
    want: { text: "app.ts(10,3)", path: "app.ts", line: 10, col: 3 },
  },
  {
    name: "tsc pretty output, colon location",
    row: "src/app.ts:10:3 - error TS2322: Type 'string' is not assignable.",
    needle: "src",
    want: { text: "src/app.ts:10:3", path: "src/app.ts", line: 10, col: 3 },
  },
  {
    name: "MSVC paren line only",
    row: "C:\\proj\\main.cpp(12): error C2065: 'x': undeclared identifier",
    needle: "main",
    want: { text: "C:\\proj\\main.cpp(12)", path: "C:\\proj\\main.cpp", line: 12 },
  },
  // pytest + python
  {
    name: "python traceback frame is one run, quotes excluded from the path",
    row: '  File "/home/dev/proj/app/calc.py", line 12, in add',
    needle: "line 12",
    want: { text: '"/home/dev/proj/app/calc.py", line 12', path: "/home/dev/proj/app/calc.py", line: 12 },
  },
  {
    name: "python traceback with a relative file name",
    row: '  File "calc.py", line 3, in <module>',
    needle: "calc",
    want: { text: '"calc.py", line 3', path: "calc.py", line: 3 },
  },
  {
    name: "pytest short location",
    row: "tests/test_calc.py:14: AssertionError",
    needle: "test_calc",
    want: { text: "tests/test_calc.py:14", path: "tests/test_calc.py", line: 14 },
  },
  {
    name: "pytest node id names only the file",
    row: "FAILED tests/test_calc.py::test_add - assert 3 == 4",
    needle: "tests",
    want: { text: "tests/test_calc.py", path: "tests/test_calc.py" },
  },
  // eslint
  {
    name: "eslint stylish file header is a bare absolute path",
    row: "/home/dev/proj/src/index.js",
    needle: "index",
    want: { text: "/home/dev/proj/src/index.js", path: "/home/dev/proj/src/index.js" },
  },
  {
    name: "eslint unix format",
    row: "/home/dev/proj/src/index.js:12:5: 'x' is defined but never used. [Error/no-unused-vars]",
    needle: "proj",
    want: { text: "/home/dev/proj/src/index.js:12:5", path: "/home/dev/proj/src/index.js", line: 12, col: 5 },
  },
  // go
  {
    name: "go test failure on a bare file name",
    row: "    calc_test.go:15: got 4, want 3",
    needle: "calc",
    want: { text: "calc_test.go:15", path: "calc_test.go", line: 15 },
  },
  {
    name: "go build dot-slash path",
    row: "./main.go:12:5: undefined: foo",
    needle: "main",
    want: { text: "./main.go:12:5", path: "./main.go", line: 12, col: 5 },
  },
  // grep -n glues the match text on
  {
    name: "grep -n line",
    row: "src/app.ts:42:const x = 1;",
    needle: "src",
    want: { text: "src/app.ts:42", path: "src/app.ts", line: 42 },
  },
  // git status
  {
    name: "git status modified path",
    row: "\tmodified:   desktop/ui/src/app.ts",
    needle: "desktop",
    want: { text: "desktop/ui/src/app.ts", path: "desktop/ui/src/app.ts" },
  },
  {
    name: "git status untracked nested directory",
    row: "\tdocs/api/",
    needle: "docs",
    want: { text: "docs/api/", path: "docs/api/" },
  },
  {
    // One separator and no extension reads like prose; a top-level
    // untracked directory is the cost of not underlining `and/or`.
    name: "git status untracked top-level directory is not a path",
    row: "\tdocs/",
    needle: "docs",
    want: null,
  },
  {
    name: "git status rename target",
    row: "\trenamed:    old/name.ts -> src/name.ts",
    needle: "src",
    want: { text: "src/name.ts", path: "src/name.ts" },
  },
  // ls -l
  {
    name: "ls -l symlink target",
    row: "lrwxrwxrwx 1 dev dev   19 Nov 13 09:52 current -> releases/v2/app.bin",
    needle: "releases",
    want: { text: "releases/v2/app.bin", path: "releases/v2/app.bin" },
  },
  {
    name: "ls -l plain file name is not a path",
    row: "-rw-r--r-- 1 dev dev 1234 Nov 13 09:52 Cargo.toml",
    needle: "Cargo",
    want: null,
  },
  {
    name: "ls -l time column is not a location",
    row: "-rw-r--r-- 1 dev dev 1234 Nov 13 09:52 Cargo.toml",
    needle: "09:52",
    want: null,
  },
  // anchored prefixes, quoting, punctuation
  {
    name: "home-relative path",
    row: "wrote ~/notes/today.md",
    needle: "~",
    want: { text: "~/notes/today.md", path: "~/notes/today.md" },
  },
  {
    name: "parent-relative path",
    row: "see ../shared/config.toml for details",
    needle: "shared",
    want: { text: "../shared/config.toml", path: "../shared/config.toml" },
  },
  {
    name: "forward-slash drive path",
    row: "opened C:/proj/README.md",
    needle: "proj",
    want: { text: "C:/proj/README.md", path: "C:/proj/README.md" },
  },
  {
    name: "backslash path without a drive",
    row: "compiling src\\main.rs",
    needle: "main",
    want: { text: "src\\main.rs", path: "src\\main.rs" },
  },
  {
    name: "backticks and sentence punctuation are stripped",
    row: "Edited `src/term/paths.ts`.",
    needle: "term",
    want: { text: "src/term/paths.ts", path: "src/term/paths.ts" },
  },
  {
    name: "single quotes and a closing paren are stripped",
    row: "(cannot open 'src/lib.rs')",
    needle: "lib",
    want: { text: "src/lib.rs", path: "src/lib.rs" },
  },
  {
    name: "trailing comma after a location",
    row: "at src/app.ts:7:2, then",
    needle: "app",
    want: { text: "src/app.ts:7:2", path: "src/app.ts", line: 7, col: 2 },
  },
  // negatives
  {
    name: "a URL is not a path",
    row: "docs at https://example.com/guide/start.html now",
    needle: "guide",
    want: null,
  },
  { name: "a time is not a location", row: "started 12:30:45 done", needle: "30", want: null },
  { name: "a ratio is not a path", row: "passed 3/4 checks", needle: "3/4", want: null },
  { name: "a bare word with an extension is not a path", row: "see foo.bar here", needle: "foo", want: null },
  { name: "a UNC path is ignored", row: "share \\\\server\\share\\x.txt", needle: "server", want: null },
  { name: "a glob is not a path", row: "matched src/**/*.ts", needle: "src", want: null },
  { name: "a call expression is not a path", row: "called fn(a/b) twice", needle: "a/b", want: null },
  { name: "a python stdin frame has no file", row: '  File "<stdin>", line 1, in <module>', needle: "stdin", want: null },
  { name: "whitespace is not a path", row: "src/a.rs   src/b.rs", needle: "  ", want: null },
  // An unlocated token with one separator needs a `name.ext` final segment.
  { name: "prose with a slash is not a path", row: "pass a file and/or a dir", needle: "and/or", want: null },
  { name: "a go import path is not a path", row: "ok  \texample.com/calc\t0.002s", needle: "example", want: null },
  { name: "a word pair is not a path", row: "the input/output layer", needle: "input", want: null },
  { name: "one separator without an extension is not a path", row: "see src/foo here", needle: "src", want: null },
  // Unlocated positives: an extension, an anchored prefix, or two separators.
  { name: "one separator with an extension", row: "edited src/main.rs", needle: "src", want: { text: "src/main.rs", path: "src/main.rs" } },
  { name: "dot-slash directory", row: "wrote ./build", needle: "./", want: { text: "./build", path: "./build" } },
  { name: "dot-dot directory", row: "linked ../lib", needle: "../", want: { text: "../lib", path: "../lib" } },
  {
    name: "two separators without an extension",
    row: "generated docs/api/index",
    needle: "docs",
    want: { text: "docs/api/index", path: "docs/api/index" },
  },
  { name: "a bare drive directory", row: "cd C:\\proj", needle: "C:", want: { text: "C:\\proj", path: "C:\\proj" } },
  {
    name: "a home-relative dotfile directory",
    row: "config in ~/.config/app",
    needle: "~",
    want: { text: "~/.config/app", path: "~/.config/app" },
  },
  { name: "backslash with an extension", row: "see docs\\api.md", needle: "docs", want: { text: "docs\\api.md", path: "docs\\api.md" } },
  // Known false positive, kept on purpose: a located `name.ext:N` is how tsc
  // and go name a file without a directory, so `host.tld:port` reads as one.
  // The click-time existence check turns it into a harmless "not found".
  {
    name: "a host:port reads as a located file name",
    row: "listening on example.com:443",
    needle: "example",
    want: { text: "example.com:443", path: "example.com", line: 443 },
  },
];

describe("pathRunAt", () => {
  for (const row of DETECT) {
    it(row.name, () => {
      const col = row.row.indexOf(row.needle);
      expect(col).toBeGreaterThanOrEqual(0);
      const run = pathRunAt(row.row, col);
      if (row.want === null) {
        expect(run).toBeNull();
        return;
      }
      expect(run).not.toBeNull();
      const r = run as PathRun;
      expect(row.row.slice(r.colStart, r.colEnd + 1)).toBe(row.want.text);
      expect(r.path).toBe(row.want.path);
      expect(r.line).toBe(row.want.line);
      expect(r.col).toBe(row.want.col);
    });
  }

  it("covers the run's first and last columns, not one past either edge", () => {
    const text = "  --> src/main.rs:12:5 next";
    const start = text.indexOf("src");
    const end = start + "src/main.rs:12:5".length - 1;
    expect(pathRunAt(text, start)?.colStart).toBe(start);
    expect(pathRunAt(text, end)?.colEnd).toBe(end);
    expect(pathRunAt(text, start - 1)).toBeNull();
    expect(pathRunAt(text, end + 1)).toBeNull();
  });

  it("returns null for columns outside the text", () => {
    expect(pathRunAt("src/a.rs", -1)).toBeNull();
    expect(pathRunAt("src/a.rs", 8)).toBeNull();
  });

  it("finds every run on a row, left to right", () => {
    expect(pathRuns("src/a.rs:1 and lib/b.rs(2,3)").map((r) => r.path)).toEqual(["src/a.rs", "lib/b.rs"]);
  });
});

// --- resolution ---------------------------------------------------------------

const POSIX: PathBase = { dir: "/home/dev/proj", home: "/home/dev" };
const WIN: PathBase = { dir: "C:\\proj", home: "C:\\code\\dev" };

describe("resolvePath", () => {
  const rows: Array<[string, string, PathBase | null, string]> = [
    ["relative joins the base", "src/main.rs", POSIX, "/home/dev/proj/src/main.rs"],
    ["dot-slash joins the base", "./main.go", POSIX, "/home/dev/proj/main.go"],
    ["dot-dot climbs from the base", "../shared/x.toml", POSIX, "/home/dev/shared/x.toml"],
    ["absolute stays absolute", "/etc/hosts.d/a.conf", POSIX, "/etc/hosts.d/a.conf"],
    ["tilde expands to home", "~/notes/today.md", POSIX, "/home/dev/notes/today.md"],
    ["Windows relative joins with backslashes", "src/app.ts", WIN, "C:\\proj\\src\\app.ts"],
    ["Windows dot-dot climbs", "..\\other\\x.rs", WIN, "C:\\other\\x.rs"],
    ["Windows drive path stays absolute", "D:\\data\\x.csv", WIN, "D:\\data\\x.csv"],
    ["forward-slash drive path normalizes", "c:/proj/README.md", WIN, "C:\\proj\\README.md"],
    ["Windows tilde expands to home", "~\\x.txt", WIN, "C:\\code\\dev\\x.txt"],
    ["dot-dot never climbs past the root", "../../../x", { dir: "/a", home: null }, "/x"],
    ["no base leaves a relative path as printed", "src/main.rs", null, "src/main.rs"],
    ["no home leaves tilde as printed", "~/x.md", { dir: "/home/dev/proj", home: null }, "~/x.md"],
  ];
  for (const [name, path, base, want] of rows) {
    it(name, () => {
      expect(resolvePath({ path }, base).path).toBe(want);
    });
  }

  it("carries the location through and omits it when absent", () => {
    expect(resolvePath({ path: "src/a.rs", line: 12, col: 5 }, POSIX)).toEqual({
      path: "/home/dev/proj/src/a.rs",
      line: 12,
      col: 5,
    });
    expect(resolvePath({ path: "src/a.rs" }, POSIX)).toEqual({ path: "/home/dev/proj/src/a.rs" });
  });

  it("joinPath follows the base's separator style", () => {
    expect(joinPath("/home/dev", "a\\b")).toBe("/home/dev/a/b");
    expect(joinPath("C:\\proj", "a/b")).toBe("C:\\proj\\a\\b");
  });
});

describe("container mounts", () => {
  const mounts = [
    { source: "/home/dev/proj", destination: "/workspace/proj" },
    { source: "/home/dev/proj/vendor-cache", destination: "/workspace/proj/vendor" },
    { source: "/home/dev/data", destination: "/data/" },
  ];

  it("maps through the longest matching destination", () => {
    expect(mapThroughMounts("/workspace/proj/src/a.rs", mounts)).toBe("/home/dev/proj/src/a.rs");
    expect(mapThroughMounts("/workspace/proj/vendor/lib.rs", mounts)).toBe("/home/dev/proj/vendor-cache/lib.rs");
  });

  it("matches whole segments only, and the destination itself", () => {
    expect(mapThroughMounts("/workspace/project/a.rs", mounts)).toBeNull();
    expect(mapThroughMounts("/workspace/proj", mounts)).toBe("/home/dev/proj");
    expect(mapThroughMounts("/data/x.csv", mounts)).toBe("/home/dev/data/x.csv");
  });

  it("returns null when no mount covers the path", () => {
    expect(mapThroughMounts("/usr/lib/x.so", mounts)).toBeNull();
  });

  it("normalizes both Docker Desktop source forms to a drive path", () => {
    expect(hostMountSource("/run/desktop/mnt/host/c/code/dev/proj")).toBe("C:\\code\\dev\\proj");
    expect(hostMountSource("/host_mnt/c/code/dev/proj")).toBe("C:\\code\\dev\\proj");
    expect(hostMountSource("C:\\code\\dev\\proj")).toBe("C:\\code\\dev\\proj");
    expect(hostMountSource("/home/dev/proj")).toBe("/home/dev/proj");
    const win = [{ source: "/run/desktop/mnt/host/c/proj", destination: "/workspace/proj" }];
    expect(mapThroughMounts("/workspace/proj/desktop/ui/src/app.ts", win)).toBe("C:\\proj\\desktop\\ui\\src\\app.ts");
  });

  it("resolvePath maps container paths and joins relative ones onto the workdir first", () => {
    const base: PathBase = { dir: "/workspace/proj", home: null, mounts };
    expect(resolvePath({ path: "/workspace/proj/src/a.rs", line: 3 }, base)).toEqual({
      path: "/home/dev/proj/src/a.rs",
      line: 3,
    });
    expect(resolvePath({ path: "src/a.rs" }, base).path).toBe("/home/dev/proj/src/a.rs");
  });

  it("an unmapped container path is flagged and kept as printed", () => {
    const base: PathBase = { dir: "/workspace/proj", home: null, mounts };
    expect(resolvePath({ path: "/usr/lib/x.so" }, base)).toEqual({ path: "/usr/lib/x.so", inContainer: true });
    expect(resolvePath({ path: "src/a.rs" }, { dir: null, home: null, mounts: [] })).toEqual({
      path: "src/a.rs",
      inContainer: true,
    });
  });
});

describe("baseFor", () => {
  const home = "/home/dev";
  it("project command: relative working_dir under the root, absolute as-is, else the root", () => {
    expect(baseFor({ kind: "projectCommand", projectRoot: "/p", workingDir: "desktop" }, home).dir).toBe("/p/desktop");
    expect(baseFor({ kind: "projectCommand", projectRoot: "/p", workingDir: "/opt/x" }, home).dir).toBe("/opt/x");
    expect(baseFor({ kind: "projectCommand", projectRoot: "C:\\p", workingDir: "desktop/ui" }, home).dir).toBe(
      "C:\\p\\desktop\\ui",
    );
    expect(baseFor({ kind: "projectCommand", projectRoot: "/p", workingDir: null }, home).dir).toBe("/p");
  });

  it("project shell: the root", () => {
    expect(baseFor({ kind: "projectShell", projectRoot: "/p" }, home)).toEqual({ dir: "/p", home });
  });

  it("workspace command: working_dir, else spawn cwd, else home", () => {
    expect(baseFor({ kind: "workspaceCommand", workingDir: "/w", spawnCwd: "/s" }, home).dir).toBe("/w");
    expect(baseFor({ kind: "workspaceCommand", workingDir: null, spawnCwd: "/s" }, home).dir).toBe("/s");
    expect(baseFor({ kind: "workspaceCommand", workingDir: null, spawnCwd: null }, home).dir).toBe(home);
  });

  it("workspace shell: spawn cwd, else home", () => {
    expect(baseFor({ kind: "workspaceShell", spawnCwd: "/s" }, home).dir).toBe("/s");
    expect(baseFor({ kind: "workspaceShell", spawnCwd: null }, home).dir).toBe(home);
    expect(baseFor({ kind: "workspaceShell", spawnCwd: null }, null).dir).toBeNull();
  });

  it("host agent: project root when bound, else spawn cwd", () => {
    expect(baseFor({ kind: "agent", projectRoot: "/p", spawnCwd: "/s" }, home).dir).toBe("/p");
    expect(baseFor({ kind: "agent", projectRoot: null, spawnCwd: "/s" }, home).dir).toBe("/s");
  });

  it("container agent: the exec workdir, no host home, the mounts", () => {
    const mounts = [{ source: "/home/dev/proj", destination: "/workspace/proj" }];
    expect(
      baseFor({ kind: "agent", projectRoot: "/p", spawnCwd: "/s", mounts, containerCwd: "/workspace/proj" }, home),
    ).toEqual({ dir: "/workspace/proj", home: null, mounts });
  });
});

// --- text surfaces --------------------------------------------------------------

describe("linkifyPaths", () => {
  it("wraps each path in an anchor carrying the resolved target; the rest stays text", () => {
    const nodes = linkifyPaths("error at src/a.rs:3:1\nplain line\n  --> lib/b.rs:9", POSIX);
    const div = document.createElement("div");
    div.append(...nodes);
    expect(div.textContent).toBe("error at src/a.rs:3:1\nplain line\n  --> lib/b.rs:9");
    expect(div.innerHTML).not.toContain("href");
    const anchors = [...div.querySelectorAll<HTMLAnchorElement>(`a.${PATH_LINK_CLASS}`)];
    expect(anchors.map((a) => a.textContent)).toEqual(["src/a.rs:3:1", "lib/b.rs:9"]);
    expect({ ...anchors[0].dataset }).toEqual({ path: "/home/dev/proj/src/a.rs", line: "3", col: "1" });
    expect({ ...anchors[1].dataset }).toEqual({ path: "/home/dev/proj/lib/b.rs", line: "9" });
    expect(anchors[0].title).toBe("/home/dev/proj/src/a.rs:3:1");
  });

  it("text with no path is a single text node", () => {
    const nodes = linkifyPaths("all 12 tests passed in 0.3s", POSIX);
    expect(nodes).toHaveLength(1);
    expect(nodes[0].nodeType).toBe(Node.TEXT_NODE);
  });

  it("markup-looking text stays text", () => {
    const div = document.createElement("div");
    div.append(...linkifyPaths("<b>src/x.ts</b>", POSIX));
    expect(div.querySelector("b")).toBeNull();
    expect(div.textContent).toBe("<b>src/x.ts</b>");
  });

  it("an unmapped container path is marked", () => {
    const div = document.createElement("div");
    div.append(...linkifyPaths("/usr/lib/x.so", { dir: null, home: null, mounts: [] }));
    expect(div.querySelector<HTMLElement>(`a.${PATH_LINK_CLASS}`)?.dataset.inContainer).toBe("true");
  });

  it("describeTarget prints a column only alongside a line", () => {
    expect(describeTarget({ path: "/a", line: 2, col: 3 })).toBe("/a:2:3");
    expect(describeTarget({ path: "/a", line: 2 })).toBe("/a:2");
    expect(describeTarget({ path: "/a" })).toBe("/a");
  });
});

// --- the panel journey ------------------------------------------------------

const CELL_W = 8; // jsdom probe fallback at fontSize 14
const CELL_H = 16.8; // line-height model: fontSize x 1.2

function makePanel(over: Partial<ConstructorParameters<typeof TerminalPanel>[0]> = {}) {
  const { api, onFrame } = stubApi();
  const container = document.createElement("div");
  const opened: string[] = [];
  const paths: OpenPathTarget[] = [];
  const panel = new TerminalPanel({
    container,
    api,
    platform: "Linux",
    pathBase: POSIX,
    openUrl: (url) => {
      opened.push(url);
      return Promise.resolve();
    },
    openPath: (target) => {
      paths.push(target);
    },
    ...over,
  });
  mockHostRect(container, 800, 400);
  return { api, onFrame, container, panel, opened, paths };
}

const FLUSH_PROMISE = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

const ctrlDown = (col: number, row: number): MouseEvent =>
  new MouseEvent("mousedown", { clientX: col * CELL_W + 4, clientY: row * CELL_H + 8, ctrlKey: true, bubbles: true });

async function show(rows: Record<number, string | CellSpec[]>, over: Parameters<typeof makePanel>[0] = {}) {
  const made = makePanel(over);
  await made.panel.start();
  made.onFrame()!(frame({ cols: 60, rows: 5, seq: 1, content: rows }));
  await paint();
  const viewport = made.container.querySelector(".chappa-term-viewport") as HTMLElement;
  return { ...made, viewport };
}

describe("panel path links", () => {
  it("ctrl+click on a path run opens the resolved target at its location, no terminal write", async () => {
    const { api, panel, viewport, paths, opened } = await show({ 1: "  --> src/main.rs:12:5" });
    viewport.dispatchEvent(ctrlDown(8, 1));
    await FLUSH_PROMISE();
    expect(paths).toEqual([{ path: "/home/dev/proj/src/main.rs", line: 12, col: 5 }]);
    expect(opened).toEqual([]);
    expect(api.mouse).not.toHaveBeenCalledWith(42, expect.objectContaining({ kind: "press" }));
    panel.dispose();
  });

  it("a path with no location opens without one", async () => {
    const { panel, viewport, paths } = await show({ 0: "\tmodified:   desktop/ui/src/app.ts" });
    viewport.dispatchEvent(ctrlDown(20, 0));
    await FLUSH_PROMISE();
    expect(paths).toEqual([{ path: "/home/dev/proj/desktop/ui/src/app.ts" }]);
    panel.dispose();
  });

  it("a URL run wins over the path detector", async () => {
    const { panel, viewport, paths, opened } = await show({ 2: "see https://example.com/src/a.rs now" });
    viewport.dispatchEvent(ctrlDown(25, 2));
    await FLUSH_PROMISE();
    expect(opened).toEqual(["https://example.com/src/a.rs"]);
    expect(paths).toEqual([]);
    panel.dispose();
  });

  it("a plain click on a path reaches the terminal", async () => {
    const { api, panel, viewport, paths } = await show({ 1: "  --> src/main.rs:12:5" });
    viewport.dispatchEvent(
      new MouseEvent("mousedown", { clientX: 8 * CELL_W + 4, clientY: 1 * CELL_H + 8, bubbles: true }),
    );
    await FLUSH_PROMISE();
    expect(paths).toEqual([]);
    expect(api.mouse).toHaveBeenCalledWith(42, expect.objectContaining({ kind: "press", row: 1, col: 8 }));
    panel.dispose();
  });

  it("hover underlines the run and the bar shows the resolved target", async () => {
    const { container, panel, viewport } = await show({ 1: "  --> src/main.rs:12:5" });
    viewport.dispatchEvent(new MouseEvent("mousemove", { clientX: 8 * CELL_W + 4, clientY: 1 * CELL_H + 8, bubbles: true }));
    await FLUSH_PROMISE();
    const underline = container.querySelector<HTMLDivElement>(".chappa-link-underline")!;
    const bar = container.querySelector<HTMLDivElement>(".chappa-link-bar")!;
    expect(underline.style.display).toBe("block");
    expect(underline.style.left).toBe(`${6 * CELL_W}px`);
    expect(underline.style.width).toBe(`${"src/main.rs:12:5".length * CELL_W}px`);
    expect(bar.textContent).toBe("/home/dev/proj/src/main.rs:12:5");
    panel.dispose();
  });

  it("without an opener the click flashes the resolved target", async () => {
    const { container, panel, viewport } = await show({ 1: "  --> src/main.rs:12:5" }, { openPath: undefined });
    viewport.dispatchEvent(ctrlDown(8, 1));
    await FLUSH_PROMISE();
    const flash = container.querySelector<HTMLDivElement>(".chappa-link-flash")!;
    expect(flash.style.display).toBe("block");
    expect(flash.textContent).toBe("no file opener: /home/dev/proj/src/main.rs:12:5");
    panel.dispose();
  });

  it("an unmapped container path flashes instead of opening", async () => {
    const { container, panel, viewport, paths } = await show(
      { 0: "/usr/lib/libx.so: cannot open" },
      { pathBase: { dir: "/workspace/proj", home: null, mounts: [] } },
    );
    viewport.dispatchEvent(ctrlDown(3, 0));
    await FLUSH_PROMISE();
    expect(paths).toEqual([]);
    expect(container.querySelector<HTMLDivElement>(".chappa-link-flash")!.textContent).toBe(
      "inside container: /usr/lib/libx.so",
    );
    panel.dispose();
  });
});

describe("click-time opening hooks", () => {
  it("a mount with no host side keeps its paths inside the container", () => {
    const mounts = [
      { source: "/home/dev/proj", destination: "/workspace/proj" },
      { source: "", destination: "/workspace/proj/node_modules" },
    ];
    expect(mapThroughMounts("/workspace/proj/node_modules/x/index.js", mounts)).toBeNull();
    expect(mapThroughMounts("/workspace/proj/src/a.ts", mounts)).toBe("/home/dev/proj/src/a.ts");
  });

  it("the opener's returned message is flashed on the panel", async () => {
    const { container, panel, viewport } = await show(
      { 1: "  --> src/gone.rs:3:1" },
      { openPath: () => Promise.resolve("not found: /home/dev/proj/src/gone.rs") },
    );
    viewport.dispatchEvent(ctrlDown(8, 1));
    await FLUSH_PROMISE();
    const flash = container.querySelector<HTMLDivElement>(".chappa-link-flash")!;
    expect(flash.style.display).toBe("block");
    expect(flash.textContent).toBe("not found: /home/dev/proj/src/gone.rs");
    panel.dispose();
  });

  it("a rejected open flashes its reason, a successful one flashes nothing", async () => {
    const failing = await show({ 1: "  --> src/main.rs:12:5" }, { openPath: () => Promise.reject("cannot start code") });
    failing.viewport.dispatchEvent(ctrlDown(8, 1));
    await FLUSH_PROMISE();
    expect(failing.container.querySelector<HTMLDivElement>(".chappa-link-flash")!.textContent).toBe("cannot start code");
    failing.panel.dispose();

    const fine = await show({ 1: "  --> src/main.rs:12:5" }, { openPath: () => Promise.resolve(null) });
    fine.viewport.dispatchEvent(ctrlDown(8, 1));
    await FLUSH_PROMISE();
    expect(fine.container.querySelector<HTMLDivElement>(".chappa-link-flash")!.style.display).not.toBe("block");
    fine.panel.dispose();
  });

  it("setPathBase switches a container panel from unmapped to mapped paths", async () => {
    const { container, panel, viewport, paths } = await show(
      { 0: "/workspace/proj/src/a.rs:4 error" },
      { pathBase: { dir: "/workspace/proj", home: null, mounts: [] } },
    );
    viewport.dispatchEvent(ctrlDown(3, 0));
    await FLUSH_PROMISE();
    expect(paths).toEqual([]);
    expect(container.querySelector<HTMLDivElement>(".chappa-link-flash")!.textContent).toBe(
      "inside container: /workspace/proj/src/a.rs",
    );
    panel.setPathBase({
      dir: "/workspace/proj",
      home: null,
      mounts: [{ source: "/run/desktop/mnt/host/c/code/proj", destination: "/workspace/proj" }],
    });
    viewport.dispatchEvent(ctrlDown(3, 0));
    await FLUSH_PROMISE();
    expect(paths).toEqual([{ path: "C:\\code\\proj\\src\\a.rs", line: 4 }]);
    panel.dispose();
  });
});
