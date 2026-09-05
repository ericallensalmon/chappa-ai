# Chappa'Ai

*A window to many worlds.* A desktop workspace for CLI coding agents and the
dev processes around them: one window, many terminals, scoped by project,
with notifications that tell you **which** agent in **which** project wants
attention.

**Status:** 0.1, under active development. Windows-first (ConPTY); macOS and
Linux are untested.

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
  shared like anything else in the repository. Commands can auto-start,
  auto-restart, and restart on file changes.
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

## License

MIT, see [LICENSE](LICENSE). The bundled fonts (Geist Mono, JetBrains Mono)
are under the SIL Open Font License; their licenses ship beside them in
`desktop/ui/public/fonts/`.
