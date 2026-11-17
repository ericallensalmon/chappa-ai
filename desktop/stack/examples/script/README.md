# Example: a stack run by its own scripts

A small project whose `chappa.yml` defines one stack, `scripted`, with two
slots and a single component, `app`, of the `script` driver. `app` stands
for a whole existing stack: its scripts bring up two pretend services,
`api` and `web`, and its status script reports both, so the Environment
shows them as two rows (`app/api`, `app/web`), in the groups the status
script names ("Back end", "Front end").

The scripts are small `node` programs that keep the services' state in a
file under `.work/fake/<instance>.json` instead of starting anything, so
the example needs only `node` on the PATH: no docker, no podman.

```
chappa.yml           the `stack` source process + the `stacks:` definition
scripts/state.js     the pretend state file, shared by the scripts
scripts/up.js        deploy and start both services (safe to run again)
scripts/stop.js      stop both, web first
scripts/start.js     start what is deployed (fails when nothing is)
scripts/down.js      remove both
scripts/restart.js   restart one service, named by the row
scripts/status.js    print the status JSON
scripts/plan.js      `plan <verb>`: say what <verb> would do
.gitignore           .work/ and .chappa/stack-state/ stay local
```

| verb | script | confirm shows |
|---|---|---|
| Up | `node scripts/up.js` | what `plan up` prints ("api: start on port 15000" …) |
| Start | `node scripts/start.js` | `plan start` |
| Stop | `node scripts/stop.js` | `plan stop` |
| Down | `node scripts/down.js` | `plan down` |
| Restart (per row) | `node scripts/restart.js <row>` | the resolved argv |

The status script fills every column of a row: `group`, `detail` (the
second line), `since` or `uptime`, `restarts`, `source` (`{mode, ref,
commit}`) and `probe` (`{url, rule, status}`; the runner judges it by the
rule, and no `status` is "no answer"). This example pretends its services
answer while they run.

`app` stands for the whole stack, so its Up, Start, Stop and Down are drawn
once, at the top of the Environment, not on every row. Restart's argv names
`{row}`, so it is a verb of each row instead: the app fills in the row's
reported name (`api`, `web`) as one argument, and only for a name the last
status report listed. `rebuild` has no script here, so it is not offered.

## Ports

`app` takes two ports per slot (`ports: 2`): slot 0 has 15000 (api) and
15001 (web), slot 1 has 15100 and 15101. The scripts read them from
`CHAPPA_STACK_APP_PORT` and `CHAPPA_STACK_APP_PORT_1`; `env:` also passes
them as `APP_API_URL` / `APP_WEB_URL`.

## Running it in the app

1. Build the stack runner once (`cargo build -p chappa-stack` from
   `desktop/`): the source command uses `cargo run -q`, and a cold build
   takes longer than the app waits for a source to come up.
2. Open this folder as a project and trust its `chappa.yml`. The `stack`
   process starts and the app offers its source; trust it in Settings ▸
   Sources.
3. Environment: `app/api` and `app/web` show as not deployed (behind the
   "not deployed" toggle) with a "status not read yet" badge for the first
   few seconds, while the status script runs in the background.
4. Up, at the top of the view: the confirm is "Run the `up` script for
   App." and the lines `plan up` printed. Accept, and a pane runs
   `node scripts/up.js` with its output; the next poll shows both rows `up`
   with uptime and an "Open api" / "Open web" link. The view says once,
   under its header, that App reports these rows and runs their actions.
5. Restart on the `api` row (hover it, or its "⋯" menu): the confirm shows
   `node scripts/restart.js api`, and api's uptime starts again.
6. Stop: both rows go `stopped-idle` (gray). Delete `.work/fake/` by hand
   instead, and the rows turn `not-deployed`; edit the state file to say
   `"running": false` while the stack still wants them running, and they
   show red, `stopped-problem`, because the stack's intent says running.
7. Down: both rows go back to not deployed.

## Wrapping an existing PowerShell stack script

A stack that already has a bring-up script keeps it; the definition only
names it per verb. PowerShell files never run on their own, so the argv
names the interpreter:

```yaml
stacks:
  app:
    components:
      stack:
        driver: script
        up:      [powershell, -NoProfile, -ExecutionPolicy, Bypass, -File, scripts/Up-Stack.ps1, -Slot, "{slot}"]
        stop:    [powershell, -NoProfile, -File, scripts/Stop-Stack.ps1, -Slot, "{slot}"]
        down:    [powershell, -NoProfile, -File, scripts/Remove-Stack.ps1, -Slot, "{slot}"]
        status:  [powershell, -NoProfile, -File, scripts/Get-StackStatus.ps1, -Slot, "{slot}", -Json]
        plan:    [powershell, -NoProfile, -File, scripts/Up-Stack.ps1, -Slot, "{slot}", -WhatIf, -Verb]
        repos:   [../api, ../web]          # so a work item gets worktrees of these clones
        timeouts: { up: 45m, status: 30s }
```

What the script has to provide:

- **Arguments, not a shell line.** Each word is passed as is; nothing is
  expanded. Use `{slot}`, `{instance}`, `{port:<component>}` and, for the
  bound work item, `{item}`, `{branch:<repo>}`, `{worktree:<repo>}`.
- **The environment it builds from.** Every script gets
  `CHAPPA_STACK_INSTANCE`, `CHAPPA_STACK_SLOT`, `CHAPPA_STACK_ITEM` (empty
  when no item is bound) and one `CHAPPA_STACK_WORKTREE_<REPO>` per
  worktree of the bound item (`CHAPPA_STACK_WORKTREE_API`). A script that
  reads those instead of a fixed checkout builds the active item's code.
- **An exit code.** 0 is success; anything else fails the job, and without
  a status script the component then shows the failed state.
- **Status JSON on stdout**, nothing else there (write progress to
  stderr): `{"components": [{"name", "state", "since"?, "exitCode"?,
  "detail"?, "links"?}], "facts": [{"label", "value"}], "alerts": [{"tone",
  "text"}]}`, at most 1 MiB, within the status limit (20 s by default).
  `state` is one of `up`, `starting`, `unhealthy`, `crashloop`,
  `stopped-problem`, `stopped-idle`, `oneshot-ok`, `oneshot-failed`,
  `not-deployed`. Links show only when they point at the stack's own host
  name (or loopback when the stack has none), on any port.
- **A plan form** (optional): the `plan` argv is run with the verb
  appended (`… -Verb up`) and should print, without changing anything, what
  that verb would do; the first 20 lines become the confirm. Until it has
  answered, the verb's button is disabled ("Its plan is still being worked
  out") and enables itself on a later poll.
- **Batch files** (`.cmd`, `.bat`) run through `cmd /C`, and no argument
  may then contain `& | < > ^ % "`.

Saved states record a script component only as "not restorable beyond
re-running `up`": resuming one runs its `up` script again, with the
resumed item's worktrees in its environment.

## Running it by hand

From this folder, with `CHAPPA_STACK_INSTANCE=scripted-0`,
`CHAPPA_STACK_APP_PORT=15000` and `CHAPPA_STACK_APP_PORT_1=15001` set:

```
node scripts/status.js
node scripts/plan.js up
node scripts/up.js
node scripts/stop.js
node scripts/down.js
```
