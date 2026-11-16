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

A `linkHosts` entry is a host, optionally with a port, matched exactly
(`github.com` does not cover `api.github.com`). `host:*` means any port on
that host, the default one included; the trust dialog shows it as "any
port on host". It is accepted only on a host that names one party: this
machine (`localhost`, `127.0.0.1`, `[::1]`), an IP literal, or a name of
two or more labels that is not a public suffix (`dev.example.test:*` is
fine, `test:*`, `co.uk:*` and `devbox:*` are not). Any other `*`, a bare
`*` included, makes the manifest unusable. Links still need `https`, or
plain `http` to this machine.

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
before it is sent; the source refuses the call without one.

An endpoint action with `disabled: "<reason>"` is drawn disabled, the
reason as its tooltip, and the app refuses a call to it without sending
anything; send it without the field once it can run (the next poll
enables the button). The reason is trimmed, control characters are
dropped and it is cut at 200 characters; any value but a string makes the
action unusable. Refuse such a call yourself too.

An endpoint action can ask for values in its dialog with `inputs`:

```json
{ "kind": "endpoint", "id": "seed", "label": "Seed", "mutates": true,
  "confirm": "Seed the database", "params": { "slot": 0 },
  "inputs": [
    { "name": "profile", "label": "Profile", "kind": "choice", "required": true,
      "options": [{ "value": "small", "label": "Small" }, { "value": "big", "label": "Big" }], "default": "small" },
    { "name": "seeds", "label": "Seeds", "kind": "multi",
      "options": [{ "value": "db/empty", "label": "empty" }] },
    { "name": "why", "label": "Why", "kind": "text", "max": 80 },
    { "name": "count", "label": "Count", "kind": "number", "max": 50 }
  ] }
```

`kind` is `choice` (one of `options`), `multi` (any of them, each once),
`text` (one line, at most `max` characters, 200 when no `max` is given)
or `number` (at most `max` when given). `name` is `[A-Za-z0-9._:/-]`, at
most 100 characters, unique in the action; a `default` must be a value
its input takes. An action whose `inputs` are malformed is not offered
(an item action with them makes the manifest unusable). The app shows
the inputs in the confirm dialog, checks each value against what was
declared before the call (a missing required value, an unknown option,
text that is too long or not one line, a number over `max`: the call is
not made), and sends them merged into `params`, an input replacing a
fixed param of the same name. An input left empty is left out. Check the
values again yourself: the app's check is a courtesy, not a guarantee.

Every actions call names who asked for it in an `X-Chappa-Actor` request
header: `human:<OS user name>` for an action the person ran from the app,
`agent:<actor id>` for one an agent caused (no agent can run a source
action today, so every call is `human:` for now). The value is printable
ASCII, at most 80 characters. A source that lets only people make some
moves (a review gate, an acknowledgement) refuses a call whose header is
missing or does not start with `human:`. The header is the app's word,
not proof: it says who the app was acting for.

## One item, several sources

A work item lives in a tracker, its pull requests on a forge, and its
workspace (folder, local status) with whatever sets that up. It is still
one row in Work: the tracker owns it, and other sources add to it.

**Adding to another source's item.** A Work entry with
`ref: {source, key}` adds to the item `key` of the source whose manifest
`source.id` is `source`:

```json
{ "ref": { "source": "tracker", "key": "ABC-1" }, "key": "ABC-1",
  "title": "Fix login", "url": "https://…",
  "folder": "/srv/example/abc-1", "active": true,
  "branches": [{ "forge": "github", "repo": "owner/api", "branch": "task/ABC-1-fix-login" }],
  "badges": [], "banners": [], "sections": [], "actions": [] }
```

- The owner's tracker fields win (key, title, type, status, url,
  priority, labels, parent, reporter, created, assigned to me). Yours are
  used only when the owner does not list the item: send the last `title`
  and `url` you know, and the row still shows, badged "not listed by
  <owner>" or "<owner> unavailable". A workspace never disappears
  because a query changed.
- Your local fields win: `folder`, `active`, `localStatus`, `branches`
  (the owner's count only when no source adding to the item sets one).
  When two sources set one differently, the first in Settings ▸ Sources
  wins and the row says so.
- `badges`, `banners`, `sections`, `actions` and `prs` are added to the
  owner's (PRs deduped by forge, repo and number). Your actions run
  against you, judged by your trust, like on a row of your own; a
  `banners` entry on an item shows at the top of its panel and as a dot
  on its row.
- Your endpoint calls for such an entry (its own actions, its sections'
  and its banners') carry the owner in the target:
  `target: {view: "work", key: ref.key, source: ref.source, item}`, where
  `item` is the owner's row exactly as the app last received it (left
  out when the owner does not list it). That is how you learn the
  tracker's status. Treat `item` as untrusted input: sanitise its key
  before it becomes a path, and never put it into a shell. A call from a
  row you own yourself (a work item, a pull request) carries your own
  `source.id` and your row as you sent it the same way.
- **Owner updates.** To hear about a tracker's change without waiting for
  the next action, declare a path in the manifest:
  `"ownerUpdates": "/api/work/owner"`. After each Work poll of an owning
  source, the app posts `{source, key, item}` there for every row of it
  you add to whose content changed since it was last posted to you: once
  per row per poll, debounced, and only while the user trusts you (the
  path is part of what they trust). Answer `2xx`; anything else is
  logged and not retried until the row changes again. After a restart of
  your process or a new trust answer you get every row once more.

**Offering actions on other sources' rows** (a "Start work" before you
have anything of your own about the item) goes in the manifest:

```json
"itemActions": [
  { "kind": "endpoint", "id": "start-work", "label": "Start work", "mutates": true,
    "confirm": "Create a workspace for {key} — {title}", "note": "optional",
    "on": { "view": "work", "sources": ["tracker"] } }
]
```

Only `endpoint` actions; a mutating one needs its `confirm`. `on.view` is
`work` or `prs`; `on.sources` lists manifest ids, or `"*"` for every
other source. The app fills `{key}`, `{title}` (one line, at most 80
characters) and `{source}` (the owner's label) from the row, and that is
the question the user answers. The action is offered on every matching
row except the ones you already add to (your entry brings its own
actions), and it is part of what the user trusts: changing an item
action asks again. The call is `{id, target: {view, key, source, item},
params, note, confirmed}`; for a PR row `key` is `forge:repo:number` and
`item` the PR.

**PRs join the item** by three rules, in order: the item's `prs` names
the PR ("linked"); one of its `branches` is the PR's `sourceBranch` on
the same forge and repo, `refs/heads/` ignored on both sides ("branch");
or the owner's item key appears as a whole word in the PR's
`sourceBranch` or `title` ("key in branch" / "key in title"). The last
one is a per-source setting in Settings ▸ Sources (on by default): a
pattern, `[A-Z][A-Z0-9]+-\d+` unless changed, finds candidate keys, and
a PR joins only an item whose key it found exactly (`ABC-12` never joins
`ABC-1`; case counts). A joined PR shows on the item saying how, and the
PR's row gets a chip with the item's key.

Run the fixture twice to see it: once as above, once with
`--data desktop/sources/fixture/data-overlay` (a "Workspaces" source that
adds to the first one's ABC-1 and ABC-2, keeps ABC-9 the tracker no
longer lists, and offers Start work on the rest). Its actions answer with
the call they received, so the target can be read back.

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
  "ackActionId": "data-safety-ack",
  "bound": { "source": "tracker", "key": "ABC-1",
             "folders": ["/srv/example/.work/ABC-1", "/srv/example/.work/ABC-1/src/api-1a2b3c4d"] },
  "project": "/srv/example",
  "reasons": [ { "component": "db", "text": "db: 2 findings (card-number ×2)" } ]
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
| `bound` | the work item the instance is bound to (the slot's active item): `source` and `key` of the row that owns it (`source` is `null` when the item's record cannot be read) and `folders`, the absolute folders agents work in for it: the item folder first, then each worktree. `null` when no item is active |
| `project` | the stack project's folder, absolute. What the block covers when `bound` is `null`: agents started in the stack project itself are as close to the data as anything |
| `reasons` | one line per component with something to say (not clean, cannot be judged, not checked), in definition order: `{component, text}`. The app shows them as the reason a launch is refused |

When the source cannot read its definition at all, it still sends the
block, with `state: "error"`, `policy: null` and `blocksAgents: false`:
it cannot tell, and the app treats it as blocking (it fails closed).

### What the app does with it

- **Blocking**: `blocksAgents` is true, or `state` is `error` whatever
  `blocksAgents` says. A source that sent a block and then goes down,
  stops answering, fails its Environment poll or loses trust keeps its
  last block in force (shown as "last known: …, source unavailable")
  until it answers again. A source that never sends a block (an older
  source, or one that runs no databases) enforces nothing.
- **Across restarts**: the app keeps each source's last answer (clean
  or not) on disk. When it starts, before a source has answered again,
  a kept block that blocks is in force as the last known one, and a kept
  clean answer allows. A trusted source whose manifest `source.id`
  starts with `stack:` and that has never answered (a new stack, or the
  first run of an app that did not keep answers yet) keeps agents out of
  the folder it was launched on (its `--project`, else its working
  folder) until its first answer. The app also remembers that a source's
  trusted manifest said it is a stack, so on later launches that hold
  starts the moment the command starts, before its manifest arrives;
  trusting a changed manifest that is no longer a stack forgets it.
  Detaching a source in Settings or removing its project drops what was
  kept for it at once. Removing a launched source's command (or its
  `source:` key) from `chappa.yml` stops it, but its kept block stays in
  force ("last known: …, source removed — trust the new chappa.yml to
  lift") until that content is trusted: the trust prompt is answered, or
  the content is already trusted (an in-app edit of a trusted project).
  Anything that can write the file could otherwise lift a block. A
  source that only crashed or failed to start keeps its record.
- **Where**: the folders in `bound`, or `project` when nothing is bound
  (the launching project's folder when the block names none). A folder
  and everything below it, compared after resolving `..` and symlinks,
  ignoring case on Windows.
- **New agents** whose working folder is covered are refused, whichever
  way they start: a source's `agent` action (its button is disabled with
  the reason first), the app's own launch, the control surface and MCP,
  starting a stopped agent row again, and resuming one. Terminals are not
  agents and are never refused.
- **Running agents** in a covered folder are not stopped. Each gets a
  Needs you entry, and what the control surface, MCP or a timer sends it
  is held (queued, in order, "held: N messages" on its row) until the
  block lifts, then delivered. What the person types is never held.
- **Lifting it** is the source's: a clean check, or the `ackActionId`
  action, which the app calls with `X-Chappa-Actor: human:<name>` so the
  acknowledgement records who. Besides the Environment view, the Needs
  you row of the block offers that same action (its confirm, its
  required note) while the source answers and says `not-clean`. The app
  polls the view again right after an action answers `200`, so a lifted
  block clears at once.

## Secret scan on the Environment view

A stack source also says whether the folders agents work in, and what it
builds from, hold possible credentials, in a top-level `secretScan`
block beside `dataSafety`:

```json
"secretScan": {
  "instance": "app/0",
  "state": "not-clean",
  "policy": "block-agents",
  "blocksAgents": true,
  "lastScanUtc": "2026-11-15T12:00:00Z",
  "findings": [
    { "rule": "github-pat", "file": ".work/ABC-1/src/api-1a2b3c4d/.env", "line": 1 },
    { "rule": "generic-api-key", "file": ".work/ABC-1/src/api-1a2b3c4d/cfg.txt", "line": 2, "commit": "f6bb19e1c0de…" }
  ],
  "skipped": 0,
  "bound": { "source": "tracker", "key": "ABC-1", "folders": ["/srv/example/.work/ABC-1", "/srv/example/.work/ABC-1/src/api-1a2b3c4d"] },
  "project": "/srv/example",
  "reasons": [{ "component": null, "text": "api worktree: 2 possible secrets (…)" }],
  "ack": null,
  "ackActionId": "secret-scan-ack"
}
```

| field | meaning |
|---|---|
| `instance`, `bound`, `project`, `reasons`, `ack`, `ackActionId`, `lastScanUtc` | as in `dataSafety` |
| `state` | `clean`, `not-clean` (findings), `acknowledged` (findings or an unfinished scan acknowledged), `unscanned` (a bound worktree not scanned yet, or no scanner and no `secrets:` in the definition), `error` (a scan did not finish: no scanner while `secrets:` is declared, a timeout, a scanner error, or the scan's total size limit reached so readable files went unread) |
| `policy` | `secrets.onFinding` (`warn`, `block-agents` default, `block-build`) while the state is `not-clean` or `error`, else `null` |
| `blocksAgents` | the one field the app enforces on: a finding under a policy other than `warn`, or a scan of a bound worktree's files that did not finish (whatever the policy, until acknowledged). An unfinished scan of commits, a build context or env files only warns |
| `findings` | what no acknowledgement covers: rule, file (relative to the stack project), line, and the commit for one found in a commit ahead of the base. Never a value |
| `skipped` | files left out for the per-file size cap or for being links (a warning only) |

A finding's file is named from the stack project's folder, the way the
app's own scan before an agent launch names it, so acknowledging it on
either side (the `secret-scan-ack` action, or the launch dialog) writes
the same entry in `<project>/.work/secrets-baseline.json` and silences
both. `secret-scan-ack` is view-level, `mutates: true`, `note:
"required"`; its confirm lists every finding as `file:line (rule)` and
every unfinished worktree scan with its reason. An acknowledged
unfinished scan lasts until that worktree's next scan succeeds.

The app enforces a `secretScan` block exactly like a `dataSafety` one
(where, new agents, running agents, held deliveries, last known while
down, across restarts, removal; see above), with two differences: a
block in `error` blocks only when `blocksAgents` says so (the source
already decided where an unfinished scan matters), and the never-heard
hold of a new stack source is data safety's alone. A garbled block (no
`state`) fails closed. Every refusal, disabled button and held message
says which check it is ("the secret scan on app/0 …" or "the data on
app/0 … may be real"). A source's Needs you row offers `secret-scan-ack`
while the source answers and says `not-clean` or `error`.

## Run the fixture by hand

```sh
CHAPPA_SOURCE_ADDR=127.0.0.1:5055 \
CHAPPA_SOURCE_TOKEN=$(node -e "console.log(require('crypto').randomBytes(32).toString('hex'))") \
node desktop/sources/fixture/index.mjs
```

Use `--data <dir>` to serve a different set of JSON files (copy
`fixture/data/` and edit). Bodies are re-read on every request. Set
`CHAPPA_FIXTURE_JOB_MS` to change how long a plain job takes to finish
(default 3000). `CHAPPA_FIXTURE_DATA_SAFETY=1` adds the data-safety block
in `data-safety.json` to the Environment answer, with its acknowledge
action (a person only, a note required), so the app's enforcement can be
tried by hand. Then, from another shell, with the same token in
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
