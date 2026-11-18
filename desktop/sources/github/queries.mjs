// GraphQL documents for the GitHub source, sent through `gh api graphql`
// so the source never handles a token itself. Plain strings; every field
// carries a comment on why it is fetched. GraphQL accepts `#` comments.

/** Who "me" is: every isMe / needsMe / vote decision compares logins. */
export const VIEWER = `
query {
  viewer {
    login   # compared against authors, reviewers and thread comments
    name    # display name for the author Person when the PR is mine
  }
}`;

/**
 * One page of pull requests matching a search string (`$q`, e.g.
 * "is:pr is:open author:@me"). Capped at 100: the view is a work list,
 * not an archive, and one page keeps the call to a single request.
 */
export const SEARCH_PRS = `
query ($q: String!) {
  search(query: $q, type: ISSUE, first: 100) {
    issueCount            # how many matched, to say when the cap cut the list
    nodes {
      ... on PullRequest {
        id                # stable node id, to dedupe PRs found by several searches
        number            # shown as "#12"
        title
        url               # the row's link and the "Open" action
        body              # scanned for issue mentions (workKeys)
        isDraft           # state "draft" and the draft bucket
        state             # OPEN / MERGED / CLOSED
        createdAt         # created: the age of a PR that never moved
        updatedAt         # lastTouchedUtc: the age column
        closedAt          # closedUtc, and the 14-day "recent" window
        mergedAt          # closedUtc for merged PRs (merged wins over closed)
        headRefName       # sourceBranch, also scanned for issue mentions
        baseRefName       # targetBranch, named in "conflicts with <branch>"
        repository {
          nameWithOwner   # repo "owner/repo"; same-repo rule for workKeys
        }
        author {
          login           # Person.id, and role "mine" vs "reviewing"
          ... on User { name }  # Person.name when the user set one
        }
        reviewDecision    # APPROVED / CHANGES_REQUESTED / REVIEW_REQUIRED, from branch rules
        mergeable         # MERGEABLE / CONFLICTING / UNKNOWN
        reviewRequests(first: 50) {
          nodes {
            asCodeOwner   # requested because the PR touches code they own
            requestedReviewer {
              __typename  # User or Team: a Team request is a group request
              ... on User { login name }
              ... on Bot { login }
              ... on Mannequin { login }
              ... on Team { slug name organization { login } }
            }
          }
        }
        latestReviews(first: 50) {
          nodes {
            state         # APPROVED / CHANGES_REQUESTED / COMMENTED / PENDING / DISMISSED -> vote
            submittedAt
            author {
              login       # whose vote this is; the viewer's own one is me.vote
              ... on User { name }
            }
          }
        }
        reviewThreads(first: 100) {
          nodes {
            isResolved    # threads.active vs threads.resolved
            first: comments(first: 1) {
              nodes { author { login } }   # who started the thread
            }
            last: comments(last: 1) {
              nodes { author { login } createdAt }   # who spoke last: a reply waiting on me?
            }
          }
        }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state     # overall rollup; per-context results below decide the names
                contexts(first: 100) {
                  nodes {
                    __typename
                    ... on CheckRun { name status conclusion }   # Actions and app checks
                    ... on StatusContext { context state }       # commit statuses from older integrations
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}`;

/**
 * One page of issues matching a search string (`$q`): the Work view's
 * items. Capped at 100 like the pull request searches, for the same
 * reason. The closing pull requests ride along in the same call, so the
 * "in progress" status and the PR chips cost no extra request.
 */
export const SEARCH_ISSUES = `
query ($q: String!) {
  search(query: $q, type: ISSUE, first: 100) {
    issueCount            # how many matched, to say when the cap cut the list
    nodes {
      ... on Issue {
        number            # the key "owner/repo#12"
        title
        url               # the row's link and the "Open on GitHub" action
        createdAt         # created
        updatedAt         # lastTouchedUtc
        repository {
          nameWithOwner   # the key's "owner/repo"
        }
        author {
          login           # reporter id; isMe when it is the viewer
          ... on User { name }  # reporter name when the user set one
        }
        labels(first: 20) {
          nodes { name }  # labels, shown as chips and searched by the text filter
        }
        closedByPullRequestsReferences(first: 10, includeClosedPrs: true) {
          nodes {
            number        # the PR chip "#7", joined to the PRs view
            url
            state         # OPEN / MERGED / CLOSED: an open one makes the issue "in progress"
            isDraft
            repository { nameWithOwner }
          }
        }
      }
    }
  }
}`;

/** The Work view's search: open issues assigned to the viewer, in every
 *  repository, skipping archived ones (nothing can be done there). */
export const ISSUES_SEARCH = { label: "issues assigned to me", q: "is:issue is:open assignee:@me archived:false" };

/**
 * The search strings, one call each so that one failing search leaves
 * the others rendering, with the label a notice about the search uses. `since` is a YYYY-MM-DD date for the window of
 * recently merged or closed PRs.
 */
export function searches(since) {
  return {
    mineOpen: { role: "mine", label: "my open PRs", q: "is:pr is:open author:@me archived:false" },
    mineClosed: { role: "mine", label: "my recently closed PRs", q: `is:pr is:closed author:@me closed:>=${since}` },
    requested: { role: "reviewing", label: "review requested", q: "is:pr is:open review-requested:@me -author:@me archived:false" },
    reviewed: { role: "reviewing", label: "reviewed by me", q: "is:pr is:open reviewed-by:@me -author:@me archived:false" },
    commented: { role: "reviewing", label: "commented on", q: "is:pr is:open commenter:@me -author:@me archived:false" },
  };
}
