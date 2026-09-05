// The settings overlay. A FULL-WINDOW view above the terminal
// stack — deliberately NOT a second window: a window brings its own chrome,
// its own webview and a whole focus-restoration problem for one screen of
// controls.
//
// It started as a 420px right-side drawer; the host verification
// killed that (the "Line height" label sat under its 9-option segmented
// picker). Full-window + a centred 680px column is the fix: the rows breathe,
// and the label owns a fixed 180px flex basis so a wide control right-aligns
// or wraps beneath it instead of covering it.
//
// No Save button: every control change calls `store.update()` immediately and
// the store adopts whatever Rust returns (it clamps). The pane then re-syncs
// from that canonical value, so a rejected/clamped edit visibly snaps back.
//
// Dismiss is × or Escape only — "x returning to dashboard". There is
// deliberately NO outside-click dismiss: a full-window view has no outside,
// and the capture-phase listener that used to serve one only existed to be
// defused by an inside-guard and an `anchor` exclusion for the gear.
//
// Deliberately NOT exposed here ):
// letter spacing — it became the cell-quantization delta, so a
// user multiplier would fight the shared-grid invariant every consumer
// (PTY resize, mouse mapping, GL quads) depends on — and font-weight pickers.

import { Segmented } from "./segmented";
import { AgentsSection } from "./agents_section";
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

// Full-window, not a drawer: top/right/bottom/left rather than `inset` so the
// inline style round-trips through every engine (and jsdom) unshortened.
const PANE_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:60;display:none;" +
  "flex-direction:column;overflow-y:auto;background:#0d0e10;" +
  "color:#d6d8dc;font:13px system-ui,sans-serif;";
/** Full-width content (feedback: "settings should stretch
 *  to use the whole screen — particularly the X to close should always be in
 *  the top right"). No max-width column: the header spans the window so the
 *  × sits at the screen's top-right corner. */
const COLUMN_CSS = "width:100%;padding:24px 32px 64px;box-sizing:border-box;";
const HEADER_CSS =
  "display:flex;align-items:center;gap:8px;padding:0 0 14px;border-bottom:1px solid #1c1f24;";
const TITLE_CSS = "flex:1 1 auto;font:600 18px system-ui,sans-serif;";
const CLOSE_CSS =
  "flex:none;border:1px solid #262a31;background:#16181d;color:#8b949e;font-size:18px;" +
  "line-height:1;cursor:pointer;padding:2px 9px;border-radius:6px;";
const SECTION_CSS = "padding:16px 0 20px;border-bottom:1px solid #1c1f24;";
const SECTION_LABEL_CSS =
  "font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;padding-bottom:8px;";
// `flex-wrap` + `space-between` is the label-overlap fix: the label owns a
// stable 180px basis, the control right-aligns, and a control too wide for the
// remainder (the 9-option line-height segmented) wraps to its own line rather
// than sliding over the words.
const ROW_CSS =
  "display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:8px 14px;" +
  "padding:9px 0;min-height:30px;";
const ROW_LABEL_CSS = "flex:0 0 180px;min-width:0;";
const HINT_CSS = "font:11px system-ui,sans-serif;color:#8b949e;padding:0 0 6px;line-height:1.45;";
const INPUT_CSS =
  "background:#0d0e10;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:3px 6px;" +
  "font:12px ui-monospace,monospace;outline:none;min-width:0;";
const SELECT_CSS =
  "background:#16181d;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:3px 6px;" +
  "font:12px system-ui,sans-serif;outline:none;";
const STEP_BTN_CSS =
  "width:22px;height:22px;border:1px solid #262a31;border-radius:4px;background:#16181d;" +
  "color:#d6d8dc;font:12px ui-monospace,monospace;cursor:pointer;padding:0;";
const STEP_VALUE_CSS =
  "min-width:46px;text-align:center;font:12px ui-monospace,monospace;color:#d6d8dc;";
const PROFILE_ROW_CSS = "display:flex;align-items:center;gap:6px;padding:4px 0;";
const SMALL_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:11px system-ui,sans-serif;cursor:pointer;padding:3px 8px;";

/** Unique-id source for the `<label for>` associations (a pane can be built
 *  more than once per document in tests). */
let rowIds = 0;

export interface SettingsPaneOptions {
  store: SettingsStore;
  /** The agent-tool registry behind the Agents section. Defaults
   *  to the app-wide store; tests inject one over a fake api. */
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

  /** The Agents section: its own store, its own list/modal. */
  private readonly agents: AgentsSection;

  private profileRows: ProfileRowRefs[] = [];
  /** The profile-name list the current rows were built from; rebuilding on
   *  every store notify would drop focus mid-edit (the lesson), so a
   *  rebuild only happens when the set of profiles actually changes. */
  private profileSig = "\u0000never-rendered";
  private opened = false;
  private unsubscribe: (() => void) | null = null;

  constructor(opts: SettingsPaneOptions) {
    this.opts = opts;
    this.store = opts.store;

    this.wrap = document.createElement("div");
    this.wrap.className = "chappa-settings-pane";
    this.wrap.style.cssText = PANE_CSS;
    this.wrap.setAttribute("role", "dialog");
    this.wrap.setAttribute("aria-label", "Settings");

    const header = document.createElement("div");
    header.style.cssText = HEADER_CSS;
    const title = document.createElement("span");
    title.style.cssText = TITLE_CSS;
    title.textContent = "Settings";
    const close = document.createElement("button");
    close.type = "button";
    close.className = "chappa-settings-close";
    close.textContent = "×";
    close.title = "Close settings (Esc)";
    close.style.cssText = CLOSE_CSS;
    close.addEventListener("click", () => this.close());
    header.append(title, close);

    // --- Terminal section --------------------------------------------------
    const terminal = this.section("TERMINAL");

    this.copyToggle = this.toggle("copyOnSelect");
    terminal.append(
      this.row("Copy on select", this.copyToggle),
    );

    // The two clipboard keys, next to their clipboard sibling. Both
    // default ON and are frontend-only consumers — the terminal input layer
    // reads the live store (no actor/broadcast work, unlike wheel speed).
    this.ctrlVToggle = this.toggle("ctrlVPastes");
    terminal.append(this.row("Ctrl+V pastes", this.ctrlVToggle));
    terminal.append(
      this.hint(
        "Ctrl+V (no Alt/Shift) pastes the clipboard into the terminal; " +
          "otherwise it sends the raw 0x16 character (vim's literal-insert). " +
          "On by default: while on, ^V never reaches a TUI — deliberate.",
      ),
    );
    this.ctrlCToggle = this.toggle("ctrlCCopyOnly");
    terminal.append(this.row("Ctrl+C is copy-only", this.ctrlCToggle));
    terminal.append(
      this.hint(
        "Ctrl+C never reaches the terminal; stop a process from its rail row. " +
          "With a selection it copies; without one it does nothing. Cmd+C on " +
          "macOS keeps its usual behavior either way. On by default: a " +
          "reflexive ^C must not kill a long agent run — Escape and the rail " +
          "stop are the remaining cancel paths. Deliberate.",
      ),
    );

    this.wheelPicker = new Segmented<number>({
      label: "Scroll wheel speed",
      options: WHEEL_SPEEDS.map((n) => ({ value: n, label: `${n}x` })),
      value: 3,
      onChange: (n) => void this.store.update({ scrollWheelSpeed: n }),
    });
    this.wheelPicker.element.dataset.setting = "scrollWheelSpeed";
    terminal.append(this.row("Scroll wheel speed", this.wheelPicker.element));
    terminal.append(
      this.hint("Applies only while a TUI captures the mouse (vim, htop); plain scrollback stays at system speed."),
    );

    // Font size stepper (CSS px, 10–18). The buttons disable ON the bounds so
    // the limits are visible rather than silently swallowed.
    this.fontSizeDown = this.stepButton("−", "Smaller font", -1);
    this.fontSizeUp = this.stepButton("+", "Larger font", 1);
    this.fontSizeValue = document.createElement("span");
    this.fontSizeValue.style.cssText = STEP_VALUE_CSS;
    this.fontSizeValue.dataset.setting = "fontSize";
    const stepper = document.createElement("div");
    stepper.style.cssText = "display:flex;align-items:center;gap:4px;";
    stepper.append(this.fontSizeDown, this.fontSizeValue, this.fontSizeUp);
    terminal.append(this.row("Font size", stepper));

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
    terminal.append(this.row("Font family", this.familySelect));

    this.lineHeightPicker = new Segmented<number>({
      label: "Line height",
      options: LINE_HEIGHTS.map((h) => ({ value: h, label: h.toFixed(1) })),
      value: 1.2,
      onChange: (h) => void this.store.update({ lineHeight: h }),
    });
    this.lineHeightPicker.element.dataset.setting = "lineHeight";
    terminal.append(this.row("Line height", this.lineHeightPicker.element));

    this.marksToggle = this.toggle("syntheticPromptMarks");
    terminal.append(this.row("Synthetic prompt marks", this.marksToggle));
    terminal.append(
      this.hint(
        "Guesses: every plain Enter outside a full-screen app plants a prompt mark, " +
          "including an Enter that only cancels a dialog or answers a y/n. Off by default; " +
          "turn it on for agent CLIs that emit no real OSC 133 marks. Full-screen (alt-" +
          "screen) apps plant nothing — Claude Code needs " +
          "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 to stay in normal scrollback.",
      ),
    );

    // --- Shell profiles ----------------------------------------------------
    const shells = this.section("SHELL PROFILES");
    this.profileList = document.createElement("div");
    this.profileList.className = "chappa-profile-list";
    shells.append(this.profileList);

    this.addName = document.createElement("input");
    this.addName.type = "text";
    this.addName.placeholder = "Name";
    this.addName.spellcheck = false;
    this.addName.style.cssText = `${INPUT_CSS}flex:0 1 130px;`;
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
    shells.append(addRow);

    this.execSelect = document.createElement("select");
    this.execSelect.style.cssText = SELECT_CSS;
    this.execSelect.dataset.setting = "defaultExecProfile";
    this.execSelect.addEventListener("change", () => {
      void this.store.update({ defaultExecProfile: this.execSelect.value });
    });
    shells.append(this.row("Default execution profile", this.execSelect));
    shells.append(this.hint("The shell that runs chappa.yml command lines."));

    // --- Agents --------------------------------------------------
    const agentsSection = this.section("AGENTS");
    agentsSection.textContent = ""; // the section owns its own header line
    this.agents = new AgentsSection({
      store: opts.agentTools ?? agentToolsStore,
      settings: this.store,
      modalContainer: opts.container,
    });
    agentsSection.appendChild(this.agents.element);

    const column = document.createElement("div");
    column.className = "chappa-settings-column";
    column.style.cssText = COLUMN_CSS;
    column.append(header, terminal, shells, agentsSection);
    this.wrap.appendChild(column);
    (opts.container ?? document.body).appendChild(this.wrap);

    this.sync(this.store.get());
  }

  /** The pane's root element (tests drive the controls through it). */
  get element(): HTMLDivElement {
    return this.wrap;
  }

  isOpen(): boolean {
    return this.opened;
  }

  open(): void {
    if (this.opened) return;
    this.opened = true;
    this.sync(this.store.get());
    this.unsubscribe = this.store.subscribe((s) => this.sync(s));
    this.agents.activate();
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

  private section(label: string): HTMLDivElement {
    const el = document.createElement("div");
    el.className = "chappa-settings-section";
    el.style.cssText = SECTION_CSS;
    const head = document.createElement("div");
    head.style.cssText = SECTION_LABEL_CSS;
    head.textContent = label;
    el.appendChild(head);
    return el;
  }

  /** One label/control line. The label is a `<label for=…>` (never a wrapping
   *  `<label>`: a wrapper forwards its click to the first labelable descendant,
   *  which for the segmented pickers and the font-size stepper would silently
   *  press their first BUTTON — clicking the words "Scroll wheel speed" would
   *  set 1x). */
  private row(label: string, control: HTMLElement): HTMLDivElement {
    const el = document.createElement("div");
    el.style.cssText = ROW_CSS;
    const text = document.createElement("label");
    text.style.cssText = ROW_LABEL_CSS;
    text.textContent = label;
    if (!control.id) control.id = `chappa-set-${++rowIds}`;
    text.htmlFor = control.id;
    el.append(text, control);
    return el;
  }

  private hint(text: string): HTMLDivElement {
    const el = document.createElement("div");
    el.style.cssText = HINT_CSS;
    el.textContent = text;
    return el;
  }

  private toggle(setting: "copyOnSelect" | "ctrlVPastes" | "ctrlCCopyOnly" | "syntheticPromptMarks"): HTMLInputElement {
    const input = document.createElement("input");
    input.type = "checkbox";
    input.dataset.setting = setting;
    input.style.cssText = "flex:none;accent-color:#5ea6ff;";
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
      toggle.style.cssText = "flex:none;accent-color:#5ea6ff;";
      toggle.dataset.field = "enabled";
      toggle.addEventListener("change", () => {
        void this.patchProfile(index, { enabled: toggle.checked });
      });

      const name = document.createElement("input");
      name.type = "text";
      name.value = p.name;
      name.spellcheck = false;
      name.style.cssText = `${INPUT_CSS}flex:0 1 130px;`;
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
    // notification tore down and rebuilt the select (review finding).
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
