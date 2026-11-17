# Example: a local stack on two engines

A small project whose `chappa.yml` defines one stack, `local`, with two
slots. Each slot is an independent instance (`local-0`, `local-1`) with its
own ports, containers and database volume.

| component | group | runs as | notes |
|---|---|---|---|
| `db` | `core` | docker, `postgres:16` | named volume `db-data`, one host port |
| `migrate` | `core` | docker, `postgres:16` | `kind: oneshot`: waits for `db`, creates a table, exits; judged by exit code |
| `api` | `apis` | podman, `registry.example/api:{channel}` | also `source` (built from `../api`) and `dev` (`api-dev` / `api-watch`); probed on `/health/ready` |
| `web` | `core` | host process `web-dev` from `../web` | probed with the `web` rule (2xx or 3xx) |

```
chappa.yml               processes + the `stacks:` definition
stack/core.compose.yml   db + migrate (no published ports)
stack/apis.compose.yml   api (no published ports)
stack/data-safety.yml    detection rules: the starter file, two rules on
stack/marker-db.js       db's stand-in scrub-marker hook
stack/sample-db.js       db's stand-in sample hook (prints test fixtures)
.gitignore               .work/ and .chappa/stack-state/ stay local
shared-checkouts-allow.txt  ../web and ../api may be built from directly
```

`registry.example/api` is a placeholder: point `modes.prebuilt.image` at a
real image, or run `api` in `source` mode against a checkout in `../api`.
The `../api` and `../web` folders are not part of the example.

## Ports

Slot `n` takes ports from `14000 + 100 * n`, one per component that
publishes, in definition order:

| | slot 0 | slot 1 |
|---|---|---|
| db | 14000 | 14100 |
| api | 14001 | 14101 |
| web | 14002 | 14102 |

The compose files only `expose` container ports. The per-instance override
file publishes `host:container` for its slot; a base file that published
ports itself would collide between slots, because compose adds `ports`
from every file it merges.

## Two engines, one hostname

`db` runs on docker and `api` on podman, and the two engines share no
network. So `api` reaches the database through the host:
`DATABASE_URL` is `postgres://app:{secret:db}@{host}:{port:db}/app`, where
`{host}` is whatever address that engine's containers reach the host at.
Every URL shown to a person uses `{hostname}` (`dev.example.test`), which
resolves to loopback on the host and to each engine's gateway inside its
containers (an `extra_hosts` entry in the override). Add it to the hosts
file once:

```
127.0.0.1 dev.example.test
```

### When containers can only reach the host's gateway: `relay:`

Some engines (Windows containers under docker, behind NAT) reach the host
only at their gateway address, not at host loopback, so they cannot get
to ports another engine published on `127.0.0.1`. A stack names those
engines under `relay:`, next to `hostname:`:

```yaml
stacks:
  local:
    hostname: dev.example.test
    relay:
      engines: [docker]
```

While the instance has something running, the stack runner then listens
on that engine's gateway address, once per published port, and forwards
each connection to `127.0.0.1:<same port>`. It never listens on `0.0.0.0`.

**Windows Firewall.** The relay is a real listener on an address other
than loopback, so the first time it starts, Windows Firewall asks whether
to allow `chappa-stack` on the network. Choose **Allow** (private
networks are enough: the gateway is a virtual adapter on this machine).
Without it the containers' connections to the relay are dropped and the
components that use it cannot reach the host. The stack runner's other
checks (whether a port is free) only bind a socket without listening, so
they never raise this prompt. This example does not use `relay:` (`api`
reaches the database through `{host}`), so it never asks.

## The database password

No file of the example holds a password, and yours should not either:
`chappa.yml` and the compose files are committed and copied into every
clone. The definition names a generated secret instead:

```yaml
db:
  env:
    POSTGRES_PASSWORD: "{secret:db}"
  outputs:
    url: "postgres://app:{secret:db}@{hostname}:{port:db}/app"
migrate:
  env:
    PGPASSWORD: "{secret:db}"
api:
  env:
    DATABASE_URL: "postgres://app:{secret:db}@{host}:{port:db}/app"
```

The first time an instance is resolved, the stack runner generates a
random value for each `{secret:<name>}` the definition uses and keeps it
in `.work/instances/<instance>/secrets/<name>` (owner-only: mode 0700 / 0600
on Linux and macOS, the current user as the only access entry on Windows;
when that cannot be set, the value is not created and the job fails).
Every `{secret:db}` of that instance is the same value; `local-0` and
`local-1` get different ones. The value reaches the containers through
the generated override file, also under `.work/`, so the compose files
leave `POSTGRES_PASSWORD` and `PGPASSWORD` out.

To see or use the password, read the file. To choose your own, write it
into the file before the first `up`. Postgres takes the password only when
it creates the `db-data` volume: if you delete or change the file while
the volume exists, use **Down and delete data** so the database starts
again with the current value.

The secret scan of the instance environment skips values the runner
generated this way. A password written into `chappa.yml` or a file it
names is still reported. So is a generated value that turns up in a
commit or a tracked file of a bound worktree (rule `generated-secret`, by
file and line, never the value): it has left the work folder.

Everywhere the app shows what the stack does (the Environment view, job
panes and their logs, the `logs` terminals) a generated value is shown as
`••••`, like any other credential. Nothing reveals it there; the file is
where you read it.

## Validation

The definition validates without errors or warnings.

## Data safety

`db` keeps data, so it has a data-safety check. The stack names its
rules (`stack/data-safety.yml`, a copy of the starter file with
`email-real-domain` and `card-number` turned on) and its policy
(`onFinding: block-agents`); `db` names two hooks and when they run
(`after-up` and `every: 6h`):

- `marker: [node, stack/marker-db.js]` prints a scrub marker,
  `{"scrubbed": true, "version": "example-1", ...}`;
- `sample: [node, stack/sample-db.js]` prints sampled rows,
  `[{"table": "customers", "column": "email", "values": [...]}, ...]`.

Both are stand-ins that print fixed test data (documentation-domain
addresses, no card numbers) without touching the database, so they need
`node` on the PATH and the check comes out clean. A real stack runs the
hooks in the container (`[docker, exec, "{container}", ...]`), as the
scripts' comments show.

After every job that starts `db`, the job runs the check and prints the
verdict (counts only, never a value). The source checks again every six
hours while `db` runs, and **Run data-safety check** on the row or in the
**Data safety** facts section checks now. The verdict is kept in
`.work/instances/local-0/data-safety.json` and shown as the payload's
`dataSafety` block, the Data safety facts and, when not clean, an alert,
a `data not clean` badge and a banner saying agents are blocked, with
**Acknowledge findings** (it asks for a note).

To see a finding, add the test card number `4111 1111 1111 1111` to the
`card` values in `stack/sample-db.js` and run the check again.

## Work items and shared clones

A component in `source` or `dev` mode builds from the worktree of the
active work item (Start work on a tracker's Work row creates
`.work/<KEY>/`, Activate binds a slot to it). Building straight from a
shared clone such as `../web` is refused unless the clone is listed in
`shared-checkouts-allow.txt`, so a stack never quietly builds whatever
branch a clone happens to be on. The example has no work items, so it
lists `web` (always `dev`) and `api` there.

## Local status

The `workflow:` block gives every work item a status of its own, shown
on its Work row: To do → In development → In review → Done, with the
moves the block lists as buttons. In review may be entered by anyone
but left only by a person (the app marks the calls its UI makes as a
person's; an agent's are not), and only a person marks an item Done: an
agent asking for it leaves the move waiting for a person's Approve or
Send back. In review → In development needs a note. Each move is kept,
with who made it and when, in the item's `.work/<KEY>/item.json`.

The `trackers:` map says which state a tracker's status label means
(`"*"` = any tracker; a team whose tracker uses other labels adds its
own entry under the tracker's source id). When the tracker says In
Review and the item here is In development, the row gets a banner with
"Move here to In review". The stack only reads the tracker's status; it
never changes it.

## Saved states

While a work item is active in slot 0, its row offers **Save state**:
a small file under `.work/<KEY>/states/<UTC>.json` that records what
each component runs (mode, ref, commit, image digest or content key)
and where its data can be found, never the data. `db` keeps its data in
the `db-data` volume (`data: { strategy: snapshot }`), so a save also
runs its `create` hook, `pg_dump` into the volume, and the state points
at that snapshot. A state is marked **sensitive** when `db`'s last
data-safety check was not clean (or never ran).

A mode switch, `down`, activating another item and archive save the
active item's state first; if that save fails, nothing else runs, and
each of those actions has a "without saving" twin. The row's **Saved
states** section lists them newest first; **Resume** asks `exists` for
each snapshot, shows what differs from what runs now, pulls images by
digest, starts `db` and runs `restore` (data typed in since is replaced
by the snapshot). The newest 20 states per item are kept
(`work.keepStates`), and the newest 10 snapshots of `db`
(`persistence.keep`); pruning runs `delete` for the rest.

`down` keeps the volume. **Down and delete data** is the only action
that removes it, and its confirm lists the saved states whose snapshots
live there.

When an item's branch moved on since a state was saved, its Resume comes
in two variants, and the confirm names each moved repo: **at branch
tips** stays on the branch and builds from where it is now (the code
differs from the save), **exact commits** goes to the saved commit and
leaves that worktree detached. A detached worktree carries a "detached at
… — switch back to …" badge and a terminal that runs `git switch` until
HEAD is on a branch again. A job records who asked for it (the app's
`X-Chappa-Actor`), and a state shows it as "by …".

A component whose data comes from a recipe (`strategy: recipe`) can
define `stateSeeds`; the row's **State seeds** action picks the ones the
item wants (with a box for each `{param:x}`). They are replayed after
scrub and seed when a resume rebuilds that data, and when an up creates
or recreates the component while the item is active.

## Review workspaces

A PR row of a repo the stack has a clone of offers **Review with
environment**: its head is fetched (`refs/pull/<n>/head` on GitHub, the
source branch elsewhere) and checked out under
`.work/reviews/<forge>-<repo>-<n>/`, with a terminal and an agent there,
and **Bring up slot 0** to run the stack from it. The Environment view
lists them in a **Review workspaces** section; remove one when the
review is done (it is refused while it has uncommitted changes).

## Running it in the app

`chappa.yml` has one more command, `stack`, marked `source: {}`: it runs
`chappa-stack serve` from this repository with `cargo run`. Build it once
before opening the project, so the first start is quick enough for the
app's readiness check:

```sh
cargo build -p chappa-stack --manifest-path ../../../Cargo.toml
```

Then open this folder as a project in the app:

1. The project's trust prompt lists the `stack` command (and says it
   receives a source address and token). Trust it; the command starts.
2. Settings ▸ Sources shows **Stack local** asking for review: its link
   hosts are the stack's own `dev.example.test:<port>` addresses and its
   one view is Environment. Trust it.
3. The rail gains **Environment**. Every component is listed (`not
   deployed` ones behind the toggle) with its mode, probe and links; the
   **Stack local · slot 0** section at the bottom holds the stack-wide
   actions (Bring up, Start, Stop, Down, Rebuild all, Clean up images),
   each row its own (Start, Stop, Restart, Rebuild, Switch to …, Watch,
   Logs).
4. An action asks first, with the plan as the question ("Pull db
   (postgres:16); start db."). Accepting opens one pane that runs
   `chappa-stack run-job <id>`: the checks before up, then every step with
   its output, then the outcome. The job's files are under
   `.work/jobs/<id>.{json,log,result.json}`.

Without podman, `api` cannot start; bring `db` up on its own from its row.
`web` starts only once `../web` exists with an `npm run dev`. The source
serves slot 0; another slot is a second command with `serve --slot 1`.

To run the source outside the app (attached mode), start it with a token
file and attach it in Settings ▸ Sources with the printed address:

```sh
cargo run -p chappa-stack --manifest-path ../../../Cargo.toml -- \
  serve --addr 127.0.0.1:0 --token-file .work/source-token
```

## Running it by hand

The stack runner writes the override files and runs these commands for
you. By hand, for slot 0, after the runner (or you) has written
`.work/instances/local-0/core.override.yml` and `apis.override.yml`:

```sh
# core group, on docker
docker compose -p local-0-core \
  -f stack/core.compose.yml -f .work/instances/local-0/core.override.yml \
  up -d --no-deps db
docker compose -p local-0-core \
  -f stack/core.compose.yml -f .work/instances/local-0/core.override.yml \
  up -d --no-deps migrate

# apis group, on podman
podman compose -p local-0-apis \
  -f stack/apis.compose.yml -f .work/instances/local-0/apis.override.yml \
  up -d --no-deps api

# web, from its checkout, with the instance variables
cd ../web && CHAPPA_STACK=local CHAPPA_STACK_SLOT=0 \
  API_URL=http://dev.example.test:14001 PORT=14002 npm run dev
```

`--no-deps` is deliberate: the runner orders the starts itself and decides
which components a given `up` brings up. The override file is also what
carries the database password, so `db` does not start from the compose
file alone.

Stopping is the reverse order with a grace period
(`compose stop -t 10 <service>`); `compose down` removes an instance's
containers and keeps the `db-data` volume unless removal of volumes is
asked for explicitly.
