// Pure mapping from GitHub's GraphQL issue shape to the app's WorkItem
// shape. No I/O: the caller passes who the viewer is, so every rule is
// testable with hand-written inputs.
//
// Only open issues assigned to the viewer arrive here (the search says so),
// so `assignedToMe` is always true. The source cannot know where a local
// checkout of the repository lives, so an item carries no `folder` and no
// folder actions; and GitHub has no local workflow, so no `localStatus`.

const lower = (s) => (typeof s === "string" ? s.toLowerCase() : "");
const nodes = (conn) => (Array.isArray(conn?.nodes) ? conn.nodes.filter(Boolean) : []);

/** A linked pull request's state as the PRs view names it. */
export function prState(pr) {
  if (pr.state === "MERGED") return "merged";
  if (pr.state === "CLOSED") return "closed";
  return pr.isDraft ? "draft" : "open";
}

/**
 * The work status of an open issue: "doing" while a pull request that
 * would close it is open (a draft counts: someone is on it), else "todo".
 * Only the PRs GitHub links as closing the issue are counted, not any PR
 * that merely mentions it, which would move issues on in passing.
 */
export function issueStatus(linked) {
  return linked.some((p) => p.status === "open" || p.status === "draft")
    ? { label: "In progress", category: "doing" }
    : { label: "Open", category: "todo" };
}

/** The pull requests GitHub links to the issue as closing it. */
export function linkedPrs(node) {
  return nodes(node.closedByPullRequestsReferences)
    .filter((p) => typeof p.number === "number" && p.repository?.nameWithOwner)
    .map((p) => ({
      forge: "github",
      repo: p.repository.nameWithOwner,
      number: `#${p.number}`,
      ...(p.url ? { url: p.url } : {}),
      status: prState(p),
    }));
}

/** One assigned issue as a Work item, or null for a node that is not one. */
export function toWorkItem(node, { viewer }) {
  const repo = node?.repository?.nameWithOwner;
  if (!repo || typeof node.number !== "number") return null;
  const key = `${repo}#${node.number}`;
  const prs = linkedPrs(node);
  const login = node.author?.login;
  const reporter = login
    ? { id: login, name: node.author.name || login, ...(lower(login) === lower(viewer) ? { isMe: true } : {}) }
    : null;
  return {
    key,
    title: typeof node.title === "string" ? node.title : null,
    type: "issue",
    status: issueStatus(prs),
    url: node.url ?? null,
    assignedToMe: true,
    labels: nodes(node.labels).map((l) => l.name).filter((n) => typeof n === "string" && n !== ""),
    created: node.createdAt ?? null,
    reporter,
    lastTouchedUtc: node.updatedAt ?? null,
    prs,
    actions: node.url ? [{ kind: "link", id: "open", label: "Open on GitHub", url: node.url }] : [],
  };
}
