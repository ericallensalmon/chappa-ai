# Chappa'Ai

*A window to many worlds.* A desktop workspace for CLI coding agents and the
dev processes around them: one window, many terminals, scoped by project,
with notifications that tell you **which** agent in **which** project wants
attention.

**Status:** 0.1, under active development. Windows-first (ConPTY). The test
suite runs on Windows, Linux and macOS in CI; see [Platforms](#platforms) for
what has and has not been tried by hand.

<!-- screenshot: assets/screenshots/rail.png — The rail with the ACTIVE area on top, several projects open and their terminal and agent panes, one of them selected -->

## What it does

### Terminals, projects and agents

- **Hybrid terminal renderer.** Rust owns the PTY, VT parsing
  (`alacritty_terminal`), grid state and damage tracking. The webview is a
  GPU renderer fed sequence-numbered row patches over a binary frame
  protocol with acknowledgement, backpressure and resync. Glyph atlas and
  WebGL, with a DOM renderer as a fallback.
- **Many terminals, one window.** A rail of panels backed by a registry,
  per-panel throttling, scrollback search, hyperlinks, prompt marks, a
  minimal kitty keyboard protocol, and mouse modes. Ctrl+click (Cmd+click
  on macOS) a file path in any output to open it in your editor at the line
  it names.
- **Projects and processes.** A project is a directory. Its commands live in
  a `chappa.yml` beside the code, so they can be committed, reviewed and
  shared like anything else in the repository. A command is either a shell
  line (`command: npm run build && npm start`) or a list run directly with
  no shell in between (`command: [cargo, test, --no-fail-fast]`). Commands
  can auto-start, auto-restart, and restart on file changes.
- **Agents as first-class processes.** Registered agent tools spawn a CLI
  agent into a project, on the host or through `docker exec`, with identity
  environment variables, readiness-gated first prompts, busy limits, and
  verified teardown that reaches the process inside the container. An agent
  that spawns another agent nests under it in the rail.
- **Notifications that carry attribution.** Terminal notification escapes
  become in-app banners and a notification center entry naming the process
  and the project, with the focused pane suppressed and per-project and
  per-process levels.
- **A cross-project ACTIVE area.** Everything running or wanting attention,
  across every open project, at the top of the rail.

<!-- screenshot: assets/screenshots/agent-notification.png — An agent pane asking whether to go ahead with its plan, with the notification banner naming the agent and its project -->

### Records, resume, run summaries and the dashboard

- **Every row has a record.** Commands, agents and terminals keep a durable
  record across restarts: their last runs with status, times, exit code
  and the tail of their output. A pane that exited stays scrollable until
  you close it.
- **Resume.** An agent row whose session id is known offers **Resume last
  session**: the agent CLI continues its own saved conversation. Claude
  Code and OpenCode come with resume settings; any other CLI can be given
  its resume arguments and a way to report its session id (its JSON
  stream, a pattern over its output, or `OSC 777;session;<id>`). When the
  CLI refuses the session you see the refusal; the app never starts a
  fresh one in its place.
- **Run summaries.** A process can leave a short Markdown summary of what a
  run did (`OSC 777;summary;<text>`, or `set_summary` over MCP). The row
  shows it after the process is gone.
- **Dashboard.** Every row of the open projects as a card, grouped by
  project: last run, summary, session, live status, CPU and memory, with a
  filter and each card's run history.

<!-- screenshot: assets/screenshots/dashboard.png — The dashboard: cards grouped by project, each with its last run, its summary and a Start or Resume button -->

### Sources: Work, PRs, Needs you and Environment

A source is a local process that tells the app about work items, pull
requests, things that need attention and the state of an environment, as
JSON on a loopback port. The app owns the views, polling, notifications and
every safety check; the source owns the data. A source is a `chappa.yml`
command marked `source: {}` (or a server started elsewhere and attached in
Settings ▸ Sources), and nothing is polled until you have reviewed and
trusted what it asks for.

- **Work**: work items from every trusted source, the active one pinned on
  top and the rest grouped by status, with each item's actions, its local
  workflow and the pull requests joined to it. Several sources can add to
  one item.
- **PRs**: your pull requests and the ones you review (**My PRs**,
  **Reviewing**, **All**).
- **Needs you**: everything waiting on you across terminals, agents and
  sources, with a count on its rail entry.
- **Environment**: each stack as a header (state counts, endpoints
  answering, facts, alerts, data-safety and secret-scan status, the
  stack-wide actions) over a dense table: one line per component (state,
  mode with its commit and ref, uptime, probe, links) in folding groups,
  with each row's actions on hover and a page with everything about it.

Bundled: a GitHub source (`desktop/sources/github`: issues assigned to you
and your pull requests, through the `gh` CLI, so no token is held) and a
fixture source that serves every view from JSON files. Writing your own, in
any language: [`desktop/sources/README.md`](desktop/sources/README.md).

<!-- screenshot: assets/screenshots/work.png — The Work view: items grouped by status, the active item on top, one item open with its actions and joined pull requests -->

<!-- screenshot: assets/screenshots/needs-you.png — Needs you: waiting agents and source alerts, each with its action -->

### Stacks

`chappa-stack` runs a project's development stack (an API, a web app, a
database) as a source. A stack is defined under `stacks:` in `chappa.yml`
and runs in several **slots** at once, each an independent instance with
its own ports, containers and data. Components run through the `compose`,
`process`, `script` or `terraform` driver, in prebuilt, source or dev mode.
The Environment view shows them, and its actions run as jobs in a pane,
asking first with the plan as the question.

- **Verbs where they belong.** Stack-wide verbs (bring up, start, stop,
  down, rebuild) sit once in the stack's header, a group's on its header,
  and a component's (restart, rebuild, switch mode, watch, logs) on its
  row.
- **Selective bring-up.** The stack's Up opens a picker: the default set,
  a named selection from the definition's `selections:`, or the groups and
  components you tick.
- **Script-driven stacks.** A `script` component can wrap a whole existing
  stack: its status script reports any number of rows in groups of its
  own, a verb whose argv names `{row}` becomes a verb of each reported row
  (the row's name passed as one argument, never through a shell), and an
  `up` that takes `{rows}` gets a picker of rows to bring up.

- **Generated instance secrets.** A definition names `{secret:<name>}`
  instead of a password; each instance gets its own random value, kept
  owner-only under `.work/instances/<instance>/secrets/` and never in a
  committed file. Wherever the app shows what the stack does, the value
  appears as `••••`.
- **Work items and review workspaces.** Starting work on an item creates
  its worktrees under `.work/<KEY>/`; a pull request can be checked out
  with its own environment for review.
- **Saved states.** What each component runs, and a snapshot of its data,
  saved per work item and resumed later.
- **Data-safety checks** on components that keep data (see the
  [safety model](#safety-model)).

Two runnable examples, each with how to run it in the app:
[`desktop/stack/examples/local`](desktop/stack/examples/local/README.md)
(compose and a host process, with every feature above; container
components need Docker or podman) and
[`desktop/stack/examples/script`](desktop/stack/examples/script/README.md)
(a stack run by its own scripts, two rows in two groups, needing only
`node`).

<!-- screenshot: assets/screenshots/environment.png — The Environment view: a stack's header with its counts and actions over the component table, rows in folding groups with state, mode, uptime, probe and links -->

### A control surface for agents

`chappa-ai-mcp` is a stdio MCP server that lets an agent list, spawn and
drive processes, terminals and agents and read their output without
scraping the screen. Agents working on the same project share
**scratchpads** (project-scoped notes with revisions, tags and sections),
**todos** (with blockers, locks and comments) and **timers** (wake a
process at a time, or when one or all of a set of processes go idle). The
HTTP API under it is listed in [`desktop/CONTROL_API.md`](desktop/CONTROL_API.md).

## Safety model

Agents run with your authority, and a cloned repository can name any
command. The rule: nothing a repository says runs before you have seen it,
and nothing it holds can quietly widen what an agent gets.

<!-- screenshot: assets/screenshots/trust-prompt.png — The trust prompt for a newly opened project, listing its auto-start commands with their environment and working folder -->

**Project trust.** Opening a project whose `chappa.yml` you have not
trusted shows every command it would auto-start, with its environment and
working folder, and runs nothing until you answer. What you trust is a hash
of exactly what was shown: every byte of `chappa.yml` (commands,
environment, working folders, the `stacks:` definition and everything
else) and, for a project that runs the stack runner, a hash of the whole
parsed stack definition (a `chappa.stack.yml` beside it included, and the
`processes:` entries its components name). For a project that names its own
[secret-scan config](#a-projects-own-secret-scan-config), that file's
content is part of the trust too. Any change asks again. Other files the
definition names (scripts, compose files, Dockerfiles, rules files) are
not part of the hash.

- **Read-only while untrusted.** Auto-start commands wait for the prompt.
  A running command whose definition changed on disk is stopped and not
  restarted until the new content is trusted. A source in an untrusted
  project is not started. The stack runner of an untrusted definition
  serves its views read-only: it runs no scripts, hooks, scans or jobs,
  and refuses every stack action.
- **Sources** are trusted on their own: you review the views a source
  offers, the link hosts it may open, the folders it may launch in and the
  actions it adds to other sources' rows, and any change asks again. The
  app sends a source its token only after the source has proved it knows
  it, opens links only to approved hosts, checks every folder an action
  names against your open projects, and shows an agent's prompt before it
  runs.
- **The per-project secret-scan switch** (Settings ▸ Projects: app
  setting, always, or off) is your preference, kept in the app's profile
  and not in the project, so it is not part of the trust hash. What a
  project's tree can say about scanning is its own config file, below,
  and that is trusted like a command.

**Secret scanning.** Before an agent starts in a folder, the folder is
scanned with [gitleaks](https://github.com/gitleaks/gitleaks), untracked and
git-ignored files included. A finding blocks the launch until it is fixed
or acknowledged (or only warns, if you choose that). The stack runner also
scans the worktrees it creates and their commits ahead of the base, every
build context before a build, and env files and the instance environment
before anything starts. Findings name a rule, a file and a line, never a
value. Acknowledgements are kept per project in
`.work/secrets-baseline.json`, shared by the app and the stack runner. A
value the stack runner generated for an instance (`{secret:<name>}`, see
[Stacks](#stacks)) that turns up in a commit or a tracked file of a bound
worktree is reported too (rule `generated-secret`): it has left the work
folder.

- **The configuration is the app's, never the tree's.** gitleaks always
  runs with an explicit config: the one named in Settings ▸ Security, or
  the built-in default (gitleaks' rules plus rules for inline
  credentials). A `.gitleaks.toml` or `.gitleaksignore` inside a scanned
  folder is never used, and `GITLEAKS_CONFIG` / `GITLEAKS_CONFIG_TOML` are
  removed from every environment the app and the runner start.
- **Without gitleaks**, the switch on *Automatic* leaves launches
  unscanned; on *Always* each launch stops and goes ahead only with
  **Launch anyway** and a note, which the agent's row records.

<!-- screenshot: assets/screenshots/settings-security.png — Settings, Security tab: the secret-scan switch, what happens on a finding, and the gitleaks executable and config -->

### A project's own secret-scan config

A project can bring its own gitleaks config, named in `chappa.yml` relative
to the project folder:

```yaml
secretScan:
  config: tools/gitleaks.toml
```

The config must keep gitleaks' default rules. It is stacked on top of the
rules a scan uses without it, never in place of them: on Chappa'Ai's own
rules (gitleaks' defaults plus rules for credentials written inline), or,
when Settings ▸ Security names a config, on that one.

```toml
# tools/gitleaks.toml
[extend]
useDefault = true

# A token-shaped value the test fixtures use on purpose, let through by
# exact match. ([_] matches "_"; written that way so this file does not
# itself look like a token.)
[allowlist]
description = "the fixtures' public test token"
regexes = ['''^ghp[_]0123456789abcdefABCDEF0123456789abcd$''']
```

The file sits in the tree it configures the scan of, so it is trusted like
a command:

- Its content is part of the project's trust. The trust prompt shows it,
  lists every allowlisted `paths` pattern as "skips these files entirely"
  and every rule in `[extend] disabledRules` as "turns off these rules"
  (those rules then no longer fire anywhere in the project), and, when it
  changed, shows what changed since it was last trusted. An edit of the
  file asks again, exactly as an edited command does.
- It must contain `[extend] useDefault = true`, and `[extend] path` is
  refused (it would pull in a file the trust does not cover). A file that
  breaks a rule is shown as an error and never used. A Settings config
  that itself extends another file cannot have a project config stacked on
  it (gitleaks follows only two levels); the Settings config is then used
  alone and the scan says so.
- Scans use, in order: the project's config while it is trusted and
  unchanged, else the Settings ▸ Security config, else the built-in rules.
  Falling back can only find more. While the project's config is not used,
  the Environment view and the agent launch say so and ask you to review
  it.
- A Settings ▸ Security config inside an open project folder is refused:
  use the project's own config instead. `.gitleaksignore` files and any
  other gitleaks config in the tree are never read.

### Data safety

A stack component that keeps data can name hooks that
print a scrub marker and a sample of its rows, and rules decide whether the
data looks real (card numbers, addresses at real e-mail domains, ...).
Only counts are ever reported. While a check says the data may be real,
under the `block-agents` policy no new agent starts in the folders bound
to that instance, and what MCP, the control surface or a timer sends a
running agent there is held until the block lifts (what you type is never
held). A check that cannot be judged, or a source that stops answering,
keeps the last block in force: it fails closed.

### Smaller guards

- **Masking.** Credential-shaped values (an inline password, a URL with
  credentials) are masked wherever a command, environment or argument list
  is shown: the trust prompt, process rows and the editor, agent and
  action command lines, definition errors. Values the stack runner
  generated for an instance are shown as `••••` everywhere the app shows
  what a stack does (the Environment view, job panes and their logs, the
  `logs` terminals); the file under `.work/` is the one place to read
  them. The trust hash still covers the real value.
- **Agent CLI settings files.** Some agent CLIs store every command you
  approve, secrets included, in `.claude/settings.local.json`. When a
  project, an agent's folder or a stack worktree holds one that is not
  git-ignored, or is committed, the app warns on project open, agent
  launch and agent exit, and offers to add it to `.gitignore`.
- **`.work/` stays local.** The stack runner keeps its state, secrets and
  worktrees under `.work/` and writes a `.work/.gitignore` that ignores
  everything in it; validation also warns when `.work/` is not ignored.
- **A source's token stays with the source.** `CHAPPA_SOURCE_ADDR` and
  `CHAPPA_SOURCE_TOKEN` are removed from the app's own environment at
  startup, and the stack runner removes them from everything it starts, so
  code built from a worktree cannot call the source with your authority.
- **The control surface** listens on `127.0.0.1:8324` only, requires a
  bearer token kept in an owner-only file, and proves its identity before
  `chappa-ai-mcp` sends that token. The app's config folder is owner-only.
- **Clipboard.** A program that asks to set the clipboard (OSC 52) needs
  your OK by default; Settings ▸ Terminal can turn that off or always
  allow it.

### Opt-in switches

Both are off unless set when the app starts.

| variable | what it does | what it exposes |
|---|---|---|
| `CHAPPA_DEBUG_HTTP=1` | debug builds only: a test-harness HTTP server on `127.0.0.1:8323` | it has no token: any local process can list, start, type into, read and close terminals. Requests that look like they come from a web page are refused |
| `CHAPPA_RICH_CRASH_DUMPS=1` | native crash dumps (Windows) include heap memory and global data | whatever was in memory, secrets included: treat those dumps as secret |

## Profiles

`CHAPPA_PROFILE_DIR=<folder>` keeps all of the app's state under that
folder instead of the platform's per-user folders: settings, the projects
list, records, sources, agent tools, the coordination database and the
control token and endpoint (in `<folder>/config`), logs and crash reports
(`<folder>/logs`) and the webview's storage (`<folder>/webview`). Nothing
in the everyday profile is read or written. Use it for a second setup, a
demo, or testing. A relative path is taken from the app's working folder,
so prefer an absolute one.

```sh
CHAPPA_PROFILE_DIR="$HOME/chappa-demo/profile" npm run dev   # from desktop/
```

Processes the app starts inherit the variable, so `chappa-ai-mcp` finds the
same instance. The control surface's port is fixed, so run one instance at
a time: a second one starts without its control surface. On macOS the
webview cannot be given a folder, so under a profile it keeps its own
storage (a remembered settings tab, collapsed sections) in memory only.

[`desktop/demo`](desktop/demo/README.md) builds a demo folder with neutral
projects, the example stack, a fixture source, a scripted stand-in agent
and a profile that lists them.

## Layout

```
.github/workflows/   CI: tests and clippy on Linux, Windows and macOS; fmt; the UI; secret scans
chappa.yml           this repository's own project file (the app in dev mode)
desktop/
  Cargo.toml         Rust workspace
  term-core/         engine: pty, actor, VT parsing, key encoding, frame protocol (no Tauri deps)
  project-model/     chappa.yml, the projects store and records, process lifecycle, file watcher
  stack-core/        stacks: definition, planning, ports, secrets, data-safety and secret-scan engines
  chappa-stack/      the stack runner: serves the Environment view, runs stack jobs
  chappa-ai-mcp/     stdio MCP server over the control surface
  src-tauri/         Tauri v2 shell (package chappa-ai-desktop): commands, registry, sources, trust
  ui/                Vite and vanilla TypeScript webview, no framework
  sources/           the source contract, the GitHub source, a fixture source, their tests
  stack/             the starter data-safety rules and two example stacks (compose, script)
  demo/              builds a demo folder and profile
  vendor/            a patched copy of tauri-runtime-wry (see desktop/Cargo.toml)
  CONTROL_API.md     the control surface's routes, generated from the route table
```

## Build and test

Prerequisites: Rust (stable), Node 22+, and the
[Tauri v2 system dependencies](https://v2.tauri.app/start/prerequisites/)
for your platform. Optional: [gitleaks](https://github.com/gitleaks/gitleaks)
for secret scanning, Docker (or podman) for stacks, and the
[GitHub CLI](https://cli.github.com/) for the GitHub source.

Run the app:

```sh
cd desktop
npm install && npm install --prefix ui
npm run dev
```

The checks CI runs, all from `desktop/`; the Rust ones on Linux, Windows
and macOS:

```sh
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
npm run test:sources                              # the bundled sources, Node only
cd ui && npm ci && npx tsc --noEmit && npm test   # vitest
```

`npm run build` (in `desktop`) builds the installable bundles for the
platform, with the `chappa-ai-mcp` server inside them beside the app's
executable. A `tauri dev` run finds the server beside the debug build once
`cargo build -p chappa-ai-mcp` (or any workspace build or test run) has made
it, and otherwise on `PATH`.

Tests that need `git`, `node`, `gitleaks` or a Docker daemon skip when the
tool is missing; CI installs them and fails if one is absent, and runs the
real-Docker tests on Linux only (`CHAPPA_TEST_DOCKER=1`). `term-core`'s pty
tests need a POSIX shell and are compiled out on Windows. Every desktop
test target links the whole webview stack, so a full test build is heavy:
on a machine with 16 GB or less, `CARGO_BUILD_JOBS=2` and
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` keep it within memory.

## Platforms

- **Windows** is where the app is used day to day.
- **Linux** builds and passes the test suite in CI; the app itself has not
  been tried by hand.
- **macOS** 13 or later builds and passes the test suite in CI on Apple
  Silicon, see below. Intel Macs are not built or tested.

Every bundle ships the `chappa-ai-mcp` server beside the app's executable:
in the install folder on Windows, in `/usr/bin` from the Linux packages, and
in `Contents/MacOS` inside the macOS app. The app finds it there for its own
hooks; register it with an agent CLI by that path.

### macOS

Supported, with these notes:

- **Unsigned for now.** Every push to `main` builds an unsigned Apple
  Silicon `.dmg`,
  attached to the CI run as the `chappa-ai-macos-unsigned` artifact. macOS
  blocks an unsigned app the first time: Control-click (or right-click) the
  app in Finder and choose **Open**, then **Open** again. On macOS 15 and
  later that menu no longer offers it; open the app once, then go to
  **System Settings → Privacy & Security** and click **Open Anyway**.
- **Keys.** Cmd is the app's modifier: Cmd+C copies a selection, Cmd+V
  pastes, Cmd+A selects the whole buffer, and the app shortcuts that are
  Ctrl chords elsewhere are Cmd chords (Cmd+Shift+T for a new terminal,
  Cmd+F to search). Cmd+W closes the pane, not the window; Cmd+Q quits.
  Every Ctrl chord goes to the terminal, so Ctrl+C interrupts. Option types
  the character it makes on your keyboard layout, unless **Settings ▸
  Terminal ▸ Option is Meta** is on: then Option+key sends Esc and the key,
  as Alt does elsewhere (Option+B and Option+F move by word in a shell).
- **PATH.** Started from Finder or the Dock, the app reads the PATH from
  your login shell (`$SHELL -l`) once at startup and adds
  `/opt/homebrew/bin` and `/usr/local/bin`, so tools installed with
  Homebrew or a version manager are found by panes, agents and git. When
  `git`, `node`, `docker` or `npm` is still missing after that, it asks
  once more as an interactive login shell (`$SHELL -i -l`), which also
  reads `~/.zshrc`. Each read gives up after 3 seconds.
- **Agents' MCP server.** `chappa-ai-mcp` ships inside the app, at
  `chappa-ai.app/Contents/MacOS/chappa-ai-mcp`. Register it with an agent
  CLI by that full path, for example `claude mcp add chappa-ai --
  /Applications/chappa-ai.app/Contents/MacOS/chappa-ai-mcp`.
- **Stacks** need Docker Desktop (or another engine with the `docker` CLI
  and Compose) running.
- Settings, the control token and logs live in
  `~/Library/Application Support/com.chappa-ai.desktop/` and
  `~/Library/Logs/com.chappa-ai.desktop/`, or under `CHAPPA_PROFILE_DIR`
  (see [Profiles](#profiles)).
- **Not tried by hand yet:** the app window itself (rendering, fonts and
  Retina scaling), the keys above, notifications, the folder picker,
  opening files in an editor, Docker stacks, and agent CLIs started from a
  Finder launch. Reports welcome.

## License

MIT, see [LICENSE](LICENSE). The bundled fonts (Geist Mono, JetBrains Mono)
are under the SIL Open Font License; their licenses ship beside them in
`desktop/ui/public/fonts/`.
