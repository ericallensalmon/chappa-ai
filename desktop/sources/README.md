# Writing a source

A source is a local process that tells Chappa'Ai about work items, pull
requests, things that need attention and the state of an environment. The
app owns the views, polling, notifications and every safety check; the
source owns the data and any vendor quirk. A source speaks JSON over HTTP
on a loopback port. It can be written in any language.

The wire shapes are in `desktop/ui/src/sources/types.ts`. A complete
working example is in `fixture/`.

## Lifecycle

A source is a command in `chappa.yml` with a `source:` key:

```yaml
commands:
  my-source:
    command: [node, sources/my-source/index.mjs]
    source: {}
```

The app starts it like any other command (and the usual trust prompt for
the project applies). Before it spawns, it picks a free loopback port and
a random 32-byte token and passes both in the environment:

| variable | value |
|---|---|
| `CHAPPA_SOURCE_ADDR` | `127.0.0.1:<port>`; bind exactly this address |
| `CHAPPA_SOURCE_TOKEN` | 64 lowercase hex characters (32 bytes) |

The source is ready when the identity check below answers. Only loopback
addresses are accepted. A source that exits is restarted like any command.
A server started elsewhere can instead be attached in Settings, with an
address and an owner-only token file.

## Proving identity

Another program could grab the port before the source does, so the app
never sends the token to a server that has not proved it knows it. The
app first calls, with no token:

```
GET /chappa/v1/identity?challenge=<64 lowercase hex chars>
```

and the source answers `{"proof": "<hex>"}`:

```
key   = HMAC-SHA256(key = token bytes, message = "chappa source identity v1")
proof = lowercase hex of HMAC-SHA256(key, challenge_hex || 0x00 || addr)
```

Token bytes are the 32 bytes the hex value decodes to. `challenge_hex` is
the challenge string exactly as received (not decoded). `addr` is the
literal `CHAPPA_SOURCE_ADDR` string. Reject a challenge that is not 64
lowercase hex characters with `400`. In Node:

```js
import { createHmac } from "node:crypto";

const key = createHmac("sha256", Buffer.from(token, "hex"))
  .update("chappa source identity v1").digest();
const proof = createHmac("sha256", key)
  .update(challenge).update(Buffer.from([0])).update(addr).digest("hex");
```

`lib/identity.mjs` is this helper, ready to copy. Test vectors for any
language are in `fixture/test/identity-vectors.json`: a JSON array of
`{token, challenge, addr, proof}`.

After a good proof the app sends `Authorization: Bearer <token>` on every
other request. Answer `401` to anything without it, comparing in constant
time.

## The manifest

`GET /chappa/v1/manifest` returns what the source offers:

```json
{
  "contract": 1,
  "source": { "id": "github", "label": "GitHub", "version": "0.1.0" },
  "views": {
    "work":        { "list": "/api/work", "item": "/api/work/{key}", "pollSeconds": 60 },
    "prs":         { "list": "/api/prs", "pollSeconds": 300 },
    "attention":   { "list": "/api/attention" },
    "environment": { "list": "/api/env", "pollSeconds": 120 }
  },
  "workflows": "/api/workflows",
  "actions": "/api/actions",
  "jobs": "/api/jobs/{id}",
  "linkHosts": ["github.com"],
  "roots": []
}
```

Every view is optional, and `pollSeconds` is clamped to 15..3600. The
list of endpoints is closed: the app calls only the paths named here.
`linkHosts` and `roots` are what the source asks to be allowed to open
and launch in; the user approves them (shown with the view list) and
the approval covers exactly what was shown, so changing them asks again.
An untrusted source is not polled.

Every list response is `{ "freshness": {...}, "items": [...] }` plus
view-specific fields (`banners`, or for the environment `groups`,
`components`, `summary`, `facts`, `alerts`, `dataSafety`). Unknown fields are ignored.
A non-2xx reply carries `{kind, message, detail?}`.

## Actions

Items, pull requests, components and banners carry `actions`. Five kinds:

- `agent`: start an agent in `cwd` with a first `prompt` (optional
  `tool` name, `title`, `env`).
- `terminal`: open a terminal in `cwd`, optionally running `command`
  (argv, never a shell string; optional `env`).
- `endpoint`: the app calls the source.
- `link`: open an `https` URL on a trusted host.
- `panel`: open a drill-in page served by `endpoint`; it returns
  `{title, freshness, sections, banners?, actions?}`. The page's path
  must be named by a `panel` action in one of the source's view
  answers (a page naming another page does not make it fetchable on
  its own). The page's actions, its banners' and its sections' run like
  any other, with `target: {view: "panel:<endpoint>", key: null}`.

Who an action belongs to decides its `target.key`: an item's own
actions and its sections' carry the item's key (`key`, else `id`; a
pull request's is `forge:repo:number`). A view's own actions, its
banners' and its `facts`' carry `key: null`: a banner or a section is
part of what holds it, and its `id` only names it.

The app checks every `cwd` against the open projects, their git
worktrees and the approved roots, shows an agent's prompt before it
runs, and refuses links to hosts that were not approved.

### Endpoint actions

The app calls `POST {actions}` with:

```json
{ "id": "approve", "target": { "view": "work", "key": "ABC-1" },
  "params": {}, "note": "optional text", "confirmed": true }
```

An action that changes something declares `mutates: true` and a
`confirm` question; the app does not show it without one. `confirmed:
true` is sent only after the user accepts the question, and the source
must refuse a mutating call without it (the fixture answers `409`).
Reply with one of:

- `200 {message?}`: done.
- `202 {jobId}`: started. The app polls `GET {jobs}` (with `{id}`
  replaced) until it answers `{state: "done"|"failed", log?}`; while
  working it answers `{state: "running"}`.
- `202 {jobId, run: {argv, cwd, env?, title?}}`: started, and the app
  should run the job's command. It checks `cwd`, runs `argv` (no
  shell) in a pane with `env` added, and when the command exits posts
  `{exitCode, endedUtc, output}` to `POST {jobs}` (same path as the
  poll). Until then answer `running`. Send `run` only when the request
  carries `X-Chappa-Runs-Jobs: 1`; without that header the caller
  cannot host a pane, so run the job yourself and answer `202 {jobId}`.
- `503`: the outcome is uncertain. The app tells the user it may or may
  not have happened and never retries a mutating call itself.

An endpoint action with `note: "required"` asks the user for a note
before it is sent; the source refuses the call without one. The app may
name who acted in an `X-Chappa-Actor` request header.

## Data safety on the Environment view

A source that runs databases can say whether their data is known to be
scrubbed. The Environment answer then carries one top-level `dataSafety`
object for the instance it serves (the stack runner, `chappa-stack`,
sends it always):

```json
"dataSafety": {
  "instance": "local/0",
  "state": "not-clean",
  "policy": "block-agents",
  "blocksAgents": true,
  "lastScanUtc": "2026-11-14T12:00:00Z",
  "rulesHash": "3f2a…",
  "marker": { "scrubbed": true, "version": "2026-11", "at": "2026-11-01T00:00:00Z" },
  "findings": [
    { "rule": "card-number", "table": "payments", "column": "card", "count": 2, "severity": "bad" }
  ],
  "unscanned": [],
  "ack": null,
  "ackActionId": "data-safety-ack"
}
```

| field | meaning |
|---|---|
| `instance` | `<stack>/<slot>` |
| `state` | `clean`, `not-clean` (findings, a missing or negative scrub marker, a sample that could not be checked), `acknowledged` (not clean, and someone acknowledged exactly this result), `unscanned` (a component keeps data and nothing checks it, or its check has not run since it started), `error` (the check could not be judged: a broken rules file, a hook that cannot be filled in). The worst over the components: `error` > `not-clean` > `unscanned` > `acknowledged` > `clean` |
| `policy` | the strongest policy in force: `warn` < `block-agents` < `stop` < `block-up`; `null` when none is |
| `blocksAgents` | **the one field to enforce on**: true exactly when `policy` is set and is not `warn`. While it is true, the app launches no new agent into anything bound to the instance |
| `lastScanUtc`, `rulesHash` | the newest check and the rules file it used |
| `marker` | the scrub attestation: `scrubbed` only when every checked marker says so; `null` when no marker was checked |
| `findings` | counts per rule and place. Never a value |
| `unscanned` | the components counted as unscanned |
| `ack` | the acknowledgement in force: `{by, note, atUtc}`, or `null` |
| `ackActionId` | the endpoint action that acknowledges. It is offered (on the data-safety banner and facts section, `key: null`, `mutates: true`, `note: "required"`, a `confirm` naming the findings) only while `state` is `not-clean`; it lasts until a check reports anything different |

When the source cannot read its definition at all, it still sends the
block, with `state: "error"`, `policy: null` and `blocksAgents: false`:
it cannot tell, and what that means is the app's decision.

## Run the fixture by hand

```sh
CHAPPA_SOURCE_ADDR=127.0.0.1:5055 \
CHAPPA_SOURCE_TOKEN=$(node -e "console.log(require('crypto').randomBytes(32).toString('hex'))") \
node desktop/sources/fixture/index.mjs
```

Use `--data <dir>` to serve a different set of JSON files (copy
`fixture/data/` and edit). Bodies are re-read on every request. Set
`CHAPPA_FIXTURE_JOB_MS` to change how long a plain job takes to finish
(default 3000). Then, from another shell, with the same token in
`$TOKEN`:

```sh
curl "http://127.0.0.1:5055/chappa/v1/identity?challenge=$(printf 'ab%.0s' $(seq 32))"
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:5055/chappa/v1/manifest
```

## GitHub source

`github/` is a real source: it serves the Work and PRs views for the
GitHub account the `gh` CLI is logged in to. It asks `gh api graphql` for the
data, so neither the app nor the source ever holds a GitHub token.

Requirements: `gh` on `PATH` (or named by `GH_BIN`) and logged in
(`gh auth login`). Without either it answers an empty list with one
clear error instead of failing.

```yaml
commands:
  github-source:
    command: [node, sources/github/index.mjs]
    source: {}
```

What it serves:

- `GET /api/prs`: open PRs you wrote (role `mine`) plus the ones merged
  or closed in the last 14 days (bucket `recent`), and open PRs where
  your review is requested, you reviewed, or you commented (role
  `reviewing`), up to 100 per role. Buckets and reasons are computed in
  `github/buckets.mjs` from the review decision, requested reviewers (a
  team request alone puts a PR in `group`), latest reviews, check runs
  and statuses, mergeability and review threads. `workKeys` lists the
  same-repository issues the body or branch name points at
  (`owner/repo#12`).
- `GET /api/work`: open issues assigned to you in every repository
  (not archived), up to 100, keyed `owner/repo#12`, type `issue`.
  Status is "Open" (`todo`), or "In progress" (`doing`) while a pull
  request GitHub links as closing the issue is open or a draft; those
  PRs are the item's `prs`, which the Work view joins to the PRs view.
  No `folder` (the source cannot know where a checkout lives) and no
  local workflow. Each item has one link action, "Open on GitHub".
- Link actions (the PR, its files, its checks) and one endpoint action,
  `refresh`, which changes nothing on GitHub.

GitHub is asked at most once a minute (`CHAPPA_GITHUB_REFRESH_SECONDS`)
however often the app polls; the last list is served with
`refreshing: true` while a fetch runs. A search that fails is named in
`freshness.errors` and the others still render; a search that matched
more than one page holds ("showing 100 of 212") says so in
`freshness.notices`. Both views come from one fetch, so polling Work
asks GitHub nothing extra; a failing issue search is an error on Work
only.

Tests (Node 22, no packages):

```sh
cd desktop && npm run test:sources
```
