import { describe, expect, it } from "vitest";
import {
  STATE_ORDER,
  buildEnvironmentModel,
  headerCounts,
  readProbe,
  readSourceCell,
  stateInfo,
  worse,
  type EnvBlock,
  type EnvRow,
} from "./environment_model";
import { fixture, payload } from "./test-utils";

const fx = (over: Record<string, unknown> = {}) => payload("fx", { ...fixture("environment"), ...over }, { sourceLabel: "Fixture" });
const rows = (b: EnvBlock): EnvRow[] => b.groups.flatMap((g) => g.rows);
const env = (components: unknown[], extra: Record<string, unknown> = {}) =>
  payload("s", { freshness: { asOf: "2026-01-15T10:00:00Z" }, groups: [{ id: "g", label: "G" }], components, ...extra });

describe("environment model: states", () => {
  it.each([
    ["up", "ok", "up", false],
    ["starting", "warn", "starting", false],
    ["unhealthy", "bad", "unhealthy", true],
    ["crashloop", "bad", "crash loop", true],
    ["stopped-problem", "bad", "deployed but not running", true],
    ["stopped-idle", "muted", "stopped", false],
    ["oneshot-ok", "muted", "ran ok", false],
    ["oneshot-failed", "bad", "failed", true],
    ["not-deployed", "muted", "not deployed", false],
    ["melting", "muted", "unknown", false],
  ])("%s reads as tone %s, text %s, red %s", (state, tone, text, red) => {
    const info = stateInfo(state);
    expect([info.tone, info.text, info.red]).toEqual([tone, text, red]);
    const r = rows(buildEnvironmentModel([env([{ id: "x", group: "g", state }])]).blocks[0])[0];
    expect([r.state, r.tone, r.stateText, r.red]).toEqual([state, tone, text, red]);
  });

  it("the table covers exactly the nine contract states, in order", () => {
    expect(STATE_ORDER).toEqual(["up", "starting", "unhealthy", "crashloop", "stopped-problem", "stopped-idle", "oneshot-ok", "oneshot-failed", "not-deployed"]);
  });

  it("the exit code is a detail, never the colour: 137 and 0 alike under stopped-problem", () => {
    const b = buildEnvironmentModel([
      env([
        { id: "a", group: "g", state: "stopped-problem", exitCode: 137 },
        { id: "b", group: "g", state: "stopped-problem", exitCode: 0 },
        { id: "c", group: "g", state: "stopped-idle", exitCode: 137 },
      ]),
    ]).blocks[0];
    expect(rows(b).map((r) => [r.id, r.tone, r.details])).toEqual([
      ["a", "bad", ["exit 137"]],
      ["b", "bad", ["exit 0"]],
      ["c", "muted", ["exit 137"]],
    ]);
  });
});

describe("environment model: header counts", () => {
  it("uses the source's summary, non-zero only, in table order, with the contract's wording", () => {
    expect(headerCounts({ "stopped-idle": 2, up: 3, "stopped-problem": 1, starting: 0, "oneshot-ok": 4 }, [])).toEqual([
      "3 up",
      "1 deployed but not running",
      "2 stopped",
      "4 ran ok",
    ]);
  });

  it("counts from the components when the summary is absent; an unknown state is not counted", () => {
    const m = buildEnvironmentModel([
      env([
        { id: "a", group: "g", state: "up" },
        { id: "b", group: "g", state: "up" },
        { id: "c", group: "g", state: "stopped-problem" },
        { id: "d", group: "g", state: "oneshot-ok" },
        { id: "e", group: "g", state: "melting" },
      ]),
    ]);
    expect(m.blocks[0].header).toEqual(["2 up", "1 deployed but not running", "1 ran ok"]);
  });

  it("the fixture's summary names every state once", () => {
    expect(buildEnvironmentModel([fx()]).blocks[0].header).toEqual([
      "1 up", "1 starting", "1 unhealthy", "1 in a crash loop", "1 deployed but not running", "1 stopped", "1 ran ok", "1 failed", "1 not deployed",
    ]);
  });
});

describe("environment model: blocks, groups, cells", () => {
  it("one block per source, never merged, each labelled by its source", () => {
    const m = buildEnvironmentModel([fx(), payload("other", fixture("environment"), { sourceLabel: "Other" })]);
    expect(m.blocks.map((b) => b.sourceLabel)).toEqual(["Fixture", "Other"]);
    expect(rows(m.blocks[1]).map((r) => r.key)[0]).toBe("other|api");
    expect(m.red.length).toBe(8);
  });

  it("groups follow the source's order; a component naming no known group goes under a trailing Other", () => {
    const m = buildEnvironmentModel([
      payload("s", {
        groups: [{ id: "b", label: "Bee" }, { id: "a" }],
        components: [
          { id: "1", group: "a", state: "up" },
          { id: "2", group: "nope", state: "up" },
          { id: "3", group: "b", state: "up" },
          { id: "4", state: "up" },
        ],
      }),
    ]);
    expect(m.blocks[0].groups.map((g) => [g.label, g.rows.map((r) => r.id)])).toEqual([
      ["Bee", ["3"]],
      ["a", ["1"]],
      ["Other", ["2", "4"]],
    ]);
  });

  it("a fixture row: label, kind, details, source cell, probe cell, links with the browser one first, hint, badges, alerts", () => {
    const m = buildEnvironmentModel([fx()]);
    const byId = new Map(rows(m.blocks[0]).map((r) => [r.id, r]));
    const api = byId.get("api")!;
    expect(api.label).toBe("API");
    expect(api.details).toEqual(["up 3h 12m"]);
    expect(api.source).toMatchObject({ mode: "dev", text: "dev · abc-1-health @ 1a2b3c4" });
    expect(api.source!.title).toContain("working copy");
    expect(api.probe).toMatchObject({ up: true, rule: "health", text: "health up 200" });
    expect(api.links.map((l) => [l.label, l.browser])).toEqual([["Open", true], ["Docs", false]]);
    expect(api.hint).toBe("Serving from your working copy");
    expect(api.badges).toEqual([{ text: "dev", tone: "info" }]);
    expect(api.hasActions).toBe(true);
    expect(byId.get("web")!.probe).toMatchObject({ up: false, text: "web down: no response" });
    expect(byId.get("web")!.source!.text).toBe("source · main");
    expect(byId.get("db")!.probe).toMatchObject({ up: false, status: 503, text: "health down: 503 fails the health rule" });
    expect(byId.get("db")!.source).toMatchObject({ mode: "prebuilt", text: "prebuilt · registry.example/db:16" });
    expect(byId.get("db")!.alerts).toEqual([{ text: "db is unhealthy", severity: "bad", componentId: "db" }]);
    expect(byId.get("queue")!.details).toEqual(["7 restarts", "exit 1"]);
    expect(byId.get("migrate")!.oneshot).toBe(true);
    expect(byId.get("api")!.oneshot).toBe(false);
    expect(m.blocks[0].alerts.length).toBe(3);
    expect(m.blocks[0].facts).toEqual([{ id: "runtimes", title: "Runtimes", rows: [{ label: "Node", value: "22", badges: [] }, { label: "Memory", value: "6.1 GiB free", badges: [] }] }]);
    expect(m.blocks[0].banners).toEqual([{ id: "stale-images", tone: "warn", title: "Images are out of date", lines: ["Pull to update"] }]);
  });

  it("source cells: the commit is shortened and the mode explained", () => {
    expect(readSourceCell({ mode: "source", ref: "main", commit: "0123456789abcdef" })).toMatchObject({ text: "source · main @ 0123456", title: expect.stringContaining("commit 0123456789abcdef") });
    expect(readSourceCell({ mode: "prebuilt" })?.text).toBe("prebuilt");
    expect(readSourceCell({ ref: "main" })).toBeNull();
    expect(readSourceCell(null)).toBeNull();
  });

  it("probe cells: down says why, a rule defaults to health, a malformed probe reads as none", () => {
    expect(readProbe({ url: "https://dev.example.test/", rule: "web", up: true, status: 302 })?.text).toBe("web up 302");
    expect(readProbe({ url: "https://dev.example.test/", up: false, status: 404 })?.text).toBe("health down: 404 fails the health rule");
    expect(readProbe({ url: "https://dev.example.test/", rule: "any", up: false })?.text).toBe("any down: no response");
    expect(readProbe({ url: "https://dev.example.test/", rule: "web", up: false, status: 500 })?.title).toContain("passes on 2xx or 3xx");
    expect(readProbe({ url: "https://dev.example.test/" })).toBeNull();
    expect(readProbe({ up: true })).toBeNull();
  });

  it("counts the not-deployed components of each block", () => {
    expect(buildEnvironmentModel([fx()]).blocks[0].notDeployed).toBe(1);
  });
});

describe("environment model: alert dot", () => {
  it("worst of alerts and red components; starting does not dot", () => {
    expect(buildEnvironmentModel([fx()]).alertLevel).toBe("bad");
    expect(buildEnvironmentModel([env([{ id: "a", group: "g", state: "starting" }, { id: "b", group: "g", state: "up" }])]).alertLevel).toBeNull();
    expect(buildEnvironmentModel([env([{ id: "a", group: "g", state: "up" }], { alerts: [{ text: "Disk nearly full", severity: "warn" }] })]).alertLevel).toBe("warn");
    expect(buildEnvironmentModel([env([{ id: "a", group: "g", state: "up" }], { alerts: [{ text: "x", severity: "warn" }, { text: "y", severity: "bad" }] })]).alertLevel).toBe("bad");
    for (const state of ["unhealthy", "crashloop", "stopped-problem", "oneshot-failed"]) {
      expect(buildEnvironmentModel([env([{ id: "a", group: "g", state }])]).alertLevel, state).toBe("bad");
    }
    for (const state of ["stopped-idle", "oneshot-ok", "not-deployed", "melting"]) {
      expect(buildEnvironmentModel([env([{ id: "a", group: "g", state }])]).alertLevel, state).toBeNull();
    }
    expect(worse("warn", null)).toBe("warn");
    expect(worse(null, null)).toBeNull();
    expect(worse("warn", "bad")).toBe("bad");
  });
});

describe("environment model: lenient reads", () => {
  it("another contract is a needs-update banner; the other sources still show", () => {
    const m = buildEnvironmentModel([payload("old", { contract: 9, components: [] }, { sourceLabel: "Old" }), fx()]);
    expect(m.needsUpdate).toEqual([{ sourceId: "old", sourceLabel: "Old" }]);
    expect(m.blocks.map((b) => b.sourceId)).toEqual(["fx"]);
  });

  it("garbage fields read as absent; rows without an id are skipped and counted; a duplicate id too", () => {
    const m = buildEnvironmentModel([
      env(
        [
          { id: "a", group: "g", state: 7, uptime: 3, restarts: "many", exitCode: "1", source: "dev", probe: [], links: "x", badges: [{ text: "" }, { text: "ok", tone: "weird" }], actions: {} },
          { group: "g", state: "up" },
          "junk",
          { id: "a", group: "g", state: "up" },
        ],
        { summary: "nine", alerts: [{ severity: "bad" }, { text: "loud", severity: "huge" }], facts: [{ rows: [{ value: "no label" }] }, "x"], banners: [{ tone: "bad" }] },
      ),
    ]);
    const b = m.blocks[0];
    const r = rows(b)[0];
    expect([r.state, r.stateText, r.details, r.source, r.probe, r.links, r.hasActions]).toEqual(["unknown", "unknown", [], null, null, [], false]);
    expect(r.badges).toEqual([{ text: "ok", tone: "info" }]);
    expect(rows(b).length).toBe(1);
    expect(m.freshness.errors).toEqual(["3 components could not be read"]);
    expect(b.header).toEqual([]);
    expect(b.alerts).toEqual([{ text: "loud", severity: "warn", componentId: null }]);
    expect(b.facts).toEqual([]);
    expect(b.banners).toEqual([]);
  });

  it("a source with no payload yet is a block waiting for data", () => {
    const m = buildEnvironmentModel([payload("s", null, { sourceLabel: "S" })]);
    expect(m.hasData).toBe(false);
    expect(m.blocks[0]).toMatchObject({ sourceLabel: "S", hasData: false, groups: [] });
    expect(m.alertLevel).toBeNull();
  });

  it("a payload that is not an object, or has no components, does not crash", () => {
    expect(buildEnvironmentModel([payload("s", [1, 2])]).blocks[0].groups).toEqual([]);
    expect(buildEnvironmentModel([payload("s", { groups: "x" })]).blocks[0].groups).toEqual([]);
  });
});
