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
 * The search strings, one call each so that one failing search leaves
 * the others rendering. `since` is a YYYY-MM-DD date for the window of
 * recently merged or closed PRs.
 */
export function searches(since) {
  return {
    mineOpen: { role: "mine", q: "is:pr is:open author:@me archived:false" },
    mineClosed: { role: "mine", q: `is:pr is:closed author:@me closed:>=${since}` },
    requested: { role: "reviewing", q: "is:pr is:open review-requested:@me -author:@me archived:false" },
    reviewed: { role: "reviewing", q: "is:pr is:open reviewed-by:@me -author:@me archived:false" },
    commented: { role: "reviewing", q: "is:pr is:open commenter:@me -author:@me archived:false" },
  };
}
