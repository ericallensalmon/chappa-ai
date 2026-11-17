#!/usr/bin/env node
// Creates a demo folder for Chappa'Ai: a few small projects with neutral
// content, the example stack, a fixture source and a scripted stand-in for
// an agent CLI, plus a profile that already lists them. Run the app with
// CHAPPA_PROFILE_DIR pointing at the profile and it shows only this content:
// nothing of the everyday profile is read or written.
//
//   node desktop/demo/seed.mjs <demo-folder> [--force] [--fixture-environment] [--docker-stack]
//
// Everything is written inside <demo-folder>. An existing folder is used
// only when it is empty or was made by this script (`--force` then starts it
// over). Node's standard library only.

import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const desktop = resolve(here, "..");
const MARKER = ".chappa-demo";

function usage(message) {
  if (message) console.error(message);
  console.error("usage: node desktop/demo/seed.mjs <demo-folder> [--force] [--fixture-environment] [--docker-stack]");
  process.exit(2);
}

const args = process.argv.slice(2);
const flags = new Set(args.filter((a) => a.startsWith("--")));
const positional = args.filter((a) => !a.startsWith("--"));
for (const flag of flags) {
  if (!["--force", "--fixture-environment", "--docker-stack"].includes(flag)) usage(`unknown option ${flag}`);
}
if (positional.length !== 1) usage();
const demo = resolve(positional[0]);

// ---- the folder ---------------------------------------------------------------

if (existsSync(demo)) {
  const entries = readdirSync(demo);
  const ours = entries.includes(MARKER);
  if (entries.length > 0 && !ours) {
    usage(`${demo} exists and was not made by this script; choose an empty or new folder`);
  }
  if (ours && !flags.has("--force")) {
    usage(`${demo} already holds a demo; pass --force to start it over (its profile goes too)`);
  }
  if (ours) {
    for (const entry of entries) rmSync(join(demo, entry), { recursive: true, force: true });
  }
}
mkdirSync(demo, { recursive: true });
writeFileSync(join(demo, MARKER), "Made by desktop/demo/seed.mjs. Safe to delete.\n");

const projects = join(demo, "projects");
const profile = join(demo, "profile");

/** Write `text` to `rel` under the demo folder (never outside it). */
function write(rel, text) {
  const path = resolve(demo, rel);
  if (!path.startsWith(demo)) throw new Error(`refusing to write outside the demo: ${rel}`);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, text.replace(/^\n/, ""));
  return path;
}

/** Copy a repository file or folder to `rel` under the demo folder. */
function copy(from, rel) {
  const to = resolve(demo, rel);
  if (!to.startsWith(demo)) throw new Error(`refusing to copy outside the demo: ${rel}`);
  mkdirSync(dirname(to), { recursive: true });
  cpSync(join(desktop, from), to, { recursive: true });
}

// A command that prints for a while, then exits, with a run summary.
const summaryOsc = (text) => `process.stdout.write("\\x1b]777;summary;" + ${JSON.stringify(text)} + "\\x07");`;

// ---- api: a small HTTP service ---------------------------------------------------

write("projects/api/chappa.yml", `
name: api
processes:
  serve:
    command: [node, server.mjs]
    auto_start: true
    auto_restart: false
    restart_when_changed: [server.mjs]
    env:
      PORT: "4100"
  test:
    command: [node, test.mjs]
    auto_start: false
    auto_restart: false
    restart_when_changed: []
    env: {}
`);
write("projects/api/server.mjs", `
// A stand-in API: answers /health and prints a request line now and then.
import { createServer } from "node:http";

const port = Number(process.env.PORT ?? 4100);
const routes = ["GET /orders", "GET /orders/42", "POST /orders", "GET /health", "GET /customers"];
createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify({ ok: true, path: req.url }));
}).listen(port, "127.0.0.1", () => console.log(\`api listening on http://localhost:\${port}\`));

let n = 0;
setInterval(() => {
  const route = routes[n++ % routes.length];
  const ms = 3 + ((n * 7) % 40);
  console.log(\`\${new Date().toISOString()}  \${route.padEnd(18)} 200  \${ms}ms\`);
}, 2500);
`);
write("projects/api/test.mjs", `
// A stand-in test run: prints results, leaves a summary, exits.
const suites = [
  ["orders", 9],
  ["customers", 6],
  ["health", 2],
  ["pagination", 4],
];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let total = 0;
for (const [name, count] of suites) {
  await sleep(400);
  total += count;
  console.log(\`  \\x1b[32m✓\\x1b[0m \${name.padEnd(12)} \${count} tests\`);
}
console.log(\`\\n\${total} passed, 0 failed\`);
${summaryOsc("**Tests passed**: 21 of 21 in 4 suites (orders, customers, health, pagination).")}
`);
write("projects/api/README.md", `
# api

A small order service used by the demo. \`serve\` runs it on port 4100;
\`test\` runs the suite.
`);

// ---- web: a front end ------------------------------------------------------------

write("projects/web/chappa.yml", `
name: web
processes:
  dev:
    command: [node, server.mjs]
    auto_start: true
    auto_restart: false
    restart_when_changed: []
    env:
      PORT: "4200"
  build:
    command: [node, build.mjs]
    auto_start: false
    auto_restart: false
    restart_when_changed: []
    env: {}
`);
write("projects/web/package.json", `
{
  "name": "web",
  "private": true,
  "version": "0.1.0",
  "scripts": { "dev": "node server.mjs", "build": "node build.mjs" }
}
`);
write("projects/web/server.mjs", `
// A stand-in dev server: serves one page and reports rebuilds.
import { createServer } from "node:http";

const port = Number(process.env.PORT ?? 4200);
createServer((req, res) => {
  res.setHeader("content-type", "text/html");
  res.end("<!doctype html><title>web</title><h1>Orders</h1>");
}).listen(port, "127.0.0.1", () => {
  console.log("  \\x1b[1mweb\\x1b[0m dev server");
  console.log(\`  ➜ Local: http://localhost:\${port}/\`);
});

const files = ["src/orders.ts", "src/app.ts", "src/styles.css", "src/cart.ts"];
let n = 0;
setInterval(() => {
  const file = files[n++ % files.length];
  console.log(\`\${new Date().toLocaleTimeString()} page reload \${file}\`);
}, 4000);
`);
write("projects/web/build.mjs", `
// A stand-in production build: prints the bundle sizes, exits.
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
console.log("building for production...");
await sleep(800);
for (const [file, size] of [["index.html", "0.4 kB"], ["assets/index.css", "6.1 kB"], ["assets/index.js", "48.2 kB"]]) {
  console.log(\`  dist/\${file.padEnd(20)} \${size}\`);
}
console.log("built in 0.9s");
${summaryOsc("Production build: 3 files, 54.7 kB.")}
`);

// ---- docs: a docs site and the tracker source -------------------------------------

write("projects/docs/chappa.yml", `
name: docs
processes:
  preview:
    command: [node, preview.mjs]
    auto_start: false
    auto_restart: false
    restart_when_changed: []
    env: {}
  check:
    command: [node, check.mjs]
    auto_start: false
    auto_restart: false
    restart_when_changed: []
    env: {}
  # A source: work items, pull requests and things that need attention,
  # served from the JSON files in tools/fixture/data.
  tracker:
    command: [node, tools/fixture/index.mjs]
    source: {}
    auto_start: true
    auto_restart: false
    restart_when_changed: []
    env: {}
`);
write("projects/docs/preview.mjs", `
// A stand-in docs preview server.
console.log("docs preview on http://localhost:4300");
setInterval(() => console.log(\`\${new Date().toLocaleTimeString()} rebuilt 12 pages\`), 6000);
`);
write("projects/docs/check.mjs", `
// A stand-in link check: prints, exits.
console.log("checking 12 pages, 148 links");
console.log("  guide/getting-started.md  ok");
console.log("  guide/orders.md           ok");
console.log("  api/endpoints.md          ok");
console.log("all links resolve");
${summaryOsc("Link check: 148 links on 12 pages, none broken.")}
`);
write("projects/docs/guide/getting-started.md", `
# Getting started

Run \`serve\` in the api project and \`dev\` in the web project, then open
http://localhost:4200.
`);

// The fixture source, copied so the project needs nothing outside the demo.
copy("sources/fixture/index.mjs", "projects/docs/tools/fixture/index.mjs");
copy("sources/lib/identity.mjs", "projects/docs/tools/lib/identity.mjs");
copy("sources/fixture/data", "projects/docs/tools/fixture/data");
{
  const dataDir = join(projects, "docs", "tools", "fixture", "data");
  const manifestPath = join(dataDir, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  manifest.source.label = "Tracker";
  // The stack below feeds the Environment view; the fixture's stand-in
  // environment is kept only on request.
  if (!flags.has("--fixture-environment")) delete manifest.views.environment;
  manifest.roots = [projects];
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
  // Point the work items' folders at the demo projects, so their agent and
  // terminal actions open somewhere real.
  const workPath = join(dataDir, "work.json");
  const work = readFileSync(workPath, "utf8")
    .replaceAll('"/srv/example/abc-1"', JSON.stringify(join(projects, "api")))
    .replaceAll('"/srv/example"', JSON.stringify(projects));
  writeFileSync(workPath, work);
}

// ---- stacks: the example stacks ---------------------------------------------------

/**
 * Copy an example stack into `projects/<name>`. The examples run the stack
 * runner with cargo from inside the repository; the demo lives elsewhere, so
 * it runs the built `chappa-stack` from PATH instead.
 */
function stackProject(example, name, files) {
  for (const file of files) {
    copy(join("stack", "examples", example, file), join("projects", name, file));
  }
  const yml = readFileSync(join(desktop, "stack", "examples", example, "chappa.yml"), "utf8");
  const runner = /^ {4}command: \[cargo, run, .*chappa-stack, --, serve\]$/m;
  if (!runner.test(yml)) throw new Error(`the ${example} example's runner command has changed; update seed.mjs`);
  const demoYml = yml
    .replace(/^# An example project[\s\S]*?\nname: .*$/m, `name: ${name}`)
    .replace(runner, "    command: [chappa-stack, serve]")
    .replace(/^ {2}# Built from this repository.*\n/m, "");
  write(join("projects", name, "chappa.yml"), demoYml);
}

// A stack run by its own scripts: two pretend services in two groups,
// needing nothing but node, so the Environment view has live rows without
// any container engine.
stackProject("script", "stack", ["scripts", ".gitignore"]);
// The compose example (Docker for db, podman for api); its components build
// from ../api and ../web, the demo's own projects.
if (flags.has("--docker-stack")) {
  stackProject("local", "compose-stack", ["stack", ".gitignore", "shared-checkouts-allow.txt"]);
}

// ---- the stand-in agent ------------------------------------------------------------

const agentScript = write("agent/demo-agent.mjs", `
// A scripted stand-in for an agent CLI: it prints a short session, raises a
// notification when it wants input, and never calls a model.
import { createInterface } from "node:readline";

const out = (s) => process.stdout.write(s);
const osc = (body) => out("\\x1b]" + body + "\\x07");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const dim = (s) => "\\x1b[2m" + s + "\\x1b[0m";
const bold = (s) => "\\x1b[1m" + s + "\\x1b[0m";
const tool = (name, arg) => out("  " + "\\x1b[36m●\\x1b[0m " + bold(name) + dim("(" + arg + ")") + "\\n");

async function say(lines, pause = 350) {
  for (const line of lines) {
    out(line + "\\n");
    await sleep(pause);
  }
}

const session = "demo-" + Date.now().toString(36);
osc("777;session;" + session);
out(bold("demo-agent") + dim(" 0.1 · scripted session, no model") + "\\n\\n");

const rl = createInterface({ input: process.stdin, output: process.stdout, terminal: true, prompt: "\\x1b[35m›\\x1b[0m " });
let turn = 0;
rl.prompt();
// One reply at a time, in the order the lines came.
let queue = Promise.resolve();
rl.on("line", (line) => {
  queue = queue.then(() => reply(line));
});
async function reply(line) {
  turn += 1;
  if (turn === 1) {
    await say([dim("Reading the request: ") + (line.trim() || "(empty)"), ""]);
    tool("read", "server.mjs");
    tool("read", "test.mjs");
    tool("search", "\\"/orders\\"");
    await say([
      "",
      "The orders route answers every request with the same body. Plan:",
      "  1. add a 404 for unknown order ids",
      "  2. cover it with a test in test.mjs",
      "",
      bold("Proceed with this plan?") + " (reply yes, or tell me what to change)",
    ]);
    // Rings in the app: a banner naming this agent and its project.
    osc("777;notify;demo-agent;Plan ready: proceed?");
  } else {
    tool("edit", "server.mjs");
    tool("edit", "test.mjs");
    tool("run", "node test.mjs");
    await say(["  22 passed, 0 failed", "", "Done. Unknown order ids now answer 404, with a test."]);
    osc("777;summary;Added a 404 for unknown order ids and a test for it (22 passing).");
    osc("777;notify;demo-agent;Finished: 22 tests passing");
  }
  out("\\n");
  rl.prompt();
}
rl.on("close", () => queue.then(() => process.exit(0)));
`);

// ---- the profile ---------------------------------------------------------------------

// The projects list, in rail order. No trust is recorded: opening each one
// shows the trust prompt first.
const projectNames = ["api", "web", "docs", "stack", ...(flags.has("--docker-stack") ? ["compose-stack"] : [])];
write(
  "profile/config/projects.json",
  JSON.stringify(
    projectNames.map((name, i) => ({ id: i + 1, name, path: join(projects, name) })),
    null,
    2,
  ) + "\n",
);
// The stand-in agent as a registered tool. Its session id is announced by
// the script itself, so the row can be resumed (resuming starts a new
// scripted session).
write(
  "profile/config/agent_tools.json",
  JSON.stringify(
    {
      next_id: 2,
      tools: [
        {
          id: 1,
          name: "demo-agent",
          tool_type: "custom",
          program: "node",
          args: [agentScript],
          runtime: { kind: "host" },
          env: {},
          enabled: true,
          max_busy: null,
          transport: "tty",
          resume_argv: [],
          prompt_argv: [],
          session_capture: { kind: "announced" },
        },
      ],
    },
    null,
    2,
  ) + "\n",
);

// ---- how to start it -------------------------------------------------------------------

const debugBin = join(desktop, "target", "debug");
console.log(`Demo written to ${demo}

Projects: ${projectNames.map((n) => join("projects", n)).join(", ")}
Profile:  ${profile}

The stack projects' sources run the stack runner from PATH. Build it once:
  cd ${desktop}
  cargo build -p chappa-stack

Then start the app against the demo profile, from ${desktop}:

  bash / zsh:
    CHAPPA_PROFILE_DIR="${profile}" PATH="${debugBin}:$PATH" npm run dev

  PowerShell:
    $env:CHAPPA_PROFILE_DIR = "${profile}"; $env:PATH = "${debugBin};$env:PATH"; npm run dev

  cmd:
    set "CHAPPA_PROFILE_DIR=${profile}" && set "PATH=${debugBin};%PATH%" && npm run dev

Quit any other running copy of the app first: only one instance can hold
the control surface's port.`);
