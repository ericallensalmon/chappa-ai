// Agent-tool registry store. The settings.ts shape: one
// observable box over the Rust commands (`list_agent_tools` /
// `upsert_agent_tool` / `delete_agent_tool` / `parse_agent_command`), an
// injectable api seam for tests, and the pure helpers the Agents section
// and the rail's "New agent ▸" menu share.
//
// The ONE piece of agent-CLI knowledge in the frontend is the template
// table below, a mirror of `AgentTool::template` Rust-side: the claude
// template ships `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` as a VISIBLE,
// deletable env row (forensics: Claude Code v2.x flips to the alt
// screen after boot, killing scrollback, synthetic marks and search). Spawn
// code — here and Rust-side — knows nothing about claude.

import * as ipc from "./ipc";

export type AgentTool = ipc.AgentToolDto;
export type AgentToolType = ipc.AgentToolType;
export type AgentRuntime = ipc.AgentRuntimeDto;
export type ParsedAgentCommand = ipc.ParsedAgentCommandDto;

/** The tool-type set plus `custom`, in display order. */
export const AGENT_TOOL_TYPES: readonly AgentToolType[] = [
  "claude",
  "opencode",
  "codex",
  "gemini",
  "copilot",
  "kimi",
  "amp",
  "custom",
];

export const CLAUDE_ALT_SCREEN_ENV = "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN";

/** A new tool's defaults (the Rust template, mirrored). */
export function templateFor(toolType: AgentToolType): AgentTool {
  return {
    id: 0,
    name: toolType === "custom" ? "" : toolType,
    tool_type: toolType,
    program: toolType === "custom" ? "" : toolType,
    args: [],
    model: null,
    runtime: { kind: "host" },
    env: toolType === "claude" ? { [CLAUDE_ALT_SCREEN_ENV]: "1" } : {},
    enabled: true,
    max_busy: null,
    // Always tty by default; the modal OFFERS json where a machine mode exists.
    transport: "tty",
  };
}

/** Deep copy so a form can edit without touching the store's object. */
export function cloneTool(t: AgentTool): AgentTool {
  return {
    ...t,
    args: [...t.args],
    env: { ...t.env },
    runtime: { ...t.runtime },
  };
}

/** One line for the list row: `host` or `docker: C (-u U, -w W)`. */
export function runtimeSummary(runtime: AgentRuntime): string {
  if (runtime.kind === "host") return "host";
  const parts: string[] = [];
  if (runtime.user) parts.push(`-u ${runtime.user}`);
  if (runtime.workdir) parts.push(`-w ${runtime.workdir}`);
  if (runtime.tty === false) parts.push("no tty");
  return `docker: ${runtime.container}${parts.length ? ` (${parts.join(", ")})` : ""}`;
}

/** The container a tool names, or null for Host. */
export function toolContainer(tool: AgentTool): string | null {
  return tool.runtime.kind === "docker_exec" ? tool.runtime.container : null;
}

/** The IPC seam. Tests inject a fake. */
export type AgentToolsListing = ipc.AgentToolsListingDto;

export interface AgentToolsApi {
  listAgentTools(): Promise<AgentToolsListing>;
  upsertAgentTool(tool: AgentTool): Promise<AgentTool>;
  deleteAgentTool(id: number): Promise<void>;
  parseAgentCommand(line: string): Promise<ParsedAgentCommand>;
}

/** The real adapter. Outside Tauri (plain browser, vitest) the registry is
 *  empty and edits stay in memory. */
export const tauriAgentToolsApi: AgentToolsApi = {
  listAgentTools: () =>
    ipc.inTauri() ? ipc.listAgentTools() : Promise.resolve({ tools: [], machine_mode_types: [] }),
  upsertAgentTool: (tool) =>
    ipc.inTauri() ? ipc.upsertAgentTool(tool) : Promise.resolve({ ...tool, id: tool.id || Date.now() }),
  deleteAgentTool: (id) => (ipc.inTauri() ? ipc.deleteAgentTool(id) : Promise.resolve()),
  parseAgentCommand: (line) =>
    ipc.inTauri()
      ? ipc.parseAgentCommand(line)
      : Promise.resolve({
          tool: { ...templateFor("custom"), program: line.trim(), transport: "tty" },
          docker_detected: false,
          warnings: ["parser unavailable outside Tauri"],
        }),
};

/** The observable box. `load()` at boot / on pane open; every mutation
 *  round-trips through Rust and adopts what it returns. */
export class AgentToolsStore {
  private tools: AgentTool[] = [];
  /** The tool types with a machine mode, as Rust reports them
   *  (`list_agent_tools.machine_mode_types`) — no frontend copy. */
  private machineModeTypes: AgentToolType[] = [];
  private loaded = false;
  private readonly subs = new Set<(tools: AgentTool[]) => void>();

  constructor(private readonly api: AgentToolsApi = tauriAgentToolsApi) {}

  /** A copy of the current list. */
  get(): AgentTool[] {
    return this.tools.map(cloneTool);
  }

  /** The tools the "New agent ▸" menu offers: enabled only. */
  enabled(): AgentTool[] {
    return this.get().filter((t) => t.enabled);
  }

  byId(id: number): AgentTool | undefined {
    const t = this.tools.find((t) => t.id === id);
    return t ? cloneTool(t) : undefined;
  }

  /** Whether the Transport picker may offer `json` for `toolType`
   *  (Rust's `agent_json::machine_mode`, shipped with the listing). */
  hasMachineMode(toolType: AgentToolType): boolean {
    return this.machineModeTypes.includes(toolType);
  }

  async load(): Promise<AgentTool[]> {
    try {
      const listing = await this.api.listAgentTools();
      this.tools = listing.tools;
      this.machineModeTypes = [...listing.machine_mode_types];
      this.loaded = true;
    } catch {
      this.tools = [];
    }
    this.notify();
    return this.get();
  }

  /** Insert (`id === 0`) or replace. Rejects with Rust's message. */
  async upsert(tool: AgentTool): Promise<AgentTool> {
    if (!this.loaded) await this.load();
    const stored = await this.api.upsertAgentTool(tool);
    const idx = this.tools.findIndex((t) => t.id === stored.id);
    if (idx >= 0) this.tools[idx] = stored;
    else this.tools.push(stored);
    this.notify();
    return cloneTool(stored);
  }

  /** Rejects — with the message NAMING the live processes — when a live
   *  agent references the tool (the Rust refusal). */
  async delete(id: number): Promise<void> {
    await this.api.deleteAgentTool(id);
    this.tools = this.tools.filter((t) => t.id !== id);
    this.notify();
  }

  parse(line: string): Promise<ParsedAgentCommand> {
    return this.api.parseAgentCommand(line);
  }

  subscribe(fn: (tools: AgentTool[]) => void): () => void {
    this.subs.add(fn);
    return () => {
      this.subs.delete(fn);
    };
  }

  private notify(): void {
    const snapshot = this.get();
    for (const fn of [...this.subs]) fn(snapshot);
  }
}

/** The app-wide instance. */
export const agentToolsStore = new AgentToolsStore();
