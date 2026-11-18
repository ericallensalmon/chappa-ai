import { describe, expect, it } from "vitest";
import type * as ipc from "../ipc";
import {
  BLOCKED_BADGE_TEXT,
  blockForRow,
  blockedBanner,
  folderInside,
  heldCount,
  heldText,
  markDataSafety,
  normFolder,
  SECRETS_BADGE_TEXT,
} from "./data_safety";
import type { Chip } from "./prs_model";

function block(over: Partial<ipc.DataSafetyBlockDto> = {}): ipc.DataSafetyBlockDto {
  return {
    sourceId: "st",
    sourceLabel: "Stack",
    instance: "app/0",
    state: "not-clean",
    current: true,
    reason: "db: 3 findings (email-real-domain ×2, card-number ×1) — acknowledge in Environment",
    folders: ["/srv/example/.work/ABC-1", "/srv/example/.work/ABC-1/src/api-1a2b3c4d"],
    bound: { source: "tracker", key: "ABC-1" },
    component: "db",
    ackActionId: "data-safety-ack",
    ...over,
  };
}

const row = (over: Partial<{ key: string; folder: string | null; owner: { manifestId: string } | null; ownerRef: { source: string; key: string } | null }> = {}) => ({
  key: "ABC-1",
  folder: null,
  owner: { manifestId: "tracker" },
  ownerRef: null,
  badges: [] as Chip[],
  ...over,
});

describe("a secret-scan block in the views", () => {
  const secret = (over: Partial<ipc.DataSafetyBlockDto> = {}) =>
    block({
      kind: "secretScan",
      reason: "api worktree: 1 possible secret (.work/ABC-1/src/api-1a2b3c4d/.env:1 (github-pat)) — acknowledge in Environment",
      component: null,
      ackActionId: "secret-scan-ack",
      ...over,
    });

  it("wears its own badge and banner and leaves data safety's as they were", () => {
    const rows = [row()];
    markDataSafety(rows, [secret()]);
    expect(rows[0].badges.map((b) => b.text)).toEqual([SECRETS_BADGE_TEXT]);
    expect(blockedBanner(secret()).title).toContain("secret scan of app/0");
    expect(blockedBanner(block()).title).toBe("Agents are blocked here: the data on app/0 may be real");
    const plain = [row()];
    markDataSafety(plain, [block()]);
    expect(plain[0].badges.map((b) => b.text)).toEqual([BLOCKED_BADGE_TEXT]);
  });

});

describe("data safety in the views", () => {
  it("folders compare by whole parts, without trailing slashes or the verbatim prefix, Windows paths ignoring case", () => {
    expect(normFolder("C:\\Code\\X\\")).toBe("c:/code/x");
    expect(normFolder("\\\\?\\C:\\Code\\X")).toBe("c:/code/x");
    expect(normFolder("/srv/Example/")).toBe("/srv/Example");
    expect(folderInside("/srv/example", "/srv/example")).toBe(true);
    expect(folderInside("/srv/example", "/srv/example/.work/ABC-1")).toBe(true);
    expect(folderInside("/srv/example/.work/ABC-1", "/srv/example/.work/ABC-10")).toBe(false);
    expect(folderInside("C:\\code\\x", "c:/CODE/x/.work")).toBe(true);
    expect(folderInside("/srv/example", "/srv/Example/x")).toBe(false);
  });

  it("a row is covered when the block is bound to its item, or names its folder or one above it", () => {
    const b = block();
    expect(blockForRow(row(), [b])).toBe(b);
    // The overlay's own row before the owner is listed names the item by its ref.
    expect(blockForRow(row({ owner: null, ownerRef: { source: "tracker", key: "ABC-1" } }), [b])).toBe(b);
    // Another source's ABC-1 is another item.
    expect(blockForRow(row({ owner: { manifestId: "other" } }), [b])).toBeNull();
    expect(blockForRow(row({ key: "ABC-2" }), [b])).toBeNull();
    // Unbound: the stack project, and every item folder inside it.
    const project = block({ bound: null, folders: ["/srv/example"] });
    expect(blockForRow(row({ key: "ABC-2", folder: "/srv/example/.work/ABC-2" }), [project])).toBe(project);
    expect(blockForRow(row({ key: "ABC-2", folder: "/srv/elsewhere" }), [project])).toBeNull();
    // A bound block with an unknown owner matches by folder only.
    const anon = block({ bound: { source: null, key: "ABC-1" } });
    expect(blockForRow(row(), [anon])).toBeNull();
    expect(blockForRow(row({ folder: "/srv/example/.work/ABC-1" }), [anon])).toBe(anon);
  });

  it("marking adds the bad badge once, the reason as its title, and the block for the panel", () => {
    const rows = [row(), row({ key: "ABC-2" })];
    markDataSafety(rows, [block()]);
    markDataSafety(rows, [block()]);
    expect(rows[0].badges).toEqual([{ text: BLOCKED_BADGE_TEXT, tone: "bad", title: block().reason }]);
    expect((rows[0] as { dataSafety?: unknown }).dataSafety).toEqual(block());
    expect(rows[1].badges).toEqual([]);
    expect((rows[1] as { dataSafety?: unknown }).dataSafety).toBeNull();
    expect(blockedBanner(block()).title).toBe("Agents are blocked here: the data on app/0 may be real");
    expect(blockedBanner(block()).lines[0]).toBe(block().reason);
  });

  it("the held marker counts messages", () => {
    const state: ipc.DataSafetyStateDto = { blocks: [], agents: [], held: [{ termId: 3, count: 1 }] };
    expect(heldCount(state, 3)).toBe(1);
    expect(heldCount(state, 4)).toBe(0);
    expect(heldText(1)).toBe("held: 1 message");
    expect(heldText(3)).toBe("held: 3 messages");
  });
});
