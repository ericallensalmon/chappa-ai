#!/usr/bin/env node
// A complete fixture source for chappa: serves every view, the manifest,
// workflows, a panel, endpoint actions and jobs from JSON files.
//
//   CHAPPA_SOURCE_ADDR=127.0.0.1:5055 CHAPPA_SOURCE_TOKEN=<64 hex> \
//     node desktop/sources/fixture/index.mjs [--data <dir>]
//
// chappa sets both variables when it launches a source. Bodies come from
// `<dir>/{manifest,work,prs,attention,environment,workflows,panel,actions}.json`
// and `panel-<name>.json` for every other panel at `/api/panel/<name>`
// (default: ./data next to this file), re-read on every request so a test
// or a person can swap them while it runs. No packages; one line per
// request on stderr.
//
// `--data desktop/sources/fixture/data-overlay` runs a second source,
// "Workspaces", that adds to the first one's work items (`ref: {source:
// "fixture", key}`) and offers `itemActions` on its rows: run both to see
// one item fed by two sources.
import { createServer } from "node:http";
import { createHash, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { isValidChallenge, proof } from "../lib/identity.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const MAX_BODY = 8 * 1024 * 1024; // an exit report can carry a 4 MiB pane dump

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const addr = process.env.CHAPPA_SOURCE_ADDR;
const token = process.env.CHAPPA_SOURCE_TOKEN;
const dataDir = resolve(arg("--data", join(here, "data")));
// How long a job without a `run` block takes to finish by itself.
const jobMs = Number(process.env.CHAPPA_FIXTURE_JOB_MS ?? 3000);

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

const data = (name) => JSON.parse(readFileSync(join(dataDir, `${name}.json`), "utf8"));
const jobs = new Map();
let jobCounter = 0;

function send(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}
const fail = (res, status, kind, message, detail) =>
  send(res, status, { kind, message, ...(detail === undefined ? {} : { detail }) });

function readJson(req) {
  return new Promise((ok, no) => {
    const parts = [];
    let size = 0;
    req.on("data", (c) => {
      size += c.length;
      if (size > MAX_BODY) { no(new Error("body too large")); req.destroy(); } else parts.push(c);
    });
    req.on("end", () => {
      try { ok(parts.length ? JSON.parse(Buffer.concat(parts).toString("utf8")) : {}); } catch (e) { no(e); }
    });
    req.on("error", no);
  });
}

function startJob(spec, runsJobs) {
  const jobId = `job-${++jobCounter}`;
  const job = { state: "running", log: "" };
  jobs.set(jobId, job);
  const run = spec.job.run && runsJobs ? spec.job.run : null;
  // Without a pane to host the run, the source runs the job itself, so
  // it finishes on its own like a plain job.
  if (!run) setTimeout(() => { if (job.state === "running") { job.state = "done"; job.log = "finished"; } }, jobMs).unref();
  return { jobId, ...(run ? { run } : {}) };
}

async function handleAction(req, res) {
  const body = await readJson(req);
  const spec = data("actions")[body.id];
  if (!spec) return fail(res, 404, "unknown-action", `No action ${JSON.stringify(body.id)}`);
  if (spec.mutates && body.confirmed !== true) {
    return fail(res, 409, "confirmation-required", "A mutating action must be confirmed", "send confirmed: true after the user accepts");
  }
  if (spec.job) return send(res, 202, startJob(spec, req.headers["x-chappa-runs-jobs"] === "1"));
  // `echo`: answer with what the call carried, so a test sees the exact
  // target (view, key, and for rows of other sources the owner and its row).
  if (spec.echo) return send(res, spec.status, { message: JSON.stringify({ target: body.target ?? null, params: body.params ?? null, note: body.note ?? null }) });
  return send(res, spec.status, spec.body ?? {});
}

async function handleExitReport(req, res, id) {
  const job = jobs.get(id);
  if (!job) return fail(res, 404, "unknown-job", `No job ${id}`);
  const report = await readJson(req);
  job.state = report.exitCode === 0 ? "done" : "failed";
  job.log = typeof report.output === "string" ? report.output : "";
  return send(res, 200, {});
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
  if (get && p === "/chappa/v1/manifest") return send(res, 200, data("manifest"));
  if (get && p === "/api/work") return send(res, 200, data("work"));
  if (get && p.startsWith("/api/work/")) {
    const key = decodeURIComponent(p.slice("/api/work/".length));
    const item = data("work").items.find((i) => i.key === key);
    return item ? send(res, 200, item) : fail(res, 404, "not-found", `No work item ${key}`);
  }
  if (get && p === "/api/prs") return send(res, 200, data("prs"));
  if (get && p === "/api/attention") return send(res, 200, data("attention"));
  if (get && p === "/api/env") return send(res, 200, data("environment"));
  if (get && p === "/api/workflows") return send(res, 200, data("workflows"));
  if (get && p === "/api/panel/validate") return send(res, 200, data("panel"));
  if (get && /^\/api\/panel\/[a-z0-9-]+$/.test(p)) {
    const name = p.slice("/api/panel/".length);
    try {
      return send(res, 200, data(`panel-${name}`));
    } catch {
      return fail(res, 404, "not-found", `No panel ${name}`);
    }
  }
  if (req.method === "POST" && p === "/api/actions") return handleAction(req, res);
  if (p.startsWith("/api/jobs/")) {
    const id = decodeURIComponent(p.slice("/api/jobs/".length));
    if (get) {
      const job = jobs.get(id);
      return job ? send(res, 200, job.log ? { state: job.state, log: job.log } : { state: job.state }) : fail(res, 404, "unknown-job", `No job ${id}`);
    }
    if (req.method === "POST") return handleExitReport(req, res, id);
  }
  return fail(res, 404, "not-found", `No route for ${req.method} ${p}`);
}

const server = createServer((req, res) => {
  const url = new URL(req.url, "http://fixture.invalid");
  res.on("finish", () => console.error(`${req.method} ${url.pathname} ${res.statusCode}`));
  route(req, res, url).catch((e) => {
    if (!res.headersSent) fail(res, 400, "bad-request", String(e?.message ?? e));
  });
});

server.listen(Number(m[2]), host, () => console.error(`fixture source listening on ${addr}`));
