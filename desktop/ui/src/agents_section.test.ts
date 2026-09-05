// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { SettingsPane } from "./settings_pane";
import { AgentsSection } from "./agents_section";
import {
  AgentToolsStore,
  CLAUDE_ALT_SCREEN_ENV,
  runtimeSummary,
  templateFor,
  type AgentTool,
  type AgentToolsApi,
} from "./agent_tools";
import { stubSettings } from "./term/test-utils";

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

function docker(id: number, over: Partial<AgentTool> = {}): AgentTool {
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

function fakeApi(initial: AgentTool[]) {
  let tools = initial.map((t) => ({ ...t }));
  let nextId = 100;
  const api: AgentToolsApi & { tools: () => AgentTool[] } = {
    tools: () => tools,
    listAgentTools: vi.fn(async () => ({ tools: tools.map((t) => ({ ...t })), machine_mode_types: ["claude", "opencode"] as const })),
    upsertAgentTool: vi.fn(async (t: AgentTool) => {
      if (t.program.trim() === "") throw new Error("program is required");
      const stored = { ...t, id: t.id || nextId++ };
      const i = tools.findIndex((x) => x.id === stored.id);
      if (i >= 0) tools[i] = stored;
      else tools.push(stored);
      return stored;
    }),
    deleteAgentTool: vi.fn(async (id: number) => {
      tools = tools.filter((t) => t.id !== id);
    }),
    parseAgentCommand: vi.fn(async (line: string) => ({
      tool: docker(0, { name: "opencode · model-fast" }),
      docker_detected: line.startsWith("docker exec"),
      warnings: line.startsWith("docker exec") ? [] : ["could not detect container"],
    })),
  };
  return api;
}

async function section(initial: AgentTool[] = []) {
  const api = fakeApi(initial);
  const store = new AgentToolsStore(api);
  const settings = stubSettings();
  await settings.store.load();
  const host = document.createElement("div");
  document.body.appendChild(host);
  const el = new AgentsSection({ store, settings: settings.store, modalContainer: host });
  host.appendChild(el.element);
  el.activate();
  await FLUSH();
  return { api, store, settings, host, section: el };
}

const field = <T extends HTMLElement>(root: ParentNode, name: string): T =>
  root.querySelector<T>(`[data-field="${name}"]`)!;
const action = (root: ParentNode, name: string): HTMLButtonElement =>
  root.querySelector<HTMLButtonElement>(`[data-action="${name}"]`)!;

describe("Agents settings section", () => {
  it("renders every tool with name, type, model, runtime summary and enabled toggle; the toggle persists", async () => {
    const { api, host, section: s } = await section([docker(3), docker(4, { name: "claude", tool_type: "claude", program: "claude", model: null, runtime: { kind: "host" }, enabled: false })]);
    const rows = host.querySelectorAll<HTMLElement>(".chappa-agent-row");
    expect(rows.length).toBe(2);
    expect(rows[0].querySelector(".chappa-agent-name")?.textContent).toBe("worker");
    expect(rows[0].querySelector(".chappa-agent-type")?.textContent).toBe("opencode");
    expect(rows[0].querySelector(".chappa-agent-model")?.textContent).toBe("model-fast");
    expect(rows[0].querySelector(".chappa-agent-runtime")?.textContent).toBe("docker: dev-worker (-u dev, -w /workspace/app)");
    expect(rows[1].querySelector(".chappa-agent-runtime")?.textContent).toBe("host");
    expect(field<HTMLInputElement>(rows[1], "enabled").checked).toBe(false);
    // Enabling persists immediately (no Save for the list).
    const toggle = field<HTMLInputElement>(rows[1], "enabled");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.upsertAgentTool).toHaveBeenCalledTimes(1);
    expect(api.tools()[1].enabled).toBe(true);
    expect(runtimeSummary({ kind: "docker_exec", container: "c", tty: false })).toBe("docker: c (no tty)");
    s.dispose();
  });

  it("the claude template shows the alt-screen env row, and it survives an edit", async () => {
    const { api, host, section: s } = await section([]);
    expect(templateFor("claude").env[CLAUDE_ALT_SCREEN_ENV]).toBe("1");
    field<HTMLSelectElement>(host, "newAgentType").value = "claude";
    action(host, "addAgent").click();
    const modal = s.modalElement!;
    expect(modal).not.toBeNull();
    expect(field<HTMLInputElement>(modal, "program").value).toBe("claude");
    const envRows = modal.querySelectorAll<HTMLElement>(".chappa-agent-env-row");
    expect(envRows.length).toBe(1);
    expect(field<HTMLInputElement>(envRows[0], "envKey").value).toBe(CLAUDE_ALT_SCREEN_ENV);
    expect(field<HTMLInputElement>(envRows[0], "envValue").value).toBe("1");
    // Edit something else, save: the env row is still there in what Rust gets.
    field<HTMLInputElement>(modal, "name").value = "claude · opus";
    field<HTMLInputElement>(modal, "model").value = "opus";
    action(modal, "saveAgent").click();
    await FLUSH();
    expect(api.upsertAgentTool).toHaveBeenCalledTimes(1);
    const saved = api.tools()[0];
    expect(saved.name).toBe("claude · opus");
    expect(saved.model).toBe("opus");
    expect(saved.env).toEqual({ [CLAUDE_ALT_SCREEN_ENV]: "1" });
    expect(saved.runtime).toEqual({ kind: "host" });
    expect(s.modalElement).toBeNull();

    // …and the row can be DELETED (the default is user-controlled): reopen,
    // remove it, save → no env at all.
    action(host, "editAgent").click();
    action(s.modalElement!, "removeEnv").click();
    action(s.modalElement!, "saveAgent").click();
    await FLUSH();
    expect(api.tools()[0].env).toEqual({});
    s.dispose();
  });

  it("validates: program required, container required for Docker, args one per line verbatim", async () => {
    const { api, host, section: s } = await section([]);
    field<HTMLSelectElement>(host, "newAgentType").value = "custom";
    action(host, "addAgent").click();
    const modal = s.modalElement!;
    action(modal, "saveAgent").click();
    await FLUSH();
    expect(modal.querySelector<HTMLElement>(".chappa-agent-modal-error")?.textContent).toMatch(/Program is required/);
    expect(api.upsertAgentTool).not.toHaveBeenCalled();
    field<HTMLInputElement>(modal, "program").value = "mytool";
    // Docker without a container is refused too.
    modal.querySelectorAll<HTMLButtonElement>('[data-field="runtime"] button')[1].click();
    action(modal, "saveAgent").click();
    await FLUSH();
    expect(modal.querySelector<HTMLElement>(".chappa-agent-modal-error")?.textContent).toMatch(/Container is required/);
    expect(api.upsertAgentTool).not.toHaveBeenCalled();
    field<HTMLInputElement>(modal, "container").value = "box";
    field<HTMLInputElement>(modal, "user").value = "me";
    field<HTMLInputElement>(modal, "max_busy_in_container").value = "2";
    field<HTMLTextAreaElement>(modal, "args").value = "--prompt\ntwo words here\n";
    field<HTMLInputElement>(modal, "max_busy").value = "1";
    action(modal, "saveAgent").click();
    await FLUSH();
    expect(api.upsertAgentTool).toHaveBeenCalledTimes(1);
    const saved = api.tools()[0];
    expect(saved.program).toBe("mytool");
    expect(saved.name).toBe("mytool");
    expect(saved.args).toEqual(["--prompt", "two words here"]);
    expect(saved.runtime).toEqual({ kind: "docker_exec", container: "box", user: "me", workdir: null, tty: true, max_busy_in_container: 2 });
    expect(saved.max_busy).toBe(1);
    expect(saved.tool_type).toBe("custom");
    s.dispose();
  });

  it("the paste box prefills the modal from the parser, warnings included", async () => {
    const { api, host, section: s } = await section([]);
    const input = field<HTMLInputElement>(host, "agentCommand");
    input.value = "docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast";
    action(host, "addFromCommand").click();
    await FLUSH();
    expect(api.parseAgentCommand).toHaveBeenCalledWith(input.value || expect.any(String));
    const modal = s.modalElement!;
    expect(field<HTMLInputElement>(modal, "program").value).toBe("opencode");
    expect(field<HTMLInputElement>(modal, "name").value).toBe("opencode · model-fast");
    expect(field<HTMLInputElement>(modal, "model").value).toBe("model-fast");
    expect(field<HTMLTextAreaElement>(modal, "args").value).toBe("-m\ngateway/model-fast");
    expect(field<HTMLInputElement>(modal, "container").value).toBe("dev-worker");
    expect(field<HTMLInputElement>(modal, "user").value).toBe("dev");
    expect(field<HTMLInputElement>(modal, "workdir").value).toBe("/workspace/app");
    // The docker rows are revealed (Docker is the active runtime).
    expect(field<HTMLInputElement>(modal, "container").closest<HTMLElement>("div")!.style.display).not.toBe("none");
    expect(modal.querySelector(".chappa-agent-modal-warning")).toBeNull();
    action(modal, "cancelAgent").click();
    expect(s.modalElement).toBeNull();

    // An unclassifiable string still opens the modal, flagged.
    input.value = "something odd";
    action(host, "addFromCommand").click();
    await FLUSH();
    expect(s.modalElement!.querySelector(".chappa-agent-modal-warning")?.textContent).toMatch(/could not detect container/);
    s.dispose();
  });

  it("delete shows Rust's refusal naming the live processes and keeps the row", async () => {
    const { api, host, section: s } = await section([docker(3)]);
    (api.deleteAgentTool as ReturnType<typeof vi.fn>).mockImplementationOnce(async () => {
      throw new Error("agent tool 3 is in use by a live agent: worker · build (process 7) — close it first");
    });
    action(host, "deleteAgent").click();
    await FLUSH();
    const error = host.querySelector<HTMLElement>(".chappa-agent-error")!;
    expect(error.style.display).not.toBe("none");
    expect(error.textContent).toMatch(/worker · build \(process 7\)/);
    expect(host.querySelectorAll(".chappa-agent-row").length).toBe(1);
    // Once nothing references it, the delete goes through and the error clears.
    action(host, "deleteAgent").click();
    await FLUSH();
    expect(host.querySelectorAll(".chappa-agent-row").length).toBe(0);
    expect(error.style.display).toBe("none");
    s.dispose();
  });

  it("review: an edit of args survives a later Enabled toggle (handlers read the store, rows re-sign on every field)", async () => {
    const { api, host, section: s } = await section([docker(3)]);
    // Edit ONLY the args (name/type/model/runtime/enabled — the old
    // signature fields — all unchanged).
    action(host, "editAgent").click();
    const modal = s.modalElement!;
    field<HTMLTextAreaElement>(modal, "args").value = "-m\ngateway/model-large\n--verbose";
    action(modal, "saveAgent").click();
    await FLUSH();
    expect(api.tools()[0].args).toEqual(["-m", "gateway/model-large", "--verbose"]);
    // Now toggle Enabled from the row: the upsert must carry the NEW args,
    // not the closure's stale copy (which used to silently revert the edit).
    const toggle = field<HTMLInputElement>(host.querySelector(".chappa-agent-row")!, "enabled");
    toggle.checked = false;
    toggle.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.upsertAgentTool).toHaveBeenCalledTimes(2);
    const sent = (api.upsertAgentTool as ReturnType<typeof vi.fn>).mock.calls[1][0] as AgentTool;
    expect(sent.enabled).toBe(false);
    expect(sent.args).toEqual(["-m", "gateway/model-large", "--verbose"]);
    expect(api.tools()[0]).toMatchObject({ enabled: false, args: ["-m", "gateway/model-large", "--verbose"] });
    // Edit again → the reopened modal shows the stored args (row re-signed).
    action(host, "editAgent").click();
    expect(field<HTMLTextAreaElement>(s.modalElement!, "args").value).toBe("-m\ngateway/model-large\n--verbose");
    action(s.modalElement!, "cancelAgent").click();
    s.dispose();
  });

  it("the agent settings commit through the settings store and snap back to the clamped value", async () => {
    const { host, settings, section: s } = await section([]);
    const quiet = host.querySelector<HTMLInputElement>('[data-setting="agentReadyQuietMs"]')!;
    expect(quiet.value).toBe("750");
    quiet.value = "10";
    quiet.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.agentReadyQuietMs).toBe(250);
    expect(quiet.value).toBe("250");
    const stale = host.querySelector<HTMLInputElement>('[data-setting="agentStaleAfterS"]')!;
    expect(stale.value).toBe("900");
    stale.value = "1200";
    stale.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.agentStaleAfterS).toBe(1200);
    const maxWait = host.querySelector<HTMLInputElement>('[data-setting="agentReadyMaxWaitMs"]')!;
    expect(maxWait.value).toBe("5000");
    maxWait.value = "100";
    maxWait.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.agentReadyMaxWaitMs).toBe(1000);
    expect(maxWait.value).toBe("1000");
    s.dispose();
  });

  // The orphan reaper is a BOOLEAN agent knob, default ON — it gates
  // only the automatic startup/probe sweeps, so it round-trips through the
  // settings store like every other agent setting.
  it("The orphan-reaper toggle round-trips through the settings store (default ON)", async () => {
    const { host, settings, section: s } = await section([]);
    const reap = host.querySelector<HTMLInputElement>('[data-setting="agentReapOrphans"]')!;
    expect(reap.type).toBe("checkbox");
    expect(reap.checked).toBe(true); // default ON
    reap.checked = false;
    reap.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.agentReapOrphans).toBe(false);
    // The store re-subscribes and re-renders the CHECKED state.
    expect(reap.checked).toBe(false);
    reap.checked = true;
    reap.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.agentReapOrphans).toBe(true);
    s.dispose();
  });

  // The five timer knobs are mirrored into the SAME pane and behave
  // like every other setting — commit on change, adopt the clamped value Rust
  // returns, never a Save button.
  it("the timer settings commit through the settings store and snap back to the clamped value", async () => {
    const { host, settings } = await section([]);
    const get = (key: string) => host.querySelector<HTMLInputElement>(`[data-setting="${key}"]`)!;
    for (const [key, initial] of [
      ["idleThresholdMs", "120000"],
      ["timerConfirmMs", "5000"],
      ["timerDeliveryTimeoutMs", "30000"],
      ["timerDedupeMs", "5000"],
      ["timerRetentionHours", "24"],
    ] as const) {
      expect(get(key).value, key).toBe(initial);
    }
    const idle = get("idleThresholdMs");
    idle.value = "1";
    idle.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.idleThresholdMs).toBe(1000);
    expect(idle.value).toBe("1000");
    // 0 is IN range for the two "disable me" knobs and survives.
    const dedupe = get("timerDedupeMs");
    dedupe.value = "0";
    dedupe.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.timerDedupeMs).toBe(0);
    const retention = get("timerRetentionHours");
    retention.value = "99999";
    retention.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(settings.saved()!.timerRetentionHours).toBe(720);
    expect(retention.value).toBe("720");
  });

  it("the Transport picker offers JSON for claude/opencode only, snaps back to tty for other types, and the row shows a json tag", async () => {
    const { api, host, section: s } = await section([docker(3, { transport: "json" }), docker(4, { name: "codex", tool_type: "codex", program: "codex" })]);
    const rows = host.querySelectorAll<HTMLElement>(".chappa-agent-row");
    expect(rows[0].querySelector<HTMLElement>(".chappa-agent-transport")!.textContent).toBe("json");
    expect(rows[1].querySelector<HTMLElement>(".chappa-agent-transport")!.style.display).toBe("none");
    // New claude tool: picker visible, defaults to TTY, JSON selectable.
    field<HTMLSelectElement>(host, "newAgentType").value = "claude";
    action(host, "addAgent").click();
    const modal = s.modalElement!;
    const picker = field<HTMLElement>(modal, "transport");
    expect(picker.style.display).toBe("");
    const options = picker.querySelectorAll<HTMLButtonElement>(".chappa-segmented-option");
    expect([...options].map((o) => o.textContent)).toEqual(["TTY", "JSON"]);
    options[1].click();
    // Switching the type to codex hides the picker and the save is tty.
    const type = field<HTMLSelectElement>(modal, "tool_type");
    type.value = "codex";
    type.dispatchEvent(new Event("change"));
    expect(picker.style.display).toBe("none");
    expect(modal.querySelector(".chappa-agent-transport-hint")?.textContent).toMatch(/tty only/);
    type.value = "claude";
    type.dispatchEvent(new Event("change"));
    expect(picker.style.display).toBe("");
    options[1].click();
    action(modal, "saveAgent").click();
    await FLUSH();
    const saved = api.tools().find((t) => t.tool_type === "claude")!;
    expect(saved.transport).toBe("json");
    // …and an opencode tool edited to codex loses json on save.
    action(host, "editAgent").click();
    const edit = s.modalElement!;
    const editType = field<HTMLSelectElement>(edit, "tool_type");
    editType.value = "codex";
    editType.dispatchEvent(new Event("change"));
    action(edit, "saveAgent").click();
    await FLUSH();
    expect(api.tools().find((t) => t.id === 3)!.transport).toBe("tty");
    s.dispose();
  });

  it("the settings pane mounts the Agents section and re-loads the registry on open", async () => {
    const api = fakeApi([docker(3)]);
    const store = new AgentToolsStore(api);
    const settings = stubSettings();
    await settings.store.load();
    const pane = new SettingsPane({ store: settings.store, agentTools: store });
    pane.open();
    await FLUSH();
    expect(api.listAgentTools).toHaveBeenCalled();
    expect(pane.element.textContent).toMatch(/AGENTS/);
    expect(pane.element.querySelectorAll(".chappa-agent-row").length).toBe(1);
    // Escape inside the agent modal closes the MODAL, not the pane.
    pane.element.querySelector<HTMLButtonElement>('[data-action="editAgent"]')!.click();
    expect(pane.agentsSection.modalElement).not.toBeNull();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(pane.agentsSection.modalElement).toBeNull();
    expect(pane.isOpen()).toBe(true);
    pane.dispose();
  });
});
