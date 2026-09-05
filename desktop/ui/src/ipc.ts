// Thin wrapper over Tauri IPC so tests can stub this module and no other
// file imports @tauri-apps/api directly. Also the single source of truth for
// the frontend→Rust DTO shapes: the src-tauri serde
// derives must decode exactly these JSON shapes. All commands are
// snake_case on the wire to match Rust command names; args are the plain
// objects shown.
import { invoke, Channel } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export { Channel };
export type { UnlistenFn };

export function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(cmd, args);
}

export function inTauri(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

// --- DTOs (owns these until the serde derives land) ----------

/** Modifier bits — the wire value of term-core `Mods`: shift=1 alt=2 ctrl=4 super=8. */
export const MOD_SHIFT = 1;
export const MOD_ALT = 2;
export const MOD_CTRL = 4;
export const MOD_SUPER = 8;

/** A semantic key, mirroring term-core `keys::Key`. Internally tagged so the
 *  Rust side derives cleanly with `#[serde(tag = "kind", rename_all = "snake_case")]`. */
export type KeyDto =
  | { kind: "char"; ch: string }
  | { kind: "enter" }
  | { kind: "tab" }
  | { kind: "backspace" }
  | { kind: "escape" }
  | { kind: "up" }
  | { kind: "down" }
  | { kind: "left" }
  | { kind: "right" }
  | { kind: "home" }
  | { kind: "end" }
  | { kind: "page_up" }
  | { kind: "page_down" }
  | { kind: "insert" }
  | { kind: "delete" }
  | { kind: "f"; n: number };

export interface KeyEventDto {
  key: KeyDto;
  mods: number;
}

/** Mouse kinds mirroring term-core `keys::MouseKind`. */
export type MouseKindDto = "press" | "release" | "drag" | "move" | "wheel_up" | "wheel_down";

export interface MouseEventDto {
  kind: MouseKindDto;
  /** 0 left, 1 middle, 2 right; meaningful on press/drag. */
  button: number;
  /** 0-based viewport cell. */
  col: number;
  row: number;
  mods: number;
}

/** Spawn spec for create_terminal; mirrors term-core `pty::PtySpec`. When
 *  `command` is empty the Rust side resolves the platform default shell
 *  (pwsh/powershell on win32, $SHELL otherwise) — the webview can read
 *  neither $SHELL nor PATH. */
export interface PtySpecDto {
  command: string;
  args?: string[];
  cwd?: string;
  env?: Record<string, string>;
  cols?: number;
  rows?: number;
  /** A shell-profile NAME from `Settings.shellProfiles`. Only
   *  meaningful when `command` is empty: the Rust side resolves the profile's
   *  command line instead of the platform default shell. An unknown or
   *  disabled name falls back to the default shell (Rust decides). */
  profile?: string;
}

export type SelectionKindDto = "simple" | "block" | "lines" | "semantic";

/** Viewport point, row 0 = top of screen. */
export interface PointDto {
  row: number;
  col: number;
}

export type SelectionOpDto =
  | { op: "start"; point: PointDto; kind: SelectionKindDto }
  | { op: "update"; point: PointDto; kind: SelectionKindDto }
  | { op: "clear" };

// --- command wrappers -------------------------------------------------------

export interface CreateTerminalOptions {
  spec: PtySpecDto;
  scrollback?: number;
  /** Fired for every binary frame arriving on the channel. */
  onFrame: (buf: ArrayBuffer) => void;
}

export function createTerminal(opts: CreateTerminalOptions): Promise<number> {
  // Raw channel payloads usually arrive as ArrayBuffer, but some webview
  // serialization paths deliver number[] instead (both have been
  // observed) — normalize so decodeFrame always sees an ArrayBuffer.
  const frames = new Channel<ArrayBuffer | number[]>();
  frames.onmessage = (data) => {
    opts.onFrame(data instanceof ArrayBuffer ? data : Uint8Array.from(data).buffer);
  };
  return call<number>("create_terminal", {
    spec: opts.spec,
    scrollback: opts.scrollback,
    frames,
  });
}

export function writeKey(id: number, ev: KeyEventDto): Promise<void> {
  return call("write_key", { id, ev });
}

export function paste(id: number, text: string): Promise<void> {
  return call("paste", { id, text });
}

export function mouse(id: number, ev: MouseEventDto): Promise<void> {
  return call("mouse", { id, ev });
}

export function resize(id: number, cols: number, rows: number): Promise<void> {
  return call("resize", { id, cols, rows });
}

export function scroll(id: number, delta: number): Promise<void> {
  return call("scroll", { id, delta });
}

export function setDisplayOffset(id: number, offset: number): Promise<void> {
  return call("set_display_offset", { id, offset });
}

export function ack(id: number, seq: number): Promise<void> {
  return call("ack", { id, seq });
}

/** Actor-side frame stats for the debug HUD. */
export interface DebugStatsDto {
  framesSent: number;
  bytesSent: number;
  damageRowsLast: number;
  coalescedTicks: number;
  outstanding: boolean;
}

export function debugStats(id: number): Promise<DebugStatsDto | null> {
  return call("debug_stats", { id });
}

export function requestFull(id: number): Promise<void> {
  return call("request_full", { id });
}

/** Adopt an EXISTING live terminal by handing IT this webview's
 *  frames channel (the reverse of createTerminal — the terminal was spawned
 *  by the backend, not by the frontend). Rust swaps the channel in as the
 *  terminal's sink, forces a FULL frame, and answers the rich row facts. */
export function attachTerminal(
  id: number,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<TerminalInfoDto> {
  return call("attach_terminal", { id, frames });
}

export function selection(id: number, op: SelectionOpDto): Promise<void> {
  return call("selection", { id, op });
}

/**
 * Serialize the current selection onto the OS clipboard. Resolves to the
 * copied text, or null when nothing was copied.
 *
 * `skipWhitespaceOnly` (copy-on-select): a selection whose text is
 * blank after trimming must NOT replace the clipboard — an accidental
 * micro-drag would otherwise wipe it. Rust does the trimming and answers null.
 *
 * The key is camelCase: Tauri v2 binds camelCase args for a bare
 * `#[tauri::command]` (verified on host 2026-08-15 — copy-on-select works).
 */
export function copySelection(id: number, skipWhitespaceOnly?: boolean): Promise<string | null> {
  return call<string | null>("copy_selection", { id, skipWhitespaceOnly });
}

export function search(id: number, regex: string | null): Promise<void> {
  return call("search", { id, regex });
}

/** Move the viewport to the next/previous regex match. Wraps
 *  around; no-op when no matches. `dir` is snake_case on the wire to match
 *  the Rust `SearchNavDirDto`. */
export function searchNav(id: number, dir: "next" | "prev"): Promise<void> {
  return call("search_nav", { id, dir });
}

/**
 * Close a terminal. For a docker-exec agent Rust kills and verifies
 * the CONTAINER-side process first and answers the verification
 * (`gone | still-present | container-down | unresolved`); anything else
 * answers null. The app toasts a non-`gone` answer.
 */
export function closeTerminal(id: number): Promise<string | null> {
  return call<string | null>("close_terminal", { id });
}

// --- agents --------------------------------------------------------

/** The tool-type set plus `custom`. */
export type AgentToolType =
  | "claude"
  | "opencode"
  | "codex"
  | "gemini"
  | "copilot"
  | "kimi"
  | "amp"
  | "custom";

/** Where the agent CLI runs — the Rust `Runtime` enum, internally tagged. */
export type AgentRuntimeDto =
  | { kind: "host" }
  | {
      kind: "docker_exec";
      container: string;
      user?: string | null;
      workdir?: string | null;
      /** `-it` (default true); false = `-i` only. */
      tty?: boolean;
      /** Shared across every tool naming this container. */
      max_busy_in_container?: number | null;
    };

/**
 * One registered agent tool (`agent_tools.json`). snake_case on the wire —
 * the same row is the MCP `list_agent_tools` shape. `name`
 * is display only and NEVER parsed; the command line is DERIVED Rust-side.
 */
export interface AgentToolDto {
  /** 0 on an insert; stable and never reused once stored. */
  id: number;
  name: string;
  tool_type: AgentToolType;
  program: string;
  /** Verbatim argv, never re-split. */
  args: string[];
  model: string | null;
  runtime: AgentRuntimeDto;
  /** Merged into the spawn env (document order). */
  env: Record<string, string>;
  enabled: boolean;
  /** Per-tool concurrency; null = unlimited. */
  max_busy: number | null;
  /** `tty` (terminal panel, default) or `json` (the CLI's machine
   *  mode over pipes → typed events → the transcript view). Only types
   *  with a machine mode (claude, opencode) may be `json`. */
  transport: AgentTransport;
}

export type AgentTransport = "tty" | "json";

/** The paste box's answer: a prefilled tool plus what could not be classified. */
export interface ParsedAgentCommandDto {
  tool: AgentToolDto;
  docker_detected: boolean;
  warnings: string[];
}

export interface ContainerStateDto {
  status: "running" | "exited" | "missing" | "unknown";
  started_at: string | null;
  uptime_s: number | null;
  mem_bytes?: number;
  mem_limit_bytes?: number;
}

export type AgentBridge = "ok" | "container-down" | "stale";

/** The `agent` block on a process record. */
export interface AgentMetaDto {
  tool_id: number;
  tool_type: AgentToolType;
  model: string | null;
  runtime: AgentRuntimeDto;
  /** The project the agent was spawned into (attribution + close-project). */
  project_id: number | null;
  /** The registry id of the process that SPAWNED this agent — the
   *  rail nests this row under it; null for a root. */
  parent_process_id: number | null;
  spawned_at_ms: number;
  spawn_uuid: string;
  nspid: "pid-file" | "unresolved";
  container: ContainerStateDto | null;
  bridge: AgentBridge;
  transport: AgentTransport;
  /** The json transport's typed "waiting for input" fact. */
  awaiting_input: boolean;
}

/** One typed event from a json-transport agent (`agent://event`
 *  payload and the `get_agent_events` rows). */
export type AgentEventKind =
  | "turn_started"
  | "turn_ended"
  | "tool_call"
  | "text"
  | "usage"
  | "awaiting_input"
  | "compaction"
  | "error"
  | "raw";

export interface AgentEventDto {
  id: number;
  /** Per-agent, 1-based, monotonic — the `since` cursor. */
  seq: number;
  /** Unix ms. */
  ts: number;
  kind: AgentEventKind;
  payload: Record<string, unknown>;
}

/** The `agent://event` wire object (the event fields plus the routing keys). */
export interface AgentEventEvent extends AgentEventDto {
  event: string;
  term_id: number;
}

/** What `send_agent_input` answers — the NEXT `turn_started`. */
export interface TurnReceiptDto {
  delivered: boolean;
  reason?: string;
  waited_ms: number;
  /** The effective wait (default + cap applied). */
  wait_ms?: number;
  seq_before: number;
  turn_started_seq?: number;
}

export function sendAgentInput(id: number, text: string, waitMs?: number): Promise<TurnReceiptDto> {
  return call("send_agent_input", { id, text, waitMs });
}

export function getAgentEvents(id: number, since = 0): Promise<AgentEventDto[]> {
  return call("get_agent_events", { id, since });
}

export interface SpawnAgentRequestDto {
  agent_tool_id: number;
  project_id?: number;
  name?: string;
  /** Appended to the tool's args VERBATIM. */
  extra_args?: string[];
  prompt?: string;
  force?: boolean;
  cols?: number;
  rows?: number;
  /** Control/MCP-only (no UI toggle). Explicit parent for rail
   *  nesting; null/absent = nest under the spawning actor. */
  parent_process_id?: number | null;
  /** Control/MCP-only. Close the row automatically when the child
   *  exits (an orchestrator firing short-lived workers). */
  close_on_exit?: boolean;
}

export interface PromptReceiptDto {
  delivered: boolean;
  reason?: string;
  waited_ms?: number;
  had_output_within_ms?: boolean;
}

export interface SpawnAgentResponseDto {
  process_id: number;
  term_id: number;
  name: string;
  agent_instructions: string;
  prompt_receipt?: PromptReceiptDto;
  container?: ContainerStateDto;
  agent: AgentMetaDto;
  forced?: string;
}

/** An `agent://bridge` payload: once per bridge transition. */
export interface AgentBridgeEvent {
  event: string;
  term_id: number;
  id: number;
  bridge: AgentBridge;
  container: ContainerStateDto;
}

/** An `agent://reap` payload — ONE notification-center entry per
 *  container a sweep reaped anything in. */
export interface AgentReapEvent {
  container: string;
  count: number;
}

/** `list_agent_tools`: the rows plus the tool types with a machine mode —
 *  the ONE source (Rust's `MACHINE_MODE_TYPES`) for what the Transport
 *  picker may offer `json` for. */
export interface AgentToolsListingDto {
  tools: AgentToolDto[];
  machine_mode_types: readonly AgentToolType[];
}

export function listAgentTools(): Promise<AgentToolsListingDto> {
  return call("list_agent_tools");
}

export function upsertAgentTool(tool: AgentToolDto): Promise<AgentToolDto> {
  return call("upsert_agent_tool", { tool });
}

/** Rejects with a message NAMING the live processes when one references the
 *  tool. */
export function deleteAgentTool(id: number): Promise<void> {
  return call("delete_agent_tool", { id });
}

export function parseAgentCommand(line: string): Promise<ParsedAgentCommandDto> {
  return call("parse_agent_command", { line });
}

/** Spawn an agent into this webview: `frames` is the per-terminal channel
 *  (the project-process precedent). */
export function spawnAgent(
  req: SpawnAgentRequestDto,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<SpawnAgentResponseDto> {
  return call("spawn_agent", { req, frames });
}

// --- rail / multi-terminal ----------------------------------------

/** One live terminal's rail row (list_terminals snapshot). `status` uses
 *  the status vocabulary: starting | running | stopped | exited | failed. */
export interface TerminalInfoDto {
  id: number;
  name: string;
  status: string;
  exit_code: number | null;
  cols: number;
  rows: number;
  seq: number;
  /** Placement facts every spawn kind carries: where an adopted row
   *  belongs (project AGENTS / workspace agents / workspace TERMINALS). */
  kind?: "terminal" | "agent" | "process";
  project_id?: number | null;
  agent_tool_id?: number | null;
  /** The spawning process's registry id — the rail nests this row
   *  under it; null for a root. */
  parent_process_id?: number | null;
}

export function listTerminals(): Promise<TerminalInfoDto[]> {
  return call("list_terminals");
}

/** A `term://created` broadcast payload: every successful spawn
 *  path (frontend spawn, MCP/control spawn_terminal, spawn_agent, project
 *  process start/respawn) announces a live terminal so a webview that did
 *  NOT initiate it can adopt it into a rail row. Deliberately NOT a
 *  `term://*` JsonEvent envelope — it is a plain spawn announcement, not a
 *  per-terminal event stream (there is no seq cursor for it). */
export interface TerminalCreatedEvent {
  term_id: number;
  name: string;
  kind: "terminal" | "agent" | "process";
  project_id: number | null;
  agent_tool_id: number | null;
  /** The spawning process's registry id — the rail nests this row
   *  under it; null for a root. */
  parent_process_id: number | null;
}

/** A `term://closed` broadcast payload: `{term_id, reason}` where
 *  `reason` is `"closed"` (a manual/MCP close, project stop) or
 *  `"close_on_exit"` (the row reaped itself because its child exited with the
 *  spawn's flag). The frontend drops the entry on receipt, so a backend-side
 *  close (MCP `close_terminal`, `close_on_exit`) never leaves a stale row. */
export interface TerminalClosedEvent {
  term_id: number;
  reason: "closed" | "close_on_exit";
}

/** A `term://*` global event payload (registry::JsonEvent flattened): the
 *  event name + per-terminal seq are always present; the rest is the
 *  event-kind-specific data (status/exit_code/title…). */
export interface TerminalEventDto {
  event: string;
  term_id: number;
  seq: number;
  status?: string;
  exit_code?: number | null;
  title?: string;
  /** `term://notify` only: the OSC 9/777/99 message body (OSC 9 sends an
   *  empty `title` and the message as `body`). */
  body?: string;
  [key: string]: unknown;
}

/**
 * A `term://stats` payload: one terminal's process-tree cost, at
 * most once per 2s tick and only on a material change. `cpu_pct` is already
 * normalized 0–100 across all cores Rust-side, so a busy process reads
 * 100 rather than "300%"; `subproc_count` excludes the shell itself.
 *
 * Carries the same `event`/`term_id`/`seq` envelope every other `term://*`
 * event has — the backend could have written the key as `id`, but a second spelling
 * of "which terminal" across the event surface is exactly the kind of split
 * the frontend decodes wrong once and forever.
 */
export interface TerminalStatsEvent {
  event: string;
  term_id: number;
  seq: number;
  cpu_pct: number;
  mem_bytes: number;
  subproc_count: number;
}

/** Subscribe to an app-wide `term://*` event. Resolves to the unlisten fn. */
export function listenEvent<T = TerminalEventDto>(
  event: string,
  handler: (payload: T) => void,
): Promise<UnlistenFn> {
  return listen<T>(event, (e) => handler(e.payload));
}

// --- projects / chappa.yml ---------------------------------------

/** One stored project row (the switcher dropdown entries). */
export interface ProjectInfoDto {
  id: number;
  name: string;
  path: string;
  icon: string | null;
  /** RAW project-default notification level: "all" | "important" |
   *  "none", or null meaning "never set" — which resolves to 'all'. Kept raw
   *  (a plain string) so an unknown value from a hand-edited store degrades
   *  through `notifications.resolveLevel` instead of failing to decode. */
  notificationLevel: string | null;
}

/** One process row of an open project. `status` uses the status vocabulary. */
export interface ProjectProcessDto {
  name: string;
  command: string;
  status: string;
  autoStart: boolean;
  autoRestart: boolean;
  restartWhenChanged: string[];
  termId: number | null;
  exitCode: number | null;
  /** RAW per-process notification-level OVERRIDE; null = inherit the
   *  project default. See `ProjectInfoDto.notificationLevel` on why it's raw. */
  notificationLevel: string | null;
  /** The rest of the yml definition (pre-fills `Edit command…`).
   *  `workingDir` as written in the file, null = project root; `env` in file
   *  order. */
  workingDir: string | null;
  env: Record<string, string>;
  /** Chappa-side toggles (projects.json, never the yml). */
  favorite: boolean;
  disableAutoRename: boolean;
}

/** The `open_project` answer: header + process rows + trust-gate state. */
export interface OpenProjectResult {
  project: ProjectInfoDto;
  trustPending: boolean;
  trustCommands: string[];
  processes: ProjectProcessDto[];
}


/** A `project://process_status` event payload (the process-row lifecycle
 *  source; the registry's `term://status` stays the per-terminal source). */
export interface ProjectProcessStatusEvent {
  project_id: number;
  name: string;
  status: string;
  exit_code: number | null;
  term_id: number | null;
}

/** A `project://start_requested` event payload: the control
 *  surface (agents via chappa-ai-mcp, or curl) asked for a chappa.yml process
 *  start. The spawn needs THIS webview's frames channel, so Rust routes the
 *  request here and the app runs the same start path a rail click takes. */
export interface ProjectStartRequestedEvent {
  project_id: number;
  name: string;
}

// --- settings -----------------------------------------------------

/** One named shell entry offered by the new-terminal flow. `command` is a
 *  full command line (e.g. `powershell.exe -NoLogo`); `enabled` false hides it
 *  from the menu without deleting it. */
export interface ShellProfileDto {
  name: string;
  command: string;
  enabled: boolean;
}

/**
 * The whole settings struct. camelCase on the wire (the Rust struct derives
 * `#[serde(rename_all = "camelCase")]`, like the project DTOs).
 * Units/ranges are the contract — Rust clamps, so the value a `set_settings`
 * RETURNS is the canonical one, never the optimistic patch.
 */
export interface SettingsDto {
  /** Selection-finished → clipboard. Default false. */
  copyOnSelect: boolean;
  /** Ctrl+V (no alt/shift/meta) pastes the clipboard through the
   *  existing paste path instead of sending 0x16 to the terminal. Default
   *  TRUE — deliberately, so existing settings.json files gain the behavior
   *  on upgrade. Frontend-only consumer (term/input.ts reads it live). */
  ctrlVPastes: boolean;
  /** Ctrl+C is ALWAYS an app shortcut — with a selection it copies,
   *  without one it is a no-op — so 0x03 never reaches the pty (stop a
   *  process from its rail row, not with a reflexive ^C). Default TRUE for
   *  the same deliberate-upgrade reason. Cmd+C (mac) keeps today's behavior
   *  in both modes; this setting is about the Windows/Linux Ctrl reflex.
   *  Frontend-only consumer. */
  ctrlCCopyOnly: boolean;
  /** TUI wheel multiplier, 1..=6 (default 3). Applied Rust-side (term-core
   *  `keys.rs::alt_screen_wheel`); normal scrollback stays at system speed. */
  scrollWheelSpeed: number;
  /** Terminal font size in CSS px, 10..=18 (default 14). Real pixels, not a
   *  percentage. */
  fontSize: number;
  /** BARE bundled face name ("Geist Mono" | "JetBrains Mono") — the fallback
   *  STACK is built frontend-side (settings.ts `terminalFontStack`). */
  fontFamily: string;
  /** Cell-height multiplier, 1.0..=1.8 in 0.1 steps (default 1.2). Cell height
   *  in CSS px = fontSize × lineHeight. */
  lineHeight: number;
  /** Plant a prompt mark on every unmodified Enter outside the alt screen.
   *  Default FALSE — deliberately opt-in, it guesses. */
  syntheticPromptMarks: boolean;
  shellProfiles: ShellProfileDto[];
  /** Which shell runs chappa.yml command lines: the builtin "cmd"/"sh", or a
   *  shell-profile name. */
  defaultExecProfile: string;
  /** Quiet window (ms, 250..=5000, default 750) after an agent's
   *  first output before a queued `prompt` is written. */
  agentReadyQuietMs: number;
  /** The ready gate's fallback (ms, 1000..=20000, default
   *  5000) — continuous output since the first byte for this long counts as
   *  ready (`prompt_receipt.reason = ready-by-timeout`). */
  agentReadyMaxWaitMs: number;
  /** Seconds of silence (30..=86400, default 900) before a docker-
   *  exec agent whose winsize poke also produces nothing is `stale`. */
  agentStaleAfterS: number;
  /** Byte-stream silence (ms, 1000..=3600000, default 120000) before
   *  an idle timer counts a process as idle. The "trust idle only past 120 s"
   *  rule, made server-side; per-timer `idle_ms` overrides it. */
  idleThresholdMs: number;
  /** The fire-time re-validation window (ms, 0..=600000, default
   *  5000). A byte in this window re-arms the timer instead of firing it. */
  timerConfirmMs: number;
  /** How long a firing waits on the ready gate (ms, 1000..=300000,
   *  default 30000) before recording `delivered: false`. */
  timerDeliveryTimeoutMs: number;
  /** Duplicate-body coalescing window (ms, 0..=600000, default
   *  5000); 0 disables coalescing. */
  timerDedupeMs: number;
  /** How long fired timers stay listable (hours, 1..=720, default
   *  24). */
  timerRetentionHours: number;
  /** Sweep docker containers for marker-bearing orphan processes no
   *  live row owns, at app start and on each bridge probe tick. Default
   *  TRUE. The explicit `reap_orphans` control route is NOT gated by this. */
  agentReapOrphans: boolean;
}

export function getSettings(): Promise<SettingsDto> {
  return call<SettingsDto>("get_settings");
}

/** Full-struct set. Rust validates/clamps and returns the canonical result —
 *  always adopt the RETURNED value, never the value you sent. */
export function setSettings(settings: SettingsDto): Promise<SettingsDto> {
  return call<SettingsDto>("set_settings", { settings });
}

// --- projects / chappa.yml, continued -----------------------------

export function listProjects(): Promise<ProjectInfoDto[]> {
  return call("list_projects");
}

/**
 * Register a directory as a project. `name` is the user's explicit
 * choice from the add modal; null/omitted/blank means "derive it" — the Rust
 * side falls back to the project file's name, then the folder name. Null (the
 * modal's untouched-field answer) serializes to `None`, so it IS the derive-it
 * case on the wire.
 */
export function addProject(
  path: string,
  name?: string | null,
): Promise<ProjectInfoDto> {
  return call("add_project", { path, name });
}

/** Rename a STORED project (✎). Store-only: nothing touches the
 *  project file. */
export function renameProject(id: number, name: string): Promise<void> {
  return call("rename_project", { id, name });
}

/**
 * Open the OS folder picker and resolve the chosen directory as a
 * plain path, or null when the user cancelled.
 *
 * The `inTauri()` guard is the rule: the picker must never be reachable
 * outside Tauri; the plain-browser dev page falls back to the typed-path
 * field only. Outside Tauri this resolves null instead of throwing, so a
 * caller needs no second guard.
 *
 * `title`/`start` are already single lowercase words, so the camelCase binding
 * Tauri v2 applies to bare `#[tauri::command]` args (the `skipWhitespaceOnly`
 * precedent above) is a no-op for them.
 */
export function pickDirectory(title?: string, start?: string): Promise<string | null> {
  if (!inTauri()) return Promise.resolve(null);
  return call<string | null>("pick_directory", { title, start });
}

export function removeProject(id: number): Promise<void> {
  return call("remove_project", { id });
}

export function openProject(id: number): Promise<OpenProjectResult> {
  return call("open_project", { id });
}

/** The trust-gate answer. `run = true` records the current chappa.yml hash as
 *  trusted and the caller proceeds to start the auto-start processes; `false`
 *  declines (the gate re-arms on the next open). */
export function confirmProjectTrust(id: number, run: boolean): Promise<void> {
  return call("confirm_project_trust", { id, run });
}

export function listProjectProcesses(id: number): Promise<ProjectProcessDto[]> {
  return call("list_project_processes", { id });
}

/** Start one project process. `frames` is the per-terminal channel the
 *  registry needs; the Rust side spawns the chappa.yml command through the
 *  platform execution profile. */
export function startProjectProcess(
  id: number,
  name: string,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<number> {
  return call("start_project_process", { id, name, frames });
}

export function stopProjectProcess(id: number, name: string): Promise<void> {
  return call("stop_project_process", { id, name });
}

// --- notification levels ------------------------------------------

/**
 * Persist a notification level. `processName` null/omitted targets the PROJECT
 * DEFAULT; a name targets that command's override. `level` null CLEARS — the
 * override is removed (back to inherit) or the project default is reset (back
 * to 'all'). camelCase args, like `copy_selection`'s `skipWhitespaceOnly`.
 */
export function setNotificationLevel(
  projectId: number,
  processName: string | null,
  level: string | null,
): Promise<void> {
  return call("set_notification_level", { projectId, processName, level });
}

/**
 * Fire an OS notification. NO decision logic Rust-side — the
 * pump stopped notifying on its own, so if the UI does not call this, nothing
 * toasts. `notifications.decide` is the only thing that may call it.
 */
export function osNotify(title: string, body: string): Promise<void> {
  return call("os_notify", { title, body });
}

export function restartProjectProcess(
  id: number,
  name: string,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<number> {
  return call("restart_project_process", { id, name, frames });
}

// --- chappa.yml write-back + chappa-side toggles -------------------

/** The `Edit command…` / `+ Add command` form on the wire (camelCase; the
 *  Rust `ProcessDefDto`). `workingDir` null/blank = project root; `env` is an
 *  ORDERED list of `[key, value]` pairs — an object would be alphabetized by
 *  serde_json on the Rust side and reorder the file's env block on every save
 *. */
export interface ProcessDefDto {
  name: string;
  command: string;
  workingDir: string | null;
  autoStart: boolean;
  autoRestart: boolean;
  restartWhenChanged: string[];
  env: Array<[string, string]>;
}

/** A `project://yml_reloaded` event: the project's chappa.yml was re-read
 *  (external edit, or our own write-back). `error` null = reloaded, re-fetch
 *  the rows; a string = the file no longer parses — show it, and every
 *  mutation stays disabled until a reload succeeds. */
export interface ProjectYmlReloadedEvent {
  project_id: number;
  error: string | null;
  /** The reloaded content's hash is not the trusted one (the gate is
   *  per content hash): Rust stopped any running process whose definition
   *  changed instead of executing the new command, and lists here what Run
   *  would execute. Answer through `confirmProjectTrust`. */
  trust_pending?: boolean;
  trust_commands?: string[];
}

/**
 * Edit (`originalName` set) or add (`originalName` null) one chappa.yml command.
 * Rust validates (blank name/command, name clash, working_dir containment),
 * backs the file up once, writes atomically, reloads, and answers the
 * project's rows after the reload (empty when the project is not open).
 */
export function saveProjectProcess(
  id: number,
  originalName: string | null,
  def: ProcessDefDto,
): Promise<ProjectProcessDto[]> {
  return call("save_project_process", { id, originalName, def });
}

/** Delete one command. A running process is stopped first (Rust-side). */
export function deleteProjectProcess(id: number, name: string): Promise<ProjectProcessDto[]> {
  return call("delete_project_process", { id, name });
}

/** Copy `names` from project `sourceId` into `targetId`'s chappa.yml (only the
 *  target file is written; a clash gets a ` copy` suffix). Answers the
 *  TARGET's rows when it is open. */
export function duplicateProjectProcesses(
  sourceId: number,
  names: string[],
  targetId: number,
): Promise<ProjectProcessDto[]> {
  return call("duplicate_project_processes", { sourceId, names, targetId });
}

/** `Add to favorites` — projects.json only, never the yml. */
export function setProcessFavorite(
  projectId: number,
  processName: string,
  favorite: boolean,
): Promise<void> {
  return call("set_process_favorite", { projectId, processName, favorite });
}

/** `Disable automatic renaming` — projects.json only, never the yml. */
export function setProcessAutoRename(
  projectId: number,
  processName: string,
  disabled: boolean,
): Promise<void> {
  return call("set_process_auto_rename", { projectId, processName, disabled });
}

/** The frames Channel for a project-process spawn (start/restart). Real in
 *  Tauri; outside it (jsdom tests) a benign stub object — the injected api
 *  mock supplies the actual channel behavior, and constructing the real
 *  `Channel` would throw (it needs `__TAURI_INTERNALS__` to register its
 *  callback). */
export function makeProjectFramesChannel(
  onFrame: (data: ArrayBuffer | number[]) => void,
): Channel<ArrayBuffer | number[]> {
  if (!inTauri()) {
    return { onmessage: onFrame, id: -1 } as unknown as Channel<ArrayBuffer | number[]>;
  }
  const ch = new Channel<ArrayBuffer | number[]>();
  ch.onmessage = (data) => onFrame(data);
  return ch;
}

// --- workspace commands -------------------------------------------

/** One workspace-command row: a stored definition + runtime status. There is
 *  no project; rows are addressed by the command NAME (the workspace scope). */
export interface WorkspaceCommandDto {
  name: string;
  command: string;
  status: string;
  autoStart: boolean;
  autoRestart: boolean;
  restartWhenChanged: string[];
  termId: number | null;
  exitCode: number | null;
  /** Absolute path, or null = the user's home dir (empty in the editor). */
  workingDir: string | null;
  env: Record<string, string>;
}

/** A `workspace://status` event payload (the card-status source for the
 *  COMMANDS section; the appointed terminal still emits `term://status`). */
export interface WorkspaceStatusEvent {
  name: string;
  status: string;
  exit_code: number | null;
  term_id: number | null;
}

/** The COMMANDS rows, in stored order. */
export function listWorkspaceCommands(): Promise<WorkspaceCommandDto[]> {
  return call("list_workspace_commands", {});
}

/** Edit (`originalName` set) or add (null) one workspace command. Rust
 *  validates (blank name/command, name clash, working_dir absolute-or-empty),
 *  writes workspace.json atomically, and answers the rows. */
export function saveWorkspaceCommand(
  originalName: string | null,
  def: ProcessDefDto,
): Promise<WorkspaceCommandDto[]> {
  return call("save_workspace_command", { originalName, def });
}

/** Delete one workspace command. A running one is stopped first (Rust-side). */
export function deleteWorkspaceCommand(name: string): Promise<WorkspaceCommandDto[]> {
  return call("delete_workspace_command", { name });
}

/** Start one command. `frames` is the per-terminal channel the
 *  registry needs. */
export function startWorkspaceCommand(
  name: string,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<number> {
  return call("start_workspace_command", { name, frames });
}

export function stopWorkspaceCommand(name: string): Promise<void> {
  return call("stop_workspace_command", { name });
}

export function restartWorkspaceCommand(
  name: string,
  frames: Channel<ArrayBuffer | number[]>,
): Promise<number> {
  return call("restart_workspace_command", { name, frames });
}

/** The frames Channel for a workspace-command spawn (start/restart). Same
 *  stub-in-jsdom discipline as `makeProjectFramesChannel`. */
export function makeWorkspaceFramesChannel(
  onFrame: (data: ArrayBuffer | number[]) => void,
): Channel<ArrayBuffer | number[]> {
  if (!inTauri()) {
    return { onmessage: onFrame, id: -1 } as unknown as Channel<ArrayBuffer | number[]>;
  }
  const ch = new Channel<ArrayBuffer | number[]>();
  ch.onmessage = (data) => onFrame(data);
  return ch;
}

// --- scratchpads (read-only rail list + modal) --------------------

/** One row of `list_scratchpads` (no content). Mirrors the control surface's
 *  `GET /scratchpads` row. */
export interface ScratchpadSummaryDto {
  scratchpad_id: number;
  project_id: number | null;
  name: string;
  revision: number;
  tags: string[];
  archived: boolean;
  created_at: number;
  updated_at: number;
  updated_by: string;
  line_count: number;
  bytes: number;
}

/** The full row (`read_scratchpad`). */
export interface ScratchpadDto {
  scratchpad_id: number;
  project_id: number | null;
  name: string;
  content: string;
  revision: number;
  tags: string[];
  archived: boolean;
  created_at: number;
  updated_at: number;
  updated_by: string;
}

/** Unarchived pads of one project (null = the global pads), newest first;
 *  `includeGlobal` adds the global pads to a project scope (a plain Claude
 *  Code session without CHAPPA_AI_PROJECT_ID writes exactly those). */
export function listScratchpads(projectId: number | null, includeGlobal = false): Promise<ScratchpadSummaryDto[]> {
  return call<ScratchpadSummaryDto[]>("list_scratchpads", { projectId, includeGlobal });
}

export function readScratchpad(id: number): Promise<ScratchpadDto> {
  return call<ScratchpadDto>("read_scratchpad", { id });
}
