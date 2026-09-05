// Multi-terminal shell: the process rail + panel manager.
//
// Owns the left rail (name, status-vocabulary dot, exit-code badge,
// activity + bell indicators) and the stack of TerminalPanels behind it, of
// which exactly one is active/visible at a time. Only the active panel acks
// — the hidden machinery treats "not the active panel" as a third
// hidden condition — so hidden panels coalesce damage behind the actor's gate
// and repaint on reveal via a requested FULL.
//
// GL-context budget: keep at most 8 live WebGL contexts; creating/activating
// a 9th releases the least-recently-used panel's context (disposed) and
// lazily recreates it on the panel's next activation (renderer_webgl.ts's
// context-loss path already does the recreate + FULL dance).
//
// Keyboard: Ctrl+PgUp/PgDn cycle, Ctrl+Shift+T opens a shell, Ctrl+Shift+W
// closes the active panel. The terminal never sees these (input.ts's
// isAppShortcut owns them); this App performs the action from a window
// listener, exactly like the HUD does for Ctrl+Shift+D.

import { TerminalPanel, tauriApi, type PanelHost, type TerminalApi } from "./term/panel";
import { TranscriptPanel } from "./transcript";
import { isPanelShortcut, isProjectShortcut, quotePaths } from "./term/input";
import { SettingsPane } from "./settings_pane";
import { EditPane, type ProjectEditInfo } from "./edit_pane";
import {
  addProjectDialog,
  commandDialog,
  confirmDialog,
  parentDirectory,
  promptDialog,
  type AddProjectAnswer,
  type CommandForm,
} from "./dialog";
import { settingsStore, type Settings, type SettingsStore } from "./settings";
import {
  decide,
  LEVELS,
  LEVEL_LABELS,
  parseLevel,
  resolveLevel,
  type Level,
} from "./notifications";
import * as ipc from "./ipc";
import { registerDismiss } from "./dismiss";
import { NotifyCenter, type CenterEntry } from "./notify_center";
import { NotifyBanners } from "./notify_banners";
import { NotifyDrawer } from "./notify_drawer";
import { StatsStore, statsChip, statsTooltip } from "./stats";
import { agentToolsStore, type AgentToolsStore } from "./agent_tools";
import { ScratchpadsSection, type ScratchpadsApi } from "./scratchpads_section";

/** The process-status vocabulary. */
export type RailStatus = "starting" | "running" | "stopped" | "exited" | "failed";

/** The agent block a rail entry carries. */
export interface AgentEntry {
  toolId: number;
  toolType: ipc.AgentToolType;
  model: string | null;
  runtime: ipc.AgentRuntimeDto;
  /** The project the agent was spawned into (Rust's `agent.project_id`):
   *  ACTIVE / notification-center attribution, and "Close project" closes
   *  it like a bound terminal. `null` = spawned with no project. */
  projectId: number | null;
  container: ipc.ContainerStateDto | null;
  bridge: ipc.AgentBridge;
  /** `tty` (terminal panel) or `json` (transcript view). The
   *  typed "waiting for input" fact is NOT here: it is the entry's
   *  `attention` (`"agent-waiting"`), set through the rail reducer. */
  transport: ipc.AgentTransport;
  /** The registry id of the process that SPAWNED this agent (from
   *  `agent.parent_process_id`) — the forest nests this row under it; `null`
   *  for a root. IMMUTABLE: a parent's exit/close never changes this (the
   *  frontend promotes the orphan); closing a parent closes only that row. */
  parentId: number | null;
}

/** Whether a rail status counts as alive (a dead entry is never "waiting"). */
export function isLive(status: RailStatus): boolean {
  return status === "starting" || status === "running";
}

/**
 * One agent row fed to {@link agentForest}, carrying its own rail
 * identity and its parent link alongside the full entry (so the renderer can
 * draw the row without a second lookup).
 */
export interface ForestAgent<R = unknown> {
  id: number;
  /** The entry's `agent.parentId` — the process that spawned it, `null` for
   *  a root. IMMUTABLE (history, not a live link). */
  parentId: number | null;
  /** The row itself. */
  entry: R;
}

/**
 * Order a surface's agent rows as a FOREST — an ordered
 * `[row, depth][]` where children render immediately after their parent.
 * Rules:
 *  - A row is a ROOT when `parentId` is null OR its parent is not on the same
 *    surface (parent closed, parent in the other section by the
 *    predicate, or parent a non-agent terminal that renders nowhere as an
 *    agent). Orphan promotion is silent — no "parent gone" affordance.
 *  - Children render immediately after their parent, in id order; roots keep
 *    the given (insertion) order.
 *  - Depth is unbounded in the model; the RENDERER caps the indent at depth 3
 *    (a visual cap, not a model one) — this function keeps true depth.
 * Pure and unit-tested in isolation — no DOM.
 */
export function agentForest<R>(rows: ForestAgent<R>[]): Array<[ForestAgent<R>, number]> {
  const ids = new Set(rows.map((r) => r.id));
  const childrenByParent = new Map<number, ForestAgent<R>[]>();
  const roots: ForestAgent<R>[] = [];
  for (const row of rows) {
    if (row.parentId === null || !ids.has(row.parentId)) {
      roots.push(row);
    } else {
      const sib = childrenByParent.get(row.parentId);
      if (sib) sib.push(row);
      else childrenByParent.set(row.parentId, [row]);
    }
  }
  for (const list of childrenByParent.values()) list.sort((a, b) => a.id - b.id);
  const out: Array<[ForestAgent<R>, number]> = [];
  const seen = new Set<number>();
  const visit = (row: ForestAgent<R>, depth: number): void => {
    if (seen.has(row.id)) return; // cycle guard — parents spawn first, cannot happen
    seen.add(row.id);
    out.push([row, depth]);
    for (const child of childrenByParent.get(row.id) ?? []) visit(child, depth + 1);
  };
  for (const root of roots) visit(root, 0);
  // Defence-in-depth: a row that never resolved into a root still renders as a
  // root — nothing may vanish from the rail (invariant).
  for (const row of rows) {
    if (!seen.has(row.id)) visit(row, 0);
  }
  return out;
}

/** A bridge fault is `container-down` or `stale` — anything but `ok`. */
export function bridgeFault(agent: AgentEntry | undefined): boolean {
  return agent !== undefined && agent.bridge !== "ok";
}

/** The container dot's tooltip: status + uptime + bridge. */
export function containerTooltip(agent: AgentEntry): string {
  const c = agent.container;
  const status = c?.status ?? "unknown";
  const up = c?.uptime_s != null ? ` · up ${formatUptime(c.uptime_s)}` : "";
  const name = agent.runtime.kind === "docker_exec" ? agent.runtime.container : "";
  return `${name}: ${status}${up} · bridge ${agent.bridge}`;
}

function formatUptime(s: number): string {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

/** One rail row. The App applies {@link railReducer} actions to these. */
export interface RailEntry {
  id: number;
  /** A TerminalPanel, or (json agents) a TranscriptPanel — the
   *  manager only needs the PanelHost surface. */
  panel: PanelHost;
  host: HTMLElement;
  name: string;
  status: RailStatus;
  exitCode: number | null;
  title: string | null;
  /** A hidden panel produced output (term://activity); shown until activate. */
  activity: boolean;
  /** A bell rang while hidden (term://bell); shown until activate. */
  bell: boolean;
  /** The typed attention tier: `"agent-waiting"` while a
   *  json agent has SAID it wants input (its `awaiting_input` event, or the
   *  spawn response), cleared by the next send / `turn_started`, and never
   *  set on a dead entry. Set ONLY through {@link railReducer}'s
   *  `attention` action; `renderRail` and the ACTIVE derivation read it.
   *  The heuristic `"badge"` tier is still DERIVED from `bell` at render
   *  time — this field carries facts, not heuristics. */
  attention: Attention;
  /** Rail-row kind: "process" entries are started project processes
   *  and render in the project section, NOT the terminals list. Absent
   *  (default) = a plain shell row.
   *
   *  "project-shell" is a shell BOUND to a project: the entry carries the
   *  association itself, one mechanism, not two. That one mechanism is this
   *  `kind` discriminant plus
   *  its own payload field below, exactly mirroring `process` — never a second
   *  association smuggled onto `process`. */
  kind?: "shell" | "process" | "project-shell" | "agent";
  /** For process entries: which project process this panel belongs to. */
  process?: { projectId: number; name: string };
  /** For "agent" entries: the spawn's agent block — model tag,
   *  runtime, the probe's container state and the bridge field. Renders in
   *  the workspace rail with a model tag and (docker) a container dot; a
   *  bridge fault (`container-down` / `stale`) is attention at the `badge`
   *  tier until the probe reports `ok` again. */
  agent?: AgentEntry;
  /** For "project-shell" entries: which project owns this terminal.
   *  It renders in that project's TERMINALS subsection (never the workspace
   *  rail) and resolves its notification level from THAT project, whichever
   *  project happens to be open. */
  projectShell?: { projectId: number };
  /** A terminal ADOPTED from the backend (`term://created` /
   *  mount reconcile) — the rail row exists but its pane is built lazily,
   *  on first click, via `attach_terminal` (the row has NO panel until
   *  then; `panel` is an inert placeholder). Once realized the flag clears
   *  and the entry is a normal panel entry. `process` adopted rows render
   *  in their project's COMMANDS card, not the rail — this flag is what
   *  lets the shared row machinery hold a placeholder for them. */
  adopted?: boolean;
}

/**
 * One project process row. Definition comes from the opened
 * chappa.yml; status is driven by `project://process_status` events; the panel
 * (once started) renders into the stack behind its own term id.
 */
export interface ProcessRow {
  projectId: number;
  name: string;
  command: string;
  status: RailStatus;
  autoStart: boolean;
  autoRestart: boolean;
  /** The rest of the yml definition, so `Edit command…` pre-fills
   *  from the row. `workingDir` null = project root; `env` in file order. */
  restartWhenChanged: string[];
  workingDir: string | null;
  env: Record<string, string>;
  /** Chappa-side toggles (projects.json, never the yml): favorites
   *  sort first within COMMANDS; a renaming-disabled row ignores OSC titles. */
  favorite: boolean;
  disableAutoRename: boolean;
  exitCode: number | null;
  termId: number | null;
  panel: TerminalPanel | null;
  host: HTMLElement | null;
  /** RAW per-process notification-level override; null = inherit the
   *  project default. Raw so a garbage value degrades via resolveLevel. */
  notificationLevel: string | null;
  /** Card badges: renderRail() skips process entries, so a hidden
   *  process terminal's bell/activity has to surface on its CARD instead. */
  bell: boolean;
  activity: boolean;
  /** DOM refs for in-place status updates (the section is not rebuilt per
   *  event — that would drop focus mid-interaction). */
  el: HTMLElement | null;
  pill: HTMLElement | null;
  bellEl: HTMLElement | null;
  activityEl: HTMLElement | null;
  startBtn: HTMLButtonElement | null;
  stopBtn: HTMLButtonElement | null;
  restartBtn: HTMLButtonElement | null;
}

/**
 * Per-open-project frontend state: process rows, project terminals and the
 * notification level cache, keyed by project id — a flat
 * `processRows: Map<name, …>` keyed by name alone cannot survive two open
 * projects (names collide across projects);
 * this is the core refactor". Project terminals need no field here — their
 * `RailEntry.projectShell.projectId` is already the association, and the
 * notification cache stays in `projects` (keyed by id).
 */
export interface ProjectState {
  id: number;
  /** This project's process rows, keyed by process name (unique WITHIN one
   *  chappa.yml; two open projects may reuse a name freely). */
  processRows: Map<string, ProcessRow>;
  /** Why the last chappa.yml reload failed, or null while it parses.
   *  While set, every yml mutation is disabled with this as the reason. */
  ymlError: string | null;
  /** Monotonic counter of row-list sources (reload fetches and mutation
   *  answers). A fetch that resolves after a newer source has been applied is
   *  stale and dropped — it could dispose the panel of a PTY Rust still runs
   *. */
  reloadSeq: number;
  /** Aggregated badge state for the COLLAPSED summary row: lit by
   *  any bell/notify-badge (bell) or hidden output (activity) belonging to
   *  this project while it is backgrounded; cleared on expand. */
  bell: boolean;
  activity: boolean;
  /** Summary-row DOM refs, present only while this project is collapsed
   *  (the expanded project renders the full section instead). */
  summaryEl: HTMLElement | null;
  summaryPill: HTMLElement | null;
  summaryBellEl: HTMLElement | null;
  summaryActivityEl: HTMLElement | null;
}

/**
 * One workspace-command row. The stored definition + runtime status
 * of a workspace-level command, shown in the workspace COMMANDS subsection
 * (directly above the workspace TERMINALS section). Addressed by NAME — there
 * is no project id, so the row key is the command name outright. Status is
 * driven by `workspace://status` events; the appointed terminal lives as a
 * plain workspace rail entry (the "same rule as plain shells" for
 * notifications).
 */
export interface WorkspaceCommandRow {
  name: string;
  command: string;
  status: RailStatus;
  autoStart: boolean;
  autoRestart: boolean;
  restartWhenChanged: string[];
  /** Absolute path, or null = the user's home dir (empty in the editor). */
  workingDir: string | null;
  env: Record<string, string>;
  exitCode: number | null;
  termId: number | null;
  panel: TerminalPanel | null;
  host: HTMLElement | null;
  /** DOM refs for in-place status updates (the section is not rebuilt per
   *  event — that would drop focus mid-interaction). */
  el: HTMLElement | null;
  pill: HTMLElement | null;
  startBtn: HTMLButtonElement | null;
  stopBtn: HTMLButtonElement | null;
  restartBtn: HTMLButtonElement | null;
}

/**
 * Attention flag for an ACTIVE-section row — a small enum so task
 * 31's upgrade is additive:
 *  - `"badge"`: an undismissed bell/notify badge (the state, READ
 *    here, never duplicated). This is the claude-agent case TODAY: claude
 *    emits a terminal bell / OSC 9 when it wants input (per its own
 *    notification settings), the hidden terminal badges, and the badge marks
 *    the row "needs attention" until activation clears it.
 *  - `"agent-waiting"`: the structured transport — the json
 *    agent's typed `awaiting_input` event (a FACT, not a heuristic). Lives
 *    on `RailEntry.attention` via the reducer's `attention` action; the
 *    top-sort and the accent ring key off "attention !== null".
 */
export type Attention = null | "badge" | "agent-waiting";

/**
 * One ACTIVE-section row: a running/starting project process, or a
 * live terminal currently carrying an unacknowledged badge. Deliberately a
 * plain data record — the future stack-area management dashboard (recorded in
 * the task as future work) is a renderer over exactly this list plus process
 * stats.
 */
export interface ActiveRow {
  kind: "process" | "terminal";
  /** Owning project, or null for a plain workspace shell. */
  projectId: number | null;
  /** Display label: process name / terminal title-or-name. */
  name: string;
  /** The terminal a click activates (null: a process whose panel is gone —
   *  its stopped pane fronts instead, via activateProcess). */
  termId: number | null;
  /** For process rows: the processRows key (click re-resolves by
   *  (projectId, processName), never by a captured object). */
  processName: string | null;
  status: RailStatus;
  attention: Attention;
  /** Hidden-output marker (the activity dot). Activity alone lists a
   *  terminal but is NOT attention — no ring, no top-sort. */
  activity: boolean;
}

/** Rail mutations, pure so the reducer is unit-testable without a DOM. */
export type RailAction =
  | { type: "status"; status: RailStatus; exitCode: number | null }
  | { type: "title"; title: string }
  | { type: "activity" }
  | { type: "bell" }
  | { type: "activate" }
  /** The typed attention tier (agent-waiting). Ignored on a dead
   *  entry: a process that is gone is never "waiting". */
  | { type: "attention"; attention: Attention };

export function railReducer(entry: RailEntry, action: RailAction): RailEntry {
  switch (action.type) {
    case "status":
      // A dead entry cannot be waiting for input: the typed tier drops with
      // the status (the badge heuristic is derived elsewhere and unaffected).
      return {
        ...entry,
        status: action.status,
        exitCode: action.exitCode,
        attention: isLive(action.status) ? entry.attention : null,
      };
    case "attention":
      return { ...entry, attention: isLive(entry.status) ? action.attention : null };
    case "title":
      return { ...entry, title: action.title };
    case "activity":
      return { ...entry, activity: true };
    case "bell":
      return { ...entry, bell: true };
    case "activate":
      return { ...entry, activity: false, bell: false };
  }
}

/** Pre-adopt `agent://event` buffer bounds (a few ids, a short ring each). */
export const PRE_ADOPT_EVENT_IDS_MAX = 16;
export const PRE_ADOPT_EVENTS_PER_ID = 64;

/**
 * LRU bookkeeping for the GL-context budget (keep at most one context
 * per panel, but no more than `cap` live app-wide). Pure and unit-tested.
 */
export class GlLru {
  /** Panel ids holding a live GL context, most-recently-used last. */
  private order: number[] = [];

  constructor(private readonly cap = 8) {}

  /**
   * Register that `id` now holds a live GL context. Returns the id whose
   * context must be released to stay within `cap`, or null. The
   * just-registered id is never returned (it is the one that just became
   * live). The evicted id is removed from the order here.
   */
  acquire(id: number): number | null {
    this.order = this.order.filter((x) => x !== id);
    this.order.push(id);
    if (this.order.length <= this.cap) return null;
    const evict = this.order.shift()!;
    return evict === id ? id : evict;
  }

  /** Forget `id` (its context was released or the panel closed). No-op if the
   *  id is not tracked (the eviction path already removed it). */
  release(id: number): void {
    this.order = this.order.filter((x) => x !== id);
  }

  /** Number of live contexts currently tracked. */
  get size(): number {
    return this.order.length;
  }
}

export interface AppOptions {
  root: HTMLElement;
  api?: TerminalApi;
  /** The agent-tool registry behind "New agent ▸" and the settings
   *  Agents section. Defaults to the app-wide store. */
  agentTools?: AgentToolsStore;
  /** The two scratchpad reads behind the read-only SCRATCHPADS subsection
   *. Defaults to the Tauri commands. */
  scratchpads?: ScratchpadsApi;
  // The three dialog seams are Promise-ONLY: the defaults are now
  // the in-app `dialog.ts` modals, which cannot answer synchronously the way
  // `window.confirm` did. A single async shape keeps every call site honest —
  // a sync-or-async union would let a `await`-less call site compile.
  /** Close confirmation for a running terminal; defaults to an in-app modal. */
  confirm?: (name: string) => Promise<boolean>;
  /** Trust-gate confirm: shown when an opened project's chappa.yml
   *  content-hash isn't recorded as trusted. Resolve true = Run (record the
   *  hash + start the auto-start commands), false = Skip (decline; the gate
   *  re-arms on the next open). Defaults to an in-app modal listing the
   *  auto-start commands. */
  confirmProjectTrust?: (commands: string[]) => Promise<boolean>;
  /** Close-project confirm: shown when the explicit "Close project"
   *  would stop RUNNING processes, listing them. Resolve true = stop + close.
   *  Never shown when nothing is running. Defaults to an in-app modal. */
  confirmProjectClose?: (name: string, running: string[]) => Promise<boolean>;
  /** Remove-project confirm (the switcher row's 🗑): ALWAYS shown — removal
   *  forgets chappa-side state (trust, favorites, notification levels) even
   *  when nothing is running; `running` lists what additionally stops.
   *  Resolve true = remove. Defaults to an in-app modal. */
  confirmProjectRemove?: (name: string, running: string[]) => Promise<boolean>;
  /**
   * The "Add project…" modal. Resolves `{path, name}` or null to
   * cancel. `browse` is the native folder picker the modal should offer, or
   * null for none — the app passes null outside Tauri.
   *
   * REPLACES the earlier `promptPath?: () => Promise<string | null>` seam: the
   * flow now collects a NAME too, so a path-only answer can no longer express
   * it.
   */
  promptAddProject?: (
    browse: (() => Promise<string | null>) | null,
  ) => Promise<AddProjectAnswer | null>;
  /**
   * Whether the add-project modal offers "Browse…" at all. Defaults to
   * `inTauri()` — the picker must never be reachable outside Tauri; the
   * plain-browser dev page falls back to the typed-path field only. jsdom
   * tests set it true and stub `api.pickDirectory`.
   */
  canBrowseDirectories?: boolean;
  /** Rename prompt for the switcher's per-row ✎; null cancels.
   *  Defaults to an in-app prompt pre-filled with the current name. */
  promptRename?: (current: string) => Promise<string | null>;
  /** The command editor: `Edit command…` passes the row's current
   *  definition, `+ Add command` / `Add ▸` pass null. Resolves the form or
   *  null to cancel. Defaults to the in-app `commandDialog`. */
  promptCommand?: (initial: CommandForm | null, title: string) => Promise<CommandForm | null>;
  /** The WORKSPACE command editor: same editor with the
   *  working_dir hint changed to "absolute path (or empty for home)". Defaults
   *  to `commandDialog(initial, { title, workdirHint })`. */
  promptWorkspaceCommand?: (
    initial: CommandForm | null,
    title: string,
  ) => Promise<CommandForm | null>;
  /** `Delete command "<name>"` confirm. Defaults to an in-app modal. */
  confirmDeleteCommand?: (name: string) => Promise<boolean>;
  /** A failed yml mutation's reason (name clash, containment, write error)
   *  shown to the user. Defaults to an in-app OK modal. */
  showError?: (message: string) => Promise<void>;
  /** Settings source. Defaults to the app-wide store; tests inject
   *  one built on a fake ipc so the gear/pane journeys are drivable. */
  settings?: SettingsStore;
  /** Clock for the notification center's timestamps. Date.now by
   *  default — injected here at the APP layer so tests pin time. */
  now?: () => number;
}

const APP_CSS = `
.chappa-app{position:fixed;inset:0;display:flex;background:#0d0e10;color:#d6d8dc;font-family:system-ui,sans-serif;}
.chappa-rail{width:260px;min-width:260px;border-right:1px solid #22252a;display:flex;flex-direction:column;background:#101216;}
.chappa-rail-header{padding:10px 12px 6px;font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;}
.chappa-project-bar{position:relative;padding:8px 8px 4px;border-bottom:1px solid #1c1f24;}
.chappa-project-switcher{display:block;width:100%;padding:6px 8px;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#d6d8dc;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-project-switcher:hover{background:#1d2127;}
.chappa-project-menu{position:absolute;top:36px;left:8px;right:8px;z-index:30;max-height:240px;overflow-y:auto;background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-project-option,.chappa-project-add{display:block;width:100%;text-align:left;background:none;border:0;color:#d6d8dc;padding:6px 8px;border-radius:4px;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-project-option:hover,.chappa-project-add:hover{background:#24272e;}
/* The switcher row is [open project | ✎ rename]. */
.chappa-project-row{display:flex;align-items:center;gap:2px;}
.chappa-project-row .chappa-project-option{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-project-rename{flex:none;width:22px;height:22px;border:0;border-radius:4px;background:none;color:#6b7280;font:12px ui-monospace,monospace;line-height:1;cursor:pointer;padding:0;}
.chappa-project-rename:hover{background:#24272e;color:#d6d8dc;}
.chappa-project-remove{flex:none;display:flex;align-items:center;justify-content:center;width:22px;height:22px;border:0;border-radius:4px;background:none;color:#6b7280;font-size:12px;line-height:1;cursor:pointer;padding:0;}
.chappa-project-remove:hover{background:#24272e;color:#f85149;}
.chappa-project-add{color:#8b949e;border-top:1px solid #262a31;margin-top:2px;padding-top:7px;}
/* The switcher row's × closes an OPEN project (stops everything). */
.chappa-project-close{flex:none;width:22px;height:22px;border:0;border-radius:4px;background:none;color:#6b7280;font:12px ui-monospace,monospace;line-height:1;cursor:pointer;padding:0;}
.chappa-project-close:hover{background:#24272e;color:#f85149;}
/* Collapsed open projects: one-line summary rows above the
   expanded project's section. The row is a DIV holding two sibling buttons
   (expand | ×) — the renderProjectMenu rule: nesting the × inside the row
   button would be invalid HTML. */
.chappa-project-summaries{display:flex;flex-direction:column;}
.chappa-project-summary{display:flex;align-items:center;gap:6px;width:100%;padding:6px 10px;box-sizing:border-box;border-bottom:1px solid #1c1f24;color:#d6d8dc;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-project-summary:hover{background:#171a1f;}
.chappa-project-summary-expand{flex:1 1 auto;min-width:0;display:flex;align-items:center;gap:6px;padding:0;border:0;background:none;color:inherit;font:inherit;text-align:left;cursor:pointer;}
.chappa-project-summary-name{flex:1 1 auto;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-project-section{padding:8px 8px 2px;border-bottom:1px solid #1c1f24;}
/* Two rows (2026-08-31: the 42b sync button crammed the header and
   made the name unreadable): row 1 = the name alone, full width; row 2 =
   status pill left, S/A/P actions right. */
.chappa-project-header{display:flex;flex-direction:column;gap:2px;padding:2px 2px 6px;}
.chappa-project-header-sub{display:flex;align-items:center;gap:6px;}
.chappa-project-name{font:13px system-ui,sans-serif;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-project-pill{flex:1 1 auto;font:11px ui-monospace,monospace;color:#6b7280;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-project-pill.running{color:#3fb950;}
.chappa-project-actions{display:flex;gap:4px;}
.chappa-project-action{flex:none;width:20px;height:20px;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:11px ui-monospace,monospace;cursor:pointer;}
.chappa-project-action:hover{background:#1d2127;color:#d6d8dc;}
.chappa-process-row{position:relative;display:flex;align-items:flex-start;gap:8px;padding:6px 6px;border-radius:6px;cursor:pointer;}
.chappa-process-row:hover{background:#171a1f;}
.chappa-process-row.active{background:#1d232b;}
.chappa-process-row.active::before{content:"";position:absolute;left:0;top:0;bottom:0;width:3px;background:#5ea6ff;border-radius:0 2px 2px 0;}
.chappa-process-info{flex:1 1 auto;min-width:0;}
.chappa-process-title{display:flex;align-items:center;gap:4px;}
.chappa-process-name{font:13px system-ui,sans-serif;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-process-badge{flex:none;font:10px ui-monospace,monospace;letter-spacing:.05em;color:#8b949e;border:1px solid #2a2c31;border-radius:4px;padding:0 4px;}
.chappa-process-badge.auto{color:#d29922;border-color:#5a4a17;}
.chappa-process-cmd{font:11px ui-monospace,monospace;color:#6b7280;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;margin-top:2px;}
.chappa-process-side{flex:none;display:flex;flex-direction:column;align-items:flex-end;gap:4px;}
.chappa-status-pill{font:11px ui-monospace,monospace;color:#6b7280;}
.chappa-status-pill.running{color:#3fb950;}
.chappa-status-pill.starting{color:#d29922;}
.chappa-status-pill.failed{color:#f85149;}
.chappa-process-actions{display:flex;gap:4px;}
.chappa-process-btn{flex:none;width:18px;height:18px;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:10px ui-monospace,monospace;cursor:pointer;padding:0;}
.chappa-process-btn:hover:not(:disabled){background:#1d2127;color:#d6d8dc;}
.chappa-process-btn:disabled{opacity:.35;cursor:default;}
.chappa-rail-header{position:relative;display:flex;align-items:center;gap:6px;}
.chappa-rail-header-label{flex:1 1 auto;}
.chappa-new-term,.chappa-project-term-add,.chappa-project-term-more{flex:none;width:20px;height:20px;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:12px ui-monospace,monospace;line-height:1;cursor:pointer;padding:0;}
.chappa-new-term:hover,.chappa-project-term-add:hover,.chappa-project-term-more:hover{background:#1d2127;color:#d6d8dc;}
.chappa-new-term-menu,.chappa-project-term-menu{position:absolute;top:22px;right:0;z-index:30;min-width:170px;max-height:240px;overflow-y:auto;background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-new-term-option,.chappa-project-term-option{display:block;width:100%;text-align:left;background:none;border:0;color:#d6d8dc;padding:6px 8px;border-radius:4px;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-new-term-option:hover,.chappa-project-term-option:hover{background:#24272e;}
.chappa-new-term-empty,.chappa-project-term-empty{padding:6px 8px;color:#6b7280;font:12px system-ui,sans-serif;}
/* The project TERMINALS subsection. House pattern for a section
   header: uppercase and muted, a hairline rule, a count at the right edge.
   The add affordances sit ON that header line (2026-08-15). */
.chappa-project-terms{padding:0 0 4px;}
.chappa-subsection-header{position:relative;display:flex;align-items:center;gap:6px;padding:5px 2px 4px;border-top:1px solid #1c1f24;font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;}
.chappa-subsection-label{flex:1 1 auto;}
.chappa-subsection-count{flex:none;color:#6b7280;}
.chappa-project-term-list{display:flex;flex-direction:column;}
/* The workspace COMMANDS subsection: house header above the
   workspace TERMINALS section, plus the small header-line Add-command button. */
.chappa-workspace-cmds{padding:0 0 2px;}
.chappa-workspace-cmd-add{flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:11px system-ui,sans-serif;letter-spacing:0;cursor:pointer;padding:1px 6px;}
.chappa-workspace-cmd-add:hover{background:#1d2127;color:#d6d8dc;}
/* The project AGENTS subsection (agents nest under their
   project). The header is the
   same house subsection header as TERMINALS (its N/M count reads
   running/total); the row list and the "New agent ▸" spawner reuse the
   terminal-row / workspace-spawner classes, so no parallel widget CSS. */
.chappa-project-agents{padding:0 0 4px;}
.chappa-rail-list{flex:1 1 auto;overflow-y:auto;padding:0 6px 8px;}
.chappa-rail-footer{flex:none;display:flex;align-items:center;gap:4px;border-top:1px solid #1c1f24;padding:6px;}
.chappa-rail-gear{flex:1 1 auto;display:flex;align-items:center;gap:8px;min-width:0;padding:7px 8px;border:0;border-radius:6px;background:none;color:#8b949e;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-rail-gear:hover{background:#171a1f;color:#d6d8dc;}
.chappa-rail-gear.open{background:#1d232b;color:#d6d8dc;}
.chappa-rail-item{display:flex;align-items:center;gap:8px;width:100%;padding:7px 8px;border:0;border-radius:6px;background:none;color:inherit;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-rail-item:hover{background:#171a1f;}
.chappa-rail-item.active{background:#1d232b;}
.chappa-rail-item.active::before{content:"";position:absolute;left:0;width:3px;align-self:stretch;background:#5ea6ff;border-radius:0 2px 2px 0;}
.chappa-status-dot{width:8px;height:8px;border-radius:50%;flex:none;background:#565b64;}
.chappa-status-dot.running{background:#3fb950;}
.chappa-status-dot.starting{background:#d29922;}
.chappa-status-dot.failed{background:#f85149;}
.chappa-status-dot.exited{background:#565b64;}
.chappa-status-dot.stopped{background:#565b64;}
.chappa-rail-name{flex:1 1 auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
/* The per-terminal cpu readout. Fixed width, always present (empty
   when idle) so a row never reflows or re-truncates its label when stats start
   or stop arriving. */
.chappa-rail-cpu{flex:none;width:30px;text-align:right;color:#6b7280;font:11px ui-monospace,monospace;}
.chappa-rail-activity{width:8px;height:8px;border-radius:50%;flex:none;background:#5ea6ff;}
.chappa-rail-bell{flex:none;color:#f0b429;font-size:11px;line-height:1;}
.chappa-exit-code{flex:none;font:11px ui-monospace,monospace;color:#8b949e;background:#1c1f24;border-radius:4px;padding:0 5px;}
.chappa-rail-close{flex:none;border:0;background:none;color:#6b7280;font-size:14px;line-height:1;cursor:pointer;padding:0 2px;border-radius:4px;}
/* Agents: "New agent ▸" header affordance + menu, the model tag,
   the container dot (status-coloured), the bridge-fault attention ring, and
   the close-verification toast. */
.chappa-new-agent{flex:none;border:0;background:none;color:#8b949e;font:11px ui-monospace,monospace;letter-spacing:.04em;cursor:pointer;padding:0 4px;border-radius:4px;}
.chappa-new-agent:hover{color:#d6d8dc;background:#1d2127;}
.chappa-new-agent-menu{position:absolute;top:22px;right:0;z-index:31;min-width:200px;max-height:240px;overflow-y:auto;background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-new-agent-option{display:block;width:100%;text-align:left;background:none;border:0;color:#d6d8dc;padding:4px 8px;border-radius:4px;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-new-agent-option:hover{background:#24272e;}
.chappa-new-agent-empty{padding:4px 8px;color:#6b7280;font:12px system-ui,sans-serif;}
.chappa-rail-model{flex:none;max-width:90px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#6b7280;font:11px ui-monospace,monospace;}
.chappa-rail-container{width:7px;height:7px;border-radius:2px;flex:none;background:#565b64;}
.chappa-rail-container.running{background:#3fb950;}
.chappa-rail-container.exited,.chappa-rail-container.missing{background:#f85149;}
.chappa-rail-item.attention{box-shadow:inset 0 0 0 1px #f0b429;}
/* Json transport: the rail tag, the typed agent-waiting
   ring (accent, distinct from the amber heuristic badge), the transcript
   host (a column flex so the log scrolls and the box stays put). */
.chappa-rail-transport{flex:none;color:#5ea6ff;font:10px ui-monospace,monospace;letter-spacing:.06em;border:1px solid #274a6b;border-radius:3px;padding:0 3px;line-height:14px;}
.chappa-rail-item.attention[data-attention="agent-waiting"]{box-shadow:inset 0 0 0 1px #5ea6ff;}
/* Subagent nesting. A child row indents one dot-column per depth
   (the VALUE is the visual cap --chappa-sub-depth, min(depth,3); the model
   depth is unbounded) and draws the dashed tree connector: an elbow from the
   PARENT's status-dot column into the child's own dot, plus a vertical guide
   descending that column so a sibling chain reads as one tree. Dashed,
   muted #565b64-ish, 1px. The LAST child's vertical stops at its dot. */
.chappa-rail-child{padding-left:calc(8px + var(--chappa-sub-depth,0)*14px);}
.chappa-rail-child::before{content:"";position:absolute;left:15px;top:0;width:1px;height:100%;border-left:1px dashed #565b64;opacity:.55;}
.chappa-rail-child-last::before{height:50%;}
.chappa-rail-child::after{content:"";position:absolute;left:15px;top:50%;width:calc(var(--chappa-sub-depth,0)*14px - 4px);border-top:1px dashed #565b64;opacity:.55;}
/* The active bar and the connector both used ::before, and the
   2-class .active rule won — a FOCUSED child row lost its dashed guide. For child rows the active bar is
   painted with an inset box-shadow instead, and the guide is re-declared at higher specificity. */
.chappa-rail-item.chappa-rail-child.active{box-shadow:inset 3px 0 0 #5ea6ff;}
.chappa-rail-item.chappa-rail-child.active::before{left:15px;top:0;width:1px;height:100%;background:none;border-left:1px dashed #565b64;border-radius:0;opacity:.55;}
.chappa-rail-item.chappa-rail-child-last.active::before{height:50%;}
.chappa-panel-host.chappa-transcript-host.active{display:flex;flex-direction:column;}
.chappa-transcript{flex:1 1 auto;min-height:0;}
.chappa-toast-stack{position:fixed;left:12px;bottom:12px;z-index:56;display:flex;flex-direction:column;gap:6px;max-width:360px;}
.chappa-toast{background:#1b1d21;border:1px solid #f0b429;border-radius:6px;padding:7px 10px;color:#d6d8dc;font:12px system-ui,sans-serif;box-shadow:0 4px 16px rgba(0,0,0,.5);cursor:pointer;}
.chappa-rail-close:hover{color:#f85149;}
.chappa-stack{flex:1 1 auto;display:flex;flex-direction:column;min-width:0;position:relative;}
/* The project context menu's Remove entry is the destructive one. */
.chappa-project-ctx-remove{color:#f85149;}
.chappa-project-ctx-remove:hover{background:#2a1717;color:#ff7b72;}
.chappa-process-activity{width:8px;height:8px;border-radius:50%;flex:none;background:#5ea6ff;}
.chappa-process-bell{flex:none;color:#f0b429;font-size:11px;line-height:1;}
.chappa-row-menu{position:fixed;z-index:40;display:none;min-width:170px;background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-row-menu-item{display:block;width:100%;text-align:left;background:none;border:0;color:#d6d8dc;padding:4px 8px;border-radius:4px;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-row-menu-item:hover{background:#24272e;}
.chappa-row-submenu-parent{position:relative;}
.chappa-row-submenu{position:absolute;left:100%;top:0;display:none;min-width:180px;background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:4px;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-row-submenu.open{display:block;}
/* The full command menu: muted section label, disabled mutations,
   the per-card favorite star, the trailing + Add command card, and the
   chappa.yml load-error line that explains why editing is off. */
.chappa-row-menu-label{padding:6px 8px 2px;font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.04em;}
.chappa-row-menu-item:disabled{color:#565b64;cursor:default;}
.chappa-row-menu-item:disabled:hover{background:none;}
.chappa-process-star{flex:none;border:0;background:none;color:#565b64;font-size:12px;line-height:1;cursor:pointer;padding:0 2px;border-radius:3px;}
.chappa-process-star:hover{color:#d6d8dc;}
.chappa-process-star.on{color:#f0b429;}
.chappa-process-add{display:block;width:100%;margin:4px 0 6px;padding:6px 8px;border:1px dashed #262a31;border-radius:6px;background:none;color:#8b949e;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-process-add:hover{background:#171a1f;color:#d6d8dc;}
.chappa-process-add:disabled{color:#565b64;cursor:default;}
.chappa-project-yml-error{display:none;margin:0 2px 8px;padding:6px 8px;border:1px solid #5c2f2f;border-radius:6px;background:#2a1717;color:#f0a0a0;font:12px ui-monospace,monospace;white-space:pre-wrap;word-break:break-word;}
.chappa-panel-host{display:none;flex:1 1 auto;min-width:0;min-height:0;}
.chappa-panel-host.active{display:block;}
.chappa-panel-host.chappa-process-empty{display:none;}
.chappa-panel-host.chappa-process-empty.active{display:flex;flex-direction:column;align-items:center;justify-content:center;gap:8px;}
.chappa-process-empty-title{font:600 15px system-ui,sans-serif;color:#d6d8dc;}
.chappa-process-empty-hint{font:13px system-ui,sans-serif;color:#8b949e;}
.chappa-process-empty-start{margin-top:4px;padding:6px 14px;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#d6d8dc;font:13px system-ui,sans-serif;cursor:pointer;}
.chappa-process-empty-start:hover{background:#1d232b;}
/* ACTIVE section: running processes + badged live terminals across
   ALL open projects, at the top of the rail. Hidden entirely when empty.
   Attention rows (undismissed bell/notify badge) sort first and carry the
   accent ring. */
.chappa-active-section{padding:0 8px 4px;border-bottom:1px solid #1c1f24;}
.chappa-active-label{flex:1 1 auto;}
.chappa-active-count{flex:none;color:#6b7280;}
.chappa-active-row{position:relative;display:flex;align-items:center;gap:8px;width:100%;padding:6px 8px;border:0;border-radius:6px;background:none;color:#d6d8dc;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-active-row:hover{background:#171a1f;}
.chappa-active-row.attention{box-shadow:inset 0 0 0 1px #5ea6ff;}
.chappa-active-project{flex:none;max-width:88px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:11px ui-monospace,monospace;color:#6b7280;}
.chappa-active-attention{flex:none;color:#f0b429;font-size:11px;line-height:1;}
.chappa-active-activity{width:8px;height:8px;border-radius:50%;flex:none;background:#5ea6ff;}
/* The rail footer's notification bell + unread count, NEXT TO the
   settings gear. The banner stack and center drawer style themselves
   (notify_banners.ts / notify_drawer.ts). */
.chappa-notify-bell{position:relative;flex:none;display:flex;align-items:center;justify-content:center;width:32px;height:30px;border:0;border-radius:6px;background:none;color:#8b949e;font-size:13px;line-height:1;cursor:pointer;padding:0;}
.chappa-notify-bell:hover{background:#171a1f;color:#d6d8dc;}
.chappa-notify-bell-count{position:absolute;top:0;right:0;min-width:14px;height:14px;border-radius:7px;background:#5ea6ff;color:#0d0e10;font:600 9px ui-monospace,monospace;line-height:14px;text-align:center;padding:0 3px;box-sizing:border-box;}
`;

// --- the bell glyph ----------------------------------------------------------

/**
 * Monochrome outline bell, stroked in `currentColor`: the rail footer's
 * grey + hover and the amber attention accents all
 * come from the CSS
 * `color` on the wrapping element. The 🔔 emoji this replaces ignored CSS
 * color entirely — Windows drew the full-color Segoe glyph, gold in a
 * monochrome rail (2026-08-31).
 */
function bellIcon(): SVGSVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "1em");
  svg.setAttribute("height", "1em");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.style.display = "block";
  const body = document.createElementNS(NS, "path");
  body.setAttribute("d", "M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9");
  const clapper = document.createElementNS(NS, "path");
  clapper.setAttribute("d", "M13.73 21a2 2 0 0 1-3.46 0");
  svg.append(body, clapper);
  return svg;
}

/** Outline trash can, same currentColor treatment as [`bellIcon`] — the 🗑
 *  emoji has the identical colored-glyph problem the bell had. */
function trashIcon(): SVGSVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "1em");
  svg.setAttribute("height", "1em");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.style.display = "block";
  const lid = document.createElementNS(NS, "path");
  lid.setAttribute("d", "M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2");
  const can = document.createElementNS(NS, "path");
  can.setAttribute("d", "M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6");
  svg.append(lid, can);
  return svg;
}

// --- remembered browse directory ----------------------------------

/**
 * Where the next "Browse…" starts. localStorage, NOT settings.json: this is
 * pure UI convenience state with no schema, no validation and no meaning to
 * Rust — putting it in settings.json would grow the settings DTO (and its
 * clamp/round-trip contract) for a breadcrumb, which is overkill. Losing it
 * costs one extra click.
 */
const LAST_PROJECT_DIR_KEY = "chappa-ai.lastProjectDir";

/** localStorage, or null where there is none (SSR/jsdom-without-storage) or
 *  where touching it throws (a webview with storage disabled). */
function safeStorage(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch {
    return null;
  }
}

function readLastProjectDir(): string | undefined {
  try {
    return safeStorage()?.getItem(LAST_PROJECT_DIR_KEY) ?? undefined;
  } catch {
    return undefined;
  }
}

/**
 * Remember where to open the picker next time: the PARENT of the project just
 * added, so the following add lands among its siblings. Starting INSIDE the
 * project you just registered would be one level too deep — "consecutive adds
 * start nearby" means the workspace folder, not the project itself.
 *
 * `parentDirectory` (dialog.ts, the folderBaseName split discipline) owns the
 * boundary regime: `C:\chappa-ai` remembers `C:\` (never the drive-RELATIVE `C:`,
 * which resolves against the drive's per-process CWD and opened the picker
 * somewhere arbitrary), `/chappa-ai` remembers `/`, and a parentless path (relative,
 * bare drive, bare root) stores NOTHING — the old code stored the path itself.
 */
function rememberProjectDir(path: string): void {
  const parent = parentDirectory(path);
  if (parent === null) return;
  try {
    safeStorage()?.setItem(LAST_PROJECT_DIR_KEY, parent);
  } catch {
    // Storage full or disabled: the breadcrumb is optional, the add is not.
  }
}

/** The font-affecting slice of a settings value: bare family name, CSS px
 *  size, dimensionless line height. Panels re-measure only when this changes. */
function fontSignature(s: Settings): string {
  return `${s.fontFamily}|${s.fontSize}|${s.lineHeight}`;
}

let appStylesInjected = false;

function injectAppStyles(): void {
  if (appStylesInjected || typeof document === "undefined") return;
  appStylesInjected = true;
  const el = document.createElement("style");
  el.textContent = APP_CSS;
  document.head.appendChild(el);
}

/** A "New agent ▸" spawner (the workspace TERMINALS header; the
 *  project AGENTS section shares it): the button plus its dropdown of the
 *  ENABLED agent tools. One builder renders both surfaces so the affordance
 *  is literally the same widget — same classes, same option anatomy, same
 *  dismiss discipline; only the pick callback differs (the workspace one
 *  keeps the binding, the section one names the project explicitly). */
interface AgentSpawner {
  btn: HTMLButtonElement;
  menu: HTMLElement;
  hide: () => void;
}

export class App {
  private readonly root: HTMLElement;
  private readonly api: TerminalApi;
  private readonly railList: HTMLElement;
  private readonly railCount: HTMLElement;
  private readonly stack: HTMLElement;
  /** Every term id the app has accounted for — spawned by it,
   *  adopted from the backend, or referenced by a project/workspace process
   *  row — the dedup set behind `term://created` and the mount reconcile.
   *  Kept separate from `entries` because adopted PROCESS terminals have no
   *  rail row (they render as their project's command card) yet must not be
   *  re-adopted by a later broadcast. */
  private readonly seen = new Set<number>();
  // --- workspace commands -----------------------------------------
  /** The COMMANDS subsection of the workspace area, directly ABOVE the
   *  workspace TERMINALS section: house header (label + count + `+ Add
   *  command`) and the command-card rows. Built ONCE, updated in place. */
  private readonly wsCmdsEl: HTMLElement;
  private readonly wsCmdCount: HTMLElement;
  private readonly wsCmdList: HTMLElement;
  /** What the COMMANDS subsection last rendered (the projectTermsSig
   *  precedent). */
  private wsCmdsSig = "\u0000never-rendered";
  private readonly wsCmds = new Map<string, WorkspaceCommandRow>();
  /** The REDUCED per-command context menu (Start/Stop, Edit
   *  command…, Copy command, Delete command). ONE reused element. */
  private readonly wsCmdMenu: HTMLElement;
  private wsCmdMenuFor: string | null = null;
  private readonly lru = new GlLru(8);
  private entries = new Map<number, RailEntry>();
  /** `agent://event`s that arrived for an id we have not adopted
   *  yet (spawn response in flight), replayed after adopt. Bounded. */
  private readonly pendingAgentEvents = new Map<number, ipc.AgentEventEvent[]>();
  private activeId: number | null = null;
  private readonly promptCommand: (
    initial: CommandForm | null,
    title: string,
  ) => Promise<CommandForm | null>;
  private readonly promptWorkspaceCommand: (
    initial: CommandForm | null,
    title: string,
  ) => Promise<CommandForm | null>;
  private readonly confirmDeleteCommand: (name: string) => Promise<boolean>;
  private readonly showError: (message: string) => Promise<void>;
  private readonly confirmClose: (name: string) => Promise<boolean>;
  private readonly confirmTrust: (commands: string[]) => Promise<boolean>;
  private readonly confirmProjectClose: (name: string, running: string[]) => Promise<boolean>;
  private readonly confirmProjectRemove: (name: string, running: string[]) => Promise<boolean>;
  private readonly promptAddProject: (
    browse: (() => Promise<string | null>) | null,
  ) => Promise<AddProjectAnswer | null>;
  private readonly promptRename: (current: string) => Promise<string | null>;
  private readonly canBrowseDirectories: boolean;
  /** Keys of the modal dialogs currently on screen. The dialogs are async, so
   *  a second × press (or a Ctrl+Shift+W held down) would otherwise stack a
   *  second copy of the same question over the first. */
  private readonly pendingDialogs = new Set<string>();
  /** Project ids whose `open_project` invoke is in flight (the pendingDialogs
   *  pattern). The `openProjects.has` guard only covers COMPLETED opens: two
   *  overlapping opens of the same id would both reach Rust, whose
   *  reset-idempotency kills the first call's fresh runtime. */
  private readonly pendingProjectOpens = new Set<number>();

  // --- settings ---------------------------------------------------
  private readonly settings: SettingsStore;
  private readonly gear: HTMLButtonElement;
  private readonly newTermMenu: HTMLElement;
  private readonly newTermMenuBtn: HTMLButtonElement;
  // --- agents (shares the spawner) -------------------------
  private readonly agentTools: AgentToolsStore;
  /** The "New agent ▸" spawners (the workspace affordance, the
   *  project-section spawner): one shared builder, one option renderer —
   *  the affordance is the same widget on both surfaces, and at most one of
   *  the two menus is open at a time. */
  private readonly agentSpawners: AgentSpawner[] = [];
  private readonly newAgentSpawner!: AgentSpawner;
  /** The close-verification toast stack (bottom-left, so it never fights the
   *  notification banners bottom-right). */
  private readonly toasts: HTMLElement;
  private settingsPane: SettingsPane | null = null;
  private unsubscribeSettings: (() => void) | null = null;
  /** The font triple the open panels were last told about — CSS px size, bare
   *  family name, dimensionless line height. Only a change in THIS triple
   *  re-measures panels; the other settings need no per-panel work (wheel
   *  speed and synthetic marks are broadcast to the actors Rust-side). */
  private fontSig = "";

  // --- project view ----------------------------------------------
  private readonly switcher: HTMLButtonElement;
  private readonly projectMenu: HTMLElement;
  private readonly projectSection: HTMLElement;
  private readonly projectNameEl: HTMLElement;
  private readonly projectPill: HTMLElement;
  private readonly projectProcessesEl: HTMLElement;
  // --- project terminals -----------------------------------------
  /** The TERMINALS subsection: header line (label + count + ＋ + ▾) and the
   *  bound-terminal rows. Built ONCE and updated in place — rebuilding it per
   *  render would tear the open profile menu out from under its own click. */
  private readonly projectTermsEl: HTMLElement;
  private readonly projectTermCount: HTMLElement;
  private readonly projectTermList: HTMLElement;
  private readonly projectTermMenu: HTMLElement;
  private readonly projectTermMenuBtn: HTMLButtonElement;
  private projects = new Map<number, ipc.ProjectInfoDto>();
  // --- project agents --------------------------------------------
  /** The AGENTS subsection of the expanded project (agents nest under their
   *  project): header
   *  line (label + N/M count + New agent ▸) and the bound agent rows. Built
   *  ONCE and updated in place by renderProjectAgents — the projectTermsEl
   *  precedent. */
  private readonly projectAgentsEl: HTMLElement;
  private readonly projectAgentCount: HTMLElement;
  private readonly projectAgentList: HTMLElement;
  /** What the AGENTS subsection last rendered (the projectTermsSig precedent):
   *  renderRail tails into it on EVERY event, and most change nothing a
   *  bound agent row shows. Sentinel start so the first render always builds. */
  private projectAgentsSig = "\u0000never-rendered";
  // --- scratchpads ------------------------------------------------
  /** The read-only SCRATCHPADS subsection under the expanded project: rows
   *  (name, age, actor, revision) → content modal with Copy. Agents write
   *  through chappa-ai-mcp; this is the human's read path. */
  private readonly scratchpads: ScratchpadsSection;
  // --- multi-project rail ----------------------------------------
  /** Every OPEN project's state, keyed by project id, in open order. */
  private openProjects = new Map<number, ProjectState>();
  /** The ONE project rendering the full section (header, picker, TERMINALS,
   *  COMMANDS). Replaces the `activeProjectId` everywhere it meant
   *  "the project the rail is showing" — every other open project renders a
   *  one-line summary row and keeps running untouched. */
  private expandedProjectId: number | null = null;
  /** Summary rows of the collapsed open projects. */
  private readonly projectSummariesEl: HTMLElement;
  // --- edit-project pane + project context menu ------------------
  /** The Edit-project pane: a full-window
   *  overlay reaching project settings (Name / sync / Notification level) plus
   *  an OVERVIEW card. Built lazily on first open, like the settings pane. */
  private editPane: EditPane | null = null;
  /** The project context menu: ONE element reused by the expanded
   *  project header and every collapsed summary row, with the three entries
   *  Edit project… / Close project / Remove project…. */
  private readonly projectCtxMenu: HTMLElement;
  private readonly projectCtxEdit: HTMLButtonElement;
  private readonly projectCtxClose: HTMLButtonElement;
  private readonly projectCtxRemove: HTMLButtonElement;
  /** Which project the open project context menu targets (a collapsed
   *  project's row can target a not-yet-expanded id). */
  private projectCtxMenuFor: number | null = null;
  // --- ACTIVE section --------------------------------------------
  /** The ACTIVE section at the top of the rail: house header + one row per
   *  running/starting process and per badged live terminal, across ALL open
   *  projects. Hidden entirely when empty. */
  private readonly activeSection: HTMLElement;
  private readonly activeCount: HTMLElement;
  private readonly activeList: HTMLElement;
  /** What the ACTIVE section last rendered (the projectTermsSig precedent):
   *  the render hooks fire on every rail/process repaint, and most events
   *  change nothing an ACTIVE row shows. Sentinel start so the first render
   *  always builds. */
  private activeSig = "0000never-rendered";
  /** What the TERMINALS subsection last rendered (the settings-pane
   *  profileSig precedent): renderRail tails into renderProjectTerminals on
   *  EVERY event, and most of them change nothing a bound-terminal row shows.
   *  Sentinel start so the first render always builds. */
  private projectTermsSig = "\u0000never-rendered";
  /** The stopped-command pane: one lazily built
   *  host shared by every stopped/never-started command; `emptyFor`
   *  names the command it currently fronts (project-scoped: two open
   *  projects can share a process name). */
  private emptyHost: HTMLDivElement | null = null;
  private emptyTitle: HTMLElement | null = null;
  private emptyFor: { projectId: number; name: string } | null = null;

  // --- notification levels ---------------------------------------
  /** Whether the chappa-ai WINDOW has focus. Half of the focused rule: "if
   *  the terminal is the active panel and the window is focused, suppress
   *  ENTIRELY". Seeded from document.hasFocus(), then kept by focus/blur. */
  private windowFocused = true;
  /** The per-command context menu (the first submenu in the app).
   *  ONE menu element reused by every row — the panel.ts pattern. */
  private readonly rowMenu: HTMLElement;
  private readonly rowMenuPrimary: HTMLButtonElement;
  private readonly rowMenuCopy: HTMLButtonElement;
  private readonly rowSubmenuParent: HTMLElement;
  private readonly rowSubmenu: HTMLElement;
  /** Which command the open context menu belongs to (project-scoped: two
   *  open projects can share a process name). */
  private rowMenuFor: { projectId: number; name: string } | null = null;
  // --- the full command menu --------------------------------------
  private readonly rowMenuFavorite: HTMLButtonElement;
  private readonly rowMenuRename: HTMLButtonElement;
  private readonly rowMenuEdit: HTMLButtonElement;
  private readonly rowMenuDelete: HTMLButtonElement;
  /** `Add ▸` / `Duplicate to ▸` / `Duplicate all commands to ▸` — rendered
   *  LAZILY on open (the notification submenu renders on menu show), so the
   *  closed menu never carries stale project lists. */
  private readonly rowAddParent: HTMLElement;
  private readonly rowAddSub: HTMLElement;
  private readonly rowDupParent: HTMLElement;
  private readonly rowDupSub: HTMLElement;
  private readonly rowDupAllParent: HTMLElement;
  private readonly rowDupAllSub: HTMLElement;
  /** The chappa.yml load-error line above the COMMANDS rows. */
  private readonly projectYmlErrorEl: HTMLElement;

  // --- notification center ----------------------------------------
  /** The session-only event ring behind the drawer, banners and bell badge. */
  private readonly center: NotifyCenter;
  /** Bottom-right in-app banner stack (the focused-window osNotify surface). */
  private readonly banners: NotifyBanners;
  /** The rail-footer bell (next to the gear) + its unread count chip. */
  private readonly bellBtn: HTMLButtonElement;
  private readonly bellCount: HTMLElement;
  /** The right-side center drawer, built lazily on first open. */
  private drawer: NotifyDrawer | null = null;
  private unsubscribeCenter: (() => void) | null = null;
  private readonly now: () => number;
  /** Unbinds for the shared capture-phase dismiss registrations (dismiss.ts
   *  — it replaced the four hand-rolled menu copies). */
  private readonly dismissUnbinds: Array<() => void> = [];

  // --- process stats ----------------------------------------------
  /** `term://stats` samples, keyed by term id: the rail row's `N%` chip and
   *  its mem/subprocess tooltip. The hint bar's copy lives on the panel. */
  private readonly stats = new StatsStore();

  constructor(opts: AppOptions) {
    this.api = opts.api ?? tauriApi;
    this.settings = opts.settings ?? settingsStore;
    this.agentTools = opts.agentTools ?? agentToolsStore;
    this.now = opts.now ?? Date.now;
    this.center = new NotifyCenter({ now: this.now });
    this.root = opts.root;
    this.root.textContent = "";
    // The flex-row layout (rail | stack) lives on this class; without it the
    // stack collapses to height 0 and every panel is "hidden" (zero-size
    // host) forever — no acks, no resize, black pane (found on the
    // host; jsdom tests cannot see layout).
    this.root.classList.add("chappa-app");
    injectAppStyles();

    const rail = document.createElement("aside");
    rail.className = "chappa-rail";

    // Project switcher dropdown + process section atop the rail.
    const projectBar = document.createElement("div");
    projectBar.className = "chappa-project-bar";
    this.switcher = document.createElement("button");
    this.switcher.className = "chappa-project-switcher";
    this.switcher.textContent = "Project…";
    // preventDefault keeps the hidden terminal textarea focused while the menu
    // toggles; the click still fires (this is the clickable that opens it).
    this.switcher.addEventListener("mousedown", (e) => e.preventDefault());
    this.switcher.addEventListener("click", () => this.toggleProjectMenu());
    this.projectMenu = document.createElement("div");
    this.projectMenu.className = "chappa-project-menu";
    this.projectMenu.style.display = "none";
    projectBar.append(this.switcher, this.projectMenu);
    // Capture-phase dismiss (the shared helper): a press inside the
    // menu must not hide it before the option's click can land. Preserved
    // drift: NO anchor exclusion (the switcher's press-while-open
    // close-then-reopen is long-standing) and NO Escape.
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.projectMenu.style.display !== "none",
        dismiss: () => this.hideProjectMenu(),
        inside: [this.projectMenu],
      }),
    );

    // ACTIVE section: the cross-project running/needs-attention
    // rows at the top of the rail. House header (uppercase muted + hairline
    // + right count, the chappa-subsection pattern); the list is rebuilt behind
    // the activeSig guard. Hidden entirely while empty.
    this.activeSection = document.createElement("section");
    this.activeSection.className = "chappa-active-section";
    this.activeSection.style.display = "none";
    const activeHeader = document.createElement("div");
    activeHeader.className = "chappa-subsection-header chappa-active-header";
    const activeLabel = document.createElement("span");
    // Section-OWN classes (styled identically): pinned tests reach
    // the TERMINALS subsection count via an unscoped ".chappa-subsection-count"
    // query, and this section renders before it in the DOM.
    activeLabel.className = "chappa-active-label";
    activeLabel.textContent = "ACTIVE";
    this.activeCount = document.createElement("span");
    this.activeCount.className = "chappa-active-count";
    this.activeCount.textContent = "0";
    activeHeader.append(activeLabel, this.activeCount);
    this.activeList = document.createElement("div");
    this.activeList.className = "chappa-active-list";
    this.activeSection.append(activeHeader, this.activeList);

    // Collapsed open projects: one-line summary rows, kept above
    // the expanded project's full section.
    this.projectSummariesEl = document.createElement("div");
    this.projectSummariesEl.className = "chappa-project-summaries";

    this.projectSection = document.createElement("section");
    this.projectSection.className = "chappa-project-section";
    this.projectSection.style.display = "none";
    this.projectNameEl = document.createElement("span");
    this.projectNameEl.className = "chappa-project-name";
    this.projectPill = document.createElement("span");
    this.projectPill.className = "chappa-project-pill";
    this.projectProcessesEl = document.createElement("div");
    this.projectProcessesEl.className = "chappa-project-processes";
    const projectHeader = document.createElement("div");
    projectHeader.className = "chappa-project-header";
    projectHeader.title = "project header actions: Ctrl+Shift+S/A/P";
    // Right-click the expanded project header for the project
    // context menu (Edit project… / Close project / Remove project…).
    projectHeader.addEventListener("contextmenu", (e) =>
      this.showProjectCtxMenu(e, this.expandedProjectId),
    );
    const headerActions = document.createElement("div");
    headerActions.className = "chappa-project-actions";
    const btnAuto = this.headerButton("S", "Start auto-starting");
    const btnAll = this.headerButton("A", "Start all");
    const btnStop = this.headerButton("P", "Stop all");
    btnAuto.addEventListener("click", () => void this.startAuto());
    btnAll.addEventListener("click", () => void this.startAll());
    btnStop.addEventListener("click", () => void this.stopAll());
    headerActions.append(btnAuto, btnAll, btnStop);
    // Row 1: the name alone (full width, readable). Row 2: pill + actions.
    // The rail is visually quieter: the sync toggle and
    // the notification-level picker moved into the Edit-project pane.
    const headerSub = document.createElement("div");
    headerSub.className = "chappa-project-header-sub";
    headerSub.append(this.projectPill, headerActions);
    projectHeader.append(this.projectNameEl, headerSub);

    // TERMINALS subsection. The project section has a TERMINALS
    // subsection between the header/picker and the COMMANDS rows:
    // house section header (uppercase muted + hairline + right count) with the
    // add affordance ON the header line — a small `+` (and the profile `▾` if
    // cheap, same menu as the rail's) at the right of the TERMINALS header.
    // Deliberate (2026-08-15): the add affordances ride the
    // header line, NOT a separate '+ Add terminal' row.
    this.projectTermsEl = document.createElement("div");
    this.projectTermsEl.className = "chappa-project-terms";
    const termsHeader = document.createElement("div");
    termsHeader.className = "chappa-subsection-header";
    const termsLabel = document.createElement("span");
    termsLabel.className = "chappa-subsection-label";
    termsLabel.textContent = "TERMINALS";
    this.projectTermCount = document.createElement("span");
    this.projectTermCount.className = "chappa-subsection-count";
    this.projectTermCount.textContent = "0";
    const termsAdd = document.createElement("button");
    termsAdd.className = "chappa-project-term-add";
    termsAdd.textContent = "＋";
    termsAdd.title = "New terminal in this project";
    termsAdd.addEventListener("mousedown", (e) => e.preventDefault());
    termsAdd.addEventListener("click", () => {
      this.hideProjectTermMenu();
      this.newProjectShell();
    });
    this.projectTermMenuBtn = document.createElement("button");
    this.projectTermMenuBtn.className = "chappa-project-term-more";
    this.projectTermMenuBtn.textContent = "▾";
    this.projectTermMenuBtn.title = "New project terminal from a shell profile";
    this.projectTermMenuBtn.addEventListener("mousedown", (e) => e.preventDefault());
    this.projectTermMenuBtn.addEventListener("click", () => this.toggleProjectTermMenu());
    this.projectTermMenu = document.createElement("div");
    this.projectTermMenu.className = "chappa-project-term-menu";
    this.projectTermMenu.style.display = "none";
    termsHeader.append(termsLabel, this.projectTermCount, termsAdd, this.projectTermMenuBtn, this.projectTermMenu);
    this.projectTermList = document.createElement("div");
    this.projectTermList.className = "chappa-project-term-list";
    this.projectTermsEl.append(termsHeader, this.projectTermList);
    // Same capture-phase dismiss as the rail's menu (shared helper).
    // Preserved drift: the ▾ toggle is anchor-excluded; no Escape.
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.projectTermMenu.style.display !== "none",
        dismiss: () => this.hideProjectTermMenu(),
        inside: [this.projectTermMenu, this.projectTermMenuBtn],
      }),
    );

    // "parse failed → all mutations disabled in UI with a visible
    // reason" — the reason sits right above the rows it disables.
    this.projectYmlErrorEl = document.createElement("div");
    this.projectYmlErrorEl.className = "chappa-project-yml-error";
    // AGENTS subsection: between the TERMINALS subsection and
    // SCRATCHPADS — agents nest under their project, where the work they
    // are doing lives. House header (uppercase muted +
    // hairline + right count) with the count reading running/total (`N/M`),
    // and the SAME "New agent ▸" spawner the workspace header has — the
    // shared builder, its pick spawning with THIS project's id. Rendered even
    // at 0/0: discoverability is the point (could not find agent
    // creation today); zero rows under it, no empty-state text.
    this.projectAgentsEl = document.createElement("div");
    this.projectAgentsEl.className = "chappa-project-agents";
    const agentsHeader = document.createElement("div");
    agentsHeader.className = "chappa-subsection-header";
    const agentsLabel = document.createElement("span");
    agentsLabel.className = "chappa-subsection-label";
    agentsLabel.textContent = "AGENTS";
    this.projectAgentCount = document.createElement("span");
    this.projectAgentCount.className = "chappa-subsection-count";
    this.projectAgentCount.textContent = "0/0";
    // The section spawner names the project explicitly; the
    // workspace spawner keeps the fallback chain.
    const agentsSpawner = this.buildAgentSpawner((toolId) =>
      void this.spawnAgent(toolId, this.expandedProjectId ?? undefined),
    );
    agentsHeader.append(agentsLabel, this.projectAgentCount, agentsSpawner.btn, agentsSpawner.menu);
    this.projectAgentList = document.createElement("div");
    this.projectAgentList.className = "chappa-project-term-list";
    this.projectAgentsEl.append(agentsHeader, this.projectAgentList);
    // SCRATCHPADS subsection: below COMMANDS, same house header.
    this.scratchpads = new ScratchpadsSection({ api: opts.scratchpads, modalContainer: this.root });
    this.projectSection.append(
      projectHeader,
      this.projectTermsEl,
      this.projectYmlErrorEl,
      this.projectProcessesEl,
      this.projectAgentsEl,
      this.scratchpads.element,
    );

    const header = document.createElement("div");
    header.className = "chappa-rail-header";
    const headerLabel = document.createElement("span");
    headerLabel.className = "chappa-rail-header-label";
    this.railCount = document.createElement("span");
    headerLabel.append("TERMINALS ", this.railCount);
    // New-terminal affordances: the ＋ is today's behaviour (the
    // platform default shell, empty command); the ▾ offers the ENABLED shell
    // profiles. Disabled profiles never appear.
    const newTerm = document.createElement("button");
    newTerm.className = "chappa-new-term";
    newTerm.textContent = "＋";
    newTerm.title = "New terminal (Ctrl+Shift+T)";
    newTerm.addEventListener("mousedown", (e) => e.preventDefault());
    newTerm.addEventListener("click", () => {
      this.hideNewTermMenu();
      void this.newShell();
    });
    this.newTermMenuBtn = document.createElement("button");
    this.newTermMenuBtn.className = "chappa-new-term";
    this.newTermMenuBtn.textContent = "▾";
    this.newTermMenuBtn.title = "New terminal from a shell profile";
    this.newTermMenuBtn.addEventListener("mousedown", (e) => e.preventDefault());
    this.newTermMenuBtn.addEventListener("click", () => this.toggleNewTermMenu());
    this.newTermMenu = document.createElement("div");
    this.newTermMenu.className = "chappa-new-term-menu";
    this.newTermMenu.style.display = "none";
    // "New agent ▸": the same header-line affordance pattern (task
    // 34), listing the ENABLED agent tools; a pick spawns into the expanded
    // project (cwd + identity env) with an empty prompt. Built by
    // the SHARED spawner builder — the project AGENTS section's spawner is
    // the other instance — and the workspace one keeps its current binding
    // behavior (the explicit project id is the section spawner's difference).
    this.newAgentSpawner = this.buildAgentSpawner((toolId) => void this.spawnAgent(toolId), () => {
      this.hideNewTermMenu();
    });
    header.append(
      headerLabel,
      newTerm,
      this.newTermMenuBtn,
      this.newAgentSpawner.btn,
      this.newTermMenu,
      this.newAgentSpawner.menu,
    );
    this.toasts = document.createElement("div");
    this.toasts.className = "chappa-toast-stack";
    // Same capture-phase dismiss as the project menu (shared helper, task
    // 35). Preserved drift: the ▾ toggle is anchor-excluded; no Escape.
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.newTermMenu.style.display !== "none",
        dismiss: () => this.hideNewTermMenu(),
        inside: [this.newTermMenu, this.newTermMenuBtn],
      }),
    );

    this.railList = document.createElement("div");
    this.railList.className = "chappa-rail-list";

    // Rail footer: the gear, beneath the launcher hint.
    // `.chappa-rail` is a column flexbox and
    // `.chappa-rail-list` is flex:1, so appending after the list pins this to
    // the bottom.
    const footer = document.createElement("div");
    footer.className = "chappa-rail-footer";
    this.gear = document.createElement("button");
    this.gear.className = "chappa-rail-gear";
    this.gear.title = "Settings";
    const gearGlyph = document.createElement("span");
    gearGlyph.textContent = "⚙";
    const gearLabel = document.createElement("span");
    gearLabel.textContent = "Settings";
    this.gear.append(gearGlyph, gearLabel);
    this.gear.addEventListener("mousedown", (e) => e.preventDefault());
    this.gear.addEventListener("click", () => this.toggleSettings());
    // The notification bell + unread count, NEXT TO the gear.
    this.bellBtn = document.createElement("button");
    this.bellBtn.className = "chappa-notify-bell";
    this.bellBtn.title = "Notifications";
    const bellGlyph = document.createElement("span");
    bellGlyph.append(bellIcon());
    this.bellCount = document.createElement("span");
    this.bellCount.className = "chappa-notify-bell-count";
    this.bellCount.style.display = "none";
    this.bellBtn.append(bellGlyph, this.bellCount);
    this.bellBtn.addEventListener("mousedown", (e) => e.preventDefault());
    this.bellBtn.addEventListener("click", () => this.toggleDrawer());
    footer.append(this.gear, this.bellBtn);

    // The workspace COMMANDS subsection. House header DIRECTLY ABOVE
    // the workspace TERMINALS section (label + count + `+ Add command` on the
    // header line, style), then the command-card rows. Zero rows still
    // shows just the header — no empty-state text (same rule as TERMINALS).
    this.wsCmdsEl = document.createElement("div");
    this.wsCmdsEl.className = "chappa-project-terms chappa-workspace-cmds";
    const wsHeader = document.createElement("div");
    wsHeader.className = "chappa-subsection-header";
    const wsLabel = document.createElement("span");
    wsLabel.className = "chappa-subsection-label";
    wsLabel.textContent = "COMMANDS";
    this.wsCmdCount = document.createElement("span");
    this.wsCmdCount.className = "chappa-subsection-count";
    this.wsCmdCount.textContent = "0";
    const wsAdd = document.createElement("button");
    wsAdd.className = "chappa-workspace-cmd-add";
    wsAdd.textContent = "+ Add command";
    wsAdd.addEventListener("mousedown", (e) => e.preventDefault());
    wsAdd.addEventListener("click", () => void this.addWorkspaceCommand());
    wsHeader.append(wsLabel, this.wsCmdCount, wsAdd);
    this.wsCmdList = document.createElement("div");
    this.wsCmdList.className = "chappa-project-term-list chappa-workspace-cmd-list";
    this.wsCmdsEl.append(wsHeader, this.wsCmdList);

    // ACTIVE sits at the TOP of the rail — above the project bar
    // and (a fortiori) above the workspace TERMINALS. The workspace COMMANDS
    // subsection sits directly ABOVE the workspace TERMINALS header
    // (`header`), and TERMINALS ABOVE the rail list.
    rail.append(
      this.activeSection,
      projectBar,
      this.projectSummariesEl,
      this.projectSection,
      this.wsCmdsEl,
      header,
      this.railList,
      footer,
    );

    this.stack = document.createElement("main");
    this.stack.className = "chappa-stack";

    // The per-command context menu. Built once, reused by every row;
    // it lives in the app root (not document.body) so it tears down with the
    // app and cannot outlive it.
    this.rowMenu = document.createElement("div");
    this.rowMenu.className = "chappa-row-menu";
    // Inline display is the source of truth for open/closed (the project- and
    // new-terminal-menu convention); the class rule is only the initial paint.
    this.rowMenu.style.display = "none";
    // The command-row context menu, in this order (completes it):
    // Start · Add to favorites
    // · Copy command · [Clear output — no reset command exposed, omitted] ·
    // Disable automatic renaming · "Lesser used" · Notification level ▸ · Edit
    // command… · Add ▸ · Duplicate to ▸ · Duplicate all commands to ▸ ·
    // Delete command "<name>".
    this.rowMenuPrimary = this.rowMenuItem("Start");
    this.rowMenuFavorite = this.rowMenuItem("Add to favorites");
    this.rowMenuFavorite.classList.add("chappa-row-menu-favorite");
    this.rowMenuCopy = this.rowMenuItem("Copy command");
    this.rowMenuRename = this.rowMenuItem("Disable automatic renaming");
    this.rowMenuRename.classList.add("chappa-row-menu-rename");
    const lesser = document.createElement("div");
    lesser.className = "chappa-row-menu-label";
    lesser.textContent = "Lesser used";
    this.rowMenu.appendChild(lesser);
    this.rowSubmenuParent = document.createElement("div");
    this.rowSubmenuParent.className = "chappa-row-submenu-parent";
    const subToggle = this.rowMenuItem("Notification level ▸", this.rowSubmenuParent);
    subToggle.classList.add("chappa-row-submenu-toggle");
    this.rowSubmenu = document.createElement("div");
    this.rowSubmenu.className = "chappa-row-submenu";
    this.rowSubmenuParent.appendChild(this.rowSubmenu);
    this.rowMenu.appendChild(this.rowSubmenuParent);
    // Hover OR click opens the submenu: hover is the pointer path, and a
    // click is the keyboard-less fallback and what jsdom can drive.
    this.rowSubmenuParent.addEventListener("mouseenter", () => this.openRowSubmenu());
    subToggle.addEventListener("click", () => this.openRowSubmenu());
    this.rowMenuEdit = this.rowMenuItem("Edit command…");
    this.rowMenuEdit.classList.add("chappa-row-menu-edit");
    [this.rowAddParent, this.rowAddSub] = this.rowSubmenuPair("Add ▸", "chappa-row-add");
    [this.rowDupParent, this.rowDupSub] = this.rowSubmenuPair("Duplicate to ▸", "chappa-row-dup");
    [this.rowDupAllParent, this.rowDupAllSub] = this.rowSubmenuPair(
      "Duplicate all commands to ▸",
      "chappa-row-dup-all",
    );
    this.rowMenuDelete = this.rowMenuItem("Delete command");
    this.rowMenuDelete.classList.add("chappa-row-menu-delete");
    this.rowMenuPrimary.addEventListener("click", () => this.onRowMenuPrimary());
    this.rowMenuFavorite.addEventListener("click", () => this.onRowMenuFavorite());
    this.rowMenuCopy.addEventListener("click", () => void this.onRowMenuCopy());
    this.rowMenuRename.addEventListener("click", () => this.onRowMenuRename());
    this.rowMenuEdit.addEventListener("click", () => void this.onRowMenuEdit());
    this.rowMenuDelete.addEventListener("click", () => void this.onRowMenuDelete());
    // Capture-phase dismiss with the inside-guard (shared helper):
    // a press INSIDE the menu (submenu included — a descendant) must not hide
    // it before the item's click can land. This copy is one of the two WITH
    // Escape (preserved drift).
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.rowMenu.style.display !== "none",
        dismiss: () => this.hideRowMenu(),
        inside: [this.rowMenu],
        escape: true,
      }),
    );

    // The project context menu: right-click the expanded project
    // header or a collapsed summary row. Three entries IN ORDER (the
    // a fuller menu — Commands/Add/Go to… — is out of
    // scope): Edit project…, Close project, Remove project…. The same
    // capture-phase dismiss with Escape as the command-row menu, anchored to
    // the one surface.
    this.projectCtxMenu = document.createElement("div");
    this.projectCtxMenu.className = "chappa-row-menu chappa-project-ctx";
    this.projectCtxMenu.style.display = "none";
    this.projectCtxEdit = this.rowMenuCtxItem("Edit project…");
    this.projectCtxClose = this.rowMenuCtxItem("Close project");
    this.projectCtxRemove = this.rowMenuCtxItem("Remove project…");
    this.projectCtxRemove.classList.add("chappa-project-ctx-remove");
    this.projectCtxEdit.addEventListener("click", () => this.onProjectCtxEdit());
    this.projectCtxClose.addEventListener("click", () => this.onProjectCtxClose());
    this.projectCtxRemove.addEventListener("click", () => this.onProjectCtxRemove());
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.projectCtxMenu.style.display !== "none",
        dismiss: () => this.hideProjectCtxMenu(),
        inside: [this.projectCtxMenu],
        escape: true,
      }),
    );

    // Window focus: half of the focused-terminal rule. jsdom fires
    // both events, so the tests drive the real path.
    if (typeof document !== "undefined" && typeof document.hasFocus === "function") {
      this.windowFocused = document.hasFocus();
    }
    window.addEventListener("focus", this.onWindowFocus);
    window.addEventListener("blur", this.onWindowBlur);
    // A desktop app never shows the webview's browser menu (Back / Refresh /
    // Print / Inspect — hit on a rail row, seen on the host).
    // Custom menus (terminal pane, command rows) preventDefault at their own
    // targets; this catches everywhere else. Text fields keep the native
    // menu (right-click copy/paste); devtools stays on F12 in dev builds.
    document.addEventListener("contextmenu", this.suppressNativeMenu);

    // The REDUCED workspace-command context menu — Start, Stop,
    // Edit command…, Copy command, Delete command. NO favorites, notification
    // level, YML badge, rename or Duplicate-to (a reduced subset). One
    // reused element, like rowMenu.
    this.wsCmdMenu = document.createElement("div");
    this.wsCmdMenu.className = "chappa-row-menu chappa-ws-cmd-menu";
    this.wsCmdMenu.style.display = "none";
    const wsStart = this.wsCmdMenuItem("Start", "chappa-ws-cmd-start");
    const wsStop = this.wsCmdMenuItem("Stop", "chappa-ws-cmd-stop");
    const wsEdit = this.wsCmdMenuItem("Edit command…", "chappa-ws-cmd-edit");
    const wsCopy = this.wsCmdMenuItem("Copy command", "chappa-ws-cmd-copy");
    const wsDelete = this.wsCmdMenuItem("Delete command", "chappa-ws-cmd-delete");
    const wsTarget = (): WorkspaceCommandRow | null =>
      this.wsCmdMenuFor ? (this.wsCmds.get(this.wsCmdMenuFor) ?? null) : null;
    wsStart.addEventListener("click", () => {
      const r = wsTarget();
      this.hideWsCmdMenu();
      if (r) void this.startWorkspace(r.name);
    });
    wsStop.addEventListener("click", () => {
      const r = wsTarget();
      this.hideWsCmdMenu();
      if (r) void this.stopWorkspace(r.name);
    });
    wsEdit.addEventListener("click", () => {
      const r = wsTarget();
      this.hideWsCmdMenu();
      if (r) void this.editWorkspaceCommand(r.name);
    });
    wsCopy.addEventListener("click", () => {
      const r = wsTarget();
      this.hideWsCmdMenu();
      if (r) this.copyWorkspaceCommand(r);
    });
    wsDelete.addEventListener("click", () => {
      const r = wsTarget();
      this.hideWsCmdMenu();
      if (r) void this.deleteWorkspaceCommand(r.name);
    });
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => this.wsCmdMenu.style.display !== "none",
        dismiss: () => this.hideWsCmdMenu(),
        inside: [this.wsCmdMenu],
      }),
    );

    this.root.append(
      rail,
      this.stack,
      this.rowMenu,
      this.projectCtxMenu,
      this.wsCmdMenu,
      this.toasts,
    );
    // Banner stack: bottom-right INSIDE the window, in the app root
    // so it tears down with the app. The drawer is built lazily on first open.
    this.banners = new NotifyBanners({
      container: this.root,
      onFocus: (entry) => this.focusCenterEntry(entry),
      onOpenCenter: () => this.openDrawer(),
      projectLabel: (id) => this.projectLabel(id),
    });
    this.unsubscribeCenter = this.center.subscribe(() => this.updateBellBadge());
    // In-app modals, never the webview's native dialogs — those render the
    // origin ("localhost:5173 says") and ignore the app palette entirely.
    this.confirmClose =
      opts.confirm ??
      ((name) => confirmDialog(`Close terminal "${name}"?`, { okLabel: "Close" }));
    this.confirmTrust =
      opts.confirmProjectTrust ??
      ((commands) =>
        confirmDialog(
          `Run these auto-start commands for this project?\n\n${commands.join("\n")}`,
          { okLabel: "Run" },
        ));
    this.confirmProjectClose =
      opts.confirmProjectClose ??
      ((name, running) =>
        confirmDialog(
          `Close project "${name}"? This stops:\n\n${running.join("\n")}`,
          { okLabel: "Close" },
        ));
    this.confirmProjectRemove =
      opts.confirmProjectRemove ??
      ((name, running) =>
        confirmDialog(
          `Remove project "${name}" from chappa-ai?\n\nThe folder and its ` +
            `chappa.yml stay on disk; trust, favorites and notification levels ` +
            `for it are forgotten.` +
            (running.length > 0 ? `\n\nThis stops:\n${running.join("\n")}` : ""),
          { okLabel: "Remove" },
        ));
    this.canBrowseDirectories = opts.canBrowseDirectories ?? ipc.inTauri();
    this.promptAddProject =
      opts.promptAddProject ?? ((browse) => addProjectDialog({ browse }));
    this.promptRename =
      opts.promptRename ??
      ((current) =>
        promptDialog("Rename project:", { value: current, okLabel: "Rename" }));
    this.promptCommand =
      opts.promptCommand ?? ((initial, title) => commandDialog(initial, { title }));
    this.promptWorkspaceCommand =
      opts.promptWorkspaceCommand ??
      ((initial, title) =>
        commandDialog(initial, { title, workdirHint: "absolute path (or empty for home)" }));
    this.confirmDeleteCommand =
      opts.confirmDeleteCommand ??
      // No "from chappa.yml": the definition may live in the
      // native store or workspace.json — naming the yml was wrong for both.
      ((name) =>
        confirmDialog(`Delete command "${name}"?`, { okLabel: "Delete" }));
    this.showError =
      opts.showError ?? (async (message) => void (await confirmDialog(message, { okLabel: "OK" })));
  }

  /**
   * Run one modal dialog under `key`, ignoring a re-entrant request for the
   * same key while the first is still on screen. Resolves null for the ignored
   * call — a null answer means "no decision was taken", which every caller
   * treats as "do nothing".
   */
  private async ask<T>(key: string, run: () => Promise<T>): Promise<T | null> {
    if (this.pendingDialogs.has(key)) return null;
    this.pendingDialogs.add(key);
    try {
      return await run();
    } finally {
      this.pendingDialogs.delete(key);
    }
  }

  /** Start the app: project switcher + event wiring + keyboard listener.
   *  BLANK START: no shell is auto-spawned — the app opens with an
   *  empty stack and the rail affordances. Only the stored `auto_start`
   *  workspace commands spawn, each with its own frames channel. */
  async mount(): Promise<void> {
    this.wireEvents();
    window.addEventListener("keydown", this.onWindowKeydown);
    // Settings load ONCE, before the first spawn: a panel built at the
    // default font and re-measured a beat later would make ConPTY
    // reflow-repaint its first prompt row (the host-run bug).
    await this.startSettings();
    await this.loadProjects();
    // After projects load, adopt every live terminal the app does
    // not already know (backend spawns that happened while the app was closed
    // or reloading — the vite-reload orphan / parked item).
    await this.reconcileAdoption();
    await this.loadWorkspaceCommands();
    await this.startAutoStartWorkspace();
  }

  /** Boot the settings store and subscribe: a font change re-measures every
   *  open panel. Wheel speed and synthetic prompt marks need NO frontend
   *  work — `set_settings` broadcasts them to every open actor and new spawns
   *  read them server-side. */
  private async startSettings(): Promise<void> {
    const s = await this.settings.load();
    this.fontSig = fontSignature(s);
    this.unsubscribeSettings = this.settings.subscribe((next) => this.onSettingsChanged(next));
  }

  private onSettingsChanged(s: Settings): void {
    const sig = fontSignature(s);
    if (sig === this.fontSig) return;
    this.fontSig = sig;
    for (const entry of this.entries.values()) {
      // setFontMetrics awaits the face, re-measures and resizes; the WebGL
      // renderer's own setMetrics resets the atlas and requests a full frame
      // when family/size changed, so there is nothing to duplicate here.
      void entry.panel.setFontMetrics(s.fontFamily, s.fontSize, s.lineHeight);
    }
  }

  /** Ctrl+Shift+T: open a fresh shell panel. `profile` names a
   *  shell profile from the new-terminal menu; without it the Rust
   *  side resolves the platform default shell, exactly as before.
   *
   *  `projectId` BINDS the new shell to a project: same spawn path,
   *  one extra association on the entry — the existing spawn path is reused,
   *  never forked. */
  async newShell(cwd?: string, profile?: string, projectId?: number): Promise<void> {
    const host = document.createElement("div");
    host.className = "chappa-panel-host";
    this.stack.appendChild(host);
    const panel = new TerminalPanel({
      container: host,
      api: this.api,
      settings: this.settings,
      cwd,
      profile,
      active: false,
      onCreated: (id) => this.onCreated(id, panel, host, profile ?? "shell", projectId),
    });
    try {
      await panel.start();
    } catch (err) {
      // Not in Tauri, or spawn failed: surface, don't leave an empty host.
      console.error(`[chappa-ai] failed to spawn shell: ${err}`);
      host.remove();
    }
  }

  /** Add the rail row the moment the terminal id is known, then reconcile so
   *  a term://status event that raced the create can't leave us at "starting". */
  private onCreated(
    id: number,
    panel: PanelHost,
    host: HTMLElement,
    name: string,
    projectId?: number,
  ): void {
    // The id is OURS now — a `term://created` broadcast (or the
    // mount reconcile) must not adopt it into a duplicate row.
    this.seen.add(id);
    this.entries.set(id, {
      id,
      panel,
      host,
      name,
      status: "starting",
      exitCode: null,
      title: null,
      activity: false,
      bell: false,
      attention: null,
      // A project-bound shell renders in that project's TERMINALS
      // subsection instead of the workspace rail.
      ...(projectId === undefined
        ? {}
        : { kind: "project-shell" as const, projectShell: { projectId } }),
    });
    this.lruAcquire(id);
    this.activate(id);
    this.renderRail();
    void this.reconcile();
  }

  // --- adopt backend-spawned terminals ------------------------------

  /** `term://created` — a terminal the app did NOT initiate appeared on the
   *  backend (MCP/control spawn, agent spawn, project process). Adopt it as a
   *  rail row in the right section; no pane until the user clicks it. */
  private applyCreated(p: ipc.TerminalCreatedEvent): void {
    this.adopt({
      term_id: p.term_id,
      name: p.name,
      kind: p.kind,
      project_id: p.project_id,
      agent_tool_id: p.agent_tool_id,
      // The broadcast carries the parent so the row nests immediately.
      parent_id: p.parent_process_id ?? null,
    });
  }

  /** The mount reconcile: adopt every LIVE terminal the app does not already
   *  know — the vite-reload / app-was-closed orphan case (the parked
   *  item). Runs after projects load so placement (bound agents) is resolved.
   *  The reconcile row (`list_terminals`' RailRow) builds the SAME shape the
   *  created broadcast does, so both ingest paths place a row identically.
   */
  private async reconcileAdoption(): Promise<void> {
    try {
      const list = await this.api.listTerminals();
      if (!list) return;
      for (const t of list) {
        this.adopt({
          term_id: t.id,
          name: t.name,
          kind: t.kind ?? "terminal",
          project_id: t.project_id ?? null,
          agent_tool_id: t.agent_tool_id ?? null,
          parent_id: t.parent_process_id ?? null,
        });
      }
    } catch {
      // Not in Tauri / transient — a later term://created catches up.
    }
  }

  /** Adopt a terminal into the rail, placing it by kind/binding:
   *   - agent bound to the EXPANDED project → that project's AGENTS section;
   *   - agent unbound → workspace agents;
   *   - plain terminal → workspace TERMINALS;
   *   - process → NO rail row (it renders as its project's command card) —
   *     the id is still remembered so a later broadcast/reconcile does not
   *     re-adopt it.
   *  The row uses the shared anatomy + close ×; no panel yet (placeholder),
   *  built on first click via [`realizeAdopted`]. Idempotent by id.
   */
  private adopt(f: {
    term_id: number;
    name: string;
    kind: "terminal" | "agent" | "process";
    project_id: number | null;
    agent_tool_id: number | null;
    /** The spawning process's id — the forest nests this row under
     *  it when that parent is on the same surface. */
    parent_id: number | null;
  }): void {
    const id = f.term_id;
    if (this.seen.has(id) || this.entries.has(id)) return;
    this.seen.add(id);
    const host = document.createElement("div");
    let entry: RailEntry = {
      id,
      panel: this.placeholderPanel(host),
      host,
      name: f.name,
      status: "starting",
      exitCode: null,
      title: null,
      activity: false,
      bell: false,
      attention: null,
      adopted: true,
      kind: "shell",
    };
    if (f.kind === "agent") {
      entry = {
        ...entry,
        kind: "agent",
        agent: {
          toolId: f.agent_tool_id ?? 0,
          toolType: "custom",
          model: null,
          runtime: { kind: "host" },
          projectId: f.project_id,
          container: null,
          bridge: "ok",
          transport: "tty",
          parentId: f.parent_id,
        },
      };
    } else if (f.kind === "process") {
      entry = {
        ...entry,
        kind: "process",
        process: { projectId: f.project_id ?? 0, name: f.name },
      };
      // A process terminal renders as its project's command card, not a rail
      // row — but it must still live in `entries` so clicking its card (or a
      // reconcile) can realize a live pane. The id is already `seen` above.
    }
    this.entries.set(id, entry);
    this.renderRail();
  }

  /** The inert panel an adopted row holds until its first click: no panel
   *  yet, the row alone. Realized by [`realizeAdopted`] the moment the user
   *  focuses the row. */
  private placeholderPanel(host: HTMLElement): PanelHost {
    return {
      host,
      start: () => Promise.reject(new Error("adopted terminal has no panel until attach")),
      setActive: () => {},
      focusInput: () => {},
      pasteText: () => {},
      setFontMetrics: async () => {},
      setSubprocessCount: () => {},
      glLive: false,
      releaseGlContext: () => {},
      restoreGlContext: () => {},
      dispose: () => {},
    };
  }

  /** First click on an adopted row: build the real pane, wiring it via
   *  `attach_terminal` (frames flow from the FULL resync; input/resize/scroll
   *  are the ordinary commands after that), then activate like any panel. */
  private async realizeAdopted(id: number): Promise<void> {
    const existing = this.entries.get(id);
    if (!existing?.adopted) return;
    const host = document.createElement("div");
    host.className = "chappa-panel-host";
    this.stack.appendChild(host);
    const panel = new TerminalPanel({
      container: host,
      api: this.api,
      settings: this.settings,
      active: false,
      spawn: (onFrame, dims) => this.attachAdoptedChannel(id, onFrame, dims),
    });
    try {
      await panel.start();
    } catch (err) {
      // Nothing to show — drop the host and keep the (unattached) row.
      console.error(`[chappa-ai] failed to attach adopted terminal ${id}: ${err}`);
      host.remove();
      return;
    }
    const base = this.entries.get(id);
    if (!base) {
      host.remove();
      return;
    }
    this.entries.set(id, { ...base, panel, host, adopted: false });
    this.lruAcquire(id);
    this.activate(id);
  }

  /** The attach seam for a realized adopted row: hand the panel's frames
   *  channel to `attach_terminal` and answer the (known) id. */
  private attachAdoptedChannel(
    id: number,
    onFrame: (buf: ArrayBuffer) => void,
    _dims?: { cols: number; rows: number },
  ): Promise<number> {
    const frames = ipc.makeProjectFramesChannel((data) => {
      onFrame(data instanceof ArrayBuffer ? data : Uint8Array.from(data).buffer);
    });
    return this.api.attachTerminal(id, frames).then(() => id);
  }

  /** Bring `id` into the GL budget (acquire; evict the LRU victim if over 8). */
  private lruAcquire(id: number): void {
    const entry = this.entries.get(id);
    if (!entry || !entry.panel.glLive) return;
    const evict = this.lru.acquire(id);
    if (evict !== null && evict !== id) {
      this.entries.get(evict)?.panel.releaseGlContext();
      this.lru.release(evict);
    }
  }

  /** Hydrate rail statuses from the Rust snapshot (list_terminals). */
  private async reconcile(): Promise<void> {
    try {
      const list = await this.api.listTerminals();
      if (!list) return;
      for (const t of list) {
        const entry = this.entries.get(t.id);
        if (!entry) continue;
        this.entries.set(
          t.id,
          railReducer(entry, {
            type: "status",
            status: t.status as RailStatus,
            exitCode: t.exit_code ?? null,
          }),
        );
      }
      this.renderRail();
    } catch {
      /* not in tauri, or transient — the pump's events will catch up */
    }
  }

  /** Make `id` the visible, focused terminal. */
  activate(id: number): void {
    // An ADOPTED row has no pane yet — the click builds it via
    // attach_terminal (realizeAdopted re-invokes this once realized).
    const pending = this.entries.get(id);
    if (pending?.adopted) {
      void this.realizeAdopted(id);
      return;
    }
    if (this.activeId === id) {
      // Same panel re-clicked: the click just moved DOM focus off the
      // terminal's textarea — give it back so typing works.
      this.entries.get(id)?.panel.focusInput();
      return;
    }
    this.hideProcessEmptyState();
    this.activeId = id;
    for (const [entryId, entry] of this.entries) {
      const active = entryId === id;
      // Host visibility BEFORE setActive: setActive(true) focuses the hidden
      // textarea, and focus() inside a display:none subtree is a silent
      // no-op (first click on a rail row/card typed nothing until
      // a second click; jsdom cannot see this — its focus ignores CSS).
      entry.host.classList.toggle("active", active);
      entry.panel.setActive(active);
      if (active) {
        // Restore any LRU-released context, then re-budget.
        entry.panel.restoreGlContext();
        this.lruAcquire(entryId);
        this.entries.set(
          entryId,
          railReducer(this.entries.get(entryId)!, { type: "activate" }),
        );
      }
    }
    // Process rows mirror the activation (the accent marker is how you can
    // tell which project pane the stack is showing). Activating a card also
    // clears ITS badges — same "shown until activate" rule as the rail.
    // Every open project's rows are walked: a collapsed project's row DOM is
    // detached but its badge STATE must still clear on activation (// "per-terminal badges persist until activation").
    for (const state of this.openProjects.values()) {
      for (const row of state.processRows.values()) {
        const isActive = row.termId === id;
        row.el?.classList.toggle("active", isActive);
        if (isActive && (row.bell || row.activity)) {
          row.bell = false;
          row.activity = false;
          this.updateProcessBadges(row);
        }
      }
    }
    this.renderRail();
  }

  /** Ctrl+PgUp/PgDn cycle, wrapping. */
  private cycle(dir: 1 | -1): void {
    const ids = [...this.entries.keys()];
    if (ids.length === 0) return;
    const idx = ids.indexOf(this.activeId ?? -1);
    const next = ((idx < 0 ? 0 : idx) + dir + ids.length) % ids.length;
    this.activate(ids[next]);
  }

  /** Middle-click/× close; running terminals get a confirmation. Async
   *  because the confirmation is an in-app modal, not `window.confirm`. */
  async closePanel(id: number): Promise<void> {
    const entry = this.entries.get(id);
    if (!entry) return;
    const running = entry.status === "starting" || entry.status === "running";
    if (running) {
      const ok = await this.ask(`close:${id}`, () =>
        this.confirmClose(entry.title ?? entry.name),
      );
      if (ok !== true) return;
      // The panel can have gone away while the modal was up (an exit, a
      // closeAll teardown) — forceClosePanel would be a no-op, but re-checking
      // keeps the intent readable.
      if (!this.entries.has(id)) return;
    }
    this.forceClosePanel(id);
  }

  /** Close `id` unconditionally (app teardown — never prompts). Process
   *  panels are stopped via stop_project_process (which closes the terminal
   *  and updates the process lifecycle), then disposed. Agent panels (task
   *  31) close through `closeTerminal` HERE — the answer is the container-
   *  side verification, toasted when it is not `gone`. */
  private forceClosePanel(id: number): void {
    const entry = this.entries.get(id);
    if (!entry) return;
    let closeViaPanel = true;
    // An ADOPTED row never spawned the terminal — closing it is the
    // backend terminal's own close_terminal, exactly like an agent's (the
    // pane, if any, was attached into an existing terminal). Adopted AGENT
    // rows fall through to the agent branch below; adopted shells/processes
    // are covered here. The agent branch additionally toasts the container
    // verification, so adopted agents keep BOTH behaviors.
    if (entry.adopted && entry.kind !== "agent" && entry.kind !== "process") {
      void this.api.closeTerminal(id).catch(() => {});
    }
    if (entry.kind === "agent") {
      closeViaPanel = false;
      const label = this.entryLabel(entry);
      void this.api
        .closeTerminal(id)
        .then((verification) => {
          if (verification && verification !== "gone") {
            this.showToast(`${label}: container-side process ${verification}`, verification);
          }
        })
        .catch(() => {});
    }
    if (entry.kind === "process" && entry.process) {
      const { projectId, name } = entry.process;
      void this.api.stopProjectProcess(projectId, name).catch(() => {});
      const row = this.rowFor(projectId, name);
      if (row && row.panel === entry.panel) {
        row.termId = null;
        row.status = "stopped";
        this.processRowChanged(row);
      }
    }
    this.dropEntry(id, closeViaPanel);
  }

  /** Tear down an entry's DOM/state WITHOUT any backend call — the
   *  `term://closed` drop path (the backend already closed the terminal), and
   *  the shared tail of [`forceClosePanel`] (whose backend calls ran above).
   *  Idempotent by id: a missing entry is a no-op, so the frontend's own ×
   *  path (which called close_terminal → the backend broadcasts term://closed)
   *  does not double-remove. An adopted row with no panel yet (placeholder)
   *  simply disappears. */
  private dropEntry(id: number, closeViaPanel: boolean): void {
    if (!this.entries.has(id)) return;
    const entry = this.entries.get(id)!;
    // A process row (command card) references this entry's panel — keep it
    // consistent without re-stopping (the backend already stopped it).
    if (entry.kind === "process" && entry.process) {
      const row = this.rowFor(entry.process.projectId, entry.process.name);
      if (row && row.panel === entry.panel) {
        row.termId = null;
        row.status = "stopped";
        this.processRowChanged(row);
      }
    }
    entry.panel.dispose(closeViaPanel);
    this.entries.delete(id);
    this.lru.release(id);
    // The row is gone, so its last sample is too — a recycled term id
    // must never inherit the previous terminal's `N%`.
    this.stats.forget(id);
    entry.host.remove();
    if (this.activeId === id) {
      this.activeId = null;
      const ids = [...this.entries.keys()];
      if (ids.length > 0) this.activate(ids[ids.length - 1]);
    }
    this.renderRail();
  }

  /** Close every panel (app teardown, no confirmations). Every OPEN
   *  project's process panels are `entries` too (a collapsed project's panels
   *  stay in the stack, hidden), so this loop stops ALL open
   *  projects' processes, not just the expanded one's; forceClosePanel routes
   *  each `process` entry through stop_project_process on its OWN project. */
  closeAll(): void {
    for (const id of [...this.entries.keys()]) this.forceClosePanel(id);
    this.openProjects.clear();
    this.expandedProjectId = null;
    this.unsubscribeSettings?.();
    this.unsubscribeSettings = null;
    this.settingsPane?.dispose();
    this.scratchpads.dispose();
    this.settingsPane = null;
    this.editPane?.dispose();
    this.editPane = null;
    this.banners.dispose();
    this.drawer?.dispose();
    this.drawer = null;
    this.unsubscribeCenter?.();
    this.unsubscribeCenter = null;
    for (const unbind of this.dismissUnbinds) unbind();
    this.dismissUnbinds.length = 0;
    window.removeEventListener("focus", this.onWindowFocus);
    window.removeEventListener("blur", this.onWindowBlur);
    document.removeEventListener("contextmenu", this.suppressNativeMenu);
  }

  /** Swallow the webview's native context menu except on editable fields. */
  private readonly suppressNativeMenu = (e: MouseEvent): void => {
    const t = e.target as HTMLElement | null;
    if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t?.isContentEditable) {
      return;
    }
    e.preventDefault();
  };

  /** OS file drop → quoted paths pasted into the active terminal. This is
   *  the ONE code path for drops (quotePaths → normal paste, bracketed-paste
   *  guard applies): Tauri's default drag-drop interception means the webview
   *  never receives an HTML5 drop with usable paths, so wireEvents feeds this
   *  from `tauri://drag-drop` (InputController's own drop handler only covers
   *  configs with the interception disabled — same quotePaths route). */
  handleFileDrop(paths: string[]): void {
    if (paths.length === 0 || this.activeId === null) return;
    this.entries.get(this.activeId)?.panel.pasteText(quotePaths(paths));
  }

  private onWindowKeydown = (e: KeyboardEvent): void => {
    if (isPanelShortcut(e)) {
      e.preventDefault();
      if (e.key === "PageUp") this.cycle(-1);
      else if (e.key === "PageDown") this.cycle(1);
      else if (e.key.toLowerCase() === "t") void this.newShell();
      else if (e.key.toLowerCase() === "w" && this.activeId !== null) void this.closePanel(this.activeId);
      return;
    }
    if (isProjectShortcut(e)) {
      // Project-header keys: S start auto-starting,
      // A start all, P stop all. The terminal never sees these (input.ts
      // reserves them).
      e.preventDefault();
      const k = e.key.toLowerCase();
      if (k === "s") void this.startAuto();
      else if (k === "a") void this.startAll();
      else if (k === "p") void this.stopAll();
    }
  };

  private async wireEvents(): Promise<void> {
    if (!ipc.inTauri()) return;
    const on = <T>(event: string, apply: (payload: T) => void): void => {
      void ipc.listenEvent<T>(event, apply).catch(() => {});
    };
    // A terminal the app did NOT spawn appeared on the backend —
    // adopt it into a rail row (placed by kind/binding; pane attaches on click).
    on<ipc.TerminalCreatedEvent>("term://created", (p) => this.applyCreated(p));
    // The backend CLOSED a terminal (the frontend × is one source,
    // but so are control/MCP close_terminal, project stop, and close_on_exit).
    // Drop the row/panel locally — no backend call (it is already gone). An
    // unknown id is a no-op; the frontend's own × path must not double-remove
    // (this is idempotent by id — the entry is already deleted).
    on<ipc.TerminalClosedEvent>("term://closed", (p) => this.dropEntry(p.term_id, false));
    on<ipc.TerminalEventDto>("term://status", (p) => {
      const entry = this.entries.get(p.term_id);
      if (!entry) return;
      this.entries.set(
        p.term_id,
        railReducer(entry, {
          type: "status",
          status: (p.status ?? entry.status) as RailStatus,
          exitCode: p.exit_code ?? null,
        }),
      );
      this.renderRail();
    });
    on<ipc.TerminalEventDto>("term://title", (p) => this.applyTitle(p));
    on<ipc.TerminalEventDto>("term://activity", (p) => this.applyActivity(p));
    on<ipc.TerminalEventDto>("term://bell", (p) => this.applyBell(p));
    // Process stats: rail `N%` + the panel hint bar's subprocess
    // count. Low-rate (2s tick, material changes only) — same emit-only shape
    // as term://activity above.
    on<ipc.TerminalStatsEvent>("term://stats", (p) => this.applyStats(p));
    // The docker-exec bridge probe's transitions (once each).
    on<ipc.AgentBridgeEvent>("agent://bridge", (p) => this.applyBridge(p));
    on<ipc.AgentReapEvent>("agent://reap", (p) => this.applyReap(p));
    // A json-transport agent's typed events.
    on<ipc.AgentEventEvent>("agent://event", (p) => this.applyAgentEvent(p));
    // Tauri intercepts OS drag-drop before the webview sees it; the drop
    // event carries the real OS paths (HTML5 File objects never do here).
    on<{ paths?: string[] }>("tauri://drag-drop", (p) => {
      this.handleFileDrop(p.paths ?? []);
    });
    // OSC 9/777/99 notifications: the Rust pump no longer toasts on its own
    // — everything routes through the decision function.
    on<ipc.TerminalEventDto>("term://notify", (p) => this.applyNotify(p));
    // Project process rows: the process-row lifecycle source. The
    // registry's term://status stays the per-terminal source; this carries
    // the project-scoped status, exit code and (for respawns) stable term id.
    on<ipc.ProjectProcessStatusEvent>("project://process_status", (p) => {
      this.applyProcessStatus(p);
    });
    // Workspace-command card status (the appointed terminal still
    // emits term://status for its rail row).
    on<ipc.WorkspaceStatusEvent>("workspace://status", (p) => this.applyWorkspaceStatus(p));
    // Chappa.yml changed on disk (a hand edit, or our
    // own write-back echoing through the watcher) — re-fetch the rows, or show
    // the reason.
    on<ipc.ProjectYmlReloadedEvent>("project://yml_reloaded", (p) => {
      void this.applyYmlReloaded(p);
    });
    // An agent (chappa-ai-mcp → control surface) asked to start a
    // process. Rust cannot fabricate the frames Channel a spawn needs, so
    // the request lands here and takes the exact rail-click path.
    on<ipc.ProjectStartRequestedEvent>("project://start_requested", (p) => {
      this.applyStartRequested(p);
    });
  }

  /** `term://title` — an OSC title. A process whose row has
   *  `Disable automatic renaming` set keeps its name (the event is dropped
   *  before the reducer, so nothing downstream ever sees the title). */
  private applyTitle(p: ipc.TerminalEventDto): void {
    const entry = this.entries.get(p.term_id);
    if (!entry) return;
    if (entry.kind === "process" && entry.process) {
      const row = this.rowFor(entry.process.projectId, entry.process.name);
      if (row?.disableAutoRename) return;
    }
    this.entries.set(p.term_id, railReducer(entry, { type: "title", title: p.title ?? "" }));
    this.renderRail();
  }

  /** `project://yml_reloaded`. */
  private async applyYmlReloaded(p: ipc.ProjectYmlReloadedEvent): Promise<void> {
    const state = this.openProjects.get(p.project_id);
    if (!state) return;
    state.ymlError = p.error;
    if (p.error !== null) {
      // Rows stay as they were (Rust restarted/removed nothing); only the
      // reason and the disabled mutations change.
      if (this.expandedProjectId === p.project_id) this.renderProject();
      return;
    }
    const seq = ++state.reloadSeq;
    let rows: ipc.ProjectProcessDto[];
    try {
      rows = await this.api.listProjectProcesses(p.project_id);
    } catch {
      return; // the next reload (or mutation answer) carries the rows
    }
    // A newer source (a later reload, or a mutation's answer) landed while
    // this fetch was in flight: its snapshot is stale — drop it.
    if (this.openProjects.get(p.project_id)?.reloadSeq !== seq) return;
    this.applyProcessList(p.project_id, rows);
    if (p.trust_pending) void this.reloadTrustGate(p.project_id, p.trust_commands ?? []);
  }

  /** The trust gate re-armed by a reload of UNTRUSTED content (
   *  review: trust is per content hash, so an external edit of chappa.yml is
   *  gated exactly like an open). Rust already stopped the affected running
   *  processes; Run records the hash — the user starts them by hand (a reload
   *  never auto-starts). An empty list is recorded silently, as at open. */
  private async reloadTrustGate(id: number, commands: string[]): Promise<void> {
    if (!this.openProjects.has(id)) return;
    let run: boolean | null = true;
    if (commands.length > 0) {
      run = await this.ask(`trust:${id}`, () => this.confirmTrust(commands));
      if (run === null) return; // the same gate is already on screen
    }
    try {
      await this.api.confirmProjectTrust(id, run);
    } catch {
      // Transient IPC failure: the gate re-arms on the next reload/open.
    }
  }

  /** Adopt a fresh row list for an open project (a reload's
   *  re-fetch, or a mutation command's answer). Rows that survive by name
   *  keep their panel/host/badges and take the new definition + status; rows
   *  that vanished dispose their panel; new names get fresh rows. Order
   *  follows the list (= the file). */
  private applyProcessList(id: number, dtos: ipc.ProjectProcessDto[]): void {
    const state = this.openProjects.get(id);
    if (!state) return;
    const next = new Map<string, ProcessRow>();
    for (const dto of dtos) {
      const existing = state.processRows.get(dto.name);
      if (!existing) {
        next.set(dto.name, this.rowFromDto(id, dto));
        continue;
      }
      existing.command = dto.command;
      existing.autoStart = dto.autoStart;
      existing.autoRestart = dto.autoRestart;
      existing.restartWhenChanged = dto.restartWhenChanged ?? [];
      existing.workingDir = dto.workingDir ?? null;
      existing.env = dto.env ?? {};
      existing.favorite = dto.favorite ?? false;
      existing.disableAutoRename = dto.disableAutoRename ?? false;
      existing.notificationLevel = dto.notificationLevel;
      existing.status = dto.status as RailStatus;
      existing.exitCode = dto.exitCode;
      // Rust dropped the terminal (a reload stopped it): the panel is stale.
      // A restart-in-place keeps the id, so a same-id answer keeps the panel.
      if (dto.termId === null && existing.termId !== null) this.disposeProcessPanel(existing);
      next.set(dto.name, existing);
    }
    for (const [name, row] of state.processRows) {
      if (next.has(name)) continue;
      this.disposeProcessPanel(row);
      if (this.emptyFor?.projectId === id && this.emptyFor.name === name) {
        this.hideProcessEmptyState();
      }
    }
    state.processRows = next;
    if (this.expandedProjectId === id) {
      this.renderProject();
    } else {
      this.refreshProjectSummary(id);
      this.renderActiveSection();
    }
  }

  /** One process row from its wire DTO (open + reload). The `??` fallbacks
   *  keep a pre-answer (no definition/toggle fields) loadable. */
  private rowFromDto(projectId: number, p: ipc.ProjectProcessDto): ProcessRow {
    return {
      projectId,
      name: p.name,
      command: p.command,
      status: p.status as RailStatus,
      autoStart: p.autoStart,
      autoRestart: p.autoRestart,
      restartWhenChanged: p.restartWhenChanged ?? [],
      workingDir: p.workingDir ?? null,
      env: p.env ?? {},
      favorite: p.favorite ?? false,
      disableAutoRename: p.disableAutoRename ?? false,
      exitCode: p.exitCode,
      termId: p.termId,
      panel: null,
      host: null,
      notificationLevel: p.notificationLevel,
      bell: false,
      activity: false,
      el: null,
      pill: null,
      bellEl: null,
      activityEl: null,
      startBtn: null,
      stopBtn: null,
      restartBtn: null,
    };
  }

  /** Apply one `project://start_requested` event: start the named process
   *  of that project if it is open here (idempotent on a live process — same
   *  guard as a click). Unknown project/process = ignored. */
  private applyStartRequested(p: ipc.ProjectStartRequestedEvent): void {
    if (!this.rowFor(p.project_id, p.name)) return;
    void this.startProcess(p.project_id, p.name);
  }

  /** Apply one `project://process_status` event to its process row (extracted
   *  from the Tauri event wiring so the interaction tests can drive it). */
  private applyProcessStatus(p: ipc.ProjectProcessStatusEvent): void {
    // Boundary: the row is addressed by (project_id, name) — two open
    // projects can each own a process of the same name, and this event must
    // land on ITS project's row only.
    const row = this.rowFor(p.project_id, p.name);
    if (!row) return;
    row.status = (p.status ?? row.status) as RailStatus;
    row.exitCode = p.exit_code ?? null;
    if (p.term_id !== null && p.term_id !== undefined) row.termId = p.term_id;
    this.processRowChanged(row);
  }

  // --- notification events ----------------------------------------
  //
  // Every bell/notify goes through `notifications.decide` — the ONE decision
  // point. The old unconditional badge (which badged even the focused pane) is
  // gone: the focused-terminal rule says a focused terminal's event is
  // "suppress[ed] ENTIRELY — no OS notification AND no badge".

  /** `term://activity` — a hidden panel produced output. Not a notification
   *  event (no level, never an OS toast), but it shares the badge plumbing so
   *  a process card can show it too. */
  private applyActivity(p: ipc.TerminalEventDto): void {
    const entry = this.entries.get(p.term_id);
    if (!entry || p.term_id === this.activeId) return;
    // Same split as badgeEntry: process terminals have no rail row — the card
    // is their only badge surface, and the rail rebuild would change nothing.
    if (entry.kind === "process") {
      this.markProcessCard(entry, "activity");
      return;
    }
    this.entries.set(p.term_id, railReducer(entry, { type: "activity" }));
    // A collapsed project's bound terminal has no visible row — aggregate
    // onto its summary; the entry badge itself persists for expand.
    if (entry.kind === "project-shell" && entry.projectShell) {
      this.aggregateBadge(entry.projectShell.projectId, "activity");
    }
    this.renderRail();
  }

  /** `term://stats` — one terminal's process-tree cost. Not a
   *  notification event: no level, no badge, no OS toast, and it never marks a
   *  terminal as needing attention. Purely the two readouts.
   *
   *  Both gates matter. Rust already suppresses immaterial change, so an
   *  unchanged payload here is a redelivery or an exit's second zero — either
   *  way it must not cost a rail rebuild, which is the whole point of the
   *  "reserve the space" rule: stats appearing must not jitter or
   *  re-truncate row labels. */
  private applyStats(p: ipc.TerminalStatsEvent): void {
    if (!this.stats.apply(p)) return;
    const entry = this.entries.get(p.term_id);
    if (!entry) return;
    entry.panel.setSubprocessCount(this.stats.get(p.term_id)?.subprocCount ?? 0);
    this.renderRail();
  }

  /** `term://bell` — a BEL. All = bell → OS notification too;
   *  Important = bell → badge only; None = badges silently. Bells carry no
   *  body, so the numeric-only rule never applies to them. */
  private applyBell(p: ipc.TerminalEventDto): void {
    const entry = this.entries.get(p.term_id);
    if (!entry) return;
    const d = decide("bell", this.levelForEntry(entry), p.term_id === this.activeId, this.windowFocused);
    this.routeDecision(p.term_id, entry, "bell", d, this.entryLabel(entry), "Bell");
  }

  /** `term://notify` — OSC 9/777/99. OSC 9 sends an empty title and the
   *  message as the body, so an empty title falls back to the terminal's
   *  display name (a titleless toast is unattributable). */
  private applyNotify(p: ipc.TerminalEventDto): void {
    const entry = this.entries.get(p.term_id);
    if (!entry) return;
    const body = typeof p.body === "string" ? p.body : "";
    const title = typeof p.title === "string" && p.title.trim() !== "" ? p.title : this.entryLabel(entry);
    const d = decide("notify", this.levelForEntry(entry), p.term_id === this.activeId, this.windowFocused, body);
    this.routeDecision(p.term_id, entry, "notify", d, title, body);
  }

  /**
   * Act on one `decide()` answer — the single decision point's single
   * OUTPUT point. `decide()` itself stays a pure
   * 2-output function; this caller maps its `osNotify` onto the toast-vs-
   * banner split:
   *  - window UNFOCUSED → OS toast (as before);
   *  - window FOCUSED → in-app banner instead (the terminal is necessarily
   *    a non-active terminal, else it was suppressed — an OS toast for
   *    a window you are looking at is the wrong tool, and dev-build toasts
   *    attribute to PowerShell with dead clicks).
   * Badge outcomes are unchanged.
   *
   * Center recording: "An event is RECORDED iff decide() produced any output
   * (badge or toast) — focused-suppressed events do NOT enter the center,
   * matching the measured behavior" (verdict: "a
   * focused terminal's produces nothing at all, center included").
   */
  private routeDecision(
    termId: number,
    entry: RailEntry,
    kind: "bell" | "notify",
    d: { osNotify: boolean; badge: boolean },
    title: string,
    body: string,
  ): void {
    if (d.badge) this.badgeEntry(termId, entry);
    if (!d.badge && !d.osNotify) return; // suppressed entirely: never recorded
    const recorded = this.center.record({
      termId,
      ...this.centerAttribution(entry),
      kind,
      title,
      body,
    });
    if (d.osNotify) {
      if (this.windowFocused) this.banners.show(recorded);
      else void this.api.osNotify(title, body);
    }
  }

  /** Center attribution for one entry: process terminals carry their process
   *  name + project; project-bound shells their display label + project;
   *  plain shells attribute name only (label, no project). */
  private centerAttribution(entry: RailEntry): {
    projectId: number | null;
    processName: string | null;
  } {
    if (entry.kind === "process" && entry.process) {
      return { projectId: entry.process.projectId, processName: entry.process.name };
    }
    if (entry.kind === "project-shell" && entry.projectShell) {
      return { projectId: entry.projectShell.projectId, processName: this.entryLabel(entry) };
    }
    // An agent spawned into a project attributes to it.
    return { projectId: entry.agent?.projectId ?? null, processName: this.entryLabel(entry) };
  }

  /** Banner-card / drawer-row click: the v1 implicit action — focus that
   *  terminal (expand its project when it has one and it is still open, then
   *  activate — the activateActiveRow shape). Activation is also what clears
   *  the badge. A terminal that no longer exists just expands the
   *  project (the entry stays readable in the center). */
  private focusCenterEntry(entry: CenterEntry): void {
    this.center.markRead(entry);
    if (entry.projectId !== null && this.openProjects.has(entry.projectId)) {
      this.expandProject(entry.projectId);
    }
    if (this.entries.has(entry.termId)) this.activate(entry.termId);
  }

  /** The footer bell's unread chip, driven by the center subscription. */
  private updateBellBadge(): void {
    const n = this.center.unread;
    this.bellCount.textContent = n > 99 ? "99+" : String(n);
    this.bellCount.style.display = n > 0 ? "block" : "none";
  }

  private ensureDrawer(): NotifyDrawer {
    if (!this.drawer) {
      this.drawer = new NotifyDrawer({
        center: this.center,
        container: this.root,
        onFocus: (entry) => this.focusCenterEntry(entry),
        projectLabel: (id) => this.projectLabel(id),
        now: this.now,
        // The bell is the drawer's toggle: without the anchor exclusion its
        // press would dismiss on mousedown and re-open on the same click
        // (the ▾-menu trap).
        anchors: [this.bellBtn],
      });
    }
    return this.drawer;
  }

  private openDrawer(): void {
    this.ensureDrawer().open();
  }

  private toggleDrawer(): void {
    this.ensureDrawer().toggle();
  }

  /** The terminal's display name (rail row / process card label). A stored
   *  title can be the EMPTY string (`term://title` with "" is kept verbatim),
   *  and `??` passes it through — an empty/whitespace title must fall back to
   *  the name or toasts fire unattributable (review finding). */
  private entryLabel(entry: RailEntry): string {
    const title = entry.title?.trim();
    return title ? title : entry.name;
  }

  /** Badge one entry. Shells badge via the rail reducer + rail render;
   *  process terminals badge ONLY their process card — renderRail skips
   *  process entries entirely, so writing reducer state + rebuilding the rail
   *  for them was pure churn per badged event (review finding). */
  private badgeEntry(id: number, entry: RailEntry): void {
    if (entry.kind === "process") {
      this.markProcessCard(entry, "bell");
      return;
    }
    this.entries.set(id, railReducer(entry, { type: "bell" }));
    // A collapsed project's bound terminal aggregates onto its summary row
    //; the entry badge itself persists for the next expand.
    if (entry.kind === "project-shell" && entry.projectShell) {
      this.aggregateBadge(entry.projectShell.projectId, "bell");
    }
    this.renderRail();
  }

  /** Light a process card's bell/activity badge in place (no row rebuild —
   *  updateProcessRow's rule: rebuilding drops focus mid-interaction). A
   *  collapsed project's card DOM is detached, but the row STATE persists and
   *  repaints on expand; the event also aggregates onto the summary row. */
  private markProcessCard(entry: RailEntry, kind: "bell" | "activity"): void {
    if (entry.kind !== "process" || !entry.process) return;
    const row = this.rowFor(entry.process.projectId, entry.process.name);
    if (!row || row.termId !== entry.id) return;
    if (kind === "bell") row.bell = true;
    else row.activity = true;
    this.updateProcessBadges(row);
    this.aggregateBadge(entry.process.projectId, kind);
    // A badge on a process card is an ATTENTION change for its
    // ACTIVE row, and this path never reaches renderRail or
    // processRowChanged — hook here too (activeSig-guarded).
    this.renderActiveSection();
  }

  /**
   * The level in force for one terminal: per-process override → project
   * default → 'all'.
   *
   * PROJECT-BOUND SHELLS resolve their OWNING project: a bound terminal
   * takes ITS project's default (then 'all'), regardless of which project is
   * active — this replaced the active-project hack for these terminals. No
   * per-terminal override exists for them yet (inherit-only), so the
   * override tier is null; the command-row submenu pattern is where one
   * would attach.
   *
   * PLAIN WORKSPACE SHELLS have no project: they follow the EXPANDED
   * project's default when one is expanded, else 'all'.
   * Rationale: the picker reads "Notification level" for the whole window's
   * current context, and a shell opened while working in a project is part of
   * that context — a user who silences a project would otherwise still be
   * toasted by the shell sitting next to it.
   */
  private levelForEntry(entry: RailEntry): Level {
    if (entry.kind === "process" && entry.process) {
      const row = this.rowFor(entry.process.projectId, entry.process.name);
      return resolveLevel(row?.notificationLevel, this.projectDefaultLevel(entry.process.projectId));
    }
    if (entry.kind === "project-shell" && entry.projectShell) {
      return resolveLevel(null, this.projectDefaultLevel(entry.projectShell.projectId));
    }
    return resolveLevel(null, this.projectDefaultLevel(this.expandedProjectId));
  }

  /** The stored RAW project default for `id` (null when no project / unset). */
  private projectDefaultLevel(id: number | null): string | null {
    if (id === null) return null;
    return this.projects.get(id)?.notificationLevel ?? null;
  }

  /** The EXPANDED project's state, or null when none is expanded. */
  private expandedState(): ProjectState | null {
    return this.expandedProjectId === null
      ? null
      : (this.openProjects.get(this.expandedProjectId) ?? null);
  }

  /** One process row, addressed by (project id, name) — never by name alone
   *  (the same name can exist in two open projects). */
  private rowFor(projectId: number, name: string): ProcessRow | undefined {
    return this.openProjects.get(projectId)?.processRows.get(name);
  }

  private readonly onWindowFocus = (): void => {
    this.windowFocused = true;
  };

  private readonly onWindowBlur = (): void => {
    this.windowFocused = false;
  };

  private renderRail(): void {
    this.railList.textContent = "";
    // Only plain WORKSPACE shells get rail rows; project processes and
    // project-bound terminals live in the project section (their
    // panels are still in `entries` for activation/LRU). Adds the
    // third exclusion: an agent bound to the EXPANDED project renders under
    // that project's AGENTS subsection instead — the same predicate both
    // sides use, so a row can never render twice or vanish (an agent of an
    // open-but-collapsed, or never-opened, project stays here). // the workspace agents render as a FOREST (children nested under their
    // spawning parent), interleaved with the shells at their insertion
    // positions.
    let count = 0;
    for (const placed of this.orderedWorkspaceRows()) {
      count += 1;
      this.railList.appendChild(this.buildTerminalRow(placed.id, placed.entry, undefined, placed.depth, placed.lastChild));
    }
    this.railCount.textContent = String(count);
    // The project subsections are the rail's sibling surfaces: every state
    // change that repaints one repaints the others, so a bound terminal's or
    // agent's status / title / badge lands without duplicating the call sites.
    this.renderProjectTerminals();
    this.renderProjectAgents();
    // ACTIVE re-derives with the rail too: entry status, titles and
    // shell badges all route through here. activeSig makes a no-change free.
    this.renderActiveSection();
  }

  /**
   * The workspace rail's ordered rows — shells in entry order, with
   * the agent rows arranged as a forest (each agent's subtree rendered
   * contiguously under it, at the root's insertion position; non-root agents
   * ride their parent's subtree and are NOT emitted separately). Counts stay
   * FLAT: every row on this surface is produced exactly once.
   */
  private orderedWorkspaceRows(): Array<{
    id: number;
    entry: RailEntry;
    depth: number;
    lastChild: boolean;
  }> {
    const agents: ForestAgent<RailEntry>[] = [];
    const order: Array<{ id: number; entry: RailEntry; shell: boolean }> = [];
    for (const [id, entry] of this.entries) {
      if (entry.kind === "process" || entry.kind === "project-shell") continue;
      if (this.agentInExpandedSection(entry)) continue;
      if (entry.agent) {
        agents.push({ id, parentId: entry.agent.parentId, entry });
        order.push({ id, entry, shell: false });
      } else {
        order.push({ id, entry, shell: true });
      }
    }
    const forest = agentForest(agents);
    const forestIndex = new Map<number, number>();
    for (let i = 0; i < forest.length; i++) forestIndex.set(forest[i][0].id, i);
    const consumed = new Set<number>();
    const out: Array<{ id: number; entry: RailEntry; depth: number; lastChild: boolean }> = [];
    for (const item of order) {
      if (item.shell) {
        out.push({ id: item.id, entry: item.entry, depth: 0, lastChild: false });
        continue;
      }
      const start = forestIndex.get(item.id);
      if (start === undefined || consumed.has(item.id)) continue;
      consumed.add(item.id);
      const baseDepth = forest[start][1];
      // Emit the contiguous subtree rooted at `start` (the root + descendants).
      for (let j = start; j < forest.length; j++) {
        const [fa, depth] = forest[j];
        if (j !== start && depth <= baseDepth) break;
        consumed.add(fa.id);
        // A "last child" (the tree connector's vertical stops at its dot) is a
        // depth ≥ 1 row whose next forest row is shallower (or the end).
        const lastChild = depth >= 1 && (j + 1 >= forest.length || forest[j + 1][1] < depth);
        out.push({ id: fa.id, entry: fa.entry, depth, lastChild });
      }
    }
    return out;
  }

  /** An agent row that renders in the EXPANDED project's AGENTS
   *  subsection — `agent.projectId` names it and it is the one project
   *  carrying the section. The mirror image of the rail's exclusion above. */
  private agentInExpandedSection(entry: RailEntry): boolean {
    return (
      entry.kind === "agent" &&
      this.expandedProjectId !== null &&
      entry.agent?.projectId === this.expandedProjectId
    );
  }

  /**
   * One terminal row: status dot, name/title, activity + bell badges, exit
   * code and ×. Shared by the workspace rail, the project TERMINALS and the
   * project AGENTS subsections — the "same row anatomy as the rail,
   * shared code where it exists (do NOT fork renderRail; extract the row
   * builder if needed)".
   *
   * `extraClass` marks the subsection variant so tests and CSS can address
   * it; the anatomy itself is identical by construction.
   *
   * Nesting: `depth > 0` adds `chappa-rail-child` (the indent is a
   * CSS custom property `--chappa-sub-depth`, whose VALUE is capped at 3 — a
   * visual cap only) and `lastChild` adds `chappa-rail-child-last`, so the
   * CSS draws the dashed tree connector with the guide stopping at the last
   * child's dot. Everything else on the row is unchanged.
   */
  private buildTerminalRow(
    id: number,
    entry: RailEntry,
    extraClass?: string,
    depth = 0,
    lastChild = false,
  ): HTMLElement {
    const row = document.createElement("button");
    row.className = "chappa-rail-item";
    if (extraClass) row.classList.add(extraClass);
    row.classList.toggle("active", id === this.activeId);
    row.style.position = "relative";
    if (depth > 0) {
      row.classList.add("chappa-rail-child");
      if (lastChild) row.classList.add("chappa-rail-child-last");
      // The INDENT value is the visual cap (3); the model depth is unbounded.
      row.style.setProperty("--chappa-sub-depth", String(Math.min(depth, 3)));
    }
    row.title = entry.title ?? entry.name;
    row.addEventListener("click", () => this.activate(id));
    row.addEventListener("auxclick", (e) => {
      if (e.button === 1) void this.closePanel(id);
    });

    const dot = document.createElement("span");
    dot.className = `chappa-status-dot ${entry.status}`;
    dot.title = entry.status;
    const name = document.createElement("span");
    name.className = "chappa-rail-name";
    name.textContent = entry.title ?? entry.name;
    // The `N%` readout. The span is ALWAYS in the row at a fixed width,
    // empty when there is nothing to show: stats appearing and disappearing
    // must not jitter the row or
    // re-truncate the
    // label, and the only way to guarantee that is to reserve the space rather
    // than add and remove an element. Running terminals only — a stopped
    // shell's last reading is a stale number pretending to be live.
    const cpu = document.createElement("span");
    cpu.className = "chappa-rail-cpu";
    const stats = entry.status === "running" ? this.stats.get(id) : null;
    const chip = stats ? statsChip(stats) : "";
    cpu.textContent = chip;
    if (chip) cpu.title = statsTooltip(stats!);
    row.append(dot, name);
    // Agent rows: a muted model tag after the name, a container
    // state dot for docker-exec runtimes (tooltip: status + uptime +
    // bridge), and the attention ring on a bridge fault.
    if (entry.agent) {
      row.classList.add("agent");
      if (entry.agent.model) {
        const model = document.createElement("span");
        model.className = "chappa-rail-model";
        model.textContent = entry.agent.model;
        model.title = `model ${entry.agent.model}`;
        row.append(model);
      }
      if (entry.agent.runtime.kind === "docker_exec") {
        const container = document.createElement("span");
        const status = entry.agent.container?.status ?? "unknown";
        container.className = `chappa-rail-container ${status}`;
        container.title = containerTooltip(entry.agent);
        row.append(container);
      }
      if (bridgeFault(entry.agent)) {
        row.classList.add("attention");
        row.dataset.bridge = entry.agent.bridge;
      }
      // The transport tag and the typed agent-waiting ring.
      if (entry.agent.transport === "json") {
        const transport = document.createElement("span");
        transport.className = "chappa-rail-transport";
        transport.textContent = "json";
        transport.title = "structured transport (machine-mode JSON, transcript view)";
        row.append(transport);
      }
    }
    // The typed attention tier (agent-waiting): read off the entry,
    // never recomputed here.
    if (entry.attention !== null) {
      row.classList.add("attention");
      row.dataset.attention = entry.attention;
      if (entry.attention === "agent-waiting") row.title = `${row.title} — waiting for your input`;
    }
    row.append(cpu);

    if (entry.activity) {
      const activity = document.createElement("span");
      activity.className = "chappa-rail-activity";
      activity.title = "produced output while hidden";
      row.append(activity);
    }
    if (entry.bell) {
      const bell = document.createElement("span");
      bell.className = "chappa-rail-bell";
      bell.append(bellIcon());
      bell.title = "bell rang while hidden";
      row.append(bell);
    }
    if (entry.status === "exited" || entry.status === "failed") {
      const code = document.createElement("span");
      code.className = "chappa-exit-code";
      code.textContent = entry.exitCode === null ? "?" : String(entry.exitCode);
      code.title = `exit code ${code.textContent}`;
      row.append(code);
    }

    const close = document.createElement("button");
    close.className = "chappa-rail-close";
    close.textContent = "×";
    close.title = "Close";
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.closePanel(id);
    });
    row.append(close);
    return row;
  }

  // --- settings pane + new-terminal menu --------------------------

  /** Gear click: open/close the full-window settings view. The pane is built
   *  lazily and kept (its subscription is what re-syncs controls after a Rust
   *  clamp). The pane has no outside-click dismiss, so the gear
   *  no longer needs registering as its anchor. */
  toggleSettings(): void {
    if (!this.settingsPane) {
      this.settingsPane = new SettingsPane({
        store: this.settings,
        agentTools: this.agentTools,
        onClose: () => {
          this.gear.classList.remove("open");
          if (this.activeId !== null) this.entries.get(this.activeId)?.panel.focusInput();
        },
      });
    }
    this.settingsPane.toggleOpen();
    this.gear.classList.toggle("open", this.settingsPane.isOpen());
  }

  /** The settings overlay, once opened (tests drive its controls). */
  get settingsPaneElement(): HTMLElement | null {
    return this.settingsPane?.element ?? null;
  }

  private toggleNewTermMenu(): void {
    if (this.newTermMenu.style.display === "none") {
      this.renderNewTermMenu();
      this.newTermMenu.style.display = "block";
    } else {
      this.hideNewTermMenu();
    }
  }

  private hideNewTermMenu(): void {
    this.newTermMenu.style.display = "none";
  }

  /** The profile list: "Default shell" plus every ENABLED shell profile.
   *  A disabled profile is absent — that is what the enable toggle means.
   *
   *  Shared with the project TERMINALS subsection's ▾ ("the profile
   *  ▾ if the rail's menu reuses cheaply"). The class names are per-host so
   *  each surface's selectors keep matching only its OWN menu; the option set
   *  and the spawn semantics are one implementation. */
  private fillProfileMenu(
    menu: HTMLElement,
    optionClass: string,
    emptyClass: string,
    pick: (profile?: string) => void,
  ): void {
    menu.textContent = "";
    const option = (label: string, profile?: string): void => {
      const btn = document.createElement("button");
      btn.className = optionClass;
      btn.textContent = label;
      if (profile) btn.dataset.profile = profile;
      btn.addEventListener("mousedown", (e) => e.stopPropagation());
      btn.addEventListener("click", () => pick(profile));
      menu.appendChild(btn);
    };
    option("Default shell");
    const enabled = this.settings.get().shellProfiles.filter((p) => p.enabled);
    for (const p of enabled) option(p.name, p.name);
    if (enabled.length === 0) {
      const empty = document.createElement("div");
      empty.className = emptyClass;
      empty.textContent = "No enabled shell profiles";
      menu.appendChild(empty);
    }
  }

  private renderNewTermMenu(): void {
    this.fillProfileMenu(this.newTermMenu, "chappa-new-term-option", "chappa-new-term-empty", (profile) => {
      this.hideNewTermMenu();
      void this.newShell(undefined, profile);
    });
  }

  // --- agents (shares the spawner) -------------------------

  /**
   * Build a "New agent ▸" spawner: the button plus its dropdown of the
   * ENABLED agent tools. ONE builder for both surfaces (the workspace
   * TERMINALS header and the project AGENTS section) — the affordance
   * is the same widget, not a copy: same classes, same option anatomy, same
   * dismiss discipline. `pick` is the only difference (the workspace spawner
   * keeps the binding, the section one names the project explicitly);
   * `onOpen` runs when the menu opens (the workspace spawner also closes the
   * terminal menu — the behaviour).
   */
  private buildAgentSpawner(pick: (toolId: number) => void, onOpen?: () => void): AgentSpawner {
    const btn = document.createElement("button");
    btn.className = "chappa-new-agent";
    btn.textContent = "New agent ▸";
    btn.title = "Spawn an agent from a registered tool";
    btn.addEventListener("mousedown", (e) => e.preventDefault());
    const menu = document.createElement("div");
    menu.className = "chappa-new-agent-menu";
    menu.style.display = "none";
    const hide = (): void => {
      menu.style.display = "none";
    };
    const spawner: AgentSpawner = { btn, menu, hide };
    btn.addEventListener("click", () => {
      if (menu.style.display === "none") {
        // One "New agent ▸" menu at a time: opening one closes the other
        // surface's (the workspace and project spawners are siblings in the
        // rail), the terminal-menu behaviour for the workspace one.
        for (const s of this.agentSpawners) if (s !== spawner) s.hide();
        onOpen?.();
        this.fillAgentMenu(menu, pick);
        menu.style.display = "block";
        // The store loads lazily (boot never loads it), so a cold session —
        // or a tool added in the settings pane of a PREVIOUS session — showed
        // "No enabled agent tools" until settings happened to load the list
        // (2026-08-31). Refresh on open and refill if still showing;
        // the sync fill above keeps the menu instant when the cache is warm.
        void this.agentTools.load().then(() => {
          if (menu.style.display !== "none") this.fillAgentMenu(menu, pick);
        });
      } else {
        hide();
      }
    });
    this.agentSpawners.push(spawner);
    // Same capture-phase dismiss as the other rail menus (shared
    // helper). Preserved drift: the button is anchor-excluded; no Escape.
    this.dismissUnbinds.push(
      registerDismiss({
        isOpen: () => menu.style.display !== "none",
        dismiss: hide,
        inside: [menu, btn],
      }),
    );
    return spawner;
  }

  /** Hide every "New agent ▸" menu (a pick closes the menus before it spawns). */
  private hideAgentMenus(): void {
    for (const s of this.agentSpawners) s.hide();
  }

  /** ENABLED tools only — a disabled tool is absent, exactly like a disabled
   *  shell profile. */
  private fillAgentMenu(menu: HTMLElement, pick: (toolId: number) => void): void {
    menu.textContent = "";
    const tools = this.agentTools.enabled();
    for (const tool of tools) {
      const btn = document.createElement("button");
      btn.className = "chappa-new-agent-option";
      btn.dataset.toolId = String(tool.id);
      btn.textContent = tool.model ? `${tool.name} · ${tool.model}` : tool.name;
      btn.title = `${tool.tool_type} · ${tool.runtime.kind === "docker_exec" ? tool.runtime.container : "host"}`;
      btn.addEventListener("mousedown", (e) => e.stopPropagation());
      btn.addEventListener("click", () => {
        this.hideAgentMenus();
        pick(tool.id);
      });
      menu.appendChild(btn);
    }
    if (tools.length === 0) {
      const empty = document.createElement("div");
      empty.className = "chappa-new-agent-empty";
      empty.textContent = "No enabled agent tools — add one in Settings";
      menu.appendChild(empty);
    }
  }

  /**
   * Spawn an agent from tool `toolId` into `projectId` — or, when omitted,
   * the expanded project (cwd + the identity env's CHAPPA_AI_PROJECT_ID) —
   * with an EMPTY prompt. Same panel path as a shell; the `spawn` seam (the
   * project-process precedent) calls `spawn_agent` with the frames channel
   * and keeps the response so the entry carries the agent block from the
   * first render. The project AGENTS section's spawner names the
   * project explicitly; the workspace spawner omits it (the
   * fallback chain, unchanged).
   */
  async spawnAgent(toolId: number, projectId?: number): Promise<ipc.SpawnAgentResponseDto | null> {
    const tool = this.agentTools.byId(toolId);
    if (!tool) return null;
    const project = projectId ?? this.expandedProjectId ?? undefined;
    const host = document.createElement("div");
    host.className = "chappa-panel-host";
    this.stack.appendChild(host);
    let response: ipc.SpawnAgentResponseDto | null = null;
    // The entry's agent block from the spawn response — shared by both
    // panel kinds so the rail row is right from the first render.
    const adopt = (id: number, panel: PanelHost): void => {
      const resp = response!;
      this.onCreated(id, panel, host, resp.name);
      const entry = this.entries.get(id);
      if (!entry) return;
      this.entries.set(id, {
        ...entry,
        kind: "agent",
        agent: {
          toolId: resp.agent.tool_id,
          toolType: resp.agent.tool_type,
          model: resp.agent.model,
          runtime: resp.agent.runtime,
          projectId: resp.agent.project_id ?? project ?? null,
          container: resp.container ?? resp.agent.container,
          bridge: resp.agent.bridge,
          transport: resp.agent.transport ?? "tty",
          // User-spawned agents are roots (the Tauri command route
          // passes no actor, so the spawn records `null`); a backend-spawned
          // child nests under its recorded parent.
          parentId: resp.agent.parent_process_id ?? null,
        },
      });
      this.renderRail();
      // The spawn response's typed waiting fact goes through the SAME path
      // as a live `awaiting_input` event (ring + notify decision) — an
      // agent that was already waiting when we adopted it badges too.
      if (resp.agent.awaiting_input) this.markAgentWaiting(id);
      // Events that raced the adopt (the Rust side emits from the first
      // line, before the spawn response resolves) replay through the normal
      // path now that the entry exists.
      const pending = this.pendingAgentEvents.get(id);
      this.pendingAgentEvents.delete(id);
      for (const e of pending ?? []) this.applyAgentEvent(e);
    };
    // A json-transport tool gets the transcript view, not a
    // terminal — same host, same activation/focus discipline, no frames.
    const panel: PanelHost =
      tool.transport === "json"
        ? new TranscriptPanel({
            container: host,
            api: this.api,
            active: false,
            spawn: async () => {
              const frames = ipc.makeProjectFramesChannel(() => {});
              response = await this.api.spawnAgent({ agent_tool_id: toolId, project_id: project }, frames);
              return response.term_id;
            },
            onCreated: (id) => adopt(id, panel),
            onSent: (id) => this.setAgentWaiting(id, false),
          })
        : new TerminalPanel({
            container: host,
            api: this.api,
            settings: this.settings,
            active: false,
            spawn: async (onFrame, dims) => {
              const frames = ipc.makeProjectFramesChannel((data) => {
                onFrame(data instanceof ArrayBuffer ? data : Uint8Array.from(data).buffer);
              });
              response = await this.api.spawnAgent(
                { agent_tool_id: toolId, project_id: project, cols: dims?.cols, rows: dims?.rows },
                frames,
              );
              return response.term_id;
            },
            onCreated: (id) => adopt(id, panel),
          });
    try {
      await panel.start();
    } catch (err) {
      // Refused (busy guard names the processes), disabled, or the spawn
      // itself failed: surface the reason, drop the empty host.
      console.error(`[chappa-ai] failed to spawn agent: ${err}`);
      host.remove();
      void this.showError(String(err instanceof Error ? err.message : err));
      return null;
    }
    return response;
  }

  /**
   * `agent://event` — one typed event from a json agent. The
   * transcript panel renders it; two kinds also touch the rail:
   *  - `awaiting_input` sets the `agent-waiting` attention (rail ring, ACTIVE
   *    top-sort) AND runs through the SAME notify decision as an OSC 9 —
   *    badge / banner / OS toast / center per the terminal's level, no new
   *    decision path;
   *  - `turn_started` clears it (an MCP send whose receipt we never saw
   *    still clears the ring when the CLI acknowledges).
   */
  private applyAgentEvent(p: ipc.AgentEventEvent): void {
    const id = p.id ?? p.term_id;
    const entry = this.entries.get(id);
    if (!entry) {
      // Not adopted yet (the spawn response is still in flight): keep it,
      // bounded, and replay it through this same path after adopt.
      this.bufferPreAdoptEvent(id, p);
      return;
    }
    if (entry.panel instanceof TranscriptPanel) entry.panel.apply(p);
    if (!entry.agent) return;
    if (p.kind === "awaiting_input") {
      this.markAgentWaiting(id);
    } else if (p.kind === "turn_started") {
      this.setAgentWaiting(id, false);
    }
  }

  /** Hold an `agent://event` for an id with no entry yet. Small and
   *  bounded: a few ids, a ring per id (the oldest drop first — the Rust
   *  ring's `get_agent_events` replay covers the transcript anyway; what
   *  must not be lost is the LAST waiting fact). */
  private bufferPreAdoptEvent(id: number, p: ipc.AgentEventEvent): void {
    let list = this.pendingAgentEvents.get(id);
    if (!list) {
      if (this.pendingAgentEvents.size >= PRE_ADOPT_EVENT_IDS_MAX) {
        const oldest = this.pendingAgentEvents.keys().next().value;
        if (oldest !== undefined) this.pendingAgentEvents.delete(oldest);
      }
      list = [];
      this.pendingAgentEvents.set(id, list);
    }
    list.push(p);
    if (list.length > PRE_ADOPT_EVENTS_PER_ID) list.splice(0, list.length - PRE_ADOPT_EVENTS_PER_ID);
  }

  /** The agent said it wants input: the `agent-waiting` tier AND the same
   *  notify decision as an OSC 9 — badge / banner / OS toast / center per
   *  the terminal's level, no new decision path. Idempotent while already
   *  waiting (a replayed + live duplicate notifies once). */
  private markAgentWaiting(id: number): void {
    if (!this.setAgentWaiting(id, true)) return;
    const fresh = this.entries.get(id);
    if (!fresh) return;
    const body = "waiting for your input";
    const d = decide("notify", this.levelForEntry(fresh), id === this.activeId, this.windowFocused, body);
    this.routeDecision(id, fresh, "notify", d, this.entryLabel(fresh), body);
  }

  /** Flip the typed waiting tier on an agent entry through the reducer
   *  (rail + ACTIVE re-derive). Returns whether anything changed — false
   *  for a non-agent, an unchanged tier, or a dead entry (never waiting). */
  private setAgentWaiting(id: number, waiting: boolean): boolean {
    const entry = this.entries.get(id);
    if (!entry?.agent) return false;
    const next = railReducer(entry, { type: "attention", attention: waiting ? "agent-waiting" : null });
    if (next.attention === entry.attention) return false;
    this.entries.set(id, next);
    this.renderRail();
    return true;
  }

  /** `agent://bridge` — one transition of the docker-exec probe. The row
   *  gains/loses the attention ring; the container dot + tooltip refresh. */
  private applyBridge(p: ipc.AgentBridgeEvent): void {
    const entry = this.entries.get(p.id ?? p.term_id);
    if (!entry?.agent) return;
    this.entries.set(entry.id, {
      ...entry,
      agent: { ...entry.agent, bridge: p.bridge, container: p.container ?? entry.agent.container },
    });
    this.renderRail();
  }

  /** `agent://reap` — a backend sweep reaped N orphaned marker
   *  processes in a container. Backend-originated, not bound to any terminal:
   *  recorded straight into the notification center (ONE entry per
   *  container), which bumps the bell unread. */
  private applyReap(p: ipc.AgentReapEvent): void {
    const title = "Orphan reaper";
    const body = `Reaped ${p.count} orphaned agent processes in ${p.container}`;
    this.center.record({
      termId: 0,
      projectId: null,
      processName: null,
      kind: "notify",
      title,
      body,
    });
  }

  /** A transient bottom-left toast (the close verification when it
   *  is not `gone`). Auto-dismisses; click dismisses. */
  private showToast(text: string, kind = "info"): void {
    const el = document.createElement("div");
    el.className = "chappa-toast";
    el.dataset.kind = kind;
    el.textContent = text;
    el.addEventListener("click", () => el.remove());
    this.toasts.appendChild(el);
    setTimeout(() => el.remove(), 8000);
  }

  // --- project view ----------------------------------------------

  /** A small header action button (S/A/P). */
  private headerButton(label: string, title: string): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "chappa-project-action";
    btn.textContent = label;
    btn.title = title;
    return btn;
  }

  /** One process-row lifecycle button. */
  private processButton(
    label: string,
    title: string,
    onActivate: () => void,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "chappa-process-btn";
    btn.textContent = label;
    btn.title = title;
    btn.addEventListener("click", onActivate);
    return btn;
  }

  /** Hydrate the switcher dropdown from the stored projects. */
  private async loadProjects(): Promise<void> {
    try {
      const projects = await this.api.listProjects();
      this.projects.clear();
      for (const p of projects) this.projects.set(p.id, p);
    } catch {
      // Not in Tauri, or the store isn't initialized: an empty switcher.
    }
    this.renderProjectMenu();
  }

  private renderProjectMenu(): void {
    this.projectMenu.textContent = "";
    for (const p of this.projects.values()) {
      // The row is a flex pair — the open button plus a ✎ rename.
      // (Nesting the ✎ inside the option button would be invalid HTML and its
      // click would open the project on the way out.)
      const row = document.createElement("div");
      row.className = "chappa-project-row";
      const opt = document.createElement("button");
      opt.className = "chappa-project-option";
      opt.dataset.id = String(p.id);
      opt.textContent = p.name;
      opt.title = p.path;
      opt.addEventListener("mousedown", (e) => e.stopPropagation());
      opt.addEventListener("click", () => {
        this.hideProjectMenu();
        void this.openProject(p.id);
      });
      const rename = document.createElement("button");
      rename.className = "chappa-project-rename";
      rename.dataset.id = String(p.id);
      rename.textContent = "✎";
      rename.title = `Rename "${p.name}"`;
      rename.addEventListener("mousedown", (e) => e.stopPropagation());
      rename.addEventListener("click", (e) => {
        // The row is not a button, but the click still reaches the menu's own
        // handlers; stop it so ✎ never doubles as "open this project".
        e.stopPropagation();
        this.hideProjectMenu();
        void this.renameProjectFlow(p.id);
      });
      // Remove (2026-08-31: "no way to remove projects" — the Rust
      // command predates this affordance). Every row offers it; the flow
      // always confirms, so a stray click destroys nothing.
      const remove = document.createElement("button");
      remove.className = "chappa-project-remove";
      remove.dataset.id = String(p.id);
      remove.append(trashIcon());
      remove.title = `Remove "${p.name}" from chappa-ai (files stay on disk)`;
      remove.addEventListener("mousedown", (e) => e.stopPropagation());
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        this.hideProjectMenu();
        void this.removeProjectFlow(p.id);
      });
      row.append(opt, rename, remove);
      // An OPEN project's row also offers "Close project" — the
      // EXPLICIT close (stops processes, disposes panels). A closed project
      // has nothing to close, so no ×.
      if (this.openProjects.has(p.id)) {
        const closeBtn = document.createElement("button");
        closeBtn.className = "chappa-project-close";
        closeBtn.dataset.id = String(p.id);
        closeBtn.textContent = "×";
        closeBtn.title = `Close project "${p.name}" (stops its processes)`;
        closeBtn.addEventListener("mousedown", (e) => e.stopPropagation());
        closeBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          this.hideProjectMenu();
          void this.closeProjectFlow(p.id);
        });
        row.append(closeBtn);
      }
      this.projectMenu.appendChild(row);
    }
    const add = document.createElement("button");
    add.className = "chappa-project-add";
    add.textContent = "＋ Add project…";
    add.addEventListener("mousedown", (e) => e.stopPropagation());
    add.addEventListener("click", () => {
      this.hideProjectMenu();
      void this.addProjectFlow();
    });
    this.projectMenu.appendChild(add);
  }

  private toggleProjectMenu(): void {
    if (this.projectMenu.style.display === "none") {
      this.renderProjectMenu();
      this.projectMenu.style.display = "block";
    } else {
      this.hideProjectMenu();
    }
  }

  private hideProjectMenu(): void {
    this.projectMenu.style.display = "none";
  }

  /** The "Add project…" affordance: the path+name modal, then
   *  register and open. `name` is the user's explicit (edited) choice, or
   *  null when the field was never touched — the modal's prefill is only a
   *  preview, and null lets Rust apply ITS default (a project file `name:`,
   *  then the folder basename), which the preview cannot know about. */
  private async addProjectFlow(): Promise<void> {
    // The picker starts at the remembered directory (the parent of the last
    // added project). Outside Tauri there is no picker at all.
    const browse = this.canBrowseDirectories
      ? () => this.api.pickDirectory("Choose a project directory", readLastProjectDir())
      : null;
    const answer = await this.ask("addProject", () => this.promptAddProject(browse));
    if (!answer) return;
    const { path, name } = answer;
    if (!path) return;
    try {
      await this.registerAndOpen(path, name);
    } catch (err) {
      // 2026-08-31 ("adding a project failed (nothing happened)"): this
      // used to be a console.error and nothing else. Surface the reason.
      await this.showError(`Could not add project: ${err}`);
    }
  }

  /** The add flow's success path: register with Rust, adopt the answered row,
   *  remember the directory for the next Browse…, open. */
  private async registerAndOpen(
    path: string,
    name: string | null | undefined,
  ): Promise<void> {
    const p = await this.api.addProject(path, name);
    this.projects.set(p.id, p);
    rememberProjectDir(path);
    this.renderProjectMenu();
    await this.openProject(p.id);
  }

  /** The switcher's per-row ✎: rename a STORED project. Store-only —
   *  chappa.yml is never touched, and the open project's header follows. Shares
   *  its validation/application with the Edit pane (renameProjectApply). */
  private async renameProjectFlow(id: number): Promise<void> {
    const current = this.projects.get(id);
    if (!current) return;
    const typed = await this.ask(`rename:${id}`, () => this.promptRename(current.name));
    // null = cancelled, or a second ✎ on the same row while the first is up.
    if (typed === null) return;
    await this.renameProjectApply(id, typed);
  }

  /** Open a stored project: fresh chappa.yml → process rows (status stopped,
   *  nothing spawned) → trust gate (Run starts the auto-start commands).
   *
   *  "opening a project no longer closes the previous one" — the
   *  previously expanded project collapses to a summary row and keeps
   *  running. An ALREADY-open id just expands, WITHOUT reaching the Rust
   *  command: `open_project` is idempotent Rust-side by RESETTING the runtime
   *  (it closes the project's live terminals and rebuilds process state from
   *  a fresh yml read), which would kill exactly the background processes a
   *  switch must keep. */
  async openProject(id: number): Promise<void> {
    if (this.openProjects.has(id)) {
      this.expandProject(id);
      return;
    }
    // In-flight guard: the check above runs BEFORE the first await, so a
    // second overlapping open of the same id would also reach the Rust
    // command — and open_project's reset-idempotency would tear down the
    // first call's runtime. Second caller returns; the live open owns it.
    if (this.pendingProjectOpens.has(id)) return;
    this.pendingProjectOpens.add(id);
    try {
      await this.openProjectInner(id);
    } finally {
      this.pendingProjectOpens.delete(id);
    }
  }

  /** The body of {@link openProject}, guarded by `pendingProjectOpens`. */
  private async openProjectInner(id: number): Promise<void> {
    let result: ipc.OpenProjectResult;
    try {
      result = await this.api.openProject(id);
    } catch (err) {
      console.error(`[chappa-ai] failed to open project ${id}: ${err}`);
      return;
    }
    const state: ProjectState = {
      id,
      processRows: new Map(),
      ymlError: null,
      reloadSeq: 0,
      bell: false,
      activity: false,
      summaryEl: null,
      summaryPill: null,
      summaryBellEl: null,
      summaryActivityEl: null,
    };
    this.openProjects.set(id, state);
    // Adopt the open answer's notification default AND sync flag onto the
    // stored row (the switcher list came from list_projects, which may have
    // predated a change).
    const stored = this.projects.get(id);
    this.projects.set(
      id,
      stored
        ? {
            ...stored,
            notificationLevel: result.project.notificationLevel,
          }
        : { ...result.project, id },
    );
    for (const p of result.processes) {
      state.processRows.set(p.name, this.rowFromDto(id, p));
    }
    // The fresh project expands; the previously expanded one (if any)
    // collapses to a summary row — its processes, panels and terminals are
    // untouched.
    this.expandProject(id);
    // Trust gate: an untrusted chappa.yml
    // hash arms one Run/Skip confirm listing the auto-start commands; Run
    // records the hash and proceeds. A trusted hash skips straight to
    // auto-start.
    if (result.trustPending && result.trustCommands.length === 0) {
      // Nothing to run means nothing to confirm — a dialog asking to "run
      // these auto-start commands" over an EMPTY list is noise (seen when
      // adding a project with a bare chappa.yml). Record the hash as trusted so
      // the gate doesn't re-arm on every open; there are no commands to start.
      try {
        await this.api.confirmProjectTrust(id, true);
      } catch {
        /* transient — the gate re-arms on the next open */
      }
      return;
    }
    if (result.trustPending) {
      const run = await this.ask(`trust:${id}`, () => this.confirmTrust(result.trustCommands));
      // null = the same project's gate is already on screen; the live dialog
      // owns the decision, so this open must not answer it a second time.
      if (run === null) return;
      try {
        await this.api.confirmProjectTrust(id, run);
      } catch {
        // Transient IPC failure: the gate re-arms on the next open.
      }
      // The gate's Run targets the project THIS open answered for, even if
      // another project was expanded while the dialog sat on screen.
      if (run) void this.startAuto(id);
    } else {
      void this.startAuto(id);
    }
  }

  /** A project-bound shell or an agent spawned into the project
   *: both close with the project and are listed in its
   *  close confirm. Process entries have their own lifecycle. */
  private boundToProject(entry: RailEntry, projectId: number): boolean {
    if (entry.kind === "project-shell") return entry.projectShell?.projectId === projectId;
    if (entry.kind === "agent") return entry.agent?.projectId === projectId;
    return false;
  }

  /** Stop one open project's running processes, dispose its panels + bound
   *  terminals, and forget its state (the body of the EXPLICIT
   *  "Close project" — a mere switch never runs this). */
  private closeProjectState(id: number): void {
    const state = this.openProjects.get(id);
    if (!state) return;
    for (const row of state.processRows.values()) {
      if (row.status === "starting" || row.status === "running") {
        void this.api.stopProjectProcess(row.projectId, row.name).catch(() => {});
      }
      this.disposeProcessPanel(row);
    }
    state.processRows.clear();
    // "Project terminals of a closed project close too (normal close flow)".
    // forceClosePanel is the dispose → close_terminal path, no per-terminal
    // confirm: the project-level confirm already spoke for everything.
    for (const [termId, entry] of [...this.entries]) {
      if (this.boundToProject(entry, id)) this.forceClosePanel(termId);
    }
    if (this.emptyFor?.projectId === id) this.hideProcessEmptyState();
    state.summaryEl?.remove();
    this.openProjects.delete(id);
    if (this.expandedProjectId === id) {
      this.expandedProjectId = null;
      this.switcher.textContent = "Project…";
      this.renderProject();
    }
    this.renderProjectSummaries();
    // Closing a BACKGROUND project (rows dropped above, no bound
    // terminals to route through forceClosePanel → renderRail) otherwise
    // leaves its rows in ACTIVE until the next unrelated repaint.
    this.renderActiveSection();
  }

  /** Start one process: build a fresh panel bound to its spawn, then start it
   *  through start_project_process. Idempotent on a live process. */
  private async startProcess(projectId: number, name: string): Promise<void> {
    const row = this.rowFor(projectId, name);
    if (!row) return;
    if (row.status === "starting" || row.status === "running") return;
    // Non-reentrant: flip to "starting" BEFORE the first await so a concurrent
    // second caller (double-click, or startAuto racing a row click) hits the
    // guard above instead of disposing the in-flight panel and double-spawning.
    row.status = "starting";
    this.updateProcessRow(row);
    // A dead process (exited/failed) keeps its panel for scrollback, but that
    // panel's terminal id and frames channel are consumed — panel.start() is
    // idempotent and would hand back the dead id without ever reaching Rust
    // (regression: manual re-start stuck on "starting"). A re-start always
    // begins from a fresh panel bound to a fresh spawn, matching the Rust
    // side, which forces a fresh term id on every user start.
    if (row.panel) this.disposeProcessPanel(row);
    if (!row.panel) {
      const host = document.createElement("div");
      host.className = "chappa-panel-host";
      this.stack.appendChild(host);
      const panel = new TerminalPanel({
        container: host,
        api: this.api,
        settings: this.settings,
        active: false,
        spawn: (onFrame) => this.spawnProcessChannel(row.projectId, name, onFrame),
      });
      row.panel = panel;
      row.host = host;
    }
    const panel = row.panel;
    try {
      const termId = await panel.start();
      this.registerProcessPanel(row, termId);
    } catch (err) {
      // Spawn failed (bad command, working_dir escape, …): surface and reset.
      // The optimistic "starting" above must not stick on a failed spawn.
      console.error(`[chappa-ai] failed to start process ${name}: ${err}`);
      this.disposeProcessPanel(row);
      row.status = "failed";
      this.processRowChanged(row);
    }
  }

  /** Focus a command's pane: its panel when one exists (running OR exited-
   *  with-scrollback), else the stopped-command empty state
   *  ("Process is stopped / Start it to run this process. / ▶ Start"). */
  private activateProcess(projectId: number, name: string): void {
    const row = this.rowFor(projectId, name);
    if (!row) return;
    if (row.termId !== null && this.entries.has(row.termId)) {
      this.activate(row.termId);
    } else {
      this.showProcessEmptyState(projectId, name);
    }
  }

  /** Front the stopped-command pane for one command: every real panel
   *  deactivates (the stack must not keep showing the previous command) and
   *  the card takes the active marker. */
  private showProcessEmptyState(projectId: number, name: string): void {
    const row = this.rowFor(projectId, name);
    if (!row) return;
    this.activeId = null;
    for (const entry of this.entries.values()) {
      entry.panel.setActive(false);
      entry.host.classList.remove("active");
    }
    if (!this.emptyHost) {
      const host = document.createElement("div");
      host.className = "chappa-panel-host chappa-process-empty";
      this.emptyTitle = document.createElement("div");
      this.emptyTitle.className = "chappa-process-empty-title";
      const hint = document.createElement("div");
      hint.className = "chappa-process-empty-hint";
      hint.textContent = "Start it to run this process.";
      const btn = document.createElement("button");
      btn.className = "chappa-process-empty-start";
      btn.textContent = "▶ Start";
      btn.addEventListener("mousedown", (e) => e.preventDefault());
      btn.addEventListener("click", () => {
        const t = this.emptyFor;
        if (t) void this.startProcess(t.projectId, t.name);
      });
      host.append(this.emptyTitle, hint, btn);
      this.stack.appendChild(host);
      this.emptyHost = host;
    }
    this.emptyTitle!.textContent = "Process is stopped";
    this.emptyFor = { projectId, name };
    this.emptyHost.classList.add("active");
    for (const s of this.openProjects.values()) {
      for (const r of s.processRows.values()) {
        r.el?.classList.toggle("active", r === row);
      }
    }
    this.renderRail();
  }

  private hideProcessEmptyState(): void {
    this.emptyFor = null;
    this.emptyHost?.classList.remove("active");
  }

  /** Stop one process (idempotent: a non-running process stays put — the
   *  stopped command keeps no pane) and dispose its panel. */
  private async stopProcess(projectId: number, name: string): Promise<void> {
    const row = this.rowFor(projectId, name);
    if (!row) return;
    try {
      await this.api.stopProjectProcess(projectId, name);
    } catch {
      // Not in Tauri / transient — the process row still reflects the event.
    }
    row.status = "stopped";
    row.exitCode = null;
    // Stopping the command the stack is showing lands on ITS stopped pane,
    // not on whatever panel the dispose fallback would activate.
    const wasFocused = row.termId !== null && this.activeId === row.termId;
    this.disposeProcessPanel(row);
    if (wasFocused) this.showProcessEmptyState(projectId, name);
    this.processRowChanged(row);
  }

  /** Restart = stop if live, then start (a stopped process just starts). */
  private async restartProcess(projectId: number, name: string): Promise<void> {
    await this.stopProcess(projectId, name);
    await this.startProcess(projectId, name);
  }

  // --- workspace commands -----------------------------------------

  /** Fetch the stored workspace commands and rebuild the COMMANDS rows (blank
   *  start: mount spawns nothing — this only seeds the command list). */
  async loadWorkspaceCommands(): Promise<void> {
    let rows: Map<string, WorkspaceCommandRow> = new Map();
    try {
      const list = await this.api.listWorkspaceCommands();
      rows = new Map(list.map((dto) => [dto.name, wsRowFromDto(dto)]));
    } catch {
      // Not in Tauri / transient — the COMMANDS section stays empty.
    }
    this.wsCmds.clear();
    for (const [name, row] of rows) this.wsCmds.set(name, row);
    this.wsCmdsSig = "\u0000never-rendered";
    this.renderWorkspaceCmds();
  }

  /** The `up-all` seed: `auto_start` commands start at launch, after settings
   *  and the command list load — each with its own frames channel (the
   *  webview-only artifact Rust cannot fabricate). */
  private async startAutoStartWorkspace(): Promise<void> {
    for (const row of this.wsCmds.values()) {
      if (row.autoStart) await this.startWorkspace(row.name);
    }
  }

  /** The frames-channel spawn a workspace command panel uses. */
  private spawnWorkspaceChannel(
    name: string,
    onFrame: (buf: ArrayBuffer) => void,
  ): Promise<number> {
    const frames = ipc.makeWorkspaceFramesChannel((data) => {
      onFrame(data instanceof ArrayBuffer ? data : Uint8Array.from(data).buffer);
    });
    return this.api.startWorkspaceCommand(name, frames);
  }

  /** Start one workspace command: build a fresh panel bound to its spawn and
   *  start it through startWorkspaceCommand. Idempotent on a live command. */
  async startWorkspace(name: string): Promise<void> {
    const row = this.wsCmds.get(name);
    if (!row) return;
    if (row.status === "starting" || row.status === "running") return;
    // Non-reentrant: flip to "starting" before the first await (the
    // startProcess precedent).
    row.status = "starting";
    this.updateWorkspaceRow(row);
    if (row.host) this.disposeWorkspacePanel(row);
    const host = document.createElement("div");
    host.className = "chappa-panel-host";
    this.stack.appendChild(host);
    const panel = new TerminalPanel({
      container: host,
      api: this.api,
      settings: this.settings,
      active: false,
      spawn: (onFrame) => this.spawnWorkspaceChannel(name, onFrame),
    });
    row.host = host;
    row.panel = panel;
    try {
      const termId = await panel.start();
      this.registerWorkspacePanel(row, termId);
    } catch (err) {
      // Spawn failed (bad command, relative working_dir, …): surface + reset.
      console.error(`[chappa-ai] failed to start workspace command ${name}: ${err}`);
      this.disposeWorkspacePanel(row);
      row.status = "failed";
      this.updateWorkspaceRow(row);
    }
  }

  /** Register a started command's panel as a PLAIN workspace rail entry (the
   *  "same rule as plain shells" for notifications — no project attribution). */
  private registerWorkspacePanel(row: WorkspaceCommandRow, termId: number): void {
    if (!row.panel || !row.host) return;
    this.seen.add(termId);
    row.termId = termId;
    row.status = "starting";
    this.entries.set(termId, {
      id: termId,
      panel: row.panel,
      host: row.host,
      name: row.name,
      status: "starting",
      exitCode: null,
      title: null,
      activity: false,
      bell: false,
      attention: null,
    });
    this.lruAcquire(termId);
    this.activate(termId);
    this.updateWorkspaceRow(row);
    this.renderRail();
  }

  /** Tear down a workspace command's panel + host and unregister its entry. */
  private disposeWorkspacePanel(row: WorkspaceCommandRow): void {
    if (row.termId !== null) {
      const wasActive = this.activeId === row.termId;
      this.entries.delete(row.termId);
      this.lru.release(row.termId);
      this.stats.forget(row.termId);
      if (wasActive) {
        this.activeId = null;
        const ids = [...this.entries.keys()];
        if (ids.length > 0) this.activate(ids[ids.length - 1]);
      }
    }
    if (row.panel) {
      row.panel.dispose();
      row.panel = null;
    }
    if (row.host) {
      row.host.remove();
      row.host = null;
    }
    row.termId = null;
  }

  /** Stop one command (idempotent) and dispose its panel. */
  async stopWorkspace(name: string): Promise<void> {
    const row = this.wsCmds.get(name);
    if (!row) return;
    try {
      await this.api.stopWorkspaceCommand(name);
    } catch {
      // Not in Tauri / transient — the row still reflects the event.
    }
    row.status = "stopped";
    row.exitCode = null;
    this.disposeWorkspacePanel(row);
    this.updateWorkspaceRow(row);
  }

  /** Restart = stop if live, then start (a stopped command just starts). */
  async restartWorkspace(name: string): Promise<void> {
    await this.stopWorkspace(name);
    await this.startWorkspace(name);
  }

  /** `+ Add command` — the editor, with the workspace working-dir
   *  hint ("absolute path (or empty for home)"). */
  private async addWorkspaceCommand(): Promise<void> {
    const form = await this.promptWorkspaceCommand(null, "Add command");
    if (!form) return;
    try {
      const rows = await this.api.saveWorkspaceCommand(null, defFromForm(form));
      this.applyWorkspaceMutation(rows);
    } catch (err) {
      await this.showError(`Could not add command: ${err}`);
    }
  }

  /** `Edit command…` — same editor, pre-filled from the row, same hint. */
  private async editWorkspaceCommand(name: string): Promise<void> {
    const row = this.wsCmds.get(name);
    if (!row) return;
    const form = await this.promptWorkspaceCommand(wsFormFromRow(row), "Edit command");
    if (!form) return;
    try {
      const rows = await this.api.saveWorkspaceCommand(row.name, defFromForm(form));
      this.applyWorkspaceMutation(rows);
    } catch (err) {
      await this.showError(`Could not edit command: ${err}`);
    }
  }

  /** `Delete command "<name>"` (confirm). A running command is
   *  stopped Rust-side. */
  private async deleteWorkspaceCommand(name: string): Promise<void> {
    const row = this.wsCmds.get(name);
    if (!row) return;
    const ok = await this.confirmDeleteCommand(name);
    if (!ok) return;
    try {
      this.wsCmds.delete(name);
      this.disposeWorkspacePanel(row);
      this.wsCmdsSig = "\u0000never-rendered";
      this.renderWorkspaceCmds();
      const rows = await this.api.deleteWorkspaceCommand(name);
      this.applyWorkspaceMutation(rows);
    } catch (err) {
      await this.showError(`Could not delete command: ${err}`);
    }
  }

  /** `Copy command` — the row's command line to the clipboard. */
  private copyWorkspaceCommand(row: WorkspaceCommandRow): void {
    try {
      void navigator.clipboard.writeText(row.command);
    } catch {
      /* clipboard unavailable (jsdom) — nothing to do */
    }
  }

  /** Adopt a store-mutation answer into the rows (Rust returns the rows). */
  private applyWorkspaceMutation(rows: ipc.WorkspaceCommandDto[]): void {
    const next = new Map(rows.map((dto) => [dto.name, wsRowFromDto(dto)]));
    // Dispose panels of any command that disappeared (deleted).
    for (const name of this.wsCmds.keys()) {
      if (!next.has(name)) {
        const gone = this.wsCmds.get(name);
        if (gone) this.disposeWorkspacePanel(gone);
      }
    }
    this.wsCmds.clear();
    for (const [name, row] of next) this.wsCmds.set(name, row);
    this.wsCmdsSig = "\u0000never-rendered";
    this.renderWorkspaceCmds();
  }

  /** `workspace://status` — update one command row in place. */
  applyWorkspaceStatus(p: ipc.WorkspaceStatusEvent): void {
    const row = this.wsCmds.get(p.name);
    if (!row) return;
    row.status = (p.status as RailStatus) ?? row.status;
    row.exitCode = p.exit_code ?? null;
    if (p.term_id !== null) row.termId = p.term_id;
    else if (row.status === "stopped" || row.status === "failed") row.termId = null;
    this.updateWorkspaceRow(row);
  }

  /** Right-click a workspace command row: open the reduced context menu. */
  private showWsCmdMenu(e: MouseEvent, name: string): void {
    e.preventDefault();
    this.hideWsCmdMenu();
    this.wsCmdMenuFor = name;
    this.wsCmdMenu.style.display = "block";
    const rect = this.wsCmdMenu.getBoundingClientRect();
    const x = Math.min(e.clientX, window.innerWidth - rect.width - 4);
    const y = Math.min(e.clientY, window.innerHeight - rect.height - 4);
    this.wsCmdMenu.style.left = `${Math.max(0, x)}px`;
    this.wsCmdMenu.style.top = `${Math.max(0, y)}px`;
  }

  private hideWsCmdMenu(): void {
    this.wsCmdMenu.style.display = "none";
    this.wsCmdMenuFor = null;
  }

  /** Render the COMMANDS rows (guarded by wsCmdsSig — most events change
   *  nothing a row shows). */
  private renderWorkspaceCmds(): void {
    const rows = [...this.wsCmds.values()];
    const count = rows.length;
    if (this.wsCmdCount.textContent !== String(count)) {
      this.wsCmdCount.textContent = String(count);
    }
    const sig = `${count}:${rows.map((r) => `${r.name}=${r.status}`).join(",")}:${this.wsCmds.size}`;
    if (sig === this.wsCmdsSig) return;
    this.wsCmdsSig = sig;
    this.wsCmdList.textContent = "";
    for (const row of rows) this.wsCmdList.appendChild(this.buildWorkspaceCmdRow(row));
  }

  /** One COMMANDS card: status pill, name, command text, start/stop/restart.
   *  Reuses the process-card anatomy; REDUCED context menu on right-click. */
  private buildWorkspaceCmdRow(row: WorkspaceCommandRow): HTMLElement {
    const el = document.createElement("div");
    el.className = "chappa-process-row";

    const info = document.createElement("div");
    info.className = "chappa-process-info";
    const title = document.createElement("div");
    title.className = "chappa-process-title";
    const nameEl = document.createElement("span");
    nameEl.className = "chappa-process-name";
    nameEl.textContent = row.name;
    nameEl.title = row.command;
    if (row.autoStart) {
      const auto = document.createElement("span");
      auto.className = "chappa-process-badge auto";
      auto.textContent = "AUTO";
      auto.title = "auto_start: runs at app launch";
      title.append(auto);
    }
    title.append(nameEl);
    const cmd = document.createElement("div");
    cmd.className = "chappa-process-cmd";
    cmd.textContent = row.command;
    info.append(title, cmd);

    const side = document.createElement("div");
    side.className = "chappa-process-side";
    const pill = document.createElement("span");
    pill.className = "chappa-status-pill";
    const actions = document.createElement("div");
    actions.className = "chappa-process-actions";
    const startBtn = this.processButton("▶", "Start", () => void this.startWorkspace(row.name));
    const stopBtn = this.processButton("■", "Stop", () => void this.stopWorkspace(row.name));
    const restartBtn = this.processButton("⟳", "Restart", () => void this.restartWorkspace(row.name));
    actions.append(startBtn, stopBtn, restartBtn);
    side.append(pill, actions);
    el.append(info, side);
    el.addEventListener("click", (e) => {
      if ((e.target as Element).closest(".chappa-process-btn")) return;
      if (row.termId !== null && this.entries.has(row.termId)) this.activate(row.termId);
    });
    el.addEventListener("contextmenu", (e) => this.showWsCmdMenu(e, row.name));

    el.classList.toggle("active", row.termId !== null && row.termId === this.activeId);

    row.el = el;
    row.pill = pill;
    row.startBtn = startBtn;
    row.stopBtn = stopBtn;
    row.restartBtn = restartBtn;
    this.updateWorkspaceRow(row);
    return el;
  }

  /** In-place status refresh for one command row. */
  private updateWorkspaceRow(row: WorkspaceCommandRow): void {
    if (!row.el || !row.pill) return;
    row.pill.textContent = row.status;
    row.pill.className = `chappa-status-pill ${row.status}`;
    const live = row.status === "starting" || row.status === "running";
    if (row.startBtn) row.startBtn.disabled = live;
    if (row.stopBtn) row.stopBtn.disabled = !live;
    if (row.restartBtn) row.restartBtn.disabled = false;
    row.el.classList.toggle("active", row.termId !== null && row.termId === this.activeId);
  }

  /** The state the S/A/P chords (and the trust gate's Run) target: the given
   *  project, else the EXPANDED one ("S/A/P chords target the
   *  expanded project"). */
  private chordState(projectId?: number): ProjectState | null {
    if (projectId !== undefined) return this.openProjects.get(projectId) ?? null;
    return this.expandedState();
  }

  /** `S` — start the auto-starting processes (and the trust gate's Run). */
  async startAuto(projectId?: number): Promise<void> {
    const state = this.chordState(projectId);
    if (!state) return;
    for (const row of state.processRows.values()) {
      if (row.autoStart) await this.startProcess(state.id, row.name);
    }
  }

  /** `A` — start every process. */
  async startAll(projectId?: number): Promise<void> {
    const state = this.chordState(projectId);
    if (!state) return;
    for (const name of state.processRows.keys()) await this.startProcess(state.id, name);
  }

  /** `P` — stop every process. */
  async stopAll(projectId?: number): Promise<void> {
    const state = this.chordState(projectId);
    if (!state) return;
    for (const row of state.processRows.values()) {
      if (row.status === "starting" || row.status === "running") {
        await this.stopProcess(state.id, row.name);
      }
    }
  }

  /** The frames-channel spawn a project-process panel uses: the webview side
   *  of `start_project_process` (the Rust side spawns the chappa.yml command and
   *  returns the registry id). */
  private spawnProcessChannel(
    projectId: number,
    name: string,
    onFrame: (buf: ArrayBuffer) => void,
  ): Promise<number> {
    const frames = ipc.makeProjectFramesChannel((data) => {
      onFrame(data instanceof ArrayBuffer ? data : Uint8Array.from(data).buffer);
    });
    return this.api.startProjectProcess(projectId, name, frames);
  }

  /** Register a started process's panel as a "process" rail entry (activat-
   *  able, LRU-tracked, but rendered in the project section, not the rail). */
  private registerProcessPanel(row: ProcessRow, termId: number): void {
    if (!row.panel || !row.host) return;
    this.seen.add(termId);
    row.termId = termId;
    row.status = "starting";
    this.entries.set(termId, {
      id: termId,
      panel: row.panel,
      host: row.host,
      name: row.name,
      status: "starting",
      exitCode: null,
      title: null,
      activity: false,
      bell: false,
      attention: null,
      kind: "process",
      process: { projectId: row.projectId, name: row.name },
    });
    this.lruAcquire(termId);
    this.activate(termId);
    this.processRowChanged(row);
  }

  /** Tear down a process's panel + host and unregister its entry. */
  private disposeProcessPanel(row: ProcessRow): void {
    if (row.termId !== null) {
      const wasActive = this.activeId === row.termId;
      this.entries.delete(row.termId);
      this.lru.release(row.termId);
      this.stats.forget(row.termId);
      if (wasActive) {
        this.activeId = null;
        const ids = [...this.entries.keys()];
        if (ids.length > 0) this.activate(ids[ids.length - 1]);
      }
    }
    if (row.panel) {
      row.panel.dispose();
      row.panel = null;
    }
    if (row.host) {
      row.host.remove();
      row.host = null;
    }
    row.termId = null;
    // The badges described a terminal that no longer exists.
    row.bell = false;
    row.activity = false;
    this.updateProcessBadges(row);
  }

  /** Build the project section: header pill + TERMINALS subsection + one row
   *  per process.
   *
   *  The old guard also hid the section when `processRows.size === 0`. Now
   *  an open project ALWAYS shows its section: the TERMINALS
   *  subsection is the only way to add a project terminal, and a chappa.yml with
   *  no commands would otherwise leave it unreachable. */
  private renderProject(): void {
    const state = this.expandedState();
    if (!state) {
      this.projectSection.style.display = "none";
      this.renderProjectTerminals();
      this.renderProjectAgents();
      this.scratchpads.setProject(undefined);
      this.renderActiveSection();
      return;
    }
    this.projectSection.style.display = "block";
    this.renderProjectTerminals();
    this.renderProjectAgents();
    // The subsection re-fetches only when the expanded project
    // actually changes (this runs on every rail event).
    this.scratchpads.setProject(state.id);
    // The load-error line, and the mutation surfaces it disables.
    this.projectYmlErrorEl.textContent = state.ymlError
      ? `chappa.yml failed to load — editing disabled until it parses again:\n${state.ymlError}`
      : "";
    this.projectYmlErrorEl.style.display = state.ymlError ? "block" : "none";
    this.projectProcessesEl.textContent = "";
    // Favorites first within COMMANDS; a stable sort keeps the file
    // order inside each group.
    const rows = [...state.processRows.values()].sort(
      (a, b) => Number(b.favorite) - Number(a.favorite),
    );
    for (const row of rows) {
      this.projectProcessesEl.appendChild(this.buildProcessRow(row));
    }
    const add = document.createElement("button");
    add.className = "chappa-process-add";
    add.textContent = "+ Add command";
    add.disabled = state.ymlError !== null;
    add.title = state.ymlError ? `editing disabled: ${state.ymlError}` : "Add a command to chappa.yml";
    add.addEventListener("mousedown", (e) => e.preventDefault());
    add.addEventListener("click", () => void this.onAddCommand(state.id));
    this.projectProcessesEl.appendChild(add);
    this.refreshProjectHeader();
    // Open/expand runs through here without touching renderRail or
    // any process row — and opening a SECOND project is what turns the
    // owning-project tags on (they sign into activeSig).
    this.renderActiveSection();
  }

  // --- multi-project rail -----------------------------------------

  /**
   * Expand one OPEN project: it renders the full section, the previously
   * expanded one collapses to a summary row. Switching is expand/collapse
   * only, nothing stops — no lifecycle call anywhere on this path.
   */
  expandProject(id: number): void {
    const state = this.openProjects.get(id);
    if (!state || this.expandedProjectId === id) return;
    this.expandedProjectId = id;
    // Badge-clearing rule (decided): the AGGREGATED summary badge
    // clears on expand — it means "something happened while collapsed", and
    // expanding is looking. Per-terminal / per-card badges persist until that
    // terminal is activated (the rule, untouched).
    state.bell = false;
    state.activity = false;
    // The stopped-command pane is project-scoped: don't keep fronting a
    // collapsed project's empty state.
    if (this.emptyFor !== null && this.emptyFor.projectId !== id) this.hideProcessEmptyState();
    this.switcher.textContent = this.projects.get(id)?.name ?? `project ${id}`;
    this.renderProject();
    this.renderProjectSummaries();
    // The workspace rail's exclusion of bound agents keys off the
    // EXPANDED project, and this is the one switch path that changes it
    // without any entry change (no close, no spawn) — re-render the rail so a
    // bound agent falls back to the workspace list (or back into its
    // project's AGENTS section) the moment the switch happens. The sig
    // guards make the re-derivation of the other subsections a no-op.
    this.renderRail();
  }

  /**
   * The EXPLICIT "Close project": confirm when anything is running
   * (listing what will stop), then stop processes + dispose panels + close
   * the project's terminals — today's closeProject behaviour, now opt-in.
   * Nothing running skips the confirm.
   */
  async closeProjectFlow(id: number): Promise<void> {
    const state = this.openProjects.get(id);
    if (!state) return;
    // "Confirm when anything is running, listing what will stop" — that is
    // the running PROCESSES plus the project's live bound TERMINALS (they
    // close too, with no per-terminal confirm of their own).
    const running = [...state.processRows.values()]
      .filter((r) => r.status === "starting" || r.status === "running")
      .map((r) => r.name);
    for (const entry of this.entries.values()) {
      if (this.boundToProject(entry, id) && (entry.status === "starting" || entry.status === "running")) {
        running.push(this.entryLabel(entry));
      }
    }
    if (running.length > 0) {
      const ok = await this.ask(`closeProject:${id}`, () =>
        this.confirmProjectClose(this.projects.get(id)?.name ?? `project ${id}`, running),
      );
      if (ok !== true) return;
      // The project can have gone away while the modal was up.
      if (!this.openProjects.has(id)) return;
    }
    this.closeProjectState(id);
    // Keep the one-expanded invariant: closing the expanded project while
    // others stay open expands the most recently opened remaining one.
    if (this.expandedProjectId === null && this.openProjects.size > 0) {
      const remaining = [...this.openProjects.keys()];
      this.expandProject(remaining[remaining.length - 1]);
    }
  }

  /**
   * Forget a stored project (the switcher row's 🗑; the Rust `remove_project`
   * command predates this affordance). Confirm ALWAYS — even closed, removal
   * forgets chappa-side state (trust, favorites, notification levels); the
   * folder and its chappa.yml stay on disk. Rust stops any live processes and
   * drops the runtime; the frontend then closes the local project view like
   * an explicit close (no second confirm) and drops the switcher row.
   */
  async removeProjectFlow(id: number): Promise<void> {
    const name = this.projects.get(id)?.name ?? `project ${id}`;
    const running: string[] = [];
    const state = this.openProjects.get(id);
    if (state) {
      for (const r of state.processRows.values()) {
        if (r.status === "starting" || r.status === "running") running.push(r.name);
      }
      for (const entry of this.entries.values()) {
        if (this.boundToProject(entry, id) && (entry.status === "starting" || entry.status === "running")) {
          running.push(this.entryLabel(entry));
        }
      }
    }
    const ok = await this.ask(`removeProject:${id}`, () => this.confirmProjectRemove(name, running));
    if (ok !== true) return;
    try {
      await this.api.removeProject(id);
    } catch (err) {
      await this.showError(`Could not remove project: ${err}`);
      return;
    }
    if (this.openProjects.has(id)) this.closeProjectState(id);
    if (this.expandedProjectId === null && this.openProjects.size > 0) {
      const remaining = [...this.openProjects.keys()];
      this.expandProject(remaining[remaining.length - 1]);
    }
    this.projects.delete(id);
    this.renderProjectMenu();
  }

  /** Rebuild the collapsed-project summary rows (open order, minus the
   *  expanded one). Rebuilt whole on open/expand/close — the rows carry no
   *  focus; STATUS updates between rebuilds land in place via
   *  refreshProjectSummary. */
  private renderProjectSummaries(): void {
    this.projectSummariesEl.textContent = "";
    for (const state of this.openProjects.values()) {
      if (state.id === this.expandedProjectId) {
        state.summaryEl = null;
        state.summaryPill = null;
        state.summaryBellEl = null;
        state.summaryActivityEl = null;
        continue;
      }
      this.projectSummariesEl.appendChild(this.buildSummaryRow(state));
    }
  }

  /** One collapsed project's one-line summary row: name + running-count pill
   *  + aggregated bell/activity badge + ×.
   *
   *  The row is a DIV holding the expand button and the × as SIBLINGS — the
   *  renderProjectMenu rule ("nesting the ✎ inside the option button would be
   *  invalid HTML"). Both buttons are tabbable: Enter/Space on the expand
   *  button expands (its click bubbles to the row's delegated handler, the
   *  process-card pattern), on the × it closes. */
  private buildSummaryRow(state: ProjectState): HTMLElement {
    const row = document.createElement("div");
    row.className = "chappa-project-summary";
    row.dataset.projectId = String(state.id);
    const projectName = this.projects.get(state.id)?.name ?? `project ${state.id}`;
    row.title = projectName;
    // Right-click a collapsed summary row for the same project
    // context menu as the expanded header.
    row.addEventListener("contextmenu", (e) => this.showProjectCtxMenu(e, state.id));
    // preventDefault keeps the terminal textarea focused (the switcher's own
    // pattern); the click still expands.
    row.addEventListener("mousedown", (e) => e.preventDefault());
    // The WHOLE row is the expand target (padding and gaps included), with
    // only the × excluded — the buildProcessRow precedent.
    row.addEventListener("click", (e) => {
      if ((e.target as Element).closest(".chappa-project-summary-close")) return;
      this.expandProject(state.id);
    });

    const expand = document.createElement("button");
    expand.className = "chappa-project-summary-expand";
    expand.title = `Expand project "${projectName}"`;
    const name = document.createElement("span");
    name.className = "chappa-project-summary-name";
    name.textContent = projectName;
    const pill = document.createElement("span");
    pill.className = "chappa-project-pill";
    const activityEl = document.createElement("span");
    activityEl.className = "chappa-rail-activity";
    activityEl.title = "a hidden terminal produced output";
    const bellEl = document.createElement("span");
    bellEl.className = "chappa-rail-bell";
    bellEl.append(bellIcon());
    bellEl.title = "bell or notification while collapsed";
    expand.append(name, pill, activityEl, bellEl);
    const close = document.createElement("button");
    close.className = "chappa-rail-close chappa-project-summary-close";
    close.textContent = "×";
    close.title = `Close project "${projectName}" (stops its processes)`;
    close.addEventListener("mousedown", (e) => e.stopPropagation());
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.closeProjectFlow(state.id);
    });
    row.append(expand, close);

    state.summaryEl = row;
    state.summaryPill = pill;
    state.summaryBellEl = bellEl;
    state.summaryActivityEl = activityEl;
    this.refreshProjectSummary(state.id);
    return row;
  }

  /** In-place refresh of one collapsed project's summary row (running count
   *  + aggregated badges). No-op while the project is expanded (no row). */
  private refreshProjectSummary(projectId: number): void {
    const state = this.openProjects.get(projectId);
    if (!state || !state.summaryEl) return;
    const { running, total } = this.runningCount(state);
    state.summaryPill!.textContent = `${running}/${total}`;
    state.summaryPill!.classList.toggle("running", running > 0);
    state.summaryBellEl!.style.display = state.bell ? "inline" : "none";
    state.summaryActivityEl!.style.display = state.activity ? "inline-block" : "none";
  }

  /** Aggregate one badge-worthy event onto its project's summary row. Only
   *  COLLAPSED projects aggregate — the expanded one shows its per-row
   *  badges instead. */
  private aggregateBadge(projectId: number, kind: "bell" | "activity"): void {
    if (projectId === this.expandedProjectId) return;
    const state = this.openProjects.get(projectId);
    if (!state) return;
    if (kind === "bell") state.bell = true;
    else state.activity = true;
    this.refreshProjectSummary(projectId);
  }

  // --- ACTIVE section ---------------------------------------------
  //
  // READS existing state only: process rows, rail entries and
  // their badges. No event handling of its own, no parallel store —
  // attention IS the badge, and it clears exactly when the badge clears
  // (activation). Re-derived from the render passes that already run: the
  // tails of renderRail, processRowChanged, markProcessCard, renderProject
  // and closeProjectState all call renderActiveSection(), and the activeSig
  // guard makes the no-visible-change case free.

  /**
   * The current ACTIVE rows, across ALL open projects, in stable derivation
   * order — open projects in open order (each project's running processes in
   * chappa.yml order), then badged live terminals in entries order — with
   * attention rows stably partitioned to the TOP.
   *
   * A process terminal never yields a separate terminal row: its process is
   * running (or its child is dead), so the process row is its one surface and
   * carries its card badge as attention.
   */
  private collectActiveRows(): ActiveRow[] {
    const live = (s: RailStatus): boolean => s === "starting" || s === "running";
    const rows: ActiveRow[] = [];
    for (const state of this.openProjects.values()) {
      for (const row of state.processRows.values()) {
        if (!live(row.status)) continue;
        rows.push({
          kind: "process",
          projectId: state.id,
          name: row.name,
          termId: row.termId,
          processName: row.name,
          status: row.status,
          attention: row.bell ? "badge" : null,
          activity: row.activity,
        });
      }
    }
    for (const [id, entry] of this.entries) {
      if (entry.kind === "process") continue; // the process row above is its surface
      if (!live(entry.status)) continue;
      // A bridge fault is attention at the `badge` tier (the
      // reserved `agent-waiting` slot is the typed awaiting-input).
      const fault = bridgeFault(entry.agent);
      // The typed awaiting_input fact is the `agent-waiting` tier
      // on the entry itself — it outranks the heuristic badge and clears on
      // the next send, not on activation.
      const waiting = entry.attention === "agent-waiting";
      if (!entry.bell && !entry.activity && !fault && !waiting) continue;
      rows.push({
        kind: "terminal",
        projectId: this.centerAttribution(entry).projectId,
        name: this.entryLabel(entry),
        termId: id,
        processName: null,
        status: entry.status,
        attention: waiting ? "agent-waiting" : entry.bell || fault ? "badge" : null,
        activity: entry.activity,
      });
    }
    // The sort rule: attention rows to the TOP, each partition keeping the
    // stable derivation order above. The typed
    // "agent-waiting" tier sorts ABOVE the heuristic "badge" tier — an agent
    // that has SAID it wants input outranks a terminal that merely rang.
    return [
      ...rows.filter((r) => r.attention === "agent-waiting"),
      ...rows.filter((r) => r.attention === "badge"),
      ...rows.filter((r) => r.attention === null),
    ];
  }

  /** Rebuild the ACTIVE section behind the activeSig guard (the
   *  projectTermsSig precedent — the hooks fire per event and most events
   *  change nothing an ACTIVE row shows). Empty ⇒ the section hides
   *  entirely. */
  private renderActiveSection(): void {
    const rows = this.collectActiveRows();
    const multi = this.openProjects.size > 1;
    // "\u001f" (fields) / "\u001e" (rows) — the ESCAPE SEQUENCES, never literal control bytes
    // in source (the projectTermsSig rule) — separate fields and rows; none
    // of the joined pieces can contain them in practice. The owning-project
    // tag is part of what a row renders, so it signs too (shown only when
    // more than one project is open).
    const sig = rows
      .map((r) =>
        [
          r.kind,
          r.projectId ?? "",
          r.processName ?? "",
          r.termId ?? "",
          r.name,
          r.status,
          r.attention ?? "",
          r.activity,
          multi && r.projectId !== null ? this.projectLabel(r.projectId) : "",
        ].join("\u001f"),
      )
      .join("\u001e");
    if (sig === this.activeSig) return;
    this.activeSig = sig;
    this.activeList.textContent = "";
    if (rows.length === 0) {
      this.activeSection.style.display = "none";
      return;
    }
    this.activeSection.style.display = "block";
    this.activeCount.textContent = String(rows.length);
    for (const row of rows) {
      this.activeList.appendChild(this.buildActiveRow(row, multi));
    }
  }

  /** A project's display name (tag text / fallbacks). */
  private projectLabel(id: number): string {
    return this.projects.get(id)?.name ?? `project ${id}`;
  }

  /** One ACTIVE row: status dot, name, owning-project tag (muted, only when
   *  more than one project is open), activity dot, attention marker. */
  private buildActiveRow(row: ActiveRow, showProjectTag: boolean): HTMLElement {
    const el = document.createElement("button");
    el.className = "chappa-active-row";
    el.dataset.kind = row.kind;
    if (row.projectId !== null) el.dataset.projectId = String(row.projectId);
    if (row.attention !== null) el.classList.add("attention");
    el.title = row.name;
    // preventDefault keeps the terminal textarea focused (the switcher's own
    // pattern); the click still lands.
    el.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", () => this.activateActiveRow(row));

    const dot = document.createElement("span");
    dot.className = `chappa-status-dot ${row.status}`;
    dot.title = row.status;
    const name = document.createElement("span");
    name.className = "chappa-rail-name";
    name.textContent = row.name;
    el.append(dot, name);
    if (showProjectTag && row.projectId !== null) {
      const tag = document.createElement("span");
      tag.className = "chappa-active-project";
      tag.textContent = this.projectLabel(row.projectId);
      tag.title = tag.textContent;
      el.append(tag);
    }
    // Marker classes are the section's OWN (chappa-active-*, never the rail's
    // chappa-rail-bell/-activity): the badge state renders on BOTH surfaces and
    // shared classes would double every unscoped badge query.
    if (row.activity) {
      const activity = document.createElement("span");
      activity.className = "chappa-active-activity";
      activity.title = "produced output while hidden";
      el.append(activity);
    }
    if (row.attention !== null) {
      const mark = document.createElement("span");
      mark.className = "chappa-active-attention";
      if (row.attention === "agent-waiting") mark.textContent = "⌨";
      else mark.append(bellIcon());
      mark.title = row.attention === "agent-waiting" ? "waiting for your input" : "needs attention";
      el.dataset.attention = row.attention;
      el.append(mark);
    }
    return el;
  }

  /** ACTIVE row click: expand the owning project (project-scoped rows), then
   *  activate the terminal — or, for a process, its pane via activateProcess
   *  (which fronts the stopped pane when the panel is gone). Activation is
   *  what clears the badge, so attention clears exactly here (rule,
   *  no parallel store). */
  private activateActiveRow(row: ActiveRow): void {
    if (row.projectId !== null) this.expandProject(row.projectId);
    if (row.kind === "process" && row.projectId !== null && row.processName !== null) {
      // Re-resolved by (projectId, name) — a captured termId could predate a
      // respawn (the keying rule).
      this.activateProcess(row.projectId, row.processName);
    } else if (row.termId !== null && this.entries.has(row.termId)) {
      this.activate(row.termId);
    }
  }

  // --- project terminals ------------------------------------------

  /**
   * The TERMINALS subsection rows + count: every entry BOUND to the open
   * project. Closing the project (switching away) keeps its terminals
   * alive but hides the section with the rest of the project UI;
   * re-opening shows them again — which falls out of filtering by
   * `activeProjectId` here, since the entries themselves are never touched by
   * a project switch.
   */
  private renderProjectTerminals(): void {
    const rows: Array<[number, RailEntry]> = [];
    if (this.expandedProjectId !== null) {
      for (const [id, entry] of this.entries) {
        if (entry.kind !== "project-shell" || entry.projectShell?.projectId !== this.expandedProjectId) {
          continue;
        }
        rows.push([id, entry]);
      }
    }
    // Cheap signature over what a row actually renders (id, displayed title,
    // status, badges, active accent). "\u001f" — the ESCAPE SEQUENCE, never a
    // literal control byte in source — is the join separator; none of the
    // joined pieces can contain it in practice.
    const sig = rows
      .map(([id, e]) =>
        [id, e.title ?? e.name, e.status, e.bell, e.activity, id === this.activeId].join("\u001f"),
      )
      .join("\u001f");
    if (sig === this.projectTermsSig) return;
    this.projectTermsSig = sig;
    this.projectTermList.textContent = "";
    for (const [id, entry] of rows) {
      this.projectTermList.appendChild(this.buildTerminalRow(id, entry, "chappa-project-term-row"));
    }
    this.projectTermCount.textContent = String(rows.length);
  }

  /**
   * The AGENTS subsection rows + N/M count: every agent entry
   * BOUND to the expanded project, `N` = running/starting, `M` = total
   * listed. The header + spawner render even at 0/0 — discoverability is
   * the point — with zero rows under it and no empty-state text.
   * The workspace rail's exclusion is the SAME predicate
   * (agentInExpandedSection), so a bound row renders exactly once: under its
   * project while it is the expanded one, in the workspace list otherwise
   * (nothing may vanish from the rail). The row is the shared
   * buildTerminalRow — the workspace agent-row pieces verbatim (status dot,
   * title plumbing, the model tag / container dot / bridge ring and
   * the typed attention tier, bell/activity badges, exit code, close ×,
   * click = focus its panel).
   */
  private renderProjectAgents(): void {
    const rows: Array<[number, RailEntry]> = [];
    if (this.expandedProjectId !== null) {
      for (const [id, entry] of this.entries) {
        if (this.agentInExpandedSection(entry)) rows.push([id, entry]);
      }
    }
    // The N/M count stays FLAT over every agent row on this surface,
    // subagents included — parent + child + another root running, one exited
    // root → 3/4. `rows.length` counts each bound agent once; children are
    // never double-counted and never skipped.
    const running = rows.filter(([, e]) => isLive(e.status)).length;
    // The rows render as a FOREST (children nested under their
    // spawning parent). The `render` list feeds both the signature (which
    // includes depth, so an orphan promotion repaints) and the DOM.
    const forest = agentForest(
      rows.map(([id, e]) => ({ id, parentId: e.agent?.parentId ?? null, entry: e })),
    );
    const render: Array<{ id: number; entry: RailEntry; depth: number; lastChild: boolean }> = [];
    for (let j = 0; j < forest.length; j++) {
      const [fa, depth] = forest[j];
      const lastChild = depth >= 1 && (j + 1 >= forest.length || forest[j + 1][1] < depth);
      render.push({ id: fa.id, entry: fa.entry, depth, lastChild });
    }
    // Cheap signature over what a row actually renders (the
    // renderProjectTerminals precedent, plus the agent-only facts the row
    // shows: the typed attention tier and the bridge state that rings it,
    // and the nesting depth so promotion/closure repaints).
    const sig = [
      `${running}/${rows.length}`,
      render
        .map(({ id, entry: e, depth }) =>
          [id, e.title ?? e.name, e.status, e.bell, e.activity, e.attention, e.agent?.bridge, id === this.activeId, depth].join(
            "\u001f",
          ),
        )
        .join("\u001f"),
    ].join("\u001f");
    if (sig === this.projectAgentsSig) return;
    this.projectAgentsSig = sig;
    this.projectAgentList.textContent = "";
    for (const { id, entry, depth, lastChild } of render) {
      this.projectAgentList.appendChild(this.buildTerminalRow(id, entry, "chappa-project-agent-row", depth, lastChild));
    }
    this.projectAgentCount.textContent = `${running}/${rows.length}`;
  }

  /** The subsection's ＋ / profile pick: `+` spawns the default shell
   *  (profile menu: that profile) with `cwd = project root`. The root is the
   *  stored project's own path. */
  private newProjectShell(profile?: string): void {
    const id = this.expandedProjectId;
    if (id === null) return;
    void this.newShell(this.projects.get(id)?.path, profile, id);
  }

  private toggleProjectTermMenu(): void {
    if (this.projectTermMenu.style.display === "none") {
      this.fillProfileMenu(
        this.projectTermMenu,
        "chappa-project-term-option",
        "chappa-project-term-empty",
        (profile) => {
          this.hideProjectTermMenu();
          this.newProjectShell(profile);
        },
      );
      this.projectTermMenu.style.display = "block";
    } else {
      this.hideProjectTermMenu();
    }
  }

  private hideProjectTermMenu(): void {
    this.projectTermMenu.style.display = "none";
  }

  /** One process row: name, AUTO/YML badges, command line, status pill, and
   *  start/stop/restart. Status updates land in place via updateProcessRow. */
  private buildProcessRow(row: ProcessRow): HTMLElement {
    const { projectId, name } = row;
    const el = document.createElement("div");
    el.className = "chappa-process-row";

    const info = document.createElement("div");
    info.className = "chappa-process-info";
    const title = document.createElement("div");
    title.className = "chappa-process-title";
    const nameEl = document.createElement("span");
    nameEl.className = "chappa-process-name";
    nameEl.textContent = name;
    nameEl.title = row.command;
    // The favorite star — the card's own toggle for `Add to
    // favorites` (chappa-side only, projects.json).
    const star = document.createElement("button");
    star.className = "chappa-process-star";
    star.classList.toggle("on", row.favorite);
    star.textContent = row.favorite ? "★" : "☆";
    star.title = row.favorite ? "Remove from favorites" : "Add to favorites";
    star.addEventListener("mousedown", (e) => e.preventDefault());
    star.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleFavorite(projectId, name);
    });
    title.append(star, nameEl);
    if (row.autoStart) {
      const auto = document.createElement("span");
      auto.className = "chappa-process-badge auto";
      auto.textContent = "AUTO";
      auto.title = "auto_start: runs when the project opens (after the trust gate)";
      title.append(auto);
    }
    const cmd = document.createElement("div");
    cmd.className = "chappa-process-cmd";
    cmd.textContent = row.command;
    info.append(title, cmd);

    const side = document.createElement("div");
    side.className = "chappa-process-side";
    const pill = document.createElement("span");
    pill.className = "chappa-status-pill";
    const actions = document.createElement("div");
    actions.className = "chappa-process-actions";
    const startBtn = this.processButton("▶", "Start", () => void this.startProcess(projectId, name));
    const stopBtn = this.processButton("■", "Stop", () => void this.stopProcess(projectId, name));
    const restartBtn = this.processButton("⟳", "Restart", () => void this.restartProcess(projectId, name));
    actions.append(startBtn, stopBtn, restartBtn);
    side.append(pill, actions);
    el.append(info, side);
    // The WHOLE card is the way BACK to a process's panel (its only auto-
    // activation is at start) — the handler sits on the row so the padding
    // and the gaps around the info column are clickable too, with only the
    // action buttons excluded. (twice: first there was no way
    // back at all, then only the info column's own box took the click.)
    el.addEventListener("click", (e) => {
      if ((e.target as Element).closest(".chappa-process-btn")) return;
      this.activateProcess(projectId, name);
    });
    // Right-click opens the per-command menu. A separate listener so the
    // click-to-activate path above
    // is untouched.
    el.addEventListener("contextmenu", (e) => this.showRowMenu(e, row));

    // Card badges: a hidden process terminal's bell/activity has no
    // rail row to land on, so it lands here. Built once and toggled in place.
    const activityEl = document.createElement("span");
    activityEl.className = "chappa-process-activity";
    activityEl.title = "produced output while hidden";
    activityEl.style.display = "none";
    const bellEl = document.createElement("span");
    bellEl.className = "chappa-process-bell";
    bellEl.append(bellIcon());
    bellEl.title = "bell or notification while hidden";
    bellEl.style.display = "none";
    title.append(activityEl, bellEl);

    // Build-time active accent: re-expanding a project whose process panel is
    // STILL the active pane rebuilds this card, and activate() only toggles
    // rows that existed when it ran — without this the accent silently
    // dropped until the next activation/status event.
    el.classList.toggle("active", row.termId !== null && row.termId === this.activeId);

    row.el = el;
    row.pill = pill;
    row.bellEl = bellEl;
    row.activityEl = activityEl;
    row.startBtn = startBtn;
    row.stopBtn = stopBtn;
    row.restartBtn = restartBtn;
    this.updateProcessRow(row);
    this.updateProcessBadges(row);
    return el;
  }

  /** Show/hide a card's badges from `row.bell`/`row.activity`, in place. */
  private updateProcessBadges(row: ProcessRow): void {
    if (row.bellEl) row.bellEl.style.display = row.bell ? "inline" : "none";
    if (row.activityEl) row.activityEl.style.display = row.activity ? "inline-block" : "none";
  }

  /** One process row's state changed: repaint the row in place, then the two
   *  aggregate surfaces that summarize it — the expanded project's header
   *  pill and (when the row's project is collapsed) its summary row. Every
   *  mutation site targets the row's OWN project, so `row.projectId` is
   *  always the summary to refresh. */
  private processRowChanged(row: ProcessRow): void {
    this.updateProcessRow(row);
    this.refreshProjectHeader();
    this.refreshProjectSummary(row.projectId);
    // ACTIVE lists running/starting processes: every lifecycle /
    // status mutation funnels through here, so this tail is its process-side
    // rebuild hook (renderRail's tail is the terminal side).
    this.renderActiveSection();
  }

  /** In-place status refresh for one process row (no section rebuild — that
   *  would drop focus mid-interaction). */
  private updateProcessRow(row: ProcessRow): void {
    if (!row.el || !row.pill) return;
    row.pill.textContent = row.status;
    row.pill.className = `chappa-status-pill ${row.status}`;
    const live = row.status === "starting" || row.status === "running";
    if (row.startBtn) row.startBtn.disabled = live;
    if (row.stopBtn) row.stopBtn.disabled = !live;
    if (row.restartBtn) row.restartBtn.disabled = false;
  }

  /** Running/total for one open project's rows. */
  private runningCount(state: ProjectState): { running: number; total: number } {
    const running = [...state.processRows.values()].filter(
      (r) => r.status === "starting" || r.status === "running",
    ).length;
    return { running, total: state.processRows.size };
  }

  /** The header status pill: `running/total Running`. The header
   *  is quiet — the sync toggle and the notification-level
   *  picker live in the Edit-project pane. Also repaints the pane when it is
   *  open (renames and sync flips land here). */
  private refreshProjectHeader(): void {
    const state = this.expandedState();
    const project = this.projects.get(this.expandedProjectId ?? -1);
    this.projectNameEl.textContent = project?.name ?? (this.expandedProjectId !== null ? `project ${this.expandedProjectId}` : "");
    const { running, total } = state ? this.runningCount(state) : { running: 0, total: 0 };
    this.projectPill.textContent = `${running}/${total} Running`;
    this.projectPill.classList.toggle("running", running > 0);
    this.editPane?.refresh();
  }

  // --- notification level UI (relocated to the Edit pane in 44) ----

  /** Persist a new project default and adopt it locally (the resolution for
   *  every inheriting command changes with it).
   *
   *  "all" is sent as a CLEAR (level null): 'all' is already the bottom of
   *  the resolution chain, so an explicit "all" and an unset field resolve
   *  identically — and clearing keeps a never-customized projects.json entry
   *  byte-identical (review finding). This mapping is PROJECT-level
   *  only: a per-process "all" override is meaningful (it beats a project
   *  default of "none") and is persisted verbatim by chooseRowLevel.
   *
   *  On a failed write the optimistic value is REVERTED and the pane
   *  re-syncs — a silently swallowed failure left the UI filtering with a
   *  level that never landed on disk (review finding). */
  private setProjectLevel(projectId: number, level: Level): void {
    const stored = this.projects.get(projectId);
    const prev = stored?.notificationLevel ?? null;
    const wire = level === "all" ? null : level;
    if (stored) this.projects.set(projectId, { ...stored, notificationLevel: wire });
    void this.api.setNotificationLevel(projectId, null, wire).catch(() => {
      const cur = this.projects.get(projectId);
      if (cur) this.projects.set(projectId, { ...cur, notificationLevel: prev });
      this.editPane?.refresh();
    });
    this.editPane?.refresh();
  }

  // --- edit-project pane ------------------------------------------

  /** The live snapshot the Edit pane reads: header + the expanded-or-not
   *  project's stored row + its running counts + yml load state. */
  private editPaneInfo(id: number): ProjectEditInfo | null {
    const project = this.projects.get(id);
    if (!project) return null;
    const state = this.openProjects.get(id);
    const { running, total } = state ? this.runningCount(state) : { running: 0, total: 0 };
    return {
      name: project.name,
      path: project.path,
      notificationLevel: project.notificationLevel,
      ymlError: state?.ymlError ?? null,
      running,
      total,
    };
  }

  /** Open the Edit-project pane for a project. Built lazily like the settings
   *  pane; the context menu targets an OPEN project (expanded header or any
   *  collapsed summary row). */
  openEditPane(id: number): void {
    if (this.projects.has(id) && !this.openProjects.has(id)) return;
    if (!this.editPane) {
      this.editPane = new EditPane({
        info: (pid) => this.editPaneInfo(pid),
        rename: async (pid, name) => {
          const err = await this.renameProjectApply(pid, name);
          if (err) throw err;
        },
        setLevel: (pid, level) => this.setProjectLevel(pid, level),
      });
    }
    this.editPane.open(id);
  }

  /** The Edit-project pane, once opened (tests drive its controls). */
  get editPaneElement(): HTMLElement | null {
    return this.editPane?.element ?? null;
  }

  /** Apply a rename (the ✎ flow's body). Returns an error string on failure
   *  (null on success / no-op), so the pane can revert its field. */
  private async renameProjectApply(id: number, name: string): Promise<string | null> {
    const current = this.projects.get(id);
    if (!current) return "no such project";
    const trim = name.trim();
    // Blank is a refusal, not a rename-to-nothing (Rust refuses it too); an
    // unchanged name is a no-op rather than a pointless write.
    if (trim === "" || trim === current.name) return null;
    try {
      await this.api.renameProject(id, trim);
    } catch (err) {
      return String(err);
    }
    this.projects.set(id, { ...current, name: trim });
    if (this.expandedProjectId === id) {
      this.switcher.textContent = trim;
      this.refreshProjectHeader();
    }
    this.renderProjectSummaries(); // a collapsed row's name follows too
    this.renderProjectMenu();
    this.editPane?.refresh(); // a pane open on a non-expanded project's title
    return null;
  }

  // --- per-command context menu -----------------------------------
  //
  // The command-row menu, v1 subset: Start/Stop, Copy
  // command, Clear output …, Delete, keeping the submenu pattern. Clear output
  // and Delete command are leftovers and stay unbuilt here; this
  // adds the menu itself plus `Notification level ▸ (All / Important / None,
  // checkmark on current)`.

  private rowMenuItem(label: string, parent?: HTMLElement): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "chappa-row-menu-item";
    btn.textContent = label;
    (parent ?? this.rowMenu).appendChild(btn);
    return btn;
  }

  /** A workspace-command context-menu item (appended to wsCmdMenu). */
  private wsCmdMenuItem(label: string, cls: string): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = `chappa-row-menu-item ${cls}`;
    btn.textContent = label;
    this.wsCmdMenu.appendChild(btn);
    return btn;
  }

  /** A `label ▸` menu row with its (initially empty) submenu; hover or click
   *  opens it, closing the other submenus. Same pattern as the notification
   *  submenu, whose parent/sub fields predate this helper. */
  private rowSubmenuPair(label: string, cls: string): [HTMLElement, HTMLElement] {
    const parent = document.createElement("div");
    parent.className = "chappa-row-submenu-parent";
    const toggle = this.rowMenuItem(label, parent);
    toggle.classList.add("chappa-row-submenu-toggle", `${cls}-toggle`);
    const sub = document.createElement("div");
    sub.className = `chappa-row-submenu ${cls}`;
    parent.appendChild(sub);
    this.rowMenu.appendChild(parent);
    parent.addEventListener("mouseenter", () => this.openRowSubmenu(sub));
    toggle.addEventListener("click", () => this.openRowSubmenu(sub));
    return [parent, sub];
  }

  /** A project context-menu item (appended to `projectCtxMenu`). */
  private rowMenuCtxItem(label: string): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "chappa-row-menu-item";
    btn.textContent = label;
    btn.dataset.action = label;
    this.projectCtxMenu.appendChild(btn);
    return btn;
  }

  /** Right-click on the expanded project header or a collapsed summary row:
   *  the project context menu (Edit project… / Close project / Remove
   *  project…). preventDefault so the webview's native menu never joins. */
  private showProjectCtxMenu(e: MouseEvent, id: number | null): void {
    e.preventDefault();
    if (id === null) return;
    // A collapsed row can target a project that is NOT the expanded one.
    this.projectCtxMenuFor = id;
    this.projectCtxMenu.style.display = "block";
    const rect = this.projectCtxMenu.getBoundingClientRect();
    const left = Math.max(0, Math.min(e.clientX, window.innerWidth - rect.width));
    const top = Math.max(0, Math.min(e.clientY, window.innerHeight - rect.height));
    this.projectCtxMenu.style.left = `${left}px`;
    this.projectCtxMenu.style.top = `${top}px`;
  }

  private hideProjectCtxMenu(): void {
    this.projectCtxMenu.style.display = "none";
  }

  private onProjectCtxEdit(): void {
    const id = this.projectCtxMenuFor;
    this.hideProjectCtxMenu();
    if (id !== null) this.openEditPane(id);
  }

  private onProjectCtxClose(): void {
    const id = this.projectCtxMenuFor;
    this.hideProjectCtxMenu();
    if (id !== null) void this.closeProjectFlow(id);
  }

  private onProjectCtxRemove(): void {
    const id = this.projectCtxMenuFor;
    this.hideProjectCtxMenu();
    if (id !== null) void this.removeProjectFlow(id);
  }

  /** Right-click on a command row. */
  private showRowMenu(e: MouseEvent, row: ProcessRow): void {
    // preventDefault so the webview's native menu never appears alongside.
    e.preventDefault();
    this.rowMenuFor = { projectId: row.projectId, name: row.name };
    // Only the APPLICABLE lifecycle item is offered: Start on a stopped
    // command, Stop on a live one.
    const live = row.status === "starting" || row.status === "running";
    this.rowMenuPrimary.textContent = live ? "Stop" : "Start";
    this.rowMenuPrimary.dataset.action = live ? "stop" : "start";
    // The toggles read their current state off the row…
    this.rowMenuFavorite.textContent = row.favorite ? "Remove from favorites" : "Add to favorites";
    this.rowMenuRename.textContent = row.disableAutoRename
      ? "Enable automatic renaming"
      : "Disable automatic renaming";
    this.rowMenuDelete.textContent = `Delete command "${row.name}"`;
    // …and every yml MUTATION is disabled — with the reason as its tooltip —
    // while the file fails to load ("never write while a load error is
    // outstanding").
    const error = this.openProjects.get(row.projectId)?.ymlError ?? null;
    for (const item of [
      this.rowMenuEdit,
      this.rowMenuDelete,
      this.rowAddParent.querySelector<HTMLButtonElement>("button")!,
      this.rowDupParent.querySelector<HTMLButtonElement>("button")!,
      this.rowDupAllParent.querySelector<HTMLButtonElement>("button")!,
    ]) {
      item.disabled = error !== null;
      item.title = error ? `editing disabled: ${error}` : "";
    }
    this.renderRowSubmenu(row);
    this.closeRowSubmenu();
    // Show first, then clamp: the menu must be laid out to measure. A raw
    // clientX/clientY near the bottom/right edge puts the menu offscreen
    // (review finding); jsdom reports 0×0 rects, so the clamp is a
    // no-op there.
    this.rowMenu.style.display = "block";
    const rect = this.rowMenu.getBoundingClientRect();
    const left = Math.max(0, Math.min(e.clientX, window.innerWidth - rect.width));
    const top = Math.max(0, Math.min(e.clientY, window.innerHeight - rect.height));
    this.rowMenu.style.left = `${left}px`;
    this.rowMenu.style.top = `${top}px`;
  }

  /** The `Notification level ▸` submenu: Inherit + the three levels, with the
   *  checkmark on the RESOLVED state — the override when one is set, else the
   *  Inherit row (whose label names what is being inherited). */
  private renderRowSubmenu(row: ProcessRow): void {
    this.rowSubmenu.textContent = "";
    const override = parseLevel(row.notificationLevel);
    const inherited = resolveLevel(null, this.projectDefaultLevel(row.projectId));
    const item = (label: string, checked: boolean, level: Level | null): void => {
      const btn = this.rowMenuItem("", this.rowSubmenu);
      // A fixed-width check column keeps the labels aligned whether or not the
      // row is the current one (a bare "✓ " prefix would shift them).
      const mark = document.createElement("span");
      mark.className = "chappa-row-check";
      mark.style.cssText = "display:inline-block;width:14px;";
      mark.textContent = checked ? "✓" : "";
      btn.append(mark, label);
      btn.dataset.level = level ?? "inherit";
      if (checked) btn.dataset.checked = "true";
      btn.addEventListener("click", () => this.chooseRowLevel(row.projectId, row.name, level));
    };
    item(`Inherit (${LEVEL_LABELS[inherited]})`, override === null, null);
    for (const level of LEVELS) item(LEVEL_LABELS[level], override === level, level);
  }

  /** A submenu choice: null = Inherit (the override is CLEARED). On a failed
   *  write the optimistic override is reverted (review finding). */
  private chooseRowLevel(projectId: number, name: string, level: Level | null): void {
    const row = this.rowFor(projectId, name);
    this.hideRowMenu();
    if (!row) return;
    const prev = row.notificationLevel;
    row.notificationLevel = level;
    void this.api.setNotificationLevel(projectId, name, level).catch(() => {
      const cur = this.rowFor(projectId, name);
      if (cur) cur.notificationLevel = prev;
    });
  }

  private onRowMenuPrimary(): void {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    if (target === null) return;
    const row = this.rowFor(target.projectId, target.name);
    if (!row) return;
    if (row.status === "starting" || row.status === "running") {
      void this.stopProcess(target.projectId, target.name);
    } else {
      void this.startProcess(target.projectId, target.name);
    }
  }

  /** "Copy command" → the OS clipboard. There is no Rust clipboard-write
   *  command exposed to the frontend (OSC 52 writes are handled inside the
   *  pump), so this is `navigator.clipboard`, the documented fallback. */
  private async onRowMenuCopy(): Promise<void> {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    const row = target === null ? undefined : this.rowFor(target.projectId, target.name);
    if (!row) return;
    try {
      await navigator.clipboard.writeText(row.command);
    } catch {
      // Clipboard write denied or unavailable — nothing else to try.
    }
  }

  /** Open one submenu (the notification one by default), closing the rest.
   *  The submenus render on open: `Add ▸` is one item, the two
   *  Duplicate ones list every STORED project (the current one included —
   *  a same-project duplicate is the ` copy` case). Disabled while the
   *  project's chappa.yml fails to load. */
  private openRowSubmenu(sub: HTMLElement = this.rowSubmenu): void {
    this.closeRowSubmenu();
    const target = this.rowMenuFor;
    if (target && sub !== this.rowSubmenu) {
      sub.textContent = "";
      const disabled = (this.openProjects.get(target.projectId)?.ymlError ?? null) !== null;
      if (disabled) return; // the parent is disabled too; nothing to open
      if (sub === this.rowAddSub) {
        const item = this.rowMenuItem("+ Add command", sub);
        item.classList.add("chappa-row-add-command");
        item.addEventListener("click", () => {
          this.hideRowMenu();
          void this.onAddCommand(target.projectId);
        });
      } else {
        const all = sub === this.rowDupAllSub;
        for (const project of this.projects.values()) {
          const item = this.rowMenuItem(project.name, sub);
          item.dataset.projectId = String(project.id);
          item.addEventListener("click", () => {
            this.hideRowMenu();
            // Duplicate all sends NO names: Rust enumerates the freshly
            // loaded source file, so a row snapshot lagging a side edit
            // cannot fail the batch.
            void this.duplicateCommands(target.projectId, all ? [] : [target.name], project.id, all);
          });
        }
      }
    }
    sub.classList.add("open");
  }

  private closeRowSubmenu(): void {
    for (const sub of this.rowMenu.querySelectorAll<HTMLElement>(".chappa-row-submenu")) {
      sub.classList.remove("open");
    }
  }

  private hideRowMenu(): void {
    this.closeRowSubmenu();
    // The lazily rendered submenus are emptied so a stale project list can
    // never show on the next open.
    for (const sub of [this.rowAddSub, this.rowDupSub, this.rowDupAllSub]) sub.textContent = "";
    this.rowMenu.style.display = "none";
  }

  // --- menu actions --------------------------------------------------

  /** `Add to favorites` / `Remove from favorites` from the menu. */
  private onRowMenuFavorite(): void {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    if (target) this.toggleFavorite(target.projectId, target.name);
  }

  /** Optimistic favorite toggle, persisted to projects.json (never the yml);
   *  reverted if the write fails. Re-renders the section so the row moves
   *  into/out of the favorites group. */
  private toggleFavorite(projectId: number, name: string): void {
    const row = this.rowFor(projectId, name);
    if (!row) return;
    const prev = row.favorite;
    row.favorite = !prev;
    if (this.expandedProjectId === projectId) this.renderProject();
    void this.api.setProcessFavorite(projectId, name, row.favorite).catch(() => {
      const cur = this.rowFor(projectId, name);
      if (cur) cur.favorite = prev;
      if (this.expandedProjectId === projectId) this.renderProject();
    });
  }

  /** `Disable automatic renaming` / its inverse — the same optimistic
   *  pattern; `applyTitle` reads the flag. */
  private onRowMenuRename(): void {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    const row = target === null ? undefined : this.rowFor(target.projectId, target.name);
    if (!row) return;
    const prev = row.disableAutoRename;
    row.disableAutoRename = !prev;
    void this.api.setProcessAutoRename(row.projectId, row.name, row.disableAutoRename).catch(() => {
      const cur = this.rowFor(row.projectId, row.name);
      if (cur) cur.disableAutoRename = prev;
    });
  }

  /** `Edit command…`: the editor pre-filled from the row; Save writes the yml
   *  (a changed name is a position-preserving rename Rust-side). */
  private async onRowMenuEdit(): Promise<void> {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    const row = target === null ? undefined : this.rowFor(target.projectId, target.name);
    if (!row) return;
    const form = await this.ask(`edit:${row.projectId}:${row.name}`, () =>
      this.promptCommand(formFromRow(row), "Edit command"),
    );
    if (!form) return;
    await this.saveCommand(row.projectId, row.name, form);
  }

  /** A mutation command's answer is the newest row source: it supersedes any
   *  reload fetch still in flight (see `ProjectState.reloadSeq`). */
  private applyMutationAnswer(projectId: number, rows: ipc.ProjectProcessDto[]): void {
    const state = this.openProjects.get(projectId);
    if (!state) return;
    state.reloadSeq += 1;
    this.applyProcessList(projectId, rows);
  }

  /** `+ Add command` (card button) / `Add ▸ + Add command` (menu). */
  private async onAddCommand(projectId: number): Promise<void> {
    if (!this.openProjects.has(projectId)) return;
    const form = await this.ask(`add:${projectId}`, () => this.promptCommand(null, "Add command"));
    if (!form) return;
    await this.saveCommand(projectId, null, form);
  }

  /** The save behind edit + add: Rust validates, backs up once, writes
   *  atomically, reloads, and answers the rows — adopted verbatim. A refusal
   *  (name clash, containment, load error) surfaces through `showError`.
   *  The concurrent-change refusal ("chappa.yml changed on disk"
   *  guard) additionally re-opens the editor pre-filled with the SAME form —
   *  the modal keeps its values so re-apply is one click;
   *  the rail rows are already the on-disk version (Rust reloaded on
   *  refusal). */
  private async saveCommand(
    projectId: number,
    originalName: string | null,
    form: CommandForm,
  ): Promise<void> {
    try {
      const rows = await this.api.saveProjectProcess(projectId, originalName, defFromForm(form));
      this.applyMutationAnswer(projectId, rows);
    } catch (err) {
      await this.showError(`Could not save command: ${err}`);
      if (String(err).includes("changed on disk")) {
        const key = originalName === null ? `add:${projectId}` : `edit:${projectId}:${originalName}`;
        const title = originalName === null ? "Add command" : "Edit command";
        const again = await this.ask(key, () => this.promptCommand(form, title));
        if (again) await this.saveCommand(projectId, originalName, again);
      }
    }
  }

  /** `Delete command "<name>"`: confirm, then Rust stops it (if running) and
   *  removes it from the file. */
  private async onRowMenuDelete(): Promise<void> {
    const target = this.rowMenuFor;
    this.hideRowMenu();
    const row = target === null ? undefined : this.rowFor(target.projectId, target.name);
    if (!row) return;
    const ok = await this.ask(`delete:${row.projectId}:${row.name}`, () =>
      this.confirmDeleteCommand(row.name),
    );
    if (!ok) return;
    try {
      const rows = await this.api.deleteProjectProcess(row.projectId, row.name);
      this.applyMutationAnswer(row.projectId, rows);
    } catch (err) {
      await this.showError(`Could not delete command: ${err}`);
    }
  }

  /** `Duplicate to ▸` / `Duplicate all commands to ▸`: only the TARGET's
   *  chappa.yml is written; when the target is open in the rail its rows are
   *  adopted from the answer (it hot-reloads). */
  private async duplicateCommands(
    sourceId: number,
    names: string[],
    targetId: number,
    all = false,
  ): Promise<void> {
    if (names.length === 0 && !all) return;
    try {
      const rows = await this.api.duplicateProjectProcesses(sourceId, names, targetId);
      if (this.openProjects.has(targetId)) this.applyMutationAnswer(targetId, rows);
    } catch (err) {
      await this.showError(`Could not duplicate: ${err}`);
    }
  }
}

/** The editor's initial form from a row. */
function formFromRow(row: ProcessRow): CommandForm {
  return {
    name: row.name,
    command: row.command,
    workingDir: row.workingDir ?? "",
    autoStart: row.autoStart,
    autoRestart: row.autoRestart,
    restartWhenChanged: [...row.restartWhenChanged],
    env: Object.entries(row.env),
  };
}

/** The wire definition from a form. `env` stays an ordered pair
 *  list all the way to Rust — an object would be alphabetized by serde_json
 *  before IndexMap ever saw it. */
function defFromForm(form: CommandForm): ipc.ProcessDefDto {
  return {
    name: form.name,
    command: form.command,
    workingDir: form.workingDir === "" ? null : form.workingDir,
    autoStart: form.autoStart,
    autoRestart: form.autoRestart,
    restartWhenChanged: form.restartWhenChanged,
    env: form.env.map(([k, v]) => [k, v]),
  };
}

/** A workspace-command row from a stored DTO. */
function wsRowFromDto(dto: ipc.WorkspaceCommandDto): WorkspaceCommandRow {
  return {
    name: dto.name,
    command: dto.command,
    status: (dto.status as RailStatus) ?? "stopped",
    autoStart: dto.autoStart,
    autoRestart: dto.autoRestart,
    restartWhenChanged: [...dto.restartWhenChanged],
    workingDir: dto.workingDir ?? null,
    env: dto.env,
    exitCode: dto.exitCode,
    termId: dto.termId,
    panel: null,
    host: null,
    el: null,
    pill: null,
    startBtn: null,
    stopBtn: null,
    restartBtn: null,
  };
}

/** The editor's initial form from a workspace-command row. */
function wsFormFromRow(row: WorkspaceCommandRow): CommandForm {
  return {
    name: row.name,
    command: row.command,
    workingDir: row.workingDir ?? "",
    autoStart: row.autoStart,
    autoRestart: row.autoRestart,
    restartWhenChanged: [...row.restartWhenChanged],
    env: Object.entries(row.env),
  };
}
