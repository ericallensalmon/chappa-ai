// The Agents section of the settings pane: the tool list
// (name, type, model, runtime summary, enabled toggle), an edit modal with
// every registry field (env as key/value rows, runtime as a Host/Docker
// segmented picker revealing container/user/workdir/tty, the two busy
// limits), the "Add from command…" paste box (Rust's command-string parser →
// prefilled modal), and "Delete" with the live-reference refusal shown
// inline. The agent settings (`agentReadyQuietMs`, `agentReadyMaxWaitMs`,
// `agentStaleAfterS`) live here too — they are agent knobs, not terminal ones,
// and so do the timer knobs (`idleThresholdMs`, `timerConfirmMs`,
// `timerDeliveryTimeoutMs`, `timerDedupeMs`, `timerRetentionHours`): a timer
// wakes an AGENT, and its idle rule is the agent-liveness rule.
//
// Same house rules as the rest of the pane: no Save button for the list
// (toggles persist immediately); the MODAL has Save/Cancel because a
// half-typed tool must not be written on every keystroke. Every commit
// round-trips through the store, which adopts what Rust returns.

import { Segmented } from "./segmented";
import {
  AGENT_TOOL_TYPES,
  runtimeSummary,
  templateFor,
  type AgentTool,
  type AgentToolType,
  type AgentToolsStore,
} from "./agent_tools";
import {
  AGENT_READY_MAX_WAIT_MAX,
  AGENT_READY_MAX_WAIT_MIN,
  AGENT_READY_QUIET_MAX,
  AGENT_READY_QUIET_MIN,
  AGENT_STALE_AFTER_MAX,
  AGENT_STALE_AFTER_MIN,
  IDLE_THRESHOLD_MAX,
  IDLE_THRESHOLD_MIN,
  TIMER_CONFIRM_MAX,
  TIMER_CONFIRM_MIN,
  TIMER_DEDUPE_MAX,
  TIMER_DEDUPE_MIN,
  TIMER_DELIVERY_TIMEOUT_MAX,
  TIMER_DELIVERY_TIMEOUT_MIN,
  TIMER_RETENTION_HOURS_MAX,
  TIMER_RETENTION_HOURS_MIN,
  type Settings,
  type SettingsStore,
} from "./settings";

const ROW_CSS = "display:flex;align-items:center;gap:8px;padding:5px 0;";
const TAG_CSS =
  "flex:none;font:11px ui-monospace,monospace;color:#8b949e;padding:1px 6px;border:1px solid #262a31;border-radius:10px;";
const MUTED_CSS = "flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#8b949e;font:12px ui-monospace,monospace;";
const NAME_CSS = "flex:0 1 200px;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";
const SMALL_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:11px system-ui,sans-serif;cursor:pointer;padding:3px 8px;";
const INPUT_CSS =
  "background:#0d0e10;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:3px 6px;" +
  "font:12px ui-monospace,monospace;outline:none;min-width:0;";
const SELECT_CSS =
  "background:#16181d;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:3px 6px;" +
  "font:12px system-ui,sans-serif;outline:none;";
const HINT_CSS = "font:11px system-ui,sans-serif;color:#8b949e;padding:0 0 6px;line-height:1.45;";
const SUBHEADING_CSS =
  "font:11px system-ui,sans-serif;color:#8b949e;letter-spacing:.08em;text-transform:uppercase;" +
  "padding:14px 0 2px;border-top:1px solid #262a31;margin-top:8px;";
const ERROR_CSS = "font:12px system-ui,sans-serif;color:#f0b429;padding:4px 0;white-space:pre-wrap;";
/** The five timer knobs, as one table — label, settings key, range,
 *  and the hint under it. Ranges mirror `project_model::settings`; Rust clamps,
 *  so an out-of-range entry snaps back visibly. */
type TimerSetting =
  | "idleThresholdMs"
  | "timerConfirmMs"
  | "timerDeliveryTimeoutMs"
  | "timerDedupeMs"
  | "timerRetentionHours";

const TIMER_SETTINGS: readonly [string, TimerSetting, number, number, string][] = [
  [
    "Idle threshold (ms)",
    "idleThresholdMs",
    IDLE_THRESHOLD_MIN,
    IDLE_THRESHOLD_MAX,
    "How long a process's pty byte stream must be silent before an idle timer calls it idle. Derived from bytes, never from render diffs; a timer can override it with idle_ms.",
  ],
  [
    "Fire-time confirm (ms)",
    "timerConfirmMs",
    TIMER_CONFIRM_MIN,
    TIMER_CONFIRM_MAX,
    "A met idle condition is re-checked after this long instead of firing at once. A byte in the window re-arms the timer — this is what stops a flicker from waking an agent. 0 fires on the first observation.",
  ],
  [
    "Delivery timeout (ms)",
    "timerDeliveryTimeoutMs",
    TIMER_DELIVERY_TIMEOUT_MIN,
    TIMER_DELIVERY_TIMEOUT_MAX,
    "How long a firing waits for its target's ready gate before recording delivered: false. The body is never silently dropped.",
  ],
  [
    "Duplicate window (ms)",
    "timerDedupeMs",
    TIMER_DEDUPE_MIN,
    TIMER_DEDUPE_MAX,
    "An identical body to the same process inside this window is coalesced instead of typed again (timer_list shows coalesced_with). 0 disables it.",
  ],
  [
    "Fired-timer retention (h)",
    "timerRetentionHours",
    TIMER_RETENTION_HOURS_MIN,
    TIMER_RETENTION_HOURS_MAX,
    "How long fired, cancelled and expired timers stay visible to timer_list(include_fired) with their delivery receipts.",
  ],
];

const SETTING_ROW_CSS =
  "display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:8px 14px;padding:9px 0;min-height:30px;";
const SETTING_LABEL_CSS = "flex:0 0 180px;min-width:0;";
// The modal: a backdrop over the pane, a 560px column of label/control rows.
const BACKDROP_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:70;display:flex;align-items:center;" +
  "justify-content:center;background:rgba(0,0,0,.55);";
const BOX_CSS =
  "width:560px;max-width:calc(100vw - 32px);max-height:calc(100vh - 32px);overflow-y:auto;" +
  "background:#1b1d21;border:1px solid #2a2c31;border-radius:8px;padding:16px 18px;" +
  "color:#d6d8dc;font:13px system-ui,sans-serif;box-shadow:0 8px 32px rgba(0,0,0,.6);";
const FIELD_CSS = "display:flex;align-items:center;gap:10px;padding:5px 0;";
const FIELD_LABEL_CSS = "flex:0 0 120px;color:#adb3bd;";
const BUTTON_ROW_CSS = "display:flex;justify-content:flex-end;gap:8px;padding-top:12px;";
const OK_BTN_CSS =
  "border:1px solid #3b6fb0;border-radius:4px;background:#244a7a;color:#e6edf3;font:12px system-ui,sans-serif;cursor:pointer;padding:4px 12px;";

let ids = 0;

export interface AgentsSectionOptions {
  store: AgentToolsStore;
  settings: SettingsStore;
  /** Where the modal mounts. Defaults to document.body. */
  modalContainer?: HTMLElement;
}

/** What the modal edits: a tool draft plus what the paste box could not
 *  classify. */
interface Draft {
  tool: AgentTool;
  warnings: string[];
  /** The stored id being edited (0 = insert). */
  originalId: number;
}

export class AgentsSection {
  readonly element: HTMLDivElement;
  private readonly store: AgentToolsStore;
  private readonly settings: SettingsStore;
  private readonly modalContainer: HTMLElement;
  private readonly list: HTMLDivElement;
  private readonly error: HTMLDivElement;
  private readonly typeSelect: HTMLSelectElement;
  private readonly pasteInput: HTMLInputElement;
  private readonly quietInput: HTMLInputElement;
  private readonly maxWaitInput: HTMLInputElement;
  private readonly staleInput: HTMLInputElement;
  private readonly reapInput: HTMLInputElement;
  private readonly timerInputs: { input: HTMLInputElement; key: TimerSetting }[] = [];
  private listSig = "\u0000never";
  private unsubscribe: (() => void) | null = null;
  private unsubscribeSettings: (() => void) | null = null;
  private modal: HTMLDivElement | null = null;
  private modalPicker: Segmented<"host" | "docker_exec"> | null = null;

  constructor(opts: AgentsSectionOptions) {
    this.store = opts.store;
    this.settings = opts.settings;
    this.modalContainer = opts.modalContainer ?? document.body;

    this.element = document.createElement("div");
    this.element.className = "chappa-agents-section";
    const head = document.createElement("div");
    head.style.cssText =
      "font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;padding-bottom:8px;";
    head.textContent = "AGENTS";
    this.element.appendChild(head);
    this.element.appendChild(
      hint(
        "Registered agent CLIs for the rail's “New agent ▸” menu and the spawn_agent MCP tool. " +
          "Name is display only; program + args are the truth (never re-split). Docker runtimes run " +
          "the program under a pid-recording wrapper so closing kills the container-side process.",
      ),
    );

    this.list = document.createElement("div");
    this.list.className = "chappa-agent-list";
    this.element.appendChild(this.list);

    this.error = document.createElement("div");
    this.error.className = "chappa-agent-error";
    this.error.style.cssText = ERROR_CSS;
    this.error.style.display = "none";
    this.element.appendChild(this.error);

    // Add from template.
    const addRow = document.createElement("div");
    addRow.style.cssText = ROW_CSS;
    this.typeSelect = document.createElement("select");
    this.typeSelect.style.cssText = SELECT_CSS;
    this.typeSelect.dataset.field = "newAgentType";
    for (const t of AGENT_TOOL_TYPES) {
      const opt = document.createElement("option");
      opt.value = t;
      opt.textContent = t;
      this.typeSelect.appendChild(opt);
    }
    const addBtn = document.createElement("button");
    addBtn.type = "button";
    addBtn.textContent = "Add agent…";
    addBtn.style.cssText = SMALL_BTN_CSS;
    addBtn.dataset.action = "addAgent";
    addBtn.addEventListener("click", () => {
      this.openModal({
        tool: templateFor(this.typeSelect.value as AgentToolType),
        warnings: [],
        originalId: 0,
      });
    });
    addRow.append(this.typeSelect, addBtn);
    this.element.appendChild(addRow);

    // Add from a command-style string.
    const pasteRow = document.createElement("div");
    pasteRow.style.cssText = ROW_CSS;
    this.pasteInput = document.createElement("input");
    this.pasteInput.type = "text";
    this.pasteInput.placeholder = "docker exec -u dev -it -w /workspace/app dev-worker opencode -m …";
    this.pasteInput.spellcheck = false;
    this.pasteInput.style.cssText = `${INPUT_CSS}flex:1 1 auto;`;
    this.pasteInput.dataset.field = "agentCommand";
    const pasteBtn = document.createElement("button");
    pasteBtn.type = "button";
    pasteBtn.textContent = "Add from command…";
    pasteBtn.style.cssText = SMALL_BTN_CSS;
    pasteBtn.dataset.action = "addFromCommand";
    pasteBtn.addEventListener("click", () => void this.addFromCommand());
    this.pasteInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void this.addFromCommand();
      }
    });
    pasteRow.append(this.pasteInput, pasteBtn);
    this.element.appendChild(pasteRow);
    this.element.appendChild(
      hint("Paste a tool command; the fields are detected (docker exec -u/-w/-it/-e, container, program, -m model)."),
    );

    // The two agent settings.
    this.quietInput = this.numberSetting(
      "Prompt ready quiet (ms)",
      "agentReadyQuietMs",
      AGENT_READY_QUIET_MIN,
      AGENT_READY_QUIET_MAX,
    );
    this.element.appendChild(
      hint(
        "A queued prompt is written once the agent has produced output and then stayed silent this long — " +
          "the portable ready signal; no app-specific prompt guessing.",
      ),
    );
    this.maxWaitInput = this.numberSetting(
      "Prompt ready max wait (ms)",
      "agentReadyMaxWaitMs",
      AGENT_READY_MAX_WAIT_MIN,
      AGENT_READY_MAX_WAIT_MAX,
    );
    this.element.appendChild(
      hint(
        "A TUI that repaints continuously never goes quiet: this long after its first output the prompt is " +
          "written anyway (receipt reason ready-by-timeout).",
      ),
    );
    this.staleInput = this.numberSetting(
      "Bridge stale after (s)",
      "agentStaleAfterS",
      AGENT_STALE_AFTER_MIN,
      AGENT_STALE_AFTER_MAX,
    );
    this.element.appendChild(
      hint("A docker agent silent this long whose winsize poke produces nothing is flagged stale."),
    );
    // The orphan reaper (startup + probe sweeps). A boolean, not a
    // number — commit on toggle, adopt what Rust returns.
    this.reapInput = this.boolSetting(
      "Reap orphaned docker agents",
      "agentReapOrphans",
      "Sweep containers for CHAPPA_AI_SPAWN_UUID marker processes no live row owns " +
        "(orphans of a hard restart or a mid-close docker hiccup) and kill them — at app start and on the bridge probe.",
    );

    // Timer knobs. Same house rules: commit on change, adopt what
    // Rust returns, no Save button.
    this.element.appendChild(subheading("Timers"));
    for (const [label, key, min, max, why] of TIMER_SETTINGS) {
      const input = this.numberSetting(label, key, min, max);
      this.timerInputs.push({ input, key });
      this.element.appendChild(hint(why));
    }

    this.sync(this.store.get());
    this.syncSettings(this.settings.get());
  }

  /** Called by the pane on open: re-load the registry and subscribe. */
  activate(): void {
    void this.store.load();
    this.unsubscribe?.();
    this.unsubscribe = this.store.subscribe((tools) => this.sync(tools));
    this.unsubscribeSettings?.();
    this.unsubscribeSettings = this.settings.subscribe((s) => this.syncSettings(s));
  }

  deactivate(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.unsubscribeSettings?.();
    this.unsubscribeSettings = null;
    this.closeModal();
  }

  dispose(): void {
    this.deactivate();
    this.element.remove();
  }

  /** The open edit modal, for tests. */
  get modalElement(): HTMLDivElement | null {
    return this.modal;
  }

  // --- list ------------------------------------------------------------------

  sync(tools: AgentTool[]): void {
    // EVERY field signs, not just the rendered ones: the row is only rebuilt
    // on a signature change, and an edit of program/args/env/max_busy that
    // left the old (rendered-fields-only) signature alone kept a row whose
    // handlers still held the OLD tool. JSON per tool, "\u001e" (record
    // separator, the escape sequence — never a literal control byte in
    // source) between tools: no field can run into its neighbour.
    const sig = tools.map((t) => JSON.stringify(t)).join("\u001e");
    if (sig === this.listSig) return;
    this.listSig = sig;
    this.list.textContent = "";
    if (tools.length === 0) {
      const empty = document.createElement("div");
      empty.className = "chappa-agent-empty";
      empty.style.cssText = HINT_CSS;
      empty.textContent = "No agent tools yet.";
      this.list.appendChild(empty);
      return;
    }
    for (const tool of tools) this.list.appendChild(this.buildRow(tool));
  }

  private buildRow(tool: AgentTool): HTMLDivElement {
    const el = document.createElement("div");
    el.className = "chappa-agent-row";
    el.style.cssText = ROW_CSS;
    el.dataset.agentTool = String(tool.id);

    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.checked = tool.enabled;
    toggle.title = "Offer this tool in the New agent menu";
    toggle.style.cssText = "flex:none;accent-color:#5ea6ff;";
    toggle.dataset.field = "enabled";
    // Handlers resolve the tool BY ID at click time, never from the closure:
    // the store is the truth and a row can outlive an edit (see `sync`).
    toggle.addEventListener("change", () => {
      const current = this.store.byId(tool.id);
      if (!current) return;
      current.enabled = toggle.checked;
      void this.commit(current);
    });

    const name = document.createElement("span");
    name.className = "chappa-agent-name";
    name.style.cssText = NAME_CSS;
    name.textContent = tool.name || tool.program;
    name.title = name.textContent;

    const type = document.createElement("span");
    type.className = "chappa-agent-type";
    type.style.cssText = TAG_CSS;
    type.textContent = tool.tool_type;

    const model = document.createElement("span");
    model.className = "chappa-agent-model";
    model.style.cssText = TAG_CSS;
    model.textContent = tool.model ?? "";
    model.style.display = tool.model ? "" : "none";

    const runtime = document.createElement("span");
    runtime.className = "chappa-agent-runtime";
    runtime.style.cssText = MUTED_CSS;
    runtime.textContent = runtimeSummary(tool.runtime);
    runtime.title = runtime.textContent;

    // A small "json" tag for machine-mode tools (tty is the
    // default and says nothing).
    const transport = document.createElement("span");
    transport.className = "chappa-agent-transport";
    transport.style.cssText = TAG_CSS;
    transport.textContent = tool.transport === "json" ? "json" : "";
    transport.title = "structured transport: machine-mode JSON events, transcript view";
    transport.style.display = tool.transport === "json" ? "" : "none";

    const edit = document.createElement("button");
    edit.type = "button";
    edit.textContent = "Edit";
    edit.style.cssText = SMALL_BTN_CSS;
    edit.dataset.action = "editAgent";
    edit.addEventListener("click", () => {
      const current = this.store.byId(tool.id);
      if (!current) return;
      this.openModal({ tool: current, warnings: [], originalId: tool.id });
    });

    const del = document.createElement("button");
    del.type = "button";
    del.textContent = "Delete";
    del.style.cssText = SMALL_BTN_CSS;
    del.dataset.action = "deleteAgent";
    del.addEventListener("click", () => void this.remove(tool.id));

    el.append(toggle, name, type, model, runtime, transport, edit, del);
    return el;
  }

  private showError(message: string | null): void {
    this.error.textContent = message ?? "";
    this.error.style.display = message ? "" : "none";
  }

  private async commit(tool: AgentTool): Promise<boolean> {
    try {
      await this.store.upsert(tool);
      this.showError(null);
      return true;
    } catch (err) {
      this.showError(String(err instanceof Error ? err.message : err));
      return false;
    }
  }

  /** Delete, or show Rust's refusal (it names the live processes). */
  private async remove(id: number): Promise<void> {
    try {
      await this.store.delete(id);
      this.showError(null);
    } catch (err) {
      this.showError(String(err instanceof Error ? err.message : err));
    }
  }

  private async addFromCommand(): Promise<void> {
    const line = this.pasteInput.value.trim();
    if (line === "") return;
    try {
      const parsed = await this.store.parse(line);
      this.pasteInput.value = "";
      this.showError(null);
      this.openModal({ tool: parsed.tool, warnings: parsed.warnings, originalId: 0 });
    } catch (err) {
      this.showError(String(err instanceof Error ? err.message : err));
    }
  }

  // --- agent settings ---------------------------------------------------------

  private numberSetting(
    label: string,
    setting: "agentReadyQuietMs" | "agentReadyMaxWaitMs" | "agentStaleAfterS" | TimerSetting,
    min: number,
    max: number,
  ): HTMLInputElement {
    const row = document.createElement("div");
    row.style.cssText = SETTING_ROW_CSS;
    const text = document.createElement("label");
    text.style.cssText = SETTING_LABEL_CSS;
    text.textContent = label;
    const input = document.createElement("input");
    input.type = "number";
    input.min = String(min);
    input.max = String(max);
    input.style.cssText = `${INPUT_CSS}width:90px;`;
    input.dataset.setting = setting;
    input.id = `chappa-agent-set-${++ids}`;
    text.htmlFor = input.id;
    // Commit on change (blur/Enter), never per keystroke: every write is a
    // full-struct set_settings.
    input.addEventListener("change", () => {
      const n = Number(input.value);
      if (!Number.isFinite(n)) {
        input.value = String(this.settings.get()[setting]);
        return;
      }
      void this.settings.update({ [setting]: n } as Partial<Settings>);
    });
    row.append(text, input);
    this.element.appendChild(row);
    return input;
  }

  private boolSetting(label: string, setting: keyof Settings, why: string): HTMLInputElement {
    const row = document.createElement("div");
    row.style.cssText = SETTING_ROW_CSS;
    const text = document.createElement("label");
    text.style.cssText = SETTING_LABEL_CSS;
    text.textContent = label;
    const input = document.createElement("input");
    input.type = "checkbox";
    input.style.cssText = "flex:none;accent-color:#5ea6ff;";
    input.dataset.setting = setting;
    input.id = `chappa-agent-set-${++ids}`;
    text.htmlFor = input.id;
    input.addEventListener("change", () => {
      void this.settings.update({ [setting]: input.checked } as Partial<Settings>);
    });
    row.append(text, input);
    this.element.appendChild(row);
    this.element.appendChild(hint(why));
    return input;
  }

  private syncSettings(s: Settings): void {
    if (document.activeElement !== this.quietInput) this.quietInput.value = String(s.agentReadyQuietMs);
    if (document.activeElement !== this.maxWaitInput) this.maxWaitInput.value = String(s.agentReadyMaxWaitMs);
    if (document.activeElement !== this.staleInput) this.staleInput.value = String(s.agentStaleAfterS);
    this.reapInput.checked = s.agentReapOrphans;
    for (const { input, key } of this.timerInputs) {
      if (document.activeElement !== input) input.value = String(s[key]);
    }
  }

  // --- the edit modal --------------------------------------------------------

  private closeModal(): void {
    this.modalPicker?.dispose();
    this.modalPicker = null;
    this.modal?.remove();
    this.modal = null;
    document.removeEventListener("keydown", this.escape, true);
  }

  private readonly escape = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    e.preventDefault();
    e.stopPropagation();
    this.closeModal();
  };

  private openModal(draft: Draft): void {
    this.closeModal();
    const tool = draft.tool;
    const wrap = document.createElement("div");
    wrap.className = "chappa-agent-modal";
    wrap.style.cssText = BACKDROP_CSS;
    wrap.setAttribute("role", "dialog");
    wrap.setAttribute("aria-label", draft.originalId ? "Edit agent tool" : "New agent tool");
    const box = document.createElement("div");
    box.style.cssText = BOX_CSS;
    wrap.appendChild(box);
    const title = document.createElement("div");
    title.style.cssText = "font:600 15px system-ui,sans-serif;padding-bottom:8px;";
    title.textContent = draft.originalId ? "Edit agent tool" : "New agent tool";
    box.appendChild(title);

    if (draft.warnings.length > 0) {
      const warn = document.createElement("div");
      warn.className = "chappa-agent-modal-warning";
      warn.style.cssText = ERROR_CSS;
      warn.textContent = draft.warnings.join("\n");
      box.appendChild(warn);
    }

    const field = (label: string, control: HTMLElement): HTMLDivElement => {
      const row = document.createElement("div");
      row.style.cssText = FIELD_CSS;
      const text = document.createElement("label");
      text.style.cssText = FIELD_LABEL_CSS;
      text.textContent = label;
      if (!control.id) control.id = `chappa-agent-f-${++ids}`;
      text.htmlFor = control.id;
      row.append(text, control);
      box.appendChild(row);
      return row;
    };
    const textInput = (name: string, value: string, placeholder = ""): HTMLInputElement => {
      const input = document.createElement("input");
      input.type = "text";
      input.value = value;
      input.placeholder = placeholder;
      input.spellcheck = false;
      input.style.cssText = `${INPUT_CSS}flex:1 1 auto;`;
      input.dataset.field = name;
      return input;
    };

    const nameInput = textInput("name", tool.name, "display name (never parsed)");
    field("Name", nameInput);

    const typeSelect = document.createElement("select");
    typeSelect.style.cssText = SELECT_CSS;
    typeSelect.dataset.field = "tool_type";
    for (const t of AGENT_TOOL_TYPES) {
      const opt = document.createElement("option");
      opt.value = t;
      opt.textContent = t;
      typeSelect.appendChild(opt);
    }
    typeSelect.value = tool.tool_type;
    field("Type", typeSelect);

    const programInput = textInput("program", tool.program, "the agent CLI, e.g. claude");
    field("Program", programInput);

    // argv: one entry per LINE — each line reaches the child as exactly one
    // argument, spaces included (the never-re-split rule made visible).
    const argsInput = document.createElement("textarea");
    argsInput.value = tool.args.join("\n");
    argsInput.rows = Math.max(2, Math.min(6, tool.args.length + 1));
    argsInput.spellcheck = false;
    argsInput.placeholder = "one argument per line";
    argsInput.style.cssText = `${INPUT_CSS}flex:1 1 auto;resize:vertical;`;
    argsInput.dataset.field = "args";
    field("Args", argsInput);

    const modelInput = textInput("model", tool.model ?? "", "attribution only");
    field("Model", modelInput);

    // Runtime picker + the docker-only fields it reveals.
    let runtimeKind: "host" | "docker_exec" = tool.runtime.kind;
    const docker = tool.runtime.kind === "docker_exec" ? tool.runtime : null;
    const containerInput = textInput("container", docker?.container ?? "", "container name");
    const userInput = textInput("user", docker?.user ?? "", "-u (optional)");
    const workdirInput = textInput("workdir", docker?.workdir ?? "", "-w (optional)");
    const ttyToggle = document.createElement("input");
    ttyToggle.type = "checkbox";
    ttyToggle.checked = docker ? docker.tty !== false : true;
    ttyToggle.dataset.field = "tty";
    ttyToggle.style.cssText = "flex:none;accent-color:#5ea6ff;";
    const busyContainerInput = document.createElement("input");
    busyContainerInput.type = "number";
    busyContainerInput.min = "0";
    busyContainerInput.value = docker?.max_busy_in_container != null ? String(docker.max_busy_in_container) : "";
    busyContainerInput.placeholder = "unlimited";
    busyContainerInput.style.cssText = `${INPUT_CSS}width:90px;`;
    busyContainerInput.dataset.field = "max_busy_in_container";
    const dockerRows: HTMLDivElement[] = [];
    this.modalPicker = new Segmented<"host" | "docker_exec">({
      label: "Runtime",
      options: [
        { value: "host", label: "Host" },
        { value: "docker_exec", label: "Docker" },
      ],
      value: runtimeKind,
      onChange: (kind) => {
        runtimeKind = kind;
        for (const row of dockerRows) row.style.display = kind === "docker_exec" ? "" : "none";
      },
    });
    this.modalPicker.element.dataset.field = "runtime";
    field("Runtime", this.modalPicker.element);
    dockerRows.push(
      field("Container", containerInput),
      field("User", userInput),
      field("Workdir", workdirInput),
      field("TTY (-it)", ttyToggle),
      field("Max busy in container", busyContainerInput),
    );
    for (const row of dockerRows) row.style.display = runtimeKind === "docker_exec" ? "" : "none";

    // Transport picker. `json` is OFFERED only for types with a
    // machine mode (claude, opencode); switching the type to anything else
    // snaps it back to tty (Rust refuses json without a machine mode).
    let transport: AgentTool["transport"] = tool.transport ?? "tty";
    const transportPicker = new Segmented<AgentTool["transport"]>({
      label: "Transport",
      options: [
        { value: "tty", label: "TTY" },
        { value: "json", label: "JSON" },
      ],
      value: transport,
      onChange: (value) => {
        transport = value;
      },
    });
    transportPicker.element.dataset.field = "transport";
    const transportHint = document.createElement("span");
    transportHint.className = "chappa-agent-transport-hint";
    transportHint.style.cssText = MUTED_CSS;
    const transportWrap = document.createElement("div");
    transportWrap.style.cssText = "display:flex;align-items:center;gap:10px;flex:1 1 auto;";
    transportWrap.append(transportPicker.element, transportHint);
    const syncTransport = (): void => {
      const offered = this.store.hasMachineMode(typeSelect.value as AgentToolType);
      if (!offered && transport !== "tty") {
        transport = "tty";
        transportPicker.setValue("tty");
      }
      transportPicker.element.style.display = offered ? "" : "none";
      transportHint.textContent = offered
        ? "JSON = machine mode over pipes: typed events + transcript view instead of a terminal."
        : `tty only — ${typeSelect.value} has no machine mode.`;
    };
    typeSelect.addEventListener("change", syncTransport);
    syncTransport();
    field("Transport", transportWrap);

    // Env rows: key/value pairs, each deletable — the claude template's
    // CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 is one of these, visibly.
    const envWrap = document.createElement("div");
    envWrap.className = "chappa-agent-env";
    envWrap.style.cssText = "display:flex;flex-direction:column;gap:4px;flex:1 1 auto;";
    const envRows: Array<{ el: HTMLDivElement; key: HTMLInputElement; value: HTMLInputElement }> = [];
    const addEnvRow = (k: string, v: string): void => {
      const row = document.createElement("div");
      row.className = "chappa-agent-env-row";
      row.style.cssText = "display:flex;gap:6px;align-items:center;";
      const key = textInput("envKey", k, "KEY");
      key.style.flex = "0 1 220px";
      const value = textInput("envValue", v, "value");
      const remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "×";
      remove.title = "Remove this env row";
      remove.style.cssText = SMALL_BTN_CSS;
      remove.dataset.action = "removeEnv";
      remove.addEventListener("click", () => {
        row.remove();
        const i = envRows.findIndex((r) => r.el === row);
        if (i >= 0) envRows.splice(i, 1);
      });
      row.append(key, value, remove);
      envWrap.appendChild(row);
      envRows.push({ el: row, key, value });
    };
    for (const [k, v] of Object.entries(tool.env)) addEnvRow(k, v);
    const addEnv = document.createElement("button");
    addEnv.type = "button";
    addEnv.textContent = "+ env";
    addEnv.style.cssText = `${SMALL_BTN_CSS}align-self:flex-start;`;
    addEnv.dataset.action = "addEnv";
    addEnv.addEventListener("click", () => addEnvRow("", ""));
    envWrap.appendChild(addEnv);
    field("Env", envWrap);

    const busyInput = document.createElement("input");
    busyInput.type = "number";
    busyInput.min = "0";
    busyInput.value = tool.max_busy != null ? String(tool.max_busy) : "";
    busyInput.placeholder = "unlimited";
    busyInput.style.cssText = `${INPUT_CSS}width:90px;`;
    busyInput.dataset.field = "max_busy";
    field("Max busy (this tool)", busyInput);

    const enabledToggle = document.createElement("input");
    enabledToggle.type = "checkbox";
    enabledToggle.checked = tool.enabled;
    enabledToggle.dataset.field = "enabled";
    enabledToggle.style.cssText = "flex:none;accent-color:#5ea6ff;";
    field("Enabled", enabledToggle);

    const error = document.createElement("div");
    error.className = "chappa-agent-modal-error";
    error.style.cssText = ERROR_CSS;
    error.style.display = "none";
    box.appendChild(error);

    const buttons = document.createElement("div");
    buttons.style.cssText = BUTTON_ROW_CSS;
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.style.cssText = SMALL_BTN_CSS;
    cancel.dataset.action = "cancelAgent";
    cancel.addEventListener("click", () => this.closeModal());
    const save = document.createElement("button");
    save.type = "button";
    save.textContent = "Save";
    save.style.cssText = OK_BTN_CSS;
    save.dataset.action = "saveAgent";
    save.addEventListener("click", () => {
      const program = programInput.value.trim();
      if (program === "") {
        error.textContent = "Program is required.";
        error.style.display = "";
        return;
      }
      const env: Record<string, string> = {};
      for (const r of envRows) {
        const k = r.key.value.trim();
        if (k !== "") env[k] = r.value.value;
      }
      const limit = (input: HTMLInputElement): number | null => {
        const n = Number(input.value);
        return input.value.trim() === "" || !Number.isFinite(n) || n <= 0 ? null : Math.floor(n);
      };
      let runtime: AgentTool["runtime"];
      if (runtimeKind === "docker_exec") {
        const container = containerInput.value.trim();
        if (container === "") {
          error.textContent = "Container is required for a Docker runtime.";
          error.style.display = "";
          return;
        }
        runtime = {
          kind: "docker_exec",
          container,
          user: userInput.value.trim() || null,
          workdir: workdirInput.value.trim() || null,
          tty: ttyToggle.checked,
          max_busy_in_container: limit(busyContainerInput),
        };
      } else {
        runtime = { kind: "host" };
      }
      const next: AgentTool = {
        id: draft.originalId,
        name: nameInput.value.trim() || program,
        tool_type: typeSelect.value as AgentToolType,
        program,
        args: argsInput.value.split("\n").map((a) => a.replace(/\r$/, "")).filter((a) => a !== ""),
        model: modelInput.value.trim() || null,
        runtime,
        env,
        enabled: enabledToggle.checked,
        max_busy: limit(busyInput),
        transport: this.store.hasMachineMode(typeSelect.value as AgentToolType) ? transport : "tty",
      };
      void this.commit(next).then((ok) => {
        if (ok) this.closeModal();
        else {
          error.textContent = this.error.textContent;
          error.style.display = "";
        }
      });
    });
    buttons.append(cancel, save);
    box.appendChild(buttons);

    wrap.addEventListener("mousedown", (e) => {
      if (e.target === wrap) this.closeModal();
    });
    document.addEventListener("keydown", this.escape, true);
    this.modalContainer.appendChild(wrap);
    this.modal = wrap;
    nameInput.focus();
  }
}

function hint(text: string): HTMLDivElement {
  const el = document.createElement("div");
  el.style.cssText = HINT_CSS;
  el.textContent = text;
  return el;
}

function subheading(text: string): HTMLDivElement {
  const el = document.createElement("div");
  el.style.cssText = SUBHEADING_CSS;
  el.textContent = text;
  return el;
}
