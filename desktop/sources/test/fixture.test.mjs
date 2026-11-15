import test, { before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { proof } from "../lib/identity.mjs";

const script = fileURLToPath(new URL("../fixture/index.mjs", import.meta.url));
const dataDir = fileURLToPath(new URL("../fixture/data/", import.meta.url));
// Any 64-hex value will do; built here so no line looks like a real key.
const TOKEN = "ab".repeat(32);
const CHALLENGE = "ab".repeat(32);

function freePort() {
  return new Promise((ok, no) => {
    const s = createServer();
    s.once("error", no);
    s.listen(0, "127.0.0.1", () => { const { port } = s.address(); s.close(() => ok(port)); });
  });
}

async function start(extraArgs = [], env = {}) {
  const port = await freePort();
  const addr = `127.0.0.1:${port}`;
  const child = spawn(process.execPath, [script, ...extraArgs], {
    env: { ...process.env, CHAPPA_SOURCE_ADDR: addr, CHAPPA_SOURCE_TOKEN: TOKEN, ...env },
    stdio: ["ignore", "ignore", "pipe"],
  });
  let log = "";
  await new Promise((ok, no) => {
    child.stderr.on("data", (c) => { log += c; if (log.includes("listening")) ok(); });
    child.once("exit", (code) => no(new Error(`fixture exited ${code}: ${log}`)));
  });
  const call = async (path, { method = "GET", body, auth = true, headers = {} } = {}) => {
    const res = await fetch(`http://${addr}${path}`, {
      method,
      headers: { ...(auth ? { authorization: `Bearer ${TOKEN}` } : {}), ...(body ? { "content-type": "application/json" } : {}), ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: await res.json() };
  };
  return { addr, call, stop: () => child.kill(), log: () => log };
}

let src;
before(async () => { src = await start([], { CHAPPA_FIXTURE_JOB_MS: "200" }); });
after(() => src.stop());

const confirmed = (id) => ({ id, target: { view: "work", key: "ABC-1" }, params: {}, confirmed: true });
const RUNS = { "x-chappa-runs-jobs": "1" };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test("identity answers without a token with the proof for this address", async () => {
  const r = await src.call(`/chappa/v1/identity?challenge=${CHALLENGE}`, { auth: false });
  assert.equal(r.status, 200);
  assert.equal(r.body.proof, proof(TOKEN, CHALLENGE, src.addr));
});

test("identity refuses a bad or missing challenge", async () => {
  for (const q of ["", "?challenge=abc", `?challenge=${"A".repeat(64)}`, `?challenge=${"a".repeat(65)}`]) {
    const r = await src.call(`/chappa/v1/identity${q}`, { auth: false });
    assert.equal(r.status, 400, q);
    assert.equal(r.body.kind, "bad-challenge");
  }
});

test("everything else needs the token", async () => {
  for (const [path, opts] of [["/chappa/v1/manifest", {}], ["/api/work", {}], ["/api/actions", { method: "POST", body: {} }], ["/api/jobs/job-1", {}]]) {
    const r = await src.call(path, { ...opts, auth: false });
    assert.equal(r.status, 401, path);
    assert.equal(r.body.kind, "unauthorized");
  }
  const wrong = await src.call("/api/work", { auth: false, headers: { authorization: `Bearer ${"0".repeat(64)}` } });
  assert.equal(wrong.status, 401);
});

test("identity, then token, then manifest", async () => {
  const id = await src.call(`/chappa/v1/identity?challenge=${CHALLENGE}`, { auth: false });
  assert.equal(id.body.proof, proof(TOKEN, CHALLENGE, src.addr));
  const m = await src.call("/chappa/v1/manifest");
  assert.equal(m.status, 200);
  assert.equal(m.body.contract, 1);
  assert.deepEqual(Object.keys(m.body.views).sort(), ["attention", "environment", "prs", "work"]);
  assert.match(m.body.actions, /^\/api\//);
});

test("every manifest path is served and every list has freshness", async () => {
  const m = (await src.call("/chappa/v1/manifest")).body;
  for (const [name, v] of Object.entries(m.views)) {
    const r = await src.call(v.list);
    assert.equal(r.status, 200, name);
    assert.ok("asOf" in r.body.freshness, name);
    assert.ok(Array.isArray(r.body.items ?? r.body.components), name);
  }
  assert.equal((await src.call(m.workflows)).status, 200);
  const item = await src.call(m.views.work.item.replace("{key}", encodeURIComponent("ABC-1")));
  assert.equal(item.body.key, "ABC-1");
  assert.equal((await src.call("/api/work/nope")).status, 404);
  const panel = await src.call("/api/panel/validate");
  assert.ok(panel.body.title && panel.body.freshness && Array.isArray(panel.body.sections));
  // Every other panel a payload names is served from its own file.
  const named = (await src.call("/api/prs")).body.items.flatMap((p) => p.actions ?? []).filter((a) => a.kind === "panel");
  assert.deepEqual(named.map((a) => a.endpoint), ["/api/panel/checks"]);
  const checks = await src.call("/api/panel/checks");
  assert.equal(checks.body.title, "Checks for #2");
  assert.ok(checks.body.actions.some((a) => a.kind === "endpoint"), "a panel with its own endpoint action");
  assert.equal((await src.call("/api/panel/nowhere")).status, 404);
});

test("data covers every PR bucket, all nine states and a pending approval", async () => {
  const buckets = (await src.call("/api/prs")).body.items.map((p) => `${p.role}/${p.bucket}`).sort();
  assert.deepEqual(buckets, [
    "mine/action", "mine/draft", "mine/ready", "mine/recent", "mine/waiting",
    "reviewing/group", "reviewing/mythreads", "reviewing/replied", "reviewing/vote", "reviewing/voted",
  ]);
  const env = (await src.call("/api/env")).body;
  assert.equal(new Set(env.components.map((c) => c.state)).size, 9);
  assert.ok(env.summary && env.alerts.length && env.facts.length);
  assert.ok(env.components.some((c) => c.state === "not-deployed"));
  assert.ok((await src.call("/api/work")).body.items.some((i) => i.localStatus?.pendingApproval));
});

test("a mutating action without confirmed is refused with 409", async () => {
  for (const body of [{ id: "approve", target: { view: "work", key: "ABC-1" } }, { ...confirmed("approve"), confirmed: false }, { ...confirmed("approve"), confirmed: "true" }]) {
    const r = await src.call("/api/actions", { method: "POST", body });
    assert.equal(r.status, 409);
    assert.equal(r.body.kind, "confirmation-required");
    assert.equal(typeof r.body.message, "string");
  }
});

test("200, unknown action and 503", async () => {
  const ok = await src.call("/api/actions", { method: "POST", body: { id: "ping", target: { view: "work", key: "ABC-1" } } });
  assert.deepEqual([ok.status, ok.body.message], [200, "pong"]);
  assert.equal((await src.call("/api/actions", { method: "POST", body: confirmed("approve") })).status, 200);
  assert.equal((await src.call("/api/actions", { method: "POST", body: confirmed("nope") })).status, 404);
  const flaky = await src.call("/api/actions", { method: "POST", body: confirmed("flaky-deploy") });
  assert.equal(flaky.status, 503);
  assert.equal(flaky.body.kind, "uncertain");
});

test("202 {jobId} polls from running to done", async () => {
  const r = await src.call("/api/actions", { method: "POST", body: confirmed("start-build"), headers: RUNS });
  assert.equal(r.status, 202);
  assert.match(r.body.jobId, /^job-/);
  assert.equal(r.body.run, undefined);
  assert.equal((await src.call(`/api/jobs/${r.body.jobId}`)).body.state, "running");
  await sleep(500);
  assert.equal((await src.call(`/api/jobs/${r.body.jobId}`)).body.state, "done");
  assert.equal((await src.call("/api/jobs/missing")).status, 404);
});

test("202 with a run block stays running until the exit report arrives", async () => {
  const r = await src.call("/api/actions", { method: "POST", body: confirmed("run-seed"), headers: RUNS });
  assert.equal(r.status, 202);
  assert.deepEqual(Object.keys(r.body.run).sort(), ["argv", "cwd", "env", "title"]);
  await sleep(500);
  assert.equal((await src.call(`/api/jobs/${r.body.jobId}`)).body.state, "running");
  const post = await src.call(`/api/jobs/${r.body.jobId}`, { method: "POST", body: { exitCode: 0, endedUtc: "2026-01-15T10:00:00Z", output: "seeded\n" } });
  assert.equal(post.status, 200);
  const done = await src.call(`/api/jobs/${r.body.jobId}`);
  assert.deepEqual(done.body, { state: "done", log: "seeded\n" });
});

test("a non-zero exit report marks the job failed", async () => {
  const r = await src.call("/api/actions", { method: "POST", body: confirmed("run-seed"), headers: RUNS });
  await src.call(`/api/jobs/${r.body.jobId}`, { method: "POST", body: { exitCode: 2, endedUtc: "2026-01-15T10:00:00Z", output: "boom" } });
  assert.equal((await src.call(`/api/jobs/${r.body.jobId}`)).body.state, "failed");
});

test("without the runs-jobs header the run block is dropped and the source finishes the job", async () => {
  const r = await src.call("/api/actions", { method: "POST", body: confirmed("run-seed") });
  assert.equal(r.status, 202);
  assert.equal(r.body.run, undefined);
  await sleep(500);
  assert.equal((await src.call(`/api/jobs/${r.body.jobId}`)).body.state, "done");
});

test("requests are logged one line each on stderr", () => {
  assert.match(src.log(), /^GET \/chappa\/v1\/identity 200$/m);
  assert.match(src.log(), /^POST \/api\/actions 409$/m);
});

test("--data swaps the response bodies", async () => {
  const dir = mkdtempSync(join(tmpdir(), "chappa-fixture-"));
  try {
    cpSync(dataDir, dir, { recursive: true });
    const work = JSON.parse(readFileSync(join(dir, "work.json"), "utf8"));
    work.items = [{ key: "ABC-9", title: "Swapped", status: null }];
    writeFileSync(join(dir, "work.json"), JSON.stringify(work));
    const other = await start(["--data", dir]);
    try {
      assert.deepEqual((await other.call("/api/work")).body.items.map((i) => i.key), ["ABC-9"]);
    } finally { other.stop(); }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("refuses a non-loopback address and a bad token", async () => {
  for (const env of [{ CHAPPA_SOURCE_ADDR: "0.0.0.0:5999" }, { CHAPPA_SOURCE_ADDR: "192.168.1.5:5999" }, { CHAPPA_SOURCE_TOKEN: "short" }]) {
    const code = await new Promise((ok) => {
      const c = spawn(process.execPath, [script], { env: { ...process.env, CHAPPA_SOURCE_ADDR: "127.0.0.1:5999", CHAPPA_SOURCE_TOKEN: TOKEN, ...env }, stdio: "ignore" });
      c.once("exit", ok);
    });
    assert.equal(code, 2, JSON.stringify(env));
  }
});

const overlayDir = fileURLToPath(new URL("../fixture/data-overlay/", import.meta.url));

test("the overlay data runs a second source that adds to the first one's work items", async () => {
  const ws = await start(["--data", overlayDir]);
  try {
    const manifest = (await ws.call("/chappa/v1/manifest")).body;
    assert.equal(manifest.source.id, "workspaces");
    assert.deepEqual(Object.keys(manifest.views), ["work"]);
    assert.deepEqual(manifest.itemActions.map((a) => [a.id, a.kind, a.mutates, a.on.view]), [["start-work", "endpoint", true, "work"], ["review-later", "endpoint", false, "prs"]]);
    assert.match(manifest.itemActions[0].confirm, /\{key\}.*\{title\}/);
    const work = (await ws.call("/api/work")).body;
    assert.ok(work.freshness);
    // Every entry points at the item it adds to; one of them at a source that is not the fixture.
    assert.deepEqual(work.items.map((i) => [i.ref.source, i.ref.key, i.key]), [
      ["fixture", "ABC-1", "ABC-1"],
      ["fixture", "ABC-2", "ABC-2"],
      ["fixture", "ABC-9", "ABC-9"],
      ["tracker", "XYZ-1", "XYZ-1"],
    ]);
    // The tracker lists ABC-1 and ABC-2 but not ABC-9: the owner-missing case.
    const tracker = JSON.parse(readFileSync(join(dataDir, "work.json"), "utf8")).items.map((i) => i.key);
    assert.ok(tracker.includes("ABC-1") && tracker.includes("ABC-2") && !tracker.includes("ABC-9"));
  } finally { ws.stop(); }
});

test("an echoing action answers with exactly the target, params and note it was sent", async () => {
  const ws = await start(["--data", overlayDir]);
  try {
    const item = { key: "ABC-3", title: "Retire the old queue" };
    const refused = await ws.call("/api/actions", { method: "POST", body: { id: "start-work", target: { view: "work", key: "ABC-3", source: "fixture", item } } });
    assert.equal(refused.status, 409);
    const r = await ws.call("/api/actions", { method: "POST", body: { id: "start-work", target: { view: "work", key: "ABC-3", source: "fixture", item }, params: { from: "tracker", slot: 0 }, note: "n", confirmed: true } });
    assert.equal(r.status, 200);
    assert.deepEqual(JSON.parse(r.body.message), { target: { view: "work", key: "ABC-3", source: "fixture", item }, params: { from: "tracker", slot: 0 }, note: "n" });
    const later = await ws.call("/api/actions", { method: "POST", body: { id: "review-later", target: { view: "prs", key: "github:owner/repo:#1" } } });
    assert.deepEqual(JSON.parse(later.body.message), { target: { view: "prs", key: "github:owner/repo:#1" }, params: null, note: null });
  } finally { ws.stop(); }
});

test("the overlay asks for owner updates, notes each one, and offers an action with inputs", async () => {
  const dir = mkdtempSync(join(tmpdir(), "chappa-owner-"));
  const log = join(dir, "owner.jsonl");
  const ws = await start(["--data", overlayDir], { CHAPPA_FIXTURE_OWNER_LOG: log });
  try {
    const manifest = (await ws.call("/chappa/v1/manifest")).body;
    assert.equal(manifest.ownerUpdates, "/api/work/owner");
    const item = { key: "ABC-1", status: { label: "Done" } };
    const r = await ws.call("/api/work/owner", { method: "POST", body: { source: "fixture", key: "ABC-1", item } });
    assert.deepEqual(r, { status: 200, body: { message: "noted fixture:ABC-1" } });
    assert.deepEqual(readFileSync(log, "utf8").trim().split("\n").map((l) => JSON.parse(l)), [{ source: "fixture", key: "ABC-1", item }]);
    assert.equal((await ws.call("/api/work/owner", { method: "POST", body: { source: "fixture", key: "ABC-1" } })).status, 400);
    assert.equal((await ws.call("/api/work/owner", { method: "POST", body: { key: "ABC-1", item }, auth: false })).status, 401);
    // An action with inputs, under ABC-2's Repos section: echoed like the rest.
    const abc2 = (await ws.call("/api/work")).body.items.find((i) => i.key === "ABC-2");
    const pick = abc2.sections[0].actions.find((x) => x.id === "pick-seeds");
    assert.deepEqual(pick.inputs.map((i) => [i.name, i.kind]), [["seeds", "multi"], ["count", "number"], ["why", "text"], ["size", "choice"]]);
    const echoed = await ws.call("/api/actions", { method: "POST", body: { id: "pick-seeds", target: { view: "work", key: "ABC-2" }, params: { count: 3, slot: 0, size: "l" }, confirmed: true } });
    assert.deepEqual(JSON.parse(echoed.body.message).params, { count: 3, slot: 0, size: "l" });
  } finally {
    ws.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an echoing action also echoes the caller's actor header when it sent one", async () => {
  const ws = await start(["--data", overlayDir]);
  try {
    const r = await ws.call("/api/actions", { method: "POST", body: { id: "review-later", target: { view: "prs", key: "github:owner/repo:#1" } }, headers: { "x-chappa-actor": "human:Alex" } });
    assert.equal(JSON.parse(r.body.message).actor, "human:Alex");
  } finally { ws.stop(); }
});

test("the data-safety mode serves the block until a person acknowledges it", async () => {
  // The default fixture has no block at all.
  assert.equal((await src.call("/api/env")).body.dataSafety, undefined);
  const dir = mkdtempSync(join(tmpdir(), "fixture-ds-"));
  cpSync(dataDir, dir, { recursive: true });
  const ds = await start(["--data", dir], { CHAPPA_FIXTURE_DATA_SAFETY: "1" });
  try {
    const env = (await ds.call("/api/env")).body;
    assert.equal(env.dataSafety.blocksAgents, true);
    assert.deepEqual(env.dataSafety.bound, { source: "fixture", key: "ABC-1", folders: ["/srv/example/abc-1"] });
    const banner = env.banners.find((b) => b.id === "data-safety");
    assert.deepEqual(banner.actions.map((a) => [a.id, a.mutates, a.note]), [["data-safety-ack", true, "required"]]);
    const ack = (headers, body) => ds.call("/api/actions", { method: "POST", body: { id: "data-safety-ack", target: { view: "environment", key: null }, ...body }, headers });
    assert.equal((await ack({ "x-chappa-actor": "human:Alex" }, { note: "known rows" })).body.kind, "confirmation-required");
    assert.equal((await ack({ "x-chappa-actor": "human:Alex" }, { confirmed: true })).body.kind, "note-required");
    const agent = await ack({ "x-chappa-actor": "agent:7" }, { confirmed: true, note: "x" });
    assert.equal(agent.status, 409);
    assert.match(agent.body.message, /agent:7/);
    assert.equal((await ack({}, { confirmed: true, note: "x" })).body.kind, "human-only", "no header is not a person");
    const ok = await ack({ "x-chappa-actor": "human:Alex" }, { confirmed: true, note: "known rows" });
    assert.equal(ok.status, 200);
    const after = (await ds.call("/api/env")).body;
    assert.equal(after.dataSafety.state, "acknowledged");
    assert.equal(after.dataSafety.blocksAgents, false);
    assert.equal(after.dataSafety.ack.by, "human:Alex");
    assert.ok(!after.banners.some((b) => b.id === "data-safety"));
    // A new verdict (the file changes) ends the acknowledgement.
    const block = JSON.parse(readFileSync(join(dir, "data-safety.json"), "utf8"));
    block.findings[0].count = 3;
    writeFileSync(join(dir, "data-safety.json"), JSON.stringify(block));
    assert.equal((await ds.call("/api/env")).body.dataSafety.blocksAgents, true);
  } finally { ds.stop(); rmSync(dir, { recursive: true, force: true }); }
});
