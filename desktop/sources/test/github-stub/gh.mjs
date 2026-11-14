#!/usr/bin/env node
// Stand-in for the gh CLI in the GitHub source's tests (GH_BIN points
// here). It answers `gh api graphql -f query=... [-f q=<search>]` with
// canned JSON from ./fixtures, picked by the search string, and never
// talks to the network.
//
//   GH_STUB_MODE=logged-out   behave like gh with no login (exit 4)
//   GH_STUB_FAIL=a,b          fail the named fixtures (exit 1), e.g. requested
//   GH_STUB_DELAY_MS=n        wait before answering
//   GH_STUB_LOG=<file>        append the fixture name of every call
//   GH_STUB_TOTAL=name:n      report n matches for that search (a capped search)
//
// In fixture files the string "__DAYS_AGO_<n>__" becomes the ISO time n
// days before now, so "recently merged" stays recent whenever tests run.
import { appendFileSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const vars = {};
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === "-f" || argv[i] === "-F") {
    const [k, ...v] = argv[++i].split("=");
    vars[k] = v.join("=");
  }
}

if (process.env.GH_STUB_MODE === "logged-out") {
  process.stderr.write("To get started with GitHub CLI, please run:  gh auth login\n");
  process.exit(4);
}
if (argv[0] !== "api" || argv[1] !== "graphql" || !vars.query) {
  process.stderr.write(`stub gh: unexpected arguments ${JSON.stringify(argv)}\n`);
  process.exit(2);
}

const q = vars.q ?? "";
const name =
  !q ? "viewer" :
  q.includes("review-requested:@me") ? "requested" :
  q.includes("reviewed-by:@me") ? "reviewed" :
  q.includes("commenter:@me") ? "commented" :
  q.includes("is:closed") ? "mine-closed" :
  q.includes("author:@me") ? "mine-open" : "unknown";

if (process.env.GH_STUB_LOG) appendFileSync(process.env.GH_STUB_LOG, `${name}\n`);

const answer = () => {
  if ((process.env.GH_STUB_FAIL ?? "").split(",").includes(name)) {
    process.stderr.write("gh: Something went wrong while executing your query.\n");
    process.exit(1);
  }
  const text = readFileSync(join(here, "fixtures", `${name}.json`), "utf8").replace(
    /"__DAYS_AGO_(\d+)__"/g,
    (_, n) => JSON.stringify(new Date(Date.now() - Number(n) * 86_400_000).toISOString()),
  );
  const [totalOf, total] = (process.env.GH_STUB_TOTAL ?? "").split(":");
  if (totalOf === name) {
    const body = JSON.parse(text);
    body.data.search.issueCount = Number(total);
    process.stdout.write(JSON.stringify(body));
    return;
  }
  process.stdout.write(text);
};
const delay = Number(process.env.GH_STUB_DELAY_MS ?? 0);
if (delay > 0) setTimeout(answer, delay); else answer();
