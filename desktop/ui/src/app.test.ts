// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App, GlLru, agentForest, railReducer, type ActiveRow, type RailEntry, type RailStatus, PRE_ADOPT_EVENTS_PER_ID, PRE_ADOPT_EVENT_IDS_MAX } from "./app";
import type { NotifyCenter } from "./notify_center";
import type { TerminalApi } from "./term/panel";
import { emptyAgentTool, frame, stubSettings } from "./term/test-utils";
import { AgentToolsStore, type AgentToolsApi } from "./agent_tools";
import type * as ipc from "./ipc";
import type { CommandForm } from "./dialog";

function makeEntry(id: number, over: Partial<RailEntry> = {}): RailEntry {
  return {
    id,
    panel: undefined as unknown as RailEntry["panel"],
    host: document.createElement("div"),
    name: `term-${id}`,
    status: "starting",
    exitCode: null,
    title: null,
    activity: false,
    bell: false,
    attention: null,
    ...over,
  };
}

describe("railReducer", () => {
  it("applies status transitions with their exit codes", () => {
    const entry = makeEntry(1);
    expect(railReducer(entry, { type: "status", status: "running", exitCode: null }).status).toBe("running");
    // exited-0 vs failed (nonzero) — the rail badge keys off the code.
    const exited = railReducer(entry, { type: "status", status: "exited", exitCode: 0 });
    expect(exited.status).toBe("exited");
    expect(exited.exitCode).toBe(0);
    const failed = railReducer(entry, { type: "status", status: "failed", exitCode: 2 });
    expect(failed.status).toBe("failed");
    expect(failed.exitCode).toBe(2);
    const stopped = railReducer(entry, { type: "status", status: "stopped", exitCode: null });
    expect(stopped.status).toBe("stopped");
  });

  it("tracks titles, activity and bell until activation clears them", () => {
    const entry = makeEntry(1);
    expect(railReducer(entry, { type: "title", title: "vim" }).title).toBe("vim");
    const busy = railReducer(railReducer(entry, { type: "activity" }), { type: "bell" });
    expect(busy.activity).toBe(true);
    expect(busy.bell).toBe(true);
    const cleared = railReducer(busy, { type: "activate" });
    expect(cleared.activity).toBe(false);
    expect(cleared.bell).toBe(false);
    // Immutable: the original entry is untouched.
    expect(entry.activity).toBe(false);
    expect(entry.bell).toBe(false);
  });
});

describe("GlLru", () => {
  it("keeps at most `cap` live contexts and evicts the least-recently-used", () => {
    const lru = new GlLru(2);
    expect(lru.acquire(1)).toBeNull();
    expect(lru.acquire(2)).toBeNull();
    expect(lru.size).toBe(2);
    // A third context evicts #1 (oldest), never the just-acquired #3.
    expect(lru.acquire(3)).toBe(1);
    expect(lru.size).toBe(2);
    // Touching #2 again (activation) makes it MRU; #3 is now the victim.
    expect(lru.acquire(2)).toBeNull();
    expect(lru.size).toBe(2);
    expect(lru.acquire(4)).toBe(3);
  });

  it("release forgets an id so it can be re-acquired without eviction", () => {
    const lru = new GlLru(2);
    lru.acquire(1);
    lru.acquire(2);
    lru.release(1);
    expect(lru.size).toBe(1);
    expect(lru.acquire(1)).toBeNull();
    expect(lru.size).toBe(2);
  });

  it("releasing the just-evicted id is a harmless no-op", () => {
    const lru = new GlLru(1);
    lru.acquire(1);
    expect(lru.acquire(2)).toBe(1);
    lru.release(1); // already gone
    expect(lru.size).toBe(1);
  });
});

// --- App integration (fake api) --------------------------------------------

function stubApi(): {
  api: TerminalApi;
  ids: () => number[];
  onFrame: (id: number) => ((buf: ArrayBuffer) => void) | null;
  list: () => { id: number; status: string; exit_code: number | null }[];
  /** OS notifications the app authorized. */
  notified: Array<[string, string]>;
  /** set_notification_level args. */
  levels: Array<[number, string | null, string | null]>;
} {
  const ids: number[] = [];
  const frames = new Map<number, (buf: ArrayBuffer) => void>();
  let next = 1;
  const statuses = new Map<number, string>();
  const notified: Array<[string, string]> = [];
  const levels: Array<[number, string | null, string | null]> = [];
  const api: TerminalApi = {
    createTerminal: vi.fn(async (opts) => {
      const id = next++;
      ids.push(id);
      frames.set(id, opts.onFrame);
      statuses.set(id, "starting");
      return id;
    }),
    writeKey: vi.fn(async () => {}),
    paste: vi.fn(async () => {}),
    mouse: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    scroll: vi.fn(async () => {}),
    setDisplayOffset: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    requestFull: vi.fn(async () => {}),
    selection: vi.fn(async () => {}),
    copySelection: vi.fn(async () => null),
    search: vi.fn(async () => {}),
    searchNav: vi.fn(async () => {}),
    closeTerminal: vi.fn(async () => null),
    debugStats: vi.fn(async () => null),
    listTerminals: vi.fn(async () =>
      ids.map((id) => ({ id, name: "shell", status: statuses.get(id) ?? "starting", exit_code: null, cols: 80, rows: 24, seq: 0 })),
    ),
    attachTerminal: vi.fn(async (id) => ({
      id,
      name: "shell",
      status: statuses.get(id) ?? "running",
      exit_code: null,
      cols: 80,
      rows: 24,
      seq: 1,
      kind: "terminal" as const,
      project_id: null,
      agent_tool_id: null,
    })),
    listProjects: vi.fn(async () => []),
    addProject: vi.fn(async () => ({ id: 1, name: "p", path: "/p", icon: null, notificationLevel: null })),
    renameProject: vi.fn(async () => {}),
    removeProject: vi.fn(async () => {}),
    pickDirectory: vi.fn(async () => null),
    openProject: vi.fn(async () => ({
      project: { id: 1, name: "p", path: "/p", icon: null, notificationLevel: null },
      trustPending: false,
      trustCommands: [],
      processes: [],
    })),
    confirmProjectTrust: vi.fn(async () => {}),
    listProjectProcesses: vi.fn(async () => []),
    startProjectProcess: vi.fn(async () => 42),
    stopProjectProcess: vi.fn(async () => {}),
    restartProjectProcess: vi.fn(async () => 42),
    setNotificationLevel: vi.fn(async (projectId: number, processName: string | null, level: string | null) => {
      levels.push([projectId, processName, level]);
    }),
    osNotify: vi.fn(async (title: string, body: string) => {
      notified.push([title, body]);
    }),
    saveProjectProcess: vi.fn(async () => []),
    deleteProjectProcess: vi.fn(async () => []),
    duplicateProjectProcesses: vi.fn(async () => []),
    setProcessFavorite: vi.fn(async () => {}),
    setProcessAutoRename: vi.fn(async () => {}),
    // Workspace commands.
    listWorkspaceCommands: vi.fn(async () => []),
    saveWorkspaceCommand: vi.fn(async () => []),
    deleteWorkspaceCommand: vi.fn(async () => []),
    startWorkspaceCommand: vi.fn(async () => 42),
    stopWorkspaceCommand: vi.fn(async () => {}),
    restartWorkspaceCommand: vi.fn(async () => 42),
    listAgentTools: vi.fn(async () => ({ tools: [], machine_mode_types: ["claude", "opencode"] as const })),
    upsertAgentTool: vi.fn(async (t) => t),
    deleteAgentTool: vi.fn(async () => {}),
    parseAgentCommand: vi.fn(async () => ({ tool: emptyAgentTool(), docker_detected: false, warnings: [] })),
    // A fake spawn — the id comes from the same counter as shells,
    // the agent block echoes the tool it was asked for.
    spawnAgent: vi.fn(async (req: ipc.SpawnAgentRequestDto) => {
      const id = next++;
      ids.push(id);
      statuses.set(id, "starting");
      return fakeSpawnResponse(id, req);
    }),
    // The json transport's message + event-ring reads.
    sendAgentInput: vi.fn(async (_id: number, _text: string) => ({ delivered: true, waited_ms: 12, seq_before: 0, turn_started_seq: 1 })),
    getAgentEvents: vi.fn(async () => []),
  };
  return {
    api,
    ids: () => ids,
    onFrame: (id: number) => frames.get(id) ?? null,
    list: () => [],
    notified,
    levels,
  };
}

/** The docker-exec opencode tool the agent tests register. */
function dockerTool(id = 3, over: Partial<ipc.AgentToolDto> = {}): ipc.AgentToolDto {
  return {
    id,
    name: "worker",
    tool_type: "opencode",
    program: "opencode",
    args: ["-m", "gateway/model-fast"],
    model: "model-fast",
    runtime: { kind: "docker_exec", container: "dev-worker", user: "dev", workdir: "/workspace/app", tty: true, max_busy_in_container: 1 },
    env: {},
    enabled: true,
    max_busy: null,
    transport: "tty",
    ...over,
  };
}

function fakeSpawnResponse(id: number, req: ipc.SpawnAgentRequestDto): ipc.SpawnAgentResponseDto {
  const tool = dockerTool(req.agent_tool_id);
  return {
    process_id: id,
    term_id: id,
    name: req.name ?? tool.name,
    agent_instructions: `You are running as chappa-ai process ${id}.`,
    container: { status: "running", started_at: "2026-08-28T10:00:00Z", uptime_s: 120 },
    agent: {
      tool_id: tool.id,
      tool_type: tool.tool_type,
      model: tool.model,
      runtime: tool.runtime,
      project_id: req.project_id ?? null,
      parent_process_id: null,
      spawned_at_ms: 0,
      spawn_uuid: "u",
      nspid: "pid-file",
      container: null,
      bridge: "ok",
      transport: tool.transport,
      awaiting_input: false,
    },
  };
}

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe("App", () => {
  it("mounts BLANK — no shell is auto-spawned", async () => {
    const { api, ids } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();

    // Blank start: mount spawns nothing — empty stack + rail
    // affordances. The workspace COMMANDS header still renders, and the
    // workspace TERMINALS section at zero shows just its header (no row).
    expect(ids()).toEqual([]);
    expect(root.querySelectorAll(".chappa-rail-item").length).toBe(0);
    // The root MUST carry the layout class — without it .chappa-stack collapses
    // to height 0 and every panel is permanently "hidden" (regression).
    expect(root.classList.contains("chappa-app")).toBe(true);
    expect(
      root.querySelector(".chappa-workspace-cmds .chappa-subsection-label")?.textContent,
    ).toBe("COMMANDS");

    // An explicit spawn still works.
    await app.newShell();
    await FLUSH();
    expect(ids()).toEqual([1]);
    expect(root.querySelectorAll(".chappa-rail-item").length).toBe(1);
    expect(root.querySelector(".chappa-rail-name")?.textContent).toBe("shell");
    panelDispose(app);
  });

  it("opens more shells on request and switches the active panel", async () => {
    const { api, ids } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell();
    await app.newShell();
    await FLUSH();

    expect(ids()).toEqual([1, 2]);
    const rows = root.querySelectorAll(".chappa-rail-item");
    expect(rows.length).toBe(2);
    // The newest panel is active; the first one is hidden.
    expect(rows[1].classList.contains("active")).toBe(true);
    const hosts = root.querySelectorAll<HTMLElement>(".chappa-panel-host");
    expect(hosts[1].classList.contains("active")).toBe(true);
    expect(hosts[0].classList.contains("active")).toBe(false);
    panelDispose(app);
  });

  it("closes a panel and activates the remaining one", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const confirmed: string[] = [];
    const app = new App({
      root,
      api,
      confirm: async (name) => {
        confirmed.push(name);
        return true;
      },
    });
    await app.mount();
    await app.newShell();
    await app.newShell();
    await FLUSH();

    // Close the active (second) panel: the first becomes active.
    void app.closePanel(2);
    // The confirm is invoked synchronously — only the ANSWER is async.
    expect(confirmed).toEqual(["shell"]);
    await FLUSH();
    const rows = root.querySelectorAll(".chappa-rail-item");
    expect(rows.length).toBe(1);
    expect(rows[0].classList.contains("active")).toBe(true);
    expect(api.closeTerminal).toHaveBeenCalledWith(2);
    panelDispose(app);
  });

  it("routes an OS file drop to the ACTIVE panel as one quoted paste", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell();
    await app.newShell(); // id 2 becomes active
    await FLUSH();

    // Production route: wireEvents feeds this from `tauri://drag-drop`
    // (HTML5 drops never carry OS paths under Tauri's interception).
    app.handleFileDrop(["C:\\tmp\\a b.txt", "C:\\tmp\\c.txt"]);
    expect(api.paste).toHaveBeenCalledTimes(1);
    expect(api.paste).toHaveBeenCalledWith(2, '"C:\\\\tmp\\\\a b.txt" "C:\\\\tmp\\\\c.txt"');

    // Empty drops are ignored.
    app.handleFileDrop([]);
    expect(api.paste).toHaveBeenCalledTimes(1);
    panelDispose(app);
  });

  it("a refused confirmation keeps the running panel", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell();

    void app.closePanel(1);
    await FLUSH();
    expect(root.querySelectorAll(".chappa-rail-item").length).toBe(1);
    expect(api.closeTerminal).not.toHaveBeenCalled();
    panelDispose(app);
  });

  it("a second close press does not stack a second confirmation", async () => {
    // The confirm is async now (an in-app modal), so a held Ctrl+Shift+W or a
    // double-click on × would otherwise queue a dialog per press.
    const { api } = stubApi();
    const root = document.createElement("div");
    let asked = 0;
    let release: (ok: boolean) => void = () => {};
    const app = new App({
      root,
      api,
      confirm: () =>
        new Promise<boolean>((resolve) => {
          asked += 1;
          release = resolve;
        }),
    });
    await app.mount();
    await app.newShell();

    void app.closePanel(1);
    void app.closePanel(1);
    await FLUSH();
    expect(asked).toBe(1);
    release(true);
    await FLUSH();
    expect(api.closeTerminal).toHaveBeenCalledTimes(1);
    panelDispose(app);
  });

  it("defaults to the in-app modal, never the webview's window.confirm", async () => {
    const { api } = stubApi();
    const nativeConfirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const root = document.createElement("div");
    const app = new App({ root, api });
    await app.mount();
    await app.newShell();

    const closing = app.closePanel(1);
    await FLUSH();
    expect(nativeConfirm).not.toHaveBeenCalled();
    const dialog = document.querySelector<HTMLElement>(".chappa-dialog-backdrop")!;
    expect(dialog).not.toBeNull();
    expect(dialog.textContent).toContain("Close terminal");
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await closing;
    expect(api.closeTerminal).toHaveBeenCalledWith(1);
    // The modal cleans itself up on resolve.
    expect(document.querySelector(".chappa-dialog-backdrop")).toBeNull();
    nativeConfirm.mockRestore();
    panelDispose(app);
  });
});

function panelDispose(app: App): void {
  // Teardown without tripping the confirm dialogs: close all directly.
  (app as unknown as { entries: Map<number, RailEntry> }).entries.forEach((e) => e.panel.dispose());
}

// --- project view --------------------------------------------------

/** A TerminalApi stub with controllable project state + call recording. */
function projectApi(over: {
  projects?: ipc.ProjectInfoDto[];
  openResult?: ipc.OpenProjectResult;
  /** What the fake native picker answers (null = cancelled). */
  picked?: string | null;
} = {}): {
  api: TerminalApi;
  calls: {
    openProject: number[];
    startProcess: Array<[number, string]>;
    stopProcess: Array<[number, string]>;
    restartProcess: Array<[number, string]>;
    confirmTrust: Array<[number, boolean]>;
    /** the EXACT wire args of add_project (path + explicit name; null = the
     *  modal's untouched-name answer). */
    addProject: Array<[string, string | null | undefined]>;
    /** [id, new name] of every rename_project call. */
    renameProject: Array<[number, string]>;
    /** the switcher row's 🗑 (2026-08-31) */
    removeProject: number[];
    /** [title, start] of every pick_directory call. */
    pickDirectory: Array<[string | undefined, string | undefined]>;
    /** [project, process, level] of every set-level call. */
    setLevel: Array<[number, string | null, string | null]>;
    osNotify: Array<[string, string]>;
    /** [projectId, originalName, def] of every save_project_process. */
    saveProcess: Array<[number, string | null, ipc.ProcessDefDto]>;
    deleteProcess: Array<[number, string]>;
    /** [sourceId, names, targetId]. */
    duplicate: Array<[number, string[], number]>;
    favorite: Array<[number, string, boolean]>;
    autoRename: Array<[number, string, boolean]>;
    /** Every project id an import was requested for. */
  };
} {
  const projects = over.projects ?? [
    { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
  ];
  const openResult = over.openResult ?? {
    project: projects[0],
    trustPending: false,
    trustCommands: [],
    processes: [
      {
        name: "import + summarize on open",
        command: "py app/summarize.py",
        status: "stopped",
        autoStart: true,
        autoRestart: false,
        restartWhenChanged: [],
        termId: null,
        exitCode: null,
        notificationLevel: null,
        workingDir: null,
        env: {},
        favorite: false,
        disableAutoRename: false,
      },
      {
        name: "chappa-ai tui",
        command: "py app/tui.py",
        status: "stopped",
        autoStart: false,
        autoRestart: false,
        restartWhenChanged: [],
        termId: null,
        exitCode: null,
        notificationLevel: null,
        workingDir: null,
        env: {},
        favorite: false,
        disableAutoRename: false,
      },
    ],
  };
  const calls = {
    openProject: [] as number[],
    startProcess: [] as Array<[number, string]>,
    stopProcess: [] as Array<[number, string]>,
    restartProcess: [] as Array<[number, string]>,
    confirmTrust: [] as Array<[number, boolean]>,
    addProject: [] as Array<[string, string | null | undefined]>,
    renameProject: [] as Array<[number, string]>,
    removeProject: [] as number[],
    pickDirectory: [] as Array<[string | undefined, string | undefined]>,
    setLevel: [] as Array<[number, string | null, string | null]>,
    osNotify: [] as Array<[string, string]>,
    saveProcess: [] as Array<[number, string | null, ipc.ProcessDefDto]>,
    deleteProcess: [] as Array<[number, string]>,
    duplicate: [] as Array<[number, string[], number]>,
    favorite: [] as Array<[number, string, boolean]>,
    autoRename: [] as Array<[number, string, boolean]>,
  };
  let next = 1;
  const api: TerminalApi = {
    createTerminal: vi.fn(async () => 900),
    writeKey: vi.fn(async () => {}),
    paste: vi.fn(async () => {}),
    mouse: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    scroll: vi.fn(async () => {}),
    setDisplayOffset: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    requestFull: vi.fn(async () => {}),
    selection: vi.fn(async () => {}),
    copySelection: vi.fn(async () => null),
    search: vi.fn(async () => {}),
    searchNav: vi.fn(async () => {}),
    closeTerminal: vi.fn(async () => null),
    debugStats: vi.fn(async () => null),
    listTerminals: vi.fn(async () => []),
    attachTerminal: vi.fn(async (id: number) => ({
      id,
      name: "adopted",
      status: "running",
      exit_code: null,
      cols: 80,
      rows: 24,
      seq: 1,
      kind: "terminal" as const,
      project_id: null,
      agent_tool_id: null,
    })),
    listProjects: vi.fn(async () => projects),
    addProject: vi.fn(async (path: string, name?: string | null) => {
      calls.addProject.push([path, name]);
      return { id: 2, name: name ?? "other", path, icon: null, notificationLevel: null };
    }),
    renameProject: vi.fn(async (id: number, name: string) => {
      calls.renameProject.push([id, name]);
    }),
    removeProject: vi.fn(async (id: number) => {
      calls.removeProject.push(id);
    }),
    pickDirectory: vi.fn(async (title?: string, start?: string) => {
      calls.pickDirectory.push([title, start]);
      return over.picked ?? null;
    }),
    openProject: vi.fn(async (id: number) => {
      calls.openProject.push(id);
      return openResult;
    }),
    confirmProjectTrust: vi.fn(async (id: number, run: boolean) => {
      calls.confirmTrust.push([id, run]);
    }),
    listProjectProcesses: vi.fn(async () => openResult.processes),
    startProjectProcess: vi.fn(async (id: number, name: string) => {
      calls.startProcess.push([id, name]);
      return next++;
    }),
    stopProjectProcess: vi.fn(async (id: number, name: string) => {
      calls.stopProcess.push([id, name]);
    }),
    restartProjectProcess: vi.fn(async (id: number, name: string) => {
      calls.restartProcess.push([id, name]);
      return next++;
    }),
    setNotificationLevel: vi.fn(
      async (projectId: number, processName: string | null, level: string | null) => {
        calls.setLevel.push([projectId, processName, level]);
      },
    ),
    osNotify: vi.fn(async (title: string, body: string) => {
      calls.osNotify.push([title, body]);
    }),
    // The fake "file" is `openResult.processes`; the mutation stubs
    // answer the list the way Rust does (rows after the reload).
    saveProjectProcess: vi.fn(async (id: number, originalName: string | null, def: ipc.ProcessDefDto) => {
      calls.saveProcess.push([id, originalName, def]);
      const row: ipc.ProjectProcessDto = {
        name: def.name,
        command: def.command,
        status: "stopped",
        autoStart: def.autoStart,
        autoRestart: def.autoRestart,
        restartWhenChanged: def.restartWhenChanged,
        termId: null,
        exitCode: null,
        notificationLevel: null,
        workingDir: def.workingDir,
        env: Object.fromEntries(def.env),
        favorite: false,
        disableAutoRename: false,
      };
      const idx = openResult.processes.findIndex((p) => p.name === originalName);
      if (idx >= 0) openResult.processes[idx] = { ...openResult.processes[idx], ...row };
      else openResult.processes.push(row);
      return openResult.processes;
    }),
    deleteProjectProcess: vi.fn(async (id: number, name: string) => {
      calls.deleteProcess.push([id, name]);
      openResult.processes = openResult.processes.filter((p) => p.name !== name);
      return openResult.processes;
    }),
    duplicateProjectProcesses: vi.fn(async (sourceId: number, names: string[], targetId: number) => {
      calls.duplicate.push([sourceId, names, targetId]);
      return [];
    }),
    setProcessFavorite: vi.fn(async (projectId: number, processName: string, favorite: boolean) => {
      calls.favorite.push([projectId, processName, favorite]);
    }),
    setProcessAutoRename: vi.fn(async (projectId: number, processName: string, disabled: boolean) => {
      calls.autoRename.push([projectId, processName, disabled]);
    }),
    listWorkspaceCommands: vi.fn(async () => []),
    saveWorkspaceCommand: vi.fn(async () => []),
    deleteWorkspaceCommand: vi.fn(async () => []),
    startWorkspaceCommand: vi.fn(async () => 900),
    stopWorkspaceCommand: vi.fn(async () => {}),
    restartWorkspaceCommand: vi.fn(async () => 900),
    listAgentTools: vi.fn(async () => ({ tools: [], machine_mode_types: ["claude", "opencode"] as const })),
    upsertAgentTool: vi.fn(async (t) => t),
    deleteAgentTool: vi.fn(async () => {}),
    parseAgentCommand: vi.fn(async () => ({ tool: emptyAgentTool(), docker_detected: false, warnings: [] })),
    spawnAgent: vi.fn(async (req: ipc.SpawnAgentRequestDto) => fakeSpawnResponse(800 + next++, req)),
    sendAgentInput: vi.fn(async () => ({ delivered: true, waited_ms: 0, seq_before: 0 })),
    getAgentEvents: vi.fn(async () => []),
  };
  return { api, calls };
}

function click(el: Element): void {
  el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
  el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
}

function projectRows(root: HTMLElement): NodeListOf<Element> {
  return root.querySelectorAll(".chappa-process-row");
}

function rowButton(row: Element, title: string): HTMLButtonElement {
  return [...row.querySelectorAll<HTMLButtonElement>(".chappa-process-btn")].find(
    (b) => b.title === title,
  )!;
}

describe("App projects", () => {
  it("mounts a switcher fed by list_projects", async () => {
    const { api } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    expect(api.listProjects).toHaveBeenCalledTimes(1);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("Project…");
    panelDispose(app);
  });

  it("the switcher opens via mousedown→click and a project option opens it", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();

    // mousedown → click on the switcher button: the menu (with one project
    // option + the add affordance) appears.
    const switcher = root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!;
    expect((root.querySelector(".chappa-project-menu") as HTMLElement).style.display).toBe("none");
    click(switcher);
    const menu = root.querySelector<HTMLElement>(".chappa-project-menu")!;
    expect(menu.style.display).toBe("block");
    const options = root.querySelectorAll(".chappa-project-option");
    expect(options.length).toBe(1);
    expect(options[0].textContent).toBe("chappa-ai");

    // mousedown → click on the option: open_project runs and process rows land.
    click(options[0]);
    await FLUSH();
    expect(calls.openProject).toEqual([1]);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    expect(projectRows(root).length).toBe(2);
    expect(root.querySelector(".chappa-project-name")?.textContent).toBe("chappa-ai");
    panelDispose(app);
  });

  it("renders process rows with AUTO/YML badges, command, and the status pill", async () => {
    const { api } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const rows = projectRows(root);
    expect(rows.length).toBe(2);
    const first = rows[0];
    expect(first.querySelector(".chappa-process-name")?.textContent).toBe("import + summarize on open");
    expect(first.querySelector(".chappa-process-cmd")?.textContent).toBe("py app/summarize.py");
    // AUTO badge only on auto_start; no YML badge — the project file is
    // unconditional, so there is nothing for a badge to distinguish.
    expect(first.querySelector(".chappa-process-badge.auto")).not.toBeNull();
    expect(rows[1].querySelector(".chappa-process-badge.auto")).toBeNull();
    expect(first.querySelectorAll(".chappa-process-badge").length).toBe(1);
    // The project is trusted, so the auto_start process already started
    // (pill: starting); the non-auto process stays stopped.
    expect(first.querySelector(".chappa-status-pill")?.textContent).toBe("starting");
    expect(rows[1].querySelector(".chappa-status-pill")?.textContent).toBe("stopped");
    panelDispose(app);
  });

  it("the Edit pane has NO sync switch and always shows the project-file row", async () => {
    const { api } = projectApi({
      projects: [{ id: 1, name: "bare", path: "/p/bare", icon: null, notificationLevel: null }],
    });
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    // AUTO badge on the auto_start command; the YML badge (which used to
    // distinguish Synced from native-only) is gone — every project is now
    // file-backed.
    const first = projectRows(root)[0];
    expect(first.querySelector(".chappa-process-badge.auto")).not.toBeNull();
    expect(first.querySelectorAll(".chappa-process-badge").length).toBe(1);

    // The Edit pane: no sync toggle (removed it) and the
    // project-file row (chappa.yml) is unconditional, with its load-state chip.
    app.openEditPane(1);
    expect(app.editPaneElement!.querySelector('[data-setting="configSync"]')).toBeNull();
    const paneText = app.editPaneElement!.textContent!;
    expect(paneText).toContain("CHAPPA.YML");
    expect(paneText).toContain("chappa.yml");
    panelDispose(app);
  });

  it("trust gate: Skip confirms and starts nothing", async () => {
    const { api, calls } = projectApi({
      openResult: {
        project: { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
        trustPending: true,
        trustCommands: ["py app/summarize.py"],
        processes: [
          { name: "s", command: "py app/summarize.py", status: "stopped", autoStart: true, autoRestart: false, restartWhenChanged: [], termId: null, exitCode: null, notificationLevel: null, workingDir: null, env: {}, favorite: false, disableAutoRename: false },
        ],
      },
    });
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    expect(calls.confirmTrust).toEqual([[1, false]]);
    expect(calls.startProcess).toEqual([]);
    panelDispose(app);
  });

  it("trust gate: an EMPTY command list is trusted silently, no dialog", async () => {
    // A dialog asking to run nothing is noise (found on the host:
    // adding a project with a bare chappa.yml popped an empty Run/Cancel).
    const shown: string[][] = [];
    const { api, calls } = projectApi({
      openResult: {
        project: { id: 1, name: "bare", path: "/p/bare", icon: null, notificationLevel: null },
        trustPending: true,
        trustCommands: [],
        processes: [],
      },
    });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async (commands) => {
        shown.push(commands);
        return true;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    expect(shown).toEqual([]); // never asked
    expect(calls.confirmTrust).toEqual([[1, true]]); // hash recorded anyway
    expect(calls.startProcess).toEqual([]);
    panelDispose(app);
  });

  it("trust gate: Run lists the auto-start commands, records the hash, and starts them", async () => {
    const shown: string[][] = [];
    const { api, calls } = projectApi({
      openResult: {
        project: { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
        trustPending: true,
        trustCommands: ["py app/summarize.py", "py app/design_sync.py"],
        processes: [
          { name: "import + summarize on open", command: "py app/summarize.py", status: "stopped", autoStart: true, autoRestart: false, restartWhenChanged: [], termId: null, exitCode: null, notificationLevel: null, workingDir: null, env: {}, favorite: false, disableAutoRename: false },
          { name: "chappa-ai tui", command: "py app/tui.py", status: "stopped", autoStart: false, autoRestart: false, restartWhenChanged: [], termId: null, exitCode: null, notificationLevel: null, workingDir: null, env: {}, favorite: false, disableAutoRename: false },
        ],
      },
    });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async (commands) => {
        shown.push(commands);
        return true;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    // The confirm listed exactly the auto-start commands.
    expect(shown).toEqual([["py app/summarize.py", "py app/design_sync.py"]]);
    expect(calls.confirmTrust).toEqual([[1, true]]);
    // Only the auto_start process spawned (chappa-ai tui is not auto_start).
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);
    panelDispose(app);
  });

  it("process-row Start/Stop/Restart buttons work via mousedown→click", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const rows = projectRows(root);
    const first = rows[0];

    // Start (disabled while stopped? no — start is enabled when stopped).
    click(rowButton(first, "Start"));
    await FLUSH();
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);
    // The process is now live: the pill says starting and Stop is enabled.
    expect(first.querySelector(".chappa-status-pill")?.textContent).toBe("starting");
    expect((rowButton(first, "Stop") as HTMLButtonElement).disabled).toBe(false);

    // Stop: the lifecycle command runs and the row returns to stopped.
    click(rowButton(first, "Stop"));
    await FLUSH();
    expect(calls.stopProcess).toEqual([[1, "import + summarize on open"]]);
    expect(first.querySelector(".chappa-status-pill")?.textContent).toBe("stopped");

    // Restart: stop-then-start.
    click(rowButton(first, "Restart"));
    await FLUSH();
    expect(calls.stopProcess).toEqual([
      [1, "import + summarize on open"],
      [1, "import + summarize on open"],
    ]);
    expect(calls.startProcess.length).toBe(2);
    panelDispose(app);
  });

  it("re-Start after a natural exit spawns fresh (dead panel must not eat the start)", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const first = projectRows(root)[0];
    click(rowButton(first, "Start"));
    await FLUSH();
    expect(calls.startProcess.length).toBe(1);

    // The child runs and exits cleanly. The panel is kept (scrollback), but
    // its terminal id and frames channel are spent.
    const appAny = app as unknown as { applyProcessStatus: (p: ipc.ProjectProcessStatusEvent) => void };
    appAny.applyProcessStatus({ project_id: 1, name: "import + summarize on open", status: "running", exit_code: null, term_id: 42 });
    appAny.applyProcessStatus({ project_id: 1, name: "import + summarize on open", status: "exited", exit_code: 0, term_id: 42 });
    expect(first.querySelector(".chappa-status-pill")?.textContent).toBe("exited");

    // Start again via the full event sequence. panel.start() is idempotent,
    // so a retained dead panel would return its old id and the spawn would
    // never reach Rust (the stuck-on-"starting" regression). The re-start
    // MUST hit the api a second time — from a fresh panel.
    click(rowButton(first, "Start"));
    await FLUSH();
    expect(calls.startProcess.length).toBe(2);
    expect(first.querySelector(".chappa-status-pill")?.textContent).toBe("starting");
    panelDispose(app);
  });

  it("clicking a process card focuses its panel via mousedown→click (the way back)", async () => {
    const { api } = projectApi();
    const root = document.createElement("div");
    // Attached: the refocus assertion needs real jsdom focus, which is a
    // no-op on detached subtrees.
    document.body.appendChild(root);
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const rows = projectRows(root);
    click(rowButton(rows[0], "Start"));
    await FLUSH();
    click(rowButton(rows[1], "Start"));
    await FLUSH();
    // Each start auto-activates its panel — the SECOND row holds the marker.
    expect(rows[1].classList.contains("active")).toBe(true);
    expect(rows[0].classList.contains("active")).toBe(false);

    // mousedown → click on the first card's info area: the way back to a
    // panel after switching away (there was none).
    click(rows[0].querySelector(".chappa-process-info")!);
    expect(rows[0].classList.contains("active")).toBe(true);
    expect(rows[1].classList.contains("active")).toBe(false);

    // The row element ITSELF is clickable too — padding and the gaps around
    // the info column must not be dead zones (follow-up).
    click(rows[1]);
    expect(rows[1].classList.contains("active")).toBe(true);
    expect(rows[0].classList.contains("active")).toBe(false);

    // The status pill (side column, not a button) also focuses.
    click(rows[0].querySelector(".chappa-status-pill")!);
    expect(rows[0].classList.contains("active")).toBe(true);

    // Re-clicking the ALREADY-active card gives the terminal's textarea
    // keyboard focus back (the click itself steals DOM focus; without the
    // refocus the pane looks focused but eats no keys — regression).
    (document.activeElement as HTMLElement | null)?.blur?.();
    click(rows[0]);
    const ta = root.querySelector<HTMLTextAreaElement>(".chappa-panel-host.active .chappa-textarea");
    expect(document.activeElement).toBe(ta);

    // Activation ORDER contract: the host must be visible BEFORE
    // setActive(true) runs, because setActive focuses the textarea and
    // focus() inside display:none silently does nothing in a real browser
    // (jsdom focus ignores CSS, so we lock the call order instead).
    const entries = (app as unknown as { entries: Map<number, { panel: { setActive(a: boolean): void }; host: HTMLElement }> }).entries;
    const hidden = [...entries.entries()].find(([id]) => id !== (app as unknown as { activeId: number }).activeId)!;
    const [hiddenId, hiddenEntry] = hidden;
    let hostVisibleAtSetActive: boolean | null = null;
    const origSetActive = hiddenEntry.panel.setActive.bind(hiddenEntry.panel);
    hiddenEntry.panel.setActive = (a: boolean) => {
      if (a) hostVisibleAtSetActive = hiddenEntry.host.classList.contains("active");
      origSetActive(a);
    };
    app.activate(hiddenId);
    expect(hostVisibleAtSetActive).toBe(true);
    panelDispose(app);
    root.remove();
  });

  it("clicking a stopped command fronts its empty state, and its ▶ Start spawns", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const rows = projectRows(root);
    // Run the first command so the stack has a real panel showing.
    click(rowButton(rows[0], "Start"));
    await FLUSH();
    const started = root.querySelectorAll(".chappa-panel-host:not(.chappa-process-empty)");

    // mousedown → click the SECOND (never-started) card: the stopped-command
    // pane fronts instead of the stack staying on command one.
    click(rows[1]);
    const empty = root.querySelector<HTMLElement>(".chappa-process-empty")!;
    expect(empty.classList.contains("active")).toBe(true);
    expect(empty.querySelector(".chappa-process-empty-title")?.textContent).toBe("Process is stopped");
    expect(rows[1].classList.contains("active")).toBe(true);
    // Every real panel deactivated.
    for (const host of started) expect(host.classList.contains("active")).toBe(false);

    // ▶ Start from the pane spawns THAT command; the empty state hides when
    // its panel activates.
    click(empty.querySelector(".chappa-process-empty-start")!);
    await FLUSH();
    expect(calls.startProcess.length).toBe(2);
    expect(calls.startProcess[1][1]).not.toBe(calls.startProcess[0][1]);
    expect(empty.classList.contains("active")).toBe(false);

    // Stop on the FOCUSED command lands on its own stopped pane.
    click(rowButton(rows[1], "Stop"));
    await FLUSH();
    expect(empty.classList.contains("active")).toBe(true);
    expect(rows[1].classList.contains("active")).toBe(true);
    panelDispose(app);
  });

  it("a started process panel is NOT a rail row (it lives in the project section)", async () => {
    const { api } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.newShell(); // blank start: the mount shell
    await app.openProject(1);
    await FLUSH();
    click(rowButton(projectRows(root)[0], "Start"));
    await FLUSH();
    // The mount shell is the only rail row; the process panel is not.
    expect(root.querySelectorAll(".chappa-rail-item").length).toBe(1);
    expect(projectRows(root).length).toBe(2);
    panelDispose(app);
  });

  it("project://process_status drives the row pill in place", async () => {
    const { api } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    const row = projectRows(root)[0];
    const appAny = app as unknown as { applyProcessStatus: (p: ipc.ProjectProcessStatusEvent) => void };

    appAny.applyProcessStatus({ project_id: 1, name: "import + summarize on open", status: "running", exit_code: null, term_id: 42 });
    expect(row.querySelector(".chappa-status-pill")?.textContent).toBe("running");
    expect(row.querySelector(".chappa-status-pill")?.classList.contains("running")).toBe(true);

    // Failed exit flips the pill red.
    appAny.applyProcessStatus({ project_id: 1, name: "import + summarize on open", status: "failed", exit_code: 2, term_id: 42 });
    expect(row.querySelector(".chappa-status-pill")?.textContent).toBe("failed");
    expect(row.querySelector(".chappa-status-pill")?.classList.contains("failed")).toBe(true);
    panelDispose(app);
  });

  it("project://start_requested (control surface) takes the rail-click start path", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    const appAny = app as unknown as { applyStartRequested: (p: ipc.ProjectStartRequestedEvent) => void };

    appAny.applyStartRequested({ project_id: 1, name: "import + summarize on open" });
    await FLUSH();
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);

    // Idempotent on a live row: the second request is the click guard's no-op.
    appAny.applyStartRequested({ project_id: 1, name: "import + summarize on open" });
    await FLUSH();
    expect(calls.startProcess.length).toBe(1);

    // Unknown project / process: ignored, never a throw or a spawn.
    appAny.applyStartRequested({ project_id: 99, name: "import + summarize on open" });
    appAny.applyStartRequested({ project_id: 1, name: "no such process" });
    await FLUSH();
    expect(calls.startProcess.length).toBe(1);
    panelDispose(app);
  });

  it("S/A/P chords act on the project (and the terminal never sees them)", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const key = (k: string): void => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: k, ctrlKey: true, shiftKey: true, bubbles: true, cancelable: true }),
      );
    };
    // S = start auto-starting (only the auto_start process).
    key("s");
    await FLUSH();
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);

    // A = start all (the other process now too).
    key("a");
    await FLUSH();
    expect(calls.startProcess.length).toBe(2);

    // P = stop all.
    key("p");
    await FLUSH();
    expect(calls.stopProcess).toEqual([
      [1, "import + summarize on open"],
      [1, "chappa-ai tui"],
    ]);
    panelDispose(app);
  });

  it("the header S/A/P buttons perform the same actions via mousedown→click", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    const headerBtn = (label: string): HTMLButtonElement =>
      [...root.querySelectorAll<HTMLButtonElement>(".chappa-project-action")].find((b) => b.textContent === label)!;
    click(headerBtn("S"));
    await FLUSH();
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);
    click(headerBtn("A"));
    await FLUSH();
    expect(calls.startProcess.length).toBe(2);
    click(headerBtn("P"));
    await FLUSH();
    expect(calls.stopProcess.length).toBe(2);
    panelDispose(app);
  });

  it("Add project prompts for a path + name and opens the new project", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptAddProject: async () => ({ path: "/p/new", name: "new one" }),
    });
    await app.mount();
    // Open the switcher and mousedown→click the add affordance.
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-add")!);
    await FLUSH();
    expect(calls.addProject).toEqual([["/p/new", "new one"]]);
    expect(calls.openProject).toEqual([2]);
    // The menu closed after the add.
    expect((root.querySelector(".chappa-project-menu") as HTMLElement).style.display).toBe("none");
    panelDispose(app);
  });

  it("adds a folder without chappa.yml directly — no init offer", async () => {
    // add_project accepts a bare directory (no project file, no
    // chappa.yml), gains a file only once a command is added.
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptAddProject: async () => ({ path: "C:\\p\\fresh dir", name: null }),
    });
    await app.mount();
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-add")!);
    await FLUSH();
    // No init round trip; the bare directory registered and opened, and the
    // add-failure surface was never armed.
    expect(calls.addProject).toEqual([["C:\\p\\fresh dir", null]]);
    expect(calls.openProject).toEqual([2]);
    panelDispose(app);
  });

  it("an add failure surfaces a reason (project registration refused)", async () => {
    const shown: string[] = [];
    const { api, calls } = projectApi();
    api.addProject = vi.fn(async () => {
      throw new Error("projects.json is read-only");
    });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptAddProject: async () => ({ path: "/p/fresh", name: null }),
      showError: async (m) => {
        shown.push(m);
      },
    });
    await app.mount();
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-add")!);
    await FLUSH();
    // A registration refusal surfaces through showError (it used to be a bare
    // console.error — "nothing happened") rather than a dead init retry.
    expect(shown.length).toBe(1);
    expect(shown[0]).toContain("projects.json is read-only");
    expect(calls.openProject).toEqual([]);
    panelDispose(app);
  });

  it("switching projects keeps the previous one's processes running", async () => {
    // REWRITTEN when the semantics flipped: "opening a
    // project no longer closes the previous one … Switching =
    // expand/collapse, nothing stops."
    const { api, calls } = projectApi({
      projects: [
        { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
        { id: 2, name: "other", path: "/p/other", icon: null, notificationLevel: null },
      ],
    });
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    await app.openProject(1);
    click(rowButton(projectRows(root)[0], "Start"));
    await FLUSH();
    expect(calls.startProcess).toEqual([[1, "import + summarize on open"]]);

    // Switch to project 2: NO stop of project 1's process (auto-start of
    // project 2's trusted commands is the only new lifecycle traffic).
    await app.openProject(2);
    await FLUSH();
    expect(calls.stopProcess).toEqual([]);
    // Project 2's rows now show; project 1 collapsed to a one-line summary.
    expect(projectRows(root).length).toBe(2);
    const summary = root.querySelector(".chappa-project-summary")!;
    expect(summary.querySelector(".chappa-project-summary-name")?.textContent).toBe("chappa-ai");
    expect(summary.querySelector(".chappa-project-pill")?.textContent).toBe("1/2");
    panelDispose(app);
  });

  it("overlapping opens of the same project reach Rust exactly once", async () => {
    // The `openProjects.has` guard runs before the first await, so two
    // overlapping opens both used to reach `open_project` — whose
    // reset-idempotency would tear down the first call's fresh runtime.
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    const first = app.openProject(1);
    const second = app.openProject(1); // synchronous with the first
    await Promise.all([first, second]);
    await FLUSH();
    expect(calls.openProject).toEqual([1]);
    // The one open landed fully: rows rendered, switcher named.
    expect(projectRows(root).length).toBe(2);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    panelDispose(app);
  });
});

// --- add-project modal + rename ------------------------------------

describe("App add-project flow", () => {
  const LAST_DIR_KEY = "chappa-ai.lastProjectDir";
  const field = (cls: string): HTMLInputElement =>
    document.querySelector<HTMLInputElement>(cls)!;
  const dialogButton = (cls: string): HTMLButtonElement | null =>
    document.querySelector<HTMLButtonElement>(cls);
  const type = (el: HTMLInputElement, value: string): void => {
    el.value = value;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  };
  /** Open the switcher and press "＋ Add project…". */
  const openAddModal = (root: HTMLElement): void => {
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-add")!);
  };

  beforeEach(() => {
    // The remembered dir is real localStorage; keep tests independent.
    try {
      window.localStorage.removeItem(LAST_DIR_KEY);
    } catch {
      /* no storage in this environment: the app degrades, so does the test */
    }
  });

  it("browse fills the path, the name auto-fills, a user edit sticks, Add sends both", async () => {
    const { api, calls } = projectApi({ picked: "/home/dev/code/chappa-ai" });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      canBrowseDirectories: true,
    });
    await app.mount();
    openAddModal(root);
    await FLUSH();

    // The picker starts at the remembered dir — nothing remembered yet.
    dialogButton(".chappa-dialog-browse")!.click();
    await FLUSH();
    expect(calls.pickDirectory).toEqual([["Choose a project directory", undefined]]);
    expect(field(".chappa-dialog-path").value).toBe("/home/dev/code/chappa-ai");
    expect(field(".chappa-dialog-name").value).toBe("chappa-ai");

    // The user renames it, THEN changes the path again: the name must stick.
    type(field(".chappa-dialog-name"), "chappa-ai notes");
    type(field(".chappa-dialog-path"), "/home/dev/code/elsewhere");
    expect(field(".chappa-dialog-name").value).toBe("chappa-ai notes");

    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    // EXACT wire args: the explicit name rides along, never derived.
    expect(calls.addProject).toEqual([["/home/dev/code/elsewhere", "chappa-ai notes"]]);
    expect(calls.openProject).toEqual([2]);
    panelDispose(app);
  });

  it("refuses to Add with an empty field and adds nothing on Cancel", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      canBrowseDirectories: true,
    });
    await app.mount();

    openAddModal(root);
    await FLUSH();
    // Empty path (and therefore empty auto-name): Add is inert, the modal stays.
    expect(dialogButton(".chappa-dialog-ok")!.disabled).toBe(true);
    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(calls.addProject).toEqual([]);
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();

    // A path with the name blanked by hand is still refused.
    type(field(".chappa-dialog-path"), "/p/new");
    type(field(".chappa-dialog-name"), "  ");
    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(calls.addProject).toEqual([]);

    // Cancel: nothing added, nothing opened.
    dialogButton(".chappa-dialog-cancel")!.click();
    await FLUSH();
    expect(calls.addProject).toEqual([]);
    expect(calls.openProject).toEqual([]);
    expect(document.querySelector(".chappa-dialog-backdrop")).toBeNull();
    panelDispose(app);
  });

  it("offers NO Browse… button when there is no native picker (plain browser)", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    // jsdom is not Tauri, so the default `canBrowseDirectories` is false.
    const app = new App({ root, api, confirm: async () => false, confirmProjectTrust: async () => false });
    await app.mount();
    openAddModal(root);
    await FLUSH();
    expect(dialogButton(".chappa-dialog-browse")).toBeNull();

    // The typed-path field is the whole fallback and still works. The name
    // was never edited, so the wire carries null — Rust owns the default
    // (yml name → folder basename), never the preview text.
    type(field(".chappa-dialog-path"), "/p/typed");
    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(calls.addProject).toEqual([["/p/typed", null]]);
    expect(calls.pickDirectory).toEqual([]);
    panelDispose(app);
  });

  it("remembers the browsed directory across adds (localStorage round trip)", async () => {
    const { api, calls } = projectApi({ picked: "/home/dev/work/chappa-ai" });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      canBrowseDirectories: true,
    });
    await app.mount();

    openAddModal(root);
    await FLUSH();
    dialogButton(".chappa-dialog-browse")!.click();
    await FLUSH();
    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    // Browsed, never renamed → untouched name rides as null.
    expect(calls.addProject).toEqual([["/home/dev/work/chappa-ai", null]]);
    // The PARENT is remembered: the next add starts among the siblings, not
    // inside the project just added.
    expect(window.localStorage.getItem(LAST_DIR_KEY)).toBe("/home/dev/work");

    // Second add: the picker is asked to start there.
    openAddModal(root);
    await FLUSH();
    dialogButton(".chappa-dialog-browse")!.click();
    await FLUSH();
    expect(calls.pickDirectory[1]).toEqual(["Choose a project directory", "/home/dev/work"]);
    dialogButton(".chappa-dialog-cancel")!.click();
    await FLUSH();
    panelDispose(app);
  });

  it("remembers a drive-ABSOLUTE parent for a drive-root project (C:\\chappa-ai → C:\\)", async () => {
    // The old parent derivation stored the bare "C:" — a drive-RELATIVE
    // string that resolves against the drive's per-process CWD, so the next
    // picker opened somewhere arbitrary.
    const { api } = projectApi({ picked: "C:\\chappa-ai" });
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      canBrowseDirectories: true,
    });
    await app.mount();
    openAddModal(root);
    await FLUSH();
    dialogButton(".chappa-dialog-browse")!.click();
    await FLUSH();
    dialogButton(".chappa-dialog-ok")!.click();
    await FLUSH();
    expect(window.localStorage.getItem(LAST_DIR_KEY)).toBe("C:\\");
    panelDispose(app);
  });

  it("stores NO breadcrumb for a parentless (relative) path", async () => {
    // The old code stored the path ITSELF here — a relative breadcrumb the
    // picker cannot use.
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptAddProject: async () => ({ path: "relative", name: "rel" }),
    });
    await app.mount();
    openAddModal(root);
    await FLUSH();
    expect(calls.addProject).toEqual([["relative", "rel"]]);
    expect(window.localStorage.getItem(LAST_DIR_KEY)).toBeNull();
    panelDispose(app);
  });

  it("renames a project from the switcher's ✎ and updates the local name", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptRename: async (current) => `${current} notes`,
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");

    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    const pencil = root.querySelector<HTMLButtonElement>(".chappa-project-rename")!;
    expect(pencil.title).toBe('Rename "chappa-ai"');
    click(pencil);
    await FLUSH();

    // EXACT wire args, and the ✎ never doubles as "open this project".
    expect(calls.renameProject).toEqual([[1, "chappa-ai notes"]]);
    expect(calls.openProject).toEqual([1]);
    // Local state follows: the switcher, the SECTION HEADER (it kept the old
    // name until an unrelated status event before the review fix),
    // and the menu row.
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai notes");
    expect(root.querySelector(".chappa-project-name")?.textContent).toBe("chappa-ai notes");
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    expect(root.querySelector(".chappa-project-option")?.textContent).toBe("chappa-ai notes");
    panelDispose(app);
  });

  it("removes a project from the switcher's 🗑 after a confirm, listing what stops", async () => {
    const confirms: Array<[string, string[]]> = [];
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      confirmProjectRemove: async (name, running) => {
        confirms.push([name, running]);
        return true;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    // A running process must be named in the confirm.
    const row = [...(app as unknown as { openProjects: Map<number, { processRows: Map<string, { status: string }> }> }).openProjects.get(1)!.processRows.values()][0];
    row.status = "running";

    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    const trash = root.querySelector<HTMLButtonElement>(".chappa-project-remove")!;
    expect(trash.title).toBe('Remove "chappa-ai" from chappa-ai (files stay on disk)');
    click(trash);
    await FLUSH();

    expect(confirms).toEqual([["chappa-ai", ["import + summarize on open"]]]);
    expect(calls.removeProject).toEqual([1]);
    // The 🗑 never doubles as "open this project"; the row is gone and the
    // project view closed.
    expect(calls.openProject).toEqual([1]);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    expect(root.querySelector(".chappa-project-option")).toBeNull();
    // The expanded section is hidden (its name span is a persistent element)
    // and the switcher is back to its placeholder.
    expect(root.querySelector<HTMLElement>(".chappa-project-section")?.style.display).toBe("none");
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("Project…");
    panelDispose(app);
  });

  it("a declined or failed remove leaves the project alone", async () => {
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    let answer = false;
    const shown: string[] = [];
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      confirmProjectRemove: async () => answer,
      showError: async (m) => {
        shown.push(m);
      },
    });
    await app.mount();
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-remove")!);
    await FLUSH();
    expect(calls.removeProject).toEqual([]);

    // Confirmed but Rust refuses: the error surfaces, the row survives.
    answer = true;
    api.removeProject = vi.fn(async () => {
      throw new Error("no such project: 1");
    });
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-remove")!);
    await FLUSH();
    expect(shown.length).toBe(1);
    expect(shown[0]).toContain("no such project");
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    expect(root.querySelector(".chappa-project-option")?.textContent).toBe("chappa-ai");
    panelDispose(app);
  });

  it("a blank or unchanged rename never reaches the wire", async () => {
    const answers = ["   ", "chappa-ai", ""];
    for (const answer of answers) {
      const { api, calls } = projectApi();
      const root = document.createElement("div");
      const app = new App({
        root,
        api,
        confirm: async () => false,
        confirmProjectTrust: async () => false,
        promptRename: async () => answer,
      });
      await app.mount();
      click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
      click(root.querySelector<HTMLButtonElement>(".chappa-project-rename")!);
      await FLUSH();
      expect(calls.renameProject, JSON.stringify(answer)).toEqual([]);
      panelDispose(app);
    }

    // …and a cancelled prompt (null) is likewise a no-op.
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptRename: async () => null,
    });
    await app.mount();
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    click(root.querySelector<HTMLButtonElement>(".chappa-project-rename")!);
    await FLUSH();
    expect(calls.renameProject).toEqual([]);
    panelDispose(app);
  });
});

// --- settings: gear, pane, new-terminal profiles -------------------

describe("App settings", () => {
  it("puts a gear + Settings entry at the BOTTOM of the rail", async () => {
    const { api } = stubApi();
    const settings = stubSettings();
    const root = document.createElement("div");
    const app = new App({ root, api, settings: settings.store, confirm: async () => false });
    await app.mount();

    const rail = root.querySelector(".chappa-rail")!;
    const gear = rail.querySelector<HTMLButtonElement>(".chappa-rail-gear")!;
    expect(gear.textContent).toContain("Settings");
    // The footer is the rail's LAST child, after the flex:1 terminal list —
    // that is what pins the gear to the bottom.
    expect(rail.lastElementChild?.classList.contains("chappa-rail-footer")).toBe(true);
    panelDispose(app);
  });

  it("the gear toggles the settings overlay open and closed", async () => {
    const { api } = stubApi();
    const settings = stubSettings();
    const root = document.createElement("div");
    const app = new App({ root, api, settings: settings.store, confirm: async () => false });
    await app.mount();

    const gear = root.querySelector<HTMLButtonElement>(".chappa-rail-gear")!;
    expect(app.settingsPaneElement).toBeNull();
    gear.click();
    expect(app.settingsPaneElement!.style.display).toBe("flex");
    // Fresh defaults: the marks toggle renders OFF (deliberate opt-in).
    const marks = app.settingsPaneElement!.querySelector<HTMLInputElement>(
      '[data-setting="syntheticPromptMarks"]',
    )!;
    expect(marks.checked).toBe(false);
    gear.click();
    expect(app.settingsPaneElement!.style.display).toBe("none");
    panelDispose(app);
  });

  it("loads settings BEFORE the first spawn", async () => {
    const { api } = stubApi();
    const settings = stubSettings({ fontSize: 16 });
    const root = document.createElement("div");
    const app = new App({ root, api, settings: settings.store, confirm: async () => false });
    await app.mount();
    await app.newShell(); // blank start: the first spawn must follow settings load
    expect(settings.api.getSettings).toHaveBeenCalledTimes(1);
    expect(settings.api.getSettings.mock.invocationCallOrder[0]).toBeLessThan(
      (api.createTerminal as ReturnType<typeof vi.fn>).mock.invocationCallOrder[0],
    );
    panelDispose(app);
  });

  it("a font change re-measures every open panel", async () => {
    const { api } = stubApi();
    const settings = stubSettings();
    const root = document.createElement("div");
    const app = new App({ root, api, settings: settings.store, confirm: async () => false });
    await app.mount();
    await app.newShell();
    await FLUSH();

    const panels = [
      ...(app as unknown as { entries: Map<number, RailEntry> }).entries.values(),
    ].map((e) => e.panel);
    const spies = panels.map((p) => vi.spyOn(p, "setFontMetrics"));
    await settings.store.update({ fontSize: 16 });
    await FLUSH();
    for (const spy of spies) expect(spy).toHaveBeenCalledWith("Geist Mono", 16, 1.2);

    // A change that touches nothing font-related re-measures nothing.
    for (const spy of spies) spy.mockClear();
    await settings.store.update({ copyOnSelect: true });
    await FLUSH();
    for (const spy of spies) expect(spy).not.toHaveBeenCalled();
    panelDispose(app);
  });
});

describe("App new-terminal menu", () => {
  const profiles = [
    { name: "Windows PowerShell", command: "powershell.exe -NoLogo", enabled: true },
    { name: "Git Bash", command: "bash.exe", enabled: false },
  ];

  async function mounted(seed = { shellProfiles: profiles }) {
    const { api, ids } = stubApi();
    const settings = stubSettings(seed);
    const root = document.createElement("div");
    const app = new App({ root, api, settings: settings.store, confirm: async () => false });
    await app.mount();
    return { api, ids, root, app };
  }

  it("offers only ENABLED profiles (plus the default shell)", async () => {
    const { root, app } = await mounted();
    root.querySelector<HTMLButtonElement>(".chappa-new-term-menu")!;
    const buttons = root.querySelectorAll<HTMLButtonElement>(".chappa-new-term");
    buttons[1].click(); // the ▾ affordance
    const options = [...root.querySelectorAll<HTMLButtonElement>(".chappa-new-term-option")];
    expect(options.map((o) => o.textContent)).toEqual(["Default shell", "Windows PowerShell"]);
    // The disabled profile is absent — that is exactly what the toggle means.
    expect(options.some((o) => o.dataset.profile === "Git Bash")).toBe(false);
    panelDispose(app);
  });

  it("choosing a profile spawns with `profile` in the spec", async () => {
    const { api, root, app } = await mounted();
    root.querySelectorAll<HTMLButtonElement>(".chappa-new-term")[1].click();
    root.querySelectorAll<HTMLButtonElement>(".chappa-new-term-option")[1].click();
    await FLUSH();
    const calls = (api.createTerminal as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls[calls.length - 1][0].spec.profile).toBe("Windows PowerShell");
    // `command` stays empty: the profile is resolved Rust-side.
    expect(calls[calls.length - 1][0].spec.command).toBe("");
    panelDispose(app);
  });

  it("the plain ＋ keeps today's behaviour: default shell, no profile", async () => {
    const { api, root, app } = await mounted();
    root.querySelectorAll<HTMLButtonElement>(".chappa-new-term")[0].click();
    await FLUSH();
    const calls = (api.createTerminal as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls[calls.length - 1][0].spec.profile).toBeUndefined();
    expect(calls[calls.length - 1][0].spec.command).toBe("");
    panelDispose(app);
  });

  it("says so when there is nothing enabled to offer", async () => {
    const { root, app } = await mounted({ shellProfiles: [] });
    root.querySelectorAll<HTMLButtonElement>(".chappa-new-term")[1].click();
    expect(root.querySelector(".chappa-new-term-empty")?.textContent).toBe(
      "No enabled shell profiles",
    );
    panelDispose(app);
  });
});

// --- notification levels -------------------------------------------

/** The event handlers the Tauri wiring calls, reached the established way:
 *  a cast through `unknown` (the applyProcessStatus pattern above). */
type NotifyDriver = {
  applyBell: (p: ipc.TerminalEventDto) => void;
  applyNotify: (p: ipc.TerminalEventDto) => void;
  applyActivity: (p: ipc.TerminalEventDto) => void;
  windowFocused: boolean;
  activeId: number | null;
};

const drive = (app: App): NotifyDriver => app as unknown as NotifyDriver;

const bellEvent = (id: number): ipc.TerminalEventDto => ({
  event: "term://bell",
  term_id: id,
  seq: 0,
});

const notifyEvent = (id: number, title: string, body: string): ipc.TerminalEventDto => ({
  event: "term://notify",
  term_id: id,
  seq: 0,
  title,
  body,
});

function contextMenu(el: Element): void {
  el.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
}

function submenuItems(root: HTMLElement): HTMLButtonElement[] {
  return [...root.querySelectorAll<HTMLButtonElement>(".chappa-row-submenu .chappa-row-menu-item")];
}

function levelOptions(app: App): HTMLButtonElement[] {
  // The project-default picker relocated into the Edit-project pane (task
  // 44); it is queried through `app.editPaneElement`, not the root.
  const pane = app.editPaneElement;
  if (!pane) return [];
  return [
    ...pane.querySelectorAll<HTMLButtonElement>(
      '[data-setting="notificationLevel"] .chappa-segmented-option',
    ),
  ];
}

async function openedProject(over?: Parameters<typeof projectApi>[0]) {
  const { api, calls } = projectApi(over);
  const root = document.createElement("div");
  document.body.appendChild(root);
  const app = new App({
    root,
    api,
    confirm: async () => false,
    confirmProjectTrust: async () => false,
  });
  await app.mount();
  await app.newShell(); // blank start: spawn the mount shell explicitly
  await app.openProject(1);
  await FLUSH();
  return { api, calls, root, app };
}

describe("App notification levels — the project picker in the Edit pane", () => {
  it("puts a segmented All | Important | None picker in the Edit-project pane", async () => {
    const { root, app } = await openedProject();
    app.openEditPane(1);
    const picker = app.editPaneElement!.querySelector<HTMLElement>('[data-setting="notificationLevel"]')!;
    expect(picker).not.toBeNull();
    // It now lives in the Edit-project pane, NOT the rail.
    expect(picker.closest(".chappa-edit-pane")).not.toBeNull();
    const opts = levelOptions(app);
    expect(opts.map((o) => o.textContent)).toEqual(["All", "Important", "None"]);
    // A null project default resolves to 'all' — the underlined option.
    expect(opts[0].getAttribute("aria-checked")).toBe("true");
    panelDispose(app);
    root.remove();
  });

  it("persists a picked project default with the exact wire args", async () => {
    const { calls, root, app } = await openedProject();
    app.openEditPane(1);
    const opts = levelOptions(app);
    opts[1].click();
    await FLUSH();
    // processName null = the PROJECT default.
    expect(calls.setLevel).toEqual([[1, null, "important"]]);
    expect(opts[1].getAttribute("aria-checked")).toBe("true");
    panelDispose(app);
    root.remove();
  });

  it("picking All CLEARS the project default (null on the wire, not 'all')", async () => {
    // 'all' is the bottom of the resolution chain, so explicit-all ≡ unset —
    // clearing keeps a never-customized projects.json entry byte-identical
    //. Process OVERRIDES are exempt: an explicit per-process
    // "all" beats a project default of "none" and persists verbatim.
    const { calls, root, app } = await openedProject({
      projects: [{ id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: "none" }],
    });
    app.openEditPane(1);
    const opts = levelOptions(app);
    opts[0].click();
    await FLUSH();
    expect(calls.setLevel).toEqual([[1, null, null]]);
    expect(opts[0].getAttribute("aria-checked")).toBe("true");
    panelDispose(app);
    root.remove();
  });

  it("reverts the optimistic picker value when the persist fails", async () => {
    // A silently swallowed write failure left the UI filtering with a level
    // that never landed on disk — the local value must snap
    // back and the picker re-render.
    const { root, app } = await openedProject();
    app.openEditPane(1);
    (app as unknown as { api: { setNotificationLevel: () => Promise<void> } }).api.setNotificationLevel =
      async () => {
        throw new Error("disk full");
      };
    levelOptions(app)[2].click();
    await FLUSH();
    // The revert re-syncs the pane — re-query. Back to the resolved 'all'.
    const opts = levelOptions(app);
    expect(opts[0].getAttribute("aria-checked")).toBe("true");
    expect(opts[2].getAttribute("aria-checked")).toBe("false");
    panelDispose(app);
    root.remove();
  });

  it("suppresses the webview's native context menu except on text fields", async () => {
    // A desktop app never shows Back/Refresh/Print/Inspect (host
    // round: right-clicking a rail row surfaced the browser menu).
    const { root, app } = await openedProject();
    const onRail = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    root.dispatchEvent(onRail);
    expect(onRail.defaultPrevented).toBe(true);
    // Editable fields keep the native menu (right-click copy/paste).
    const input = document.createElement("input");
    document.body.appendChild(input);
    const onInput = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    input.dispatchEvent(onInput);
    expect(onInput.defaultPrevented).toBe(false);
    input.remove();
    panelDispose(app);
    root.remove();
  });

  it("shows a stored project default as the active option on open", async () => {
    const { root, app } = await openedProject({
      projects: [{ id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: "none" }],
    });
    app.openEditPane(1);
    const opts = levelOptions(app);
    expect(opts[2].getAttribute("aria-checked")).toBe("true");
    expect(opts[0].getAttribute("aria-checked")).toBe("false");
    panelDispose(app);
    root.remove();
  });
});

describe("App notification levels — the command context menu", () => {
  it("right-clicks open the menu without disturbing click-to-activate", async () => {
    const { calls, root, app } = await openedProject();
    const rows = projectRows(root);
    const menu = root.querySelector<HTMLElement>(".chappa-row-menu")!;
    expect(menu.style.display).toBe("none");

    // The never-started command offers Start…
    contextMenu(rows[1]);
    expect(menu.style.display).toBe("block");
    const primary = menu.querySelector<HTMLButtonElement>(".chappa-row-menu-item")!;
    expect(primary.textContent).toBe("Start");
    expect(primary.dataset.action).toBe("start");

    // …and the auto-started one offers Stop (only the applicable item shows).
    contextMenu(rows[0]);
    expect(primary.textContent).toBe("Stop");
    expect(primary.dataset.action).toBe("stop");

    // A press OUTSIDE dismisses (capture-phase, inside-guarded).
    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(menu.style.display).toBe("none");

    // The pinned left-click behaviour is untouched: the card still activates.
    click(rows[1]);
    expect(rows[1].classList.contains("active")).toBe(true);
    expect(menu.style.display).toBe("none");

    // Start from the menu runs the row's existing action.
    contextMenu(rows[1]);
    primary.click();
    await FLUSH();
    expect(calls.startProcess.map((c) => c[1])).toContain("chappa-ai tui");
    panelDispose(app);
    root.remove();
  });

  it("Copy command writes the command line to the clipboard", async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    const { root, app } = await openedProject();
    contextMenu(projectRows(root)[0]);
    const copy = [
      ...root.querySelectorAll<HTMLButtonElement>(".chappa-row-menu > .chappa-row-menu-item"),
    ].find((b) => b.textContent === "Copy command")!;
    copy.click();
    await FLUSH();
    expect(writeText).toHaveBeenCalledWith("py app/summarize.py");
    panelDispose(app);
    root.remove();
  });

  it("the submenu reflects the resolved level — inherited, then override", async () => {
    const { calls, root, app } = await openedProject();
    const rows = projectRows(root);
    contextMenu(rows[0]);
    const toggle = root.querySelector<HTMLButtonElement>(".chappa-row-submenu-toggle")!;
    expect(toggle.textContent).toBe("Notification level ▸");
    const sub = root.querySelector<HTMLElement>(".chappa-row-submenu")!;
    expect(sub.classList.contains("open")).toBe(false);
    toggle.click();
    expect(sub.classList.contains("open")).toBe(true);

    let items = submenuItems(root);
    expect(items.map((i) => i.dataset.level)).toEqual(["inherit", "all", "important", "none"]);
    // No override, no project default: Inherit is checked and NAMES what is
    // inherited ('all' is the bottom of the resolution chain).
    expect(items[0].textContent).toContain("Inherit (All)");
    expect(items[0].dataset.checked).toBe("true");
    expect(items.filter((i) => i.dataset.checked).length).toBe(1);

    // Pick None: the OVERRIDE persists with the process name.
    items[3].click();
    expect(calls.setLevel).toEqual([[1, "import + summarize on open", "none"]]);
    expect((root.querySelector(".chappa-row-menu") as HTMLElement).style.display).toBe("none");

    // Reopen: the checkmark moved to the override, off Inherit.
    contextMenu(rows[0]);
    toggle.click();
    items = submenuItems(root);
    expect(items[0].dataset.checked).toBeUndefined();
    expect(items[3].dataset.checked).toBe("true");
    expect(items.filter((i) => i.dataset.checked).length).toBe(1);

    // Inherit CLEARS the override: level null on the wire.
    items[0].click();
    expect(calls.setLevel[1]).toEqual([1, "import + summarize on open", null]);

    // Back to inherited, and the Inherit row is checked again.
    contextMenu(rows[0]);
    toggle.click();
    expect(submenuItems(root)[0].dataset.checked).toBe("true");
    panelDispose(app);
    root.remove();
  });

  it("the Inherit label follows the project default", async () => {
    const { root, app } = await openedProject();
    app.openEditPane(1);
    levelOptions(app)[1].click(); // project default → Important
    await FLUSH();
    contextMenu(projectRows(root)[1]);
    root.querySelector<HTMLButtonElement>(".chappa-row-submenu-toggle")!.click();
    expect(submenuItems(root)[0].textContent).toContain("Inherit (Important)");
    panelDispose(app);
    root.remove();
  });
});

describe("App notification levels — the event path", () => {
  it("suppresses ENTIRELY on the focused terminal of a focused window", async () => {
    const { api, notified } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell();
    const d = drive(app);
    expect(d.activeId).toBe(1);

    // Active + window focused: no toast AND no badge (// "a badge means something happened while you weren't looking").
    d.windowFocused = true;
    d.applyBell(bellEvent(1));
    d.applyNotify(notifyEvent(1, "", "done"));
    expect(notified).toEqual([]);
    expect(root.querySelector(".chappa-rail-bell")).toBeNull();

    // Active but the WINDOW lost focus: back to per-level behaviour.
    d.windowFocused = false;
    d.applyBell(bellEvent(1));
    expect(notified).toEqual([["shell", "Bell"]]);
    expect(root.querySelector(".chappa-rail-bell")).not.toBeNull();
    panelDispose(app);
  });

  it("still notifies a hidden terminal while the window IS focused — as a BANNER", async () => {
    const { api, notified } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell(); // id 1 becomes active… then hidden
    await app.newShell(); // id 2 becomes active; id 1 is hidden
    await FLUSH();
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(1));
    // Routing: osNotify + window FOCUSED → in-app banner, NEVER an OS
    // toast (the terminal is necessarily non-active, else suppressed).
    expect(notified).toEqual([]);
    expect(root.querySelectorAll(".chappa-banner").length).toBe(1);
    expect(root.querySelector(".chappa-banner-title")?.textContent).toBe("shell");
    expect(root.querySelectorAll(".chappa-rail-bell").length).toBe(1);
    panelDispose(app);
  });

  it("tracks the window's focus through focus/blur events", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    const d = drive(app);
    window.dispatchEvent(new Event("blur"));
    expect(d.windowFocused).toBe(false);
    window.dispatchEvent(new Event("focus"));
    expect(d.windowFocused).toBe(true);
    panelDispose(app);
  });

  it("attributes a titleless OSC 9 and never toasts a numeric body", async () => {
    const { api, notified } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => false });
    await app.mount();
    await app.newShell();
    await FLUSH();
    const d = drive(app);
    // Window UNFOCUSED: the OS-toast path (the focused path banners
    // instead; this test's subject is attribution + numeric spam, unchanged).
    d.windowFocused = false;

    // OSC 9 sends title "" — the terminal's display name stands in.
    d.applyNotify(notifyEvent(1, "", "build finished"));
    expect(notified).toEqual([["shell", "build finished"]]);

    // Progress spam badges but never toasts, even at level 'all'.
    notified.length = 0;
    d.applyNotify(notifyEvent(1, "", "42%"));
    expect(notified).toEqual([]);
    expect(root.querySelectorAll(".chappa-rail-bell").length).toBe(1);

    // A real title is used as sent.
    d.applyNotify(notifyEvent(1, "cargo", "done"));
    expect(notified).toEqual([["cargo", "done"]]);
    panelDispose(app);
  });

  it("badges a hidden process terminal on its CARD and clears it on activate", async () => {
    const { calls, root, app } = await openedProject();
    const rows = projectRows(root);
    // The auto-started command holds term 1; start the other so term 1 hides.
    click(rowButton(rows[1], "Start"));
    await FLUSH();
    expect(rows[1].classList.contains("active")).toBe(true);

    const d = drive(app);
    // Unfocused = the OS-toast path (focused banners instead).
    d.windowFocused = false;
    d.applyBell(bellEvent(1));
    const badge = rows[0].querySelector<HTMLElement>(".chappa-process-bell")!;
    expect(badge.style.display).toBe("inline");
    // Level 'all' also toasted, attributed to the command.
    expect(calls.osNotify).toEqual([["import + summarize on open", "Bell"]]);
    // Process entries never render as rail rows — the card is their only badge
    // surface (only the mount() shell is in the rail).
    expect(root.querySelectorAll(".chappa-rail-item").length).toBe(1);
    expect(root.querySelector(".chappa-rail-bell")).toBeNull();

    // Hidden output lights the activity dot too.
    d.applyActivity({ event: "term://activity", term_id: 1, seq: 1 });
    const dot = rows[0].querySelector<HTMLElement>(".chappa-process-activity")!;
    expect(dot.style.display).toBe("inline-block");

    // Activating the card clears both.
    click(rows[0]);
    expect(badge.style.display).toBe("none");
    expect(dot.style.display).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("resolves a process terminal override → project default → 'all'", async () => {
    const { calls, root, app } = await openedProject();
    const rows = projectRows(root);
    click(rowButton(rows[1], "Start"));
    await FLUSH();
    const d = drive(app);
    // Unfocused = the OS-toast path (focused banners instead).
    d.windowFocused = false;

    // Project default → None: the hidden command badges silently.
    app.openEditPane(1);
    levelOptions(app)[2].click();
    d.applyBell(bellEvent(1));
    expect(calls.osNotify).toEqual([]);
    expect(rows[0].querySelector<HTMLElement>(".chappa-process-bell")!.style.display).toBe("inline");

    // A per-process override of All wins over the project default.
    contextMenu(rows[0]);
    root.querySelector<HTMLButtonElement>(".chappa-row-submenu-toggle")!.click();
    submenuItems(root)[1].click(); // "All"
    d.applyBell(bellEvent(1));
    expect(calls.osNotify).toEqual([["import + summarize on open", "Bell"]]);

    // Important filters bells back out, but not OSC notifies.
    contextMenu(rows[0]);
    root.querySelector<HTMLButtonElement>(".chappa-row-submenu-toggle")!.click();
    submenuItems(root)[2].click(); // "Important"
    calls.osNotify.length = 0;
    d.applyBell(bellEvent(1));
    expect(calls.osNotify).toEqual([]);
    d.applyNotify(notifyEvent(1, "", "tests failed"));
    expect(calls.osNotify).toEqual([["import + summarize on open", "tests failed"]]);
    panelDispose(app);
    root.remove();
  });

  it("a PLAIN SHELL resolves to the active project's default (chappa-ai decision)", async () => {
    // Project-less terminals resolve to the open project's default, so
    // silencing a project silences the shell sitting next to it.
    const { calls, root, app } = await openedProject({
      projects: [{ id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: "none" }],
    });
    const d = drive(app);
    // Unfocused = the OS-toast path (focused banners instead).
    d.windowFocused = false;
    // The mount() shell (id 900 from the project stub) is hidden behind the
    // auto-started command's panel.
    expect(d.activeId).not.toBe(900);
    d.applyBell(bellEvent(900));
    expect(calls.osNotify).toEqual([]);
    expect(root.querySelector(".chappa-rail-bell")).not.toBeNull();

    // Raise the project default to All: the same shell now toasts.
    app.openEditPane(1);
    levelOptions(app)[0].click();
    d.applyBell(bellEvent(900));
    expect(calls.osNotify).toEqual([["shell", "Bell"]]);
    panelDispose(app);
    root.remove();
  });
});

// --- project terminals ---------------------------------------------

const TERM_PROFILES = [
  { name: "Windows PowerShell", command: "powershell.exe -NoLogo", enabled: true },
  { name: "Git Bash", command: "bash.exe", enabled: false },
];

/** One non-auto command, so the subsection has COMMANDS rows to sit above
 *  without any spawn of its own racing the terminal ids. */
const TERM_COMMAND: ipc.ProjectProcessDto = {
  name: "chappa-ai tui",
  command: "py app/tui.py",
  status: "stopped",
  autoStart: false,
  autoRestart: false,
  restartWhenChanged: [],
  termId: null,
  exitCode: null,
  notificationLevel: null,
  workingDir: null,
  env: {},
  favorite: false,
  disableAutoRename: false,
};

/**
 * An opened project whose spawn stub hands out DISTINCT terminal ids and
 * records each spec. Two departures from `projectApi`, both forced:
 *  - its `createTerminal` answers 900 for every spawn, which would collapse
 *    the mount shell and every project terminal onto one entry;
 *  - its `openProject` answers every id with projects[0], so a two-project
 *    fixture would hand project 2 project 1's notification default — exactly
 *    the distinction the boundary case turns on.
 * The mount shell is term 700; project terminals follow at 701, 702, …
 */
async function projectTerminals(
  opts: {
    projects?: ipc.ProjectInfoDto[];
    confirm?: (name: string) => Promise<boolean>;
  } = {},
) {
  const projects = opts.projects ?? [
    { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
  ];
  const { api, calls } = projectApi({ projects });
  api.openProject = vi.fn(async (id: number) => {
    calls.openProject.push(id);
    return {
      project: projects.find((p) => p.id === id)!,
      trustPending: false,
      trustCommands: [],
      processes: [TERM_COMMAND],
    };
  });
  const spawns: Array<{ cwd?: string; profile?: string }> = [];
  let nextTerm = 700;
  api.createTerminal = vi.fn(async (o: ipc.CreateTerminalOptions) => {
    spawns.push({ cwd: o.spec.cwd, profile: o.spec.profile });
    return nextTerm++;
  });
  const root = document.createElement("div");
  document.body.appendChild(root);
  const app = new App({
    root,
    api,
    settings: stubSettings({ shellProfiles: TERM_PROFILES }).store,
    confirm: opts.confirm ?? (async () => false),
    confirmProjectTrust: async () => false,
  });
  await app.mount();
  await app.newShell(); // blank start: the workspace shell (700)
  await app.openProject(projects[0].id);
  await FLUSH();
  return { api, calls, spawns, root, app };
}

const termAdd = (root: HTMLElement): HTMLButtonElement =>
  root.querySelector<HTMLButtonElement>(".chappa-project-term-add")!;

const termRows = (root: HTMLElement): NodeListOf<Element> =>
  root.querySelectorAll(".chappa-project-term-row");

describe("App project terminals", () => {
  it("puts a TERMINALS subsection with a right count and the ＋ ON the header line", async () => {
    const { root, app } = await projectTerminals();
    const terms = root.querySelector<HTMLElement>(".chappa-project-terms")!;
    expect(terms.closest(".chappa-project-section")).not.toBeNull();
    expect(terms.querySelector(".chappa-subsection-label")?.textContent).toBe("TERMINALS");
    expect(terms.querySelector(".chappa-subsection-count")?.textContent).toBe("0");

    // "the add affordance ON the header line — a small `+` (and the profile
    // `▾` if cheap, same menu as the rail's) at the right of the TERMINALS
    // header."
    const header = terms.querySelector<HTMLElement>(".chappa-subsection-header")!;
    expect(termAdd(root).closest(".chappa-subsection-header")).toBe(header);
    expect(root.querySelector(".chappa-project-term-more")!.closest(".chappa-subsection-header")).toBe(
      header,
    );
    // Deliberate (2026-08-15): the add affordances ride the
    // header line, NOT a separate '+ Add terminal' row.
    expect(
      [...root.querySelectorAll("button")].some((b) => /add terminal/i.test(b.textContent ?? "")),
    ).toBe(false);

    // "between the header/picker and the COMMANDS rows". The notification
    // picker moved into the Edit pane, so the TERMINALS subsection
    // now sits directly under the header, before COMMANDS.
    const kids = [...root.querySelector(".chappa-project-section")!.children];
    expect(kids[0].className).toBe("chappa-project-header");
    expect(kids.indexOf(terms)).toBeLessThan(
      kids.indexOf(root.querySelector(".chappa-project-processes")!),
    );
    panelDispose(app);
    root.remove();
  });

  it("the ＋ spawns the default shell with cwd = the project root", async () => {
    const { root, app, spawns } = await projectTerminals();
    click(termAdd(root));
    await FLUSH();
    // "`+` spawns the default shell (profile menu: that profile) with
    // `cwd = project root`."
    expect(spawns[spawns.length - 1]).toEqual({ cwd: "/p/chappa-ai", profile: undefined });
    expect(termRows(root).length).toBe(1);
    expect(root.querySelector(".chappa-subsection-count")?.textContent).toBe("1");
    panelDispose(app);
    root.remove();
  });

  it("the ▾ offers the same profile list and spawns it with the same cwd", async () => {
    const { root, app, spawns } = await projectTerminals();
    click(root.querySelector<HTMLButtonElement>(".chappa-project-term-more")!);
    const options = [...root.querySelectorAll<HTMLButtonElement>(".chappa-project-term-option")];
    expect(options.map((o) => o.textContent)).toEqual(["Default shell", "Windows PowerShell"]);
    // The rail's own menu is untouched: its selectors match only its options.
    expect(root.querySelectorAll(".chappa-new-term-option").length).toBe(0);
    options[1].click();
    await FLUSH();
    expect(spawns[spawns.length - 1]).toEqual({ cwd: "/p/chappa-ai", profile: "Windows PowerShell" });
    expect(termRows(root).length).toBe(1);
    panelDispose(app);
    root.remove();
  });

  it("renders the bound terminal as a rail-anatomy row with badges", async () => {
    const { calls, root, app } = await projectTerminals();
    click(termAdd(root));
    await FLUSH();
    let row = termRows(root)[0];
    expect(row.querySelector(".chappa-status-dot")).not.toBeNull();
    expect(row.querySelector(".chappa-rail-name")?.textContent).toBe("shell");
    expect(row.querySelector(".chappa-rail-close")).not.toBeNull();

    // Hide it behind the workspace shell (the real path: click its rail row).
    click(root.querySelector(".chappa-rail-list .chappa-rail-item")!);
    const d = drive(app);
    // Unfocused = the OS-toast path (focused banners instead).
    d.windowFocused = false;
    d.applyActivity({ event: "term://activity", term_id: 701, seq: 1 });
    d.applyBell(bellEvent(701));
    row = termRows(root)[0];
    expect(row.querySelector(".chappa-rail-activity")).not.toBeNull();
    expect(row.querySelector(".chappa-rail-bell")).not.toBeNull();
    // The badges land on the SUBSECTION row, never on a workspace rail row.
    expect(root.querySelector(".chappa-rail-list .chappa-rail-bell")).toBeNull();
    // Level 'all' (no project default) also toasts, attributed to the shell.
    expect(calls.osNotify).toEqual([["shell", "Bell"]]);

    // Activating clears both — the rail's "shown until activate" rule.
    click(termRows(root)[0]);
    row = termRows(root)[0];
    expect(row.querySelector(".chappa-rail-activity")).toBeNull();
    expect(row.querySelector(".chappa-rail-bell")).toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("a bound terminal resolves ITS OWNING project's level, not the active one", async () => {
    // Boundary: two projects, the terminal bound to the NON-active one.
    // "Notification resolution for a bound terminal = ITS project's default
    // (then 'all'), regardless of which project is active."
    const { calls, root, app } = await projectTerminals({
      projects: [
        { id: 1, name: "quiet", path: "/p/quiet", icon: null, notificationLevel: "none" },
        { id: 2, name: "loud", path: "/p/loud", icon: null, notificationLevel: null },
      ],
    });
    click(termAdd(root));
    await FLUSH();
    expect(termRows(root).length).toBe(1);

    // Switch to project 2 (level 'all'): the section hides project 1's row…
    await app.openProject(2);
    await FLUSH();
    expect(termRows(root).length).toBe(0);
    expect(root.querySelector(".chappa-subsection-count")?.textContent).toBe("0");

    // …but the terminal is alive and still resolves project 1 → 'none'.
    const d = drive(app);
    d.windowFocused = false; // nothing is suppressed by the focused rule
    d.applyBell(bellEvent(701));
    expect(calls.osNotify).toEqual([]);
    // Control, same event one line later: the PLAIN workspace shell keeps the
    // behaviour — the ACTIVE project (2 → 'all') — and toasts. Only
    // the binding differs between the two entries.
    d.applyBell(bellEvent(700));
    expect(calls.osNotify).toEqual([["shell", "Bell"]]);

    // "re-opening shows them again" — with the badge it earned while hidden.
    await app.openProject(1);
    await FLUSH();
    const rows = termRows(root);
    expect(rows.length).toBe(1);
    expect(rows[0].querySelector(".chappa-rail-bell")).not.toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("× runs the normal confirm-when-running close flow", async () => {
    const confirmed: string[] = [];
    const { api, root, app } = await projectTerminals({
      confirm: async (name) => {
        confirmed.push(name);
        return true;
      },
    });
    click(termAdd(root));
    await FLUSH();
    termRows(root)[0].querySelector<HTMLButtonElement>(".chappa-rail-close")!.click();
    // A starting/running terminal asks first (synchronously; only the ANSWER
    // is async).
    expect(confirmed).toEqual(["shell"]);
    await FLUSH();
    expect(termRows(root).length).toBe(0);
    expect(root.querySelector(".chappa-subsection-count")?.textContent).toBe("0");
    expect(api.closeTerminal).toHaveBeenCalledWith(701);
    panelDispose(app);
    root.remove();
  });

  it("keeps project terminals OUT of the workspace rail and its count", async () => {
    const { root, app } = await projectTerminals();
    click(termAdd(root));
    await FLUSH();
    click(termAdd(root));
    await FLUSH();
    expect(termRows(root).length).toBe(2);
    expect(root.querySelector(".chappa-subsection-count")?.textContent).toBe("2");
    // The rail still holds only the mount() shell.
    expect(root.querySelectorAll(".chappa-rail-list .chappa-rail-item").length).toBe(1);
    expect(root.querySelector(".chappa-rail-header-label")?.textContent).toBe("TERMINALS 1");
    panelDispose(app);
    root.remove();
  });

  it("skips rebuilding the bound-terminal rows when nothing visible changed", async () => {
    const { root, app } = await projectTerminals();
    click(termAdd(root)); // bound terminal → 701, active
    await FLUSH();
    // Park activation on the workspace shell so the bound row is a bystander.
    click(root.querySelector<HTMLButtonElement>(".chappa-rail-list .chappa-rail-item")!);
    const row = termRows(root)[0];

    // A rail repaint with nothing visibly different keeps the SAME row node
    // (the signature guard): renderRail tails into the subsection on every
    // event, and most events change nothing a bound-terminal row shows.
    (app as unknown as { renderRail: () => void }).renderRail();
    expect(termRows(root)[0]).toBe(row);

    // A visible change (bell badge on the hidden bound terminal) rebuilds.
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(701));
    const rebuilt = termRows(root)[0];
    expect(rebuilt).not.toBe(row);
    expect(rebuilt.querySelector(".chappa-rail-bell")).not.toBeNull();
    panelDispose(app);
    root.remove();
  });
});

// --- multi-project rail --------------------------------------------

/** Both projects define the SAME process names — the boundary fixture: a
 *  name collision between their processes must still drive status/badges to
 *  the right rows. `server` auto-starts; `chappa-ai tui` never does. */
const COLLIDING_PROCESSES: ipc.ProjectProcessDto[] = [
  {
    name: "server",
    command: "py app/server.py",
    status: "stopped",
    autoStart: true,
    autoRestart: false,
    restartWhenChanged: [],
    termId: null,
    exitCode: null,
    notificationLevel: null,
    workingDir: null,
    env: {},
    favorite: false,
    disableAutoRename: false,
  },
  {
    name: "chappa-ai tui",
    command: "py app/tui.py",
    status: "stopped",
    autoStart: false,
    autoRestart: false,
    restartWhenChanged: [],
    termId: null,
    exitCode: null,
    notificationLevel: null,
    workingDir: null,
    env: {},
    favorite: false,
    disableAutoRename: false,
  },
];

/**
 * Two stored projects whose `openProject` answers each id with ITS OWN row
 * (the shared `projectApi` stub answers projects[0] for every id — exactly
 * the distinction a two-project fixture turns on). Process panels get
 * distinct term ids from `startProjectProcess` (1, 2, …); the mount shell is
 * 900 from the base stub.
 */
async function multiProject(opts: {
  projects?: ipc.ProjectInfoDto[];
  processes?: ipc.ProjectProcessDto[];
  confirmProjectClose?: (name: string, running: string[]) => Promise<boolean>;
} = {}) {
  const projects = opts.projects ?? [
    { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
    { id: 2, name: "health", path: "/p/health", icon: null, notificationLevel: null },
  ];
  const { api, calls } = projectApi({ projects });
  api.openProject = vi.fn(async (id: number) => {
    calls.openProject.push(id);
    return {
      project: projects.find((p) => p.id === id)!,
      trustPending: false,
      trustCommands: [],
      processes: opts.processes ?? COLLIDING_PROCESSES,
    };
  });
  const root = document.createElement("div");
  document.body.appendChild(root);
  const app = new App({
    root,
    api,
    confirm: async () => false,
    confirmProjectTrust: async () => false,
    confirmProjectClose: opts.confirmProjectClose,
  });
  await app.mount();
  await app.newShell(); // blank start: the mount shell (900)
  return { api, calls, root, app };
}

const summaries = (root: HTMLElement): NodeListOf<HTMLElement> =>
  root.querySelectorAll<HTMLElement>(".chappa-project-summary");

describe("App multi-project rail", () => {
  it("routes status, badges and lifecycle by (project, name) across a name collision", async () => {
    const { calls, root, app } = await multiProject();
    await app.openProject(1); // auto-starts chappa-ai's `server` → term 1
    await FLUSH();
    await app.openProject(2); // auto-starts health's `server` → term 2
    await FLUSH();
    expect(calls.startProcess).toEqual([
      [1, "server"],
      [2, "server"],
    ]);
    const appAny = app as unknown as {
      applyProcessStatus: (p: ipc.ProjectProcessStatusEvent) => void;
    };

    // Health is expanded; its `server` row is the visible one.
    const visible = projectRows(root)[0];
    appAny.applyProcessStatus({ project_id: 2, name: "server", status: "running", exit_code: null, term_id: 2 });
    expect(visible.querySelector(".chappa-status-pill")?.textContent).toBe("running");

    // ChappaAi's SAME-NAME process exits: health's visible row must not move; the
    // summary row's running count follows chappa-ai's own rows.
    appAny.applyProcessStatus({ project_id: 1, name: "server", status: "exited", exit_code: 0, term_id: 1 });
    expect(visible.querySelector(".chappa-status-pill")?.textContent).toBe("running");
    const summary = summaries(root)[0];
    expect(summary.querySelector(".chappa-project-summary-name")?.textContent).toBe("chappa-ai");
    expect(summary.querySelector(".chappa-project-pill")?.textContent).toBe("0/2");

    // A bell on chappa-ai's hidden `server` terminal (term 1) badges CHAPPA: the
    // aggregated summary bell lights and the toast attributes chappa-ai's process;
    // health's visible same-name card stays clean.
    const d = drive(app);
    // Unfocused = the OS-toast path (focused banners instead).
    d.windowFocused = false;
    d.applyBell(bellEvent(1));
    expect(calls.osNotify).toEqual([["server", "Bell"]]);
    expect(summary.querySelector<HTMLElement>(".chappa-rail-bell")!.style.display).toBe("inline");
    expect(visible.querySelector<HTMLElement>(".chappa-process-bell")!.style.display).toBe("none");

    // Expand chappa-ai (click its summary): NO IPC re-open — the Rust open_project
    // resets the runtime, so an already-open id must expand from cache.
    click(summary);
    expect(calls.openProject).toEqual([1, 2]);
    // Its row shows the status it earned in the background…
    const chappaAiRow = projectRows(root)[0];
    expect(chappaAiRow.querySelector(".chappa-status-pill")?.textContent).toBe("exited");
    // …the CARD badge persists until activation, while health (now the
    // summary) shows no aggregated badge of its own.
    expect(chappaAiRow.querySelector<HTMLElement>(".chappa-process-bell")!.style.display).toBe("inline");
    expect(summaries(root)[0].querySelector(".chappa-project-summary-name")?.textContent).toBe("health");
    expect(summaries(root)[0].querySelector<HTMLElement>(".chappa-rail-bell")!.style.display).toBe("none");

    // Activating the badged card finally clears it (the rule).
    click(chappaAiRow);
    expect(chappaAiRow.querySelector<HTMLElement>(".chappa-process-bell")!.style.display).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("the aggregated summary badge clears on expand, and nothing restarts", async () => {
    const { calls, root, app } = await multiProject();
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(1)); // chappa-ai's background `server`
    const summary = summaries(root)[0];
    expect(summary.querySelector<HTMLElement>(".chappa-rail-bell")!.style.display).toBe("inline");

    const startsBefore = calls.startProcess.length;
    click(summary); // expand chappa-ai
    // Expanding cleared the AGGREGATED badge: collapse chappa-ai again (expand
    // health) and its summary comes back clean.
    click(summaries(root)[0]); // health's summary → expand health
    const chappaAiSummary = summaries(root)[0];
    expect(chappaAiSummary.querySelector(".chappa-project-summary-name")?.textContent).toBe("chappa-ai");
    expect(chappaAiSummary.querySelector<HTMLElement>(".chappa-rail-bell")!.style.display).toBe("none");
    // Switching back and forth started/stopped NOTHING.
    expect(calls.startProcess.length).toBe(startsBefore);
    expect(calls.stopProcess).toEqual([]);
    panelDispose(app);
    root.remove();
  });

  it("plain workspace shells re-resolve to the newly EXPANDED project's default", async () => {
    const { calls, root, app } = await multiProject({
      projects: [
        { id: 1, name: "quiet", path: "/p/quiet", icon: null, notificationLevel: "none" },
        { id: 2, name: "loud", path: "/p/loud", icon: null, notificationLevel: null },
      ],
      processes: [],
    });
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();
    const d = drive(app);
    d.windowFocused = false; // the focused-terminal rule stays out of the way

    // Expanded = loud (null → 'all'): the mount shell (term 900) toasts.
    d.applyBell(bellEvent(900));
    expect(calls.osNotify).toEqual([["shell", "Bell"]]);

    // Expand quiet — an already-open id, so NO IPC — and the same shell now
    // resolves 'none': badge only.
    await app.openProject(1);
    expect(calls.openProject).toEqual([1, 2]);
    d.applyBell(bellEvent(900));
    expect(calls.osNotify).toEqual([["shell", "Bell"]]);
    panelDispose(app);
    root.remove();
  });

  it("explicit close confirms with the running list, stops, and tears down", async () => {
    const asked: Array<[string, string[]]> = [];
    const { calls, root, app } = await multiProject({
      confirmProjectClose: async (name, running) => {
        asked.push([name, running]);
        return true;
      },
    });
    await app.openProject(1); // auto-starts `server`
    await FLUSH();

    // The switcher menu's per-row × is the affordance for the EXPANDED
    // project.
    click(root.querySelector<HTMLButtonElement>(".chappa-project-switcher")!);
    const closeBtn = root.querySelector<HTMLButtonElement>(".chappa-project-close")!;
    expect(closeBtn.title).toContain('Close project "chappa-ai"');
    click(closeBtn);
    await FLUSH();

    expect(asked).toEqual([["chappa-ai", ["server"]]]);
    expect(calls.stopProcess).toEqual([[1, "server"]]);
    // The project is gone: section hidden, switcher reset, no summary row.
    expect(root.querySelector<HTMLElement>(".chappa-project-section")!.style.display).toBe("none");
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("Project…");
    expect(summaries(root).length).toBe(0);
    panelDispose(app);
    root.remove();
  });

  it("a refused close-project confirm stops nothing and keeps the project open", async () => {
    const { calls, root, app } = await multiProject({
      confirmProjectClose: async () => false,
    });
    await app.openProject(1);
    await FLUSH();
    await app.closeProjectFlow(1);
    expect(calls.stopProcess).toEqual([]);
    expect(root.querySelector<HTMLElement>(".chappa-project-section")!.style.display).toBe("block");
    panelDispose(app);
    root.remove();
  });

  it("skips the close confirm entirely when nothing is running", async () => {
    const asked: string[] = [];
    const { calls, root, app } = await multiProject({
      processes: [{ ...COLLIDING_PROCESSES[1] }], // only the never-started command
      confirmProjectClose: async (name) => {
        asked.push(name);
        return true;
      },
    });
    await app.openProject(1);
    await FLUSH();
    await app.closeProjectFlow(1);
    expect(asked).toEqual([]); // never asked
    expect(calls.stopProcess).toEqual([]);
    expect(root.querySelector<HTMLElement>(".chappa-project-section")!.style.display).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("closing a BACKGROUND project from its summary × leaves the expanded one alone", async () => {
    const asked: Array<[string, string[]]> = [];
    const { calls, root, app } = await multiProject({
      confirmProjectClose: async (name, running) => {
        asked.push([name, running]);
        return true;
      },
    });
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();

    click(summaries(root)[0].querySelector<HTMLButtonElement>(".chappa-project-summary-close")!);
    await FLUSH();
    expect(asked).toEqual([["chappa-ai", ["server"]]]);
    expect(calls.stopProcess).toEqual([[1, "server"]]);
    // Health stays expanded and untouched.
    expect(summaries(root).length).toBe(0);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("health");
    expect(projectRows(root).length).toBe(2);
    panelDispose(app);
    root.remove();
  });

  it("explicit close also closes the project's bound terminals (listed in the confirm)", async () => {
    const asked: Array<[string, string[]]> = [];
    const { api, root, app } = await projectTerminals();
    (app as unknown as { confirmProjectClose: (n: string, r: string[]) => Promise<boolean> }).confirmProjectClose =
      async (name: string, running: string[]) => {
        asked.push([name, running]);
        return true;
      };
    click(termAdd(root)); // bound terminal → term 701
    await FLUSH();
    await app.closeProjectFlow(1);
    await FLUSH();
    // The live bound terminal was named in the confirm and closed with the
    // project ("Project terminals of a closed project close too").
    expect(asked).toEqual([["chappa-ai", ["shell"]]]);
    expect(api.closeTerminal).toHaveBeenCalledWith(701);
    expect(termRows(root).length).toBe(0);
    panelDispose(app);
    root.remove();
  });

  it("closing the EXPANDED project expands the most recent remaining one", async () => {
    // Decided behaviour: the one-expanded invariant
    // holds whenever any project remains open.
    const { root, app } = await multiProject({
      confirmProjectClose: async () => true,
    });
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();
    await app.closeProjectFlow(2); // the expanded one
    await FLUSH();
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    expect(root.querySelector<HTMLElement>(".chappa-project-section")!.style.display).toBe("block");
    expect(summaries(root).length).toBe(0);
    panelDispose(app);
    root.remove();
  });

  it("app shutdown stops EVERY open project's processes, not just the expanded one", async () => {
    const { calls, app, root } = await multiProject();
    await app.openProject(1); // `server` → term 1
    await FLUSH();
    await app.openProject(2); // `server` → term 2
    await FLUSH();
    app.closeAll();
    const stops = calls.stopProcess.map(([id, name]) => `${id}:${name}`).sort();
    expect(stops).toEqual(["1:server", "2:server"]);
    root.remove();
  });

  it("project panels of a collapsed project stay alive in the stack (hidden)", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();
    // chappa-ai's `server` panel (term 1) is still an entry with a host in the
    // stack, hidden behind health's active panel.
    const entries = (app as unknown as { entries: Map<number, RailEntry> }).entries;
    expect(entries.has(1)).toBe(true);
    expect(entries.get(1)!.host.isConnected).toBe(true);
    expect(entries.get(1)!.host.classList.contains("active")).toBe(false);
    panelDispose(app);
    root.remove();
  });

  it("summary rows are a DIV with the expand button and × as SIBLINGS (valid HTML)", async () => {
    // The row used to be a <button> with the × nested inside it — invalid
    // HTML (renderProjectMenu's own rule). Now: div > [expand | ×].
    const { root, app } = await multiProject();
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2);
    await FLUSH();
    const summary = summaries(root)[0];
    expect(summary.tagName).not.toBe("BUTTON");
    const expand = summary.querySelector<HTMLButtonElement>(".chappa-project-summary-expand")!;
    const closeBtn = summary.querySelector<HTMLButtonElement>(".chappa-project-summary-close")!;
    expect(expand.tagName).toBe("BUTTON");
    expect(closeBtn.tagName).toBe("BUTTON");
    // Siblings, never nested: each button's closest button is itself.
    expect(expand.parentElement).toBe(summary);
    expect(closeBtn.parentElement).toBe(summary);
    expect(closeBtn.closest("button")).toBe(closeBtn);
    // The expand button's click (what Enter/Space synthesize) expands: it
    // bubbles to the row's delegated handler.
    click(expand);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    panelDispose(app);
    root.remove();
  });

  it("re-expanding keeps the ACTIVE accent on the still-active process card", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // chappa-ai's server → term 1
    await FLUSH();
    await app.openProject(2); // health's server → term 2, now active
    await FLUSH();

    click(summaries(root)[0]); // expand chappa-ai
    click(projectRows(root)[0]); // activate chappa-ai's server pane (term 1)
    expect(projectRows(root)[0].classList.contains("active")).toBe(true);

    click(summaries(root)[0]); // expand health (chappa-ai collapses)…
    click(summaries(root)[0]); // …and expand chappa-ai again: cards are REBUILT
    // The stack still fronts chappa-ai's server panel, so its rebuilt card must
    // carry the accent at build time (activate() ran before the rebuild).
    expect(projectRows(root)[0].classList.contains("active")).toBe(true);
    panelDispose(app);
    root.remove();
  });
});

// --- ACTIVE area ---------------------------------------------------

const activeSection = (root: HTMLElement): HTMLElement =>
  root.querySelector<HTMLElement>(".chappa-active-section")!;

const activeRows = (root: HTMLElement): HTMLElement[] => [
  ...root.querySelectorAll<HTMLElement>(".chappa-active-row"),
];

const activeNames = (root: HTMLElement): Array<string | undefined> =>
  activeRows(root).map((r) => r.querySelector(".chappa-rail-name")?.textContent ?? undefined);

const activeTags = (root: HTMLElement): Array<string | null> =>
  activeRows(root).map((r) => r.querySelector(".chappa-active-project")?.textContent ?? null);

describe("App ACTIVE area", () => {
  it("sits at the TOP of the rail, hidden while empty, shown when a process runs", async () => {
    const { root, app } = await multiProject();
    // The section is the rail's FIRST child — "at the top of the rail,
    // above the workspace TERMINALS".
    const rail = root.querySelector(".chappa-rail")!;
    expect(rail.firstElementChild?.classList.contains("chappa-active-section")).toBe(true);
    // Empty state: hidden ENTIRELY. The mount shell is running but carries
    // no badge — a plain live shell is not an ACTIVE row.
    expect(activeSection(root).style.display).toBe("none");

    await app.openProject(1); // trusted → auto-starts `server` (term 1)
    await FLUSH();
    expect(activeSection(root).style.display).toBe("block");
    // House header: uppercase label + right count.
    expect(activeSection(root).querySelector(".chappa-active-label")?.textContent).toBe("ACTIVE");
    expect(activeSection(root).querySelector(".chappa-active-count")?.textContent).toBe("1");
    const rows = activeRows(root);
    expect(rows.length).toBe(1);
    // Row anatomy: status dot + name; ONE project open → no owning-project tag.
    expect(rows[0].querySelector(".chappa-status-dot")).not.toBeNull();
    expect(rows[0].querySelector(".chappa-rail-name")?.textContent).toBe("server");
    expect(rows[0].querySelector(".chappa-active-project")).toBeNull();

    // The process exits: the section empties and hides again.
    const appAny = app as unknown as {
      applyProcessStatus: (p: ipc.ProjectProcessStatusEvent) => void;
    };
    appAny.applyProcessStatus({ project_id: 1, name: "server", status: "exited", exit_code: 0, term_id: 1 });
    expect(activeSection(root).style.display).toBe("none");
    expect(activeRows(root).length).toBe(0);
    panelDispose(app);
    root.remove();
  });

  it("counts a same-name process in two projects as two rows, tagged apart (keying)", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // chappa-ai's `server` → term 1
    await FLUSH();
    await app.openProject(2); // health's `server` → term 2
    await FLUSH();
    // Two rows, both named `server` — the (project, name) key holds — with
    // the muted owning-project tag now that MORE THAN ONE project is open.
    // Stable order: open projects in open order.
    expect(activeSection(root).querySelector(".chappa-active-count")?.textContent).toBe("2");
    expect(activeNames(root)).toEqual(["server", "server"]);
    expect(activeTags(root)).toEqual(["chappa-ai", "health"]);

    // ChappaAi's exits: only ITS row leaves — the same-name key must not bleed
    // across projects — and the survivor keeps its own tag.
    const appAny = app as unknown as {
      applyProcessStatus: (p: ipc.ProjectProcessStatusEvent) => void;
    };
    appAny.applyProcessStatus({ project_id: 1, name: "server", status: "exited", exit_code: 0, term_id: 1 });
    expect(activeNames(root)).toEqual(["server"]);
    expect(activeTags(root)).toEqual(["health"]);
    panelDispose(app);
    root.remove();
  });

  it("a BACKGROUND project's running process rows show with the project tag", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // chappa-ai `server` runs, then chappa-ai collapses…
    await FLUSH();
    await app.openProject(2); // …behind health
    await FLUSH();
    const chappaAiRow = activeRows(root)[0];
    expect(chappaAiRow.dataset.projectId).toBe("1");
    expect(chappaAiRow.querySelector(".chappa-active-project")?.textContent).toBe("chappa-ai");
    expect(chappaAiRow.querySelector(".chappa-status-dot")?.classList.contains("starting")).toBe(true);
    panelDispose(app);
    root.remove();
  });

  it("attention rows sort to the TOP and clear exactly when the badge clears (activate)", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // chappa-ai `server` → term 1
    await FLUSH();
    await app.openProject(2); // health `server` → term 2, active
    await FLUSH();
    // Park activation on the workspace shell so BOTH process panels are
    // hidden and can badge.
    click(root.querySelector(".chappa-rail-list .chappa-rail-item")!);
    expect(activeTags(root)).toEqual(["chappa-ai", "health"]);

    // A bell on health's hidden `server` (term 2): its row takes the
    // attention ring + marker and jumps ABOVE chappa-ai's.
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(2));
    let rows = activeRows(root);
    expect(activeTags(root)).toEqual(["health", "chappa-ai"]);
    expect(rows[0].classList.contains("attention")).toBe(true);
    expect(rows[0].querySelector(".chappa-active-attention")).not.toBeNull();
    expect(rows[1].classList.contains("attention")).toBe(false);
    expect(rows[1].querySelector(".chappa-active-attention")).toBeNull();

    // Clicking the attention row activates the terminal — which is what
    // clears the badge, and the attention flag READS that state (no
    // parallel store): the ring drops, the order returns to stable, and the
    // process CARD's own badge is gone too.
    click(rows[0]);
    expect(d.activeId).toBe(2);
    rows = activeRows(root);
    expect(activeTags(root)).toEqual(["chappa-ai", "health"]);
    expect(rows.some((r) => r.classList.contains("attention"))).toBe(false);
    expect(
      projectRows(root)[0].querySelector<HTMLElement>(".chappa-process-bell")!.style.display,
    ).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("clicking a background project's row expands THAT project and activates its terminal", async () => {
    const { calls, root, app } = await multiProject();
    await app.openProject(1); // chappa-ai `server` → term 1
    await FLUSH();
    await app.openProject(2); // health expanded, its `server` (term 2) active
    await FLUSH();
    const chappaAiRow = activeRows(root)[0];
    expect(chappaAiRow.querySelector(".chappa-active-project")?.textContent).toBe("chappa-ai");
    click(chappaAiRow);
    // ChappaAi expanded — from cache, NO IPC re-open (open_project resets the
    // runtime Rust-side) — and chappa-ai's own `server` panel took the activation,
    // not health's same-name one.
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    expect(calls.openProject).toEqual([1, 2]);
    expect(drive(app).activeId).toBe(1);
    expect(projectRows(root)[0].classList.contains("active")).toBe(true);
    panelDispose(app);
    root.remove();
  });

  it("a badged workspace shell appears as a row and leaves when activation clears it", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // `server` panel active; the mount shell hides
    await FLUSH();
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(900)); // the hidden mount shell
    const rows = activeRows(root);
    expect(activeSection(root).querySelector(".chappa-active-count")?.textContent).toBe("2");
    // The badged shell is an attention row: FIRST, ringed, and — having no
    // owning project — untagged.
    expect(activeNames(root)).toEqual(["shell", "server"]);
    expect(rows[0].classList.contains("attention")).toBe(true);
    expect(rows[0].querySelector(".chappa-active-project")).toBeNull();

    // Click → the shell activates; the badge clears, and a live shell
    // WITHOUT a badge is not an ACTIVE row — it leaves the section (the
    // rail row's own bell badge cleared with it: same store).
    click(rows[0]);
    expect(drive(app).activeId).toBe(900);
    expect(activeNames(root)).toEqual(["server"]);
    expect(root.querySelector(".chappa-rail-list .chappa-rail-bell")).toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("a badged BOUND terminal rows up tagged; its click expands the owner and activates it", async () => {
    const { root, app } = await projectTerminals({
      projects: [
        { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
        { id: 2, name: "health", path: "/p/health", icon: null, notificationLevel: null },
      ],
    });
    click(termAdd(root)); // bound terminal → term 701, in project 1
    await FLUSH();
    await app.openProject(2); // project 1 collapses; 701 stays alive…
    await FLUSH();
    // …but 701 is still the ACTIVE panel (nothing auto-started in project
    // 2): park activation on the workspace shell so it can badge at all.
    click(root.querySelector(".chappa-rail-list .chappa-rail-item")!);
    expect(activeRows(root).length).toBe(0); // alive but unbadged = not listed

    const d = drive(app);
    d.windowFocused = true;
    d.applyActivity({ event: "term://activity", term_id: 701, seq: 1 });
    d.applyBell(bellEvent(701));
    const rows = activeRows(root);
    expect(rows.length).toBe(1);
    expect(rows[0].dataset.kind).toBe("terminal");
    expect(rows[0].querySelector(".chappa-rail-name")?.textContent).toBe("shell");
    expect(rows[0].querySelector(".chappa-active-project")?.textContent).toBe("chappa-ai");
    expect(rows[0].classList.contains("attention")).toBe(true);
    // Activity renders its own dot alongside the attention marker.
    expect(rows[0].querySelector(".chappa-active-activity")).not.toBeNull();

    click(rows[0]);
    // The owning project expanded and the terminal took activation, which
    // cleared both badges — the row is gone and the TERMINALS subsection
    // row shows clean.
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    expect(drive(app).activeId).toBe(701);
    expect(activeRows(root).length).toBe(0);
    expect(termRows(root)[0].querySelector(".chappa-rail-bell")).toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("skips rebuilding when nothing visible changed (the activeSig guard)", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1);
    await FLUSH();
    const row = activeRows(root)[0];
    // A rail repaint with nothing visibly different keeps the SAME row node.
    (app as unknown as { renderRail: () => void }).renderRail();
    expect(activeRows(root)[0]).toBe(row);
    // A visible change (badge on the hidden mount shell) rebuilds.
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(900));
    expect(activeRows(root)[0]).not.toBe(row);
    panelDispose(app);
    root.remove();
  });

  it("closing a background project drops its rows from ACTIVE immediately", async () => {
    const { root, app } = await multiProject({
      confirmProjectClose: async () => true,
    });
    await app.openProject(1); // chappa-ai `server` runs
    await FLUSH();
    await app.openProject(2); // health `server` runs, expanded
    await FLUSH();
    expect(activeTags(root)).toEqual(["chappa-ai", "health"]);
    await app.closeProjectFlow(1); // background close: no renderRail path
    await FLUSH();
    expect(activeNames(root)).toEqual(["server"]);
    // Only ONE project is open again, so the owning-project tag hides.
    expect(activeTags(root)).toEqual([null]);
    panelDispose(app);
    root.remove();
  });
});

// --- notification banners + center ---------------------------------

const centerOf = (app: App): NotifyCenter =>
  (app as unknown as { center: NotifyCenter }).center;

const banners = (root: HTMLElement): HTMLElement[] => [
  ...root.querySelectorAll<HTMLElement>(".chappa-banner"),
];

const bellBtn = (root: HTMLElement): HTMLButtonElement =>
  root.querySelector<HTMLButtonElement>(".chappa-notify-bell")!;

const bellCount = (root: HTMLElement): HTMLElement =>
  root.querySelector<HTMLElement>(".chappa-notify-bell-count")!;

const drawerEl = (root: HTMLElement): HTMLElement | null =>
  root.querySelector<HTMLElement>(".chappa-drawer");

const drawerRows = (root: HTMLElement): HTMLElement[] => [
  ...root.querySelectorAll<HTMLElement>(".chappa-drawer-row"),
];

describe("App notifications — banners + center", () => {
  it("maps osNotify onto banner (focused) vs OS toast (unfocused); the center records both", async () => {
    // The routing matrix extension: decide() is unchanged; the CALLER splits
    // its osNotify by window focus. Term 1 (the auto-started process) is
    // active; the mount shell 900 is hidden.
    const { calls, root, app } = await openedProject();
    const d = drive(app);
    d.windowFocused = true;
    d.applyNotify(notifyEvent(900, "cargo", "done"));
    // FOCUSED: in-app banner, never an OS toast — the terminal is
    // necessarily non-active or the event would have been suppressed.
    expect(calls.osNotify).toEqual([]);
    let cards = banners(root);
    expect(cards.length).toBe(1);
    expect(cards[0].querySelector(".chappa-banner-title")?.textContent).toBe("cargo");
    expect(cards[0].querySelector(".chappa-banner-body")?.textContent).toBe("done");
    expect(cards[0].querySelector(".chappa-banner-attrib")?.textContent).toBe("shell");

    // UNFOCUSED: the OS toast, exactly as before — and no banner.
    d.windowFocused = false;
    d.applyNotify(notifyEvent(900, "cargo", "done again"));
    expect(calls.osNotify).toEqual([["cargo", "done again"]]);
    cards = banners(root);
    expect(cards.length).toBe(1);

    // Both non-suppressed events entered the center, newest first.
    expect(centerOf(app).entries.map((e) => e.body)).toEqual(["done again", "done"]);
    expect(centerOf(app).unread).toBe(2);
    panelDispose(app);
    root.remove();
  });

  it("a badge-only outcome records a center entry with NO banner and NO toast", async () => {
    const { calls, root, app } = await openedProject({
      projects: [{ id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: "none" }],
    });
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(900)); // hidden shell, level 'none' → badge only
    expect(calls.osNotify).toEqual([]);
    expect(banners(root).length).toBe(0);
    expect(root.querySelector(".chappa-rail-bell")).not.toBeNull();
    const entries = centerOf(app).entries;
    expect(entries.length).toBe(1);
    expect(entries[0].kind).toBe("bell");
    expect(entries[0].title).toBe("shell");
    expect(bellCount(root).textContent).toBe("1");
    expect(bellCount(root).style.display).toBe("block");
    panelDispose(app);
    root.remove();
  });

  it("a focused-suppressed event is NOT recorded — the measured semantics", async () => {
    // A focused terminal's event produces nothing at all, center included.
    const { calls, root, app } = await openedProject();
    const d = drive(app);
    d.windowFocused = true;
    expect(d.activeId).toBe(1); // the auto-started process pane
    d.applyBell(bellEvent(1));
    d.applyNotify(notifyEvent(1, "x", "y"));
    expect(centerOf(app).entries.length).toBe(0);
    expect(calls.osNotify).toEqual([]);
    expect(banners(root).length).toBe(0);
    expect(bellCount(root).style.display).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("attributes center entries: process · project, bound shell · project, plain shell name-only", async () => {
    const { root, app } = await projectTerminals(); // mount shell 700 (plain)
    click(termAdd(root)); // bound terminal → 701, project 1 "chappa-ai"
    await FLUSH();
    const rows = projectRows(root);
    click(rowButton(rows[0], "Start")); // `chappa-ai tui` process → term 1, active
    await FLUSH();
    const d = drive(app);
    d.windowFocused = false;
    d.applyBell(bellEvent(700)); // hidden plain shell
    d.applyBell(bellEvent(701)); // hidden bound shell
    click(root.querySelector(".chappa-rail-list .chappa-rail-item")!); // activate 700
    d.applyBell(bellEvent(1)); // the now-hidden process terminal
    expect(centerOf(app).entries.map((e) => [e.processName, e.projectId])).toEqual([
      ["chappa-ai tui", 1],
      ["shell", 1],
      ["shell", null],
    ]);
    panelDispose(app);
    root.remove();
  });

  it("An agent://reap notification records ONE center entry with the orphan text", async () => {
    const { root, app } = await openedProject();
    const d = drive(app) as unknown as { applyReap: (p: ipc.AgentReapEvent) => void };
    d.applyReap({ container: "dev-worker", count: 6 });
    const entries = centerOf(app).entries;
    expect(entries[0].title).toBe("Orphan reaper");
    expect(entries[0].body).toBe("Reaped 6 orphaned agent processes in dev-worker");
    expect(entries[0].kind).toBe("notify");
    expect(entries[0].processName).toBeNull(); // backend-originated, no terminal attribution
    expect(centerOf(app).unread).toBeGreaterThanOrEqual(1);
    panelDispose(app);
    root.remove();
  });

  it("stacks up to 3 banners, collapses older into +N more, hover-pauses, click focuses", async () => {
    const { root, app } = await openedProject();
    const d = drive(app);
    d.windowFocused = true;
    vi.useFakeTimers();
    try {
      for (let i = 1; i <= 5; i++) d.applyNotify(notifyEvent(900, `n${i}`, `b${i}`));
      let cards = banners(root);
      // Max 3 visible; the two OLDEST collapsed into "+2 more".
      expect(cards.length).toBe(3);
      expect(cards.map((c) => c.querySelector(".chappa-banner-title")?.textContent)).toEqual([
        "n3",
        "n4",
        "n5",
      ]);
      const more = root.querySelector<HTMLElement>(".chappa-banner-more")!;
      expect(more.style.display).toBe("block");
      expect(more.textContent).toBe("+2 more");

      // "+N more" opens the center drawer (the collapsed entries live there).
      click(more);
      expect(drawerEl(root)!.classList.contains("open")).toBe(true);
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      expect(drawerEl(root)!.classList.contains("open")).toBe(false);

      // Hover-pause: the hovered card survives past the ~5s auto-dismiss;
      // the two unhovered ones expire.
      cards[2].dispatchEvent(new MouseEvent("mouseenter"));
      vi.advanceTimersByTime(6000);
      cards = banners(root);
      expect(cards.length).toBe(1);
      expect(cards[0].querySelector(".chappa-banner-title")?.textContent).toBe("n5");
      // Leaving re-arms the full delay.
      cards[0].dispatchEvent(new MouseEvent("mouseleave"));
      vi.advanceTimersByTime(4999);
      expect(banners(root).length).toBe(1);
      vi.advanceTimersByTime(1);
      expect(banners(root).length).toBe(0);
      // The stack emptied, so the overflow row cleared with it.
      expect(more.style.display).toBe("none");

      // Click routing: focus the terminal (activate) + mark read + dismiss.
      d.applyNotify(notifyEvent(900, "n6", "b6"));
      const unreadBefore = centerOf(app).unread;
      click(banners(root)[0]);
      expect(d.activeId).toBe(900);
      expect(banners(root).length).toBe(0);
      expect(centerOf(app).unread).toBe(unreadBefore - 1);
    } finally {
      vi.useRealTimers();
    }
    panelDispose(app);
    root.remove();
  });

  it("a banner click on a project-scoped entry expands THAT project first", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1); // chappa-ai `server` → term 1
    await FLUSH();
    await app.openProject(2); // health expanded; chappa-ai collapses
    await FLUSH();
    const d = drive(app);
    d.windowFocused = true;
    d.applyBell(bellEvent(1)); // chappa-ai's hidden `server`
    const card = banners(root)[0];
    expect(card.querySelector(".chappa-banner-attrib")?.textContent).toBe("server · chappa-ai");
    click(card);
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("chappa-ai");
    expect(d.activeId).toBe(1);
    panelDispose(app);
    root.remove();
  });

  it("the drawer opens from the footer bell, marks read, clears, and dismisses via the helper", async () => {
    const { root, app } = await openedProject();
    // The bell sits in the rail footer NEXT TO the settings gear.
    expect(bellBtn(root).closest(".chappa-rail-footer")).toBe(
      root.querySelector(".chappa-rail-gear")!.closest(".chappa-rail-footer"),
    );
    const d = drive(app);
    d.windowFocused = false;
    d.applyBell(bellEvent(900));
    d.applyNotify(notifyEvent(900, "cargo", "done"));
    expect(bellCount(root).textContent).toBe("2");
    expect(bellCount(root).style.display).toBe("block");

    click(bellBtn(root)); // open
    const drawer = drawerEl(root)!;
    expect(drawer.classList.contains("open")).toBe(true);
    // The unread badge clears on open; the rows keep their dot for THIS open.
    expect(bellCount(root).style.display).toBe("none");
    let rows = drawerRows(root);
    expect(rows.length).toBe(2);
    // Newest-first with attribution + relative timestamp.
    expect(rows[0].querySelector(".chappa-drawer-row-title")?.textContent).toBe("cargo");
    expect(rows[1].querySelector(".chappa-drawer-row-title")?.textContent).toBe("shell");
    expect(rows[0].querySelector(".chappa-drawer-row-attrib")?.textContent).toBe("shell");
    expect(rows[0].querySelector(".chappa-drawer-row-time")?.textContent).toBe("just now");
    expect(rows[0].querySelector(".chappa-drawer-unread")).not.toBeNull();

    // Row click → focus the terminal + close the drawer.
    click(rows[0]);
    expect(drawer.classList.contains("open")).toBe(false);
    expect(d.activeId).toBe(900);

    // Reopen: everything read — no dots.
    click(bellBtn(root));
    rows = drawerRows(root);
    expect(rows[0].querySelector(".chappa-drawer-unread")).toBeNull();

    // Escape closes (the shared helper's opt-in half).
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(drawer.classList.contains("open")).toBe(false);

    // Outside mousedown closes; the bell anchor is excluded, so the bell
    // TOGGLES instead of dismiss-then-reopen (the ▾-menu trap).
    click(bellBtn(root));
    expect(drawer.classList.contains("open")).toBe(true);
    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(drawer.classList.contains("open")).toBe(false);
    click(bellBtn(root));
    expect(drawer.classList.contains("open")).toBe(true);
    click(bellBtn(root));
    expect(drawer.classList.contains("open")).toBe(false);

    // Clear all empties the center and the list.
    click(bellBtn(root));
    click(root.querySelector<HTMLButtonElement>(".chappa-drawer-clear")!);
    expect(centerOf(app).entries.length).toBe(0);
    expect(root.querySelector(".chappa-drawer-empty")).not.toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("caps the center ring at 200, newest first", async () => {
    const { root, app } = await openedProject();
    const d = drive(app);
    d.windowFocused = false;
    for (let i = 1; i <= 205; i++) d.applyNotify(notifyEvent(900, `n${i}`, `body ${i}`));
    const entries = centerOf(app).entries;
    expect(entries.length).toBe(200);
    expect(entries[0].title).toBe("n205");
    expect(entries[199].title).toBe("n6");
    panelDispose(app);
    root.remove();
  });
});

// --- chappa.yml write-back + the full command menu -----------------

/** Private App surface the tests drive directly (event appliers). */
interface YmlDriver {
  applyTitle(p: ipc.TerminalEventDto): void;
  applyYmlReloaded(p: ipc.ProjectYmlReloadedEvent): Promise<void>;
  entries: Map<number, RailEntry>;
}
const ymlDrive = (app: App): YmlDriver => app as unknown as YmlDriver;

/** Every visible menu row, top to bottom, submenu toggles included. */
function menuLabels(root: HTMLElement): string[] {
  const menu = root.querySelector<HTMLElement>(".chappa-row-menu")!;
  return [...menu.children].map((el) =>
    el.classList.contains("chappa-row-submenu-parent")
      ? el.querySelector(".chappa-row-menu-item")!.textContent!
      : el.textContent!,
  );
}

function menuItem(root: HTMLElement, cls: string): HTMLButtonElement {
  return root.querySelector<HTMLButtonElement>(`.chappa-row-menu .${cls}`)!;
}

function rowNames(root: HTMLElement): string[] {
  return [...root.querySelectorAll(".chappa-process-row .chappa-process-name")].map(
    (n) => n.textContent!,
  );
}

const FORM_TUI: CommandForm = {
  name: "chappa-ai tui",
  command: "py app/tui.py",
  workingDir: "",
  autoStart: false,
  autoRestart: false,
  restartWhenChanged: [],
  env: [],
};

describe("App chappa.yml write-back — the full command menu", () => {
  it("lists the command-row menu in its documented order, minus Clear output", async () => {
    const { root, app } = await openedProject();
    contextMenu(projectRows(root)[1]);
    expect(menuLabels(root)).toEqual([
      "Start",
      "Add to favorites",
      "Copy command",
      "Disable automatic renaming",
      "Lesser used",
      "Notification level ▸",
      "Edit command…",
      "Add ▸",
      "Duplicate to ▸",
      "Duplicate all commands to ▸",
      'Delete command "chappa-ai tui"',
    ]);
    // "Copy link" and "Clear output" are deliberately absent.
    expect(menuLabels(root).some((l) => /Copy link|Clear output/.test(l))).toBe(false);
    // The card button exists too, after the rows.
    const add = root.querySelector<HTMLButtonElement>(".chappa-process-add")!;
    expect(add.textContent).toBe("+ Add command");
    expect(add.disabled).toBe(false);
    panelDispose(app);
    root.remove();
  });

  it("Edit command… pre-fills from the row and saves with the ORIGINAL name (rename = key move)", async () => {
    const asked: Array<[CommandForm | null, string]> = [];
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptCommand: async (initial, title) => {
        asked.push([initial, title]);
        return {
          ...initial!,
          name: "tui (renamed)",
          command: "py app/tui.py --fast",
          workingDir: "app",
          restartWhenChanged: ["app/**/*.py"],
          env: [
            ["ZED", "1"],
            ["ALPHA", "2"],
          ],
        };
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    contextMenu(projectRows(root)[1]);
    menuItem(root, "chappa-row-menu-edit").click();
    await FLUSH();
    expect(asked).toEqual([[FORM_TUI, "Edit command"]]);
    expect(calls.saveProcess).toEqual([
      [
        1,
        "chappa-ai tui",
        {
          name: "tui (renamed)",
          command: "py app/tui.py --fast",
          workingDir: "app",
          autoStart: false,
          autoRestart: false,
          restartWhenChanged: ["app/**/*.py"],
          // Row order survives the form → wire conversion as an ORDERED pair
          // list (an object would be alphabetized Rust-side; review).
          env: [
            ["ZED", "1"],
            ["ALPHA", "2"],
          ],
        },
      ],
    ]);
    // The answer's rows are adopted: the renamed row sits where the old one was.
    expect(rowNames(root)).toEqual(["import + summarize on open", "tui (renamed)"]);
    expect(calls.saveProcess[0][2].env.map(([k]) => k)).toEqual(["ZED", "ALPHA"]);
    panelDispose(app);
    root.remove();
  });

  it("+ Add command (card button and Add ▸) opens an EMPTY editor and appends", async () => {
    const asked: Array<CommandForm | null> = [];
    let answer: CommandForm | null = null;
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptCommand: async (initial) => {
        asked.push(initial);
        return answer;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    // Cancelled editor: nothing written.
    click(root.querySelector<HTMLButtonElement>(".chappa-process-add")!);
    await FLUSH();
    expect(asked).toEqual([null]);
    expect(calls.saveProcess).toEqual([]);

    // Add ▸ → + Add command, this time with an answer.
    answer = { ...FORM_TUI, name: "docs", command: "mkdocs serve", autoStart: true };
    contextMenu(projectRows(root)[0]);
    root.querySelector<HTMLButtonElement>(".chappa-row-add-toggle")!.click();
    const item = root.querySelector<HTMLButtonElement>(".chappa-row-add-command")!;
    expect(item.textContent).toBe("+ Add command");
    item.click();
    await FLUSH();
    expect(calls.saveProcess.map((c) => [c[1], c[2].name])).toEqual([[null, "docs"]]);
    expect(rowNames(root)).toEqual(["import + summarize on open", "chappa-ai tui", "docs"]);
    // The lazily rendered submenu is emptied on close (no stale items).
    expect(root.querySelector(".chappa-row-add-command")).toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("Delete command confirms, then adopts the rows Rust answers", async () => {
    let allow = false;
    const asked: string[] = [];
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      confirmDeleteCommand: async (name) => {
        asked.push(name);
        return allow;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();

    contextMenu(projectRows(root)[1]);
    menuItem(root, "chappa-row-menu-delete").click();
    await FLUSH();
    expect(asked).toEqual(["chappa-ai tui"]);
    expect(calls.deleteProcess).toEqual([]);
    expect(rowNames(root).length).toBe(2);

    allow = true;
    contextMenu(projectRows(root)[1]);
    menuItem(root, "chappa-row-menu-delete").click();
    await FLUSH();
    expect(calls.deleteProcess).toEqual([[1, "chappa-ai tui"]]);
    expect(rowNames(root)).toEqual(["import + summarize on open"]);
    panelDispose(app);
    root.remove();
  });

  it("Duplicate to ▸ / Duplicate all commands to ▸ list every stored project and write ONLY the target", async () => {
    const { calls, root, app } = await openedProject({
      projects: [
        { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
        { id: 2, name: "other", path: "/p/other", icon: null, notificationLevel: null },
      ],
    });
    contextMenu(projectRows(root)[0]);
    root.querySelector<HTMLButtonElement>(".chappa-row-dup-toggle")!.click();
    let items = [...root.querySelectorAll<HTMLButtonElement>(".chappa-row-dup .chappa-row-menu-item")];
    // The current project is offered too (same-project copy → ` copy` suffix).
    expect(items.map((i) => [i.textContent, i.dataset.projectId])).toEqual([
      ["chappa-ai", "1"],
      ["other", "2"],
    ]);
    items[1].click();
    await FLUSH();
    expect(calls.duplicate).toEqual([[1, ["import + summarize on open"], 2]]);

    contextMenu(projectRows(root)[1]);
    root.querySelector<HTMLButtonElement>(".chappa-row-dup-all-toggle")!.click();
    items = [...root.querySelectorAll<HTMLButtonElement>(".chappa-row-dup-all .chappa-row-menu-item")];
    items[0].click();
    await FLUSH();
    // Duplicate all sends NO names: Rust enumerates the fresh source file.
    expect(calls.duplicate[1]).toEqual([1, [], 1]);
    // No save/delete ever reached the SOURCE through this path.
    expect(calls.saveProcess).toEqual([]);
    expect(calls.deleteProcess).toEqual([]);
    panelDispose(app);
    root.remove();
  });

  it("favorites persist chappa-side and sort first within COMMANDS", async () => {
    const { calls, root, app } = await openedProject();
    expect(rowNames(root)).toEqual(["import + summarize on open", "chappa-ai tui"]);
    const star = projectRows(root)[1].querySelector<HTMLButtonElement>(".chappa-process-star")!;
    expect(star.textContent).toBe("☆");
    click(star);
    expect(calls.favorite).toEqual([[1, "chappa-ai tui", true]]);
    // Re-rendered: the favorite moved to the top and the star is lit.
    expect(rowNames(root)).toEqual(["chappa-ai tui", "import + summarize on open"]);
    expect(projectRows(root)[0].querySelector(".chappa-process-star")!.textContent).toBe("★");
    // The menu reflects it, and toggles it back off.
    contextMenu(projectRows(root)[0]);
    const fav = menuItem(root, "chappa-row-menu-favorite");
    expect(fav.textContent).toBe("Remove from favorites");
    fav.click();
    expect(calls.favorite[1]).toEqual([1, "chappa-ai tui", false]);
    expect(rowNames(root)).toEqual(["import + summarize on open", "chappa-ai tui"]);
    // Never via the yml path.
    expect(calls.saveProcess).toEqual([]);
    panelDispose(app);
    root.remove();
  });

  it("Disable automatic renaming drops OSC titles for that process only", async () => {
    const { calls, root, app } = await openedProject();
    const d = ymlDrive(app);
    // The auto-started summarize row took term id 1 at open; starting the tui
    // row answers term id 2.
    click(rowButton(projectRows(root)[1], "Start"));
    await FLUSH();
    expect(d.entries.get(2)?.process?.name).toBe("chappa-ai tui");
    d.applyTitle({ event: "term://title", term_id: 2, seq: 0, title: "vim" });
    expect(d.entries.get(2)?.title).toBe("vim");

    contextMenu(projectRows(root)[1]);
    const toggle = menuItem(root, "chappa-row-menu-rename");
    expect(toggle.textContent).toBe("Disable automatic renaming");
    toggle.click();
    expect(calls.autoRename).toEqual([[1, "chappa-ai tui", true]]);
    d.applyTitle({ event: "term://title", term_id: 2, seq: 0, title: "htop" });
    expect(d.entries.get(2)?.title, "title event dropped while disabled").toBe("vim");
    // …only THAT process: the other row's terminal still renames.
    d.applyTitle({ event: "term://title", term_id: 1, seq: 0, title: "summarizing" });
    expect(d.entries.get(1)?.title).toBe("summarizing");

    contextMenu(projectRows(root)[1]);
    const back = menuItem(root, "chappa-row-menu-rename");
    expect(back.textContent).toBe("Enable automatic renaming");
    back.click();
    d.applyTitle({ event: "term://title", term_id: 2, seq: 0, title: "htop" });
    expect(d.entries.get(2)?.title).toBe("htop");
    expect(calls.saveProcess).toEqual([]);
    panelDispose(app);
    root.remove();
  });

  it("a chappa.yml load error disables every mutation with a visible reason until a reload succeeds", async () => {
    const { api, calls, root, app } = await openedProject();
    const d = ymlDrive(app);
    const errorEl = root.querySelector<HTMLElement>(".chappa-project-yml-error")!;
    expect(errorEl.style.display).toBe("none");

    await d.applyYmlReloaded({ project_id: 1, error: "chappa.yml: mapping values are not allowed" });
    expect(errorEl.style.display).toBe("block");
    expect(errorEl.textContent).toContain("mapping values are not allowed");
    expect(root.querySelector<HTMLButtonElement>(".chappa-process-add")!.disabled).toBe(true);
    contextMenu(projectRows(root)[0]);
    for (const cls of [
      "chappa-row-menu-edit",
      "chappa-row-menu-delete",
      "chappa-row-add-toggle",
      "chappa-row-dup-toggle",
      "chappa-row-dup-all-toggle",
    ]) {
      const item = menuItem(root, cls);
      expect(item.disabled, cls).toBe(true);
      expect(item.title).toContain("mapping values");
    }
    // Non-yml items stay usable.
    expect(menuItem(root, "chappa-row-menu-favorite").disabled).toBe(false);
    // Rows are untouched (Rust restarted/removed nothing).
    expect(rowNames(root).length).toBe(2);

    // A clean reload re-fetches the rows and re-enables everything.
    await d.applyYmlReloaded({ project_id: 1, error: null });
    await FLUSH();
    expect(api.listProjectProcesses).toHaveBeenCalledWith(1);
    expect(errorEl.style.display).toBe("none");
    expect(root.querySelector<HTMLButtonElement>(".chappa-process-add")!.disabled).toBe(false);
    contextMenu(projectRows(root)[0]);
    expect(menuItem(root, "chappa-row-menu-edit").disabled).toBe(false);
    expect(calls.saveProcess).toEqual([]);
    panelDispose(app);
    root.remove();
  });

  it("a reload keeps surviving rows' panels, disposes vanished ones, and follows the file order", async () => {
    const { api, root, app } = await openedProject();
    const d = ymlDrive(app);
    // Term 1 = the auto-started summarize row; term 2 = the tui row.
    click(rowButton(projectRows(root)[1], "Start"));
    await FLUSH();
    expect(d.entries.has(1) && d.entries.has(2)).toBe(true);

    // The file was edited on disk: tui first, summarize renamed away, a new
    // one added.
    const before = await api.listProjectProcesses(1);
    const tui = { ...before[1], termId: 2, status: "running" };
    const fresh = { ...before[0], name: "design doc sync", command: "py app/design_sync.py" };
    (api.listProjectProcesses as ReturnType<typeof vi.fn>).mockResolvedValueOnce([tui, fresh]);
    await d.applyYmlReloaded({ project_id: 1, error: null });
    await FLUSH();
    expect(rowNames(root)).toEqual(["chappa-ai tui", "design doc sync"]);
    // The running row kept its terminal (restart-in-place keeps the id)…
    expect(d.entries.get(2)?.process?.name).toBe("chappa-ai tui");
    expect(projectRows(root)[0].querySelector(".chappa-status-pill")!.textContent).toBe("running");
    // …the vanished (running) row's panel is disposed with it…
    expect(d.entries.has(1)).toBe(false);
    // …the vanished row is gone, and the new one is a never-started card.
    expect(projectRows(root)[1].querySelector(".chappa-status-pill")!.textContent).toBe("stopped");
    panelDispose(app);
    root.remove();
  });

  it("a stale reload snapshot never disposes a panel a newer source kept (seq guard)", async () => {
    const { api, root, app } = await openedProject();
    const d = ymlDrive(app);
    click(rowButton(projectRows(root)[1], "Start"));
    await FLUSH();
    expect(d.entries.has(2)).toBe(true);

    // Reload #1's fetch is slow and answers an OLD snapshot (tui stopped,
    // no terminal); reload #2 answers the truth (tui running on term 2).
    const before = await api.listProjectProcesses(1);
    const stale = [before[0], { ...before[1], termId: null, status: "stopped" }];
    const truth = [before[0], { ...before[1], termId: 2, status: "running" }];
    let releaseStale!: (rows: ipc.ProjectProcessDto[]) => void;
    const list = api.listProjectProcesses as ReturnType<typeof vi.fn>;
    list.mockImplementationOnce(
      () => new Promise<ipc.ProjectProcessDto[]>((resolve) => (releaseStale = resolve)),
    );
    list.mockResolvedValueOnce(truth);
    const first = d.applyYmlReloaded({ project_id: 1, error: null });
    await d.applyYmlReloaded({ project_id: 1, error: null });
    await FLUSH();
    expect(d.entries.has(2)).toBe(true);
    releaseStale(stale);
    await first;
    await FLUSH();
    expect(d.entries.has(2), "stale snapshot dropped").toBe(true);
    expect(projectRows(root)[1].querySelector(".chappa-status-pill")!.textContent).toBe("running");
    panelDispose(app);
    root.remove();
  });

  it("a reload of UNTRUSTED content re-arms the trust gate with the held-back commands", async () => {
    const asked: string[][] = [];
    const { api, calls } = projectApi();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async (commands) => {
        asked.push(commands);
        return true;
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    calls.confirmTrust.length = 0;
    const d = ymlDrive(app);
    await d.applyYmlReloaded({
      project_id: 1,
      error: null,
      trust_pending: true,
      trust_commands: ["py app/tui.py --evil"],
    });
    await FLUSH();
    expect(asked).toEqual([["py app/tui.py --evil"]]);
    expect(calls.confirmTrust).toEqual([[1, true]]);
    // Trusted content: no dialog, nothing recorded again.
    await d.applyYmlReloaded({ project_id: 1, error: null, trust_pending: false });
    await FLUSH();
    expect(asked.length).toBe(1);
    expect(calls.confirmTrust.length).toBe(1);
    panelDispose(app);
    root.remove();
  });

  it("a refused save surfaces the reason and leaves the rows alone", async () => {
    const shown: string[] = [];
    const { api } = projectApi();
    api.saveProjectProcess = vi.fn(async () => {
      throw new Error("a command named `chappa-ai tui` already exists");
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptCommand: async () => ({ ...FORM_TUI, name: "chappa-ai tui" }),
      showError: async (m) => {
        shown.push(m);
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    contextMenu(projectRows(root)[0]);
    menuItem(root, "chappa-row-menu-edit").click();
    await FLUSH();
    expect(shown.length).toBe(1);
    expect(shown[0]).toContain("already exists");
    expect(rowNames(root)).toEqual(["import + summarize on open", "chappa-ai tui"]);
    panelDispose(app);
    root.remove();
  });

  it("the changed-on-disk refusal re-opens the editor with the same values", async () => {
    // Concurrent-change guard: Rust refuses with "chappa.yml changed
    // on disk — re-apply your edit" (and reloads). The modal must come back
    // pre-filled with what the user typed, so re-apply is one click.
    const shown: string[] = [];
    const prompted: Array<CommandForm | null> = [];
    const { api, calls } = projectApi();
    let refusals = 1;
    api.saveProjectProcess = vi.fn(async (id: number, originalName: string | null, def: ipc.ProcessDefDto) => {
      calls.saveProcess.push([id, originalName, def]);
      if (refusals-- > 0) throw new Error("chappa.yml changed on disk — re-apply your edit");
      return [];
    });
    const form = { ...FORM_TUI, name: "edited name", command: "py edited.py" };
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({
      root,
      api,
      confirm: async () => false,
      confirmProjectTrust: async () => false,
      promptCommand: async (initial) => {
        prompted.push(initial);
        return form;
      },
      showError: async (m) => {
        shown.push(m);
      },
    });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    contextMenu(projectRows(root)[0]);
    menuItem(root, "chappa-row-menu-edit").click();
    await FLUSH();
    // First prompt was the row's definition; the re-open carries the FORM.
    expect(prompted.length).toBe(2);
    expect(prompted[1]).toEqual(form);
    expect(shown.length).toBe(1);
    expect(shown[0]).toContain("changed on disk");
    // The re-apply saved with the same original name.
    expect(calls.saveProcess.length).toBe(2);
    expect(calls.saveProcess[1][1]).toBe(calls.saveProcess[0][1]);
    panelDispose(app);
    root.remove();
  });
});

// --- process stats --------------------------------------------------

/** The `term://stats` handler, reached the established way (see NotifyDriver). */
type StatsDriver = { applyStats: (p: ipc.TerminalStatsEvent) => void };

const statsDrive = (app: App): StatsDriver => app as unknown as StatsDriver;

const statsEvent = (
  id: number,
  cpu: number,
  mem: number,
  subprocs: number,
): ipc.TerminalStatsEvent => ({
  event: "term://stats",
  term_id: id,
  seq: 1,
  cpu_pct: cpu,
  mem_bytes: mem,
  subproc_count: subprocs,
});

/** Mount one shell (explicitly, blank start) and mark it running. */
async function statsApp(): Promise<{ root: HTMLElement; app: App }> {
  const { api } = stubApi();
  const root = document.createElement("div");
  const app = new App({ root, api, confirm: async () => false });
  await app.mount();
  await app.newShell();
  const entries = (app as unknown as { entries: Map<number, RailEntry> }).entries;
  entries.get(1)!.status = "running";
  (app as unknown as { renderRail: () => void }).renderRail();
  return { root, app };
}

const cpuChip = (root: HTMLElement): HTMLElement =>
  root.querySelector<HTMLElement>(".chappa-rail-list .chappa-rail-cpu")!;

describe("rail process stats", () => {
  it("reserves the chip's space, so a row cannot reflow or re-truncate", async () => {
    // Stats appearing and disappearing must not
    // jitter or re-truncate row labels. The guarantee is structural — the chip
    // span is in the row at a fixed width whether or not it has text — so this
    // asserts the ANATOMY is identical in both states (jsdom has no layout to
    // measure).
    const { root, app } = await statsApp();
    const row = (): HTMLElement =>
      root.querySelector<HTMLElement>(".chappa-rail-list .chappa-rail-item")!;
    const anatomy = (): string[] => [...row().children].map((c) => c.className);

    const idle = anatomy();
    expect(cpuChip(root)).not.toBeNull();
    expect(cpuChip(root).textContent).toBe("");
    expect(cpuChip(root).title).toBe("");
    const name = row().querySelector(".chappa-rail-name")!.textContent;

    statsDrive(app).applyStats(statsEvent(1, 12.4, 50 * 1024 * 1024, 2));
    expect(cpuChip(root).textContent).toBe("12%");
    // Same children, same order, same label: only the chip's TEXT changed.
    expect(anatomy()).toEqual(idle);
    expect(row().querySelector(".chappa-rail-name")!.textContent).toBe(name);

    // …and back to idle on the exit's final zero.
    statsDrive(app).applyStats(statsEvent(1, 0, 0, 0));
    expect(cpuChip(root).textContent).toBe("");
    expect(anatomy()).toEqual(idle);
    panelDispose(app);
  });

  it("tooltips memory + subprocess count, and only while running", async () => {
    const { root, app } = await statsApp();
    statsDrive(app).applyStats(statsEvent(1, 7.6, 1536 * 1024 * 1024, 1));
    expect(cpuChip(root).textContent).toBe("8%");
    expect(cpuChip(root).title).toBe("1.5 GB · 1 subprocess");

    // An exited terminal's last reading is stale — never shown as live.
    const entries = (app as unknown as { entries: Map<number, RailEntry> }).entries;
    entries.get(1)!.status = "exited";
    (app as unknown as { renderRail: () => void }).renderRail();
    expect(cpuChip(root).textContent).toBe("");
    expect(cpuChip(root).title).toBe("");
    panelDispose(app);
  });

  it("feeds the panel hint bar and repaints nothing on an unchanged payload", async () => {
    const { root, app } = await statsApp();
    const hint = (): HTMLElement => root.querySelector<HTMLElement>(".chappa-hint-bar")!;
    expect(hint().style.display).toBe("none");

    statsDrive(app).applyStats(statsEvent(1, 12.4, 50 * 1024 * 1024, 3));
    expect(hint().textContent).toBe("3 subprocesses");
    expect(hint().style.display).toBe("block");

    // Unchanged payload → the store ignores it, so nothing repaints. Proven by
    // marking the DOM and checking the mark survives the second apply.
    cpuChip(root).dataset.mark = "1";
    statsDrive(app).applyStats(statsEvent(1, 12.4, 50 * 1024 * 1024, 3));
    expect(cpuChip(root).dataset.mark).toBe("1");

    // Down to one child, then none: the bar clears rather than saying "0".
    statsDrive(app).applyStats(statsEvent(1, 12.4, 50 * 1024 * 1024, 1));
    expect(hint().textContent).toBe("1 subprocess");
    statsDrive(app).applyStats(statsEvent(1, 0, 0, 0));
    expect(hint().textContent).toBe("");
    expect(hint().style.display).toBe("none");
    panelDispose(app);
  });

  it("ignores stats for a terminal it does not have", async () => {
    const { root, app } = await statsApp();
    statsDrive(app).applyStats(statsEvent(99, 50, 1024 * 1024 * 1024, 4));
    expect(cpuChip(root).textContent).toBe("");
    panelDispose(app);
  });
});

// --- agents --------------------------------------------------------

describe("App agents", () => {
  function agentApi(tools: ipc.AgentToolDto[]): AgentToolsApi {
    return {
      listAgentTools: vi.fn(async () => ({ tools: tools.map((t) => ({ ...t })), machine_mode_types: ["claude", "opencode"] as const })),
      upsertAgentTool: vi.fn(async (t) => t),
      deleteAgentTool: vi.fn(async () => {}),
      parseAgentCommand: vi.fn(async () => ({ tool: emptyAgentTool(), docker_detected: false, warnings: [] })),
    };
  }

  async function mountedWithAgents(tools: ipc.AgentToolDto[], closeAnswer: string | null = null) {
    const { api, ids } = stubApi();
    (api.closeTerminal as ReturnType<typeof vi.fn>).mockImplementation(async () => closeAnswer);
    const store = new AgentToolsStore(agentApi(tools));
    await store.load();
    const root = document.createElement("div");
    const app = new App({ root, api, settings: stubSettings().store, agentTools: store, confirm: async () => true });
    await app.mount();
    await app.newShell(); // blank start: the mount shell (row 0)
    return { api, ids, root, app, store };
  }

  type BridgeDriver = { applyBridge: (p: ipc.AgentBridgeEvent) => void; activeId: number | null };
  const bridgeDrive = (app: App): BridgeDriver => app as unknown as BridgeDriver;

  it("the New agent menu lists ENABLED tools only, with the model", async () => {
    const { root, app } = await mountedWithAgents([
      dockerTool(3),
      dockerTool(4, { name: "large", model: "model-large", enabled: false }),
      dockerTool(5, { name: "claude host", model: null, runtime: { kind: "host" } }),
    ]);
    const btn = root.querySelector<HTMLButtonElement>(".chappa-new-agent")!;
    expect(btn.textContent).toBe("New agent ▸");
    btn.click();
    const options = [...root.querySelectorAll<HTMLButtonElement>(".chappa-new-agent-option")];
    expect(options.map((o) => o.textContent)).toEqual(["worker · model-fast", "claude host"]);
    expect(options.map((o) => o.dataset.toolId)).toEqual(["3", "5"]);
    panelDispose(app);
  });

  it("an empty registry says so instead of offering nothing silently", async () => {
    const { root, app } = await mountedWithAgents([]);
    root.querySelector<HTMLButtonElement>(".chappa-new-agent")!.click();
    expect(root.querySelector(".chappa-new-agent-empty")?.textContent).toMatch(/No enabled agent tools/);
    panelDispose(app);
  });

  it("a COLD store refreshes on menu open — tools added elsewhere appear without visiting Settings", async () => {
    // Regression (2026-08-31): boot never loads the agent-tools store,
    // so a fresh session showed "No enabled agent tools" for a registry that
    // was anything but — until the settings pane happened to load the list.
    const { api } = stubApi();
    const store = new AgentToolsStore(agentApi([dockerTool(3)])); // NOT loaded
    const root = document.createElement("div");
    const app = new App({ root, api, settings: stubSettings().store, agentTools: store, confirm: async () => true });
    await app.mount();
    root.querySelector<HTMLButtonElement>(".chappa-new-agent")!.click();
    // Instant fill from the cold cache may be empty; the on-open refresh must
    // repopulate the still-open menu.
    await FLUSH();
    const options = [...root.querySelectorAll<HTMLButtonElement>(".chappa-new-agent-option")];
    expect(options.map((o) => o.dataset.toolId)).toEqual(["3"]);
    expect(root.querySelector(".chappa-new-agent-empty")).toBeNull();
    panelDispose(app);
  });

  it("spawning renders the agent row with a model tag and a container dot; a bridge fault rings it and lists it in ACTIVE", async () => {
    const { api, root, app } = await mountedWithAgents([dockerTool(3)]);
    root.querySelector<HTMLButtonElement>(".chappa-new-agent")!.click();
    root.querySelector<HTMLButtonElement>(".chappa-new-agent-option")!.click();
    await FLUSH();
    expect(api.spawnAgent).toHaveBeenCalledTimes(1);
    const req = (api.spawnAgent as ReturnType<typeof vi.fn>).mock.calls[0][0] as ipc.SpawnAgentRequestDto;
    expect(req.agent_tool_id).toBe(3);
    expect(req.prompt).toBeUndefined();
    const rows = root.querySelectorAll<HTMLElement>(".chappa-rail-item");
    expect(rows.length).toBe(2);
    const row = rows[1];
    expect(row.classList.contains("agent")).toBe(true);
    expect(row.querySelector(".chappa-rail-name")?.textContent).toBe("worker");
    expect(row.querySelector(".chappa-rail-model")?.textContent).toBe("model-fast");
    const dot = row.querySelector<HTMLElement>(".chappa-rail-container")!;
    expect(dot.classList.contains("running")).toBe(true);
    expect(dot.title).toMatch(/dev-worker: running · up 2m · bridge ok/);
    expect(row.classList.contains("attention")).toBe(false);
    // Not in ACTIVE yet: no badge, no fault.
    expect(root.querySelectorAll(".chappa-active-row").length).toBe(0);

    // The probe reports container-down → attention ring (badge tier), an
    // ACTIVE row, the dot flips, the tooltip says so.
    const id = bridgeDrive(app).activeId!;
    bridgeDrive(app).applyBridge({
      event: "agent://bridge",
      term_id: id,
      id,
      bridge: "container-down",
      container: { status: "exited", started_at: null, uptime_s: null },
    });
    const faulted = root.querySelectorAll<HTMLElement>(".chappa-rail-item")[1];
    expect(faulted.classList.contains("attention")).toBe(true);
    expect(faulted.dataset.bridge).toBe("container-down");
    expect(faulted.querySelector<HTMLElement>(".chappa-rail-container")!.classList.contains("exited")).toBe(true);
    expect(faulted.querySelector<HTMLElement>(".chappa-rail-container")!.title).toMatch(/exited.*bridge container-down/);
    const active = root.querySelectorAll<HTMLElement>(".chappa-active-row");
    expect(active.length).toBe(1);
    expect(active[0].classList.contains("attention")).toBe(true);

    // Back to ok → the ring and the ACTIVE row go.
    bridgeDrive(app).applyBridge({
      event: "agent://bridge",
      term_id: id,
      id,
      bridge: "ok",
      container: { status: "running", started_at: null, uptime_s: 5 },
    });
    expect(root.querySelectorAll<HTMLElement>(".chappa-rail-item")[1].classList.contains("attention")).toBe(false);
    expect(root.querySelectorAll(".chappa-active-row").length).toBe(0);
    panelDispose(app);
  });

  it("closing an agent toasts a non-`gone` verification and stays silent on `gone`", async () => {
    const { api, root, app } = await mountedWithAgents([dockerTool(3)], "still-present");
    await app.spawnAgent(3);
    await FLUSH();
    const agentId = bridgeDrive(app).activeId!;
    await app.closePanel(agentId);
    await FLUSH();
    // ONE close call — the panel must not close a second time on dispose.
    expect(api.closeTerminal).toHaveBeenCalledTimes(1);
    expect(api.closeTerminal).toHaveBeenCalledWith(agentId);
    const toast = root.querySelector<HTMLElement>(".chappa-toast")!;
    expect(toast.textContent).toMatch(/worker: container-side process still-present/);
    expect(toast.dataset.kind).toBe("still-present");

    // `gone` (or null for a host tool): no toast.
    (api.closeTerminal as ReturnType<typeof vi.fn>).mockImplementation(async () => "gone");
    await app.spawnAgent(3);
    await FLUSH();
    await app.closePanel(bridgeDrive(app).activeId!);
    await FLUSH();
    expect(root.querySelectorAll(".chappa-toast").length).toBe(1);
    panelDispose(app);
  });

  it("review: an agent spawned into the expanded project carries its projectId — attribution, and Close project closes it", async () => {
    const projects = [{ id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null }];
    const { api } = projectApi({ projects });
    api.openProject = vi.fn(async (id: number) => ({
      project: projects.find((p) => p.id === id)!,
      trustPending: false,
      trustCommands: [],
      processes: [TERM_COMMAND],
    }));
    const asked: string[][] = [];
    const root = document.createElement("div");
    document.body.appendChild(root);
    const store = new AgentToolsStore(agentApi([dockerTool(3)]));
    await store.load();
    const app = new App({ root, api, settings: stubSettings().store, agentTools: store, confirm: async () => true });
    (app as unknown as { confirmProjectClose: (n: string, r: string[]) => Promise<boolean> }).confirmProjectClose =
      async (_name: string, running: string[]) => {
        asked.push(running);
        return true;
      };
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    const resp = await app.spawnAgent(3);
    await FLUSH();
    // The request named the expanded project and the entry remembers it.
    const req = (api.spawnAgent as ReturnType<typeof vi.fn>).mock.calls[0][0] as ipc.SpawnAgentRequestDto;
    expect(req.project_id).toBe(1);
    const agentId = resp!.term_id;
    type Internals = {
      entries: Map<number, RailEntry>;
      centerAttribution: (e: RailEntry) => { projectId: number | null; processName: string | null };
      collectActiveRows: () => ActiveRow[];
      applyBridge: (p: ipc.AgentBridgeEvent) => void;
    };
    const internals = app as unknown as Internals;
    const entry = internals.entries.get(agentId)!;
    expect(entry.kind).toBe("agent");
    expect(entry.agent?.projectId).toBe(1);
    // Attribution: the notification center / ACTIVE rows name the project.
    expect(internals.centerAttribution(entry)).toEqual({ projectId: 1, processName: "worker" });
    internals.applyBridge({ event: "agent://bridge", term_id: agentId, id: agentId, bridge: "stale", container: { status: "running", started_at: null, uptime_s: 5 } });
    const active = internals.collectActiveRows().find((r) => r.termId === agentId)!;
    expect(active.projectId).toBe(1);
    // Close project: the agent is listed in the confirm and closed with it.
    await app.closeProjectFlow(1);
    await FLUSH();
    expect(asked).toEqual([["worker"]]);
    expect(api.closeTerminal).toHaveBeenCalledWith(agentId);
    expect(internals.entries.has(agentId)).toBe(false);
    panelDispose(app);
    root.remove();
  });

  // ---- Json transport ----------------------------------------------

  type PhaseBInternals = {
    pendingAgentEvents: Map<number, ipc.AgentEventEvent[]>;
    setAgentWaiting: (id: number, waiting: boolean) => boolean;
    entries: Map<number, RailEntry>;
    collectActiveRows: () => ActiveRow[];
    applyAgentEvent: (p: ipc.AgentEventEvent) => void;
    applyBell: (p: ipc.TerminalEventDto) => void;
    center: NotifyCenter;
    windowFocused: boolean;
    activeId: number | null;
  };
  const phaseB = (app: App): PhaseBInternals => app as unknown as PhaseBInternals;
  const jsonTool = (id = 7): ipc.AgentToolDto =>
    dockerTool(id, { name: "claude json", tool_type: "claude", program: "claude", args: [], model: "opus", runtime: { kind: "host" }, transport: "json" });
  /** The stub's spawn answers with the docker tool's block; make it answer
   *  with the JSON tool's block for tool 7 (transport, model, host runtime). */
  function spawnFor(api: TerminalApi, tools: ipc.AgentToolDto[]): void {
    let next = 900;
    (api.spawnAgent as ReturnType<typeof vi.fn>).mockImplementation(async (req: ipc.SpawnAgentRequestDto) => {
      const id = next++;
      const resp = fakeSpawnResponse(id, req);
      const tool = tools.find((t) => t.id === req.agent_tool_id);
      if (!tool) return resp;
      return {
        ...resp,
        name: tool.name,
        container: tool.runtime.kind === "docker_exec" ? resp.container : undefined,
        agent: { ...resp.agent, model: tool.model, runtime: tool.runtime, transport: tool.transport, tool_type: tool.tool_type },
      };
    });
  }
  const ev = (id: number, seq: number, kind: ipc.AgentEventKind, payload: Record<string, unknown> = {}): ipc.AgentEventEvent => ({
    event: "agent://event",
    term_id: id,
    id,
    seq,
    ts: 1,
    kind,
    payload,
  });

  it("a json-transport tool spawns a TRANSCRIPT view (no terminal) with a `json` rail tag; the box sends through send_agent_input", async () => {
    const { api, root, app } = await mountedWithAgents([jsonTool()]);
    spawnFor(api, [jsonTool()]);
    const resp = await app.spawnAgent(7);
    await FLUSH();
    const id = resp!.term_id;
    const req = (api.spawnAgent as ReturnType<typeof vi.fn>).mock.calls[0][0] as ipc.SpawnAgentRequestDto;
    expect(req.agent_tool_id).toBe(7);
    expect(req.cols).toBeUndefined();
    // The stack host carries the transcript, not a terminal viewport.
    const host = phaseB(app).entries.get(id)!.host;
    expect(host.classList.contains("chappa-transcript-host")).toBe(true);
    expect(host.querySelector(".chappa-transcript")).not.toBeNull();
    expect(host.querySelector(".chappa-transcript-input")).not.toBeNull();
    expect(host.querySelector("canvas, .chappa-term-viewport")).toBeNull();
    // The ring was replayed on start.
    expect(api.getAgentEvents).toHaveBeenCalledWith(id, 0);
    // Rail: the agent row shows the model tag AND the json tag.
    const row = [...root.querySelectorAll<HTMLElement>(".chappa-rail-item")].find((r) => r.classList.contains("agent"))!;
    expect(row.querySelector(".chappa-rail-model")?.textContent).toBe("opus");
    expect(row.querySelector(".chappa-rail-transport")?.textContent).toBe("json");
    expect(row.classList.contains("attention")).toBe(false);
    // Typing + Enter in the box → send_agent_input(id, text), echoed as "you".
    const input = host.querySelector<HTMLTextAreaElement>(".chappa-transcript-input")!;
    input.value = "what is 2+2";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    await FLUSH();
    expect(api.sendAgentInput).toHaveBeenCalledWith(id, "what is 2+2");
    expect(host.querySelector(".chappa-transcript-you")?.textContent).toBe("what is 2+2");
    expect(input.value).toBe("");
    // Shift+Enter does NOT send.
    input.value = "multi";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true, cancelable: true }));
    await FLUSH();
    expect(api.sendAgentInput).toHaveBeenCalledTimes(1);
    panelDispose(app);
  });

  it("awaiting_input → `agent-waiting` attention (rail ring, ACTIVE first, center entry via the notify decision); the next send clears it, so does turn_started", async () => {
    const { api, root, app } = await mountedWithAgents([jsonTool(), dockerTool(3)]);
    spawnFor(api, [jsonTool(), dockerTool(3)]);
    // A tty agent with a plain badge, to prove the agent-waiting row sorts ABOVE it.
    const badged = (await app.spawnAgent(3))!.term_id;
    await FLUSH();
    // A tty agent gets a terminal, never the tag.
    expect(root.querySelector(".chappa-rail-transport")).toBeNull();
    expect(phaseB(app).entries.get(badged)!.host.classList.contains("chappa-transcript-host")).toBe(false);
    const resp = await app.spawnAgent(7);
    await FLUSH();
    const id = resp!.term_id;
    const internals = phaseB(app);
    // Badge the tty agent (it is hidden: the json agent is active).
    internals.applyBell({ event: "term://bell", term_id: badged, seq: 1 });
    // Events land in the transcript; awaiting_input flips the row.
    internals.windowFocused = false;
    const centerBefore = internals.center.entries.length;
    internals.applyAgentEvent(ev(id, 1, "turn_started", { model: "opus" }));
    internals.applyAgentEvent(ev(id, 2, "text", { text: "4" }));
    internals.applyAgentEvent(ev(id, 3, "usage", { input_tokens: 12000, output_tokens: 3, context_pct: 6.1 }));
    internals.applyAgentEvent(ev(id, 4, "turn_ended", {}));
    internals.applyAgentEvent(ev(id, 5, "awaiting_input", { after: "result" }));
    const host = internals.entries.get(id)!.host;
    expect(host.querySelector(".chappa-transcript-sep")?.textContent).toBe("turn 1 · opus");
    expect(host.querySelector(".chappa-transcript-text")?.textContent).toBe("4");
    expect(host.querySelector(".chappa-transcript-usage")?.textContent).toBe("in 12.0k · out 3 · ctx 6.1%");
    expect(host.querySelector<HTMLElement>(".chappa-transcript-waiting")!.style.display).not.toBe("none");
    const row = [...root.querySelectorAll<HTMLElement>(".chappa-rail-item")].find((r) => r.querySelector(".chappa-rail-transport"))!;
    expect(row.classList.contains("attention")).toBe(true);
    expect(row.dataset.attention).toBe("agent-waiting");
    expect(internals.entries.get(id)!.attention).toBe("agent-waiting");
    // ACTIVE: the agent-waiting row is FIRST, the badged tty agent after it.
    const active = internals.collectActiveRows();
    expect(active.map((r) => [r.termId, r.attention])).toEqual([
      [id, "agent-waiting"],
      [badged, "badge"],
    ]);
    const activeRows = root.querySelectorAll<HTMLElement>(".chappa-active-row");
    expect(activeRows[0].dataset.attention).toBe("agent-waiting");
    expect(activeRows[0].querySelector(".chappa-active-attention")?.textContent).toBe("⌨");
    // The notification center recorded it through the SAME notify decision
    // (window unfocused → badge + toast path), attributed to the agent.
    expect(internals.center.entries.length).toBe(centerBefore + 1);
    expect(internals.center.entries[0].termId).toBe(id);
    expect(internals.center.entries[0].body).toBe("waiting for your input");
    // Sending from the box clears the ring at once (before any turn_started).
    const input = host.querySelector<HTMLTextAreaElement>(".chappa-transcript-input")!;
    input.value = "thanks";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    await FLUSH();
    expect(internals.entries.get(id)!.attention).toBeNull();
    // The typed tier is gone; what remains is the ordinary notify badge the
    // decision function set (window unfocused), which activation clears —
    // the existing machinery, untouched.
    expect(internals.collectActiveRows().find((r) => r.termId === id)?.attention).toBe("badge");
    expect(host.querySelector<HTMLElement>(".chappa-transcript-waiting")!.style.display).toBe("none");
    // An MCP send we never saw: the CLI's turn_started clears it too.
    internals.applyAgentEvent(ev(id, 6, "awaiting_input", {}));
    expect(internals.entries.get(id)!.attention).toBe("agent-waiting");
    internals.applyAgentEvent(ev(id, 7, "turn_started", {}));
    expect(internals.entries.get(id)!.attention).toBeNull();
    // The rail re-rendered: the fresh row carries no agent-waiting ring.
    const fresh = [...root.querySelectorAll<HTMLElement>(".chappa-rail-item")].find((r) => r.querySelector(".chappa-rail-transport"))!;
    expect(fresh.dataset.attention).toBeUndefined();
    // Duplicate seq (replay + live) renders once.
    internals.applyAgentEvent(ev(id, 7, "turn_started", {}));
    expect(host.querySelectorAll(".chappa-transcript-sep").length).toBe(2);
    panelDispose(app);
  });
  it("review fix: events for an id not yet adopted are buffered and replayed after adopt; the spawn response's awaiting_input badges through the SAME notify path", async () => {
    const { api, root, app } = await mountedWithAgents([jsonTool()]);
    const internals = phaseB(app);
    internals.windowFocused = false;
    const centerBefore = internals.center.entries.length;
    // The Rust side emits from the first line — BEFORE the spawn response
    // resolves. The stub delivers three events for the id it is about to
    // answer with, then answers.
    let next = 950;
    (api.spawnAgent as ReturnType<typeof vi.fn>).mockImplementation(async (req: ipc.SpawnAgentRequestDto) => {
      const id = next++;
      internals.applyAgentEvent(ev(id, 1, "turn_started", { model: "opus" }));
      internals.applyAgentEvent(ev(id, 2, "text", { text: "early" }));
      internals.applyAgentEvent(ev(id, 3, "awaiting_input", { after: "result" }));
      expect(internals.pendingAgentEvents.get(id)?.length).toBe(3);
      const resp = fakeSpawnResponse(id, req);
      return { ...resp, name: "claude json", container: undefined, agent: { ...resp.agent, transport: "json", tool_type: "claude", runtime: { kind: "host" } } };
    });
    const resp = await app.spawnAgent(7);
    await FLUSH();
    const id = resp!.term_id;
    expect(internals.pendingAgentEvents.has(id)).toBe(false);
    const host = internals.entries.get(id)!.host;
    // The buffered history rendered (in order, once) — the transcript held
    // it through its own replay, then merged.
    expect(host.querySelector(".chappa-transcript-sep")?.textContent).toBe("turn 1 · opus");
    expect(host.querySelector(".chappa-transcript-text")?.textContent).toBe("early");
    expect(host.querySelectorAll("[data-seq]").length).toBe(2);
    // The replayed awaiting_input went through the normal path: ring + center.
    expect(internals.entries.get(id)!.attention).toBe("agent-waiting");
    const row = [...root.querySelectorAll<HTMLElement>(".chappa-rail-item")].find((r) => r.querySelector(".chappa-rail-transport"))!;
    expect(row.dataset.attention).toBe("agent-waiting");
    expect(internals.center.entries.length).toBe(centerBefore + 1);
    expect(internals.center.entries[0].termId).toBe(id);
    // A duplicate live delivery of the buffered seq renders nothing new.
    internals.applyAgentEvent(ev(id, 2, "text", { text: "early" }));
    expect(host.querySelectorAll(".chappa-transcript-text").length).toBe(1);

    // adopt() with `awaiting_input: true` in the spawn response (an agent
    // that was already waiting) runs the same decision: badge + center.
    (api.spawnAgent as ReturnType<typeof vi.fn>).mockImplementation(async (req: ipc.SpawnAgentRequestDto) => {
      const resp = fakeSpawnResponse(next++, req);
      return { ...resp, name: "claude json", container: undefined, agent: { ...resp.agent, transport: "json", tool_type: "claude", runtime: { kind: "host" }, awaiting_input: true } };
    });
    const second = (await app.spawnAgent(7))!.term_id;
    await FLUSH();
    expect(internals.entries.get(second)!.attention).toBe("agent-waiting");
    expect(internals.center.entries.length).toBe(centerBefore + 2);
    expect(internals.center.entries[0].termId).toBe(second);
    // The same fact arriving again as a live event is idempotent (no
    // second center entry) — and turn_started clears through the reducer.
    internals.applyAgentEvent(ev(second, 1, "awaiting_input", {}));
    expect(internals.center.entries.length).toBe(centerBefore + 2);
    internals.applyAgentEvent(ev(second, 2, "turn_started", {}));
    expect(internals.entries.get(second)!.attention).toBeNull();
    panelDispose(app);
  });

  it("review fix: a dead entry is never `agent-waiting` — the status reducer drops the tier, setAgentWaiting refuses, the buffer is bounded", async () => {
    const { api, app } = await mountedWithAgents([jsonTool()]);
    spawnFor(api, [jsonTool()]);
    const internals = phaseB(app);
    const id = (await app.spawnAgent(7))!.term_id;
    await FLUSH();
    internals.applyAgentEvent(ev(id, 1, "awaiting_input", {}));
    expect(internals.entries.get(id)!.attention).toBe("agent-waiting");
    expect(internals.collectActiveRows().find((r) => r.termId === id)?.attention).toBe("agent-waiting");
    // The child exits: the reducer clears the tier with the status.
    const entry = internals.entries.get(id)!;
    const dead = railReducer(entry, { type: "status", status: "exited", exitCode: 1 });
    expect(dead.attention).toBeNull();
    internals.entries.set(id, dead);
    expect(internals.setAgentWaiting(id, true)).toBe(false);
    expect(internals.entries.get(id)!.attention).toBeNull();
    expect(railReducer(dead, { type: "attention", attention: "agent-waiting" }).attention).toBeNull();
    // A live entry takes the tier; activation does NOT clear it (it clears
    // on the next send / turn_started, not on a click).
    const alive = railReducer(entry, { type: "attention", attention: "agent-waiting" });
    expect(railReducer(alive, { type: "activate" }).attention).toBe("agent-waiting");
    // The pre-adopt buffer is bounded per id and across ids.
    for (let i = 0; i < PRE_ADOPT_EVENTS_PER_ID + 10; i++) internals.applyAgentEvent(ev(5000, i + 1, "text", { text: String(i) }));
    expect(internals.pendingAgentEvents.get(5000)!.length).toBe(PRE_ADOPT_EVENTS_PER_ID);
    expect(internals.pendingAgentEvents.get(5000)![0].seq).toBe(11);
    for (let k = 1; k <= PRE_ADOPT_EVENT_IDS_MAX; k++) internals.applyAgentEvent(ev(6000 + k, 1, "text", {}));
    expect(internals.pendingAgentEvents.size).toBe(PRE_ADOPT_EVENT_IDS_MAX);
    expect(internals.pendingAgentEvents.has(5000)).toBe(false);
    panelDispose(app);
  });
});

// --- project AGENTS section ----------------------------------------

describe("App project AGENTS section", () => {
  /** Two stored projects so a switch away (the "closing the project
   *  (switching away)") is drivable. */
  const PROJECTS: ipc.ProjectInfoDto[] = [
    { id: 1, name: "chappa-ai", path: "/p/chappa-ai", icon: null, notificationLevel: null },
    { id: 2, name: "dev", path: "/p/dev", icon: null, notificationLevel: null },
  ];

  function toolsApi(tools: ipc.AgentToolDto[]): AgentToolsApi {
    return {
      listAgentTools: vi.fn(async () => ({ tools: tools.map((t) => ({ ...t })), machine_mode_types: ["claude", "opencode"] as const })),
      upsertAgentTool: vi.fn(async (t) => t),
      deleteAgentTool: vi.fn(async () => {}),
      parseAgentCommand: vi.fn(async () => ({ tool: emptyAgentTool(), docker_detected: false, warnings: [] })),
    };
  }

  async function mountedWithProjects() {
    const { api } = projectApi({ projects: PROJECTS });
    // A fresh open per project (no yml commands): the AGENTS subsection is
    // the interesting surface, not COMMANDS.
    api.openProject = vi.fn(async (id: number) => ({
      project: PROJECTS.find((p) => p.id === id)!,
      trustPending: false,
      trustCommands: [],
      processes: [],
    }));
    const store = new AgentToolsStore(toolsApi([dockerTool(3)]));
    await store.load();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({ root, api, settings: stubSettings().store, agentTools: store, confirm: async () => true });
    await app.mount();
    await app.newShell(); // blank start: the workspace "shell" row
    return { api, root, app };
  }

  const agentsSection = (root: HTMLElement): HTMLElement =>
    root.querySelector<HTMLElement>(".chappa-project-section .chappa-project-agents")!;
  const agentRows = (root: HTMLElement): HTMLElement[] => [
    ...root.querySelectorAll<HTMLElement>(".chappa-project-agents .chappa-rail-item"),
  ];
  const railRows = (root: HTMLElement): HTMLElement[] => [
    ...root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item"),
  ];

  it("a bound agent renders under its open project with the N/M count and is absent from the workspace list", async () => {
    const { root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();
    const resp = await app.spawnAgent(3);
    await FLUSH();
    const agentId = resp!.term_id;

    // The section: the AGENTS header, the running/total count, and the
    // bound row — which is NOT in the workspace rail (no double render).
    const section = agentsSection(root);
    expect(section.querySelector(".chappa-subsection-label")?.textContent).toBe("AGENTS");
    expect(section.querySelector(".chappa-subsection-count")?.textContent).toBe("1/1");
    const rows = agentRows(root);
    expect(rows.length).toBe(1);
    const row = rows[0];
    expect(row.classList.contains("agent")).toBe(true);
    expect(row.querySelector(".chappa-rail-name")?.textContent).toBe("worker");
    expect(row.querySelector(".chappa-rail-model")?.textContent).toBe("model-fast");
    expect(railRows(root).map((r) => r.querySelector(".chappa-rail-name")?.textContent)).toEqual(["shell"]);

    // Section order: AGENTS sits between TERMINALS and SCRATCHPADS (i.e.
    // after COMMANDS), in the expanded project's section only.
    const sec = root.querySelector<HTMLElement>(".chappa-project-section")!;
    const idx = (cls: string): number => [...sec.children].findIndex((c) => c.classList.contains(cls));
    expect(idx("chappa-project-terms")).toBeLessThan(idx("chappa-project-agents"));
    expect(idx("chappa-project-processes")).toBeLessThan(idx("chappa-project-agents"));
    expect(idx("chappa-project-agents")).toBeLessThan(idx("chappa-scratchpads"));

    // Counts live-update on the same events that repaint the rows: the
    // agent exits → N drops, M holds, and the row repaints with the code.
    const entries = (app as unknown as { entries: Map<number, RailEntry> }).entries;
    entries.set(agentId, railReducer(entries.get(agentId)!, { type: "status", status: "exited", exitCode: 3 }));
    (app as unknown as { renderRail: () => void }).renderRail();
    expect(section.querySelector(".chappa-subsection-count")?.textContent).toBe("0/1");
    const freshRow = agentRows(root)[0];
    expect(freshRow.querySelector(".chappa-exit-code")?.textContent).toBe("3");

    // Click = focus its panel (the shared row's behaviour).
    freshRow.click();
    expect(freshRow.classList.contains("active")).toBe(true);
    panelDispose(app);
    root.remove();
  });

  it("switching away returns the bound row to the workspace list; switching back restores it", async () => {
    const { root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();
    await app.spawnAgent(3);
    await FLUSH();
    expect(agentRows(root).length).toBe(1);
    expect(railRows(root).some((r) => r.classList.contains("agent"))).toBe(false);

    // Switch away (the "closing the project (switching away)"): the
    // section hides with the rest of project 1's UI — and the row must not
    // vanish from the rail, it falls back to the workspace list.
    await app.openProject(2);
    await FLUSH();
    expect(root.querySelector<HTMLElement>(".chappa-project-section")!.style.display).toBe("block");
    // The expanded project is now dev: ITS section shows the 0/0 header +
    // spawner with zero rows.
    expect(agentsSection(root).querySelector(".chappa-subsection-label")?.textContent).toBe("AGENTS");
    expect(agentsSection(root).querySelector(".chappa-subsection-count")?.textContent).toBe("0/0");
    expect(agentRows(root).length).toBe(0);
    const names = railRows(root).map((r) => r.querySelector(".chappa-rail-name")?.textContent);
    expect(names).toContain("worker");
    expect(names).toContain("shell");

    // Switch back: the row goes home under project 1's AGENTS.
    await app.openProject(1);
    await FLUSH();
    expect(agentRows(root).length).toBe(1);
    expect(agentRows(root)[0].querySelector(".chappa-rail-name")?.textContent).toBe("worker");
    expect(railRows(root).some((r) => r.classList.contains("agent"))).toBe(false);
    expect(agentsSection(root).querySelector(".chappa-subsection-count")?.textContent).toBe("1/1");
    panelDispose(app);
    root.remove();
  });

  it("an agent of a never-opened project stays in the workspace list", async () => {
    const { root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();
    // Spawn into project 99 — a binding that names a project the app never
    // opened: no AGENTS section exists for it, so the row stays in the
    // workspace rail.
    await app.spawnAgent(3, 99);
    await FLUSH();
    const names = railRows(root).map((r) => r.querySelector(".chappa-rail-name")?.textContent);
    expect(names).toContain("worker");
    expect(names).toContain("shell");
    expect(agentRows(root).length).toBe(0);
    expect(agentsSection(root).querySelector(".chappa-subsection-count")?.textContent).toBe("0/0");
    // The binding is still remembered on the entry (attribution untouched).
    const entries = (app as unknown as { entries: Map<number, RailEntry> }).entries;
    const agent = [...entries.values()].find((e) => e.kind === "agent")!;
    expect(agent.agent?.projectId).toBe(99);
    panelDispose(app);
    root.remove();
  });

  it("the section spawner spawns with THAT project's id; the workspace spawner keeps its current binding", async () => {
    const { api, root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();

    // The section's own "New agent ▸": the same affordance, pick spawns with
    // the section's project id.
    const spawner = agentsSection(root).querySelector<HTMLButtonElement>(".chappa-new-agent")!;
    expect(spawner.textContent).toBe("New agent ▸");
    spawner.click();
    const sectionMenu = agentsSection(root).querySelector<HTMLElement>(".chappa-new-agent-menu")!;
    expect(sectionMenu.style.display).toBe("block");
    expect(sectionMenu.querySelector(".chappa-new-agent-option")?.textContent).toBe("worker · model-fast");
    sectionMenu.querySelector<HTMLButtonElement>(".chappa-new-agent-option")!.click();
    await FLUSH();
    expect(api.spawnAgent).toHaveBeenCalledTimes(1);
    const req = (api.spawnAgent as ReturnType<typeof vi.fn>).mock.calls[0][0] as ipc.SpawnAgentRequestDto;
    expect(req.agent_tool_id).toBe(3);
    expect(req.project_id).toBe(1);
    // The spawn listed under the section with the count.
    expect(agentRows(root).length).toBe(1);
    expect(agentsSection(root).querySelector(".chappa-subsection-count")?.textContent).toBe("1/1");

    // One "New agent ▸" menu at a time: opening the workspace spawner's menu
    // closes the section's.
    const wsSpawner = root.querySelector<HTMLButtonElement>(".chappa-rail-header .chappa-new-agent")!;
    wsSpawner.click();
    expect(agentsSection(root).querySelector<HTMLElement>(".chappa-new-agent-menu")!.style.display).toBe("none");
    const wsMenu = root.querySelector<HTMLElement>(".chappa-rail-header .chappa-new-agent-menu")!;
    expect(wsMenu.style.display).toBe("block");
    // The workspace spawner keeps the binding (the expanded project,
    // via its fallback chain) — the same id, a different code path.
    wsMenu.querySelector<HTMLButtonElement>(".chappa-new-agent-option")!.click();
    await FLUSH();
    const req2 = (api.spawnAgent as ReturnType<typeof vi.fn>).mock.calls[1][0] as ipc.SpawnAgentRequestDto;
    expect(req2.agent_tool_id).toBe(3);
    expect(req2.project_id).toBe(1);
    // Both bound agents listed under the section; both counts follow.
    expect(agentRows(root).length).toBe(2);
    expect(agentsSection(root).querySelector(".chappa-subsection-count")?.textContent).toBe("2/2");
    expect(railRows(root).some((r) => r.classList.contains("agent"))).toBe(false);
    panelDispose(app);
    root.remove();
  });

  it("0/0 still renders the header + spawner and no rows (no empty-state text)", async () => {
    const { root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();
    const section = agentsSection(root);
    expect(section.querySelector(".chappa-subsection-label")?.textContent).toBe("AGENTS");
    expect(section.querySelector(".chappa-subsection-count")?.textContent).toBe("0/0");
    // The spawner is live at 0/0 — discoverability is the point.
    const spawner = section.querySelector<HTMLButtonElement>(".chappa-new-agent")!;
    expect(spawner.textContent).toBe("New agent ▸");
    spawner.click();
    expect(section.querySelector<HTMLElement>(".chappa-new-agent-menu")!.style.display).toBe("block");
    // Zero rows under the header, and no empty-state text.
    const list = section.querySelector<HTMLElement>(".chappa-project-term-list")!;
    expect(list.querySelectorAll(".chappa-rail-item").length).toBe(0);
    expect(list.textContent).toBe("");
    panelDispose(app);
    root.remove();
  });

  it("badge/attention markers still appear on the project-section row", async () => {
    const { root, app } = await mountedWithProjects();
    await app.openProject(1);
    await FLUSH();
    const resp = await app.spawnAgent(3);
    await FLUSH();
    const agentId = resp!.term_id;
    // The badge rule: shown while HIDDEN — activate the mount shell
    // so the agent is not the active panel.
    app.activate(900);
    (app as unknown as { applyBell: (p: ipc.TerminalEventDto) => void }).applyBell({
      event: "term://bell",
      term_id: agentId,
      seq: 1,
    });
    let row = agentRows(root)[0];
    expect(row.querySelector(".chappa-rail-bell")).not.toBeNull();
    // The bridge fault's attention ring lands on the section row
    // too.
    (app as unknown as { applyBridge: (p: ipc.AgentBridgeEvent) => void }).applyBridge({
      event: "agent://bridge",
      term_id: agentId,
      id: agentId,
      bridge: "stale",
      container: { status: "running", started_at: null, uptime_s: 5 },
    });
    row = agentRows(root)[0];
    expect(row.classList.contains("attention")).toBe(true);
    expect(row.dataset.bridge).toBe("stale");
    // The rest of the workspace row's anatomy rides along verbatim (model
    // tag, container dot).
    expect(row.querySelector(".chappa-rail-model")?.textContent).toBe("model-fast");
    expect(row.querySelector(".chappa-rail-container")).not.toBeNull();
    // No copy of the row in the workspace rail (single render).
    expect(railRows(root).some((r) => r.classList.contains("agent"))).toBe(false);
    panelDispose(app);
    root.remove();
  });
});

// --- Edit-project pane + project context menu --------------------
//
// Every project gets an Edit-project pane
// (full-window chrome) (full-window chrome), with entry points on the
// expanded header and collapsed summary rows, and moves the interim sync and
// rail notification picker into it.

const ctxItems = (root: HTMLElement): HTMLButtonElement[] => [
  ...root.querySelectorAll<HTMLButtonElement>(".chappa-project-ctx .chappa-row-menu-item"),
];

describe("App edit-project pane + project context menu", () => {
  it("right-clicks the expanded HEADER for Edit project… / Close project / Remove project…", async () => {
    const { root, app } = await openedProject();
    const menu = root.querySelector<HTMLElement>(".chappa-project-ctx")!;
    expect(menu.style.display).toBe("none");
    contextMenu(root.querySelector<HTMLElement>(".chappa-project-header")!);
    expect(menu.style.display).toBe("block");
    expect(ctxItems(root).map((i) => i.textContent)).toEqual([
      "Edit project…",
      "Close project",
      "Remove project…",
    ]);
    panelDispose(app);
    root.remove();
  });

  it("right-clicks a collapsed SUMMARY row for the same three entries", async () => {
    const { root, app } = await multiProject();
    await app.openProject(1);
    await FLUSH();
    await app.openProject(2); // project 1 collapses
    await FLUSH();
    const summary = summaries(root)[0];
    expect(summary).not.toBeNull();
    const menu = root.querySelector<HTMLElement>(".chappa-project-ctx")!;
    expect(menu.style.display).toBe("none");
    contextMenu(summary);
    expect(menu.style.display).toBe("block");
    expect(ctxItems(root).map((i) => i.textContent)).toEqual([
      "Edit project…",
      "Close project",
      "Remove project…",
    ]);
    panelDispose(app);
    root.remove();
  });

  it("Edit project… opens the pane with the right values", async () => {
    const { root, app } = await openedProject();
    contextMenu(root.querySelector<HTMLElement>(".chappa-project-header")!);
    root.querySelector<HTMLButtonElement>('[data-action="Edit project…"]')!.click();
    const pane = app.editPaneElement!;
    expect(pane).not.toBeNull();
    expect(pane.style.display).toBe("flex");
    // Title, path, running/total counts, name field, project-file row.
    expect(pane.textContent).toContain("chappa-ai");
    expect(pane.querySelector('[data-value="path"]')?.textContent).toBe("/p/chappa-ai");
    // The auto-start command is `starting` on open → 1 Running · 2 Total.
    expect(pane.querySelector('[data-value="counts"]')?.textContent).toBe("1 Running · 2 Total");
    expect(pane.querySelector<HTMLInputElement>('[data-field="name"]')!.value).toBe("chappa-ai");
    expect(pane.textContent).toContain("chappa.yml");
    panelDispose(app);
    root.remove();
  });

  it("renaming via the pane calls rename_project and updates the header/switcher", async () => {
    const { calls, root, app } = await openedProject();
    app.openEditPane(1);
    const nameInput = app.editPaneElement!.querySelector<HTMLInputElement>('[data-field="name"]')!;
    nameInput.value = "Zeta";
    nameInput.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(calls.renameProject).toEqual([[1, "Zeta"]]);
    expect(root.querySelector(".chappa-project-name")?.textContent).toBe("Zeta");
    expect(root.querySelector(".chappa-project-switcher")?.textContent).toBe("Zeta");
    expect(app.editPaneElement!.querySelector("[data-value='path']")).not.toBeNull();
    panelDispose(app);
    root.remove();
  });

  it("a blank rename reverts instead of calling rename_project", async () => {
    const { calls, root, app } = await openedProject();
    app.openEditPane(1);
    const nameInput = app.editPaneElement!.querySelector<HTMLInputElement>('[data-field="name"]')!;
    nameInput.value = "   ";
    nameInput.dispatchEvent(new Event("change"));
    await Promise.resolve();
    expect(calls.renameProject).toEqual([]);
    expect(nameInput.value).toBe("chappa-ai");
    panelDispose(app);
    root.remove();
  });

  it("counts row reflects running/total and repaints live while open", async () => {
    const { root, app } = await openedProject();
    app.openEditPane(1);
    const counts = () =>
      app.editPaneElement!.querySelector('[data-value="counts"]')!.textContent;
    // The auto-start command is `starting`; start the second to prove repaint.
    expect(counts()).toBe("1 Running · 2 Total");
    click(rowButton(projectRows(root)[1], "Start"));
    await FLUSH();
    expect(counts()).toBe("2 Running · 2 Total");
    panelDispose(app);
    root.remove();
  });

  it("Escape and × both close the pane", async () => {
    const { root, app } = await openedProject();
    app.openEditPane(1);
    expect(app.editPaneElement!.style.display).toBe("flex");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(app.editPaneElement!.style.display).toBe("none");
    app.openEditPane(1);
    app.editPaneElement!.querySelector<HTMLButtonElement>(".chappa-edit-close")!.click();
    expect(app.editPaneElement!.style.display).toBe("none");
    panelDispose(app);
    root.remove();
  });

  it("the rail no longer contains the interim sync row or the notify row", async () => {
    const { root, app } = await openedProject();
    // The interim homes are gone from the project section.
    expect(root.querySelector(".chappa-project-settings-row")).toBeNull();
    expect(root.querySelector(".chappa-project-notify")).toBeNull();
    expect(root.querySelector(".chappa-project-sync")).toBeNull();
    // The section still starts with the two-row header and keeps its
    // subsections.
    const kids = [...root.querySelector(".chappa-project-section")!.children];
    expect(kids[0].className).toBe("chappa-project-header");
    expect(root.querySelector(".chappa-project-terms")).not.toBeNull();
    expect(root.querySelector(".chappa-project-processes")).not.toBeNull();
    panelDispose(app);
    root.remove();
  });
});

// --- workspace commands -------------------------------------------

/** A workspace-command DTO for the test stubs. */
function wsCmdDto(
  name: string,
  command: string,
  over: Partial<ipc.WorkspaceCommandDto> = {},
): ipc.WorkspaceCommandDto {
  return {
    name,
    command,
    status: "stopped",
    autoStart: false,
    autoRestart: false,
    restartWhenChanged: [],
    termId: null,
    exitCode: null,
    workingDir: null,
    env: {},
    ...over,
  };
}

async function workspaceApp(
  commands: ipc.WorkspaceCommandDto[] = [],
): Promise<{ api: TerminalApi; root: HTMLElement; app: App }> {
  const { api } = stubApi();
  vi.mocked(api.listWorkspaceCommands).mockResolvedValue(commands);
  const root = document.createElement("div");
  const app = new App({ root, api, confirm: async () => false });
  await app.mount();
  return { api, root, app };
}

describe("App workspace commands", () => {
  it("renders the COMMANDS section with a count, directly above the TERMINALS header", async () => {
    const { root, app } = await workspaceApp([
      wsCmdDto("watch", "npm run watch", { autoStart: true }),
      wsCmdDto("server", "python -m http.server"),
    ]);
    const section = root.querySelector<HTMLElement>(".chappa-workspace-cmds")!;
    expect(section).not.toBeNull();
    expect(section.querySelector(".chappa-subsection-label")?.textContent).toBe("COMMANDS");
    expect(section.querySelector(".chappa-subsection-count")?.textContent).toBe("2");

    // COMMANDS sits DIRECTLY ABOVE the workspace TERMINALS header.
    const rail = root.querySelector<HTMLElement>(".chappa-rail")!;
    const children = [...rail.children];
    const cIdx = children.indexOf(section);
    const tIdx = children.indexOf(root.querySelector(".chappa-rail-header")!);
    expect(cIdx).toBe(tIdx - 1);

    // Rows reuse the process-card anatomy (name + command text).
    const names = [...section.querySelectorAll(".chappa-process-name")].map((n) => n.textContent);
    expect(names).toEqual(["watch", "server"]);
    expect(section.querySelector(".chappa-process-cmd")?.textContent).toBe("npm run watch");
    // AUTO badge reflects autoStart.
    expect(section.querySelector(".chappa-process-badge.auto")).not.toBeNull();
    panelDispose(app);
  });

  it("adds, edits and deletes commands through the editor modal", async () => {
    const { api } = stubApi();
    const prompted: Array<[CommandForm | null, string]> = [];
    const form = (): CommandForm => ({
      name: "watch",
      command: "npm run watch",
      workingDir: "",
      autoStart: false,
      autoRestart: false,
      restartWhenChanged: [],
      env: [],
    });
    vi.mocked(api.listWorkspaceCommands).mockResolvedValue([]);
    const root = document.createElement("div");
    const app = new App({
      root,
      api,
      confirm: async () => false,
      promptWorkspaceCommand: async (initial, title) => {
        prompted.push([initial, title]);
        return form();
      },
      confirmDeleteCommand: async () => true,
    });
    await app.mount();

    // Add: "+ Add command" opens the editor with null initial; the answer is
    // saved (the stub answers the row list as Rust does).
    vi.mocked(api.saveWorkspaceCommand).mockResolvedValue([wsCmdDto("watch", "npm run watch")]);
    root.querySelector<HTMLButtonElement>(".chappa-workspace-cmd-add")!.click();
    await FLUSH();
    expect(prompted).toEqual([[null, "Add command"]]);
    expect(api.saveWorkspaceCommand).toHaveBeenCalledWith(
      null,
      expect.objectContaining({ name: "watch" }),
    );
    expect(root.querySelector(".chappa-workspace-cmd-list .chappa-process-name")?.textContent).toBe(
      "watch",
    );

    // Edit: pre-filled from the row, originalName = the current name.
    prompted.length = 0;
    vi.mocked(api.saveWorkspaceCommand).mockResolvedValue([
      wsCmdDto("watch", "npm run watch --dev"),
    ]);
    contextMenu(root.querySelector(".chappa-workspace-cmd-list .chappa-process-row")!);
    menuItem(root, "chappa-ws-cmd-edit").click();
    await FLUSH();
    expect(prompted).toEqual([[expect.objectContaining({ name: "watch" }), "Edit command"]]);
    expect(api.saveWorkspaceCommand).toHaveBeenCalledWith(
      "watch",
      expect.objectContaining({ name: "watch" }),
    );

    // Delete: confirm, then the stub answers an empty list (row gone).
    contextMenu(root.querySelector(".chappa-workspace-cmd-list .chappa-process-row")!);
    vi.mocked(api.deleteWorkspaceCommand).mockResolvedValue([]);
    menuItem(root, "chappa-ws-cmd-delete").click();
    await FLUSH();
    expect(api.deleteWorkspaceCommand).toHaveBeenCalledWith("watch");
    expect(root.querySelector(".chappa-workspace-cmd-list .chappa-process-row")).toBeNull();
    expect(
      root.querySelector(".chappa-workspace-cmds .chappa-subsection-count")?.textContent,
    ).toBe("0");
    panelDispose(app);
    root.remove();
  });

  it("opens ONLY the reduced context menu (Start/Stop/Edit/Copy/Delete)", async () => {
    const { root, app } = await workspaceApp([wsCmdDto("watch", "npm run watch")]);
    const row = root.querySelector<HTMLElement>(".chappa-workspace-cmds .chappa-process-row")!;
    row.dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 10, clientY: 10 }),
    );
    const menu = root.querySelector<HTMLElement>(".chappa-ws-cmd-menu")!;
    expect(menu.style.display).toBe("block");
    const labels = [...menu.querySelectorAll(".chappa-row-menu-item")].map((b) => b.textContent);
    expect(labels).toEqual(["Start", "Stop", "Edit command…", "Copy command", "Delete command"]);
    // No favorites / notification / YML / duplicate entries.
    expect(labels.some((l) => /favorite|notification|YML|duplicate|Disable/i.test(l ?? ""))).toBe(
      false,
    );
    panelDispose(app);
  });

  it("starts and stops a command through the wire, driven by workspace://status", async () => {
    const { api, root, app } = await workspaceApp([wsCmdDto("watch", "npm run watch")]);
    // Start: the Start button (nth 1) spawns through startWorkspaceCommand.
    root.querySelectorAll<HTMLButtonElement>(
      ".chappa-workspace-cmds .chappa-process-btn",
    )[0].click();
    await FLUSH();
    expect(api.startWorkspaceCommand).toHaveBeenCalledTimes(1);
    expect(api.startWorkspaceCommand).toHaveBeenCalledWith("watch", expect.anything());
    expect(
      root.querySelector(".chappa-workspace-cmds .chappa-status-pill")?.textContent,
    ).toBe("starting");

    // The command's terminal lands as a workspace rail entry (plain shell).
    const anyApp = app as unknown as { entries: Map<number, RailEntry> };
    expect(anyApp.entries.has(42)).toBe(true);

    // workspace://status drives the card in place.
    (app as unknown as { applyWorkspaceStatus: (p: ipc.WorkspaceStatusEvent) => void }).applyWorkspaceStatus(
      { name: "watch", status: "running", exit_code: null, term_id: 42 },
    );
    expect(
      root.querySelector(".chappa-workspace-cmds .chappa-status-pill")?.textContent,
    ).toBe("running");

    // Stop (nth 2) → dispose + stopWorkspaceCommand.
    root.querySelectorAll<HTMLButtonElement>(
      ".chappa-workspace-cmds .chappa-process-btn",
    )[1].click();
    await FLUSH();
    expect(api.stopWorkspaceCommand).toHaveBeenCalledWith("watch");
    expect(anyApp.entries.has(42)).toBe(false);
    expect(
      root.querySelector(".chappa-workspace-cmds .chappa-status-pill")?.textContent,
    ).toBe("stopped");
    panelDispose(app);
  });

  it("auto_start commands spawn at launch (the up-all seed)", async () => {
    const { api, app } = await workspaceApp([
      wsCmdDto("watch", "npm run watch", { autoStart: true }),
      wsCmdDto("manual", "python x.py", { autoStart: false }),
    ]);
    // Only the autoStart one started, with its own channel.
    expect(api.startWorkspaceCommand).toHaveBeenCalledTimes(1);
    expect(api.startWorkspaceCommand).toHaveBeenCalledWith("watch", expect.anything());
    panelDispose(app);
  });
});

// --- adopt backend-spawned terminals (rail rows + attach) -----------

describe("App adopted backend terminals", () => {
  type Created = ipc.TerminalCreatedEvent;
  type AppDriver = {
    applyCreated: (p: Created) => void;
    /** The `term://closed` drop path (the handler delegates straight here). */
    dropEntry: (id: number, closeViaPanel?: boolean) => void;
    entries: Map<number, RailEntry>;
    renderRail: () => void;
    reconcileAdoption: () => Promise<void>;
    expandedProjectId: number | null;
  };
  const drive = (app: App): AppDriver => app as unknown as AppDriver;
  const railNames = (root: HTMLElement): (string | null)[] =>
    [...root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item .chappa-rail-name")].map(
      (e) => e.textContent,
    );
  const railCount = (root: HTMLElement): number =>
    root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item").length;

  /** The one stored project the bound-agent tests open. */
  const PROJ: ipc.ProjectInfoDto = {
    id: 1,
    name: "chappa-ai",
    path: "/p/chappa-ai",
    icon: null,
    notificationLevel: null,
  };

  function mountedProject(): { api: TerminalApi; root: HTMLElement; app: App } {
    const { api } = stubApi();
    (api.listProjects as ReturnType<typeof vi.fn>).mockImplementation(async () => [PROJ]);
    (api.openProject as ReturnType<typeof vi.fn>).mockImplementation(async () => ({
      project: PROJ,
      trustPending: false,
      trustCommands: [],
      processes: [],
    }));
    const root = document.createElement("div");
    document.body.appendChild(root);
    const app = new App({ root, api, confirm: async () => true });
    return { api, root, app };
  }

  it("adopts a plain terminal into workspace TERMINALS on term://created, with the close ×", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    drive(app).applyCreated({ term_id: 50, name: "orphan", kind: "terminal", project_id: null, agent_tool_id: null, parent_process_id: null });
    expect(railNames(root)).toEqual(["orphan"]);
    const row = root.querySelector<HTMLElement>(".chappa-rail-list .chappa-rail-item")!;
    expect(row.querySelector(".chappa-rail-close")).not.toBeNull();
    // No panel yet — the row alone.
    expect(root.querySelectorAll<HTMLElement>(".chappa-panel-host").length).toBe(0);
    panelDispose(app);
  });

  it("adopts a bound agent into that project's AGENTS section and an unbound one into the workspace list", async () => {
    const { root, app } = mountedProject();
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    // Bound to the expanded project → AGENTS subsection, NOT the workspace rail.
    drive(app).applyCreated({ term_id: 70, name: "agent70", kind: "agent", project_id: 1, agent_tool_id: 3, parent_process_id: null });
    let agentRows = [...root.querySelectorAll<HTMLElement>(".chappa-project-agents .chappa-rail-item")];
    expect(agentRows.length).toBe(1);
    expect(agentRows[0].querySelector(".chappa-rail-name")?.textContent).toBe("agent70");
    expect(agentRows[0].classList.contains("agent")).toBe(true);
    expect(railNames(root)).not.toContain("agent70");
    expect(root.querySelector(".chappa-project-agents .chappa-subsection-count")?.textContent).toBe("1/1");

    // Unbound → workspace agents list (an agent row, not a plain shell).
    drive(app).applyCreated({ term_id: 71, name: "agent71", kind: "agent", project_id: null, agent_tool_id: 7, parent_process_id: null });
    const railEls = [...root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item")];
    const unbound = railEls.find((r) => r.querySelector(".chappa-rail-name")?.textContent === "agent71");
    expect(unbound).toBeDefined();
    expect(unbound!.classList.contains("agent")).toBe(true);
    panelDispose(app);
    root.remove();
  });

  it("reconcile at mount adopts a pre-existing live terminal exactly once", async () => {
    const { api } = stubApi();
    const live: ipc.TerminalInfoDto[] = [
      { id: 90, name: "orphan", status: "running", exit_code: null, cols: 80, rows: 24, seq: 1, kind: "terminal", project_id: null, agent_tool_id: null },
    ];
    (api.listTerminals as ReturnType<typeof vi.fn>).mockImplementation(async () => live);
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount(); // mount runs reconcileAdoption once
    await FLUSH();
    expect(railNames(root)).toEqual(["orphan"]);

    // A later broadcast (or a second reconcile) for the same id must NOT
    // duplicate the row — the id is remembered.
    drive(app).applyCreated({ term_id: 90, name: "orphan", kind: "terminal", project_id: null, agent_tool_id: null, parent_process_id: null });
    await drive(app).reconcileAdoption();
    expect(railCount(root)).toBe(1);
    panelDispose(app);
  });

  it("does not add a duplicate row for a self-spawn the broadcast echoes back", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    await app.newShell(); // Rust allocates id 1
    await FLUSH();
    expect(railCount(root)).toBe(1);
    // The backend broadcasts the create the app itself initiated: matching
    // by term_id must skip it.
    drive(app).applyCreated({ term_id: 1, name: "shell", kind: "terminal", project_id: null, agent_tool_id: null, parent_process_id: null });
    expect(railCount(root)).toBe(1);
    panelDispose(app);
  });

  it("clicking an adopted row attaches a live panel via attach_terminal (not create_terminal) whose frames flow from a FULL", async () => {
    const { api } = stubApi();
    let attachId: number | null = null;
    let frameCb: ((data: ArrayBuffer | number[]) => void) | null = null;
    (api.attachTerminal as ReturnType<typeof vi.fn>).mockImplementation(async (id: number, ch: { onmessage: ((d: ArrayBuffer | number[]) => void) | undefined }) => {
      attachId = id;
      frameCb = ch.onmessage ?? null;
      return { id, name: "orphan", status: "running", exit_code: null, cols: 80, rows: 24, seq: 1, kind: "terminal", project_id: null, agent_tool_id: null };
    });
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    drive(app).applyCreated({ term_id: 30, name: "orphan", kind: "terminal", project_id: null, agent_tool_id: null, parent_process_id: null });
    // Click the adopted row → the pane is built via attach_terminal.
    root.querySelector<HTMLElement>(".chappa-rail-list .chappa-rail-item")!.click();
    await FLUSH();
    expect((api.attachTerminal as ReturnType<typeof vi.fn>).mock.calls.length).toBe(1);
    expect(attachId).toBe(30);
    expect(api.createTerminal).not.toHaveBeenCalled();
    expect(root.querySelectorAll<HTMLElement>(".chappa-panel-host.active").length).toBe(1);
    // The frames stream arrives through the attached channel and starts from
    // a FULL resync (attach_terminal forces one Rust-side — the registry test
    // proves the replacement sink's first frame is a FULL). Driving that FULL
    // frame through the wired channel must be accepted cleanly: the panel is
    // live, so input/resize/scroll work as normal thereafter.
    expect(frameCb).not.toBeNull();
    expect(() => frameCb!(frame({ cols: 80, rows: 5, seq: 2, kind: 0, content: { 0: "HELLO" } }))).not.toThrow();
    await FLUSH();
    panelDispose(app);
  });

  it("closing an adopted row calls close_terminal and removes the row", async () => {
    const { api } = stubApi();
    const close = api.closeTerminal as ReturnType<typeof vi.fn>;
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    drive(app).applyCreated({ term_id: 20, name: "orphan", kind: "terminal", project_id: null, agent_tool_id: null, parent_process_id: null });
    await FLUSH();
    const row = root.querySelector<HTMLElement>(".chappa-rail-list .chappa-rail-item")!;
    row.querySelector<HTMLElement>(".chappa-rail-close")!.click();
    await FLUSH();
    expect(close).toHaveBeenCalledWith(20);
    expect(railCount(root)).toBe(0);
    panelDispose(app);
  });
});

// --- subagent nesting in the rail + term://closed -------------------

describe("agentForest (pure)", () => {
  const a = (id: number, parentId: number | null) => ({ id, parentId, entry: { id } });
  it("roots keep insertion order; children nest after their parent in id order", () => {
    const rows = [a(10, null), a(11, 10), a(12, 10), a(13, null), a(14, 13)];
    const out = agentForest(rows).map(([r, d]) => `${r.id}@${d}`);
    expect(out).toEqual(["10@0", "11@1", "12@1", "13@0", "14@1"]);
  });
  it("promotes an orphan (parent not on the surface) silently to a root", () => {
    // 2's parent 1 is absent; 3's parent 99 is absent; 4 is 2's child.
    const rows = [a(2, 1), a(3, 99), a(4, 2)];
    expect(agentForest(rows).map(([r, d]) => `${r.id}@${d}`)).toEqual(["2@0", "4@1", "3@0"]);
  });
  it("a parent that EXITED (still present) keeps nesting; a CLOSED parent promotes", () => {
    expect(agentForest([a(1, null), a(2, 1)]).map(([, d]) => d)).toEqual([0, 1]);
    expect(agentForest([a(2, 1)]).map(([, d]) => d)).toEqual([0]);
  });
  it("depth is unbounded in the model — the function reports true depth", () => {
    const chain = [a(1, null), a(2, 1), a(3, 2), a(4, 3), a(5, 4), a(6, 5)];
    expect(agentForest(chain).map(([, d]) => d)).toEqual([0, 1, 2, 3, 4, 5]);
  });
  it("nothing may vanish even in a broken cycle (root fallback, invariant)", () => {
    const out = agentForest([a(1, 2), a(2, 1)]);
    expect(out.length).toBe(2);
  });
});

describe("App subagent nesting", () => {
  type Created = ipc.TerminalCreatedEvent;
  type Driver = {
    applyCreated: (p: Created) => void;
    dropEntry: (id: number, closeViaPanel?: boolean) => void;
    entries: Map<number, RailEntry>;
    renderRail: () => void;
  };
  const drive = (app: App): Driver => app as unknown as Driver;
  const railNames = (root: HTMLElement): (string | null)[] =>
    [...root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item .chappa-rail-name")].map(
      (e) => e.textContent,
    );
  const railCount = (root: HTMLElement): number =>
    root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item").length;
  const agent = (id: number, name: string, parent: number | null, project: number | null): Created => ({
    term_id: id,
    name,
    kind: "agent",
    project_id: project,
    agent_tool_id: 3,
    parent_process_id: parent,
  });
  const render = (root: HTMLElement): HTMLElement[] => [
    ...root.querySelectorAll<HTMLElement>(".chappa-rail-list .chappa-rail-item"),
  ];

  it("workspace rail nests a spawn child under its parent with the child connector", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    drive(app).applyCreated(agent(40, "parent", null, null));
    drive(app).applyCreated(agent(41, "child", 40, null));
    drive(app).applyCreated(agent(42, "root2", null, null));
    expect(railNames(root)).toEqual(["parent", "child", "root2"]);
    const rows = render(root);
    const parentRow = rows.find((r) => r.querySelector(".chappa-rail-name")?.textContent === "parent")!;
    const childRow = rows.find((r) => r.querySelector(".chappa-rail-name")?.textContent === "child")!;
    // Child renders immediately after its parent.
    expect(rows.indexOf(parentRow)).toBe(rows.indexOf(childRow) - 1);
    expect(parentRow.classList.contains("chappa-rail-child")).toBe(false);
    expect(childRow.classList.contains("chappa-rail-child")).toBe(true);
    expect(childRow.classList.contains("chappa-rail-child-last")).toBe(true); // only child
    expect(childRow.style.getPropertyValue("--chappa-sub-depth")).toBe("1");
    panelDispose(app);
    root.remove();
  });

  it("closing a parent promotes its children to roots; the flat count stays honest", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    drive(app).applyCreated(agent(50, "p", null, null));
    drive(app).applyCreated(agent(51, "c", 50, null));
    drive(app).applyCreated(agent(52, "root2", null, null));
    expect(railCount(root)).toBe(3);
    // term://closed for the parent (backend-initiated close) drops ONLY the
    // parent row; the child stays and is promoted to a root.
    drive(app).dropEntry(50, false);
    expect(railNames(root)).toEqual(["c", "root2"]);
    expect(railCount(root)).toBe(2); // nothing vanished, nothing double-counted
    const childRow = render(root).find((r) => r.querySelector(".chappa-rail-name")?.textContent === "c")!;
    expect(childRow.classList.contains("chappa-rail-child")).toBe(false);
    // Idempotent: a second term://closed for the same id is a no-op.
    drive(app).dropEntry(50, false);
    expect(railCount(root)).toBe(2);
    panelDispose(app);
    root.remove();
  });

  /** Mount with the one stored project OPEN (expanded), fully awaited so the
   *  project AGENTS section is live. */
  async function mountedOpenProject(): Promise<{ root: HTMLElement; app: App }> {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const { api } = stubApi();
    (api.listProjects as ReturnType<typeof vi.fn>).mockImplementation(async () => [
      { id: 1, name: "p1", path: "/p/p1", icon: null, notificationLevel: null },
    ]);
    (api.openProject as ReturnType<typeof vi.fn>).mockImplementation(async () => ({
      project: { id: 1, name: "p1", path: "/p/p1", icon: null, notificationLevel: null },
      trustPending: false,
      trustCommands: [],
      processes: [],
    }));
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    await app.openProject(1);
    await FLUSH();
    return { root, app };
  }

  /** Drive the same reducer the `term://status` event does onto the entries. */
  function setStatus(id: number, status: RailStatus, entries: Map<number, RailEntry>): void {
    entries.set(id, railReducer(entries.get(id)!, { type: "status", status, exitCode: null }));
  }

  it("cross-surface child renders as a root in its own section (its own binding decides)", async () => {
    const { root, app } = await mountedOpenProject();
    // Parent unbound (workspace); child bound to the expanded project 1.
    drive(app).applyCreated(agent(60, "wsp-parent", null, null));
    drive(app).applyCreated(agent(61, "proj-child", 60, 1));
    // The child's OWN binding puts it in the project AGENTS section as a root
    // (its parent is not on that surface); the parent stays in the workspace.
    const projRows = [...root.querySelectorAll<HTMLElement>(".chappa-project-agents .chappa-rail-item")];
    expect(projRows.map((r) => r.querySelector(".chappa-rail-name")?.textContent)).toEqual(["proj-child"]);
    expect(projRows[0].classList.contains("chappa-rail-child")).toBe(false);
    expect(railNames(root)).toEqual(["wsp-parent"]);
    expect(railCount(root)).toBe(1);
    panelDispose(app);
    root.remove();
  });

  it("project AGENTS renders a forest with a FLAT N/M count (3/4)", async () => {
    const { root, app } = await mountedOpenProject();
    // parent(running) + child(running) + another root(running) + one exited
    // root → 3/4.
    drive(app).applyCreated(agent(80, "parent", null, 1));
    drive(app).applyCreated(agent(81, "child", 80, 1));
    drive(app).applyCreated(agent(82, "root-b", null, 1));
    drive(app).applyCreated(agent(83, "exited-root", null, 1));
    const entries = drive(app).entries;
    setStatus(80, "running", entries);
    setStatus(81, "running", entries);
    setStatus(82, "running", entries);
    setStatus(83, "exited", entries);
    drive(app).renderRail();
    const rows = [...root.querySelectorAll<HTMLElement>(".chappa-project-agents .chappa-rail-item")];
    const names = rows.map((r) => r.querySelector(".chappa-rail-name")?.textContent);
    expect(names).toEqual(["parent", "child", "root-b", "exited-root"]);
    expect(rows[1].classList.contains("chappa-rail-child")).toBe(true);
    expect(rows[1].classList.contains("chappa-rail-child-last")).toBe(true);
    expect(rows[3].classList.contains("chappa-rail-child")).toBe(false);
    expect(
      root.querySelector(".chappa-project-agents .chappa-subsection-count")?.textContent,
    ).toBe("3/4");
    // FLAT: a child indents but still counts — the project section carries all
    // four bound agents (never fewer, never a double-counted child).
    const projCount = root.querySelectorAll<HTMLElement>(".chappa-project-agents .chappa-rail-item").length;
    expect(projCount).toBe(4);
    // Nothing leaks onto the workspace rail (all four are project-bound) and
    // the workspace count agrees with the workspace rows (flat).
    expect(railCount(root)).toBe(0);
    expect(railCount(root)).toBe(root.querySelectorAll(".chappa-rail-list .chappa-rail-item").length);
    panelDispose(app);
    root.remove();
  });

  it("adopt/reconcile carry parentId onto the AgentEntry (every ingest path)", async () => {
    const { api } = stubApi();
    const root = document.createElement("div");
    const app = new App({ root, api, confirm: async () => true });
    await app.mount();
    // term://created path
    drive(app).applyCreated(agent(70, "c", 69, null));
    expect(drive(app).entries.get(70)?.agent?.parentId).toBe(69);
    // All agents default parentId to null when absent.
    drive(app).applyCreated(agent(71, "root", null, null));
    expect(drive(app).entries.get(71)?.agent?.parentId).toBeNull();
    panelDispose(app);
    root.remove();
  });
});
