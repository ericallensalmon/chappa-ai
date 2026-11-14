import { describe, expect, it } from "vitest";
import {
  BUCKETS,
  buildPrsModel,
  chipsFor,
  needsYouOf,
  oneLine,
  prActionKey,
  prId,
  prsNeedingYou,
  readPr,
  stripMarkdownLinks,
  summarizeFreshness,
  type PrsModel,
} from "./prs_model";
import { fixture, payload, pr, prsBody } from "./test-utils";

const buckets = (m: PrsModel, tab: "mine" | "reviewing"): string[] => m[tab].map((g) => g.bucket);
const ids = (m: PrsModel, tab: "mine" | "reviewing"): string[] => m[tab].flatMap((g) => g.rows.map((r) => r.number));

describe("the fixture's PRs", () => {
  it("split into My / Reviewing, grouped in the contract's bucket order with its labels", () => {
    const m = buildPrsModel([payload("fx", fixture("prs"))]);
    expect(buckets(m, "mine")).toEqual(["action", "draft", "ready", "waiting", "recent"]);
    expect(buckets(m, "reviewing")).toEqual(["replied", "vote", "mythreads", "voted", "group"]);
    expect(m.mine.map((g) => g.label)).toEqual([
      "Blocked on me",
      "Still a draft",
      "Approved and green",
      "Waiting on reviewers",
      "Merged or closed lately",
    ]);
    expect(m.counts).toEqual({ mine: 5, reviewing: 5 });
    // Waiting is folded (collapsed with its count), nothing else is.
    expect(m.mine.filter((g) => g.folded).map((g) => g.bucket)).toEqual(["waiting"]);
    expect(m.reviewing.some((g) => g.folded)).toBe(false);
    expect(m.hasData).toBe(true);
    expect(m.freshness).toEqual({ asOf: "2026-01-15T10:00:00Z", refreshing: false, notices: ["showing 10 of 24 pull requests"], errors: [] });
  });

  it("rows carry the fields the view shows, reasons verbatim one line each, chips from the facts", () => {
    const m = buildPrsModel([payload("fx", fixture("prs"))]);
    const one = m.mine[0].rows[0];
    expect(one).toMatchObject({
      id: "github/owner/repo/#1",
      title: "Change 1",
      author: "Alex",
      branch: "abc-1-health → main",
      reasons: ["Checks are failing: lint", "1 thread needs your reply"],
      primary: { sourceId: "fx", key: "github:owner/repo:#1", hasActions: true },
    });
    expect(one.chips).toEqual([
      { text: "1 check failing", tone: "bad", title: "lint" },
      { text: "1 thread for me", tone: "warn", title: "2 open, 1 resolved" },
      { text: "conflicts", tone: "bad" },
      { text: "1 waiting for author", tone: "warn", title: "Sam" },
    ]);
    const vote = m.reviewing.find((g) => g.bucket === "vote")!.rows[0];
    expect(vote.chips).toEqual([{ text: "required", tone: "info", title: "your review is required" }]);
    const voted = m.reviewing.find((g) => g.bucket === "voted")!.rows[0];
    expect(voted.chips.map((c) => c.text)).toEqual(["closed", "you rejected"]);
    const draft = m.mine.find((g) => g.bucket === "draft")!.rows[0];
    expect(draft.chips).toEqual([{ text: "draft", tone: "muted" }]);
    expect(draft.primary.hasActions).toBe(false);
  });

  it("the Needs you buckets: yes rows counted, waiting folded, the rest out", () => {
    const { yes, folded } = prsNeedingYou(buildPrsModel([payload("fx", fixture("prs"))]));
    expect(yes.map((r) => r.number)).toEqual(["#1", "#2", "#3", "#6", "#7"]);
    expect(folded.map((r) => r.number)).toEqual(["#4"]);
    expect(needsYouOf("mine", "recent")).toBe("no");
    expect(needsYouOf("reviewing", "action")).toBe("no");
    expect(needsYouOf("mine", "brand-new-bucket")).toBe("no");
    expect(BUCKETS.mine.map((b) => b.needsYou)).toEqual(["yes", "yes", "yes", "folded", "no"]);
    expect(BUCKETS.reviewing.map((b) => b.needsYou)).toEqual(["yes", "yes", "no", "no", "no"]);
  });
});

describe("dedupe across sources", () => {
  it("one row per (forge, repo, number); the fresher payload supplies the fields; every source is an origin", () => {
    const older = prsBody([pr("#1", { title: "old title", bucket: "waiting", actions: [{ kind: "link", id: "open", label: "Open", url: "https://registry.example/a" }] })], "2026-01-15T09:00:00Z");
    const newer = prsBody([pr("#1", { repo: "Owner/Repo", title: "new title", bucket: "ready", actions: [{ kind: "endpoint", id: "merge", label: "Merge", mutates: true, confirm: "Merge?" }] })], "2026-01-15T10:00:00Z");
    const m = buildPrsModel([payload("a", older), payload("b", newer)]);
    expect(m.counts.mine).toBe(1);
    const row = m.mine[0].rows[0];
    expect(row.title).toBe("new title");
    expect(row.bucket).toBe("ready");
    expect(row.primary.sourceId).toBe("b");
    // Each origin keeps the key ITS source's payload addresses the PR by.
    expect(row.origins.map((o) => [o.sourceId, o.key])).toEqual([
      ["b", "github:Owner/Repo:#1"],
      ["a", "github:owner/repo:#1"],
    ]);
    // Order of the sources does not change who wins.
    const flipped = buildPrsModel([payload("b", newer), payload("a", older)]);
    expect(flipped.mine[0].rows[0].title).toBe("new title");
    expect(flipped.mine[0].rows[0].origins.map((o) => o.sourceId)).toEqual(["b", "a"]);
  });

  it("a tie, or a payload without an as-of, keeps the first source's fields", () => {
    const a = prsBody([pr("#2", { title: "from a" })], "2026-01-15T10:00:00Z");
    const b = prsBody([pr("#2", { title: "from b" })], "2026-01-15T10:00:00Z");
    expect(buildPrsModel([payload("a", a), payload("b", b)]).mine[0].rows[0].title).toBe("from a");
    const undated = prsBody([pr("#2", { title: "undated" })], null);
    expect(buildPrsModel([payload("u", undated), payload("a", a)]).mine[0].rows[0].title).toBe("from a");
  });

  it("equal numbers in different repos stay two rows with distinct action keys", () => {
    const m = buildPrsModel([payload("a", prsBody([pr("#12"), pr("#12", { repo: "owner/other" })]))]);
    expect(m.mine[0].rows.map((r) => r.primary.key)).toEqual(["github:owner/repo:#12", "github:owner/other:#12"]);
    expect(prId("GitHub", "Owner/Repo", "#12")).toBe(prId("github", "owner/repo", "#12"));
    expect(prActionKey("github", "Owner/Repo", "#12")).toBe("github:Owner/Repo:#12");
  });
});

describe("a payload read leniently", () => {
  it("skips rows without the required fields and says how many; unknown fields are ignored", () => {
    const body = prsBody([
      pr("#1", { surprise: { nested: true }, reviewers: "not a list", checks: { state: "sideways" }, threads: { active: "2" } }),
      { forge: "github", repo: "owner/repo", title: "no number", role: "mine" },
      pr("#3", { role: "observer" }),
      "not an object",
    ]);
    const m = buildPrsModel([payload("a", body)]);
    expect(ids(m, "mine")).toEqual(["#1"]);
    expect(m.freshness.errors).toEqual(["3 pull requests could not be read"]);
    const row = m.mine[0].rows[0];
    expect(row.chips).toEqual([]);
    expect(row.branch).toBeNull();
  });

  it("missing optional fields read as absent; an unknown bucket lands in Other, never in Needs you", () => {
    const bare = { forge: "gitlab", repo: "grp/app", number: "!7", title: "", role: "reviewing", bucket: "later-bucket" };
    const parsed = readPr(bare)!;
    expect(parsed.reasons).toEqual([]);
    expect(parsed.me).toBeNull();
    expect(parsed.state).toBe("open");
    expect(chipsFor(parsed)).toEqual([]);
    const m = buildPrsModel([payload("a", prsBody([bare]))]);
    expect(m.reviewing.map((g) => [g.bucket, g.label])).toEqual([["other", "Other"]]);
    // An empty title shows the repo and number instead.
    expect(m.reviewing[0].rows[0].title).toBe("grp/app!7");
    expect(prsNeedingYou(m).yes).toEqual([]);
  });

  it("a payload with no items, or none at all yet, renders empty without throwing", () => {
    expect(buildPrsModel([payload("a", { freshness: { asOf: null } })]).counts).toEqual({ mine: 0, reviewing: 0 });
    const waiting = buildPrsModel([payload("a", null)]);
    expect(waiting.hasData).toBe(false);
    expect(waiting.freshness.asOf).toBeNull();
    expect(buildPrsModel([]).hasData).toBe(false);
  });

  it("a payload speaking another contract version is a needs-update banner, not rows or a throw", () => {
    const m = buildPrsModel([payload("new", { contract: 2, freshness: { asOf: null }, items: [pr("#1")] }), payload("ok", prsBody([pr("#2")]))]);
    expect(m.needsUpdate).toEqual([{ sourceId: "new", sourceLabel: "new" }]);
    expect(ids(m, "mine")).toEqual(["#2"]);
    // A source the listing already marks stale is carried as given.
    expect(buildPrsModel([], [{ sourceId: "s", sourceLabel: "S" }]).needsUpdate).toEqual([{ sourceId: "s", sourceLabel: "S" }]);
  });
});

describe("freshness", () => {
  it("errors are warnings, notices are quiet; a failed poll's error shows beside the kept data", () => {
    const body = prsBody([pr("#1")], "2026-01-15T10:00:00Z", {
      refreshing: true,
      notices: ["showing 50 of 212", 3],
      errors: [{ source: "x:search", message: "rate [limited](https://registry.example/docs)" }],
    });
    const f = summarizeFreshness([
      payload("a", body, { freshness: { asOf: "2026-01-15T10:01:00Z", errors: [{ source: "a", message: "connection refused" }] } }),
    ]);
    expect(f).toEqual({ asOf: "2026-01-15T10:00:00Z", refreshing: true, notices: ["showing 50 of 212"], errors: ["connection refused", "rate limited"] });
  });

  it("with several sources each line names its source and the as-of is the oldest", () => {
    const f = summarizeFreshness([
      payload("A", prsBody([], "2026-01-15T10:00:00Z", { notices: ["n1"] })),
      payload("B", prsBody([], "2026-01-15T09:00:00Z", { errors: [{ source: "b", message: "e1" }] })),
      payload("C", null),
    ]);
    expect(f).toEqual({ asOf: "2026-01-15T09:00:00Z", refreshing: false, notices: ["A: n1"], errors: ["B: e1"] });
  });
});

describe("text", () => {
  it("markdown links become their text; line breaks collapse to one line", () => {
    expect(stripMarkdownLinks("see [the run](https://registry.example/r/1) and ![logo](x.png)")).toBe("see the run and logo");
    expect(stripMarkdownLinks("<https://registry.example/a> stays readable")).toBe("https://registry.example/a stays readable");
    expect(stripMarkdownLinks("a [link with (parens)](https://registry.example/a_(b)) here")).toBe("a link with (parens) here");
    expect(oneLine("first\n  second\r\nthird [x](https://registry.example)")).toBe("first second third x");
    expect(stripMarkdownLinks("[not a link] and (not one either)")).toBe("[not a link] and (not one either)");
  });

  it("reasons and titles in rows are stripped too", () => {
    const m = buildPrsModel([payload("a", prsBody([pr("#1", { title: "Fix [ABC-1](https://registry.example/t/ABC-1)", reasons: ["blocked by [Sam](https://registry.example/u/sam)\nsee thread", 42] })]))]);
    const row = m.mine[0].rows[0];
    expect(row.title).toBe("Fix ABC-1");
    expect(row.reasons).toEqual(["blocked by Sam see thread"]);
  });
});
