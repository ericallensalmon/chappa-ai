// `npm run build`: the installable bundles, with the `chappa-ai-mcp` server
// inside them beside the app's own executable, where the app looks for it
// (agents reach the app through it).
//
// It builds the server first, then runs `tauri build` with the arguments
// given here (`npm run build -- --bundles dmg`) plus a config naming the
// built server for this platform's bundles:
// - macOS: Contents/MacOS/chappa-ai-mcp in the .app (and so in the .dmg);
// - Linux: /usr/bin/chappa-ai-mcp in the .deb, .rpm and AppImage;
// - Windows: chappa-ai-mcp.exe in the install folder (an installer resource).
//
// Not a Tauri sidecar (`bundle.externalBin`): the framework's build script
// copies sidecars to `<OUT_DIR>/../../..`, which with Cargo's current build
// directory layout is the build folder, where a `chappa-ai-mcp` directory
// already exists, so the app crate would not compile.

import { execFileSync, spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const desktop = join(dirname(fileURLToPath(import.meta.url)), "..");
const NAME = "chappa-ai-mcp";
const args = process.argv.slice(2);

/** The value after `--target`/`-t` (or `--target=`), else null. */
function targetArg(list) {
  for (let i = 0; i < list.length; i++) {
    const a = list[i];
    if (a === "--target" || a === "-t") return list[i + 1] ?? null;
    if (a.startsWith("--target=")) return a.slice("--target=".length);
  }
  return null;
}

function hostTriple() {
  const info = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const line = info.split(/\r?\n/).find((l) => l.startsWith("host: "));
  if (!line) throw new Error("rustc -vV printed no host triple");
  return line.slice("host: ".length).trim();
}

const target = targetArg(args);
const triple = target ?? hostTriple();
if (triple.startsWith("universal-")) {
  throw new Error(`${triple}: build the server per architecture first; not supported here`);
}
const debug = args.includes("--debug") || args.includes("-d");

// Cargo's progress goes to stderr; stdout carries one JSON message per line,
// among them the artifact naming the built executable.
const cargo = ["build", "-p", NAME, "--bin", NAME, "--message-format=json-render-diagnostics"];
if (!debug) cargo.push("--release");
if (target) cargo.push("--target", target);
const out = execFileSync("cargo", cargo, {
  cwd: desktop,
  encoding: "utf8",
  stdio: ["ignore", "pipe", "inherit"],
  maxBuffer: 64 * 1024 * 1024,
});
let server = null;
for (const line of out.split(/\r?\n/)) {
  if (!line.startsWith("{")) continue;
  const msg = JSON.parse(line);
  if (msg.reason === "compiler-artifact" && msg.target?.name === NAME && msg.executable) {
    server = msg.executable;
  }
}
if (!server) throw new Error(`cargo reported no ${NAME} executable`);

let bundle;
if (triple.includes("apple-darwin")) {
  bundle = { macOS: { files: { [`MacOS/${NAME}`]: server } } };
} else if (triple.includes("windows")) {
  bundle = { resources: { [server]: `${NAME}.exe` } };
} else {
  const files = { [`/usr/bin/${NAME}`]: server };
  bundle = { linux: { deb: { files }, rpm: { files }, appimage: { files } } };
}
console.log(`bundling ${server}`);

const cli = join(desktop, "node_modules", "@tauri-apps", "cli", "tauri.js");
const run = spawnSync(
  process.execPath,
  [cli, "build", "--config", JSON.stringify({ bundle }), ...args],
  { cwd: desktop, stdio: "inherit" },
);
if (run.error) throw run.error;
process.exit(run.status ?? 1);
