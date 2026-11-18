import { describe, expect, it } from "vitest";
import {
  BUCKETS,
  attentionOf,
  buildPrsModel,
  chipsFor,
  needsYouOf,
  oneLine,
  prActionKey,
  prId,
  prsNeedingYou,
  readPr,
  restatesChips,
  stripMarkdownLinks,
  votesOf,
  summarizeFreshness,
  type PrsModel,
  type PrsTab,
} from "./prs_model";
import { fixture, payload, pr, prsBody } from "./test-utils";

const buckets = (m: PrsModel, tab: PrsTab): string[] => m[tab].map((g) => g.id);
const ids = (m: PrsModel, tab: PrsTab): string[] => m[tab].flatMap((g) => g.rows.map((r) => r.number));
/** A review PR: open, role reviewing, in `bucket`, with `me` and reviewers as given. */
const review = (number: string, bucket: string, over: Record<string, unknown> = {}): Record<string, unknown> =>
  pr(number, { role: "reviewing", bucket, author: { name: "Sam" }, ...over });
const team = (name: string): Record<string, unknown> => ({ name, isGroup: true, vote: "none" });

describe("the fixture's PRs", () => {
  it("split into My PRs / Reviewing / All, grouped in bucket order with their labels", () => {
    const m = buildPrsModel([payload("fx", fixture("prs"))]);
    expect(buckets(m, "mine")).toEqual(["action", "draft", "ready", "waiting", "recent"]);
    // Reviewing: required by name (#7) or already engaged (#6 replied, #8
    // commented); the closed #9 and the team-only #10 are not in it.
    expect(buckets(m, "reviewing")).toEqual(["replied", "vote", "mythreads"]);
    expect(ids(m, "reviewing")).toEqual(["#6", "#7", "#8"]);
    // All: every open review by how I am on it.
    expect(m.all.map((g) => [g.id, g.label, g.folded, g.rows.map((r) => r.number)])).toEqual([
      ["required", "Required by name", false, ["#7"]],
      ["optional", "Optional", false, ["#6", "#8"]],
      ["team:platform-team", "platform-team", true, ["#10"]],
    ]);
    expect(m.mine.map((g) => g.label)).toEqual([
      "Blocked on me",
      "Still a draft",
      "Approved and green",
      "Waiting on reviewers",
      "Merged or closed lately",
    ]);
    expect(m.counts).toEqual({ mine: 5, reviewing: 3, all: 4 });
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
    // The bucket chip leads, so the row's state shows in any tab.
    expect(one.chips).toEqual([
      { text: "blocked on me", tone: "bad", title: "Blocked on me" },
      { text: "1 check failing", tone: "bad", title: "lint" },
      { text: "1 thread for me", tone: "warn", title: "2 open, 1 resolved" },
      { text: "conflicts", tone: "bad" },
      { text: "1 waiting for author", tone: "warn", title: "Sam" },
    ]);
    const vote = m.reviewing.find((g) => g.bucket === "vote")!.rows[0];
    expect(vote.chips).toEqual([
      { text: "vote asked", tone: "info", title: "My vote is asked for" },
      { text: "required", tone: "info", title: "your review is required" },
    ]);
    expect(vote.required).toBe(true);
    const team10 = m.all.find((g) => g.id === "team:platform-team")!.rows[0];
    expect(team10.chips.map((c) => c.text)).toEqual(["via a team"]);
    expect(team10.teams).toEqual(["platform-team"]);
    const draft = m.mine.find((g) => g.bucket === "draft")!.rows[0];
    // The bucket chip and the state chip say the same: one chip.
    expect(draft.chips).toEqual([{ text: "draft", tone: "warn", title: "Still a draft" }]);
    // Its panel action is its own; its section folds in the row.
    expect(draft.primary.hasActions).toBe(true);
    expect(draft.primary.actionIds).toEqual(["checks"]);
    expect(draft.sections.map((s) => [s.id, s.title, s.rows.length, s.more?.label])).toEqual([["checks", "Checks", 2, "Show skipped"]]);
    expect(m.mine.find((g) => g.bucket === "ready")!.rows[0].sections).toEqual([]);
    // The view-level banner, with its source.
    expect(m.banners.map((b) => [b.sourceId, b.banner.id, b.banner.actions?.map((a) => a.id)])).toEqual([["fx", "review-sync", ["resync"]]]);
  });

  it("a closed review is in no tab; an open one carries its bucket chip beside its vote", () => {
    const m = buildPrsModel([payload("fx", prsBody([review("#9", "voted", { state: "closed", me: { vote: "rejected" } })]))]);
    expect(m.counts).toEqual({ mine: 0, reviewing: 0, all: 0 });
    const closed = buildPrsModel([payload("fx", prsBody([review("#9", "voted", { me: { vote: "rejected" } })]))]).reviewing[0].rows[0];
    expect(closed.chips.map((c) => c.text)).toEqual(["you voted", "you rejected"]);
  });

  it("the Needs you rows: yes rows counted, waiting folded, the rest out", () => {
    const { yes, folded } = prsNeedingYou(buildPrsModel([payload("fx", fixture("prs"))]));
    expect(yes.map((r) => r.number)).toEqual(["#1", "#2", "#3", "#6", "#7"]);
    expect(folded.map((r) => r.number)).toEqual(["#4"]);
    expect(needsYouOf("mine", "recent")).toBe("no");
    expect(needsYouOf("reviewing", "action")).toBe("no");
    expect(needsYouOf("mine", "brand-new-bucket")).toBe("no");
    expect(BUCKETS.mine.map((b) => b.needsYou)).toEqual(["yes", "yes", "yes", "folded", "no"]);
    expect(BUCKETS.reviewing.map((b) => b.needsYou)).toEqual(["yes", "required", "no", "no", "no"]);
  });
});

describe("which PRs need the user", () => {
  // Every bucket against how I am on the PR: required by name, optional
  // (named, not required) and team-only. My own PRs have no `me`.
  const cases: [role: "mine" | "reviewing", bucket: string, how: "required" | "optional" | "team" | "mine", want: "yes" | "folded" | "no"][] = [
    ["mine", "action", "mine", "yes"],
    ["mine", "draft", "mine", "yes"],
    ["mine", "ready", "mine", "yes"],
    ["mine", "waiting", "mine", "folded"],
    ["mine", "recent", "mine", "no"],
    ["reviewing", "replied", "required", "yes"],
    ["reviewing", "replied", "optional", "yes"],
    ["reviewing", "vote", "required", "yes"],
    ["reviewing", "vote", "optional", "no"],
    ["reviewing", "vote", "team", "no"],
    ["reviewing", "mythreads", "required", "no"],
    ["reviewing", "mythreads", "optional", "no"],
    ["reviewing", "voted", "required", "no"],
    ["reviewing", "voted", "optional", "no"],
    ["reviewing", "group", "team", "no"],
    ["reviewing", "group", "required", "no"],
    ["reviewing", "later-bucket", "required", "no"],
  ];
  it.each(cases)("%s / %s / %s → %s", (role, bucket, how, want) => {
    const me = how === "mine" ? null : { required: how === "required", vote: "none" };
    const reviewers = how === "team" ? [team("web-team")] : [];
    const row = buildPrsModel([payload("a", prsBody([pr("#1", { role, bucket, me, reviewers })]))]);
    const all = [...row.mine, ...row.reviewing, ...row.all].flatMap((g) => g.rows);
    expect(all[0].attention).toBe(want);
    const { yes } = prsNeedingYou(row);
    expect(yes.length).toBe(want === "yes" ? 1 : 0);
  });

  it("a review that is no longer open never needs the user", () => {
    expect(attentionOf({ role: "reviewing", bucket: "replied", required: true, state: "merged" })).toBe("no");
    expect(attentionOf({ role: "reviewing", bucket: "vote", required: true, state: "draft" })).toBe("yes");
    expect(attentionOf({ role: "mine", bucket: "ready", required: false, state: "open" })).toBe("yes");
  });
});

describe("the three tabs", () => {
  it("Reviewing holds required-by-name and engaged reviews; All holds every open review", () => {
    const m = buildPrsModel([
      payload("a", prsBody([
        review("#1", "vote", { me: { required: true, vote: "none" } }),
        review("#2", "vote", { me: { required: false, vote: "none" } }),
        review("#3", "mythreads", { me: { required: false, vote: "none" } }),
        review("#4", "voted", { me: { required: false, vote: "approved" } }),
        review("#5", "replied", { me: { required: false, vote: "waiting" } }),
        review("#6", "group", { me: null, reviewers: [team("web-team")] }),
        review("#7", "voted", { state: "draft", me: { required: true, vote: "approved" } }),
        pr("#8", { bucket: "waiting" }),
      ])),
    ]);
    expect(ids(m, "mine")).toEqual(["#8"]);
    // Optional and not engaged (#2) and team-only (#6) stay out of Reviewing.
    expect(m.reviewing.map((g) => [g.id, g.rows.map((r) => r.number)])).toEqual([
      ["replied", ["#5"]],
      ["vote", ["#1"]],
      ["mythreads", ["#3"]],
      ["voted", ["#4", "#7"]],
    ]);
    expect(m.counts).toEqual({ mine: 1, reviewing: 5, all: 7 });
    expect(m.all.map((g) => [g.id, g.rows.map((r) => r.number)])).toEqual([
      ["required", ["#1", "#7"]],
      ["optional", ["#5", "#2", "#3", "#4"]],
      ["team:web-team", ["#6"]],
    ]);
  });

  it("All: one folded group per team in name order, the first team named, the others as chips, an unnamed team last", () => {
    const m = buildPrsModel([
      payload("a", prsBody([
        review("#1", "group", { reviewers: [team("web-team"), { name: "Alex", required: true }, team("db-team")] }),
        review("#2", "group", { reviewers: [team("api-team")] }),
        review("#3", "group", { reviewers: [] }),
        review("#4", "group", { reviewers: [team("Web-Team")] }),
        review("#5", "replied", { me: { required: false, vote: "none" }, reviewers: [team("web-team")] }),
      ])),
    ]);
    expect(m.all.map((g) => [g.id, g.label, g.folded, g.rows.map((r) => r.number)])).toEqual([
      ["optional", "Optional", false, ["#5"]],
      ["team:api-team", "api-team", true, ["#2"]],
      // Team names fold case; the first spelling labels the group.
      ["team:web-team", "web-team", true, ["#1", "#4"]],
      ["team", "Through a team", true, ["#3"]],
    ]);
    const multi = m.all[2].rows[0];
    expect(multi.chips.map((c) => c.text)).toEqual(["via a team", "also via db-team"]);
    // A PR named through a team AND required of me by name is Required, no team chips.
    const both = buildPrsModel([payload("a", prsBody([review("#9", "vote", { me: { required: true, vote: "none" }, reviewers: [team("web-team"), team("db-team")] })]))]);
    expect(both.all.map((g) => g.id)).toEqual(["required"]);
    expect(both.all[0].rows[0].chips.map((c) => c.text)).toEqual(["vote asked", "required"]);
  });

  it("one PR required by name in one source and team-only in another is one row; the fresher payload's `me` wins", () => {
    const named = prsBody([review("#1", "vote", { me: { required: true, vote: "none" } })], "2026-01-15T10:00:00Z");
    const viaTeam = prsBody([review("#1", "group", { me: null, reviewers: [team("web-team")] })], "2026-01-15T09:00:00Z");
    for (const order of [[payload("a", named), payload("b", viaTeam)], [payload("b", viaTeam), payload("a", named)]]) {
      const m = buildPrsModel(order);
      expect(m.counts).toEqual({ mine: 0, reviewing: 1, all: 1 });
      const row = m.reviewing[0].rows[0];
      expect([row.required, row.bucket, row.attention, row.primary.sourceId]).toEqual([true, "vote", "yes", "a"]);
      expect(row.origins.map((o) => o.sourceId)).toEqual(["a", "b"]);
      expect(m.all.map((g) => g.id)).toEqual(["required"]);
      expect(prsNeedingYou(m).yes.map((r) => r.id)).toEqual([row.id]);
    }
    // The other way round: the team-only payload is fresher, so the row is
    // team-only and needs nothing, though the other source named me.
    const stale = prsBody([review("#1", "vote", { me: { required: true, vote: "none" } })], "2026-01-15T08:00:00Z");
    const m = buildPrsModel([payload("a", stale), payload("b", viaTeam)]);
    expect(m.counts).toEqual({ mine: 0, reviewing: 0, all: 1 });
    expect(m.all.map((g) => g.id)).toEqual(["team:web-team"]);
    expect(prsNeedingYou(m).yes).toEqual([]);
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
    expect(row.chips).toEqual([{ text: "blocked on me", tone: "bad", title: "Blocked on me" }]);
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
    // Not required and not engaged: in All (as optional), not in Reviewing.
    expect(m.reviewing).toEqual([]);
    expect(m.all.map((g) => [g.id, g.rows.map((r) => r.bucket)])).toEqual([["optional", ["other"]]]);
    // An empty title shows the repo and number instead; the raw bucket is its chip.
    expect(m.all[0].rows[0].title).toBe("grp/app!7");
    expect(m.all[0].rows[0].chips).toEqual([{ text: "later-bucket", tone: "muted" }]);
    expect(prsNeedingYou(m).yes).toEqual([]);
    const required = buildPrsModel([payload("a", prsBody([{ ...bare, me: { required: true } }]))]);
    expect(required.reviewing.map((g) => [g.bucket, g.label])).toEqual([["other", "Other"]]);
  });

  it("a payload with no items, or none at all yet, renders empty without throwing", () => {
    expect(buildPrsModel([payload("a", { freshness: { asOf: null } })]).counts).toEqual({ mine: 0, reviewing: 0, all: 0 });
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

describe("the table's columns of a PR", () => {
  const one = (over: Record<string, unknown>) => buildPrsModel([payload("a", prsBody([pr("#1", over)]))]);
  const rowOf = (m: PrsModel) => [...m.mine, ...m.reviewing, ...m.all].flatMap((g) => g.rows)[0];

  it("status holds the bucket, the state, checks, conflicts and the source's badges; threads and votes have their own columns", () => {
    const row = rowOf(
      one({
        state: "draft",
        bucket: "draft",
        checks: { state: "failing", failing: ["lint"] },
        mergeable: "conflicts",
        threads: { active: 3, resolved: 2, needsMe: 1 },
        reviewers: [{ name: "Sam", vote: "approved" }],
        badges: [{ text: "hotfix", tone: "warn" }],
      }),
    );
    expect(row.status.map((c) => c.text)).toEqual(["draft", "1 check failing", "conflicts", "hotfix"]);
    expect(row.comments).toEqual({ text: "3 active / 2 resolved", forMe: 1 });
    expect(row.votes.summary).toBe("1 approved");
    expect(row.votes.me).toBeNull();
    // The flat chip list the other consumers read is unchanged.
    expect(row.chips.map((c) => c.text)).toEqual(["draft", "1 check failing", "1 thread for me", "conflicts", "1 approved", "hotfix"]);
  });

  it("votes: a summary of the others, my own vote on a review, who voted what as the tooltip", () => {
    const v = votesOf(
      readPr(
        pr("#1", {
          role: "reviewing",
          bucket: "vote",
          me: { required: true, vote: "none" },
          reviewers: [
            { name: "Alex", vote: "approved" },
            { name: "Sam", vote: "rejected", required: true },
            { name: "web-team", isGroup: true },
          ],
        }),
      )!,
    );
    expect(v).toEqual({
      summary: "1 approved · 1 rejected · 1 required",
      me: "me: no vote (required)",
      tone: "bad",
      title: "Alex: approved\nSam: rejected, required\nweb-team (team): no vote",
    });
    expect(votesOf(readPr(pr("#2"))!)).toMatchObject({ summary: "0 approved", me: null, tone: "muted" });
  });

  it("a reason is left to the chips when every fact it names is one of theirs", () => {
    const chips = [{ text: "draft", tone: "warn" as const }, { text: "conflicts", tone: "bad" as const }, { text: "1 check failing", tone: "bad" as const }];
    expect(restatesChips("still a draft", chips)).toBe(true);
    expect(restatesChips("conflicts with main", chips)).toBe(true);
    expect(restatesChips("Checks are failing: lint", chips)).toBe(true);
    // A fact no chip has, or nothing recognisable, keeps the sentence.
    expect(restatesChips("draft, waiting on Sam", chips)).toBe(false);
    expect(restatesChips("the release branch is frozen", chips)).toBe(false);
    const row = rowOf(one({ state: "draft", bucket: "draft", reasons: ["still a draft", "the release branch is frozen"] }));
    expect(row.why).toEqual(["the release branch is frozen"]);
    expect(row.reasons).toEqual(["still a draft", "the release branch is frozen"]);
  });

  it("the age is the last move, else the opening, else the close; the project only where the forge has one", () => {
    expect(rowOf(one({ created: "2026-01-01T00:00:00Z", lastTouchedUtc: "2026-01-10T00:00:00Z" })).ageUtc).toBe("2026-01-10T00:00:00Z");
    expect(rowOf(one({ created: "2026-01-01T00:00:00Z" })).ageUtc).toBe("2026-01-01T00:00:00Z");
    expect(rowOf(one({ created: 7 })).ageUtc).toBeNull();
    expect(rowOf(one({ project: "platform" })).project).toBe("platform");
    expect(rowOf(one({ project: null })).project).toBeNull();
  });
});
