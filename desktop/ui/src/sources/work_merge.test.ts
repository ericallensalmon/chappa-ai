import { describe, expect, it } from "vitest";
import type * as ipc from "../ipc";
import { buildPrsModel, type PrRow } from "./prs_model";
import { fixture, overlayFixture, payload, pr, prsBody, source } from "./test-utils";
import {
  DEFAULT_KEY_PATTERN,
  branchName,
  candidateKeys,
  compileKeyMatch,
  joinPrs,
  keyPatternError,
  prItemActionTargets,
  workItemActionTargets,
} from "./work_merge";
import { buildWorkModel, type WorkRow } from "./work_model";

const body = (items: unknown[]) => ({ freshness: { asOf: "2026-01-15T10:00:00Z" }, items });
const item = (key: string, over: Record<string, unknown> = {}) => ({ key, title: `Item ${key}`, status: { label: "To do", category: "todo" }, ...over });
const overlay = (owner: string, key: string, over: Record<string, unknown> = {}) => ({ ref: { source: owner, key }, key, ...over });
const off = { assignedOnly: false, text: "" };
const tracker = (items: unknown[]) => payload("t", body(items), { sourceLabel: "Tracker" });
const ws = (items: unknown[]) => payload("w", body(items), { sourceLabel: "Workspaces" });
const other = (items: unknown[]) => payload("o", body(items), { sourceLabel: "Other" });
const SOURCES: ipc.SourceDto[] = [
  source("t", ["work"], { label: "Tracker", manifestId: "tracker" }),
  source("w", ["work"], { label: "Workspaces", manifestId: "workspaces" }),
  source("o", ["work"], { label: "Other", manifestId: "other" }),
];
const model = (payloads: ReturnType<typeof payload>[], sources: ipc.SourceDto[] = SOURCES) => buildWorkModel(payloads, { filter: off, sources });
const rowOf = (m: ReturnType<typeof model>, key: string): WorkRow => m.all.find((r) => r.key === key)!;
const prChip = (number: string) => ({ forge: "github", repo: "owner/repo", number, url: `https://registry.example/owner/repo/pull/${number.slice(1)}`, status: "open" });

describe("merging work entries across sources", () => {
  it("owner + overlay: one row, tracker fields from the owner, local fields from the overlay, lists appended", () => {
    const m = model([
      tracker([item("ABC-1", { title: "Fix login", url: "https://registry.example/t/ABC-1", folder: "/srv/example/owner-says", prs: [prChip("#1")], badges: [{ text: "high", tone: "bad" }], actions: [{ kind: "link", id: "open", label: "Open", url: "https://registry.example/t/ABC-1" }] })]),
      ws([
        overlay("tracker", "ABC-1", {
          title: "An old cached title",
          url: "https://registry.example/cached",
          status: { label: "Cached", category: "done" },
          folder: "/srv/example/abc-1",
          active: true,
          localStatus: { id: "review", label: "In review", allowed: [] },
          branches: [{ forge: "github", repo: "owner/repo", branch: "task/ABC-1" }],
          prs: [prChip("#1"), prChip("#2")],
          badges: [{ text: "1 unpushed", tone: "warn" }],
          sections: [{ id: "repos", title: "Repos", rows: [] }],
          actions: [{ kind: "endpoint", id: "archive", label: "Archive", mutates: true, confirm: "Archive?" }],
        }),
      ]),
    ]);
    expect(m.all).toHaveLength(1);
    const r = m.all[0];
    expect(r).toMatchObject({
      id: "t|ABC-1",
      sourceId: "t",
      title: "Fix login",
      url: "https://registry.example/t/ABC-1",
      status: { label: "To do", category: "todo" },
      folder: "/srv/example/abc-1",
      folderFrom: "Workspaces",
      active: true,
      localStatus: { id: "review", label: "In review" },
      localStatusFrom: { sourceId: "w", sourceLabel: "Workspaces", key: "ABC-1" },
      branches: [{ forge: "github", repo: "owner/repo", branch: "task/ABC-1" }],
      owner: { sourceId: "t", manifestId: "tracker" },
      hasActions: true,
    });
    // Deduped by forge:repo:number, the owner's first.
    expect(r.prs.map((p) => p.text)).toEqual(["owner/repo#1", "owner/repo#2"]);
    expect(r.badges.map((b) => b.text)).toEqual(["high", "1 unpushed"]);
    expect(r.contributions.map((c) => [c.sourceId, c.role, c.key, c.actionIds, c.sections.map((s) => s.id)])).toEqual([
      ["t", "owner", "ABC-1", ["open"], []],
      ["w", "overlay", "ABC-1", ["archive"], ["repos"]],
    ]);
    // The row's own (primary) parts are still the owner's.
    expect([r.actionIds, r.sections]).toEqual([["open"], []]);
  });

  it("an owner value for a local field counts only when no overlay sets one", () => {
    const m = model([tracker([item("ABC-1", { folder: "/srv/example/owner", active: true })]), ws([overlay("tracker", "ABC-1", { badges: [{ text: "x" }] })])]);
    expect(rowOf(m, "ABC-1")).toMatchObject({ folder: "/srv/example/owner", active: true });
    expect(rowOf(m, "ABC-1").folderFrom).toBeUndefined();
  });

  it("two overlays setting a local field differently: the first in the Settings order wins, with a warning naming both", () => {
    const m = model([
      tracker([item("ABC-1")]),
      ws([overlay("tracker", "ABC-1", { folder: "/srv/example/a", active: true, branches: [{ forge: "github", repo: "owner/repo", branch: "a" }] })]),
      other([overlay("tracker", "ABC-1", { folder: "/srv/example/b", active: true, branches: [{ forge: "github", repo: "owner/repo", branch: "b" }] })]),
    ]);
    const r = rowOf(m, "ABC-1");
    expect([r.folder, r.folderFrom, r.branches[0].branch]).toEqual(["/srv/example/a", "Workspaces", "a"]);
    // `active` is the same in both: no conflict about it.
    expect(r.badges).toEqual([{ text: "Workspaces and Other both set folder, branches", tone: "warn", title: "Workspaces comes first in Settings ▸ Sources, so its values are used" }]);
    expect(r.contributions.map((c) => c.sourceId)).toEqual(["t", "w", "o"]);
    // The order is the payloads' order (the Settings order), not the sources'.
    const swapped = model([tracker([item("ABC-1")]), other([overlay("tracker", "ABC-1", { folder: "/srv/example/b" })]), ws([overlay("tracker", "ABC-1", { folder: "/srv/example/a" })])]);
    expect(rowOf(swapped, "ABC-1").folder).toBe("/srv/example/b");
    expect(rowOf(swapped, "ABC-1").badges[0].text).toBe("Other and Workspaces both set folder");
  });

  it("owner missing: the overlay shows on its own with its cached title and url, and says whether the owner no longer lists it", () => {
    const m = model([tracker([item("ABC-1")]), ws([overlay("tracker", "ABC-9", { title: "Cached", url: "https://registry.example/t/ABC-9", status: { label: "Doing", category: "doing" }, assignedToMe: false, folder: "/srv/example/abc-9" })])]);
    const r = rowOf(m, "ABC-9");
    expect(r).toMatchObject({ id: "w|ABC-9", sourceId: "w", sourceLabel: "Workspaces", title: "Cached", url: "https://registry.example/t/ABC-9", status: null, folder: "/srv/example/abc-9", owner: null, ownerRef: { source: "tracker", key: "ABC-9" }, keySourceId: "t" });
    expect(r.badges).toEqual([{ text: "not listed by Tracker", tone: "muted", title: "Tracker no longer lists ABC-9; this is what other sources keep about it" }]);
    // Not assigned to anyone as far as anyone knows, yet never filtered away.
    const filtered = buildWorkModel([tracker([item("ABC-1", { assignedToMe: true })]), ws([overlay("tracker", "ABC-9")])], { sources: SOURCES });
    expect(filtered.assignedOnly).toBe(true);
    expect(filtered.groups.flatMap((g) => g.rows.map((x) => x.key))).toEqual(["ABC-1", "ABC-9"]);
  });

  it("owner unavailable: not set up here, waiting for review, down, or not answered yet", () => {
    const badge = (payloads: ReturnType<typeof payload>[], sources: ipc.SourceDto[]) => rowOf(model(payloads, sources), "ABC-1").badges[0];
    const only = [ws([overlay("tracker", "ABC-1")])];
    expect(badge(only, [SOURCES[1]])).toEqual({ text: "tracker unavailable", tone: "warn", title: "ABC-1: no source called tracker is set up here" });
    // An untrusted owner feeds nothing (the data layer reads trusted sources
    // only), so the overlay stands alone, its owner named by its label.
    expect(badge(only, [{ ...SOURCES[0], trusted: false, status: "needs_review" }, SOURCES[1]]).title).toBe("ABC-1: Tracker needs review under Settings ▸ Sources");
    expect(badge(only, [{ ...SOURCES[0], status: "down" }, SOURCES[1]])).toMatchObject({ text: "Tracker unavailable", title: "ABC-1: Tracker is down" });
    expect(badge([payload("t", null, { sourceLabel: "Tracker" }), ...only], SOURCES).title).toBe("ABC-1: Tracker has not answered yet");
  });

  it("an untrusted overlay adds nothing and offers nothing on a trusted owner's row, and the reverse", () => {
    // What the data layer hands over for an untrusted `w`: only `t`'s payload.
    const sources = [SOURCES[0], { ...SOURCES[1], trusted: false, trust: offers("start-work", "work", ["*"]) }];
    const m = model([tracker([item("ABC-1")])], sources);
    const r = rowOf(m, "ABC-1");
    expect(r.contributions.map((c) => c.sourceId)).toEqual(["t"]);
    expect(workItemActionTargets(r, sources)).toEqual([]);
    // An untrusted owner: the overlay's row is the overlay's alone, its
    // actions run against it, and nothing of the owner is on it.
    const rev = model([ws([overlay("tracker", "ABC-1", { actions: [{ kind: "endpoint", id: "archive", label: "A", mutates: false }] })])], [{ ...SOURCES[0], trusted: false }, SOURCES[1]]);
    const o = rowOf(rev, "ABC-1");
    expect([o.owner, o.sourceId, o.contributions.map((c) => c.sourceId), o.actionIds]).toEqual([null, "w", ["w"], ["archive"]]);
  });

  it("several overlays of a missing owner make one row; a source's repeated overlay and an overlay without a key", () => {
    const m = model([ws([overlay("tracker", "ABC-9", { title: "First" }), { ref: { source: "tracker", key: "ABC-8" }, title: "Keyless" }]), other([overlay("tracker", "ABC-9", { title: "Second", badges: [{ text: "o" }] })])]);
    expect(m.all.map((r) => [r.id, r.title])).toEqual([
      ["w|ABC-9", "First"],
      ["w|ABC-8", "Keyless"],
    ]);
    expect(rowOf(m, "ABC-9").contributions.map((c) => c.sourceId)).toEqual(["w", "o"]);
    // The tracker is set up but has not answered: the other overlay's badge, then why.
    expect(rowOf(m, "ABC-9").badges.map((b) => b.text)).toEqual(["o", "Tracker unavailable"]);
    const twice = model([tracker([item("ABC-1")]), ws([overlay("tracker", "ABC-1", { badges: [{ text: "a" }] })]), payload("w", body([overlay("tracker", "ABC-1", { badges: [{ text: "b" }] })]), { sourceLabel: "Workspaces" })]);
    expect(rowOf(twice, "ABC-1").badges.map((b) => b.text)).toEqual(["a"]);
  });

  it("banners are appended from every contributor and the row's tone is their worst", () => {
    const banner = (id: string, tone: string) => ({ id, tone, title: `B ${id}` });
    const m = model([tracker([item("ABC-1", { banners: [banner("t1", "info")] }), item("ABC-2")]), ws([overlay("tracker", "ABC-1", { banners: [banner("w1", "warn")] }), overlay("tracker", "ABC-2", { banners: [banner("w2", "bad"), banner("w3", "info")] })])]);
    expect(rowOf(m, "ABC-1").banners.map((b) => [b.sourceId, b.key, b.banner.id])).toEqual([
      ["t", "ABC-1", "t1"],
      ["w", "ABC-1", "w1"],
    ]);
    expect(rowOf(m, "ABC-1").bannerTone).toBe("warn");
    expect(rowOf(m, "ABC-2").bannerTone).toBe("bad");
    expect(rowOf(model([tracker([item("ABC-3", { banners: [banner("x", "info")] })])]), "ABC-3").bannerTone).toBe("info");
    expect(rowOf(model([tracker([item("ABC-3")])]), "ABC-3").bannerTone).toBeNull();
  });

  it("the fixture pair: the tracker's rows gain the workspace source's parts", () => {
    const m = buildWorkModel([payload("fx", fixture("work"), { sourceLabel: "Fixture" }), payload("ws", overlayFixture("work"), { sourceLabel: "Workspaces" })], {
      filter: off,
      sources: [source("fx", ["work"], { label: "Fixture", manifestId: "fixture" }), source("ws", ["work"], { label: "Workspaces", manifestId: "workspaces" })],
    });
    expect(m.all.map((r) => r.id)).toEqual(["fx|ABC-1", "fx|ABC-2", "fx|owner/repo#2", "fx|ABC-3", "ws|ABC-9", "ws|XYZ-1"]);
    expect(rowOf(m, "ABC-1").title).toBe("Add a health route to api");
    expect(rowOf(m, "ABC-2")).toMatchObject({ folder: "/srv/example/abc-2", localStatus: { id: "review" }, bannerTone: "warn" });
    expect(rowOf(m, "ABC-9").badges.at(-1)?.text).toBe("not listed by Fixture");
    expect(rowOf(m, "XYZ-1").badges.at(-1)?.text).toBe("tracker unavailable");
  });
});

function offers(id: string, view: string, sources: string[]): ipc.SourceTrustDto {
  return { id: "x", label: "X", linkHosts: [], roots: [], views: ["work"], hash: "h", itemActions: [{ id, label: id, view, sources, mutates: true, confirm: "Do {key}?" }] };
}

describe("item actions on other sources' rows", () => {
  const listing = (over: Partial<ipc.SourceDto>[] = []): ipc.SourceDto[] => [
    source("t", ["work", "prs"], { label: "Tracker", manifestId: "tracker", ...over[0] }),
    source("w", ["work"], { label: "Workspaces", manifestId: "workspaces", trust: offers("start-work", "work", ["tracker"]), ...over[1] }),
    source("a", ["work"], { label: "Anywhere", manifestId: "anywhere", trust: offers("note", "work", ["*"]), ...over[2] }),
  ];
  it("offered by every trusted source that names the owner (or any), never on rows it already adds to, nor by the owner", () => {
    const sources = listing();
    const m = model([tracker([item("ABC-1"), item("ABC-2")]), ws([overlay("tracker", "ABC-2")])], sources);
    expect(workItemActionTargets(rowOf(m, "ABC-1"), sources)).toEqual([
      { sourceId: "w", sourceLabel: "Workspaces", view: "work", key: "ABC-1", owner: "t" },
      { sourceId: "a", sourceLabel: "Anywhere", view: "work", key: "ABC-1", owner: "t" },
    ]);
    expect(workItemActionTargets(rowOf(m, "ABC-2"), sources).map((t) => t.sourceId)).toEqual(["a"]);
    // On another owner's rows only the "*" one; the owner offers nothing on its own rows.
    const own = model([payload("w", body([item("W-1")]), { sourceLabel: "Workspaces" })], sources);
    expect(workItemActionTargets(rowOf(own, "W-1"), sources).map((t) => t.sourceId)).toEqual(["a"]);
    const untrusted = listing([{}, { trusted: false }, { trust: offers("note", "prs", ["*"]) }]);
    expect(workItemActionTargets(rowOf(m, "ABC-1"), untrusted)).toEqual([]);
  });

  it("none on a row whose owner is missing: there is no owner row to act on", () => {
    const sources = listing();
    const m = model([ws([overlay("tracker", "ABC-9")])], sources);
    expect(workItemActionTargets(rowOf(m, "ABC-9"), sources)).toEqual([]);
  });

  it("on PR rows: each declaring source once, keyed by the PR's origin, never by a source serving the PR", () => {
    const sources = listing([{}, { trust: offers("later", "prs", ["tracker"]) }, { trust: offers("note", "prs", ["*"]), views: [{ name: "prs", pollSeconds: 60, asOf: null, nextUtc: null, refreshing: false, lastError: null, hasPayload: true }] }]);
    const prs = buildPrsModel([payload("t", prsBody([pr("#1")]), { sourceLabel: "Tracker" })]);
    const row = prs.mine[0].rows[0];
    expect(prItemActionTargets(row, sources)).toEqual([
      { sourceId: "w", sourceLabel: "Workspaces", view: "prs", key: "github:owner/repo:#1", owner: "t" },
      { sourceId: "a", sourceLabel: "Anywhere", view: "prs", key: "github:owner/repo:#1", owner: "t" },
    ]);
    const both = buildPrsModel([payload("t", prsBody([pr("#1")]), { sourceLabel: "Tracker" }), payload("a", prsBody([pr("#1")]), { sourceLabel: "Anywhere" })]);
    expect(prItemActionTargets(both.mine[0].rows[0], sources).map((t) => t.sourceId)).toEqual(["w"]);
  });
});

describe("PRs joined to work rows", () => {
  const prRows = (items: unknown[]): PrRow[] => {
    const m = buildPrsModel([payload("p", prsBody(items), { sourceLabel: "Forge" })]);
    const seen = new Set<string>();
    return [...m.mine, ...m.reviewing, ...m.all].flatMap((g) => g.rows).filter((r) => !seen.has(r.id) && !!seen.add(r.id));
  };
  const joinsOf = (rows: WorkRow[], prs: PrRow[], rule: { enabled: boolean; pattern: string | null } | null = null) => joinPrs(rows, prs, () => rule);
  const how = (j: ReturnType<typeof joinsOf>, rowId: string) => (j.byRow.get(rowId) ?? []).map((x) => [x.pr.number, x.how]);

  it("the three rules, in order: linked, branch, key in branch, key in title", () => {
    const m = model([
      tracker([
        item("ABC-1", { prs: [prChip("#1")] }),
        item("ABC-2"),
        item("ABC-3"),
        item("ABC-4"),
      ]),
      ws([overlay("tracker", "ABC-2", { branches: [{ forge: "GitHub", repo: "Owner/Repo", branch: "refs/heads/task/two" }] })]),
    ]);
    const prs = prRows([
      pr("#1", { sourceBranch: "ABC-3-also-named" }),
      pr("#2", { sourceBranch: "task/two" }),
      pr("#3", { sourceBranch: "feature/ABC-3-queue", title: "ABC-4 in the title too" }),
      pr("#4", { title: "Fix ABC-4: the thing" }),
      pr("#5", { sourceBranch: "refs/heads/ABC-4" }),
    ]);
    const j = joinsOf(m.all, prs);
    // A linked PR is the row's own already: it joins by the first rule only.
    expect(how(j, "t|ABC-1")).toEqual([]);
    expect(how(j, "t|ABC-2")).toEqual([["#2", "branch"]]);
    expect(how(j, "t|ABC-3")).toEqual([["#1", "key in branch"], ["#3", "key in branch"]]);
    expect(how(j, "t|ABC-4")).toEqual([["#3", "key in title"], ["#4", "key in title"], ["#5", "key in branch"]]);
    // One PR on several items, shown on each; the PR side lists them in row order.
    expect(j.byPr.get(prs[0].id)?.map((x) => [x.key, x.how])).toEqual([["ABC-1", "linked"], ["ABC-3", "key in branch"]]);
    expect(j.byPr.get(prs[2].id)?.map((x) => [x.key, x.how, x.title])).toEqual([["ABC-3", "key in branch", "Item ABC-3"], ["ABC-4", "key in title", "Item ABC-4"]]);
    // The model puts the joined PRs on the row's PR list, saying how.
    const built = buildWorkModel([tracker([item("ABC-3")])], { filter: off, sources: SOURCES, prRows: prs });
    expect(built.all[0].prs.map((p) => [p.text, p.how, p.loaded])).toEqual([["owner/repo#1", "key in branch", true], ["owner/repo#3", "key in branch", true]]);
  });

  it("branches compare without refs/heads/ on either side, case-sensitively, on the same forge and repo only", () => {
    const rows = model([tracker([item("ABC-1", { branches: [{ forge: "github", repo: "owner/repo", branch: "task/x" }] })])]).all;
    const j = joinsOf(rows, prRows([
      pr("#1", { sourceBranch: "refs/heads/task/x" }),
      pr("#2", { sourceBranch: "task/X" }),
      pr("#3", { repo: "owner/other", sourceBranch: "task/x" }),
      pr("#4", { forge: "gitlab", sourceBranch: "task/x" }),
    ]), { enabled: false, pattern: null });
    expect(how(j, "t|ABC-1")).toEqual([["#1", "branch"]]);
    expect(branchName("refs/heads/a/b")).toBe("a/b");
    expect(branchName("a/refs/heads/b")).toBe("a/refs/heads/b");
  });

  it("key match: whole words only, the found text must equal the key, case-sensitive; and it can be switched off", () => {
    const rows = model([tracker([item("ABC-1"), item("abc-2")])]).all;
    const prs = prRows([
      pr("#1", { sourceBranch: "ABC-12-other" }),
      pr("#2", { sourceBranch: "XABC-1" }),
      pr("#3", { title: "abc-1 lowercase" }),
      pr("#4", { title: "ABC-1x" }),
      pr("#5", { title: "(ABC-1)" }),
      pr("#6", { title: "fix abc-2" }),
    ]);
    const j = joinsOf(rows, prs);
    expect(how(j, "t|ABC-1")).toEqual([["#5", "key in title"]]);
    // The default pattern never finds a lowercase key; a source's own pattern can.
    expect(how(j, "t|abc-2")).toEqual([]);
    expect(how(joinsOf(rows, prs, { enabled: true, pattern: "[a-z]+-\\d+" }), "t|abc-2")).toEqual([["#6", "key in title"]]);
    expect(how(joinsOf(rows, prs, { enabled: false, pattern: null }), "t|ABC-1")).toEqual([]);
    // A pattern that cannot be used matches nothing rather than everything.
    expect(how(joinsOf(rows, prs, { enabled: true, pattern: "([" }), "t|ABC-1")).toEqual([]);
  });

  it("each row uses its key source's rule: the owner's, or for a missing owner the source its overlays name", () => {
    const sources: ipc.SourceDto[] = [
      source("t", ["work"], { label: "Tracker", manifestId: "tracker", keyMatch: { enabled: false, pattern: null } }),
      source("u", ["work"], { label: "Other tracker", manifestId: "other-tracker" }),
      source("w", ["work"], { label: "Workspaces", manifestId: "workspaces" }),
    ];
    const m = buildWorkModel(
      [tracker([item("ABC-1")]), payload("u", body([item("ABC-2")]), { sourceLabel: "Other tracker" }), ws([overlay("tracker", "ABC-3"), overlay("elsewhere", "ABC-4")])],
      { filter: off, sources, prRows: prRows([pr("#1", { title: "ABC-1 ABC-2 ABC-3 ABC-4" })]), keyMatch: (id) => sources.find((s) => s.id === id)?.keyMatch ?? null },
    );
    expect(m.all.map((r) => [r.key, r.prs.map((p) => p.how)])).toEqual([
      ["ABC-1", []],
      ["ABC-2", ["key in title"]],
      ["ABC-3", []],
      ["ABC-4", ["key in title"]],
    ]);
  });

  it("candidate keys, patterns and their errors", () => {
    const re = compileKeyMatch(null)!;
    expect(re.source).toBe(DEFAULT_KEY_PATTERN);
    expect(candidateKeys("task/ABC-1-x ABC-22,XY-3 ab-4 A-5 ZZ9-9", re)).toEqual(["ABC-1", "ABC-22", "XY-3", "ZZ9-9"]);
    expect(candidateKeys("", re)).toEqual([]);
    // A pattern that can match nothing at all does not loop.
    expect(candidateKeys("abc", /x*/g)).toEqual([]);
    expect(compileKeyMatch({ enabled: false, pattern: null })).toBeNull();
    expect(compileKeyMatch({ enabled: true, pattern: "  " })!.source).toBe(DEFAULT_KEY_PATTERN);
    expect(keyPatternError("[A-Z]+-\\d+")).toBeNull();
    expect(keyPatternError("")).toBeNull();
    expect(keyPatternError("([")).toMatch(/^not a usable pattern/);
  });
});
