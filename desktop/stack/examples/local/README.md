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
`DATABASE_URL` is `postgres://app:app@{host}:{port:db}/app`, where `{host}`
is whatever address that engine's containers reach the host at. Every URL
shown to a person uses `{hostname}` (`dev.example.test`), which resolves to
loopback on the host and to each engine's gateway inside its containers
(an `extra_hosts` entry in the override). Add it to the hosts file once:

```
127.0.0.1 dev.example.test
```

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
which components a given `up` brings up.

Stopping is the reverse order with a grace period
(`compose stop -t 10 <service>`); `compose down` removes an instance's
containers and keeps the `db-data` volume unless removal of volumes is
asked for explicitly.
