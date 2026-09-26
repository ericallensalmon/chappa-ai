// The settings view. A FULL-WINDOW view above the terminal stack —
// deliberately NOT a second window: a window brings its own chrome, its own
// webview and a whole focus-restoration problem for one screen of controls.
//
// Layout: a pinned header ("← Settings" back control, then the tab row) and,
// beneath it, ONE tab's sections in a column that scrolls on its own. The
// header never scrolls away, so the way out is always on screen — with a
// scrolling page the close control disappeared as soon as a long tab was
// scrolled, and reaching for it closed the window instead.
//
// Tabs: Terminal (behavior, appearance, shell profiles), Agents (the tool
// registry and the agent knobs), Timers (the idle-timer knobs). Only tabs
// with content exist; the last tab used is remembered.
//
// No Save button: every control change calls `store.update()` immediately and
// the store adopts whatever Rust returns (it clamps). The pane then re-syncs
// from that canonical value, so a rejected/clamped edit visibly snaps back.
//
// Dismiss is the back control or Escape only. There is deliberately NO
// outside-click dismiss: a full-window view has no outside.
//
// Deliberately NOT exposed here: letter spacing — it became the
// cell-quantization delta, so a user multiplier would fight the shared-grid
// invariant every consumer (PTY resize, mouse mapping, GL quads) depends on —
// and font-weight pickers.

import { Segmented } from "./segmented";
import { AgentsSection } from "./agents_section";
import { TimersSection } from "./timers_section";
import { agentToolsStore, type AgentToolsStore } from "./agent_tools";
import {
  BUILTIN_EXEC_PROFILES,
  BUNDLED_FONTS,
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  LINE_HEIGHTS,
  WHEEL_SPEEDS,
  nearestLineHeight,
  type Settings,
  type SettingsStore,
  type ShellProfile,
} from "./settings";
import {
  CHECKBOX_CSS,
  INPUT_CSS,
  SELECT_CSS,
  SMALL_BTN_CSS,
  injectSettingsStyles,
  settingsBlock,
  settingsRow,
  settingsSection,
} from "./settings_widgets";

export type SettingsTab = "terminal" | "agents" | "timers";

const TABS: ReadonlyArray<{ id: SettingsTab; label: string }> = [
  { id: "terminal", label: "Terminal" },
  { id: "agents", label: "Agents" },
  { id: "timers", label: "Timers" },
];
const DEFAULT_TAB: SettingsTab = "terminal";
/** Where the last-used tab is remembered. localStorage: pure UI convenience,
 *  nothing Rust needs to know. */
const TAB_KEY = "chappa.settings.tab";

// Full-window, not a drawer: top/right/bottom/left rather than `inset` so the
// inline style round-trips through every engine (and jsdom) unshortened. The
// pane itself never scrolls — the column below the header does.
const PANE_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:60;display:none;" +
  "flex-direction:column;overflow:hidden;background:#0d0e10;" +
  "color:#d6d8dc;font:13px system-ui,sans-serif;";
const HEADER_CSS = "flex:none;padding:12px 32px 0;border-bottom:1px solid #1c1f24;";
const BACK_CSS =
  "display:inline-flex;align-items:center;gap:10px;border:0;background:none;color:#8b949e;" +
  "font:600 18px system-ui,sans-serif;cursor:pointer;padding:4px 0 10px;";
const BACK_TITLE_CSS = "color:#d6d8dc;";
const TABS_CSS = "display:flex;gap:4px;padding:0 0 10px;overflow-x:auto;";
const TAB_CSS =
  "flex:none;border:0;background:none;color:#8b949e;font:13px system-ui,sans-serif;" +
  "padding:6px 12px;border-radius:6px;cursor:pointer;";
/** Full-width content: no centred max-width column, so the header spans the
 *  window and the sections use the whole screen. */
const COLUMN_CSS =
  "flex:1 1 auto;min-height:0;overflow-y:auto;width:100%;padding:6px 32px 64px;box-sizing:border-box;";
const STEP_BTN_CSS =
  "width:22px;height:22px;border:1px solid #262a31;border-radius:4px;background:#16181d;" +
  "color:#d6d8dc;font:12px ui-monospace,monospace;cursor:pointer;padding:0;";
const STEP_VALUE_CSS =
  "min-width:46px;text-align:center;font:12px ui-monospace,monospace;color:#d6d8dc;";
const PROFILE_ROW_CSS = "display:flex;align-items:center;gap:6px;padding:4px 0;";

/** localStorage, or null where there is none or where touching it throws. */
function safeStorage(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch {
    return null;
  }
}

function isTab(value: unknown): value is SettingsTab {
  return TABS.some((t) => t.id === value);
}

/** The remembered tab; unknown or missing → Terminal. */
function readTab(): SettingsTab {
  try {
    const stored = safeStorage()?.getItem(TAB_KEY);
    return isTab(stored) ? stored : DEFAULT_TAB;
  } catch {
    return DEFAULT_TAB;
  }
}

function writeTab(tab: SettingsTab): void {
  try {
    safeStorage()?.setItem(TAB_KEY, tab);
  } catch {
    // Storage full or disabled: the tab still switches, it just is not
    // remembered. Not worth surfacing.
  }
}

export interface SettingsPaneOptions {
  store: SettingsStore;
  /** The agent-tool registry behind the Agents tab. Defaults to the app-wide
   *  store; tests inject one over a fake api. */
  agentTools?: AgentToolsStore;
  /** Where the pane attaches. Defaults to document.body. */
  container?: HTMLElement;
  /** Fired after a close so the caller can hand focus back to the terminal. */
  onClose?: () => void;
}

interface ProfileRowRefs {
  el: HTMLDivElement;
  toggle: HTMLInputElement;
  name: HTMLInputElement;
  command: HTMLInputElement;
}

export class SettingsPane {
  private readonly opts: SettingsPaneOptions;
  private readonly store: SettingsStore;
  private readonly wrap: HTMLDivElement;
  private readonly tabButtons = new Map<SettingsTab, HTMLButtonElement>();
  private readonly panels = new Map<SettingsTab, HTMLDivElement>();
  private tab: SettingsTab = DEFAULT_TAB;

  private readonly copyToggle: HTMLInputElement;
  /** The clipboard-key toggles (both default ON). */
  private readonly ctrlVToggle: HTMLInputElement;
  private readonly ctrlCToggle: HTMLInputElement;
  private readonly marksToggle: HTMLInputElement;
  private readonly wheelPicker: Segmented<number>;
  private readonly lineHeightPicker: Segmented<number>;
  private readonly fontSizeValue: HTMLSpanElement;
  private readonly fontSizeDown: HTMLButtonElement;
  private readonly fontSizeUp: HTMLButtonElement;
  private readonly familySelect: HTMLSelectElement;
  private readonly execSelect: HTMLSelectElement;
  private readonly profileList: HTMLDivElement;
  private readonly addName: HTMLInputElement;
  private readonly addCommand: HTMLInputElement;
  private readonly addButton: HTMLButtonElement;

  /** The Agents tab: its own store, its own list/modal. */
  private readonly agents: AgentsSection;
  /** The Timers tab. */
  private readonly timers: TimersSection;

  private profileRows: ProfileRowRefs[] = [];
  /** The profile-name list the current rows were built from; rebuilding on
   *  every store notify would drop focus mid-edit, so a rebuild only happens
   *  when the set of profiles actually changes. */
  private profileSig = "\u0000never-rendered";
  private opened = false;
  private unsubscribe: (() => void) | null = null;

  constructor(opts: SettingsPaneOptions) {
    this.opts = opts;
    this.store = opts.store;
    injectSettingsStyles();

    this.wrap = document.createElement("div");
    this.wrap.className = "chappa-settings-pane";
    this.wrap.style.cssText = PANE_CSS;
    this.wrap.setAttribute("role", "dialog");
    this.wrap.setAttribute("aria-label", "Settings");

    // --- pinned header: back control + tab row --------------------------------
    const header = document.createElement("div");
    header.className = "chappa-settings-header";
    header.style.cssText = HEADER_CSS;
    const back = document.createElement("button");
    back.type = "button";
    back.className = "chappa-settings-close";
    back.title = "Back to the workspace (Esc)";
    back.setAttribute("aria-label", "Back to the workspace");
    back.style.cssText = BACK_CSS;
    const arrow = document.createElement("span");
    arrow.setAttribute("aria-hidden", "true");
    arrow.textContent = "←";
    const title = document.createElement("span");
    title.style.cssText = BACK_TITLE_CSS;
    title.textContent = "Settings";
    back.append(arrow, title);
    back.addEventListener("click", () => this.close());

    const tabs = document.createElement("div");
    tabs.className = "chappa-settings-tabs";
    tabs.style.cssText = TABS_CSS;
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "Settings sections");
    for (const { id, label } of TABS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-settings-tab";
      btn.style.cssText = TAB_CSS;
      btn.id = `chappa-settings-tab-${id}`;
      btn.dataset.tab = id;
      btn.setAttribute("role", "tab");
      btn.setAttribute("aria-controls", `chappa-settings-panel-${id}`);
      btn.textContent = label;
      btn.addEventListener("click", () => this.selectTab(id));
      tabs.appendChild(btn);
      this.tabButtons.set(id, btn);

      const panel = document.createElement("div");
      panel.className = "chappa-settings-panel";
      panel.id = `chappa-settings-panel-${id}`;
      panel.dataset.tab = id;
      panel.setAttribute("role", "tabpanel");
      panel.setAttribute("aria-labelledby", btn.id);
      this.panels.set(id, panel);
    }
    // ←/→ (and Home/End) move between tabs — the tablist convention — and the
    // moved-to tab takes focus so a keyboard user can keep going.
    tabs.addEventListener("keydown", (e) => {
      const idx = TABS.findIndex((t) => t.id === this.tab);
      let next: number;
      if (e.key === "ArrowRight") next = (idx + 1) % TABS.length;
      else if (e.key === "ArrowLeft") next = (idx - 1 + TABS.length) % TABS.length;
      else if (e.key === "Home") next = 0;
      else if (e.key === "End") next = TABS.length - 1;
      else return;
      e.preventDefault();
      this.selectTab(TABS[next].id);
      this.tabButtons.get(TABS[next].id)?.focus();
    });
    header.append(back, tabs);

    // --- Terminal tab --------------------------------------------------------
    const terminal = this.panels.get("terminal")!;

    const behavior = settingsSection("Behavior");
    this.copyToggle = this.toggle("copyOnSelect");
    behavior.card.appendChild(settingsRow("Copy on select", this.copyToggle));
    // The two clipboard keys, next to their clipboard sibling. Both default ON
    // and are frontend-only consumers — the terminal input layer reads the
    // live store.
    this.ctrlVToggle = this.toggle("ctrlVPastes");
    behavior.card.appendChild(
      settingsRow(
        "Ctrl+V pastes",
        this.ctrlVToggle,
        "Ctrl+V (no Alt/Shift) pastes the clipboard into the terminal; otherwise it sends the raw 0x16 " +
          "character (vim's literal-insert). On by default: while on, ^V never reaches a TUI — deliberate.",
      ),
    );
    this.ctrlCToggle = this.toggle("ctrlCCopyOnly");
    behavior.card.appendChild(
      settingsRow(
        "Ctrl+C is copy-only",
        this.ctrlCToggle,
        "Ctrl+C never reaches the terminal; stop a process from its rail row. With a selection it copies; " +
          "without one it does nothing. Cmd+C on macOS keeps its usual behavior either way. On by default: " +
          "a reflexive ^C must not kill a long agent run — Escape and the rail stop are the remaining " +
          "cancel paths. Deliberate.",
      ),
    );
    this.wheelPicker = new Segmented<number>({
      label: "Scroll wheel speed",
      options: WHEEL_SPEEDS.map((n) => ({ value: n, label: `${n}x` })),
      value: 3,
      onChange: (n) => void this.store.update({ scrollWheelSpeed: n }),
    });
    this.wheelPicker.element.dataset.setting = "scrollWheelSpeed";
    behavior.card.appendChild(
      settingsRow(
        "Scroll wheel speed",
        this.wheelPicker.element,
        "Applies only while a TUI captures the mouse (vim, htop); plain scrollback stays at system speed.",
      ),
    );
    this.marksToggle = this.toggle("syntheticPromptMarks");
    behavior.card.appendChild(
      settingsRow(
        "Synthetic prompt marks",
        this.marksToggle,
        "Guesses: every plain Enter outside a full-screen app plants a prompt mark, including an Enter " +
          "that only cancels a dialog or answers a y/n. Off by default; turn it on for agent CLIs that " +
          "emit no real OSC 133 marks. Full-screen (alt-screen) apps plant nothing — Claude Code needs " +
          "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 to stay in normal scrollback.",
      ),
    );

    const appearance = settingsSection("Appearance", "The terminal face. The rest of the app uses the system font.");
    this.familySelect = document.createElement("select");
    this.familySelect.style.cssText = SELECT_CSS;
    this.familySelect.dataset.setting = "fontFamily";
    for (const face of BUNDLED_FONTS) {
      const opt = document.createElement("option");
      opt.value = face;
      opt.textContent = face;
      this.familySelect.appendChild(opt);
    }
    this.familySelect.addEventListener("change", () => {
      void this.store.update({ fontFamily: this.familySelect.value });
    });
    appearance.card.appendChild(settingsRow("Font family", this.familySelect));
    // Font size stepper (CSS px). The buttons disable ON the bounds so the
    // limits are visible rather than silently swallowed.
    this.fontSizeDown = this.stepButton("−", "Smaller font", -1);
    this.fontSizeUp = this.stepButton("+", "Larger font", 1);
    this.fontSizeValue = document.createElement("span");
    this.fontSizeValue.style.cssText = STEP_VALUE_CSS;
    this.fontSizeValue.dataset.setting = "fontSize";
    const stepper = document.createElement("div");
    stepper.style.cssText = "display:flex;align-items:center;gap:4px;";
    stepper.append(this.fontSizeDown, this.fontSizeValue, this.fontSizeUp);
    appearance.card.appendChild(settingsRow("Font size", stepper));
    this.lineHeightPicker = new Segmented<number>({
      label: "Line height",
      options: LINE_HEIGHTS.map((h) => ({ value: h, label: h.toFixed(1) })),
      value: 1.2,
      onChange: (h) => void this.store.update({ lineHeight: h }),
    });
    this.lineHeightPicker.element.dataset.setting = "lineHeight";
    appearance.card.appendChild(settingsRow("Line height", this.lineHeightPicker.element));

    const shells = settingsSection(
      "Shell profiles",
      "The shells the new-terminal menu offers. A disabled profile is hidden from the menu but stays available as the command runner.",
    );
    this.profileList = document.createElement("div");
    this.profileList.className = "chappa-profile-list";
    this.addName = document.createElement("input");
    this.addName.type = "text";
    this.addName.placeholder = "Name";
    this.addName.spellcheck = false;
    this.addName.style.cssText = `${INPUT_CSS}flex:0 1 160px;`;
    this.addName.dataset.field = "newProfileName";
    this.addCommand = document.createElement("input");
    this.addCommand.type = "text";
    this.addCommand.placeholder = "Command line";
    this.addCommand.spellcheck = false;
    this.addCommand.style.cssText = `${INPUT_CSS}flex:1 1 auto;`;
    this.addCommand.dataset.field = "newProfileCommand";
    this.addButton = document.createElement("button");
    this.addButton.type = "button";
    this.addButton.textContent = "Add";
    this.addButton.style.cssText = SMALL_BTN_CSS;
    this.addButton.dataset.action = "addProfile";
    this.addButton.addEventListener("click", () => void this.addProfile());
    const addRow = document.createElement("div");
    addRow.style.cssText = PROFILE_ROW_CSS;
    addRow.append(this.addName, this.addCommand, this.addButton);
    shells.card.appendChild(settingsBlock(this.profileList, addRow));
    this.execSelect = document.createElement("select");
    this.execSelect.style.cssText = SELECT_CSS;
    this.execSelect.dataset.setting = "defaultExecProfile";
    this.execSelect.addEventListener("change", () => {
      void this.store.update({ defaultExecProfile: this.execSelect.value });
    });
    shells.card.appendChild(
      settingsRow("Default execution profile", this.execSelect, "The shell that runs chappa.yml command lines."),
    );
    terminal.append(behavior.element, appearance.element, shells.element);

    // --- Agents tab ------------------------------------------------------------
    this.agents = new AgentsSection({
      store: opts.agentTools ?? agentToolsStore,
      settings: this.store,
      modalContainer: opts.container,
    });
    this.panels.get("agents")!.appendChild(this.agents.element);

    // --- Timers tab ------------------------------------------------------------
    this.timers = new TimersSection({ settings: this.store });
    this.panels.get("timers")!.appendChild(this.timers.element);

    const column = document.createElement("div");
    column.className = "chappa-settings-column";
    column.style.cssText = COLUMN_CSS;
    for (const { id } of TABS) column.appendChild(this.panels.get(id)!);
    this.wrap.append(header, column);
    (opts.container ?? document.body).appendChild(this.wrap);

    this.selectTab(readTab(), false);
    this.sync(this.store.get());
  }

  /** The pane's root element (tests drive the controls through it). */
  get element(): HTMLDivElement {
    return this.wrap;
  }

  isOpen(): boolean {
    return this.opened;
  }

  /** The tab currently shown. */
  get activeTab(): SettingsTab {
    return this.tab;
  }

  /** Show one tab: the others' panels hide, the pill and aria state follow,
   *  and the choice is remembered for the next open. */
  selectTab(tab: SettingsTab, remember = true): void {
    this.tab = tab;
    for (const { id } of TABS) {
      const selected = id === tab;
      const btn = this.tabButtons.get(id)!;
      btn.setAttribute("aria-selected", String(selected));
      btn.tabIndex = selected ? 0 : -1;
      this.panels.get(id)!.hidden = !selected;
    }
    if (remember) writeTab(tab);
  }

  /** Open the pane — on `tab` when given (a deep link), else on the tab used
   *  last time. */
  open(tab?: SettingsTab): void {
    if (tab) this.selectTab(tab);
    else if (!this.opened) this.selectTab(readTab(), false);
    if (this.opened) return;
    this.opened = true;
    this.sync(this.store.get());
    this.unsubscribe = this.store.subscribe((s) => this.sync(s));
    this.agents.activate();
    this.timers.activate();
    this.wrap.style.display = "flex";
    document.addEventListener("keydown", this.escape, true);
  }

  close(): void {
    if (!this.opened) return;
    this.opened = false;
    this.wrap.style.display = "none";
    document.removeEventListener("keydown", this.escape, true);
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.agents.deactivate();
    this.timers.deactivate();
    this.opts.onClose?.();
  }

  /** The Agents section (tests drive its list and modal through it). */
  get agentsSection(): AgentsSection {
    return this.agents;
  }

  toggleOpen(): void {
    if (this.opened) this.close();
    else this.open();
  }

  dispose(): void {
    this.close();
    this.wheelPicker.dispose();
    this.lineHeightPicker.dispose();
    this.agents.dispose();
    this.timers.dispose();
    this.wrap.remove();
  }

  /** Push a canonical settings value into every control. Called on open and
   *  on every store notification — including the ones this pane's own edits
   *  cause, which is how a Rust-side clamp visibly snaps a control back. */
  sync(s: Settings): void {
    this.copyToggle.checked = s.copyOnSelect;
    this.ctrlVToggle.checked = s.ctrlVPastes;
    this.ctrlCToggle.checked = s.ctrlCCopyOnly;
    this.marksToggle.checked = s.syntheticPromptMarks;
    this.wheelPicker.setValue(s.scrollWheelSpeed);
    // An off-ladder stored value (hand-edited config; Rust round-trips those)
    // displays as its nearest step — the store keeps the exact number.
    this.lineHeightPicker.setValue(nearestLineHeight(s.lineHeight));
    this.fontSizeValue.textContent = `${s.fontSize} px`;
    this.fontSizeDown.disabled = s.fontSize <= FONT_SIZE_MIN;
    this.fontSizeUp.disabled = s.fontSize >= FONT_SIZE_MAX;
    this.familySelect.value = s.fontFamily;
    this.syncProfiles(s.shellProfiles);
    this.syncExecProfiles(s);
  }

  // --- dismissal -----------------------------------------------------------

  private readonly escape = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    // The agent edit modal owns Escape while it is up (its own capture
    // listener closes it and stops propagation before this one runs).
    if (this.agents.modalElement) return;
    e.preventDefault();
    e.stopPropagation();
    this.close();
  };

  // --- builders ------------------------------------------------------------

  private toggle(setting: "copyOnSelect" | "ctrlVPastes" | "ctrlCCopyOnly" | "syntheticPromptMarks"): HTMLInputElement {
    const input = document.createElement("input");
    input.type = "checkbox";
    input.dataset.setting = setting;
    input.style.cssText = CHECKBOX_CSS;
    input.addEventListener("change", () => {
      void this.store.update({ [setting]: input.checked } as Partial<Settings>);
    });
    return input;
  }

  private stepButton(label: string, title: string, delta: number): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.textContent = label;
    btn.title = title;
    btn.style.cssText = STEP_BTN_CSS;
    btn.dataset.action = delta < 0 ? "fontSizeDown" : "fontSizeUp";
    btn.addEventListener("click", () => {
      // The store clamps too; clamping here keeps the pane from firing a
      // pointless write at the bound.
      const next = this.store.get().fontSize + delta;
      if (next < FONT_SIZE_MIN || next > FONT_SIZE_MAX) return;
      void this.store.update({ fontSize: next });
    });
    return btn;
  }

  // --- shell profiles ------------------------------------------------------

  private syncProfiles(profiles: ShellProfile[]): void {
    const sig = profiles.map((p) => p.name).join("\u0000");
    if (sig !== this.profileSig) {
      this.profileSig = sig;
      this.buildProfileRows(profiles);
      return;
    }
    // Same profiles: refresh values in place, never touching the control the
    // user is currently editing.
    profiles.forEach((p, i) => {
      const refs = this.profileRows[i];
      if (!refs) return;
      if (document.activeElement !== refs.toggle) refs.toggle.checked = p.enabled;
      if (document.activeElement !== refs.name) refs.name.value = p.name;
      if (document.activeElement !== refs.command) refs.command.value = p.command;
    });
  }

  private buildProfileRows(profiles: ShellProfile[]): void {
    this.profileList.textContent = "";
    this.profileRows = [];
    profiles.forEach((p, index) => {
      const el = document.createElement("div");
      el.style.cssText = PROFILE_ROW_CSS;
      el.dataset.profile = p.name;

      const toggle = document.createElement("input");
      toggle.type = "checkbox";
      toggle.checked = p.enabled;
      toggle.title = "Offer this profile in the new-terminal menu";
      toggle.style.cssText = CHECKBOX_CSS;
      toggle.dataset.field = "enabled";
      toggle.addEventListener("change", () => {
        void this.patchProfile(index, { enabled: toggle.checked });
      });

      const name = document.createElement("input");
      name.type = "text";
      name.value = p.name;
      name.spellcheck = false;
      name.style.cssText = `${INPUT_CSS}flex:0 1 160px;`;
      name.dataset.field = "name";
      // Commit on `change` (blur/Enter), not on every keystroke: a per-
      // keystroke set_settings would write the config file once per character
      // and broadcast to every actor.
      name.addEventListener("change", () => {
        const value = name.value.trim();
        if (value === "") {
          name.value = this.store.get().shellProfiles[index]?.name ?? "";
          return;
        }
        void this.patchProfile(index, { name: value });
      });

      const command = document.createElement("input");
      command.type = "text";
      command.value = p.command;
      command.spellcheck = false;
      command.style.cssText = `${INPUT_CSS}flex:1 1 auto;`;
      command.dataset.field = "command";
      command.addEventListener("change", () => {
        const value = command.value.trim();
        if (value === "") {
          command.value = this.store.get().shellProfiles[index]?.command ?? "";
          return;
        }
        void this.patchProfile(index, { command: value });
      });

      const remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "×";
      remove.title = `Remove ${p.name}`;
      remove.style.cssText = SMALL_BTN_CSS;
      remove.dataset.action = "removeProfile";
      remove.addEventListener("click", () => void this.removeProfile(index));

      el.append(toggle, name, command, remove);
      this.profileList.appendChild(el);
      this.profileRows.push({ el, toggle, name, command });
    });
  }

  private patchProfile(index: number, patch: Partial<ShellProfile>): Promise<Settings> {
    const s = this.store.get();
    const profiles = s.shellProfiles;
    const prev = profiles[index];
    if (!prev) return Promise.resolve(s);
    profiles[index] = { ...prev, ...patch };
    const update: Partial<Settings> = { shellProfiles: profiles };
    // A rename must follow the exec-profile reference, or the runner silently
    // falls back to the builtin while the dropdown shows "(missing)".
    if (patch.name !== undefined && patch.name !== prev.name && s.defaultExecProfile === prev.name) {
      update.defaultExecProfile = patch.name;
    }
    return this.store.update(update);
  }

  /** Add the typed profile. Validation is exactly "both fields non-empty"
   *  — the command line itself is Rust's problem to resolve. */
  private async addProfile(): Promise<void> {
    const name = this.addName.value.trim();
    const command = this.addCommand.value.trim();
    if (name === "" || command === "") return;
    const profiles = this.store.get().shellProfiles;
    profiles.push({ name, command, enabled: true });
    this.addName.value = "";
    this.addCommand.value = "";
    await this.store.update({ shellProfiles: profiles });
  }

  private async removeProfile(index: number): Promise<void> {
    const profiles = this.store.get().shellProfiles;
    if (!profiles[index]) return;
    profiles.splice(index, 1);
    await this.store.update({ shellProfiles: profiles });
  }

  /** The execution-profile dropdown: the builtins plus every profile NAME
   *  (enabled or not — disabling a profile hides it from the new-terminal
   *  menu, it does not retire it as the chappa.yml runner). */
  private syncExecProfiles(s: Settings): void {
    const wanted: string[] = [...BUILTIN_EXEC_PROFILES, ...s.shellProfiles.map((p) => p.name)];
    // A stored value that names nothing we offer (a deleted profile) stays
    // visible as its own "(missing)" option — INSIDE the signature compare:
    // appended after it, the guard was permanently unequal and every store
    // notification tore down and rebuilt the select.
    const missing = !wanted.includes(s.defaultExecProfile);
    if (missing) wanted.push(s.defaultExecProfile);
    const current = [...this.execSelect.options].map((o) => o.value);
    if (current.join("\u0000") !== wanted.join("\u0000")) {
      this.execSelect.textContent = "";
      wanted.forEach((value, i) => {
        const opt = document.createElement("option");
        opt.value = value;
        opt.textContent = missing && i === wanted.length - 1 ? `${value} (missing)` : value;
        this.execSelect.appendChild(opt);
      });
    }
    this.execSelect.value = s.defaultExecProfile;
  }
}
