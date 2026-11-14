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

The definition validates without errors. It carries one warning on
purpose: `db` is persistent and has no data-safety check, so its data
counts as sensitive. For a throwaway local database that is the honest
floor; a real project adds a `dataSafety` marker or sample hook.

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
