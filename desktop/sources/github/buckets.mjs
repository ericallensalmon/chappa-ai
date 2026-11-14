// Pure mapping from GitHub's GraphQL pull request shape to the app's Pr
// shape, including the bucket and the reasons. No I/O and no clock: the
// caller passes `now`, so every boundary is testable with hand-written
// inputs.

/** Merged or closed PRs older than this are dropped, not shown as recent. */
export const RECENT_DAYS = 14;
const DAY_MS = 24 * 60 * 60 * 1000;

const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
const lower = (s) => (typeof s === "string" ? s.toLowerCase() : "");
const nodes = (conn) => (Array.isArray(conn?.nodes) ? conn.nodes.filter(Boolean) : []);

/**
 * A GitHub review state as the contract's vote. GitHub has two real votes
 * (approve, request changes); a comment-only review, a review the viewer
 * has started but not submitted, and a dismissed review are not votes.
 */
export function mapVote(state) {
  switch (state) {
    case "APPROVED": return "approved";
    case "CHANGES_REQUESTED": return "rejected";
    case "COMMENTED":
    case "PENDING":
    case "DISMISSED":
    default: return "none";
  }
}

/** GitHub's mergeable enum; null when the PR is no longer open. */
export function mapMergeable(value, open = true) {
  if (!open) return null;
  if (value === "MERGEABLE") return "yes";
  if (value === "CONFLICTING") return "conflicts";
  return "unknown";
}

const RUN_FAILED = new Set(["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"]);
const RUN_PASSED = new Set(["SUCCESS", "NEUTRAL", "SKIPPED"]);

/**
 * The head commit's check runs and commit statuses as `checks`. Each
 * context counts once by name; a failing one makes the whole state
 * failing, else a pending one makes it pending. A stale check run (GitHub
 * marks it so when a newer run supersedes it) is ignored. No rollup at
 * all means the repository runs no checks: state "none".
 */
export function mapChecks(rollup) {
  const contexts = nodes(rollup?.contexts);
  if (!rollup || contexts.length === 0) return { state: "none", failing: [], pending: [] };
  const rank = { pass: 0, pending: 1, fail: 2 };
  const verdict = new Map();
  for (const c of contexts) {
    let name; let v;
    if (c.__typename === "StatusContext" || ("context" in c && !("conclusion" in c))) {
      name = c.context;
      v = c.state === "SUCCESS" ? "pass" : c.state === "FAILURE" || c.state === "ERROR" ? "fail" : "pending";
    } else {
      name = c.name;
      if (c.status !== "COMPLETED") v = "pending";
      else if (RUN_FAILED.has(c.conclusion)) v = "fail";
      else if (RUN_PASSED.has(c.conclusion)) v = "pass";
      else if (c.conclusion === "STALE") continue;
      else v = "pending";
    }
    if (!name) continue;
    const prev = verdict.get(name);
    if (!prev || rank[v] > rank[prev]) verdict.set(name, v);
  }
  const failing = [...verdict].filter(([, v]) => v === "fail").map(([n]) => n);
  const pending = [...verdict].filter(([, v]) => v === "pending").map(([n]) => n);
  const state = verdict.size === 0 ? "none" : failing.length ? "failing" : pending.length ? "pending" : "passing";
  return { state, failing, pending };
}

/**
 * Review thread counts. `needsMe` counts unresolved threads that wait on
 * the viewer: the last comment is someone else's, and the thread is the
 * viewer's to answer. On the viewer's own PR every thread is theirs to
 * answer; on someone else's PR only the threads the viewer started are.
 */
export function countThreads(threadConn, viewer, isMine) {
  const me = lower(viewer);
  let active = 0; let resolved = 0; let needsMe = 0;
  for (const t of nodes(threadConn)) {
    if (t.isResolved) { resolved++; continue; }
    active++;
    const starter = lower(nodes(t.first)[0]?.author?.login);
    const last = lower(nodes(t.last)[0]?.author?.login);
    if (!last || last === me) continue;
    if (isMine || starter === me) needsMe++;
  }
  return { active, resolved, needsMe };
}

/** Did the viewer write any comment in any review thread? */
function viewerInThreads(threadConn, viewer) {
  const me = lower(viewer);
  return nodes(threadConn).some((t) =>
    lower(nodes(t.first)[0]?.author?.login) === me || lower(nodes(t.last)[0]?.author?.login) === me);
}

/**
 * Issue numbers this PR points at in its own repository, as
 * "owner/repo#12": `#12`, `owner/repo#12` (same repo only), `fixes #12`,
 * an issue URL in the same repo, and a branch named like `12-fix-login`,
 * `fix/12-login` or `issue-12`. The PR's own number is left out.
 */
export function extractWorkKeys({ repo, number, body, branch }) {
  const keys = new Set();
  const repoLower = lower(repo);
  const own = String(number ?? "").replace(/^#/, "");
  const add = (n) => { if (n && n !== own && !/^0+$/.test(n)) keys.add(`${repo}#${n.replace(/^0+/, "")}`); };
  const text = String(body ?? "");
  // owner/repo#12 (counted only when it is this repository)
  for (const m of text.matchAll(/(?<![\w/.-])([\w.-]+\/[\w.-]+)#(\d+)\b/g)) {
    if (lower(m[1]) === repoLower) add(m[2]);
  }
  // A bare #12: not after a word character, slash or ampersand, so URL
  // fragments, HTML entities and the owner/repo form above are skipped.
  for (const m of text.matchAll(/(?<![\w/&#])#(\d+)\b/g)) add(m[1]);
  // https://github.com/owner/repo/issues/12
  for (const m of text.matchAll(/https:\/\/github\.com\/([\w.-]+\/[\w.-]+)\/issues\/(\d+)\b/g)) {
    if (lower(m[1]) === repoLower) add(m[2]);
  }
  const b = String(branch ?? "");
  // A branch segment that starts with the number: 12-fix-login, fix/12-login, 12
  for (const m of b.matchAll(/(?:^|\/)(\d+)(?=-|_|$)/g)) add(m[1]);
  // issue-12, issues/12, gh-12 anywhere in the branch
  for (const m of b.matchAll(/(?:^|[/_-])(?:issues?|gh)[-_/]?(\d+)\b/gi)) add(m[1]);
  return [...keys];
}

function person(actor, viewer) {
  const login = actor?.login ?? "ghost";
  const p = { id: login, name: actor?.name || login };
  if (lower(login) === lower(viewer)) p.isMe = true;
  return p;
}

const teamName = (t) => (t.organization?.login ? `${t.organization.login}/${t.slug}` : t.slug || t.name);

/**
 * The bucket and reasons for one PR, from already-mapped facts:
 *   role, state ("open"|"draft"|"merged"|"closed"), closedUtc, now (ms),
 *   checks {state, failing, pending}, mergeable, targetBranch,
 *   reviewDecision, rejectedBy [names], approvedBy [names],
 *   waitingOn [names], threads {needsMe},
 *   me (reviewing only): { requestedDirectly, viaTeams [names], vote, commented }
 * Returns null when the PR should not be listed (closed long ago, or
 * closed and not mine).
 */
export function bucketFor(f) {
  if (f.state === "merged" || f.state === "closed") {
    if (f.role !== "mine") return null;
    const closed = Date.parse(f.closedUtc ?? "");
    if (!Number.isFinite(closed) || f.now - closed > RECENT_DAYS * DAY_MS) return null;
    return { bucket: "recent", reasons: [f.state === "merged" ? `merged into ${f.targetBranch || "the base branch"}` : "closed without merging"] };
  }

  const checks = f.checks ?? { state: "none", failing: [], pending: [] };
  const failingLine = checks.failing?.length ? `${plural(checks.failing.length, "failing check")}: ${checks.failing.join(", ")}` : null;
  const pendingLine = checks.pending?.length ? `${plural(checks.pending.length, "pending check")}: ${checks.pending.join(", ")}` : null;
  const needsMe = f.threads?.needsMe ?? 0;

  if (f.role === "mine") {
    const blockers = [];
    if (failingLine) blockers.push(failingLine);
    if (f.rejectedBy?.length) blockers.push(`changes requested by ${f.rejectedBy.join(", ")}`);
    if (f.mergeable === "conflicts") blockers.push(`conflicts with ${f.targetBranch || "the base branch"}`);
    if (needsMe) blockers.push(`${plural(needsMe, "thread")} to answer`);

    if (f.state === "draft") return { bucket: "draft", reasons: ["still a draft", ...blockers] };
    if (blockers.length) return { bucket: "action", reasons: blockers };

    const green = checks.state === "passing" || checks.state === "none";
    // Without a review rule on the branch GitHub reports no decision at
    // all; an approval with no outstanding change request then counts.
    const approved = f.reviewDecision === "APPROVED" ||
      (f.reviewDecision == null && f.approvedBy?.length > 0 && !f.rejectedBy?.length);
    if (approved && green) {
      const by = f.approvedBy?.length ? ` by ${f.approvedBy.join(", ")}` : "";
      return { bucket: "ready", reasons: [`approved${by}, ${checks.state === "none" ? "no checks" : "checks passing"}`] };
    }
    const waits = [];
    if (f.reviewDecision === "REVIEW_REQUIRED") waits.push("required review not given");
    if (f.waitingOn?.length) waits.push(`waiting on ${f.waitingOn.join(", ")}`);
    if (pendingLine) waits.push(pendingLine);
    if (!waits.length) waits.push("no approving review yet");
    return { bucket: "waiting", reasons: waits };
  }

  // reviewing
  const me = f.me ?? {};
  if (needsMe) {
    return { bucket: "replied", reasons: [needsMe === 1 ? "new reply in your thread" : `new replies in ${needsMe} of your threads`] };
  }
  // GitHub drops a review request once the reviewer submits, so a request
  // that is still there always means a review is wanted now.
  if (me.requestedDirectly) {
    const lines = [me.vote === "approved" || me.vote === "rejected" ? "your review is requested again" : "your review is requested"];
    if (f.state === "draft") lines.push("still a draft");
    return { bucket: "vote", reasons: lines };
  }
  if (me.vote === "approved" || me.vote === "rejected") {
    return { bucket: "voted", reasons: [me.vote === "approved" ? "you approved" : "you requested changes"] };
  }
  if (me.commented) return { bucket: "mythreads", reasons: ["you commented; nothing new"] };
  if (me.viaTeams?.length) return { bucket: "group", reasons: [`review requested from ${me.viaTeams.join(", ")}`] };
  return { bucket: "mythreads", reasons: ["you took part; nothing new"] };
}

/**
 * One GraphQL PullRequest node as a Pr, or null when it is not listed.
 *   ctx.viewer   the viewer's login
 *   ctx.role     "mine" | "reviewing"
 *   ctx.via      Set of search names that found it ("commented", ...)
 *   ctx.now      epoch ms
 */
export function toPr(node, ctx) {
  const viewer = ctx.viewer;
  const me = lower(viewer);
  const repo = node.repository?.nameWithOwner ?? "";
  const open = node.state === "OPEN";
  const state = node.state === "MERGED" ? "merged" : node.state === "CLOSED" ? "closed" : node.isDraft ? "draft" : "open";
  const closedUtc = node.mergedAt ?? node.closedAt ?? null;
  const isMine = ctx.role === "mine";

  // Reviewers: everyone with a latest review, plus every pending request.
  const reviewers = new Map();
  const rejectedBy = []; const approvedBy = [];
  let myVote = "none";
  for (const r of nodes(node.latestReviews)) {
    const p = person(r.author, viewer);
    const vote = mapVote(r.state);
    if (lower(p.id) === me) { myVote = vote; if (r.state === "PENDING") continue; }
    if (vote === "rejected") rejectedBy.push(p.name);
    if (vote === "approved") approvedBy.push(p.name);
    reviewers.set(`u:${lower(p.id)}`, { ...p, vote });
  }
  const waitingOn = []; const viaTeams = [];
  let requestedDirectly = false;
  for (const rq of nodes(node.reviewRequests)) {
    const who = rq.requestedReviewer;
    if (!who) continue;
    if (who.__typename === "Team") {
      const name = teamName(who);
      reviewers.set(`t:${lower(name)}`, { id: name, name, isGroup: true, vote: "none" });
      waitingOn.push(name);
      viaTeams.push(name);
      continue;
    }
    const p = person(who, viewer);
    if (lower(p.id) === me) requestedDirectly = true;
    // Requested by name: the closest GitHub has to a required reviewer.
    const key = `u:${lower(p.id)}`;
    reviewers.set(key, { ...p, vote: "none", ...reviewers.get(key), required: true });
    waitingOn.push(p.name);
  }

  const checks = open ? mapChecks(nodes(node.commits)[0]?.commit?.statusCheckRollup ?? null) : null;
  const threads = countThreads(node.reviewThreads, viewer, isMine);
  const mergeable = mapMergeable(node.mergeable, open);
  const commented = Boolean(ctx.via?.has?.("commented")) || viewerInThreads(node.reviewThreads, viewer) ||
    nodes(node.latestReviews).some((r) => lower(r.author?.login) === me && r.state === "COMMENTED");

  const placed = bucketFor({
    role: ctx.role, state, closedUtc, now: ctx.now, checks, mergeable,
    targetBranch: node.baseRefName, reviewDecision: node.reviewDecision,
    rejectedBy, approvedBy, waitingOn, threads,
    // Only a team request when no direct one exists: a group request.
    me: isMine ? null : { requestedDirectly, viaTeams: requestedDirectly ? [] : viaTeams, vote: myVote, commented },
  });
  if (!placed) return null;

  const number = `#${node.number}`;
  const url = node.url;
  const pr = {
    forge: "github",
    repo,
    project: null,
    number,
    url,
    title: node.title ?? "",
    author: person(node.author, viewer),
    sourceBranch: node.headRefName,
    targetBranch: node.baseRefName,
    state,
    closedUtc,
    role: ctx.role,
    bucket: placed.bucket,
    reasons: placed.reasons,
    me: isMine ? null : { required: requestedDirectly, vote: myVote },
    reviewers: [...reviewers.values()].filter((r) => !r.isMe),
    checks,
    threads,
    mergeable,
    workKeys: extractWorkKeys({ repo, number: node.number, body: node.body, branch: node.headRefName }),
    actions: [
      { kind: "link", id: "open", label: "Open on GitHub", url },
      { kind: "link", id: "files", label: "Files changed", url: `${url}/files` },
      ...(open ? [{ kind: "link", id: "checks", label: "Checks", url: `${url}/checks` }] : []),
      { kind: "endpoint", id: "refresh", label: "Refresh now", mutates: false },
    ],
  };
  return pr;
}
