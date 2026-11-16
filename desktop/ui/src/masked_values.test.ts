// @vitest-environment jsdom
// Values that look like credentials stay in Rust: the command editor shows
// the masked text Rust sent, read-only, with a Replace field; left alone it
// answers the UNCHANGED marker (Rust keeps the original), typed into it
// answers the new value. Agent settings files git would commit become
// Needs you rows, with "Add to .gitignore" only for a file not yet tracked.
// The gitleaks config path setting is clamped.
import { afterEach, describe, expect, it, vi } from "vitest";
import { UNCHANGED, commandDialog, type CommandForm } from "./dialog";
import * as ipc from "./ipc";
import { agentSettingsSection, buildNeedsYou } from "./sources/needs_you";
import { SECRET_SCAN_CONFIG_MAX_CHARS, clampConfigPath } from "./settings";

const q = <T extends Element>(sel: string): T => document.querySelector<T>(sel)!;
const setValue = (el: HTMLInputElement, value: string): void => {
  el.value = value;
  el.dispatchEvent(new Event("input", { bubbles: true }));
};
const okButton = (): HTMLButtonElement =>
  [...document.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Save")!;

// What Rust sends for a row whose command and one env value hold a secret.
const MASKED: CommandForm = {
  name: "db",
  command: "psql postgresql://app:••••@db:5432/x",
  workingDir: "",
  autoStart: true,
  autoRestart: false,
  restartWhenChanged: [],
  env: [
    ["DB_PASSWORD", "•••• (set inline)"],
    ["NODE_ENV", "development"],
  ],
  masked: ["command", "env.DB_PASSWORD"],
};

afterEach(() => {
  document.body.innerHTML = "";
});

describe("the command editor with masked values", () => {
  it("shows masks read-only and sends UNCHANGED for what was not replaced", async () => {
    expect(UNCHANGED).toBe(ipc.UNCHANGED);
    const p = commandDialog(MASKED);
    const command = q<HTMLInputElement>(".chappa-cmd-command");
    expect(command.readOnly).toBe(true);
    expect(command.value).toBe("psql postgresql://app:••••@db:5432/x");
    expect(document.querySelector(".chappa-cmd-command-replace")).not.toBeNull();
    const rows = [...document.querySelectorAll<HTMLElement>(".chappa-cmd-env-row")];
    expect(rows[0].querySelector<HTMLInputElement>(".chappa-cmd-env-value")!.readOnly).toBe(true);
    expect(rows[0].querySelector(".chappa-cmd-env-replace")).not.toBeNull();
    expect(rows[1].querySelector(".chappa-cmd-env-replace")).toBeNull();
    okButton().click();
    const answer = (await p)!;
    expect(answer.command).toBe(UNCHANGED);
    expect(answer.env).toEqual([
      ["DB_PASSWORD", UNCHANGED],
      ["NODE_ENV", "development"],
    ]);
    // A mask never goes back as a value.
    expect(JSON.stringify(answer)).not.toContain("••••");
  });

  it("takes a typed replacement instead", async () => {
    const p = commandDialog(MASKED);
    setValue(q(".chappa-cmd-command-replace"), "  psql $DATABASE_URL ");
    setValue(q(".chappa-cmd-env-replace"), "${DB_PASSWORD}");
    okButton().click();
    const answer = (await p)!;
    expect(answer.command).toBe("psql $DATABASE_URL");
    expect(answer.env[0]).toEqual(["DB_PASSWORD", "${DB_PASSWORD}"]);
  });

  it("is unchanged for a form with nothing masked", async () => {
    const p = commandDialog({ ...MASKED, command: "npm run dev", env: [["PORT", "1"]], masked: [] });
    expect(q<HTMLInputElement>(".chappa-cmd-command").readOnly).toBe(false);
    expect(document.querySelector(".chappa-cmd-command-replace")).toBeNull();
    okButton().click();
    expect((await p)!.command).toBe("npm run dev");
  });
});

describe("agent settings files in Needs you", () => {
  const file = (exposure: "not-ignored" | "tracked"): ipc.AgentSettingsFileDto => ({
    path: `/p/${exposure}/.claude/settings.local.json`,
    ignoreLine: "/.claude/settings.local.json",
    exposure,
    message: exposure === "tracked" ? "x is committed" : "x is not git-ignored",
  });

  it("offers Add to .gitignore only for a file that is not tracked", () => {
    const add = vi.fn();
    const section = agentSettingsSection([
      { projectRoot: "/p", file: file("not-ignored"), addToGitignore: add },
      { projectRoot: "/p", file: file("tracked"), addToGitignore: add },
    ])!;
    expect(section.title).toBe("Agent settings files");
    const [open, committed] = section.rows;
    expect(open.tone).toBe("warn");
    expect(open.local?.label).toBe("Add to .gitignore");
    open.local!.run();
    expect(add).toHaveBeenCalledTimes(1);
    expect(committed.tone).toBe("bad");
    expect(committed.local).toBeUndefined();
    expect(agentSettingsSection([])).toBeNull();
    const model = buildNeedsYou({ terminals: [], work: [], prs: null, attention: [], agentSettings: [{ projectRoot: "/p", file: file("tracked") }] });
    expect(model.sections.map((s) => s.id)).toContain("agent-settings");
    expect(model.count).toBe(1);
  });
});

describe("the gitleaks config path setting", () => {
  it("is trimmed text, and an over-long or non-text value is dropped", () => {
    expect(clampConfigPath("  C:\\cfg\\gitleaks.toml ")).toBe("C:\\cfg\\gitleaks.toml");
    expect(clampConfigPath(7)).toBe("");
    expect(clampConfigPath("x".repeat(SECRET_SCAN_CONFIG_MAX_CHARS + 1))).toBe("");
  });
});
