import { describe, expect, it } from "vitest";
import { prId } from "./prs_model";
import { fixture, payload } from "./test-utils";
import { WORK_GROUPS, buildWorkModel, matchesText, readLocalStatus, readWorkItem } from "./work_model";

const fx = () => payload("fx", fixture("work"), { sourceLabel: "Fixture" });
const body = (items: unknown[], extra: Record<string, unknown> = {}) => ({ freshness: { asOf: "2026-01-15T10:00:00Z" }, items, ...extra });
const item = (key: string, over: Record<string, unknown> = {}) => ({ key, title: `Item ${key}`, status: { label: "To do", category: "todo" }, ...over });
const groupIds = (m: ReturnType<typeof buildWorkModel>) => m.groups.map((g) => g.id);
const keysOf = (m: ReturnType<typeof buildWorkModel>, id: string) => m.groups.find((g) => g.id === id)?.rows.map((r) => r.key);
const off = { assignedOnly: false, text: "" };

describe("buildWorkModel", () => {
  it("the fixture: the active item pinned first, then status groups, unknown status under Other, Done folded", () => {
    const m = buildWorkModel([fx()], { filter: off });
    expect(groupIds(m)).toEqual(["active", "todo", "done", "other"]);
    expect(keysOf(m, "active")).toEqual(["ABC-1"]);
    expect(keysOf(m, "todo")).toEqual(["ABC-2"]);
    expect(keysOf(m, "done")).toEqual(["ABC-3"]);
    expect(keysOf(m, "other")).toEqual(["owner/repo#2"]);
    expect(m.groups.map((g) => [g.label, g.folded])).toEqual([
      ["Active", false],
      ["To do", false],
      ["Done", true],
      ["Other", false],
    ]);
    expect(WORK_GROUPS.map((g) => g.id)).toEqual(["active", "doing", "todo", "done", "other"]);
    expect(m.shown).toBe(4);
    expect(m.hasData).toBe(true);
    expect(m.freshness.errors).toEqual(["rate limited, showing cached items"]);
    expect(m.freshness.refreshing).toBe(true);
    expect(m.banners.map((b) => [b.sourceId, b.banner.id, b.banner.title])).toEqual([["fx", "sync", "Syncing"]]);
  });

  it("an active item in progress leaves the In progress group; rows in a group keep the source's order", () => {
    const m = buildWorkModel(
      [payload("a", body([item("A-1", { status: { label: "Doing", category: "doing" } }), item("A-2", { active: true, status: { label: "Doing", category: "doing" } }), item("A-3", { status: { label: "Doing", category: "doing" } })]))],
      { filter: off },
    );
    expect(groupIds(m)).toEqual(["active", "doing"]);
    expect(keysOf(m, "active")).toEqual(["A-2"]);
    expect(keysOf(m, "doing")).toEqual(["A-1", "A-3"]);
  });

  it("a row: title (key when none), status chip toned by category, fields, PR chips, badges, own action ids", () => {
    const m = buildWorkModel([fx()], { filter: off, loadedPrs: new Set([prId("github", "owner/repo", "#1")]) });
    const abc1 = m.all.find((r) => r.key === "ABC-1")!;
    expect(abc1).toMatchObject({
      id: "fx|ABC-1",
      sourceId: "fx",
      sourceLabel: "Fixture",
      title: "Add a health route to api",
      type: "task",
      status: { label: "In progress", category: "doing" },
      statusChip: { text: "In progress", tone: "info" },
      priority: "high",
      labels: ["backend", "ops"],
      parent: { key: "ABC-100", title: "Observability" },
      reporter: { name: "Sam", isMe: false },
      folder: "/srv/example/abc-1",
      assignedToMe: true,
      active: true,
      hasActions: true,
    });
    expect(abc1.prs).toEqual([
      { id: "github/owner/repo/#1", key: "github:owner/repo:#1", text: "owner/repo#1", url: "https://registry.example/owner/repo/pull/1", status: "open", tone: "info", loaded: true, how: "linked" },
    ]);
    expect(abc1.badges).toEqual([{ text: "blocked", tone: "bad", title: "Waiting on approval" }]);
    expect(abc1.actionIds).toEqual(["agent-here", "terminal-here", "approve", "start-build", "run-seed", "flaky-deploy", "ping", "open-item", "validate"]);
    expect(abc1.sections.map((s) => s.id)).toEqual(["details"]);
    const bare = m.all.find((r) => r.key === "owner/repo#2")!;
    expect(bare).toMatchObject({ title: "owner/repo#2", hasTitle: false, status: null, statusChip: null, hasActions: false, localStatus: null, prs: [] });
    expect(m.all.find((r) => r.key === "ABC-3")).toMatchObject({ statusChip: { text: "Done", tone: "ok" }, reporter: { name: "Alex", isMe: true } });
  });

  it("a PR chip the PRs view does not list opens its link instead", () => {
    const m = buildWorkModel([fx()], { filter: off });
    expect(m.all.find((r) => r.key === "ABC-1")!.prs[0].loaded).toBe(false);
  });

  it("Assigned to me is on by default when any item is assigned to me; the active item always shows", () => {
    const m = buildWorkModel([fx()]);
    expect(m.anyAssigned).toBe(true);
    expect(m.assignedOnly).toBe(true);
    expect(m.all.length).toBe(4);
    expect(m.groups.flatMap((g) => g.rows.map((r) => r.key))).toEqual(["ABC-1"]);
    const mine = buildWorkModel([payload("a", body([item("A-1", { assignedToMe: true }), item("A-2"), item("A-3", { active: true })]))]);
    expect(mine.groups.flatMap((g) => g.rows.map((r) => r.key))).toEqual(["A-3", "A-1"]);
    // The user's choice wins over the default, both ways.
    expect(buildWorkModel([fx()], { filter: { assignedOnly: false, text: "" } }).shown).toBe(4);
    const none = buildWorkModel([payload("a", body([item("A-1"), item("A-2")]))]);
    expect([none.anyAssigned, none.assignedOnly, none.shown]).toEqual([false, false, 2]);
    expect(buildWorkModel([payload("a", body([item("A-1")]))], { filter: { assignedOnly: true, text: "" } }).shown).toBe(0);
  });

  it("the text filter matches every word against key, title and labels, case-insensitively", () => {
    const m = (text: string) => buildWorkModel([fx()], { filter: { assignedOnly: false, text } }).groups.flatMap((g) => g.rows.map((r) => r.key));
    expect(m("abc-")).toEqual(["ABC-1", "ABC-2", "ABC-3"]);
    expect(m("OPS")).toEqual(["ABC-1"]);
    expect(m("rename web")).toEqual(["ABC-2"]);
    expect(m("rename ops")).toEqual([]);
    expect(m("owner/repo")).toEqual(["owner/repo#2"]);
    expect(m("  ")).toHaveLength(4);
    // The key stands in for a missing title, but is not matched twice as one.
    const row = readWorkItem({ key: "K-1", title: null }, "a", "A", new Set())!;
    expect(matchesText(row, "k-1")).toBe(true);
  });

  it("keys are per source: the same key from two sources is two rows, each with its own source", () => {
    const m = buildWorkModel([payload("a", body([item("ABC-1")]), { sourceLabel: "A" }), payload("b", body([item("ABC-1")]), { sourceLabel: "B" })], { filter: off });
    expect(m.all.map((r) => [r.id, r.sourceLabel])).toEqual([
      ["a|ABC-1", "A"],
      ["b|ABC-1", "B"],
    ]);
  });

  it("lenient: unusable items are skipped and counted; a repeated key keeps the first; wrong field types read as absent", () => {
    const m = buildWorkModel(
      [
        payload(
          "a",
          body([
            item("A-1"),
            null,
            { title: "no key" },
            { key: "" },
            { key: 7 },
            item("A-1", { title: "again" }),
            {
              key: "A-2",
              title: 5,
              type: ["x"],
              status: { label: "Weird", category: "blocked" },
              labels: ["ok", 3, "", "  "],
              parent: { title: "no key" },
              reporter: { id: "x" },
              prs: [{ forge: "github", repo: "o/r" }, { forge: "github", repo: "o/r", number: "#3", status: "merged" }],
              badges: [{ text: "x", tone: "loud" }, { tone: "ok" }],
              localStatus: { id: "s" },
              sections: [{ id: "s1" }, { id: "s2", title: "Two", rows: [{ value: "no label" }, { label: "L" }] }],
              actions: [{ id: "a" }, { label: "no id" }, "x"],
              unknown: { deep: true },
            },
          ]),
        ),
      ],
      { filter: off },
    );
    expect(m.all.map((r) => r.key)).toEqual(["A-1", "A-2"]);
    expect(m.all[0].title).toBe("Item A-1");
    expect(m.freshness.errors).toEqual(["4 work items could not be read", "1 work item repeated a key and was left out"]);
    expect(m.all[1]).toMatchObject({
      title: "A-2",
      hasTitle: false,
      type: null,
      status: { label: "Weird", category: null },
      statusChip: { text: "Weird", tone: "muted" },
      labels: ["ok"],
      parent: null,
      reporter: null,
      localStatus: null,
      actionIds: ["a"],
    });
    expect(m.all[1].prs.map((p) => [p.text, p.tone])).toEqual([["o/r#3", "ok"]]);
    expect(m.all[1].badges).toEqual([{ text: "x", tone: "info" }]);
    expect(m.all[1].sections).toEqual([{ id: "s2", title: "Two", collapsed: false, rows: [{ label: "L" }] }]);
    expect(groupIds(m)).toEqual(["todo", "other"]);
  });

  it("another contract version is a needs-update banner; the other sources' rows stay", () => {
    const m = buildWorkModel([payload("a", { contract: 2, items: [item("X-1")] }, { sourceLabel: "A" }), payload("b", body([item("B-1")]), { sourceLabel: "B" })], {
      filter: off,
      needsUpdate: [{ sourceId: "c", sourceLabel: "C" }],
    });
    expect(m.needsUpdate).toEqual([
      { sourceId: "c", sourceLabel: "C" },
      { sourceId: "a", sourceLabel: "A" },
    ]);
    expect(m.all.map((r) => r.key)).toEqual(["B-1"]);
  });

  it("no payload yet: no data, no rows; garbage payloads read as empty", () => {
    expect(buildWorkModel([payload("a", null)]).hasData).toBe(false);
    for (const garbage of ["x", 4, [], { items: "nope" }]) {
      const m = buildWorkModel([payload("a", garbage)]);
      expect(m.all).toEqual([]);
      expect(m.groups).toEqual([]);
    }
  });
});

describe("readLocalStatus", () => {
  it("reads the fixture's workflow block", () => {
    const ls = readLocalStatus(fixture("work").items[0].localStatus)!;
    expect(ls).toEqual({
      id: "in-review",
      label: "In review",
      since: "2026-01-14T16:00:00Z",
      actor: "Alex",
      round: 2,
      gate: "human-exit",
      pendingApproval: { to: "done", by: "Alex", note: "Looks complete" },
      allowed: [
        { id: "approve", label: "Approve", humanOnly: true, requireNote: false },
        { id: "in-progress", label: "Send back", humanOnly: false, requireNote: true },
      ],
      history: [
        { at: "2026-01-13T10:00:00Z", from: "todo", to: "in-progress", actor: "Alex" },
        { at: "2026-01-14T16:00:00Z", from: "in-progress", to: "in-review", actor: "Alex", note: "Ready", override: "skipped checks" },
      ],
    });
  });

  it("is lenient: no id or label hides the block; bad gates, moves and history entries are dropped", () => {
    expect(readLocalStatus(null)).toBeNull();
    expect(readLocalStatus({ id: "x" })).toBeNull();
    expect(readLocalStatus({ label: "x" })).toBeNull();
    expect(
      readLocalStatus({
        id: "s",
        label: "S",
        gate: "robots",
        round: "2",
        pendingApproval: { by: "Sam" },
        allowed: [{ id: "a" }, { label: "b" }, { id: "c", label: "C", humanOnly: "yes" }],
        history: [{ to: "x" }, { at: "2026-01-01T00:00:00Z", note: 4 }],
      }),
    ).toEqual({
      id: "s",
      label: "S",
      since: null,
      actor: null,
      round: null,
      gate: null,
      pendingApproval: null,
      allowed: [{ id: "c", label: "C", humanOnly: false, requireNote: false }],
      history: [{ at: "2026-01-01T00:00:00Z" }],
    });
  });
});
