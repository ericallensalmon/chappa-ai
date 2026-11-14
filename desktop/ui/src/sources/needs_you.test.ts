import { describe, expect, it } from "vitest";
import { buildPrsModel } from "./prs_model";
import {
  SUMMARY_AT,
  SeenBaselines,
  buildNeedsYou,
  newsFor,
  pollResult,
  workAttention,
  type NeedsYouInput,
  type TerminalAttention,
} from "./needs_you";
import { fixture, payload, pr, prsBody } from "./test-utils";

const empty: NeedsYouInput = { terminals: [], work: [], prs: null, attention: [] };
const term = (key: string, over: Partial<TerminalAttention> = {}): TerminalAttention => ({
  key,
  name: key,
  project: null,
  attention: "badge",
  programStatus: null,
  ...over,
});

describe("buildNeedsYou", () => {
  it("nothing in, nothing to show, a zero badge", () => {
    expect(buildNeedsYou(empty)).toMatchObject({ sections: [], count: 0, needsUpdate: [] });
  });

  it("terminals: waiting and blocked outrank a bell, done/failed read at bell rank, titles lead with the name", () => {
    const m = buildNeedsYou({
      ...empty,
      terminals: [
        term("term:1", { name: "build", attention: "badge", programStatus: "failed" }),
        term("term:2", { name: "claude", attention: "agent-waiting", project: "api" }),
        term("cmd:1:tests", { name: "tests", attention: "agent-waiting", programStatus: "blocked" }),
        term("term:3", { name: "shell", attention: "badge" }),
      ],
    });
    expect(m.sections.map((s) => s.id)).toEqual(["terminals"]);
    const rows = m.sections[0].rows;
    expect(rows.map((r) => [r.title, r.detail])).toEqual([
      ["claude", "waiting for your input · api"],
      ["tests", "blocked, waiting for you"],
      ["build", "reported failed"],
      ["shell", "needs attention"],
    ]);
    expect(rows.map((r) => r.terminalKey)).toEqual(["term:2", "cmd:1:tests", "term:1", "term:3"]);
    // All four are listed; only the waiting and blocked ones count.
    expect(rows.map((r) => r.counted)).toEqual([true, true, false, false]);
    expect(m.count).toBe(2);
  });

  it("the badge counts waiting / blocked terminals, approvals, PRs and source attention, never a bell", () => {
    const bells = buildNeedsYou({
      ...empty,
      terminals: [
        term("term:1", { attention: "badge" }),
        term("term:2", { attention: "badge", programStatus: "done" }),
        term("term:3", { attention: "badge", programStatus: "failed" }),
      ],
    });
    // Listed, so they can be found and cleared, but nothing to count.
    expect(bells.sections[0].rows.length).toBe(3);
    expect(bells.count).toBe(0);
    const mixed = buildNeedsYou({
      terminals: [term("term:1", { attention: "badge" }), term("term:2", { attention: "agent-waiting", programStatus: "blocked" })],
      work: [payload("fx", fixture("work"))],
      prs: buildPrsModel([
        payload("p", prsBody([
          pr("#1", { bucket: "ready" }),
          pr("#2", { bucket: "waiting" }),
          pr("#3", { role: "reviewing", bucket: "vote", me: { required: true, vote: "none" } }),
          pr("#4", { role: "reviewing", bucket: "vote", me: { required: false, vote: "none" } }),
          pr("#5", { role: "reviewing", bucket: "group", me: null, reviewers: [{ name: "web-team", isGroup: true }] }),
          pr("#6", { role: "reviewing", bucket: "mythreads", me: { required: true, vote: "none" } }),
        ])),
      ]),
      attention: [payload("fx", fixture("attention"))],
    });
    const prRows = mixed.sections.find((s) => s.id === "prs")!.rows;
    // The optional vote, the team-only review and my commented one are not Needs you rows.
    expect(prRows.map((r) => [r.title, r.folded, r.counted])).toEqual([
      ["Change #1", false, true],
      ["Change #3", false, true],
      ["Change #2", true, false],
    ]);
    // 1 blocked terminal + 1 work approval + 2 PRs + 4 attention items.
    expect(mixed.count).toBe(8);
  });

  it("the fixture: work approvals and human-exit gates, yes PRs, waiting folded, attention by group", () => {
    const prs = buildPrsModel([payload("fx", fixture("prs"))]);
    const m = buildNeedsYou({
      terminals: [],
      work: [payload("fx", fixture("work"), { sourceLabel: "Fixture" })],
      prs,
      attention: [payload("fx", fixture("attention"), { sourceLabel: "Fixture" })],
    });
    expect(m.sections.map((s) => [s.id, s.title])).toEqual([
      ["work", "Work"],
      ["prs", "Pull requests"],
      ["group:Builds", "Builds"],
      ["group:Incidents", "Incidents"],
    ]);
    const work = m.sections[0].rows;
    expect(work.map((r) => [r.id, r.title, r.detail])).toEqual([
      ["work:fx|ABC-1", "ABC-1 Add a health route to api", "approve the move to done, asked by Alex: Looks complete"],
    ]);
    expect(work[0].actions).toEqual([{ sourceId: "fx", sourceLabel: "Fixture", view: "work", key: "ABC-1" }]);
    expect(work[0].opens).toEqual({ url: "https://registry.example/tracker/ABC-1" });
    const prRows = m.sections[1].rows;
    expect(prRows.map((r) => [r.title, r.folded])).toEqual([
      ["Change 1", false],
      ["Change 2", false],
      ["Change 3", false],
      ["Change 6", false],
      ["Change 7", false],
      ["Change 4", true],
    ]);
    expect(prRows[0].detail).toBe("owner/repo#1 · Checks are failing: lint");
    expect(prRows[0].actions).toEqual([{ sourceId: "fx", sourceLabel: "fx", view: "prs", key: "github:owner/repo:#1" }]);
    expect(prRows[1].actions).toEqual([]);
    const builds = m.sections[2].rows;
    expect(builds.map((r) => [r.title, r.tone, r.opens])).toEqual([
      ["Nightly build failed", "bad", { url: "https://registry.example/owner/repo/actions/runs/1" }],
      ["ABC-2 needs a decision", "info", { view: "work", key: "ABC-2" }],
    ]);
    expect(m.sections[3].rows.map((r) => [r.title, r.opens])).toEqual([
      ["Elevated error rate on api", { view: "environment", key: "api" }],
      ["Certificate expires soon", null],
    ]);
    expect(m.sections[3].rows[1].actions).toEqual([]);
    // 1 work + 5 PRs (waiting not counted) + 4 attention.
    expect(m.count).toBe(10);
  });

  it("a human-exit gate without a pending approval still needs a person", () => {
    const w = workAttention({ key: "ABC-9", title: null, localStatus: { id: "qa", label: "In QA", gate: "human-exit", allowed: [] } });
    expect(w).toEqual({ key: "ABC-9", title: "ABC-9", detail: "In QA: only a person can move it on", url: null });
    expect(workAttention({ key: "ABC-8", localStatus: { id: "x", label: "X", gate: "human", allowed: [] } })).toBeNull();
    expect(workAttention({ key: "ABC-7", localStatus: null })).toBeNull();
    expect(workAttention({ title: "no key", localStatus: { gate: "human-exit" } })).toBeNull();
  });

  it("markdown links are stripped from attention titles and details", () => {
    const m = buildNeedsYou({
      ...empty,
      attention: [payload("s", { freshness: { asOf: null }, items: [{ id: "a", title: "[Deploy 12](https://registry.example/d/12) failed", detail: "see\n[logs](https://registry.example/l)", group: "Deploys" }] })],
    });
    expect(m.sections[0].rows[0]).toMatchObject({ title: "Deploy 12 failed", detail: "see logs" });
  });

  it("wrong contract versions are banners and their rows stay out", () => {
    const m = buildNeedsYou({
      ...empty,
      attention: [payload("new", { contract: 9, freshness: { asOf: null }, items: [{ id: "x", title: "x", detail: "", group: "g" }] })],
      needsUpdate: [{ sourceId: "old", sourceLabel: "Old" }, { sourceId: "new", sourceLabel: "new" }],
    });
    expect(m.sections).toEqual([]);
    expect(m.needsUpdate.map((s) => s.sourceId)).toEqual(["old", "new"]);
  });
});

describe("the notification rules", () => {
  const items = (...ids: string[]) => ({ ok: true as const, items: ids.map((id) => ({ id, name: `item ${id}`, body: `body ${id}` })) });

  it("the first successful poll sets the baseline silently; only ids not seen before are news", () => {
    const b = new SeenBaselines();
    expect(b.update("s", "attention", items("a", "b"))).toEqual([]);
    expect(b.has("s", "attention")).toBe(true);
    expect(b.update("s", "attention", items("a", "b"))).toEqual([]);
    expect(b.update("s", "attention", items("a", "b", "c"))).toEqual([{ title: "item c", body: "body c", ids: ["c"] }]);
    // Baselines are per (source, view): the same id elsewhere is a first poll.
    expect(b.update("s", "work", items("c"))).toEqual([]);
    expect(b.update("t", "attention", items("c"))).toEqual([]);
  });

  it("a failed poll keeps the previous ids; a first poll that failed sets nothing", () => {
    const b = new SeenBaselines();
    expect(b.update("s", "prs", { ok: false })).toEqual([]);
    expect(b.has("s", "prs")).toBe(false);
    b.update("s", "prs", items("a", "b"));
    expect(b.update("s", "prs", { ok: false })).toEqual([]);
    expect(b.update("s", "prs", items("a", "b"))).toEqual([]);
  });

  it("a partial poll forgets nothing, so a recovering search does not replay its items", () => {
    const b = new SeenBaselines();
    b.update("s", "prs", items("a", "b"));
    expect(b.update("s", "prs", { ...items("a"), partial: true })).toEqual([]);
    expect(b.update("s", "prs", items("a", "b"))).toEqual([]);
    // A clean poll does forget: an item that left and came back is news.
    b.update("s", "prs", items("a"));
    expect(b.update("s", "prs", items("a", "b")).map((n) => n.ids)).toEqual([["b"]]);
  });

  it(`${SUMMARY_AT} or more new items in one update collapse into one summary that leads with an item name`, () => {
    const b = new SeenBaselines();
    b.update("s", "attention", items());
    expect(b.update("s", "attention", items("1", "2", "3", "4"))).toHaveLength(4);
    const summary = b.update("s", "attention", items("1", "2", "3", "4", "5", "6", "7", "8", "9"));
    expect(summary).toEqual([{ title: "item 5 and 4 more need you", body: "item 6, item 7, item 8, …", ids: ["5", "6", "7", "8", "9"] }]);
    expect(newsFor([{ id: "x", name: "X", body: "" }])).toEqual([{ title: "X", body: "", ids: ["x"] }]);
    expect(newsFor(["a", "b", "c", "d", "e"].map((id) => ({ id, name: id.toUpperCase(), body: "" })))[0]).toMatchObject({
      title: "A and 4 more need you",
      body: "B, C, D, …",
    });
  });

  it("an item another source already announced joins the baseline quietly", () => {
    const b = new SeenBaselines();
    b.update("a", "prs", items());
    b.update("b", "prs", items());
    expect(b.update("a", "prs", items("pr-1"))).toHaveLength(1);
    expect(b.update("b", "prs", items("pr-1"), (id) => b.seenElsewhere("prs", id, "b"))).toEqual([]);
    expect(b.seenElsewhere("prs", "pr-1", "a")).toBe(true);
    expect(b.seenElsewhere("attention", "pr-1", "b")).toBe(false);
  });

  it("forgetting a source makes its next poll a first one", () => {
    const b = new SeenBaselines();
    b.update("s", "attention", items("a"));
    b.forget("s");
    expect(b.update("s", "attention", items("a", "b"))).toEqual([]);
  });
});

describe("pollResult", () => {
  it("reads the stable ids per view: attention id, work key, PR forge/repo/number of yes buckets only", () => {
    const att = pollResult("attention", payload("fx", fixture("attention")));
    expect(att.ok && att.items.map((i) => i.id)).toEqual(["nightly-build-1", "incident-1", "review-note-1", "plain-1"]);
    const work = pollResult("work", payload("fx", fixture("work")));
    expect(work.ok && work.items).toEqual([{ id: "ABC-1", name: "ABC-1 Add a health route to api", body: "approve the move to done, asked by Alex: Looks complete" }]);
    const p = payload("fx", fixture("prs"));
    const prs = pollResult("prs", p, buildPrsModel([p]));
    expect(prs.ok && prs.items.map((i) => i.id)).toEqual([
      "github/owner/repo/#1",
      "github/owner/repo/#2",
      "github/owner/repo/#3",
      "github/owner/repo/#6",
      "github/owner/repo/#7",
    ]);
    expect(prs.ok && prs.items[0]).toEqual({ id: "github/owner/repo/#1", name: "Change 1", body: "owner/repo#1 · Checks are failing: lint" });
  });

  it("a failed poll, a missing payload or another contract is a failure; source errors make it partial", () => {
    expect(pollResult("attention", payload("s", null))).toEqual({ ok: false });
    expect(pollResult("attention", payload("s", fixture("attention"), { freshness: { asOf: null, errors: [{ source: "s", message: "down" }] } }))).toEqual({ ok: false });
    expect(pollResult("attention", payload("s", { contract: 2, items: [] }))).toEqual({ ok: false });
    const partial = pollResult("prs", payload("s", prsBody([pr("#1")], null, { errors: [{ source: "s:search", message: "x" }] })));
    expect(partial).toEqual({ ok: true, items: [], partial: true });
  });
});
