// The Edit-project pane. A full-window overlay exactly like the
// settings pane (same open/close chrome, × top-right, Escape closes),
// giving every project a real settings home: one "Edit project…" pane in
// the house style, instead of settings scattered across the rail.
//
// Unlike the settings pane (which controls the app-wide SettingsStore), this
// pane edits ONE project and reads its LIVE state through callbacks the app
// provides: the app owns `projects`/`openProjects` and the api, so it hands
// the pane a snapshot getter plus the mutation callbacks. The pane stores its
// own target project id (a brand-new project's section is reachable only
// after opening it, so the context menu can target a NOT-YET-expanded
// project); `refresh()` re-reads the snapshot so live values repaint while
// the pane is open (task: "reuse the repaint hooks the rail rows already
// use" — the app calls refresh() from processRowChanged/refreshProjectHeader).
//
// No Save button: every control change writes through the app immediately
// (the same no-Save convention as the settings pane) and refresh() re-syncs from the
// canonical value, so a rejected write visibly snaps back.
//
// Dismiss is × or Escape only, matching the settings pane — a full-window
// view has no outside to click.

import { Segmented } from "./segmented";
import { LEVELS, LEVEL_LABELS, resolveLevel, type Level } from "./notifications";

const PANE_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:60;display:none;" +
  "flex-direction:column;overflow-y:auto;background:#0d0e10;" +
  "color:#d6d8dc;font:13px system-ui,sans-serif;";
const COLUMN_CSS = "width:100%;padding:24px 32px 64px;box-sizing:border-box;";
const HEADER_CSS =
  "display:flex;align-items:center;gap:8px;padding:0 0 14px;border-bottom:1px solid #1c1f24;";
const TITLE_CSS = "flex:1 1 auto;font:600 18px system-ui,sans-serif;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";
const CLOSE_CSS =
  "flex:none;border:1px solid #262a31;background:#16181d;color:#8b949e;font-size:18px;" +
  "line-height:1;cursor:pointer;padding:2px 9px;border-radius:6px;";
const SECTION_CSS = "padding:16px 0 8px;";
const SECTION_LABEL_CSS =
  "font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;padding-bottom:6px;";
/** The OVERVIEW card: bordered box holding the path / chappa.yml / commands
 *  rows (read-only facts about the project). */
const CARD_CSS =
  "border:1px solid #1c1f24;border-radius:8px;background:#0f1114;padding:4px 12px;";
const OVERVIEW_ROW_CSS = "display:flex;align-items:center;gap:10px;padding:7px 0;";
const OVERVIEW_ROW_BORDER = "border-bottom:1px solid #1c1f24;";
const OVERVIEW_LABEL_CSS =
  "flex:0 0 90px;color:#8b949e;font:11px ui-monospace,monospace;letter-spacing:.04em;";
const OVERVIEW_VALUE_CSS =
  "flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:12px ui-monospace,monospace;color:#d6d8dc;";
/** Settings row: bold title + dimmed description in a stable left stack, the
 *  control right-aligned (the flex-wrap lets a wide control drop to its own
 *  line rather than covering the words, the label-overlap fix). */
const SETROW_CSS =
  "display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:8px 14px;" +
  "padding:9px 0;min-height:30px;border-top:1px solid #1c1f24;";
const SETTEXT_CSS = "flex:0 0 260px;min-width:0;";
const SETTITLE_CSS = "font:600 13px system-ui,sans-serif;color:#d6d8dc;";
const SETDESC_CSS = "font:11px system-ui,sans-serif;color:#8b949e;line-height:1.4;";
const CONTROL_CSS = "flex:none;display:flex;align-items:center;gap:8px;";
const INPUT_CSS =
  "background:#0d0e10;color:#d6d8dc;border:1px solid #2a2c31;border-radius:4px;padding:3px 6px;" +
  "font:12px ui-monospace,monospace;outline:none;min-width:0;";
const SMALL_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:11px system-ui,sans-serif;cursor:pointer;padding:3px 8px;";

/** A live snapshot of the project being edited, as the pane should read it.
 *  The app derives these straight from its `projects`/`openProjects` state. */
export interface ProjectEditInfo {
  name: string;
  path: string;
  /** RAW project-default notification level (a plain string; resolves
   *  through `resolveLevel`), mirroring `ProjectInfoDto.notificationLevel`. */
  notificationLevel: string | null;
  /** Why the last chappa.yml reload failed, or null while it parses — the
   *  Overview project-file row's Valid/error chip. */
  ymlError: string | null;
  running: number;
  total: number;
}

export interface EditPaneOptions {
  /** Snapshot for the pane's target project, or null if it is gone. */
  info: (id: number) => ProjectEditInfo | null;
  /** Apply a rename (Enter/blur commit). The app runs the same rename flow
   *  and validation as the ✎; rejects/throws on a failed write. */
  rename: (id: number, name: string) => Promise<void>;
  /** Persist a project-default notification LEVEL (the pane passes a Level;
   *  the app maps "all" → a null clear, its existing contract). */
  setLevel: (id: number, level: Level) => void | Promise<void>;
  /** Where the pane attaches. Defaults to document.body. */
  container?: HTMLElement;
  /** Fired after a close so the caller can hand focus back. */
  onClose?: () => void;
}

export class EditPane {
  private readonly opts: EditPaneOptions;
  private readonly wrap: HTMLDivElement;
  private readonly title: HTMLSpanElement;

  // Overview rows (read-only facts, repainted on refresh()).
  private readonly pathValue: HTMLSpanElement;
  private readonly ymlRow: HTMLDivElement;
  private readonly ymlChip: HTMLSpanElement;
  private readonly commandsValue: HTMLSpanElement;

  // Settings controls.
  private readonly nameInput: HTMLInputElement;
  private readonly levelPicker: Segmented<Level>;

  private id = -1;
  private opened = false;

  constructor(opts: EditPaneOptions) {
    this.opts = opts;
    this.wrap = document.createElement("div");
    this.wrap.className = "chappa-edit-pane";
    this.wrap.style.cssText = PANE_CSS;
    this.wrap.setAttribute("role", "dialog");
    this.wrap.setAttribute("aria-label", "Edit project");

    const header = document.createElement("div");
    header.style.cssText = HEADER_CSS;
    this.title = document.createElement("span");
    this.title.style.cssText = TITLE_CSS;
    const close = document.createElement("button");
    close.type = "button";
    close.className = "chappa-edit-close";
    close.textContent = "×";
    close.title = "Close edit-project (Esc)";
    close.style.cssText = CLOSE_CSS;
    close.addEventListener("click", () => this.close());
    header.append(this.title, close);

    // --- Overview card --------------------------------------------------------
    const overview = document.createElement("div");
    overview.className = "chappa-edit-overview";
    overview.style.cssText = CARD_CSS;

    // Path row: read-only value + copy button. No open-in-file-manager button:
    // there is no such ipc command ("do NOT add Rust for it, omit
    // otherwise"). The copy reuses the navigator.clipboard fallback the
    // command-row "Copy command" uses.
    this.pathValue = document.createElement("span");
    this.pathValue.style.cssText = OVERVIEW_VALUE_CSS;
    this.pathValue.dataset.value = "path";
    const copyPath = document.createElement("button");
    copyPath.type = "button";
    copyPath.className = "chappa-edit-copy-path";
    copyPath.textContent = "Copy path";
    copyPath.style.cssText = SMALL_BTN_CSS;
    copyPath.addEventListener("click", () => {
      try {
        void navigator.clipboard.writeText(this.pathValue.textContent ?? "");
      } catch {
        /* clipboard unavailable — non-fatal */
      }
    });
    overview.append(this.overviewRow("PATH", this.pathValue, copyPath, true));

    // chappa.yml row: the project file, always present in the Overview; a
    // Valid/error chip from the project's current load state, nothing
    // clickable. The project file is unconditional (no sync toggle).
    this.ymlRow = document.createElement("div");
    this.ymlRow.style.cssText = `${OVERVIEW_ROW_CSS}${OVERVIEW_ROW_BORDER}`;
    const ymlLabel = document.createElement("span");
    ymlLabel.style.cssText = OVERVIEW_LABEL_CSS;
    ymlLabel.textContent = "CHAPPA.YML";
    const ymlValue = document.createElement("span");
    ymlValue.style.cssText = OVERVIEW_VALUE_CSS;
    ymlValue.textContent = "chappa.yml";
    this.ymlChip = document.createElement("span");
    this.ymlChip.style.cssText =
      "flex:none;font:10px ui-monospace,monospace;letter-spacing:.05em;border-radius:4px;padding:1px 6px;";
    this.ymlRow.append(ymlLabel, ymlValue, this.ymlChip);

    // Commands row: live `N Running · M Total`.
    this.commandsValue = document.createElement("span");
    this.commandsValue.style.cssText = OVERVIEW_VALUE_CSS;
    this.commandsValue.dataset.value = "counts";
    overview.append(this.ymlRow, this.overviewRow("COMMANDS", this.commandsValue, null, false));

    // --- SETTINGS --------------------------------------------------------------
    const settings = document.createElement("div");
    settings.style.cssText = SECTION_CSS;
    const settingsLabel = document.createElement("div");
    settingsLabel.style.cssText = SECTION_LABEL_CSS;
    settingsLabel.textContent = "SETTINGS";
    settings.append(settingsLabel);

    // 1. Name — a text field committed on Enter/blur (the `change` event),
    //    through the same rename flow + validation (blank is a rejection,
    //    unchanged is a no-op), mirroring the settings profile-name editor.
    this.nameInput = document.createElement("input");
    this.nameInput.type = "text";
    this.nameInput.spellcheck = false;
    this.nameInput.style.cssText = `${INPUT_CSS}width:220px;`;
    this.nameInput.dataset.field = "name";
    this.nameInput.addEventListener("change", () => this.commitName());
    settings.append(this.settingRow("Name", "The project's display name (chappa.yml is not rewritten).", this.nameInput));

    // 2. Notification level — the All | Important | None segmented picker,
    //    relocated from the rail row (the per-process overrides in the
    //    command context menu are untouched).
    this.levelPicker = new Segmented<Level>({
      label: "Notification level",
      options: LEVELS.map((v) => ({ value: v, label: LEVEL_LABELS[v] })),
      value: "all",
      onChange: (level) => void this.commitLevel(level),
    });
    this.levelPicker.element.dataset.setting = "notificationLevel";
    settings.append(
      this.settingRow(
        "Notification level",
        "All notifications including terminal alerts.",
        this.levelPicker.element,
      ),
    );

    const column = document.createElement("div");
    column.className = "chappa-edit-column";
    column.style.cssText = COLUMN_CSS;
    column.append(header, overview, settings);
    this.wrap.appendChild(column);
    (opts.container ?? document.body).appendChild(this.wrap);
  }

  /** The pane's root element (tests drive the controls through it). */
  get element(): HTMLDivElement {
    return this.wrap;
  }

  isOpen(): boolean {
    return this.opened;
  }

  open(id: number): void {
    this.id = id;
    if (!this.sync()) return; // the project is gone — stay closed
    this.opened = true;
    this.wrap.style.display = "flex";
    document.addEventListener("keydown", this.escape, true);
  }

  close(): void {
    if (!this.opened) return;
    this.opened = false;
    this.wrap.style.display = "none";
    document.removeEventListener("keydown", this.escape, true);
    this.opts.onClose?.();
  }

  /** Re-read the live project snapshot and repaint every value. Called by the
   *  app on its rail-repaint hooks while the pane is open. */
  refresh(): void {
    if (!this.opened) return;
    this.sync();
  }

  dispose(): void {
    this.close();
    this.levelPicker.dispose();
    this.wrap.remove();
  }

  /** Push the current snapshot into every control. Returns false (and closes
   *  the pane) when the project no longer exists. */
  private sync(): boolean {
    const info = this.opts.info(this.id);
    if (!info) {
      this.close();
      return false;
    }
    this.title.textContent = info.name;
    if (document.activeElement !== this.nameInput) this.nameInput.value = info.name;
    this.pathValue.textContent = info.path;
    this.commandsValue.textContent = `${info.running} Running · ${info.total} Total`;
    // The project file row is always present; the chip is Valid (load state)
    // or the reason the last chappa.yml reload failed.
    const broken = info.ymlError !== null;
    this.ymlChip.textContent = broken ? "Error" : "Valid";
    this.ymlChip.style.color = broken ? "#f85149" : "#3fb950";
    this.ymlChip.style.border = `1px solid ${broken ? "#5c2f2f" : "#1f5a36"}`;
    this.ymlChip.title = info.ymlError ?? "";
    this.levelPicker.setValue(resolveLevel(null, info.notificationLevel));
    return true;
  }

  private readonly escape = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    e.preventDefault();
    e.stopPropagation();
    this.close();
  };

  private commitName(): void {
    const typed = this.nameInput.value.trim();
    const info = this.opts.info(this.id);
    if (!info) return;
    if (typed === "" || typed === info.name) {
      // Blank is a rejection (Rust refuses it too); unchanged is a no-op.
      this.nameInput.value = info.name;
      return;
    }
    this.opts.rename(this.id, typed).catch(() => {
      const cur = this.opts.info(this.id);
      if (cur) this.nameInput.value = cur.name;
    });
  }

  private async commitLevel(level: Level): Promise<void> {
    const info = this.opts.info(this.id);
    if (!info) return;
    await Promise.resolve(this.opts.setLevel(this.id, level)).catch(() => {});
  }

  private overviewRow(
    label: string,
    value: HTMLElement,
    control: HTMLElement | null,
    top: boolean,
  ): HTMLDivElement {
    const el = document.createElement("div");
    el.style.cssText = top
      ? `${OVERVIEW_ROW_CSS}${OVERVIEW_ROW_BORDER}`
      : OVERVIEW_ROW_CSS;
    const text = document.createElement("span");
    text.style.cssText = OVERVIEW_LABEL_CSS;
    text.textContent = label;
    el.append(text, value);
    if (control) el.append(control);
    return el;
  }

  /** One settings row: bold title + dimmed description stack, control
   *  right-aligned. */
  private settingRow(title: string, desc: string, control: HTMLElement): HTMLDivElement {
    const el = document.createElement("div");
    el.className = "chappa-edit-setting-row";
    el.style.cssText = SETROW_CSS;
    const text = document.createElement("div");
    text.style.cssText = SETTEXT_CSS;
    text.dataset.role = "text";
    const titleEl = document.createElement("div");
    titleEl.style.cssText = SETTITLE_CSS;
    titleEl.textContent = title;
    const descEl = document.createElement("div");
    descEl.style.cssText = SETDESC_CSS;
    descEl.textContent = desc;
    text.append(titleEl, descEl);
    const controlWrap = document.createElement("div");
    controlWrap.style.cssText = CONTROL_CSS;
    controlWrap.append(control);
    el.append(text, controlWrap);
    return el;
  }
}
