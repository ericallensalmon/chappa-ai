// In-app modal dialogs. Replaces `window.confirm` / `window.prompt`
// at the three App seams: closing a running terminal, the project trust gate,
// and "Add project…".
//
// Why: a webview's native dialog is chrome we do not own — it renders the
// origin ("localhost:5173 says"), it ignores the app palette, and on the Tauri
// build it is a platform box bolted onto a frameless window. the
// host verification named it exactly: "we still get the ugly message".
//
// House conventions, same as search.ts / settings_pane.ts: module-level CSS
// string consts applied via `style.cssText`, the class owns its DOM, and every
// listener it installs comes back off when it resolves. Each call builds a
// fresh overlay and tears it down on resolve — there is no retained instance,
// so a dialog can never leak a stale listener into the next one.
//
// Keyboard contract: Enter = OK/submit, Escape = cancel, backdrop press =
// cancel. The confirm focuses its OK button, the prompt focuses (and selects)
// its input, so the very first keystroke lands where it should.

const BACKDROP_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:200;display:flex;align-items:center;" +
  "justify-content:center;padding:24px;box-sizing:border-box;background:rgba(0,0,0,.55);";
const BOX_CSS =
  "min-width:320px;max-width:560px;width:100%;max-height:80vh;overflow-y:auto;box-sizing:border-box;" +
  "background:#1b1d21;border:1px solid #2a2c31;border-radius:8px;" +
  "box-shadow:0 8px 32px rgba(0,0,0,.6);padding:18px 20px 14px;color:#d6d8dc;" +
  "font:13px system-ui,sans-serif;";
// `pre-wrap`: the trust gate passes a multi-line command list and every line
// break in it is meaningful. Monospace, because those lines are command lines.
const MESSAGE_CSS =
  "margin:0 0 14px;white-space:pre-wrap;line-height:1.5;font:13px ui-monospace,monospace;" +
  "color:#d6d8dc;word-break:break-word;";
const INPUT_CSS =
  "width:100%;box-sizing:border-box;margin:0 0 14px;background:#0d0e10;color:#d6d8dc;" +
  "border:1px solid #2a2c31;border-radius:4px;padding:5px 8px;font:13px ui-monospace,monospace;" +
  "outline:none;";
const BUTTON_ROW_CSS = "display:flex;justify-content:flex-end;gap:8px;";
// Form rows (the add-project dialog). Same palette/type as the prompt;
// the label is the only new element type.
const FIELD_LABEL_CSS =
  "display:block;margin:0 0 4px;font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;";
const FIELD_ROW_CSS = "display:flex;align-items:center;gap:8px;margin:0 0 14px;";
const BROWSE_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:6px;background:#16181d;color:#8b949e;" +
  "font:13px system-ui,sans-serif;cursor:pointer;padding:5px 12px;";
const CANCEL_BTN_CSS =
  "border:1px solid #262a31;border-radius:6px;background:#16181d;color:#8b949e;" +
  "font:13px system-ui,sans-serif;cursor:pointer;padding:5px 14px;";
const OK_BTN_CSS =
  "border:1px solid #2f5c8f;border-radius:6px;background:#1d232b;color:#5ea6ff;" +
  "font:13px system-ui,sans-serif;cursor:pointer;padding:5px 14px;";

export interface ConfirmDialogOptions {
  /** Label on the affirmative button. Defaults to "OK". */
  okLabel?: string;
  /** Where the overlay attaches. Defaults to document.body. */
  container?: HTMLElement;
}

export interface PromptDialogOptions {
  /** Placeholder text for the input. */
  placeholder?: string;
  /** Pre-filled value. */
  value?: string;
  /** Label on the affirmative button. Defaults to "OK". */
  okLabel?: string;
  /** Where the overlay attaches. Defaults to document.body. */
  container?: HTMLElement;
}

/** The pieces every dialog shares: backdrop, box, message, button row. */
interface Shell {
  wrap: HTMLDivElement;
  box: HTMLDivElement;
  buttons: HTMLDivElement;
  ok: HTMLButtonElement;
  cancel: HTMLButtonElement;
}

function buildShell(message: string, okLabel: string): Shell {
  const wrap = document.createElement("div");
  wrap.className = "chappa-dialog-backdrop";
  wrap.style.cssText = BACKDROP_CSS;

  const box = document.createElement("div");
  box.className = "chappa-dialog";
  box.style.cssText = BOX_CSS;
  box.setAttribute("role", "dialog");
  box.setAttribute("aria-modal", "true");

  const text = document.createElement("div");
  text.className = "chappa-dialog-message";
  text.style.cssText = MESSAGE_CSS;
  text.textContent = message;

  const buttons = document.createElement("div");
  buttons.style.cssText = BUTTON_ROW_CSS;
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "chappa-dialog-cancel";
  cancel.textContent = "Cancel";
  cancel.style.cssText = CANCEL_BTN_CSS;
  const ok = document.createElement("button");
  ok.type = "button";
  ok.className = "chappa-dialog-ok";
  ok.textContent = okLabel;
  ok.style.cssText = OK_BTN_CSS;
  buttons.append(cancel, ok);

  box.append(text, buttons);
  wrap.appendChild(box);
  return { wrap, box, buttons, ok, cancel };
}

/**
 * Mount `shell`, wire the shared dismissal (OK/Cancel buttons, Enter/Escape,
 * backdrop press) and hand back a `settle` that resolves exactly once, removes
 * the overlay and every listener, and restores the previously focused element.
 */
function mount<T>(
  shell: Shell,
  container: HTMLElement,
  resolve: (value: T) => void,
  onEnter: () => T,
  cancelled: T,
  /** Gate on the affirmative action only. `false` makes OK/Enter a
   *  no-op — the dialog STAYS open. Escape and the backdrop still cancel: a
   *  dialog you cannot leave would be worse than the empty value it refuses. */
  canSubmit: () => boolean = () => true,
): void {
  const previous = document.activeElement as HTMLElement | null;
  let done = false;

  const settle = (value: T): void => {
    if (done) return;
    done = true;
    document.removeEventListener("keydown", onKeydown, true);
    shell.wrap.remove();
    if (previous && previous.isConnected) previous.focus();
    resolve(value);
  };

  function onKeydown(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      settle(cancelled);
    } else if (e.key === "Enter" && e.target instanceof HTMLTextAreaElement) {
      // The command editor's glob list is a textarea — Enter there
      // is a newline, not a submit (the OK button still submits).
      return;
    } else if (e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      if (canSubmit()) settle(onEnter());
    }
  }

  shell.ok.addEventListener("click", () => {
    if (canSubmit()) settle(onEnter());
  });
  shell.cancel.addEventListener("click", () => settle(cancelled));
  // Backdrop press only — a press that starts on the box (a drag across the
  // message text that ends outside) must not cancel.
  shell.wrap.addEventListener("mousedown", (e) => {
    if (e.target === shell.wrap) settle(cancelled);
  });
  // Capture phase, on the document: Escape must reach this before the settings
  // pane's own capture-phase Escape handler closes the view under the dialog.
  document.addEventListener("keydown", onKeydown, true);
  container.appendChild(shell.wrap);
}

/**
 * Modal yes/no. Resolves true on OK (click or Enter), false on Cancel,
 * Escape or a backdrop press. `message` renders with its line breaks intact.
 */
export function confirmDialog(
  message: string,
  opts: ConfirmDialogOptions = {},
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    const shell = buildShell(message, opts.okLabel ?? "OK");
    mount<boolean>(shell, opts.container ?? document.body, resolve, () => true, false);
    shell.ok.focus();
  });
}

/**
 * Modal single-line prompt. Resolves the typed string on OK/Enter (verbatim,
 * exactly like `window.prompt` — the caller decides what an empty string
 * means) and null on Cancel, Escape or a backdrop press.
 */
export function promptDialog(
  message: string,
  opts: PromptDialogOptions = {},
): Promise<string | null> {
  return new Promise<string | null>((resolve) => {
    const shell = buildShell(message, opts.okLabel ?? "OK");
    const input = document.createElement("input");
    input.type = "text";
    input.className = "chappa-dialog-input";
    input.spellcheck = false;
    input.style.cssText = INPUT_CSS;
    if (opts.placeholder) input.placeholder = opts.placeholder;
    input.value = opts.value ?? "";
    shell.box.insertBefore(input, shell.buttons);
    mount<string | null>(
      shell,
      opts.container ?? document.body,
      resolve,
      () => input.value,
      null,
    );
    input.focus();
    input.select();
  });
}

// --- add-project form dialog --------------------------------------

/** The one split discipline for both path helpers: trim, strip trailing
 *  separators, find the LAST separator of either flavor. */
function splitPath(path: string): { trimmed: string; cut: number } {
  const trimmed = path.trim().replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return { trimmed, cut };
}

/**
 * The trailing path segment of `path`, for both separators — the DEFAULT
 * project name. Trailing separators are stripped first so `C:\code\chappa-ai\` and
 * `C:\code\chappa-ai` name the same project; a path that is nothing but separators
 * (or a bare drive root) has no basename to offer and answers "".
 */
export function folderBaseName(path: string): string {
  const { trimmed, cut } = splitPath(path);
  const base = cut >= 0 ? trimmed.slice(cut + 1) : trimmed;
  // A bare Windows drive root ("C:") is a location, not a project name.
  return /^[A-Za-z]:$/.test(base) ? "" : base;
}

/**
 * The parent directory of `path`, or null when there is none to name — the
 * other half of {@link folderBaseName}'s split discipline (App's remembered
 * browse-dir uses it).
 *
 * The boundary regime is where the naive `slice(0, cut)` went wrong:
 * - `C:\chappa-ai` → `C:\`, WITH the separator. The bare `C:` is a drive-RELATIVE
 *   path (it resolves against that drive's per-process CWD), so a picker
 *   seeded with it opens somewhere arbitrary.
 * - `/chappa-ai` → `/` (decided): the filesystem root IS the parent, and unlike
 *   `C:` it is already absolute.
 * - a relative name (`chappa-ai`), a bare drive (`C:`) or a bare root (`/`) has no
 *   parent to name → null.
 */
export function parentDirectory(path: string): string | null {
  const { trimmed, cut } = splitPath(path);
  if (cut < 0) return null; // relative name, bare drive, or a root trimmed to ""
  const sep = trimmed[cut];
  const parent = trimmed.slice(0, cut);
  if (parent === "") return sep; // "/chappa-ai" → "/"
  if (/^[A-Za-z]:$/.test(parent)) return parent + sep; // "C:\chappa-ai" → "C:\"
  return parent;
}

export interface AddProjectDialogOptions {
  /**
   * The native folder picker, or null for none. "The picker must
   * never be reachable outside Tauri (inTauri guard — the plain-browser dev
   * page falls back to the typed-path field only)" — so a null `browse` omits
   * the Browse… button entirely rather than rendering a dead one.
   */
  browse?: (() => Promise<string | null>) | null;
  /** Where the overlay attaches. Defaults to document.body. */
  container?: HTMLElement;
}

/** What the add-project modal answers on Add. `path` is non-empty. `name` is
 *  the user's EXPLICIT choice (non-empty) — or null when the field was never
 *  edited: the auto-filled value was only a visual preview, and Rust owns the
 *  default (the project file's `name:`, then the folder basename).
 */
export interface AddProjectAnswer {
  path: string;
  name: string | null;
}

/**
 * The "＋ Add project…" modal: a path field (+ optional Browse…) and a name
 * field. Resolves `{path, name}` on Add and null on Cancel/Escape/backdrop.
 *
 * The name field AUTO-FILLS from the path's folder name and keeps tracking it
 * — until the user types in it. That is the exact complaint: "a NAME
 * field that auto-fills with the folder name once a path is present but stays
 * user-editable (this is the 'uses the folder name' complaint — the default is
 * fine, the inability to change it is not)". Once the user edits the name it is
 * DIRTY and a later path change never overwrites it again.
 *
 * Add is refused (the dialog stays open) while either field is blank.
 */
export function addProjectDialog(
  opts: AddProjectDialogOptions = {},
): Promise<AddProjectAnswer | null> {
  return new Promise<AddProjectAnswer | null>((resolve) => {
    const shell = buildShell("Add project", "Add");

    const pathLabel = document.createElement("label");
    pathLabel.style.cssText = FIELD_LABEL_CSS;
    pathLabel.textContent = "PROJECT DIRECTORY";
    const pathRow = document.createElement("div");
    pathRow.style.cssText = FIELD_ROW_CSS;
    const pathInput = document.createElement("input");
    pathInput.type = "text";
    pathInput.className = "chappa-dialog-path";
    pathInput.spellcheck = false;
    pathInput.placeholder = "C:\\path\\to\\project";
    // The shared INPUT_CSS carries the prompt's bottom margin; the row owns
    // spacing here, so the field itself must not add a second gap.
    pathInput.style.cssText = INPUT_CSS + "margin:0;";
    pathRow.appendChild(pathInput);

    const nameLabel = document.createElement("label");
    nameLabel.style.cssText = FIELD_LABEL_CSS;
    nameLabel.textContent = "NAME";
    const nameInput = document.createElement("input");
    nameInput.type = "text";
    nameInput.className = "chappa-dialog-name";
    nameInput.spellcheck = false;
    nameInput.placeholder = "folder name";
    nameInput.style.cssText = INPUT_CSS;

    /** True once the user typed in the name field: the path stops driving it.
     *  Until then the auto-filled name is a PREVIEW only — the answer carries
     *  `name: null` and Rust owns the default, which may differ (a project
     *  file `name:` beats the folder basename shown here). */
    let nameDirty = false;
    const syncName = (): void => {
      if (nameDirty) return;
      nameInput.value = folderBaseName(pathInput.value);
    };
    pathInput.addEventListener("input", syncName);
    nameInput.addEventListener("input", () => {
      nameDirty = true;
    });

    const answer = (): AddProjectAnswer => ({
      path: pathInput.value.trim(),
      // Untouched field = no explicit choice: null lets the Rust fallback
      // (project file name → folder basename) run instead of shadowing it
      // with the preview text.
      name: nameDirty ? nameInput.value.trim() : null,
    });
    // The gate reads the VISIBLE values (preview included): a dialog whose
    // name box shows text but whose Add stays refused would be inexplicable.
    const filled = (): boolean =>
      pathInput.value.trim() !== "" && nameInput.value.trim() !== "";
    // Disabled is the VISIBLE half of the refusal; `canSubmit` is the real
    // gate (Enter never reaches a disabled button's click handler).
    const refreshOk = (): void => {
      shell.ok.disabled = !filled();
      shell.ok.style.opacity = shell.ok.disabled ? "0.45" : "1";
    };
    // Registered AFTER syncName so the name is already synced when validity is
    // re-read.
    pathInput.addEventListener("input", refreshOk);
    nameInput.addEventListener("input", refreshOk);
    refreshOk();

    if (opts.browse) {
      const browse = opts.browse;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chappa-dialog-browse";
      btn.textContent = "Browse…";
      btn.style.cssText = BROWSE_BTN_CSS;
      btn.addEventListener("click", () => {
        void browse()
          .then((picked) => {
            // null = the user cancelled the picker (or there is no picker: the
            // ipc wrapper answers null outside Tauri). Leave the typed path.
            if (picked === null || picked === "") return;
            pathInput.value = picked;
            // A programmatic value assignment fires no `input` event, so the
            // listeners it would have run are called by hand — name sync and
            // validity.
            syncName();
            refreshOk();
            pathInput.focus();
          })
          .catch(() => {
            // A rejected pick_directory invoke (plugin missing, transient
            // IPC failure) resolves like a cancelled pick: the typed path
            // stays, the dialog stays, and nothing surfaces as an unhandled
            // rejection.
          });
      });
      pathRow.appendChild(btn);
    }

    shell.box.insertBefore(pathLabel, shell.buttons);
    shell.box.insertBefore(pathRow, shell.buttons);
    shell.box.insertBefore(nameLabel, shell.buttons);
    shell.box.insertBefore(nameInput, shell.buttons);

    mount<AddProjectAnswer | null>(
      shell,
      opts.container ?? document.body,
      resolve,
      answer,
      null,
      filled,
    );
    pathInput.focus();
    pathInput.select();
  });
}

// --- command editor dialog ----------------------------------------

/** The `Edit command…` / `+ Add command` form. Field-for-field the chappa.yml
 *  process definition; `env` is an ORDERED list of rows (the file keeps that
 *  order), `restartWhenChanged` one glob per entry. */
export interface CommandForm {
  name: string;
  command: string;
  /** "" = project root. */
  workingDir: string;
  autoStart: boolean;
  autoRestart: boolean;
  restartWhenChanged: string[];
  env: Array<[string, string]>;
}

export interface CommandDialogOptions {
  /** Dialog heading. Defaults to "Edit command" when `initial` is set, else
   *  "Add command". */
  title?: string;
  /** Placeholder for the WORKING DIRECTORY field. Defaults to the project rule
   *  ("project root (must stay inside it)"); the workspace-command editor
   * overrides it to "absolute path (or empty for home)". */
  workdirHint?: string;
  /** Where the overlay attaches. Defaults to document.body. */
  container?: HTMLElement;
}

const CHECK_ROW_CSS =
  "display:flex;align-items:center;gap:8px;margin:0 0 10px;font:13px system-ui,sans-serif;color:#d6d8dc;";
const TEXTAREA_CSS = INPUT_CSS + "min-height:52px;resize:vertical;";
const ENV_ROW_CSS = "display:flex;align-items:center;gap:6px;margin:0 0 6px;";
const SMALL_BTN_CSS =
  "flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;" +
  "font:12px system-ui,sans-serif;cursor:pointer;padding:3px 8px;";

/**
 * The command editor. Resolves the (trimmed) form on Save/Enter and null on
 * Cancel/Escape/backdrop. Save is refused while NAME or COMMAND is blank —
 * everything else is optional. Env rows with a blank KEY are dropped on
 * answer (a half-added row must not fail the save); the Rust side revalidates
 * all of it (name clash, working_dir containment).
 */
export function commandDialog(
  initial: CommandForm | null,
  opts: CommandDialogOptions = {},
): Promise<CommandForm | null> {
  return new Promise<CommandForm | null>((resolve) => {
    const shell = buildShell(opts.title ?? (initial ? "Edit command" : "Add command"), "Save");
    const field = (label: string, cls: string, value: string, placeholder = ""): HTMLInputElement => {
      const lab = document.createElement("label");
      lab.style.cssText = FIELD_LABEL_CSS;
      lab.textContent = label;
      const input = document.createElement("input");
      input.type = "text";
      input.className = cls;
      input.spellcheck = false;
      input.placeholder = placeholder;
      input.value = value;
      input.style.cssText = INPUT_CSS;
      shell.box.insertBefore(lab, shell.buttons);
      shell.box.insertBefore(input, shell.buttons);
      return input;
    };
    const check = (label: string, cls: string, value: boolean): HTMLInputElement => {
      const row = document.createElement("label");
      row.style.cssText = CHECK_ROW_CSS;
      const box = document.createElement("input");
      box.type = "checkbox";
      box.className = cls;
      box.checked = value;
      row.append(box, label);
      shell.box.insertBefore(row, shell.buttons);
      return box;
    };

    const nameInput = field("NAME", "chappa-cmd-name", initial?.name ?? "", "e.g. dev server");
    const commandInput = field("COMMAND", "chappa-cmd-command", initial?.command ?? "", "npm run dev");
    const workdirInput = field(
      "WORKING DIRECTORY",
      "chappa-cmd-workdir",
      initial?.workingDir ?? "",
      opts.workdirHint ?? "project root (must stay inside it)",
    );
    const autoStart = check("auto_start — run when the project opens", "chappa-cmd-autostart", initial?.autoStart ?? true);
    const autoRestart = check("auto_restart — respawn on exit", "chappa-cmd-autorestart", initial?.autoRestart ?? false);

    const globLabel = document.createElement("label");
    globLabel.style.cssText = FIELD_LABEL_CSS;
    globLabel.textContent = "RESTART WHEN CHANGED (one glob per line)";
    const globs = document.createElement("textarea");
    globs.className = "chappa-cmd-globs";
    globs.spellcheck = false;
    globs.placeholder = "src/**/*.py";
    globs.style.cssText = TEXTAREA_CSS;
    globs.value = (initial?.restartWhenChanged ?? []).join("\n");
    shell.box.insertBefore(globLabel, shell.buttons);
    shell.box.insertBefore(globs, shell.buttons);

    const envLabel = document.createElement("label");
    envLabel.style.cssText = FIELD_LABEL_CSS;
    envLabel.textContent = "ENV";
    const envRows = document.createElement("div");
    envRows.className = "chappa-cmd-env";
    const addEnvRow = (key: string, value: string): void => {
      const row = document.createElement("div");
      row.className = "chappa-cmd-env-row";
      row.style.cssText = ENV_ROW_CSS;
      const k = document.createElement("input");
      k.type = "text";
      k.className = "chappa-cmd-env-key";
      k.spellcheck = false;
      k.placeholder = "NAME";
      k.value = key;
      k.style.cssText = INPUT_CSS + "margin:0;flex:1 1 40%;";
      const v = document.createElement("input");
      v.type = "text";
      v.className = "chappa-cmd-env-value";
      v.spellcheck = false;
      v.placeholder = "value";
      v.value = value;
      v.style.cssText = INPUT_CSS + "margin:0;flex:1 1 60%;";
      const rm = document.createElement("button");
      rm.type = "button";
      rm.className = "chappa-cmd-env-remove";
      rm.textContent = "×";
      rm.title = "Remove variable";
      rm.style.cssText = SMALL_BTN_CSS;
      rm.addEventListener("click", () => row.remove());
      row.append(k, v, rm);
      envRows.appendChild(row);
      return;
    };
    for (const [k, v] of initial?.env ?? []) addEnvRow(k, v);
    const envAdd = document.createElement("button");
    envAdd.type = "button";
    envAdd.className = "chappa-cmd-env-add";
    envAdd.textContent = "+ Add variable";
    envAdd.style.cssText = SMALL_BTN_CSS + "margin:0 0 14px;";
    envAdd.addEventListener("click", () => {
      addEnvRow("", "");
      const keys = envRows.querySelectorAll<HTMLInputElement>(".chappa-cmd-env-key");
      keys[keys.length - 1]?.focus();
    });
    shell.box.insertBefore(envLabel, shell.buttons);
    shell.box.insertBefore(envRows, shell.buttons);
    shell.box.insertBefore(envAdd, shell.buttons);

    const answer = (): CommandForm => {
      const env: Array<[string, string]> = [];
      for (const row of envRows.querySelectorAll<HTMLElement>(".chappa-cmd-env-row")) {
        const k = row.querySelector<HTMLInputElement>(".chappa-cmd-env-key")!.value.trim();
        const v = row.querySelector<HTMLInputElement>(".chappa-cmd-env-value")!.value;
        if (k !== "") env.push([k, v]);
      }
      return {
        name: nameInput.value.trim(),
        command: commandInput.value.trim(),
        workingDir: workdirInput.value.trim(),
        autoStart: autoStart.checked,
        autoRestart: autoRestart.checked,
        restartWhenChanged: globs.value
          .split("\n")
          .map((g) => g.trim())
          .filter((g) => g !== ""),
        env,
      };
    };
    const filled = (): boolean =>
      nameInput.value.trim() !== "" && commandInput.value.trim() !== "";
    const refreshOk = (): void => {
      shell.ok.disabled = !filled();
      shell.ok.style.opacity = shell.ok.disabled ? "0.45" : "1";
    };
    nameInput.addEventListener("input", refreshOk);
    commandInput.addEventListener("input", refreshOk);
    refreshOk();

    mount<CommandForm | null>(
      shell,
      opts.container ?? document.body,
      resolve,
      answer,
      null,
      filled,
    );
    nameInput.focus();
    nameInput.select();
  });
}
