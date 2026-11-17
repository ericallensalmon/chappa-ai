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

`src-tauri` needs the platform webview libraries to compile; the other three
crates do not. `term-core`'s pty tests need a POSIX shell and are compiled out
on Windows, where the rest of the suite runs.

## Platforms

- **Windows** is where the app is used day to day.
- **Linux** builds and passes the test suite in CI; the app itself has not
  been tried by hand.
- **macOS** 13 or later builds and passes the test suite in CI on Apple
  Silicon, see below. Intel Macs are not built or tested.

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
  Cmd+F to search). Every Ctrl chord goes to the terminal, so Ctrl+C
  interrupts. Option types the character it makes on your keyboard layout.
- **PATH.** Started from Finder or the Dock, the app reads the PATH from
  your login shell (`$SHELL -l`) once at startup and adds
  `/opt/homebrew/bin` and `/usr/local/bin`, so tools installed with
  Homebrew or a version manager are found by panes, agents and git. A tool
  that only your `~/.zshrc` puts on PATH (not `~/.zprofile`) is not seen;
  move that line to `~/.zprofile`.
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
