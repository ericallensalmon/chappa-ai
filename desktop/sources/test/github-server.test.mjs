// The GitHub source over real HTTP, with gh replaced by a stub script
// (test/github-stub/gh.mjs) that prints canned GraphQL answers. Nothing
// here reaches the network beyond loopback.
import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import { proof } from "../lib/identity.mjs";

const script = fileURLToPath(new URL("../github/index.mjs", import.meta.url));
const stub = fileURLToPath(new URL("./github-stub/gh.mjs", import.meta.url));
// Any 64-hex value will do; built here so no line looks like a real key.
const TOKEN = "ab".repeat(32);
const CHALLENGE = "cd".repeat(32);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function freePort() {
  return new Promise((ok, no) => {
    const s = createServer();
    s.once("error", no);
    s.listen(0, "127.0.0.1", () => { const { port } = s.address(); s.close(() => ok(port)); });
  });
}

async function start(env = {}, cwd = undefined) {
  const port = await freePort();
  const addr = `127.0.0.1:${port}`;
  const child = spawn(process.execPath, [script], {
    env: { ...process.env, CHAPPA_SOURCE_ADDR: addr, CHAPPA_SOURCE_TOKEN: TOKEN, GH_BIN: stub, ...env },
    stdio: ["ignore", "ignore", "pipe"],
    cwd,
  });
  let log = "";
  await new Promise((ok, no) => {
    child.stderr.on("data", (c) => { log += c; if (log.includes("listening")) ok(); });
    child.once("exit", (code) => no(new Error(`source exited ${code}: ${log}`)));
  });
  const call = async (path, { method = "GET", body, auth = true } = {}) => {
    const res = await fetch(`http://${addr}${path}`, {
      method,
      headers: { ...(auth ? { authorization: `Bearer ${TOKEN}` } : {}), ...(body ? { "content-type": "application/json" } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: await res.json() };
  };
  return { addr, call, stop: () => child.kill(), log: () => log };
}

async function withSource(env, fn, cwd = undefined) {
  const src = await start(env, cwd);
  try { return await fn(src); } finally { src.stop(); }
}

const row = (items, number, repo) => items.find((p) => p.number === number && (!repo || p.repo === repo));

test("gh never sees the source's address or token", async () => {
  const dir = mkdtempSync(join(tmpdir(), "gh-env-"));
  const log = join(dir, "env.log");
  try {
    await withSource({ GH_STUB_ENV_LOG: log }, async (src) => {
      const r = await src.call("/api/prs");
      assert.equal(r.status, 200);
    });
    const lines = readFileSync(log, "utf8").trim().split("\n");
    assert.ok(lines.length > 0);
    for (const l of lines) assert.equal(l, "seen:");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("identity first, then the token; everything else is closed without it", () => withSource({}, async (src) => {
  const id = await src.call(`/chappa/v1/identity?challenge=${CHALLENGE}`, { auth: false });
  assert.equal(id.status, 200);
  assert.equal(id.body.proof, proof(TOKEN, CHALLENGE, src.addr));
  assert.equal((await src.call("/chappa/v1/identity?challenge=nope", { auth: false })).status, 400);
  for (const path of ["/chappa/v1/manifest", "/api/prs", "/api/work"]) assert.equal((await src.call(path, { auth: false })).status, 401, path);
  assert.equal((await src.call("/api/env")).status, 404);
  assert.match(src.log(), /GET \/api\/prs 401/);
}));

test("the manifest offers the Work and PRs views, github.com links and an actions endpoint", () => withSource({}, async (src) => {
  const { status, body } = await src.call("/chappa/v1/manifest");
  assert.equal(status, 200);
  assert.equal(body.contract, 1);
  assert.equal(body.source.id, "github");
  assert.deepEqual(Object.keys(body.views), ["work", "prs"]);
  assert.deepEqual(body.views.work, { list: "/api/work", pollSeconds: 300 });
  assert.deepEqual(body.views.prs, { list: "/api/prs", pollSeconds: 300 });
  assert.deepEqual(body.linkHosts, ["github.com"]);
  assert.equal(body.actions, "/api/actions");
}));

test("PRs are bucketed per role from the searches, deduped, and old ones dropped", () => withSource({}, async (src) => {
  const before = Date.now();
  const { status, body } = await src.call("/api/prs");
  assert.equal(status, 200);
  assert.equal(body.freshness.errors, undefined);
  assert.equal(body.freshness.notices, undefined);
  assert.ok(Date.parse(body.freshness.asOf) >= before - 5000);
  assert.equal(body.freshness.refreshing, false);
  const by = Object.fromEntries(body.items.map((p) => [`${p.role} ${p.repo}${p.number}`, p.bucket]));
  assert.deepEqual(by, {
    "mine owner/repo#1": "action",
    "mine owner/repo#2": "draft",
    "mine owner/repo#3": "ready",
    "mine owner/repo#4": "waiting",
    "mine owner/repo#5": "recent",
    "reviewing other/lib#7": "vote",
    "reviewing other/lib#8": "group",
    "reviewing other/lib#9": "voted",
    "reviewing other/lib#10": "replied",
    "reviewing other/lib#11": "mythreads",
  });
  const one = row(body.items, "#1", "owner/repo");
  assert.deepEqual(one.reasons, ["2 failing checks: build, lint", "changes requested by Sam", "conflicts with main", "1 thread to answer"]);
  assert.deepEqual(one.workKeys.sort(), ["owner/repo#12", "owner/repo#13"]);
  assert.deepEqual(one.threads, { active: 1, resolved: 1, needsMe: 1 });
  assert.deepEqual(row(body.items, "#4").reasons, ["required review not given", "waiting on Sam", "1 pending check: e2e"]);
}));

test("one failing search is reported and the rest still renders", () => withSource({ GH_STUB_FAIL: "requested" }, async (src) => {
  const { body } = await src.call("/api/prs");
  assert.equal(body.freshness.errors.length, 1);
  assert.equal(body.freshness.errors[0].source, "github:requested");
  assert.match(body.freshness.errors[0].message, /Something went wrong/);
  assert.equal(row(body.items, "#8"), undefined);
  // #7 was also found by the comment search, so it still shows.
  assert.ok(row(body.items, "#7"));
  assert.ok(row(body.items, "#1"));
}));

test("a capped search is a quiet notice, not an error", () => withSource({ GH_STUB_TOTAL: "mine-open:212" }, async (src) => {
  const { body } = await src.call("/api/prs");
  assert.equal(body.freshness.errors, undefined);
  assert.deepEqual(body.freshness.notices, ["my open PRs: showing 4 of 212 pull requests"]);
  assert.ok(row(body.items, "#1", "owner/repo"));
}));

test("gh not logged in gives an empty list and one clear error", () => withSource({ GH_STUB_MODE: "logged-out" }, async (src) => {
  const { status, body } = await src.call("/api/prs");
  assert.equal(status, 200);
  assert.deepEqual(body.items, []);
  assert.equal(body.freshness.errors.length, 1);
  assert.match(body.freshness.errors[0].message, /not logged in; run gh auth login/);
}));

test("gh missing gives an empty list and one clear error", () => withSource({ GH_BIN: join(tmpdir(), "gh-that-is-not-installed") }, async (src) => {
  const { status, body } = await src.call("/api/prs");
  assert.equal(status, 200);
  assert.deepEqual(body.items, []);
  assert.equal(body.freshness.errors.length, 1);
  assert.match(body.freshness.errors[0].message, /gh CLI not found/);
}));

test("a relative GH_BIN is refused, not looked up", () => withSource({ GH_BIN: "gh" }, async (src) => {
  const { body } = await src.call("/api/prs");
  assert.deepEqual(body.items, []);
  assert.match(body.freshness.errors[0].message, /GH_BIN must be an absolute path/);
}));

test("a gh in the folder the source was started in never runs", async () => {
  // The folder holds a `gh` (`gh.exe` on Windows) that is really Node, and
  // a script named `api`: run as `gh api graphql …` it runs that script,
  // which leaves a marker. PATH names that folder only relatively, the
  // way a planted program gets found; one absolute entry holds no gh.
  const dir = mkdtempSync(join(tmpdir(), "gh-cwd-"));
  const marker = join(dir, "MARKER");
  try {
    const fake = join(dir, process.platform === "win32" ? "gh.exe" : "gh");
    if (process.platform === "win32") copyFileSync(process.execPath, fake);
    else symlinkSync(process.execPath, fake);
    writeFileSync(join(dir, "api"), `require("fs").writeFileSync(${JSON.stringify(marker)}, "ran");\n`);
    const empty = join(dir, "empty-bin");
    mkdirSync(empty);
    const PATH = ["", ".", "relative-bin", empty].join(delimiter);
    const body = await withSource({ GH_BIN: undefined, PATH }, async (src) => {
      const r = await src.call("/api/prs");
      assert.equal(r.status, 200);
      return r.body;
    }, dir);
    assert.equal(existsSync(marker), false, "the gh in the working folder ran");
    assert.deepEqual(body.items, []);
    assert.match(body.freshness.errors[0].message, /gh CLI not found on PATH/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("GitHub is asked at most once per refresh interval, however often the app polls", async () => {
  const dir = mkdtempSync(join(tmpdir(), "gh-stub-"));
  const logFile = join(dir, "calls.log");
  try {
    await withSource({ GH_STUB_LOG: logFile }, async (src) => {
      for (let i = 0; i < 4; i++) assert.equal((await src.call("/api/prs")).status, 200);
      const calls = readFileSync(logFile, "utf8").trim().split("\n");
      assert.equal(calls.filter((c) => c === "viewer").length, 1);
      assert.equal(calls.length, 7); // viewer + five PR searches + the issue search
      for (let i = 0; i < 2; i++) assert.equal((await src.call("/api/work")).status, 200);
      assert.equal(readFileSync(logFile, "utf8").trim().split("\n").length, 7, "the Work view shares the fetch");
    });
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("while a refresh runs, the cached list is served with refreshing: true", () =>
  withSource({ CHAPPA_GITHUB_REFRESH_SECONDS: "0", GH_STUB_DELAY_MS: "300" }, async (src) => {
    const first = await src.call("/api/prs"); // waits for the first fetch
    assert.equal(first.body.items.length, 10);
    const second = await src.call("/api/prs"); // starts a refresh, answers at once
    assert.equal(second.body.freshness.refreshing, true);
    assert.equal(second.body.items.length, 10);
    assert.equal(second.body.freshness.asOf, first.body.freshness.asOf);
    let later;
    for (let i = 0; i < 40; i++) {
      await sleep(100);
      later = await src.call("/api/prs");
      if (later.body.freshness.asOf !== first.body.freshness.asOf) break;
    }
    assert.notEqual(later.body.freshness.asOf, first.body.freshness.asOf);
  }));

test("the refresh action is not mutating and unknown actions are refused", () => withSource({}, async (src) => {
  await src.call("/api/prs");
  const r = await src.call("/api/actions", { method: "POST", body: { id: "refresh", target: { view: "prs", key: "owner/repo#1" } } });
  assert.equal(r.status, 200);
  assert.equal(typeof r.body.message, "string");
  const u = await src.call("/api/actions", { method: "POST", body: { id: "merge", confirmed: true } });
  assert.equal(u.status, 404);
  assert.equal(u.body.kind, "unknown-action");
}));

test("Work: open issues assigned to me, keyed owner/repo#N, in progress while a closing PR is open", () => withSource({}, async (src) => {
  const before = Date.now();
  const { status, body } = await src.call("/api/work");
  assert.equal(status, 200);
  assert.equal(body.freshness.errors, undefined);
  assert.equal(body.freshness.notices, undefined);
  assert.ok(Date.parse(body.freshness.asOf) >= before - 5000);
  assert.deepEqual(body.items.map((i) => i.key), ["owner/repo#12", "owner/repo#13", "other/lib#20"]);
  const [doing, todo, other] = body.items;
  assert.deepEqual(doing.status, { label: "In progress", category: "doing" });
  assert.deepEqual(doing.prs, [{ forge: "github", repo: "owner/repo", number: "#1", url: "https://github.com/owner/repo/pull/1", status: "open" }]);
  assert.deepEqual(doing.labels, ["bug", "auth"]);
  assert.deepEqual(doing.reporter, { id: "sam", name: "Sam" });
  assert.equal(doing.type, "issue");
  assert.equal(doing.assignedToMe, true);
  assert.equal(doing.url, "https://github.com/owner/repo/issues/12");
  assert.deepEqual(doing.actions, [{ kind: "link", id: "open", label: "Open on GitHub", url: "https://github.com/owner/repo/issues/12" }]);
  assert.equal("folder" in doing, false);
  assert.equal("localStatus" in doing, false);
  // A closed closing PR does not make the issue in progress.
  assert.deepEqual(todo.status, { label: "Open", category: "todo" });
  assert.equal(todo.prs[0].status, "closed");
  assert.deepEqual(todo.reporter, { id: "alex", name: "Alex", isMe: true });
  assert.deepEqual(other.reporter, { id: "ghost-bot", name: "ghost-bot" });
  // The PRs the issues link to are the PRs view's own rows.
  const prs = (await src.call("/api/prs")).body.items;
  assert.ok(row(prs, "#1", "owner/repo"));
}));

test("Work: a capped issue search is a quiet notice", () => withSource({ GH_STUB_TOTAL: "issues:340" }, async (src) => {
  const { body } = await src.call("/api/work");
  assert.equal(body.freshness.errors, undefined);
  assert.deepEqual(body.freshness.notices, ["issues assigned to me: showing 3 of 340 issues"]);
  assert.equal((await src.call("/api/prs")).body.freshness.notices, undefined);
}));

test("Work: a failing issue search is an error on Work only; the PRs still render", () => withSource({ GH_STUB_FAIL: "issues" }, async (src) => {
  const work = (await src.call("/api/work")).body;
  assert.deepEqual(work.items, []);
  assert.equal(work.freshness.errors.length, 1);
  assert.equal(work.freshness.errors[0].source, "github:issues");
  const prs = (await src.call("/api/prs")).body;
  assert.equal(prs.freshness.errors, undefined);
  assert.equal(prs.items.length, 10);
}));

test("Work: gh not logged in empties Work too, with one clear error", () => withSource({ GH_STUB_MODE: "logged-out" }, async (src) => {
  const { body } = await src.call("/api/work");
  assert.deepEqual(body.items, []);
  assert.equal(body.freshness.errors.length, 1);
  assert.match(body.freshness.errors[0].message, /not logged in/);
}));
