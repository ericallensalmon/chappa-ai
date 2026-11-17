# Chappa'Ai

*A window to many worlds.* A desktop workspace for CLI coding agents and the
dev processes around them: one window, many terminals, scoped by project,
with notifications that tell you **which** agent in **which** project wants
attention.

**Status:** 0.1, under active development. Windows-first (ConPTY). The test
suite runs on Windows, Linux and macOS in CI; see [Platforms](#platforms) for
what has and has not been tried by hand.

## What it does

- **Hybrid terminal renderer.** Rust owns the PTY, VT parsing
  (`alacritty_terminal`), grid state and damage tracking. The webview is a
  GPU renderer fed sequence-numbered row patches over a binary frame
  protocol with acknowledgement, backpressure and resync. Glyph atlas and
  WebGL, with a DOM renderer as a fallback.
- **Many terminals, one window.** A rail of panels backed by a registry,
  per-panel throttling, scrollback search, hyperlinks, prompt marks, a
  minimal kitty keyboard protocol, and mouse modes.
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
- **A control surface for agents.** `chappa-ai-mcp` is a stdio MCP server
  that lets an agent list, spawn and drive processes, terminals and agents,
  read output without scraping the screen, and share scratchpads, todos and
  timers with other agents working on the same project.

## A project's own secret-scan config

Before an agent starts in a folder, and in the worktrees a stack builds
from, Chappa'Ai scans for credentials with gitleaks (Settings ▸ Security).
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

## Layout

```
desktop/
  Cargo.toml         # workspace
  term-core/         # engine: pty, actor, key encoding, frame protocol (no Tauri deps)
    tests/           # fixture-driven headless tests
  project-model/     # project file, process definitions, lifecycle, file watcher
  chappa-ai-mcp/     # stdio MCP server over the control surface
  src-tauri/         # thin Tauri v2 shell: commands, registry, channels, notifications
  ui/                # Vite and vanilla TypeScript, no framework
    gldebug.html     # standalone WebGL renderer probe (open via the vite dev server)
  CONTROL_API.md     # generated from the route table
```

## Build and test

Prerequisites: Rust (stable), Node 22+, and the
[Tauri v2 system dependencies](https://v2.tauri.app/start/prerequisites/)
for your platform.

```
cd desktop
cargo test -p term-core -p project-model -p chappa-ai-mcp   # headless
cd ui && npm install && npm test                            # vitest, jsdom
cd .. && npm install && npm run dev                         # the app
```

`npm run build` (in `desktop`) builds the installable bundles for the
platform, with the `chappa-ai-mcp` server inside them beside the app's
executable. A `tauri dev` run finds the server beside the debug build once
`cargo build -p chappa-ai-mcp` (or any workspace build or test run) has made
it, and otherwise on `PATH`.

`src-tauri` needs the platform webview libraries to compile; the other three
crates do not. `term-core`'s pty tests need a POSIX shell and are compiled out
on Windows, where the rest of the suite runs.

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
  `~/Library/Logs/com.chappa-ai.desktop/`.
- **Not tried by hand yet:** the app window itself (rendering, fonts and
  Retina scaling), the keys above, notifications, the folder picker,
  opening files in an editor, Docker stacks, and agent CLIs started from a
  Finder launch. Reports welcome.

## License

MIT, see [LICENSE](LICENSE). The bundled fonts (Geist Mono, JetBrains Mono)
are under the SIL Open Font License; their licenses ship beside them in
`desktop/ui/public/fonts/`.
