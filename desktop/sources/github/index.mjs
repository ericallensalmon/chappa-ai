#!/usr/bin/env node
// A chappa source for GitHub: pull requests (the PRs view) and the open
// issues assigned to me (the Work view). It asks the `gh` CLI for the
// data (`gh api graphql`), so neither the app nor this process ever holds
// a GitHub token: `gh` keeps its own login.
//
//   CHAPPA_SOURCE_ADDR=127.0.0.1:5055 CHAPPA_SOURCE_TOKEN=<64 hex> \
//     node desktop/sources/github/index.mjs
//
// chappa sets both variables when it launches a source. Optional:
//   GH_BIN                          the gh executable, an absolute path
//                                   (default: gh from PATH); a .js/.mjs/.cjs
//                                   path runs under this Node
//   CHAPPA_GITHUB_REFRESH_SECONDS   minimum seconds between GitHub fetches
//                                   (default 60), however often the app polls
// No packages; one line per request on stderr.
import { createServer } from "node:http";
import { createHash, timingSafeEqual } from "node:crypto";
import { execFile } from "node:child_process";
import { accessSync, constants, statSync } from "node:fs";
import { delimiter, dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { isValidChallenge, proof } from "../lib/identity.mjs";
import { ISSUES_SEARCH, SEARCH_ISSUES, SEARCH_PRS, VIEWER, searches } from "./queries.mjs";
import { RECENT_DAYS, toPr } from "./buckets.mjs";
import { toWorkItem } from "./issues.mjs";

const VERSION = "0.1.0";
const CAP_PER_ROLE = 100;
const GH_TIMEOUT_MS = 60_000;

const addr = process.env.CHAPPA_SOURCE_ADDR;
const token = process.env.CHAPPA_SOURCE_TOKEN;
// Read once, then gone from the environment every child process inherits
// (`gh` and anything else this source starts never sees the token).
delete process.env.CHAPPA_SOURCE_ADDR;
delete process.env.CHAPPA_SOURCE_TOKEN;
// gh runs from this source's own folder and is found by an absolute path,
// never by its bare name: the app starts a source in the project folder,
// and on Windows a bare name is looked up in the working directory before
// PATH, so a `gh.exe` a cloned repository carries would run with the
// person's GitHub login.
const HERE = dirname(fileURLToPath(import.meta.url));
const minRefreshMs = 1000 * Number(process.env.CHAPPA_GITHUB_REFRESH_SECONDS ?? 60);

if (!addr || !/^[0-9a-f]{64}$/.test(token ?? "")) {
  console.error("CHAPPA_SOURCE_ADDR and a 64-hex-char CHAPPA_SOURCE_TOKEN are required");
  process.exit(2);
}
const m = /^(\[[0-9a-fA-F:]+\]|[^:]+):(\d{1,5})$/.exec(addr);
const host = m && m[1].replace(/^\[|\]$/g, "");
if (!m || !["127.0.0.1", "::1", "localhost"].includes(host)) {
  console.error(`refusing to bind ${addr}: a source listens on loopback only`);
  process.exit(2);
}

const tokenDigest = createHash("sha256").update(token).digest();
function authorized(req) {
  const given = /^Bearer (.*)$/.exec(req.headers.authorization ?? "")?.[1] ?? "";
  // Compare fixed-length digests so neither length nor content leaks by timing.
  return timingSafeEqual(createHash("sha256").update(given).digest(), tokenDigest);
}

const MANIFEST = {
  contract: 1,
  source: { id: "github", label: "GitHub", version: VERSION },
  views: {
    work: { list: "/api/work", pollSeconds: 300 },
    prs: { list: "/api/prs", pollSeconds: 300 },
  },
  actions: "/api/actions",
  linkHosts: ["github.com"],
  roots: [],
};

// ---- gh ----------------------------------------------------------------

function runnable(file) {
  try {
    if (!statSync(file).isFile()) return false;
    if (process.platform !== "win32") accessSync(file, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * `name` on PATH, as an absolute path, or null. Only absolute PATH entries
 * are searched: an empty or relative entry means "the working directory",
 * which is exactly where a planted program would sit. On Windows the
 * PATHEXT extensions a program can be started with directly (.COM, .EXE)
 * are tried, in PATHEXT's order.
 */
function findOnPath(name, env = process.env, platform = process.platform) {
  let exts = [""];
  if (platform === "win32") {
    exts = (env.PATHEXT || ".COM;.EXE").split(";").filter((e) => /^\.(com|exe)$/i.test(e));
    if (exts.length === 0) exts = [".EXE"];
  }
  for (const dir of String(env.PATH ?? "").split(delimiter)) {
    if (!dir || !isAbsolute(dir)) continue;
    for (const ext of exts) {
      const file = join(dir, name + ext);
      if (runnable(file)) return file;
    }
  }
  return null;
}

/** The gh to run: `{ file }`, or `{ error }` fit to show the user. */
function ghProgram() {
  const named = process.env.GH_BIN;
  if (named) {
    return isAbsolute(named)
      ? { file: named }
      : { error: `gh CLI not found (GH_BIN must be an absolute path, not ${named})` };
  }
  const file = findOnPath("gh");
  return file ? { file } : { error: "gh CLI not found on PATH; install it and run gh auth login" };
}

/** A gh failure with a message fit to show the user as-is. */
class GhError extends Error {
  constructor(message, fatal = false) { super(message); this.fatal = fatal; }
}

/**
 * Run `gh api graphql` with argv (never a shell) and return `data`.
 * "gh not installed" and "gh not logged in" are fatal for the whole
 * fetch; anything else fails only this call.
 */
function ghOnce(query, vars = {}) {
  const args = ["api", "graphql", "-f", `query=${query}`];
  for (const [k, v] of Object.entries(vars)) args.push("-f", `${k}=${v}`);
  const gh = ghProgram();
  if (gh.error) return Promise.reject(new GhError(gh.error, true));
  const [file, argv] = /\.[cm]?js$/i.test(gh.file) ? [process.execPath, [gh.file, ...args]] : [gh.file, args];
  return new Promise((ok, no) => {
    execFile(file, argv, { cwd: HERE, maxBuffer: 64 * 1024 * 1024, timeout: GH_TIMEOUT_MS, windowsHide: true }, (err, stdout, stderr) => {
      const errText = String(stderr ?? "").trim();
      if (err) {
        if (err.code === "ENOENT") return no(new GhError(`gh CLI not found (${gh.file}); install it and run gh auth login`, true));
        if (err.code === 4 || /gh auth login/.test(errText)) return no(new GhError("gh is not logged in; run gh auth login", true));
        if (err.killed) return no(new GhError(`gh timed out after ${GH_TIMEOUT_MS / 1000} s`));
        const line = errText.split(/\r?\n/).find(Boolean) ?? `gh exited ${err.code}`;
        return no(new GhError(line.replace(/^gh:\s*/, "")));
      }
      let parsed;
      try { parsed = JSON.parse(stdout); } catch { return no(new GhError("gh returned something that is not JSON")); }
      if (parsed?.errors?.length && !parsed.data) return no(new GhError(parsed.errors[0]?.message ?? "GraphQL error"));
      ok(parsed.data ?? {});
    });
  });
}

/**
 * ghOnce with one retry: a large search now and then comes back cut
 * short or times out on GitHub's side, and a second try usually works.
 * Fatal errors (no gh, no login) are not retried.
 */
async function gh(query, vars = {}) {
  try { return await ghOnce(query, vars); } catch (e) {
    if (e instanceof GhError && e.fatal) throw e;
    return ghOnce(query, vars);
  }
}

// ---- fetch + cache -------------------------------------------------------

const cache = {
  last: null,          // last bodies served: { prs, work }
  perSearch: new Map(),// search name -> nodes from its last good call
  viewer: null,
  inflight: null,      // promise of the running fetch
  lastStart: 0,        // epoch ms the last fetch started
};

const isoDay = (ms) => new Date(ms).toISOString().slice(0, 10);

/** Both views' bodies after a failure that leaves nothing fresh: the
 *  previous items (or none, when `empty`) with one error line. */
function failedBodies(message, empty) {
  const keep = (body) => ({
    freshness: { asOf: body?.freshness?.asOf ?? null, errors: [{ source: "github", message }] },
    items: empty ? [] : body?.items ?? [],
  });
  return { prs: keep(cache.last?.prs), work: keep(cache.last?.work) };
}

/** The Work view's body: open issues assigned to the viewer. Its own call,
 *  so a failing issue search leaves the PRs alone and the reverse. */
async function fetchWork(viewer, started) {
  const errors = [];
  const notices = [];
  let found;
  try {
    const data = await gh(SEARCH_ISSUES, { q: ISSUES_SEARCH.q });
    found = (data.search?.nodes ?? []).filter((n) => n && typeof n.number === "number");
    cache.perSearch.set("issues", found);
    if ((data.search?.issueCount ?? 0) > found.length) {
      notices.push(`${ISSUES_SEARCH.label}: showing ${found.length} of ${data.search.issueCount} issues`);
    }
  } catch (e) {
    const kept = cache.perSearch.get("issues");
    errors.push({ source: "github:issues", message: `${e.message}${kept ? " (showing the previous results)" : ""}` });
    found = kept ?? [];
  }
  const items = [];
  const keys = new Set();
  for (const node of found) {
    try {
      const item = toWorkItem(node, { viewer });
      if (item && !keys.has(item.key)) { keys.add(item.key); items.push(item); }
    } catch (e) {
      errors.push({ source: "github", message: `could not read ${node.repository?.nameWithOwner ?? "?"}#${node.number}: ${e.message}` });
    }
  }
  return {
    freshness: {
      asOf: new Date(started).toISOString(),
      ...(errors.length ? { errors } : {}),
      ...(notices.length ? { notices } : {}),
    },
    items,
  };
}

async function fetchAll() {
  const started = Date.now();
  let viewer;
  try {
    viewer = (await gh(VIEWER)).viewer?.login;
    if (!viewer) throw new GhError("gh did not report who is logged in");
    cache.viewer = viewer;
  } catch (e) {
    const message = e instanceof GhError ? e.message : String(e?.message ?? e);
    // gh missing or logged out: nothing it showed before can be trusted
    // to still be visible, so the lists empty. Any other failure keeps
    // what was shown before and says why it is not fresh.
    return failedBodies(message, e instanceof GhError && e.fatal);
  }
  const [prs, work] = await Promise.all([fetchPrs(viewer, started), fetchWork(viewer, started)]);
  return { prs, work };
}

/** The PRs view's body from the five pull request searches. */
async function fetchPrs(viewer, started) {
  const errors = [];
  const notices = [];
  const plan = searches(isoDay(started - RECENT_DAYS * 24 * 60 * 60 * 1000));
  const results = await Promise.all(Object.entries(plan).map(async ([name, s]) => {
    try {
      const data = await gh(SEARCH_PRS, { q: s.q });
      const found = (data.search?.nodes ?? []).filter((n) => n && n.id);
      cache.perSearch.set(name, found);
      if ((data.search?.issueCount ?? 0) > found.length) {
        // A capped search is not a failure: what came back is complete as far
        // as it goes, so it is a notice, not an error.
        notices.push(`${s.label}: showing ${found.length} of ${data.search.issueCount} pull requests`);
      }
      return [name, s, found];
    } catch (e) {
      const kept = cache.perSearch.get(name);
      errors.push({ source: `github:${name}`, message: `${e.message}${kept ? " (showing the previous results)" : ""}` });
      return [name, s, kept ?? []];
    }
  }));

  // Dedupe by node id within each role, remembering which searches found
  // each PR (a PR found by the "commented" search means I commented).
  const byRole = { mine: new Map(), reviewing: new Map() };
  for (const [name, s, found] of results) {
    for (const n of found) {
      const seen = byRole[s.role].get(n.id) ?? { node: n, via: new Set() };
      seen.via.add(name);
      byRole[s.role].set(n.id, seen);
    }
  }
  const items = [];
  for (const role of ["mine", "reviewing"]) {
    let count = 0;
    for (const { node, via } of byRole[role].values()) {
      if (count >= CAP_PER_ROLE) break;
      // A PR I wrote never shows as one I review.
      if (role === "reviewing" && node.author?.login?.toLowerCase() === viewer.toLowerCase()) continue;
      try {
        const pr = toPr(node, { viewer, role, via, now: started });
        if (pr) { items.push(pr); count++; }
      } catch (e) {
        errors.push({ source: "github", message: `could not read ${node.repository?.nameWithOwner ?? "?"}#${node.number}: ${e.message}` });
      }
    }
  }
  return {
    freshness: {
      asOf: new Date(started).toISOString(),
      ...(errors.length ? { errors } : {}),
      ...(notices.length ? { notices } : {}),
    },
    items,
  };
}

/** Start a fetch unless one is running or the last started too recently. */
function maybeRefresh(force = false) {
  if (cache.inflight) return cache.inflight;
  if (!force && cache.last && Date.now() - cache.lastStart < minRefreshMs) return null;
  cache.lastStart = Date.now();
  cache.inflight = fetchAll()
    .catch((e) => failedBodies(String(e?.message ?? e), false))
    .then((bodies) => { cache.last = bodies; return bodies; })
    .finally(() => { cache.inflight = null; });
  return cache.inflight;
}

function current(view) {
  const body = cache.last[view];
  const next = new Date(Math.max(cache.lastStart + minRefreshMs, Date.now())).toISOString();
  return { ...body, freshness: { ...body.freshness, refreshing: Boolean(cache.inflight), nextUtc: next } };
}

/** A view's body (`prs` or `work`). */
async function served(view) {
  const running = maybeRefresh();
  // The very first request waits for the first fetch; after that the
  // cached body is served at once while a refresh runs behind it.
  if (!cache.last && running) await running;
  return current(view);
}

// ---- http ----------------------------------------------------------------

function send(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}
const fail = (res, status, kind, message, detail) =>
  send(res, status, { kind, message, ...(detail === undefined ? {} : { detail }) });

function readJson(req) {
  return new Promise((ok, no) => {
    const parts = []; let size = 0;
    req.on("data", (c) => {
      size += c.length;
      if (size > 64 * 1024) { no(new Error("body too large")); req.destroy(); } else parts.push(c);
    });
    req.on("end", () => {
      try { ok(parts.length ? JSON.parse(Buffer.concat(parts).toString("utf8")) : {}); } catch (e) { no(e); }
    });
    req.on("error", no);
  });
}

// A forced refresh still waits this long after the previous fetch, so a
// button held down cannot hammer the API.
const FORCED_MIN_MS = 10_000;

async function handleAction(req, res) {
  const body = await readJson(req);
  if (body.id !== "refresh") return fail(res, 404, "unknown-action", `No action ${JSON.stringify(body.id)}`);
  if (!cache.inflight && Date.now() - cache.lastStart < Math.min(FORCED_MIN_MS, minRefreshMs)) {
    return send(res, 200, { message: "Refreshed moments ago" });
  }
  maybeRefresh(true);
  return send(res, 200, { message: "Refreshing" });
}

async function route(req, res, url) {
  const p = url.pathname;
  const get = req.method === "GET";
  if (get && p === "/chappa/v1/identity") {
    const challenge = url.searchParams.get("challenge");
    if (!isValidChallenge(challenge)) return fail(res, 400, "bad-challenge", "challenge must be 64 lowercase hex characters");
    return send(res, 200, { proof: proof(token, challenge, addr) });
  }
  if (!authorized(req)) return fail(res, 401, "unauthorized", "A bearer token is required");
  if (get && p === "/chappa/v1/manifest") return send(res, 200, MANIFEST);
  if (get && p === "/api/prs") return send(res, 200, await served("prs"));
  if (get && p === "/api/work") return send(res, 200, await served("work"));
  if (req.method === "POST" && p === "/api/actions") return handleAction(req, res);
  return fail(res, 404, "not-found", `No route for ${req.method} ${p}`);
}

const server = createServer((req, res) => {
  const url = new URL(req.url, "http://source.invalid");
  res.on("finish", () => console.error(`${req.method} ${url.pathname} ${res.statusCode}`));
  route(req, res, url).catch((e) => {
    if (!res.headersSent) fail(res, 400, "bad-request", String(e?.message ?? e));
  });
});

server.listen(Number(m[2]), host, () => {
  console.error(`github source listening on ${addr}`);
  maybeRefresh(); // warm the cache so the first poll is quick
});
