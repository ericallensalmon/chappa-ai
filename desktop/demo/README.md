# Demo profile

`seed.mjs` builds a folder of neutral content for trying the app or taking
screenshots, with a profile of its own, so nothing from your everyday
setup (projects, paths, tokens, agent output) can show up. Node's standard
library only.

```sh
node desktop/demo/seed.mjs <demo-folder>
```

Pick a folder whose path you don't mind on screen (`/tmp/chappa-demo`,
`C:\demo`): project paths show in the trust prompt, and the demo agent's
path shows in Settings ▸ Agents. The script refuses a folder that is not
empty unless it made it; `--force` starts one of its own over.

It writes, all inside that folder:

| path | what |
|---|---|
| `projects/api` | a small HTTP service: `serve` (long-running, starts on open) and `test` (prints results and a run summary, exits) |
| `projects/web` | a dev server, `dev` (long-running, starts on open), and `build` (prints, exits) |
| `projects/docs` | `preview` (long-running), `check` (prints, exits) and `tracker`, a source serving the Work, PRs and Needs you views from a copy of the fixture data in `desktop/sources/fixture` |
| `projects/stack` | the script-driven example stack from `desktop/stack/examples/script`: two pretend services in two groups, run by `node` scripts, no container engine |
| `projects/compose-stack` | only with `--docker-stack`: the compose example from `desktop/stack/examples/local`; its `api` and `web` components build from the demo's `api` and `web` |
| `agent/demo-agent.mjs` | a scripted stand-in for an agent CLI: a short session, a notification when it wants a reply, a run summary when done. It never calls a model |
| `profile/` | the profile: the projects already listed, the demo agent registered as `demo-agent` |

Each stack's source runs `chappa-stack serve` from `PATH`. Build it once
(`cargo build -p chappa-stack` in `desktop/`); the script prints the exact
commands, with the folder you chose, to start the app on each shell, in
this shape:

```sh
# bash / zsh, from desktop/
CHAPPA_PROFILE_DIR="<demo-folder>/profile" PATH="$PWD/target/debug:$PATH" npm run dev
```

```powershell
# PowerShell, from desktop\
$env:CHAPPA_PROFILE_DIR = "<demo-folder>\profile"; $env:PATH = "$PWD\target\debug;$env:PATH"; npm run dev
```

Quit any other running copy of the app first: the control surface listens
on one fixed port, so a second instance runs without one (its agents
cannot reach it through `chappa-ai-mcp`).

Once it is up:

- Open each project and answer its trust prompt; auto-start commands
  start after it.
- Settings ▸ Sources lists **Tracker** and **Stack scripted** (and
  **Stack local** with `--docker-stack`) waiting for review; trust them
  for the Work, PRs, Needs you and Environment views.
- In Environment, **Up** on the scripted stack brings its two rows up
  with nothing but `node`. The compose stack lists its components without
  any engine, but bringing `db` up needs Docker; `api` needs podman, and
  `web` depends on `api`.
- Start `demo-agent` in a project, type a request, and switch to another
  pane: its question arrives as a notification. Reply `yes` and it
  finishes with a run summary.

`--fixture-environment` also keeps the fixture's stand-in Environment
view, which shows a fuller stack without Docker.

To start over, run the script again with `--force`, or delete the folder.
