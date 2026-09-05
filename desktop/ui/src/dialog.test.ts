// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import {
  addProjectDialog,
  commandDialog,
  confirmDialog,
  folderBaseName,
  parentDirectory,
  promptDialog,
  type CommandForm,
} from "./dialog";

afterEach(() => {
  document.body.textContent = "";
});

const backdrop = (): HTMLElement =>
  document.querySelector<HTMLElement>(".chappa-dialog-backdrop")!;
const okButton = (): HTMLButtonElement =>
  document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!;
const cancelButton = (): HTMLButtonElement =>
  document.querySelector<HTMLButtonElement>(".chappa-dialog-cancel")!;
const input = (): HTMLInputElement =>
  document.querySelector<HTMLInputElement>(".chappa-dialog-input")!;

const press = (key: string): void => {
  document.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
};

describe("confirmDialog", () => {
  it("resolves true on OK and false on Cancel", async () => {
    const yes = confirmDialog("Close terminal?");
    okButton().click();
    expect(await yes).toBe(true);

    const no = confirmDialog("Close terminal?");
    cancelButton().click();
    expect(await no).toBe(false);
  });

  it("Enter confirms, Escape cancels", async () => {
    const yes = confirmDialog("Close terminal?");
    press("Enter");
    expect(await yes).toBe(true);

    const no = confirmDialog("Close terminal?");
    press("Escape");
    expect(await no).toBe(false);
  });

  it("a backdrop press cancels, a press on the box does not", async () => {
    const p = confirmDialog("Close terminal?");
    const box = document.querySelector<HTMLElement>(".chappa-dialog")!;
    box.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();
    backdrop().dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(await p).toBe(false);
  });

  it("uses the caller's affirmative label and focuses it", async () => {
    const p = confirmDialog("Run these?", { okLabel: "Run" });
    expect(okButton().textContent).toBe("Run");
    expect(document.activeElement).toBe(okButton());
    okButton().click();
    await p;
  });

  it("preserves a multi-line message (the trust gate's command list)", async () => {
    const message = "Run these auto-start commands?\n\npy app/summarize.py\npy app/design_sync.py";
    const p = confirmDialog(message);
    const text = document.querySelector<HTMLElement>(".chappa-dialog-message")!;
    expect(text.textContent).toBe(message);
    // Rendered, not collapsed: `pre-wrap` is what keeps the line breaks.
    expect(text.style.whiteSpace).toBe("pre-wrap");
    okButton().click();
    await p;
  });

  it("removes its DOM and its key listener when it resolves", async () => {
    const p = confirmDialog("Close terminal?");
    okButton().click();
    expect(await p).toBe(true);
    expect(document.querySelector(".chappa-dialog-backdrop")).toBeNull();
    // A stray Escape afterwards is nobody's business — nothing swallows it.
    const ev = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
    document.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
  });
});

describe("promptDialog", () => {
  it("returns the typed string on OK", async () => {
    const p = promptDialog("Project directory:");
    input().value = "C:\\p\\chappa-ai";
    okButton().click();
    expect(await p).toBe("C:\\p\\chappa-ai");
  });

  it("Enter submits, Escape returns null", async () => {
    const typed = promptDialog("Project directory:");
    input().value = "/p/new";
    press("Enter");
    expect(await typed).toBe("/p/new");

    const cancelled = promptDialog("Project directory:");
    input().value = "/p/ignored";
    press("Escape");
    expect(await cancelled).toBeNull();
  });

  it("Cancel and a backdrop press both return null", async () => {
    const byButton = promptDialog("Project directory:");
    cancelButton().click();
    expect(await byButton).toBeNull();

    const byBackdrop = promptDialog("Project directory:");
    backdrop().dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(await byBackdrop).toBeNull();
  });

  it("focuses the input and honours the placeholder", async () => {
    const p = promptDialog("Project directory:", { placeholder: "C:\\path\\to\\project" });
    expect(document.activeElement).toBe(input());
    expect(input().placeholder).toBe("C:\\path\\to\\project");
    cancelButton().click();
    await p;
  });

  it("pre-fills `value` — the rename affordance's current name", async () => {
    const p = promptDialog("Rename project:", { value: "chappa-ai", okLabel: "Rename" });
    expect(input().value).toBe("chappa-ai");
    expect(okButton().textContent).toBe("Rename");
    input().value = "chappa-ai notes";
    okButton().click();
    expect(await p).toBe("chappa-ai notes");
  });
});

// --- add-project modal ---------------------------------------------

const pathField = (): HTMLInputElement =>
  document.querySelector<HTMLInputElement>(".chappa-dialog-path")!;
const nameField = (): HTMLInputElement =>
  document.querySelector<HTMLInputElement>(".chappa-dialog-name")!;
const browseButton = (): HTMLButtonElement | null =>
  document.querySelector<HTMLButtonElement>(".chappa-dialog-browse");

/** Drain the microtask queue (the Browse… handler is promise-chained). */
const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

/** Type into a field the way a user does: value + the `input` event. */
const type = (field: HTMLInputElement, value: string): void => {
  field.value = value;
  field.dispatchEvent(new Event("input", { bubbles: true }));
};

describe("folderBaseName", () => {
  it("takes the last segment for both separators, boundary regime included", () => {
    const cases: Array<[string, string]> = [
      ["C:\\code\\chappa-ai", "chappa-ai"],
      ["/home/dev/code/chappa-ai", "chappa-ai"],
      // Trailing separators are stripped before the split.
      ["C:\\code\\chappa-ai\\", "chappa-ai"],
      ["/home/dev/chappa-ai///", "chappa-ai"],
      // Mixed separators (a pasted path); the LAST of either wins.
      ["C:/code\\chappa-ai", "chappa-ai"],
      // No separator at all: the whole string is the name.
      ["chappa-ai", "chappa-ai"],
      // Surrounding whitespace from a paste.
      ["  /p/chappa-ai  ", "chappa-ai"],
      // Nothing to name: empty, roots, a bare drive.
      ["", ""],
      ["/", ""],
      ["\\", ""],
      ["C:\\", ""],
      ["C:", ""],
    ];
    for (const [path, want] of cases) {
      expect(folderBaseName(path), path).toBe(want);
    }
  });
});

describe("parentDirectory", () => {
  it("answers an absolute parent or null, boundary regime included", () => {
    const cases: Array<[string, string | null]> = [
      ["C:\\a\\b", "C:\\a"],
      ["/home/dev/code/chappa-ai", "/home/dev/code"],
      // The drive-root parent keeps its separator: the bare "C:" is a
      // drive-RELATIVE path (it resolves against that drive's CWD).
      ["C:\\chappa-ai", "C:\\"],
      ["C:/chappa-ai", "C:/"],
      // Trailing separators are stripped before the split.
      ["C:\\a\\b\\", "C:\\a"],
      ["/home/dev/chappa-ai///", "/home/dev"],
      // DECIDED: a root-level path answers the root itself — "/" is already
      // absolute, unlike a bare drive.
      ["/chappa-ai", "/"],
      // No parent to name: relative, bare drive, bare roots, empty.
      ["relative", null],
      ["C:", null],
      ["C:\\", null],
      ["/", null],
      ["\\", null],
      ["", null],
      ["   ", null],
    ];
    for (const [path, want] of cases) {
      expect(parentDirectory(path), path).toBe(want);
    }
  });
});

describe("addProjectDialog", () => {
  it("auto-fills the name from the path until the user edits it", async () => {
    const p = addProjectDialog();
    type(pathField(), "C:\\code\\chappa-ai");
    expect(nameField().value).toBe("chappa-ai");
    // Still tracking while untouched.
    type(pathField(), "C:\\code\\other-project");
    expect(nameField().value).toBe("other-project");

    // The user edits the name: it is now DIRTY and a later path change must
    // NOT overwrite it — the actual complaint.
    type(nameField(), "my notes app");
    type(pathField(), "C:\\code\\third");
    expect(nameField().value).toBe("my notes app");

    okButton().click();
    expect(await p).toEqual({ path: "C:\\code\\third", name: "my notes app" });
  });

  it("refuses Add (button and Enter) until BOTH fields are non-empty", async () => {
    const p = addProjectDialog();
    expect(okButton().disabled).toBe(true);
    okButton().click();
    press("Enter");
    // Still on screen: neither path did anything.
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();

    // Path only — the name is auto-filled, so blank it by hand to reach the
    // half-filled regime.
    type(pathField(), "C:\\code\\chappa-ai");
    type(nameField(), "   ");
    expect(okButton().disabled).toBe(true);
    press("Enter");
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();

    // Name only.
    type(pathField(), "");
    type(nameField(), "chappa-ai");
    expect(okButton().disabled).toBe(true);
    press("Enter");
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();

    // Both filled → Enter submits, trimmed.
    type(pathField(), "  C:\\code\\chappa-ai  ");
    press("Enter");
    expect(await p).toEqual({ path: "C:\\code\\chappa-ai", name: "chappa-ai" });
  });

  it("Escape still leaves a half-filled dialog (the refusal is not a trap)", async () => {
    const p = addProjectDialog();
    type(pathField(), "C:\\code\\chappa-ai");
    type(nameField(), "");
    press("Escape");
    expect(await p).toBeNull();
    expect(document.querySelector(".chappa-dialog-backdrop")).toBeNull();
  });

  it("Cancel and a backdrop press answer null", async () => {
    const byButton = addProjectDialog();
    type(pathField(), "C:\\code\\chappa-ai");
    cancelButton().click();
    expect(await byButton).toBeNull();

    const byBackdrop = addProjectDialog();
    backdrop().dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(await byBackdrop).toBeNull();
  });

  it("Browse… fills the path and the name; a cancelled pick changes nothing", async () => {
    let picked: string | null = "/home/dev/code/chappa-ai";
    const p = addProjectDialog({ browse: async () => picked });
    expect(browseButton()).not.toBeNull();

    browseButton()!.click();
    await flush();
    expect(pathField().value).toBe("/home/dev/code/chappa-ai");
    expect(nameField().value).toBe("chappa-ai");
    expect(okButton().disabled).toBe(false);

    // A cancelled picker (null) leaves the already-typed path alone.
    picked = null;
    browseButton()!.click();
    await flush();
    expect(pathField().value).toBe("/home/dev/code/chappa-ai");

    okButton().click();
    // The name was never EDITED: the visible "chappa-ai" was only the preview, so
    // the answer carries null and Rust owns the default (yml name → folder
    // basename).
    expect(await p).toEqual({ path: "/home/dev/code/chappa-ai", name: null });
  });

  it("a REJECTED picker behaves like a cancelled one (no unhandled rejection)", async () => {
    const p = addProjectDialog({ browse: async () => Promise.reject(new Error("no plugin")) });
    type(pathField(), "/p/typed");
    browseButton()!.click();
    await flush();
    // The dialog is still up, the typed path untouched, Add still live.
    expect(document.querySelector(".chappa-dialog-backdrop")).not.toBeNull();
    expect(pathField().value).toBe("/p/typed");
    expect(okButton().disabled).toBe(false);
    okButton().click();
    expect(await p).toEqual({ path: "/p/typed", name: null });
  });

  it("omits Browse… entirely when there is no picker (outside Tauri)", async () => {
    const p = addProjectDialog({ browse: null });
    expect(browseButton()).toBeNull();
    // The typed-path field is the whole fallback; the auto-filled name stays
    // a preview (untouched → null on the wire).
    type(pathField(), "/p/chappa-ai");
    okButton().click();
    expect(await p).toEqual({ path: "/p/chappa-ai", name: null });
  });

  it("answers the EDITED name only when the user actually edited it", async () => {
    // Dirty name → the string rides along verbatim (trimmed).
    const edited = addProjectDialog();
    type(pathField(), "/p/chappa-ai");
    type(nameField(), "  my notes  ");
    okButton().click();
    expect(await edited).toEqual({ path: "/p/chappa-ai", name: "my notes" });

    // Editing the PATH never dirties the name: still null.
    const pathOnly = addProjectDialog();
    type(pathField(), "/p/chappa-ai");
    type(pathField(), "/p/other");
    okButton().click();
    expect(await pathOnly).toEqual({ path: "/p/other", name: null });
  });

  it("no Sync-with-chappa.yml checkbox is rendered", async () => {
    // Removed the sync toggle entirely: chappa.yml is always the
    // project file, so the add dialog offers no sync choice.
    const p = addProjectDialog();
    expect(document.querySelector(".chappa-dialog-sync")).toBeNull();
    type(pathField(), "/p/plain");
    okButton().click();
    expect(await p).toEqual({ path: "/p/plain", name: null });
  });
});

// --- commandDialog -------------------------------------------------

const q = <T extends Element>(sel: string): T => document.querySelector<T>(sel)!;
const setValue = (el: HTMLInputElement | HTMLTextAreaElement, value: string): void => {
  el.value = value;
  el.dispatchEvent(new Event("input", { bubbles: true }));
};

const INITIAL: CommandForm = {
  name: "dev server",
  command: "npm run dev",
  workingDir: "web",
  autoStart: false,
  autoRestart: true,
  restartWhenChanged: ["src/**/*.ts", "vite.config.ts"],
  env: [
    ["PORT", "5173"],
    ["DEBUG", "1"],
  ],
};

describe("commandDialog", () => {
  it("pre-fills every field from the initial form and answers the edited, trimmed form", async () => {
    const p = commandDialog(INITIAL);
    expect(q(".chappa-dialog-message").textContent).toBe("Edit command");
    expect(q<HTMLInputElement>(".chappa-cmd-name").value).toBe("dev server");
    expect(q<HTMLInputElement>(".chappa-cmd-command").value).toBe("npm run dev");
    expect(q<HTMLInputElement>(".chappa-cmd-workdir").value).toBe("web");
    expect(q<HTMLInputElement>(".chappa-cmd-autostart").checked).toBe(false);
    expect(q<HTMLInputElement>(".chappa-cmd-autorestart").checked).toBe(true);
    expect(q<HTMLTextAreaElement>(".chappa-cmd-globs").value).toBe("src/**/*.ts\nvite.config.ts");
    const keys = [...document.querySelectorAll<HTMLInputElement>(".chappa-cmd-env-key")];
    expect(keys.map((k) => k.value)).toEqual(["PORT", "DEBUG"]);

    setValue(q(".chappa-cmd-name"), "  dev server 2  ");
    setValue(q(".chappa-cmd-workdir"), "  ");
    q<HTMLInputElement>(".chappa-cmd-autostart").click();
    setValue(q(".chappa-cmd-globs"), "\n src/**/*.ts \n\n");
    // Remove PORT, add a new row, leave a blank-key row behind (dropped).
    q<HTMLButtonElement>(".chappa-cmd-env-remove").click();
    q<HTMLButtonElement>(".chappa-cmd-env-add").click();
    q<HTMLButtonElement>(".chappa-cmd-env-add").click();
    const rows = [...document.querySelectorAll<HTMLElement>(".chappa-cmd-env-row")];
    expect(rows.length).toBe(3);
    setValue(rows[1].querySelector<HTMLInputElement>(".chappa-cmd-env-key")!, " NEW ");
    setValue(rows[1].querySelector<HTMLInputElement>(".chappa-cmd-env-value")!, "x y");
    okButton().click();
    expect(await p).toEqual({
      name: "dev server 2",
      command: "npm run dev",
      workingDir: "",
      autoStart: true,
      autoRestart: true,
      restartWhenChanged: ["src/**/*.ts"],
      env: [
        ["DEBUG", "1"],
        ["NEW", "x y"],
      ],
    });
  });

  it("starts empty for Add with auto_start on, and refuses Save until name + command are filled", async () => {
    const p = commandDialog(null);
    expect(q(".chappa-dialog-message").textContent).toBe("Add command");
    expect(q<HTMLInputElement>(".chappa-cmd-autostart").checked).toBe(true);
    expect(okButton().disabled).toBe(true);
    press("Enter");
    expect(backdrop()).not.toBeNull(); // refused: still open
    setValue(q(".chappa-cmd-name"), "docs");
    expect(okButton().disabled).toBe(true);
    setValue(q(".chappa-cmd-command"), "mkdocs serve");
    expect(okButton().disabled).toBe(false);
    press("Enter");
    expect(await p).toEqual({
      name: "docs",
      command: "mkdocs serve",
      workingDir: "",
      autoStart: true,
      autoRestart: false,
      restartWhenChanged: [],
      env: [],
    });
  });

  it("Enter inside the glob textarea is a newline, not a submit; Escape cancels", async () => {
    const p = commandDialog({ ...INITIAL });
    const globs = q<HTMLTextAreaElement>(".chappa-cmd-globs");
    globs.focus();
    globs.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    expect(backdrop()).not.toBeNull();
    press("Escape");
    expect(await p).toBeNull();
    expect(backdrop()).toBeNull();
  });
});
