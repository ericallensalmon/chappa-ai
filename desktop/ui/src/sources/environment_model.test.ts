import { describe, expect, it } from "vitest";
import {
  STATE_ORDER,
  buildEnvironmentModel,
  groupCountText,
  headerCounts,
  problemsText,
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
    expect(byId.get("db")!.alerts).toEqual([{ text: "db is unhealthy", severity: "bad", componentId: "db", state: false }]);
    expect(byId.get("queue")!.details).toEqual(["7 restarts", "exit 1"]);
    expect(byId.get("migrate")!.oneshot).toBe(true);
    expect(byId.get("api")!.oneshot).toBe(false);
    expect(m.blocks[0].alerts.length).toBe(3);
    // Facts and banners are read by the shared section reader: `more` stays
    // folded behind its label, a banner keeps its actions (the view's own).
    expect(m.blocks[0].facts).toEqual([
      {
        id: "runtimes",
        title: "Runtimes",
        collapsed: false,
        rows: [{ label: "Node", value: "22" }, { label: "Memory", value: "6.1 GiB free" }],
        more: { label: "Show disks", rows: [{ label: "Disk", value: "40 GiB free" }] },
      },
    ]);
    expect(m.blocks[0].banners).toEqual([
      {
        id: "stale-images",
        tone: "warn",
        title: "Images are out of date",
        lines: ["Pull to update"],
        actions: [{ kind: "endpoint", id: "check-images", label: "Check again", mutates: false }],
      },
    ]);
    // A component's sections: its own actions apart from theirs.
    expect(api.sections.map((s) => s.id)).toEqual(["config"]);
    expect(api.actionIds).toEqual(["ping"]);
    expect(byId.get("web")!.sections).toEqual([]);
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
    expect(b.alerts).toEqual([{ text: "loud", severity: "warn", componentId: null, state: false }]);
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

describe("environment model: the dense table", () => {
  it("chips carry each state's tone, then the endpoints that answer", () => {
    const b = buildEnvironmentModel([
      env([
        { id: "a", group: "g", state: "up", probe: { url: "https://x.test/h", rule: "health", up: true, status: 200 } },
        { id: "b", group: "g", state: "stopped-problem", probe: { url: "https://x.test/h2", rule: "health", up: true, status: 200 } },
        { id: "c", group: "g", state: "not-deployed" },
      ]),
    ]).blocks[0];
    expect(b.chips.map((c) => `${c.tone}:${c.text}`)).toEqual(["ok:1 up", "bad:1 deployed but not running", "muted:1 not deployed", "ok:endpoints 2/2"]);
    // The text-only counts leave the endpoints out.
    expect(b.header).toEqual(["1 up", "1 deployed but not running", "1 not deployed"]);
    const none = buildEnvironmentModel([env([{ id: "a", group: "g", state: "up", probe: { url: "https://x.test/h", rule: "web", up: false, status: null } }])]).blocks[0];
    expect(none.chips.at(-1)).toEqual({ text: "endpoints 0/1", tone: "bad", title: "0 of 1 checked endpoints answer" });
    const unprobed = buildEnvironmentModel([env([{ id: "a", group: "g", state: "up" }])]).blocks[0];
    expect(unprobed.chips.map((c) => c.text)).toEqual(["1 up"]);
  });

  it("a row's columns: uptime, mode with a short commit and ref, and the endpoint pill", () => {
    const r = rows(buildEnvironmentModel([fx()]).blocks[0]);
    const api = r.find((x) => x.id === "api")!;
    expect([api.uptime, api.source?.mode, api.sourceLine]).toEqual(["3h 12m", "dev", "1a2b3c4 abc-1-health"]);
    expect(api.endpoint.text).toBe("answers");
    expect(r.find((x) => x.id === "web")!.endpoint).toMatchObject({ text: "no answer", tone: "bad" });
    expect(r.find((x) => x.id === "queue")!.endpoint).toMatchObject({ text: "–", tone: "muted" });
    expect(r.find((x) => x.id === "queue")!.sourceLine).toBe("");
  });

  it("reads the stack-wide and the groups' action ids, and the facts as one line", () => {
    const b = buildEnvironmentModel([
      env([{ id: "a", group: "g", state: "up" }], {
        actions: [{ kind: "endpoint", id: "up", label: "Up" }, { label: "no id" }],
        groups: [{ id: "g", label: "G", actions: [{ kind: "endpoint", id: "app:up", label: "Up" }] }],
        facts: [
          { id: "stack", title: "Stack", rows: [{ label: "Slot", value: "0" }, { label: "Host" }] },
          { id: "quiet", title: "Quiet", collapsed: true, rows: [{ label: "Hidden", value: "x" }] },
        ],
      }),
    ]).blocks[0];
    expect(b.actionIds).toEqual(["up"]);
    expect(b.groups[0].actionIds).toEqual(["app:up"]);
    expect(b.factsLine).toEqual([
      { label: "Slot", value: "0", section: "Stack", tone: null },
      { label: "Host", value: "", section: "Stack", tone: null },
    ]);
    expect(buildEnvironmentModel([fx()]).blocks[0].actionIds).toEqual(["bring-up"]);
    expect(buildEnvironmentModel([fx()]).blocks[0].groups.map((g) => g.actionIds)).toEqual([[], [], ["run-jobs"]]);
  });

  it("a group's count is up out of deployed, else how many are not deployed", () => {
    const r = (state: string, id: string) => ({ id, group: "g", state });
    const g = (components: unknown[]) => groupCountText(rows(buildEnvironmentModel([env(components)]).blocks[0]));
    expect(g([r("up", "a"), r("up", "b"), r("stopped-idle", "c"), r("not-deployed", "d")])).toBe("(2/3 up)");
    expect(g([r("not-deployed", "a"), r("not-deployed", "b")])).toBe("(2 not deployed)");
  });
});

describe("environment model: the header's parts", () => {
  const facts = [
    {
      id: "stack",
      kind: "meta",
      title: "Stack env · slot 0",
      rows: [
        { label: "Instance", value: "env-0" },
        { label: "Bound item", value: "none" },
      ],
    },
    {
      id: "engines",
      title: "Engines",
      rows: [
        { label: "docker", value: "v29", tone: "ok" },
        { label: "podman", value: "not reachable (off)", tone: "bad" },
        { label: "relay", value: "8080", badges: [{ text: "problem", tone: "bad" }] },
        { label: "Memory", value: "needs 2 GB", tone: "loud" },
      ],
    },
    { id: "data-safety", title: "Data safety", status: { label: "Data", text: "clean", tone: "ok" }, rows: [{ label: "State", value: "clean" }] },
    { id: "secret-scan", title: "Secret scan", status: { label: "Scan", text: "possible secrets", tone: "bad" }, rows: [{ label: "api", value: "1" }] },
    { id: "odd", title: "Odd", status: { label: "Odd", text: "?", tone: "purple" }, rows: [] },
    { id: "half", title: "Half", status: { label: "Half" }, rows: [{ label: "x", value: "y" }] },
  ];

  it("runner metadata is apart from the facts; facts carry their tone; statuses are badges", () => {
    const b = buildEnvironmentModel([env([{ id: "a", group: "g", state: "up" }], { facts })]).blocks[0];
    expect(b.meta.map((m) => `${m.label}=${m.value}`)).toEqual(["Instance=env-0", "Bound item=none"]);
    expect(b.factsLine.map((f) => `${f.tone}:${f.label}`)).toEqual([
      "ok:docker",
      "bad:podman",
      // no tone of its own: its loudest badge
      "bad:relay",
      // an unknown tone reads neutral
      "null:Memory",
      // a status without both label and text is not a status: its rows stay facts
      "null:x",
    ]);
    expect(b.statuses.map((s) => `${s.tone}:${s.text}`)).toEqual(["ok:Data: clean ✓", "bad:Scan: possible secrets ✗", "muted:Odd: ?"]);
    expect(b.statuses[1].section.rows).toEqual([{ label: "api", value: "1" }]);
    // Every section, meta and status ones too, stays on the details page.
    expect(b.facts.map((s) => s.id)).toEqual(["stack", "engines", "data-safety", "secret-scan", "odd", "half"]);
  });

  it("a source without the marks reads as before: every fact a chip, no meta, no statuses", () => {
    const plain = facts.map(({ id, title, rows }) => ({ id, title, rows }));
    const b = buildEnvironmentModel([env([], { facts: plain })]).blocks[0];
    expect(b.meta).toEqual([]);
    expect(b.statuses).toEqual([]);
    expect(b.factsLine.map((f) => f.label)).toEqual(["Instance", "Bound item", "docker", "podman", "relay", "Memory", "State", "api", "x"]);
  });

  it("red components are the problems, in table order, each '<label>: <state>'; state alerts are marked", () => {
    const comps = [
      { id: "core", label: "Core", group: "g", state: "stopped-problem", hint: "exited 137" },
      { id: "ok", group: "g", state: "up" },
      { id: "web", group: "g", state: "unhealthy" },
      { id: "idle", group: "g", state: "stopped-idle" },
    ];
    const b = buildEnvironmentModel([
      env(comps, {
        alerts: [
          { text: "Core is deployed but not running: exited 137", severity: "bad", componentId: "core", kind: "state" },
          { text: "core: data not clean", severity: "bad", componentId: "core" },
        ],
      }),
    ]).blocks[0];
    expect(b.problems).toEqual([
      { id: "core", key: "s|core", text: "Core: deployed but not running", title: "exited 137" },
      { id: "web", key: "s|web", text: "web: unhealthy", title: "Running, but its probe or health check keeps failing" },
    ]);
    expect(b.alerts.map((a) => a.state)).toEqual([true, false]);
  });

  it("the problem text names the first few, then how many more", () => {
    const p = (n: number) => Array.from({ length: n }, (_, i) => ({ id: `c${i}`, key: `s|c${i}`, text: `c${i}: unhealthy`, title: "" }));
    expect(problemsText([])).toBe("");
    expect(problemsText(p(1))).toBe("c0: unhealthy");
    expect(problemsText(p(3))).toBe("c0: unhealthy · c1: unhealthy · c2: unhealthy");
    expect(problemsText(p(4))).toBe("c0: unhealthy · c1: unhealthy · c2: unhealthy and 1 more");
    expect(problemsText(p(56))).toMatch(/and 53 more$/);
  });
});
