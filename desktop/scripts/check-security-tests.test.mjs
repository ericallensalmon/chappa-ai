import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { appliesTo, check, parseCargo, parseList, parseNode, parseVitest } from "./check-security-tests.mjs";

const LIST = `
# comment
[rust]
a_guard_refuses
mod_a::shared_name
only_here @unix
only_there @windows
[ui]
a dialog focuses Cancel
[node]
a relative path is refused
`;

test("the checker passes only when every listed test ran and passed", () => {
  const list = parseList(LIST);
  assert.deepEqual(list.rust.map((e) => e.name), ["a_guard_refuses", "mod_a::shared_name", "only_here", "only_there"]);
  assert.ok(appliesTo(list.rust[2], "macos") && !appliesTo(list.rust[2], "windows"));
  const cargo = parseCargo(
    [
      "test tests::a_guard_refuses ... ok",
      "test mod_a::shared_name ... ok",
      "test mod_b::shared_name ... FAILED",
      "test only_here ... ok",
      "test unrelated ... ok",
    ].join("\n"),
  );
  assert.deepEqual(check(list, "rust", cargo, "linux"), []);
  // Missing on Windows: the Windows-only entry did not run.
  const win = check(list, "rust", cargo, "windows");
  assert.equal(win.length, 1);
  assert.match(win[0], /only_there: did not run/);
  // A failure, an ignored test and a deleted one are all problems.
  const broken = parseCargo("test tests::a_guard_refuses ... ignored\ntest mod_a::shared_name ... FAILED\n");
  const problems = check(list, "rust", broken, "linux");
  assert.equal(problems.length, 3);
  assert.match(problems[0], /did not pass \(ignored\)/);
  assert.match(problems[1], /did not pass \(FAILED\)/);
  assert.match(problems[2], /only_here: did not run/);

  const vitest = parseVitest(
    JSON.stringify({ testResults: [{ assertionResults: [{ title: "a dialog focuses Cancel", fullName: "dialog a dialog focuses Cancel", status: "passed" }] }] }),
  );
  assert.deepEqual(check(list, "ui", vitest, "linux"), []);
  assert.equal(check(list, "ui", new Map(), "linux").length, 1);

  const tap = parseNode("TAP version 13\n# Subtest: x\nok 1 - a relative path is refused\n  ---\n");
  assert.deepEqual(check(list, "node", tap, "linux"), []);
  assert.equal(check(list, "node", parseNode("not ok 1 - a relative path is refused\n"), "linux").length, 1);
  assert.deepEqual(check(list, "node", parseNode("✔ a relative path is refused (1.2ms)\n"), "linux"), []);
});

test("the repository's list parses", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const list = parseList(readFileSync(join(here, "..", "SECURITY_TESTS"), "utf8"));
  assert.ok(list.rust.length > 40);
  assert.ok(list.ui.length > 5);
  assert.ok(list.node.length > 0);
  const names = list.rust.map((e) => e.name);
  assert.equal(new Set(names).size, names.length, "no duplicate entries");
});
