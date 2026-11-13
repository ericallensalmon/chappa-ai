// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { openFileTarget, type FileOpenerApi } from "./open_file";
import { clampSettings, defaultSettings, EDITOR_PRESETS } from "./settings";
import { SettingsPane } from "./settings_pane";
import { TranscriptPanel } from "./transcript";
import { stubSettings } from "./term/test-utils";
import type * as ipc from "./ipc";

/** A fake opener over a tiny in-memory disk: `files` and `dirs` exist. */
function fakeOpener(disk: { files?: string[]; dirs?: string[] } = {}, openError?: string) {
  const opens: Array<[string, number | undefined, number | undefined]> = [];
  const api: FileOpenerApi = {
    homeDir: vi.fn(async () => "/home/dev"),
    resolvePath: vi.fn(async (_base: string | null, text: string): Promise<ipc.ResolvedPathDto> => {
      if (disk.files?.includes(text)) return { path: text, exists: true, kind: "file" };
      if (disk.dirs?.includes(text)) return { path: text, exists: true, kind: "dir" };
      return { path: text, exists: false, kind: null };
    }),
    openInEditor: vi.fn(async (path: string, line?: number, col?: number) => {
      if (openError !== undefined) throw openError;
      opens.push([path, line, col]);
    }),
  };
  return { api, opens };
}

describe("openFileTarget", () => {
  it("opens an existing file at the location the output named", async () => {
    const { api, opens } = fakeOpener({ files: ["/home/dev/proj/src/main.rs"] });
    expect(await openFileTarget(api, { path: "/home/dev/proj/src/main.rs", line: 12, col: 5 })).toBeNull();
    expect(opens).toEqual([["/home/dev/proj/src/main.rs", 12, 5]]);
  });

  it("reports a missing file instead of opening it", async () => {
    const { api, opens } = fakeOpener();
    expect(await openFileTarget(api, { path: "/home/dev/proj/src/gone.rs", line: 1 })).toBe(
      "not found: /home/dev/proj/src/gone.rs",
    );
    expect(opens).toEqual([]);
  });

  it("opens a directory without a location", async () => {
    const { api, opens } = fakeOpener({ dirs: ["/home/dev/proj/src"] });
    expect(await openFileTarget(api, { path: "/home/dev/proj/src", line: 3, col: 2 })).toBeNull();
    expect(opens).toEqual([["/home/dev/proj/src", undefined, undefined]]);
  });

  it("turns a failed launch or a failed check into the message to flash", async () => {
    const failing = fakeOpener({ files: ["/w/a.rs"] }, "cannot start code: program not found");
    expect(await openFileTarget(failing.api, { path: "/w/a.rs" })).toBe("cannot start code: program not found");
    const broken = fakeOpener();
    broken.api.resolvePath = vi.fn(async () => {
      throw new Error("ipc down");
    });
    expect(await openFileTarget(broken.api, { path: "/w/a.rs" })).toBe("ipc down");
  });
});

describe("editorCommand setting (frontend mirror)", () => {
  it("defaults to empty and stores a trimmed string", () => {
    expect(defaultSettings().editorCommand).toBe("");
    expect(clampSettings({}).editorCommand).toBe("");
    expect(clampSettings({ editorCommand: "  zed {path}:{line}:{col} " }).editorCommand).toBe("zed {path}:{line}:{col}");
    for (const bad of [7, null, ["code"], true]) {
      expect(clampSettings({ editorCommand: bad as unknown as string }).editorCommand).toBe("");
    }
  });

  it("every preset names the path and its location", () => {
    for (const p of EDITOR_PRESETS) {
      expect(p.template).toContain("{path}");
      expect(p.template).toContain("{line}");
    }
  });
});

describe("Opening files block", () => {
  async function openPane(stored: Partial<ipc.SettingsDto> = {}) {
    const harness = stubSettings(stored);
    await harness.store.load();
    const pane = new SettingsPane({ store: harness.store });
    pane.open();
    const select = pane.element.querySelector<HTMLSelectElement>('[data-setting="editorPreset"]')!;
    const field = pane.element.querySelector<HTMLInputElement>('[data-setting="editorCommand"]')!;
    return { harness, pane, select, field };
  }
  const settle = async (): Promise<void> => {
    await Promise.resolve();
    await Promise.resolve();
  };

  it("sits on the Terminal tab and shows the system default for an empty template", async () => {
    const { pane, select, field } = await openPane();
    expect(pane.element.querySelector("#chappa-settings-panel-terminal")!.contains(select)).toBe(true);
    expect([...select.options].map((o) => o.textContent)).toEqual([
      "System default",
      "VS Code",
      "Cursor",
      "Zed",
      "Sublime Text",
      "Custom…",
    ]);
    expect(select.selectedOptions[0].textContent).toBe("System default");
    expect(field.value).toBe("");
    pane.dispose();
  });

  it("a preset fills the field and stores its template", async () => {
    const { harness, pane, select, field } = await openPane();
    select.value = "preset:0";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(field.value).toBe("code --goto {path}:{line}:{col}");
    expect(harness.saved()!.editorCommand).toBe("code --goto {path}:{line}:{col}");
    expect(select.selectedOptions[0].textContent).toBe("VS Code");
    pane.dispose();
  });

  it("a stored template picks its preset, anything else reads as Custom", async () => {
    const preset = await openPane({ editorCommand: "subl {path}:{line}:{col}" });
    expect(preset.select.selectedOptions[0].textContent).toBe("Sublime Text");
    preset.pane.dispose();
    const custom = await openPane({ editorCommand: "notepad++ -n{line} {path}" });
    expect(custom.select.selectedOptions[0].textContent).toBe("Custom…");
    expect(custom.field.value).toBe("notepad++ -n{line} {path}");
    custom.pane.dispose();
  });

  it("the field commits trimmed on change; Custom keeps the template and focuses it", async () => {
    const { harness, pane, select, field } = await openPane({ editorCommand: "zed {path}:{line}:{col}" });
    document.body.appendChild(pane.element);
    select.value = "custom";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(harness.saved()).toBeNull();
    expect(document.activeElement).toBe(field);
    field.value = "  hx {path}:{line}  ";
    field.dispatchEvent(new Event("change"));
    await settle();
    expect(harness.saved()!.editorCommand).toBe("hx {path}:{line}");
    expect(select.selectedOptions[0].textContent).toBe("Custom…");
    select.value = "default";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(harness.saved()!.editorCommand).toBe("");
    expect(field.value).toBe("");
    pane.dispose();
  });
});

describe("TranscriptPanel.setPathBase", () => {
  it("blocks rendered after the switch resolve against the new base", async () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const panel = new TranscriptPanel({
      container,
      api: {
        sendAgentInput: vi.fn(),
        getAgentEvents: vi.fn(async () => []),
      },
      spawn: async () => 5,
      pathBase: { dir: "/workspace/proj", home: null, mounts: [] },
    });
    // The App re-points the base from `onCreated`, before any block renders.
    panel.setPathBase({
      dir: "/workspace/proj",
      home: null,
      mounts: [{ source: "/home/dev/proj", destination: "/workspace/proj" }],
    });
    await panel.start();
    panel.apply({ id: 5, seq: 1, ts: 1, kind: "text", payload: { text: "see src/a.rs:4" } });
    const anchor = container.querySelector<HTMLElement>("a.chappa-path-link")!;
    expect(anchor.dataset.path).toBe("/home/dev/proj/src/a.rs");
    expect(anchor.dataset.inContainer).toBeUndefined();
    panel.dispose(false);
    container.remove();
  });
});
