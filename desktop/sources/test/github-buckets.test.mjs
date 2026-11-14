// The GitHub source's pure mapping: votes, mergeable, checks, threads,
// issue mentions, and the bucket + reasons table, from hand-written
// GraphQL-shaped inputs.
import test from "node:test";
import assert from "node:assert/strict";
import { bucketFor, countThreads, extractWorkKeys, mapChecks, mapMergeable, mapVote, toPr, RECENT_DAYS } from "../github/buckets.mjs";

const NOW = Date.parse("2026-03-10T12:00:00Z");
const daysAgo = (n) => new Date(NOW - n * 86_400_000).toISOString();

const run = (name, status, conclusion) => ({ __typename: "CheckRun", name, status, conclusion });
const ctxStatus = (context, state) => ({ __typename: "StatusContext", context, state });
const thread = (resolved, starter, last) => ({ isResolved: resolved, first: { nodes: [{ author: { login: starter } }] }, last: { nodes: [{ author: { login: last } }] } });
const review = (login, state, name) => ({ state, author: { login, name } });
const reqUser = (login, name) => ({ requestedReviewer: { __typename: "User", login, name } });
const reqTeam = (org, slug) => ({ requestedReviewer: { __typename: "Team", slug, name: slug, organization: { login: org } } });

function node(o = {}) {
  return {
    id: "PR_1", number: o.number ?? 1, title: "A change", url: "https://github.com/owner/repo/pull/1",
    body: o.body ?? "", isDraft: o.isDraft ?? false, state: o.state ?? "OPEN",
    closedAt: o.closedAt ?? null, mergedAt: o.mergedAt ?? null,
    headRefName: o.head ?? "change", baseRefName: "main", repository: { nameWithOwner: "owner/repo" },
    author: o.author ?? { login: "alex", name: "Alex" },
    reviewDecision: o.decision ?? null, mergeable: o.mergeable ?? "MERGEABLE",
    reviewRequests: { nodes: o.requests ?? [] }, latestReviews: { nodes: o.reviews ?? [] },
    reviewThreads: { nodes: o.threads ?? [] },
    commits: { nodes: [{ commit: { statusCheckRollup: o.checks === undefined ? null : { contexts: { nodes: o.checks } } } }] },
  };
}
const mine = (o) => toPr(node(o), { viewer: "alex", role: "mine", via: new Set(), now: NOW });
const reviewing = (o, via = []) => toPr(node({ author: { login: "sam", name: "Sam" }, ...o }), { viewer: "alex", role: "reviewing", via: new Set(via), now: NOW });
const green = [run("build", "COMPLETED", "SUCCESS")];

// ---- votes, mergeable ---------------------------------------------------

test("GitHub review states map onto the vote enum", () => {
  assert.equal(mapVote("APPROVED"), "approved");
  assert.equal(mapVote("CHANGES_REQUESTED"), "rejected");
  assert.equal(mapVote("COMMENTED"), "none");
  assert.equal(mapVote("PENDING"), "none");
  assert.equal(mapVote("DISMISSED"), "none");
  assert.equal(mapVote(undefined), "none");
});

test("mergeable maps to yes / conflicts / unknown, and null once closed", () => {
  assert.equal(mapMergeable("MERGEABLE"), "yes");
  assert.equal(mapMergeable("CONFLICTING"), "conflicts");
  assert.equal(mapMergeable("UNKNOWN"), "unknown");
  assert.equal(mapMergeable(null), "unknown");
  assert.equal(mapMergeable("CONFLICTING", false), null);
});

// ---- checks ----------------------------------------------------------------

test("no rollup or no contexts means no checks", () => {
  assert.deepEqual(mapChecks(null), { state: "none", failing: [], pending: [] });
  assert.deepEqual(mapChecks({ contexts: { nodes: [] } }), { state: "none", failing: [], pending: [] });
});

test("a failing check run or status makes the state failing, with names", () => {
  const c = mapChecks({ contexts: { nodes: [run("build", "COMPLETED", "FAILURE"), run("e2e", "QUEUED", null), ctxStatus("ci/legacy", "ERROR"), run("lint", "COMPLETED", "SUCCESS")] } });
  assert.equal(c.state, "failing");
  assert.deepEqual(c.failing, ["build", "ci/legacy"]);
  assert.deepEqual(c.pending, ["e2e"]);
});

test("timed out, cancelled and action-required runs count as failing", () => {
  for (const conclusion of ["TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"]) {
    assert.equal(mapChecks({ contexts: { nodes: [run("x", "COMPLETED", conclusion)] } }).state, "failing", conclusion);
  }
});

test("pending without failures is pending; passing, neutral and skipped are passing", () => {
  assert.equal(mapChecks({ contexts: { nodes: [run("a", "IN_PROGRESS", null), ctxStatus("b", "SUCCESS")] } }).state, "pending");
  assert.equal(mapChecks({ contexts: { nodes: [ctxStatus("b", "EXPECTED")] } }).state, "pending");
  const ok = mapChecks({ contexts: { nodes: [run("a", "COMPLETED", "SUCCESS"), run("b", "COMPLETED", "NEUTRAL"), run("c", "COMPLETED", "SKIPPED"), ctxStatus("d", "SUCCESS")] } });
  assert.deepEqual(ok, { state: "passing", failing: [], pending: [] });
});

test("a stale run is ignored and a name counts once, at its worst", () => {
  assert.equal(mapChecks({ contexts: { nodes: [run("a", "COMPLETED", "STALE")] } }).state, "none");
  const c = mapChecks({ contexts: { nodes: [run("a", "COMPLETED", "SUCCESS"), run("a", "COMPLETED", "FAILURE")] } });
  assert.deepEqual(c.failing, ["a"]);
});

// ---- threads ---------------------------------------------------------------

test("on my PR, every unresolved thread whose last word is not mine needs me", () => {
  const t = { nodes: [thread(false, "sam", "sam"), thread(false, "alex", "sam"), thread(false, "sam", "alex"), thread(true, "sam", "sam")] };
  assert.deepEqual(countThreads(t, "alex", true), { active: 3, resolved: 1, needsMe: 2 });
});

test("on someone else's PR, only unresolved threads I started and they answered need me", () => {
  const t = { nodes: [thread(false, "alex", "sam"), thread(false, "sam", "sam"), thread(false, "alex", "alex"), thread(true, "alex", "sam")] };
  assert.deepEqual(countThreads(t, "alex", false), { active: 3, resolved: 1, needsMe: 1 });
});

test("login comparison ignores case, and an empty thread list is all zeros", () => {
  assert.equal(countThreads({ nodes: [thread(false, "Alex", "Sam")] }, "alex", false).needsMe, 1);
  assert.deepEqual(countThreads(null, "alex", true), { active: 0, resolved: 0, needsMe: 0 });
});

// ---- workKeys ---------------------------------------------------------------

test("issue mentions in the body become owner/repo#N keys", () => {
  const keys = extractWorkKeys({ repo: "owner/repo", number: 40, body: "Fixes #12, closes owner/repo#13.\nSee #14 too.", branch: "main" });
  assert.deepEqual(keys.sort(), ["owner/repo#12", "owner/repo#13", "owner/repo#14"]);
});

test("mentions of other repositories, URL fragments and the PR itself are skipped", () => {
  const body = "Relates to other/lib#5 and https://example.com/page#7 and &#39; and #40 and https://github.com/other/lib/issues/8";
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 40, body, branch: "" }), []);
});

test("same-repository issue URLs count, case-insensitively", () => {
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "https://github.com/Owner/Repo/issues/21", branch: "" }), ["owner/repo#21"]);
});

test("branch names that start a segment with the number, or say issue-N, count", () => {
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "", branch: "12-login-retry" }), ["owner/repo#12"]);
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "", branch: "fix/34-crash" }), ["owner/repo#34"]);
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "", branch: "feature/issue-56" }), ["owner/repo#56"]);
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "", branch: "v2-cleanup" }), []);
});

test("a key found twice is listed once", () => {
  assert.deepEqual(extractWorkKeys({ repo: "owner/repo", number: 1, body: "Fixes #12", branch: "12-x" }), ["owner/repo#12"]);
});

// ---- buckets: mine -----------------------------------------------------------

test("mine: failing checks put it in action, with the failing names", () => {
  const pr = mine({ decision: "APPROVED", checks: [run("build", "COMPLETED", "FAILURE"), run("lint", "COMPLETED", "FAILURE")] });
  assert.equal(pr.bucket, "action");
  assert.deepEqual(pr.reasons, ["2 failing checks: build, lint"]);
});

test("mine: one failing check is singular", () => {
  assert.deepEqual(mine({ checks: [run("lint", "COMPLETED", "FAILURE")] }).reasons, ["1 failing check: lint"]);
});

test("mine: conflicts put it in action", () => {
  const pr = mine({ decision: "APPROVED", mergeable: "CONFLICTING", checks: green });
  assert.equal(pr.bucket, "action");
  assert.deepEqual(pr.reasons, ["conflicts with main"]);
  assert.equal(pr.mergeable, "conflicts");
});

test("mine: a change request puts it in action, naming the reviewer", () => {
  const pr = mine({ decision: "CHANGES_REQUESTED", reviews: [review("sam", "CHANGES_REQUESTED", "Sam")], checks: green });
  assert.equal(pr.bucket, "action");
  assert.deepEqual(pr.reasons, ["changes requested by Sam"]);
  assert.equal(pr.reviewers.find((r) => r.id === "sam").vote, "rejected");
});

test("mine: threads to answer put it in action", () => {
  const pr = mine({ checks: green, threads: [thread(false, "sam", "sam")] });
  assert.equal(pr.bucket, "action");
  assert.deepEqual(pr.reasons, ["1 thread to answer"]);
});

test("mine: every blocker is listed, in a fixed order", () => {
  const pr = mine({ mergeable: "CONFLICTING", reviews: [review("sam", "CHANGES_REQUESTED", "Sam")], checks: [run("build", "COMPLETED", "FAILURE")], threads: [thread(false, "sam", "sam"), thread(false, "sam", "sam")] });
  assert.deepEqual(pr.reasons, ["1 failing check: build", "changes requested by Sam", "conflicts with main", "2 threads to answer"]);
});

test("mine: a draft is draft even when blocked, and lists the blockers after", () => {
  const pr = mine({ isDraft: true, checks: [run("build", "COMPLETED", "FAILURE")] });
  assert.equal(pr.state, "draft");
  assert.equal(pr.bucket, "draft");
  assert.deepEqual(pr.reasons, ["still a draft", "1 failing check: build"]);
});

test("mine: approved and green is ready", () => {
  const pr = mine({ decision: "APPROVED", reviews: [review("sam", "APPROVED", "Sam")], checks: green });
  assert.equal(pr.bucket, "ready");
  assert.deepEqual(pr.reasons, ["approved by Sam, checks passing"]);
});

test("mine: approved with no checks at all is ready", () => {
  assert.equal(mine({ decision: "APPROVED" }).bucket, "ready");
});

test("mine: an approval counts when the branch has no review rule", () => {
  assert.equal(mine({ decision: null, reviews: [review("sam", "APPROVED", "Sam")], checks: green }).bucket, "ready");
});

test("mine: approved but checks pending is waiting, naming the pending checks", () => {
  const pr = mine({ decision: "APPROVED", checks: [run("build", "COMPLETED", "SUCCESS"), run("e2e", "IN_PROGRESS", null)] });
  assert.equal(pr.bucket, "waiting");
  assert.deepEqual(pr.reasons, ["1 pending check: e2e"]);
});

test("mine: a required review not given is waiting, naming who is asked", () => {
  const pr = mine({ decision: "REVIEW_REQUIRED", requests: [reqUser("sam", "Sam"), reqTeam("owner", "docs")], checks: green });
  assert.equal(pr.bucket, "waiting");
  assert.deepEqual(pr.reasons, ["required review not given", "waiting on Sam, owner/docs"]);
  assert.deepEqual(pr.reviewers.map((r) => [r.name, Boolean(r.required), Boolean(r.isGroup)]), [["Sam", true, false], ["owner/docs", false, true]]);
});

test("mine: no reviews and no rule is waiting with a plain reason", () => {
  assert.deepEqual(mine({ checks: green }).reasons, ["no approving review yet"]);
});

test("mine: merged lately is recent; closed lately is recent; long ago is dropped", () => {
  const merged = mine({ state: "MERGED", mergedAt: daysAgo(2), closedAt: daysAgo(2) });
  assert.equal(merged.bucket, "recent");
  assert.equal(merged.state, "merged");
  assert.deepEqual(merged.reasons, ["merged into main"]);
  assert.equal(merged.closedUtc, daysAgo(2));
  assert.equal(merged.mergeable, null);
  assert.equal(merged.checks, null);
  const closed = mine({ state: "CLOSED", closedAt: daysAgo(RECENT_DAYS - 1) });
  assert.equal(closed.bucket, "recent");
  assert.deepEqual(closed.reasons, ["closed without merging"]);
  assert.equal(mine({ state: "CLOSED", closedAt: daysAgo(RECENT_DAYS + 1) }), null);
  assert.equal(mine({ state: "MERGED", mergedAt: null, closedAt: null }), null);
});

test("mine: me is null and I am not listed among the reviewers", () => {
  const pr = mine({ reviews: [review("alex", "COMMENTED", "Alex")] });
  assert.equal(pr.me, null);
  assert.equal(pr.reviewers.length, 0);
  assert.equal(pr.author.isMe, true);
});

// ---- buckets: reviewing ------------------------------------------------------

test("reviewing: a direct request I have not answered is vote", () => {
  const pr = reviewing({ requests: [reqUser("alex", "Alex")] });
  assert.equal(pr.bucket, "vote");
  assert.deepEqual(pr.reasons, ["your review is requested"]);
  assert.deepEqual(pr.me, { required: true, vote: "none" });
  assert.equal(pr.author.isMe, undefined);
});

test("reviewing: a request that came back after my vote is vote again", () => {
  const pr = reviewing({ requests: [reqUser("alex", "Alex")], reviews: [review("alex", "CHANGES_REQUESTED", "Alex")] });
  assert.equal(pr.bucket, "vote");
  assert.deepEqual(pr.reasons, ["your review is requested again"]);
  assert.equal(pr.me.vote, "rejected");
});

test("reviewing: a request only through a team is group", () => {
  const pr = reviewing({ requests: [reqTeam("owner", "maintainers")] });
  assert.equal(pr.bucket, "group");
  assert.deepEqual(pr.reasons, ["review requested from owner/maintainers"]);
  assert.deepEqual(pr.me, { required: false, vote: "none" });
  assert.equal(pr.reviewers[0].isGroup, true);
});

test("reviewing: a direct request beats a team request", () => {
  assert.equal(reviewing({ requests: [reqTeam("owner", "maintainers"), reqUser("alex", "Alex")] }).bucket, "vote");
});

test("reviewing: a reply in a thread I started is replied, before anything else", () => {
  const pr = reviewing({ requests: [reqUser("alex", "Alex")], threads: [thread(false, "alex", "sam")] });
  assert.equal(pr.bucket, "replied");
  assert.deepEqual(pr.reasons, ["new reply in your thread"]);
  const two = reviewing({ threads: [thread(false, "alex", "sam"), thread(false, "alex", "sam")] });
  assert.deepEqual(two.reasons, ["new replies in 2 of your threads"]);
});

test("reviewing: a resolved thread or one where I spoke last is not a reply", () => {
  assert.equal(reviewing({ threads: [thread(true, "alex", "sam")] }).bucket, "mythreads");
  assert.equal(reviewing({ threads: [thread(false, "alex", "alex")] }).bucket, "mythreads");
});

test("reviewing: approved or rejected by me is voted", () => {
  const a = reviewing({ reviews: [review("alex", "APPROVED", "Alex")] });
  assert.equal(a.bucket, "voted");
  assert.deepEqual(a.reasons, ["you approved"]);
  const r = reviewing({ reviews: [review("alex", "CHANGES_REQUESTED", "Alex")] });
  assert.equal(r.bucket, "voted");
  assert.deepEqual(r.reasons, ["you requested changes"]);
  assert.equal(r.me.vote, "rejected");
});

test("reviewing: a comment-only review or a found-by-comment search is mythreads", () => {
  const c = reviewing({ reviews: [review("alex", "COMMENTED", "Alex")] });
  assert.equal(c.bucket, "mythreads");
  assert.deepEqual(c.reasons, ["you commented; nothing new"]);
  assert.equal(reviewing({}, ["commented"]).bucket, "mythreads");
});

test("reviewing: my unsubmitted review is not a vote", () => {
  const pr = reviewing({ requests: [reqUser("alex", "Alex")], reviews: [review("alex", "PENDING", "Alex")] });
  assert.equal(pr.bucket, "vote");
  assert.equal(pr.me.vote, "none");
});

test("reviewing: a closed PR is not listed", () => {
  assert.equal(reviewing({ state: "MERGED", mergedAt: daysAgo(1) }), null);
});

// ---- the row ---------------------------------------------------------------

test("a row carries forge, repo, number, branches, link actions and a refresh", () => {
  const pr = mine({ body: "Fixes #3", head: "3-x" });
  assert.equal(pr.forge, "github");
  assert.equal(pr.repo, "owner/repo");
  assert.equal(pr.number, "#1");
  assert.equal(pr.sourceBranch, "3-x");
  assert.equal(pr.targetBranch, "main");
  assert.deepEqual(pr.workKeys, ["owner/repo#3"]);
  assert.deepEqual(pr.actions.map((a) => [a.kind, a.id]), [["link", "open"], ["link", "files"], ["link", "checks"], ["endpoint", "refresh"]]);
  for (const a of pr.actions.filter((x) => x.kind === "link")) assert.match(a.url, /^https:\/\/github\.com\//);
  assert.equal(pr.actions.find((a) => a.kind === "endpoint").mutates, false);
});

test("bucketFor takes plain facts as well", () => {
  assert.deepEqual(bucketFor({ role: "reviewing", state: "open", now: NOW, threads: { needsMe: 0 }, me: { viaTeams: ["o/t"] } }).bucket, "group");
  assert.equal(bucketFor({ role: "mine", state: "closed", now: NOW, closedUtc: "not a date" }), null);
});
