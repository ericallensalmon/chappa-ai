// File-path detection + resolution. Compiler errors, test failures, `git
// status` and agent tool output name files constantly; this module finds the
// path under the cursor on one viewport row (hover-time, no persistent index,
// the same contract as the bare-URL autolink in links.ts) and resolves it
// against the base directory of the entry that printed it. Everything here
// is pure: existence checks and opening happen on click, outside this module.

/** A detected path on one row of text, `[colStart, colEnd]` inclusive. The
 *  location (`line`, `col`) is present only when the output carried one. */
export interface PathRun {
  colStart: number;
  colEnd: number;
  path: string;
  line?: number;
  col?: number;
}

/** One container bind mount: `destination` (in-container) is backed by
 *  `source` (on the host). */
export interface Mount {
  source: string;
  destination: string;
}

/** What a panel's relative paths are relative to, computed once per panel.
 *  `dir` null = unknown (relative paths stay as printed); `home` null = `~`
 *  is left unexpanded. `mounts` non-null marks output produced INSIDE a
 *  container: paths are container paths and only reach the host through a
 *  mount (an empty list = nothing is mounted, nothing is openable). */
export interface PathBase {
  dir: string | null;
  home: string | null;
  mounts?: Mount[] | null;
}

/** A resolved open target. `inContainer` = a container path no mount maps to
 *  the host: there is nothing to open, only the raw path to hand back. */
export interface ResolvedPath {
  path: string;
  line?: number;
  col?: number;
  inContainer?: boolean;
}

/** The entry kinds that print paths, each with what it knows about where it
 *  runs. Agent `mounts` non-null = a container runtime; `containerCwd` is the
 *  exec workdir inside it. */
export type PathBaseSource =
  | { kind: "projectCommand"; projectRoot: string | null; workingDir: string | null }
  | { kind: "projectShell"; projectRoot: string | null }
  | { kind: "workspaceCommand"; workingDir: string | null; spawnCwd: string | null }
  | { kind: "workspaceShell"; spawnCwd: string | null }
  | {
      kind: "agent";
      projectRoot: string | null;
      spawnCwd: string | null;
      mounts?: Mount[] | null;
      containerCwd?: string | null;
    };

// --- detection --------------------------------------------------------------

const DRIVE_RE = /^[A-Za-z]:[\\/]/;
/** Characters that never appear in a path a tool prints unquoted, but do
 *  appear in code, globs and prose around it. Rejecting them keeps `fn(a/b)`,
 *  `src/**` and `--out=dir/x` from reading as paths. */
const NOT_PATH_RE = /[(){}<>|*?"'`=,;]/;
/** A final `name.ext` segment: the evidence a separator-free file name needs
 *  (and only when a location follows it). */
const FILE_NAME_RE = /[A-Za-z0-9_-]\.[A-Za-z][A-Za-z0-9]*$/;
/** Prefixes that make an unlocated token a path on their own: a root, a
 *  drive, `./`, `../` or `~/` (either separator). */
const ANCHORED_RE = /^(?:[\\/]|[A-Za-z]:[\\/]|\.{1,2}[\\/]|~[\\/])/;
const LEAD_RE = /^[("'`<[{]+/;
const TRAIL_RE = /[.,;:'"`)\]}>]+$/;
/** `file:line[:col]`, then a delimiter: end of token, a `:` (gcc/grep -n
 *  glue the message on), or trailing punctuation. Lazy file part so a drive
 *  colon (`C:\x.rs:3`) is never mistaken for the location separator. */
const COLON_LOC_RE = /^(.+?):(\d+)(?::(\d+))?(?=$|:|[.,;'"`)\]}>]+$)/;
/** `file(line[,col])` — tsc and MSVC. */
const PAREN_LOC_RE = /^(.+?)\((\d+)(?:,(\d+))?\)(?=$|:|[.,;'"`\]}>]+$)/;
/** A Python traceback frame: the quoted path and `, line N` are one run. */
const PY_FRAME_RE = /File "([^"]+)", line (\d+)/g;

function isDrivePath(p: string): boolean {
  return DRIVE_RE.test(p);
}

/** Whether `p` reads as a path. With a location attached, a separator or a
 *  bare `name.ext` is enough (`app.ts(10,3)`). Without one, prose and import
 *  paths (`and/or`, `example.com/calc`) carry a single separator too, so an
 *  unlocated token needs an anchored prefix, two or more separators, or a
 *  final `name.ext` segment. A bare `foo.bar` never counts unlocated. */
function plausible(p: string, located: boolean): boolean {
  if (p.startsWith("\\\\") || p.startsWith("//")) return false; // UNC / comment noise
  if (p.includes("://")) return false; // a URL is the link detector's
  if (NOT_PATH_RE.test(p)) return false;
  // A colon past a drive prefix is a location or a `pytest ::node` id.
  if (p.slice(isDrivePath(p) ? 2 : 0).includes(":")) return false;
  // Times, ratios and dates (`12:30`, `3/4`, `11/13/2026`) have no letters.
  if (!/[A-Za-z]/.test(p)) return false;
  if (located) return /[\\/]/.test(p) || FILE_NAME_RE.test(p);
  if (ANCHORED_RE.test(p)) return true;
  const separators = p.match(/[\\/]/g)?.length ?? 0;
  return separators >= 2 || (separators === 1 && FILE_NAME_RE.test(p));
}

/** Every path run on one row of text, left to right. */
export function pathRuns(text: string): PathRun[] {
  const runs: PathRun[] = [];
  PY_FRAME_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = PY_FRAME_RE.exec(text)) !== null) {
    const path = m[1];
    if (/[<>]/.test(path) || !/[A-Za-z]/.test(path)) continue; // `File "<stdin>"`
    const colStart = m.index + "File ".length;
    runs.push({ colStart, colEnd: m.index + m[0].length - 1, path, line: Number(m[2]) });
  }
  const tokenRe = /\S+/g;
  while ((m = tokenRe.exec(text)) !== null) {
    const tokenStart = m.index;
    const tokenEnd = tokenStart + m[0].length - 1;
    if (runs.some((r) => r.colStart <= tokenEnd && r.colEnd >= tokenStart)) continue;
    const run = tokenRun(m[0]);
    if (run) runs.push({ ...run, colStart: tokenStart + run.colStart, colEnd: tokenStart + run.colEnd });
  }
  return runs.sort((a, b) => a.colStart - b.colStart);
}

/** The path run inside one whitespace-free token, columns token-relative. */
function tokenRun(token: string): PathRun | null {
  const lead = LEAD_RE.exec(token)?.[0].length ?? 0;
  const s = token.slice(lead);
  for (const re of [PAREN_LOC_RE, COLON_LOC_RE]) {
    const loc = re.exec(s);
    if (loc && plausible(loc[1], true)) {
      const run: PathRun = { colStart: lead, colEnd: lead + loc[0].length - 1, path: loc[1], line: Number(loc[2]) };
      if (loc[3] !== undefined) run.col = Number(loc[3]);
      return run;
    }
  }
  let bare = s.replace(TRAIL_RE, "");
  // `tests/test_app.py::test_add` names the file; the node id is not a path.
  const colon = bare.indexOf(":", isDrivePath(bare) ? 2 : 0);
  if (colon >= 0) bare = bare.slice(0, colon);
  if (!plausible(bare, false)) return null;
  return { colStart: lead, colEnd: lead + bare.length - 1, path: bare };
}

/** The path run containing text column `col`, if any. Callers check URLs
 *  first: a URL run wins where both could match. */
export function pathRunAt(text: string, col: number): PathRun | null {
  if (col < 0 || col >= text.length) return null;
  for (const run of pathRuns(text)) {
    if (col >= run.colStart && col <= run.colEnd) return run;
    if (run.colStart > col) break;
  }
  return null;
}

// --- resolution ---------------------------------------------------------------

function isAbsolute(p: string): boolean {
  return p.startsWith("/") || p.startsWith("\\") || isDrivePath(p);
}

function windowsStyle(p: string): boolean {
  return isDrivePath(p) || p.includes("\\");
}

/** Collapse `.`, `..` and repeated separators, keeping the root. A `..` that
 *  would climb past the root of an absolute path is dropped (the OS does the
 *  same); on a relative path it is kept. */
function normalize(p: string, win: boolean): string {
  const sep = win ? "\\" : "/";
  let root = "";
  let rest = p;
  if (isDrivePath(p)) {
    root = `${p[0].toUpperCase()}:${sep}`;
    rest = p.slice(3);
  } else if (p.startsWith("/") || p.startsWith("\\")) {
    root = sep;
    rest = p.slice(1);
  }
  const out: string[] = [];
  for (const seg of rest.split(/[\\/]+/)) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
      else if (root === "") out.push(seg);
      continue;
    }
    out.push(seg);
  }
  return root + out.join(sep);
}

/** `rel` joined onto `base` in `base`'s separator style. */
export function joinPath(base: string, rel: string): string {
  const win = windowsStyle(base);
  return normalize(`${base}${win ? "\\" : "/"}${rel}`, win);
}

/** A mount source as a host path. Docker Desktop on Windows reports a host
 *  bind either as a drive path or through its VM's view of the drive
 *  (`/run/desktop/mnt/host/c/...`, older builds `/host_mnt/c/...`); both
 *  become `C:\...`. */
export function hostMountSource(source: string): string {
  const vm = /^\/(?:run\/desktop\/mnt\/host|host_mnt)\/([A-Za-z])(?=\/|$)(.*)$/.exec(source);
  if (vm) return normalize(`${vm[1]}:/${vm[2]}`, true);
  if (isDrivePath(source)) return normalize(source, true);
  return source;
}

/** A container path mapped to the host through the longest matching mount
 *  destination; null when no mount covers it. */
export function mapThroughMounts(path: string, mounts: Mount[]): string | null {
  let best: Mount | null = null;
  for (const m of mounts) {
    const dest = m.destination.length > 1 ? m.destination.replace(/\/+$/, "") : m.destination;
    const covers = path === dest || path.startsWith(dest === "/" ? "/" : `${dest}/`);
    if (covers && (best === null || dest.length > best.destination.length)) best = { source: m.source, destination: dest };
  }
  if (!best) return null;
  const host = hostMountSource(best.source);
  const rest = path.slice(best.destination.length).replace(/^\/+/, "");
  return rest === "" ? host : joinPath(host, rest);
}

/** A detected run as an open target: `~/` expands to home, absolute paths
 *  stay absolute, relative ones join the base. Container output then maps
 *  through the mounts. With no base (or no home for `~`) the path is
 *  returned as printed — the opener's own resolution gets the last word. */
export function resolvePath(run: Pick<PathRun, "path" | "line" | "col">, base: PathBase | null): ResolvedPath {
  const loc: Omit<ResolvedPath, "path"> = {};
  if (run.line !== undefined) loc.line = run.line;
  if (run.col !== undefined) loc.col = run.col;
  let path = run.path;
  const tilde = /^~(?:[\\/]|$)/.test(path);
  if (tilde && base?.home) {
    path = joinPath(base.home, path.slice(2));
  } else if (isAbsolute(path)) {
    path = normalize(path, windowsStyle(path));
  } else if (!tilde && base?.dir) {
    path = joinPath(base.dir, path);
  }
  const mounts = base?.mounts;
  if (mounts) {
    const host = path.startsWith("/") ? mapThroughMounts(path, mounts) : null;
    if (host === null) return { path, ...loc, inContainer: true };
    path = host;
  }
  return { path, ...loc };
}

/** The base directory per entry kind. Project commands run in their
 *  `working_dir` (relative to the project root) else the root; project shells
 *  in the root; workspace commands in their `working_dir` else the spawn cwd
 *  (home when that is unset too); workspace shells in the spawn cwd else
 *  home; host agents in the project root when bound, else the spawn cwd.
 *  Container agents resolve inside the container (their exec workdir, no
 *  host home) and reach the host through the mounts. */
export function baseFor(source: PathBaseSource, home: string | null): PathBase {
  switch (source.kind) {
    case "projectCommand": {
      const { projectRoot, workingDir } = source;
      if (workingDir && isAbsolute(workingDir)) return { dir: workingDir, home };
      if (workingDir && projectRoot) return { dir: joinPath(projectRoot, workingDir), home };
      return { dir: projectRoot, home };
    }
    case "projectShell":
      return { dir: source.projectRoot, home };
    case "workspaceCommand":
      return { dir: source.workingDir ?? source.spawnCwd ?? home, home };
    case "workspaceShell":
      return { dir: source.spawnCwd ?? home, home };
    case "agent":
      if (source.mounts) return { dir: source.containerCwd ?? null, home: null, mounts: source.mounts };
      return { dir: source.projectRoot ?? source.spawnCwd ?? home, home };
  }
}

// --- text surfaces -------------------------------------------------------------

/** Class of the anchors `linkifyPaths` produces (styling + click delegation). */
export const PATH_LINK_CLASS = "chappa-path-link";
/** Dotted, not solid: a hint that the run is actionable without dressing
 *  agent prose up as a page of hyperlinks. */
const PATH_LINK_CSS = "color:inherit;text-decoration:underline dotted;text-underline-offset:2px;";

/** `text` as text nodes with every detected path wrapped in an anchor that
 *  carries its resolved target in `data-path` / `data-line` / `data-col`
 *  (`data-in-container` when no mount maps it). The anchors have no `href`:
 *  a plain click stays a text-selection click, and opening is the host's
 *  ctrl/cmd+click delegation. Never builds HTML from strings. */
export function linkifyPaths(text: string, base: PathBase | null): Node[] {
  const nodes: Node[] = [];
  let pending = "";
  const lines = text.split("\n");
  lines.forEach((line, i) => {
    let at = 0;
    for (const run of pathRuns(line)) {
      pending += line.slice(at, run.colStart);
      if (pending !== "") nodes.push(document.createTextNode(pending));
      pending = "";
      const target = resolvePath(run, base);
      const a = document.createElement("a");
      a.className = PATH_LINK_CLASS;
      a.style.cssText = PATH_LINK_CSS;
      a.textContent = line.slice(run.colStart, run.colEnd + 1);
      a.dataset.path = target.path;
      if (target.line !== undefined) a.dataset.line = String(target.line);
      if (target.col !== undefined) a.dataset.col = String(target.col);
      if (target.inContainer) a.dataset.inContainer = "true";
      a.title = target.inContainer ? `inside container: ${target.path}` : describeTarget(target);
      nodes.push(a);
      at = run.colEnd + 1;
    }
    pending += line.slice(at);
    if (i < lines.length - 1) pending += "\n";
  });
  if (pending !== "") nodes.push(document.createTextNode(pending));
  return nodes;
}

/** A target as one display string: `path:line:col` (location parts only
 *  when present) — what the hover bar and anchor titles show. */
export function describeTarget(target: ResolvedPath): string {
  let out = target.path;
  if (target.line !== undefined) out += `:${target.line}`;
  if (target.line !== undefined && target.col !== undefined) out += `:${target.col}`;
  return out;
}
