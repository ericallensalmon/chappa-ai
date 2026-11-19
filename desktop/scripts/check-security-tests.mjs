#!/usr/bin/env node
// Fails unless every test named in desktop/SECURITY_TESTS ran and passed.
//
// The list holds the negative controls of the app's security boundaries: a
// test that was deleted, renamed, skipped or filtered out would otherwise
// leave a guard untested without anyone noticing. Removing an entry is a
// reviewed change to the list itself.
//
//   node scripts/check-security-tests.mjs --suite rust --os linux cargo-test.log
//   node scripts/check-security-tests.mjs --suite ui vitest.json
//   node scripts/check-security-tests.mjs --suite node node-test.log
//
// The list: `[rust]`, `[ui]` and `[node]` sections, one test name per line,
// `#` comments and blank lines ignored. A Rust name is a test function's
// name (its module path may be given too: the last segments must match). A
// trailing `@unix`, `@windows`, `@linux` or `@macos` limits a Rust entry to
// those builds (several may be given). A UI name is the vitest test's title
// or its full name; a node name is the test's name.
//
// Inputs: `cargo test` output (the `test <path> ... ok` lines), vitest's
// JSON reporter file, or `node --test` output (TAP `ok N - name` lines or
// the spec reporter's `✔ name` lines).

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export function parseList(text) {
  const sections = { rust: [], ui: [], node: [] };
  let current = null;
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) return;
    const header = line.match(/^\[(rust|ui|node)\]$/);
    if (header) {
      current = header[1];
      return;
    }
    if (current === null) throw new Error(`line ${i + 1}: a test name before any [section]`);
    const tags = [];
    let name = line;
    for (;;) {
      const m = name.match(/^(.*\S)\s+@(unix|windows|linux|macos)$/);
      if (!m) break;
      name = m[1];
      tags.unshift(m[2]);
    }
    sections[current].push({ name, tags, line: i + 1 });
  });
  return sections;
}

/** Whether an entry applies to `os` (`linux`, `macos` or `windows`). */
export function appliesTo(entry, os) {
  if (entry.tags.length === 0) return true;
  return entry.tags.some((t) => t === os || (t === "unix" && os !== "windows"));
}

/** `cargo test` output: test path → outcome (`ok`, `FAILED`, `ignored`). */
export function parseCargo(text) {
  const out = new Map();
  for (const line of text.split(/\r?\n/)) {
    const m = line.match(/^test (\S+) \.\.\. (ok|FAILED|ignored)\b/);
    if (m) {
      const prev = out.get(m[1]);
      // The same path in two crates' binaries: any failure wins.
      if (prev !== "FAILED") out.set(m[1], m[2]);
    }
  }
  return out;
}

/** vitest JSON reporter output: title and full name → status. */
export function parseVitest(text) {
  const out = new Map();
  const report = JSON.parse(text);
  for (const file of report.testResults ?? []) {
    for (const t of file.assertionResults ?? []) {
      for (const key of [t.title, t.fullName]) {
        if (key && out.get(key) !== "failed") out.set(key, t.status);
      }
    }
  }
  return out;
}

/** `node --test` output: test name → `ok` / `not ok`. */
export function parseNode(text) {
  const out = new Map();
  for (const line of text.split(/\r?\n/)) {
    let m = line.match(/^\s*(not ok|ok) \d+ - (.+?)(\s+#.*)?$/);
    if (m) {
      if (out.get(m[2]) !== "not ok") out.set(m[2], m[1]);
      continue;
    }
    m = line.match(/^\s*(✔|✖) (.+?)(\s+\([\d.]+m?s\))?$/);
    if (m && out.get(m[2]) !== "not ok") out.set(m[2], m[1] === "✔" ? "ok" : "not ok");
  }
  return out;
}

function rustOutcome(results, name) {
  const want = name.split("::");
  let found = null;
  for (const [path, outcome] of results) {
    const got = path.split("::");
    const tail = got.slice(-want.length);
    if (tail.length === want.length && tail.every((seg, i) => seg === want[i])) {
      if (outcome === "FAILED") return "FAILED";
      if (found !== "ok") found = outcome;
    }
  }
  return found;
}

/** The problems with `results` against the list's entries for `suite`. */
export function check(list, suite, results, os) {
  const problems = [];
  for (const entry of list[suite]) {
    if (suite === "rust" && !appliesTo(entry, os)) continue;
    let outcome;
    if (suite === "rust") outcome = rustOutcome(results, entry.name);
    else outcome = results.get(entry.name) ?? null;
    const passed = outcome === "ok" || outcome === "passed";
    if (!passed) {
      const why = outcome === null ? "did not run (missing, renamed or filtered out)" : `did not pass (${outcome})`;
      problems.push(`${entry.name}: ${why} [SECURITY_TESTS line ${entry.line}]`);
    }
  }
  return problems;
}

function hostOs() {
  if (process.platform === "win32") return "windows";
  if (process.platform === "darwin") return "macos";
  return "linux";
}

function main(argv) {
  let suite = null;
  let os = hostOs();
  let listPath = join(here, "..", "SECURITY_TESTS");
  const inputs = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--suite") suite = argv[++i];
    else if (a === "--os") os = argv[++i];
    else if (a === "--list") listPath = argv[++i];
    else inputs.push(a);
  }
  if (!["rust", "ui", "node"].includes(suite) || inputs.length === 0) {
    console.error("usage: check-security-tests.mjs --suite rust|ui|node [--os linux|macos|windows] [--list FILE] OUTPUT...");
    return 2;
  }
  const list = parseList(readFileSync(listPath, "utf8"));
  const parse = { rust: parseCargo, ui: parseVitest, node: parseNode }[suite];
  const results = new Map();
  for (const input of inputs) {
    for (const [k, v] of parse(readFileSync(input, "utf8"))) {
      if (!results.has(k) || v === "FAILED" || v === "failed" || v === "not ok") results.set(k, v);
    }
  }
  const problems = check(list, suite, results, os);
  const counted = list[suite].filter((e) => suite !== "rust" || appliesTo(e, os)).length;
  if (problems.length > 0) {
    console.error(`${problems.length} of ${counted} required ${suite} security tests did not run and pass:`);
    for (const p of problems) console.error(`  ${p}`);
    return 1;
  }
  console.log(`all ${counted} required ${suite} security tests ran and passed (${os})`);
  return 0;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  process.exitCode = main(process.argv.slice(2));
}
