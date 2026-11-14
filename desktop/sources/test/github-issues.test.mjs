// The GitHub source's issue mapping (Work items), from hand-written
// GraphQL-shaped inputs.
import test from "node:test";
import assert from "node:assert/strict";
import { issueStatus, linkedPrs, prState, toWorkItem } from "../github/issues.mjs";

const pr = (number, state, o = {}) => ({ number, state, isDraft: o.draft ?? false, url: `https://github.com/owner/repo/pull/${number}`, repository: { nameWithOwner: o.repo ?? "owner/repo" } });
function issue(o = {}) {
  return {
    number: o.number ?? 12,
    title: "title" in o ? o.title : "Login fails",
    url: "url" in o ? o.url : "https://github.com/owner/repo/issues/12",
    createdAt: "2026-03-01T09:00:00Z",
    updatedAt: "2026-03-09T09:00:00Z",
    repository: { nameWithOwner: o.repo ?? "owner/repo" },
    author: "author" in o ? o.author : { login: "sam", name: "Sam" },
    labels: { nodes: o.labels ?? [] },
    closedByPullRequestsReferences: { nodes: o.prs ?? [] },
  };
}
const item = (o) => toWorkItem(issue(o), { viewer: "alex" });

test("a linked PR's state as the PRs view names it", () => {
  assert.equal(prState(pr(1, "OPEN")), "open");
  assert.equal(prState(pr(1, "OPEN", { draft: true })), "draft");
  assert.equal(prState(pr(1, "MERGED")), "merged");
  assert.equal(prState(pr(1, "CLOSED")), "closed");
});

test("an issue is in progress only while a closing PR is open or a draft", () => {
  const status = (...states) => issueStatus(states.map((s) => ({ status: s }))).category;
  assert.equal(status(), "todo");
  assert.equal(status("closed", "merged"), "todo");
  assert.equal(status("closed", "open"), "doing");
  assert.equal(status("draft"), "doing");
  assert.deepEqual(issueStatus([]), { label: "Open", category: "todo" });
  assert.deepEqual(issueStatus([{ status: "open" }]), { label: "In progress", category: "doing" });
});

test("linked PRs keep their own repository and skip unreadable nodes", () => {
  assert.deepEqual(linkedPrs(issue({ prs: [pr(3, "OPEN", { repo: "other/lib" }), null, { number: "x" }, { number: 4 }] })), [
    { forge: "github", repo: "other/lib", number: "#3", url: "https://github.com/owner/repo/pull/3", status: "open" },
  ]);
});

test("the item: key, type, labels, reporter, times, PR chips and the one link action", () => {
  const w = item({ labels: [{ name: "bug" }, { name: "" }, {}], prs: [pr(1, "OPEN", { draft: true })] });
  assert.deepEqual(w, {
    key: "owner/repo#12",
    title: "Login fails",
    type: "issue",
    status: { label: "In progress", category: "doing" },
    url: "https://github.com/owner/repo/issues/12",
    assignedToMe: true,
    labels: ["bug"],
    created: "2026-03-01T09:00:00Z",
    reporter: { id: "sam", name: "Sam" },
    lastTouchedUtc: "2026-03-09T09:00:00Z",
    prs: [{ forge: "github", repo: "owner/repo", number: "#1", url: "https://github.com/owner/repo/pull/1", status: "draft" }],
    actions: [{ kind: "link", id: "open", label: "Open on GitHub", url: "https://github.com/owner/repo/issues/12" }],
  });
});

test("the reporter is me when the author is the viewer (case-insensitive); a deleted author is none", () => {
  assert.deepEqual(item({ author: { login: "Alex" } }).reporter, { id: "Alex", name: "Alex", isMe: true });
  assert.equal(item({ author: null }).reporter, null);
});

test("missing optional fields read as absent; a node without a repo or number is skipped", () => {
  const w = item({ title: null, url: null });
  assert.equal(w.title, null);
  assert.equal(w.url, null);
  assert.deepEqual(w.actions, []);
  assert.equal(toWorkItem({}, { viewer: "alex" }), null);
  assert.equal(toWorkItem({ number: 1 }, { viewer: "alex" }), null);
  assert.equal(toWorkItem(null, { viewer: "alex" }), null);
});
