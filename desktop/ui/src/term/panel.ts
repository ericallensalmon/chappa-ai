// Per-terminal panel glue.
//
// Owns ONE terminal: builds the DOM (renderer viewport + hidden textarea +
// IME preview), spawns the process via create_terminal, measures the loaded
// font to size the grid (ResizeObserver → probe span → resize(cols, rows)),
// decodes frames and renders them inside rAF, then acks each rendered frame
// (the Rust side's backpressure gate — ack inside rAF so a slow webview
// cannot be flooded). Requests a FULL frame when visibility/focus returns and
// when a frame fails to decode (seq gap, resize race).
//
// All Tauri calls go through the injected `api` so vitest can stub it; the
// default is the real ipc.ts wrapper.

import { decodeFrame, type CellStore, type Frame, type GridDims } from "./protocol";
import { cssFontFamily } from "./atlas";
import { DomRenderer } from "./renderer_dom";
import { WebGLRenderer } from "./renderer_webgl";
import type { RendererMetrics, TermRenderer } from "./renderer";
import { InputController, isMarkNavShortcut, isSearchShortcut } from "./input";
import { linkAt, LinkIndex, schemeAllowed } from "./links";
import { SearchBar } from "./search";
import { Marks } from "./marks";
import { Hud, type DebugStatsDto } from "./hud";
import { MOD_CTRL, MOD_SUPER, type KeyEventDto, type MouseEventDto, type SelectionOpDto } from "../ipc";
import { openUrl as defaultOpenUrl } from "../opener";
import { registerDismiss } from "../dismiss";
import { settingsStore, terminalFontStack, type SettingsStore } from "../settings";
import { subprocessLabel } from "../stats";
import * as ipc from "../ipc";

export interface TerminalApi {
  createTerminal(opts: ipc.CreateTerminalOptions): Promise<number>;
  writeKey(id: number, ev: KeyEventDto): Promise<void>;
  paste(id: number, text: string): Promise<void>;
  mouse(id: number, ev: MouseEventDto): Promise<void>;
  resize(id: number, cols: number, rows: number): Promise<void>;
  scroll(id: number, delta: number): Promise<void>;
  setDisplayOffset(id: number, offset: number): Promise<void>;
  ack(id: number, seq: number): Promise<void>;
  requestFull(id: number): Promise<void>;
  selection(id: number, op: SelectionOpDto): Promise<void>;
  /** `skipWhitespaceOnly`: a blank-after-trim selection answers null
   *  and leaves the clipboard alone — copy-on-select must not let an
   *  accidental micro-drag wipe it. */
  copySelection(id: number, skipWhitespaceOnly?: boolean): Promise<string | null>;
  search(id: number, regex: string | null): Promise<void>;
  searchNav(id: number, dir: "next" | "prev"): Promise<void>;
  /** Resolves the container-side verification for a docker-exec
   *  agent (`gone | still-present | container-down | unresolved`), null
   *  otherwise. */
  closeTerminal(id: number): Promise<string | null>;
  /** Actor-side frame stats for the debug HUD; null when the term is gone. */
  debugStats(id: number): Promise<DebugStatsDto | null>;
  /** Rail snapshot for hydration (list_terminals). */
  listTerminals(): Promise<ipc.TerminalInfoDto[]>;
  /** Adopt an EXISTING live terminal (backend-spawned) into THIS
   *  panel — Rust swaps the frames channel in as its sink, forces a FULL, and
   *  answers the rich row facts. The reverse of createTerminal. */
  attachTerminal(
    id: number,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<ipc.TerminalInfoDto>;
  // --- project process lifecycle ---------------------------------
  listProjects(): Promise<ipc.ProjectInfoDto[]>;
  /** `name` is the add modal's explicit name; null/omitted/blank
   *  lets the Rust side derive it (project file name, then folder name). Null
   *  is the modal's untouched-field answer and passes through verbatim. */
  addProject(path: string, name?: string | null): Promise<ipc.ProjectInfoDto>;
  /** Rename a stored project (✎ in the switcher). */
  renameProject(id: number, name: string): Promise<void>;
  /** Forget a stored project (the switcher row's 🗑, 2026-08-31 — the
   *  command predates the affordance). Rust stops any live processes and
   *  drops the runtime; the folder and its chappa.yml stay on disk. */
  removeProject(id: number): Promise<void>;
  /** Native folder picker; null = cancelled, or not in Tauri. */
  pickDirectory(title?: string, start?: string): Promise<string | null>;
  openProject(id: number): Promise<ipc.OpenProjectResult>;
  confirmProjectTrust(id: number, run: boolean): Promise<void>;
  listProjectProcesses(id: number): Promise<ipc.ProjectProcessDto[]>;
  startProjectProcess(
    id: number,
    name: string,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<number>;
  stopProjectProcess(id: number, name: string): Promise<void>;
  restartProjectProcess(
    id: number,
    name: string,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<number>;
  // --- notification levels ---------------------------------------
  /** Persist a level. `processName` null = the project default; `level` null
   *  clears (override removed / project reset). */
  setNotificationLevel(
    projectId: number,
    processName: string | null,
    level: string | null,
  ): Promise<void>;
  /** Fire an OS notification. Only `notifications.decide` may authorize this —
   *  Rust does no filtering of its own. */
  osNotify(title: string, body: string): Promise<void>;
  // --- chappa.yml write-back + chappa-side toggles ---------------
  /** Edit (`originalName` set) or add (null) a command; answers the rows
   *  after the reload. */
  saveProjectProcess(
    id: number,
    originalName: string | null,
    def: ipc.ProcessDefDto,
  ): Promise<ipc.ProjectProcessDto[]>;
  deleteProjectProcess(id: number, name: string): Promise<ipc.ProjectProcessDto[]>;
  duplicateProjectProcesses(
    sourceId: number,
    names: string[],
    targetId: number,
  ): Promise<ipc.ProjectProcessDto[]>;
  setProcessFavorite(projectId: number, processName: string, favorite: boolean): Promise<void>;
  setProcessAutoRename(projectId: number, processName: string, disabled: boolean): Promise<void>;
  // --- workspace commands ----------------------------------------
  listWorkspaceCommands(): Promise<ipc.WorkspaceCommandDto[]>;
  saveWorkspaceCommand(
    originalName: string | null,
    def: ipc.ProcessDefDto,
  ): Promise<ipc.WorkspaceCommandDto[]>;
  deleteWorkspaceCommand(name: string): Promise<ipc.WorkspaceCommandDto[]>;
  startWorkspaceCommand(
    name: string,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<number>;
  stopWorkspaceCommand(name: string): Promise<void>;
  restartWorkspaceCommand(
    name: string,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<number>;
  // --- agents ----------------------------------------------------
  listAgentTools(): Promise<ipc.AgentToolsListingDto>;
  upsertAgentTool(tool: ipc.AgentToolDto): Promise<ipc.AgentToolDto>;
  /** Rejects naming the live processes when one references the tool. */
  deleteAgentTool(id: number): Promise<void>;
  /** The "Add from command…" paste box: Rust's command-string parser. */
  parseAgentCommand(line: string): Promise<ipc.ParsedAgentCommandDto>;
  /** Spawn an agent; `frames` is the per-terminal channel. */
  spawnAgent(
    req: ipc.SpawnAgentRequestDto,
    frames: ipc.Channel<ArrayBuffer | number[]>,
  ): Promise<ipc.SpawnAgentResponseDto>;
  /** One user message to a json-transport agent; the receipt is
   *  the CLI's next `turn_started`. */
  sendAgentInput(id: number, text: string, waitMs?: number): Promise<ipc.TurnReceiptDto>;
  /** The event ring since a cursor (transcript catch-up). */
  getAgentEvents(id: number, since?: number): Promise<ipc.AgentEventDto[]>;
}

/** The real adapter; swap in a stub in tests. */
export const tauriApi: TerminalApi = {
  createTerminal: (opts) => ipc.createTerminal(opts),
  writeKey: (id, ev) => ipc.writeKey(id, ev),
  paste: (id, text) => ipc.paste(id, text),
  mouse: (id, ev) => ipc.mouse(id, ev),
  resize: (id, cols, rows) => ipc.resize(id, cols, rows),
  scroll: (id, delta) => ipc.scroll(id, delta),
  setDisplayOffset: (id, offset) => ipc.setDisplayOffset(id, offset),
  ack: (id, seq) => ipc.ack(id, seq),
  requestFull: (id) => ipc.requestFull(id),
  selection: (id, op) => ipc.selection(id, op),
  copySelection: (id, skipWhitespaceOnly) => ipc.copySelection(id, skipWhitespaceOnly),
  search: (id, regex) => ipc.search(id, regex),
  searchNav: (id, dir) => ipc.searchNav(id, dir),
  closeTerminal: (id) => ipc.closeTerminal(id),
  debugStats: (id) => ipc.debugStats(id),
  listTerminals: () => ipc.listTerminals(),
  attachTerminal: (id, frames) => ipc.attachTerminal(id, frames),
  listProjects: () => ipc.listProjects(),
  addProject: (path, name) => ipc.addProject(path, name),
  renameProject: (id, name) => ipc.renameProject(id, name),
  removeProject: (id) => ipc.removeProject(id),
  pickDirectory: (title, start) => ipc.pickDirectory(title, start),
  openProject: (id) => ipc.openProject(id),
  confirmProjectTrust: (id, run) => ipc.confirmProjectTrust(id, run),
  listProjectProcesses: (id) => ipc.listProjectProcesses(id),
  startProjectProcess: (id, name, frames) => ipc.startProjectProcess(id, name, frames),
  stopProjectProcess: (id, name) => ipc.stopProjectProcess(id, name),
  restartProjectProcess: (id, name, frames) => ipc.restartProjectProcess(id, name, frames),
  setNotificationLevel: (projectId, processName, level) =>
    ipc.setNotificationLevel(projectId, processName, level),
  osNotify: (title, body) => ipc.osNotify(title, body),
  saveProjectProcess: (id, originalName, def) => ipc.saveProjectProcess(id, originalName, def),
  deleteProjectProcess: (id, name) => ipc.deleteProjectProcess(id, name),
  duplicateProjectProcesses: (sourceId, names, targetId) =>
    ipc.duplicateProjectProcesses(sourceId, names, targetId),
  setProcessFavorite: (projectId, processName, favorite) =>
    ipc.setProcessFavorite(projectId, processName, favorite),
  setProcessAutoRename: (projectId, processName, disabled) =>
    ipc.setProcessAutoRename(projectId, processName, disabled),
  listWorkspaceCommands: () => ipc.listWorkspaceCommands(),
  saveWorkspaceCommand: (originalName, def) => ipc.saveWorkspaceCommand(originalName, def),
  deleteWorkspaceCommand: (name) => ipc.deleteWorkspaceCommand(name),
  startWorkspaceCommand: (name, frames) => ipc.startWorkspaceCommand(name, frames),
  stopWorkspaceCommand: (name) => ipc.stopWorkspaceCommand(name),
  restartWorkspaceCommand: (name, frames) => ipc.restartWorkspaceCommand(name, frames),
  listAgentTools: () => ipc.listAgentTools(),
  upsertAgentTool: (tool) => ipc.upsertAgentTool(tool),
  deleteAgentTool: (id) => ipc.deleteAgentTool(id),
  parseAgentCommand: (line) => ipc.parseAgentCommand(line),
  spawnAgent: (req, frames) => ipc.spawnAgent(req, frames),
  sendAgentInput: (id, text, waitMs) => ipc.sendAgentInput(id, text, waitMs),
  getAgentEvents: (id, since) => ipc.getAgentEvents(id, since),
};

/**
 * What the App's panel manager needs from a stacked panel (* the transcript view is a second implementation beside TerminalPanel —
 * same host/activation/focus discipline, no renderer, no GL budget).
 */
export interface PanelHost {
  readonly host: HTMLElement;
  start(): Promise<number>;
  setActive(active: boolean): void;
  focusInput(): void;
  pasteText(text: string): void;
  setFontMetrics(family: string, size: number, lineHeight: number): Promise<void>;
  setSubprocessCount(count: number): void;
  readonly glLive: boolean;
  releaseGlContext(): void;
  restoreGlContext(): void;
  dispose(closeTerminal?: boolean): void;
}

export interface TerminalPanelOptions {
  /** Filled in with the terminal DOM (relative-positioned host). */
  container: HTMLElement;
  api?: TerminalApi;
  /** A full CSS font-family STACK (not a bare face name). Defaults to the
   *  stack built from the settings store's chosen face. */
  fontFamily?: string;
  /** CSS px. Defaults to the settings store's font size. */
  fontSize?: number;
  /** Cell-height multiplier (cellH = fontSize × this). Defaults to the
   *  settings store's line height. */
  lineHeight?: number;
  /** Settings source for copy-on-select and the initial font metrics.
   *  Defaults to the app-wide store; tests inject their own. */
  settings?: SettingsStore;
  /** Shell-profile NAME to spawn (new-terminal menu). Only meaningful
   *  for the default create_terminal path. */
  profile?: string;
  /** Platform string for shell selection; tests inject this. */
  platform?: string;
  /** Renderer to construct. Defaults to the `?renderer=dom|gl` query
   *  parameter (then DOM). */
  renderer?: RendererKind;
  /** Spawn root for the shell (Ctrl+Shift+T / spawn_shell). Defaults to the
   *  app's own cwd when omitted. */
  cwd?: string;
  /** Whether this panel is the active (visible) one. Hidden panels — not the
   *  active panel is the third hidden condition — stop acking, and reveal
   *  requests a FULL + focus. Defaults to true (single-panel behaviour). */
  active?: boolean;
  /** Fired the moment create_terminal resolves, so the panel manager can add
   *  its rail row before any `term://status` event races it. */
  onCreated?: (id: number) => void;
  /** Custom spawn for project processes: builds the frames channel
   *  and returns the terminal id from start_project_process — the Rust side
   *  spawns the chappa.yml command, NOT create_terminal's default shell. Without
   *  it, `start()` uses create_terminal (plain shells). */
  spawn?: (
    onFrame: (buf: ArrayBuffer) => void,
    dims?: { cols: number; rows: number },
  ) => Promise<number>;
  /** Open a URL via the OS handler. Defaults to the real
   *  tauri-plugin-opener invoke; tests inject a stub. Scheme gating is the
   *  panel's job (links.ts), never this callback's. */
  openUrl?: (url: string) => Promise<void>;
}

/** Which renderer a panel uses. The DOM renderer stays behind the toggle as
 *  the permanent correctness oracle. */
export type RendererKind = "dom" | "gl";

/** WebGL is the product renderer (PLAN) — default since the
 *  crispness fixes (quantized cells, 1:1 atlas UVs, geometric blocks) made it
 *  strictly better than the DOM oracle. `?renderer=dom` opts back into the
 *  oracle (each panel reads the query once at construction); a failed WebGL2
 *  context still degrades to DOM automatically. */
export function rendererFromQuery(): RendererKind {
  try {
    return new URLSearchParams(window.location.search).get("renderer") === "dom" ? "dom" : "gl";
  } catch {
    return "dom";
  }
}

/** Grid from a container size at the given cell metrics (CSS px). */
export function gridForSize(width: number, height: number, cellW: number, cellH: number): { cols: number; rows: number } {
  return {
    cols: Math.max(1, Math.floor(width / cellW)),
    rows: Math.max(1, Math.floor(height / cellH)),
  };
}

/** Terminal line-height multiplier: 1.2, with letter spacing 1.0 (the bare
 *  advance).
 *  Cell height is fontSize × this, deterministic, rather than the font's own
 *  line box — fonts disagree wildly about their line boxes and reference
 *  parity wants the same grid density. */
export const LINE_HEIGHT = 1.2;

/** DOM-probe cell metrics. Width is the measured advance of "M" QUANTIZED so
 *  cellW × dpr is an integer device-px count — the ONE cell grid every
 *  consumer shares: the PTY resize, mouse→column mapping, overlay geometry,
 *  the GL renderer's quads, and the DOM renderer (which gets the
 *  quantization delta back as CSS letter-spacing, an xterm.js-derived
 *  technique). Quantizing only inside the GL renderer split the app onto two
 *  diverging grids — ~5% per column, glyphs pushed past the pane edge
 *  (host-run). Height is the line-height model in CSS px
 *  (fontSize × `lineHeight`, defaulting to {@link LINE_HEIGHT} = 1.2 — the
 *  setting replaces the constant per panel). jsdom cannot lay out,
 *  so a zero width falls back to the 0.6-em
 *  heuristic (also the pre-font-load measurement). */
export function probeCellMetrics(
  fontFamily: string,
  fontSize: number,
  dpr?: number,
  lineHeight: number = LINE_HEIGHT,
): { cellW: number; cellH: number; letterSpacing: number } {
  const d = dpr ?? (typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1);
  const probe = document.createElement("div");
  probe.style.cssText = "position:absolute;left:-9999px;top:0;visibility:hidden;white-space:pre;";
  const span = document.createElement("span");
  span.style.cssText = `font-family:${cssFontFamily(fontFamily)},monospace;font-size:${fontSize}px;line-height:normal;`;
  span.textContent = "M";
  probe.appendChild(span);
  document.body.appendChild(probe);

  const rect = span.getBoundingClientRect();
  probe.remove();

  const advance = rect.width > 0 ? rect.width : Math.round(fontSize * 0.6);
  const cellW = Math.max(1, Math.round(advance * d)) / d;
  const cellH = Math.round(fontSize * lineHeight * 1000) / 1000;
  return { cellW, cellH, letterSpacing: cellW - advance };
}

export class TerminalPanel {
  readonly host: HTMLElement;
  private readonly api: TerminalApi;
  /** A full CSS font-family STACK (already interpolated through
   *  `cssFontFamily`); mutable — `setFontMetrics` swaps it. */
  private fontFamily: string;
  /** CSS px. */
  private fontSize: number;
  /** Dimensionless: cellH (CSS px) = fontSize × this. */
  private lineHeight: number;
  private readonly settings: SettingsStore;
  private readonly opts: TerminalPanelOptions;
  private readonly rendererKind: RendererKind;

  private viewport: HTMLDivElement;
  private textarea: HTMLTextAreaElement;
  private compose: HTMLDivElement;
  private renderer: TermRenderer;
  private input: InputController;
  private menu!: HTMLDivElement;
  private menuCopy!: HTMLButtonElement;
  /** Unbind for the shared dismiss helper (replaces the hand-rolled
   *  menuDismiss/menuEscape pair, behaviour-identical: Escape included). */
  private menuUnbind: (() => void) | null = null;

  // --- links, search bar, prompt marks --------------------
  private readonly links = new LinkIndex();
  private readonly marks = new Marks();
  private readonly searchBar: SearchBar;
  private readonly linkUnderline: HTMLDivElement;
  private readonly linkBar: HTMLDivElement;
  private readonly flashEl: HTMLDivElement;
  /** The hint bar: a bottom strip
   *  of keycap shortcuts; nothing had needed one yet, so this introduces it
   *  with its first tenant — `N subprocesses` — and the shortcut keycaps fill
   *  it in later. It is an OVERLAY, not a layout row, deliberately: the
   *  renderer sizes its grid off the host box, so a bar that took layout space
   *  would reflow every terminal in the app. Hidden whenever it has nothing to
   *  say, which is the idle-shell case. */
  private readonly hintBar: HTMLDivElement;
  /** Frame header state the search/marks math reads. */
  private lastDisplayOffset = 0;
  private lastHistoryLen = 0;

  private termId = -1;
  /** Frames that arrived before create_terminal returned the id (see onFrame). */
  private pendingFrames: ArrayBuffer[] = [];
  private cellW = 0;
  private cellH = 0;
  private dpr = 1;
  private grid: GridDims | null = null;
  /** Retained cell store, applied in place by decodeFrame (delta application
   *  onto retained arrays — see protocol.ts). Null on a fresh/decode-error
   *  panel; cleared together with `grid`. */
  private store: CellStore | null = null;
  /** Seq of the last frame rendered; null until the first frame lands (also
   *  cleared on decode failure so the resync FULL isn't gap-checked). */
  private lastSeq: number | null = null;
  private hasSelection = false;
  /** Wire flags bit 1: the TUI owns the mouse (suppress local selection/
   *  wheel→scroll unless Shift). Fed to the InputController. */
  private frameMouseCapture = false;
  private hud: Hud;
  /** Panel not rendering (document hidden, zero-size host, or not the active
   *  panel): acking stops so the actor's gate stays closed and damage
   *  coalesces; on reveal a FULL is requested (see refreshHidden). */
  private hidden = false;
  /** Not the active panel = the third hidden condition. */
  private active = true;
  /** GL context released for the >8-context LRU; rendering is
   *  skipped until restoreGlContext recreates the pipeline. */
  private glReleased = false;
  /** Local resync counter (seq gaps + decode failures) for the HUD. */
  private resyncs = 0;
  private ro: ResizeObserver | null = null;
  private onFocus = (): void => {
    this.renderer.focus(true);
    this.refreshHidden();
    if (this.termId >= 0) void this.api.requestFull(this.termId);
  };
  private onBlur = (): void => this.renderer.focus(false);
  private onVisibility = (): void => this.refreshHidden();
  private onContextMenu = (e: MouseEvent): void => this.showContextMenu(e);

  constructor(opts: TerminalPanelOptions) {
    this.opts = opts;
    this.api = opts.api ?? tauriApi;
    this.settings = opts.settings ?? settingsStore;
    // Geist Mono default (OFL, vendored);
    // JetBrains Mono is the bundled fallback. The stack string flows into both CSS
    // font-family and canvas ctx.font (valid in both). The settings store
    // holds a BARE face name; terminalFontStack builds the stack (and never
    // re-quotes an already-complete one).
    const stored = this.settings.get();
    this.fontFamily = opts.fontFamily ?? terminalFontStack(stored.fontFamily);
    this.fontSize = opts.fontSize ?? stored.fontSize;
    this.lineHeight = opts.lineHeight ?? stored.lineHeight;
    this.active = opts.active ?? true;

    const host = opts.container;
    host.classList.add("chappa-term");
    host.style.position = "relative";
    host.style.overflow = "hidden";
    host.style.flex = "1 1 auto";
    host.style.minWidth = "0";
    host.style.minHeight = "0";
    this.host = host;

    this.viewport = document.createElement("div");
    // Fill the host. The renderer's .term layer is position:absolute (out of
    // flow), so an unsized viewport collapses to 0px and overflow:hidden
    // clips every row — content renders but is invisible.
    this.viewport.style.cssText = "position:absolute;inset:0;";
    this.textarea = document.createElement("textarea");
    this.textarea.className = "chappa-textarea";
    this.textarea.style.cssText =
      "position:absolute;left:0;top:0;width:1px;height:1px;opacity:0;border:0;padding:0;" +
      "background:transparent;color:transparent;caret-color:transparent;resize:none;" +
      "outline:none;overflow:hidden;white-space:pre;";
    this.textarea.setAttribute("autocorrect", "off");
    this.textarea.setAttribute("autocapitalize", "off");
    this.textarea.setAttribute("autocomplete", "off");
    this.textarea.setAttribute("spellcheck", "false");
    this.compose = document.createElement("div");
    this.compose.className = "chappa-compose";
    this.compose.style.cssText =
      "z-index:10;font:inherit;white-space:pre;background:#fff;color:#000;padding:0 4px;";
    this.compose.style.visibility = "hidden";

    host.append(this.viewport, this.textarea, this.compose);

    const kind = opts.renderer ?? rendererFromQuery();
    this.rendererKind = kind;
    this.renderer = this.buildRenderer(kind);
    this.renderer.focus(true);

    this.input = new InputController({
      element: this.viewport,
      textarea: this.textarea,
      compose: this.compose,
      cellW: () => this.cellW,
      cellH: () => this.cellH,
      cols: () => this.grid?.cols ?? 0,
      rows: () => this.grid?.rows ?? 0,
      cursorCell: () => this.lastCursor,
      onKey: (ev) => {
        if (this.termId >= 0 && this.active) void this.api.writeKey(this.termId, ev);
      },
      onPaste: (text) => {
        if (this.termId >= 0 && this.active) void this.api.paste(this.termId, text);
      },
      onMouse: (ev) => {
        // Links: a ctrl/cmd+click on a link opens it (and never
        // reaches the terminal); hover drives the underline + URL bar.
        if (ev.kind === "press" && this.handleLinkClick(ev)) return;
        if (ev.kind === "move") this.updateLinkHover(ev);
        else if (ev.kind === "drag" || ev.kind === "press") this.hideLinkHover();
        if (this.termId >= 0 && this.active) void this.api.mouse(this.termId, ev);
      },
      onScroll: (delta) => {
        if (this.termId >= 0 && this.active) void this.api.scroll(this.termId, delta);
      },
      onSelection: (op) => {
        if (this.termId >= 0 && this.active) void this.api.selection(this.termId, op);
      },
      onCopy: () => {
        if (this.termId >= 0 && this.active) void this.api.copySelection(this.termId);
      },
      onSelectionEnd: () => this.copyOnSelect(),
      hasSelection: () => this.hasSelection,
      hasMouseCapture: () => this.frameMouseCapture,
      // The clipboard-key settings, read LIVE so a settings-pane
      // toggle changes this open panel without a rebuild (the same pattern as
      // the copy-on-select UI half below).
      clipboardSettings: () => {
        const s = this.settings.get();
        return { ctrlVPastes: s.ctrlVPastes, ctrlCCopyOnly: s.ctrlCCopyOnly };
      },
      onClipboardPaste: () => this.pasteClipboard(),
    });

    this.hud = new Hud(host, {
      getRemoteStats: () =>
        this.termId >= 0
          ? this.api.debugStats(this.termId)
          : Promise.resolve(null),
      getResyncs: () => this.resyncs,
    });

    // Overlay chrome: the link underline + URL bar + the disallowed-
    // scheme flash, all renderer-agnostic (siblings of the renderer viewport).
    this.linkUnderline = document.createElement("div");
    this.linkUnderline.className = "chappa-link-underline";
    this.linkUnderline.style.cssText =
      "position:absolute;z-index:20;pointer-events:none;border-bottom:2px solid #5ea6ff;" +
      "display:none;";
    this.linkBar = document.createElement("div");
    this.linkBar.className = "chappa-link-bar";
    this.linkBar.style.cssText =
      "position:absolute;left:6px;bottom:6px;z-index:20;pointer-events:none;max-width:80%;" +
      "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;background:#1b1d21;" +
      "border:1px solid #2a2c31;border-radius:4px;padding:2px 6px;color:#9ecbff;" +
      "font:11px ui-monospace,monospace;display:none;";
    this.flashEl = document.createElement("div");
    this.flashEl.className = "chappa-link-flash";
    this.flashEl.style.cssText =
      "position:absolute;top:34px;right:14px;z-index:30;pointer-events:none;" +
      "background:#2d1a1a;border:1px solid #f85149;border-radius:4px;padding:2px 8px;" +
      "color:#f85149;font:11px ui-monospace,monospace;display:none;";
    this.hintBar = document.createElement("div");
    this.hintBar.className = "chappa-hint-bar";
    // Bottom RIGHT: the link bar owns bottom-left, and a hovered link while a
    // build runs must not stack the two on top of each other.
    this.hintBar.style.cssText =
      "position:absolute;right:6px;bottom:6px;z-index:20;pointer-events:none;" +
      "background:#16181d;border:1px solid #23262c;border-radius:4px;padding:2px 6px;" +
      "color:#8b949e;font:11px ui-monospace,monospace;display:none;";
    host.append(this.linkUnderline, this.linkBar, this.flashEl, this.hintBar);

    this.searchBar = new SearchBar({
      container: host,
      onSearch: (regex) => {
        if (this.termId >= 0 && this.active) void this.api.search(this.termId, regex);
      },
      onNav: (dir) => {
        if (this.termId >= 0 && this.active) void this.api.searchNav(this.termId, dir);
      },
      onClose: () => this.closeSearch(),
    });

    this.buildMenu();
    this.viewport.addEventListener("contextmenu", this.onContextMenu);
    // Clicking the terminal returns typing to the terminal via the
    // InputController's own focus reclaim — the search overlay STAYS open
    // with its search live (host-run verdict; Esc is the close-and-clear
    // path, Ctrl+F the way back into the bar).
    document.addEventListener("keydown", this.onFeatureKeydown);
  }

  private buildRenderer(kind: RendererKind): TermRenderer {
    const domOptions = {
      container: this.viewport,
      cellW: this.cellW,
      cellH: this.cellH,
      fontFamily: this.fontFamily,
      fontSize: this.fontSize,
      onScrollbar: (offset: number): void => {
        if (this.termId >= 0) void this.api.setDisplayOffset(this.termId, offset);
      },
    };
    if (kind === "gl") {
      try {
        return new WebGLRenderer({
          container: this.viewport,
          cellW: this.cellW,
          cellH: this.cellH,
          dpr: this.dpr,
          baseline: this.cellH * 0.8,
          fontFamily: this.fontFamily,
          fontSize: this.fontSize,
          onRequestFull: () => {
            if (this.termId >= 0) void this.api.requestFull(this.termId);
          },
        });
      } catch (err) {
        // WebGL2 unavailable (jsdom, odd webview config): the DOM renderer is
        // the permanent fallback oracle — a failed ?renderer=gl degrades.
        console.warn(`[chappa-ai] ${kind} renderer unavailable, falling back to DOM`, err);
      }
    }
    return new DomRenderer(domOptions);
  }

  private lastCursor: { row: number; col: number } | null = null;

  /** Recompute the hidden flag (document hidden OR zero-size host OR not the
   *  active panel). On a hidden→visible transition a FULL is requested: while
   *  hidden we stop acking, so the actor's gate stays closed and damage
   *  coalesces; one resync frame paints the freshest state. */
  private refreshHidden(): void {
    const rect = this.host.getBoundingClientRect();
    const nowHidden =
      document.visibilityState !== "visible" ||
      rect.width === 0 ||
      rect.height === 0 ||
      !this.active;
    if (nowHidden === this.hidden) return;
    this.hidden = nowHidden;
    if (!nowHidden && this.termId >= 0) {
      this.lastSeq = null;
      void this.api.requestFull(this.termId);
    }
  }

  /** Flip active-panel status (the panel manager's `activate`). Reveal →
   *  request_full + focus, per the hidden machinery; the LRU-released
   *  GL context is recreated lazily here too. */
  setActive(active: boolean): void {
    if (this.active === active) return;
    this.active = active;
    if (!active && this.searchBar.isOpen()) this.closeSearch();
    this.restoreGlContext();
    this.refreshHidden();
    if (active) {
      if (this.termId >= 0) {
        this.lastSeq = null;
        void this.api.requestFull(this.termId);
      }
      this.input.focus();
    }
  }

  /** Give the hidden textarea keyboard focus. `setActive(true)` does this on
   *  the inactive→active TRANSITION only; the panel manager calls this for
   *  a click on the already-active card/rail row, where the click itself
   *  just stole DOM focus from the terminal (pane looked focused
   *  but ate no keys). */
  focusInput(): void {
    this.input.focus();
  }

  /** Send text through the normal paste path (bracketed-paste guard applies
   *  Rust-side). Used by the App's tauri://drag-drop route — with Tauri's
   *  default drag-drop interception the webview never sees an HTML5 drop, so
   *  OS file paths arrive via that event, not InputController.onDrop. */
  pasteText(text: string): void {
    if (this.termId >= 0 && this.active) void this.api.paste(this.termId, text);
  }

  // --- the clipboard keys -----------------------------------------

  /**
   * `ctrl_v_pastes`: Ctrl+V pulls the OS clipboard and pushes it
   * through the EXISTING paste path — the same `api.paste` the textarea's
   * paste event uses, so the bracketed-paste guard applies Rust-side via
   * `encode_paste` exactly as for a native paste.
   *
   * The read happens at key time (the input predicate stays pure), and a
   * denied/unavailable clipboard read is a deliberate silent no-op: the
   * shortcut itself must never put anything on the terminal. An empty
   * clipboard is also a no-op (there is nothing to paste).
   */
  private pasteClipboard(): void {
    if (this.termId < 0 || !this.active) return;
    void navigator.clipboard
      .readText()
      .then((text) => {
        if (text !== "") void this.api.paste(this.termId, text);
      })
      .catch(() => {
        /* clipboard read denied / unavailable — deliberate no-op */
      });
  }

  // --- copy-on-select + live font metrics -------------------------

  /**
   * A drag-selection just finished. With `copy_on_select` on, mirror it to
   * the clipboard — but with the skip-whitespace-only flag set, so a plain
   * click or an accidental micro-drag (which also ends a "selection") answers
   * null and leaves the clipboard untouched. A non-null answer flashes
   * "Copied"; null shows nothing at all.
   */
  private copyOnSelect(): void {
    if (this.termId < 0 || !this.active) return;
    if (!this.settings.get().copyOnSelect) return;
    void this.api
      .copySelection(this.termId, true)
      .then((text) => {
        if (text !== null && text !== undefined && text !== "") this.flash("Copied", "ok");
      })
      .catch(() => {
        /* terminal gone / transient IPC failure — nothing to confirm */
      });
  }

  /**
   * Adopt new font metrics live (the settings store's fontFamily/fontSize/
   * lineHeight). `family` is a BARE bundled face name — the terminal STACK is
   * built here (chosen face, the other bundled face, monospace).
   *
   * The new face is awaited before re-measuring: probing an unloaded face
   * measures the FALLBACK (cf. metrics.ts `ensureFontLoaded` and the
   * `document.fonts.ready` await in `start()` — a host-run bug had the
   * whole grid sized in the body's proportional font).
   */
  async setFontMetrics(family: string, size: number, lineHeight: number): Promise<void> {
    this.fontFamily = terminalFontStack(family);
    this.fontSize = size;
    this.lineHeight = lineHeight;
    try {
      // Both the CSS px size and the family matter to the load request; the
      // stack is legal here (the API takes a font shorthand).
      await document.fonts.load(`${size}px ${this.fontFamily}`, "Mg");
    } catch {
      /* fonts API unavailable (jsdom, odd webview) — measure what's loaded */
    }
    this.measureCells();
    this.resizeNow();
  }

  // --- search bar, prompt marks, links ----------------------------

  /** Ctrl+F: open the search overlay, or refocus it when already open. */
  openSearch(): void {
    if (this.termId < 0) return;
    this.searchBar.open();
  }

  /** Ctrl+Alt+↑/↓: jump the viewport to the previous/next prompt mark. */
  navMark(dir: "prev" | "next"): void {
    if (this.termId < 0) return;
    const rows = this.grid?.rows ?? 0;
    if (rows === 0) return;
    const target = this.marks.nav(dir, this.lastDisplayOffset, rows, this.lastHistoryLen);
    if (target === null) return;
    const offset = this.marks.offsetForMark(target, this.lastHistoryLen, rows);
    void this.api.setDisplayOffset(this.termId, offset);
  }

  /** `term://search` event: the "N matches" count (a scan-time snapshot). */
  handleSearchStatus(total: number): void {
    this.searchBar.setCount(total);
  }

  /** `term://stats` event: the hint bar's `N subprocesses`. Shown
   *  only above zero — a plain shell with no children says nothing, and an
   *  always-present "0 subprocesses" is exactly the idle noise the hint bar
   *  must not show. */
  setSubprocessCount(count: number): void {
    const show = count > 0;
    this.hintBar.textContent = show ? subprocessLabel(count) : "";
    this.hintBar.style.display = show ? "block" : "none";
  }

  /** `term://links` event: new OSC 8 ids for this terminal. */
  handleLinks(links: { id: number; uri: string }[]): void {
    this.links.merge(links);
  }

  /** `term://prompt_mark` event: keep only PromptStart rows for nav. */
  handlePromptMark(kind: string, row: number): void {
    if (kind === "prompt_start") this.marks.add(row);
  }

  /** The keyboard: Ctrl+F opens search, Ctrl+Alt+↑/↓ navigates marks.
   *  Document-level (the search bar's own input has focus too); only the
   *  active panel responds. */
  private readonly onFeatureKeydown = (e: KeyboardEvent): void => {
    if (!this.active) return;
    if (isSearchShortcut(e)) {
      e.preventDefault();
      this.openSearch();
    } else if (isMarkNavShortcut(e)) {
      e.preventDefault();
      this.navMark(e.key === "ArrowUp" ? "prev" : "next");
    }
  };

  private closeSearch(): void {
    if (this.termId >= 0) void this.api.search(this.termId, null);
    this.searchBar.close();
    this.input.focus();
  }

  /** Ctrl/cmd+click on a link cell: open the URL when the scheme is allowed,
   *  else flash a status. Returns true when the click was consumed (never
   *  reaches the terminal). */
  private handleLinkClick(ev: MouseEventDto): boolean {
    const linkClick = (ev.mods & (MOD_CTRL | MOD_SUPER)) !== 0;
    if (!linkClick) return false;
    const run = this.store ? linkAt(this.store, this.links, ev.row, ev.col) : null;
    if (!run) return false;
    if (schemeAllowed(run.url)) {
      void (this.opts.openUrl ?? defaultOpenUrl)(run.url);
    } else {
      this.flash(`not opening ${run.url}`);
    }
    return true;
  }

  private updateLinkHover(ev: MouseEventDto): void {
    if (this.frameMouseCapture || !this.store) {
      this.hideLinkHover();
      return;
    }
    const run = linkAt(this.store, this.links, ev.row, ev.col);
    if (!run) {
      this.hideLinkHover();
      return;
    }
    this.linkUnderline.style.display = "block";
    this.linkUnderline.style.left = `${run.colStart * this.cellW}px`;
    this.linkUnderline.style.top = `${(run.row + 1) * this.cellH - 2}px`;
    this.linkUnderline.style.width = `${(run.colEnd - run.colStart + 1) * this.cellW}px`;
    this.linkUnderline.style.height = "2px";
    this.linkBar.textContent = run.url;
    this.linkBar.style.display = "block";
  }

  private hideLinkHover(): void {
    this.linkUnderline.style.display = "none";
    this.linkBar.style.display = "none";
  }

  private flashTimer: ReturnType<typeof setTimeout> | null = null;

  /** Flash tones: "error" is the disallowed-scheme red, "ok" the house
   *  green used by the copy-on-select "Copied" confirmation. */
  private flash(message: string, tone: "error" | "ok" = "error"): void {
    const color = tone === "ok" ? "#3fb950" : "#f85149";
    const background = tone === "ok" ? "#12261a" : "#2d1a1a";
    this.flashEl.style.color = color;
    this.flashEl.style.borderColor = color;
    this.flashEl.style.background = background;
    this.flashEl.textContent = message;
    this.flashEl.style.display = "block";
    if (this.flashTimer !== null) clearTimeout(this.flashTimer);
    this.flashTimer = setTimeout(() => {
      this.flashEl.style.display = "none";
    }, 2000);
  }

  /** The requested renderer kind (what `?renderer=` asked for). */
  get kind(): RendererKind {
    return this.rendererKind;
  }

  /** Whether the panel currently holds a live WebGL context (DOM fallback
   *  panels and LRU-released ones report false). */
  get glLive(): boolean {
    return this.rendererKind === "gl" && !this.glReleased && this.renderer instanceof WebGLRenderer;
  }

  /** Release the GL context (>8-context LRU). The pipeline is torn
   *  down; rendering is skipped until `restoreGlContext`. No-op for DOM
   *  panels. */
  releaseGlContext(): void {
    if (this.rendererKind !== "gl" || this.glReleased) return;
    if (!(this.renderer instanceof WebGLRenderer)) return;
    this.glReleased = true;
    this.renderer.dispose();
  }

  /** Recreate the GL pipeline after an LRU release, then request a FULL so
   *  the (hidden, hence un-acked) panel paints the freshest state on reveal.
   *  Falls back to the DOM oracle if WebGL2 is unavailable again. */
  restoreGlContext(): void {
    if (this.rendererKind !== "gl" || !this.glReleased) return;
    this.renderer = this.buildRenderer("gl");
    this.renderer.focus(true);
    this.glReleased = false;
    if (this.termId >= 0) {
      this.lastSeq = null;
      void this.api.requestFull(this.termId);
    }
  }

  /** Spawn the process, wire the frame channel, start observation. Resolves
   *  with the terminal id once `create_terminal` returns it. Idempotent. */
  async start(): Promise<number> {
    if (this.termId >= 0) return this.termId;
    // Wait for the real font BEFORE measuring and spawning: a spawn at
    // fallback-font size followed by the corrective resize milliseconds
    // later makes ConPTY reflow-repaint the fresh screen, which blanks the
    // shell's first prompt row (host-run bug: the reloaded page showed 19
    // spaces where "PS C:\…> " belonged). Spawning at the final grid means
    // no immediate resize at all.
    try {
      await document.fonts.ready;
    } catch {
      /* fonts API unavailable — probe with whatever is loaded */
    }
    this.measureCells();
    this.resizeNow();

    const spec: ipc.PtySpecDto = {
      // Empty command = the Rust side resolves the platform default shell
      // (pwsh/powershell on Windows, $SHELL elsewhere) — the webview can
      // read neither $SHELL nor PATH, so shell policy lives Rust-side only.
      command: "",
      args: [],
      cwd: this.opts.cwd,
      cols: this.grid?.cols ?? 80,
      rows: this.grid?.rows ?? 24,
      // A shell-profile name from the new-terminal menu. Rust
      // resolves the profile's command line; absent = the default shell.
      profile: this.opts.profile,
    };

    this.termId = this.opts.spawn
      ? await this.opts.spawn((buf) => this.onFrame(buf), { cols: spec.cols ?? 80, rows: spec.rows ?? 24 })
      : await this.api.createTerminal({
          spec,
          onFrame: (buf) => this.onFrame(buf),
        });
    // The panel manager adds its rail row the moment the id is known — before
    // the first term://status event can race it.
    this.opts.onCreated?.(this.termId);
    // The actor's opening frame can outrun create_terminal's returned id;
    // frames held by onFrame while termId was unset are replayed now so the
    // ack/request_full they trigger carries a real id.
    const pending = this.pendingFrames.splice(0);
    for (const buf of pending) this.onFrame(buf);
    this.input.focus();

    if (typeof ResizeObserver !== "undefined") {
      this.ro = new ResizeObserver(() => {
        this.measureCells();
        this.resizeNow();
        this.refreshHidden();
      });
      this.ro.observe(this.host);
    }
    window.addEventListener("focus", this.onFocus);
    window.addEventListener("blur", this.onBlur);
    document.addEventListener("visibilitychange", this.onVisibility);

    // JSON events are per-terminal (search count, link table, prompt
    // marks) — the panel subscribes to its own rather than routing through
    // the App's rail wiring.
    if (ipc.inTauri()) this.wireTermEvents();
    return this.termId;
  }

  /** Subscribe to the per-terminal `term://*` events the features
   *  consume. The handlers are public so tests (not in Tauri) can drive them
   *  directly with the same payloads. */
  private wireTermEvents(): void {
    const on = <T>(event: string, apply: (payload: T) => void): void => {
      void ipc.listenEvent<T>(event, apply).catch(() => {});
    };
    on<ipc.TerminalEventDto>("term://search", (p) => {
      if (p.term_id === this.termId) this.handleSearchStatus(Number(p.total ?? 0));
    });
    on<{ term_id: number; links: { id: number; uri: string }[] }>("term://links", (p) => {
      if (p.term_id === this.termId) this.handleLinks(p.links ?? []);
    });
    on<ipc.TerminalEventDto>("term://prompt_mark", (p) => {
      if (p.term_id === this.termId) this.handlePromptMark(String(p.kind ?? ""), Number(p.row ?? 0));
    });
  }

  /** Tear the panel down. `closeTerminal` false skips the Rust
   *  close — the App already closed the terminal itself to read the
   *  container-side verification, and a second close would 404. */
  dispose(closeTerminal = true): void {
    this.ro?.disconnect();
    this.ro = null;
    window.removeEventListener("focus", this.onFocus);
    window.removeEventListener("blur", this.onBlur);
    document.removeEventListener("visibilitychange", this.onVisibility);
    this.viewport.removeEventListener("contextmenu", this.onContextMenu);
    document.removeEventListener("keydown", this.onFeatureKeydown);
    this.input.destroy();
    if (!this.glReleased) this.renderer.dispose();
    this.hud.dispose();
    this.searchBar.dispose();
    this.hideMenu();
    this.menu.remove();
    this.menuUnbind?.();
    this.menuUnbind = null;
    if (this.flashTimer !== null) clearTimeout(this.flashTimer);
    if (closeTerminal && this.termId >= 0) void this.api.closeTerminal(this.termId);
    this.host.textContent = "";
  }

  /** The (family, size, lineHeight) the current cell metrics were probed at.
   *  Part of the early-return key below. */
  private measuredFamily = "";
  private measuredSize = 0;
  private measuredLineHeight = 0;

  /** Re-probe cell px and re-resize the terminal if the grid changed. */
  private measureCells(): void {
    const dpr = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;
    const probe = probeCellMetrics(this.fontFamily, this.fontSize, dpr, this.lineHeight);
    // The early return keys on the FONT as well as on the geometry: two faces
    // can share an advance width to the pixel (both bundled monos at some
    // sizes; every face in jsdom, which cannot lay out), and keying on
    // cellW/cellH/dpr alone silently skipped the swap — the renderer kept its
    // old atlas and the terminal stayed in the previous face.
    if (
      probe.cellW === this.cellW &&
      probe.cellH === this.cellH &&
      dpr === this.dpr &&
      this.fontFamily === this.measuredFamily &&
      this.fontSize === this.measuredSize &&
      this.lineHeight === this.measuredLineHeight
    ) {
      return;
    }
    this.cellW = probe.cellW;
    this.cellH = probe.cellH;
    this.dpr = dpr;
    this.measuredFamily = this.fontFamily;
    this.measuredSize = this.fontSize;
    this.measuredLineHeight = this.lineHeight;
    const metrics: RendererMetrics = {
      cellW: probe.cellW,
      cellH: probe.cellH,
      dpr,
      // Heuristic baseline until the GL renderer self-corrects via
      // measureCellMetrics once the font is actually loaded (metrics.ts
      // fallback: ascent ≈ 0.8 × fontSize).
      baseline: probe.cellH * 0.8,
      fontFamily: this.fontFamily,
      fontSize: this.fontSize,
      letterSpacing: probe.letterSpacing,
    };
    this.renderer.setMetrics(metrics);
  }

  private resizeNow(): void {
    const rect = this.host.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0 || this.cellW <= 0 || this.cellH <= 0) return;
    const grid = gridForSize(rect.width, rect.height, this.cellW, this.cellH);
    if (this.grid && grid.cols === this.grid.cols && grid.rows === this.grid.rows) return;
    this.grid = grid;
    if (this.termId >= 0) void this.api.resize(this.termId, grid.cols, grid.rows);
  }

  private onFrame(buf: ArrayBuffer): void {
    if (this.termId < 0) {
      // start() has not recorded the terminal id yet — any ack/request_full
      // sent now would carry -1 and be dropped Rust-side, deadlocking the
      // ack-gated frame stream. Hold the frame; start() replays it.
      this.pendingFrames.push(buf);
      return;
    }
    let frame: Frame;
    try {
      // A DELTA applies onto the retained store (only its damaged spans
      // overwrite; untouched cells keep their previous content). A FULL
      // always decodes fresh, sized from the frame itself: it follows a
      // resize (store dims would be stale) or a resync, and is meant to
      // reset everything anyway.
      const isFull = buf.byteLength >= 7 && new DataView(buf).getUint8(6) === 0;
      frame = !isFull && this.store ? decodeFrame(buf, this.store) : decodeFrame(buf);
    } catch {
      // Resize race (new dims bigger than the pinned store) or a decode
      // failure: drop the retained state, count the resync, and ask for a
      // fresh FULL. The actor clears its outstanding-ack gate on
      // request_full, so this can never deadlock the stream
      // (regression-tested actor-side).
      this.store = null;
      this.grid = null;
      this.lastSeq = null;
      this.resyncs += 1;
      void this.api.requestFull(this.termId);
      return;
    }
    // A FULL is authoritative: adopt its freshly-decoded store wholesale.
    // (It was decoded without the pinned store, so its cells are complete —
    // and it may exist precisely because the retained store is suspect,
    // e.g. a seq-gap resync. Keeping the old store here would paint stale
    // cells forever while acking normally.)
    if (frame.kind === "full" || !this.store) this.store = frame.cells;
    this.grid = { cols: frame.cells.cols, rows: frame.cells.rows };

    // Seq-gap resync: a frame whose seq skips the expected next value means
    // we lost one — the retained store no longer matches the actor's grid.
    // Don't paint (or ack) an untrustworthy delta; the requested FULL (which
    // also clears the actor's gate) re-syncs in one frame.
    if (this.lastSeq !== null) {
      const expected = (this.lastSeq + 1) >>> 0;
      if (frame.seq !== expected) {
        this.resyncs += 1;
        this.lastSeq = frame.seq;
        void this.api.requestFull(this.termId);
        return;
      }
    }
    this.lastSeq = frame.seq;

    // Gate copy on the wire's selection_active bit, NOT the viewport-clipped
    // range: a selection that scrolled into history (streaming output) is
    // still copyable, and Ctrl+C must not fall through as an interrupt.
    this.hasSelection = frame.selectionActive;
    this.frameMouseCapture = frame.mouseCapture;
    this.lastCursor = { row: frame.cursor.row, col: frame.cursor.col };
    // Marks math reads the frame-header geometry; a scroll invalidates the
    // hover position (the hovered cell moved under the pointer).
    const scrolled = frame.displayOffset !== this.lastDisplayOffset;
    this.lastDisplayOffset = frame.displayOffset;
    this.lastHistoryLen = frame.historyLen;
    if (scrolled) this.hideLinkHover();
    this.hud.recordFrame(buf.byteLength, frame.rows.length);

    const { termId } = this;
    const paint = (): void => {
      const t0 = performance.now();
      if (this.store && !this.glReleased) this.renderer.apply(frame, this.store);
      this.hud.recordPaint(performance.now() - t0);
      // Ack strictly after paint — and never while hidden (the actor's gate
      // staying closed is exactly the coalescing backpressure the hidden
      // panel wants; reveal requests a FULL instead).
      if (!this.hidden) void this.api.ack(termId, frame.seq);
    };
    if (typeof requestAnimationFrame === "function") {
      requestAnimationFrame(paint);
    } else {
      paint();
    }
  }

  // --- terminal-pane context menu ---------------------------------
  //
  // Right-click on the TERMINAL (not the rail command-row menu): exactly Copy
  // and Paste. Copy is gated on the wire's `selection_active` flag so a
  // selection that scrolled into history stays copyable; Paste reads the OS
  // clipboard and sends through the normal paste path (bracketed-paste guard
  // applies Rust-side).

  private buildMenu(): void {
    this.menu = document.createElement("div");
    this.menu.className = "chappa-term-menu";
    this.menu.style.cssText =
      "position:fixed;z-index:40;display:none;min-width:120px;background:#1b1d21;" +
      "border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);";
    const item = (label: string): HTMLButtonElement => {
      const btn = document.createElement("button");
      btn.textContent = label;
      btn.style.cssText =
        "display:block;width:100%;text-align:left;background:none;border:0;" +
        "color:#d6d8dc;padding:4px 8px;border-radius:4px;font:13px system-ui,sans-serif;";
      this.menu.appendChild(btn);
      return btn;
    };
    this.menuCopy = item("Copy");
    this.menuCopy.addEventListener("click", () => {
      this.hideMenu();
      if (this.termId >= 0 && this.hasSelection) void this.api.copySelection(this.termId);
    });
    const paste = item("Paste");
    paste.addEventListener("click", () => {
      this.hideMenu();
      void this.pasteFromClipboard();
    });
    document.body.appendChild(this.menu);
    this.menu.addEventListener("mousedown", this.menuInside);
    // Click elsewhere or Escape closes it — the shared capture-phase dismiss
    // (dismiss.ts; this menu is one of the two drifted copies WITH Escape).
    this.menuUnbind = registerDismiss({
      isOpen: () => this.menu.style.display !== "none",
      dismiss: () => this.hideMenu(),
      inside: [this.menu],
      escape: true,
    });
  }

  private readonly menuInside = (e: MouseEvent): void => e.stopPropagation();

  private showContextMenu(e: MouseEvent): void {
    // preventDefault so the webview's native menu never appears alongside.
    e.preventDefault();
    this.menuCopy.disabled = !this.hasSelection;
    // Inline-styled buttons have no [disabled] rule — grey it by hand or the
    // gate reads as enabled (finding).
    this.menuCopy.style.opacity = this.menuCopy.disabled ? "0.4" : "1";
    this.menuCopy.style.cursor = this.menuCopy.disabled ? "default" : "pointer";
    this.menu.style.display = "block";
    this.menu.style.left = `${e.clientX}px`;
    this.menu.style.top = `${e.clientY}px`;
  }

  private hideMenu(): void {
    this.menu.style.display = "none";
  }

  private async pasteFromClipboard(): Promise<void> {
    if (this.termId < 0 || !this.active) return;
    try {
      const text = await navigator.clipboard.readText();
      if (text) void this.api.paste(this.termId, text);
    } catch {
      // Clipboard read denied or unavailable — nothing to paste.
    }
  }
}
