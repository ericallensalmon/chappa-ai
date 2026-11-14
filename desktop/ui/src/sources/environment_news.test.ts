// The Environment's share of Needs you (red components as attention rows)
// and of the notification rules (a component entering a red state, a new bad
// alert).
import { describe, expect, it } from "vitest";
import { SeenBaselines, buildNeedsYou, pollResult, type NeedsYouInput } from "./needs_you";
import { fixture, payload } from "./test-utils";

const empty: NeedsYouInput = { terminals: [], work: [], prs: null, attention: [] };
const body = (components: unknown[], extra: Record<string, unknown> = {}) => ({
  freshness: { asOf: "2026-01-15T10:00:00Z" },
  groups: [{ id: "app", label: "Application" }],
  components,
  ...extra,
});
const api = (state: string, over: Record<string, unknown> = {}) => ({ id: "api", group: "app", label: "API", state, ...over });
const ids = (p: ReturnType<typeof payload>): string[] => {
  const r = pollResult("environment", p);
  return r.ok ? r.items.map((i) => i.id) : [];
};

describe("Needs you: Environment rows", () => {
  it("red components are counted rows under Environment that open the view on the component", () => {
    const m = buildNeedsYou({ ...empty, environment: [payload("fx", fixture("environment"), { sourceLabel: "Fixture" })] });
    const s = m.sections.find((x) => x.id === "environment")!;
    expect(s.title).toBe("Environment");
    expect(s.rows.map((r) => [r.id, r.title, r.detail, r.tone, r.counted])).toEqual([
      ["env:fx|worker", "Worker", "deployed but not running · Was running; no stop was requested", "bad", true],
      ["env:fx|db", "Database", "unhealthy · Healthcheck failing for 4 minutes", "bad", true],
      ["env:fx|queue", "Queue", "crash loop · Restarting every few seconds", "bad", true],
      ["env:fx|seed", "Seed", "failed · exit 2", "bad", true],
    ]);
    expect(s.rows[1].opens).toEqual({ view: "environment", key: "db" });
    // Only the seed job carries actions.
    expect(s.rows.map((r) => r.actions.length)).toEqual([0, 0, 0, 1]);
    expect(s.rows[3].actions[0]).toEqual({ sourceId: "fx", sourceLabel: "Fixture", view: "environment", key: "seed" });
    expect(m.count).toBe(4);
  });

  it("starting, idle, ok one-shots and not-deployed components are not rows; another contract is a banner", () => {
    const m = buildNeedsYou({
      ...empty,
      environment: [
        payload("a", body([api("starting"), { id: "b", group: "app", state: "stopped-idle" }, { id: "c", group: "app", state: "oneshot-ok" }, { id: "d", group: "app", state: "not-deployed" }])),
        payload("old", { contract: 7, components: [api("unhealthy")] }, { sourceLabel: "Old" }),
      ],
    });
    expect(m.sections).toEqual([]);
    expect(m.needsUpdate).toEqual([{ sourceId: "old", sourceLabel: "Old" }]);
  });

  it("with two sources each row names its source", () => {
    const m = buildNeedsYou({
      ...empty,
      environment: [payload("a", body([api("unhealthy")]), { sourceLabel: "A" }), payload("b", body([api("crashloop")]), { sourceLabel: "B" })],
    });
    expect(m.sections[0].rows.map((r) => [r.id, r.detail])).toEqual([
      ["env:a|api", "unhealthy · A"],
      ["env:b|api", "crash loop · B"],
    ]);
  });
});

describe("Environment notifications", () => {
  it("tracks red components by id and state, and bad alerts by component and text; warnings never", () => {
    expect(ids(payload("fx", fixture("environment")))).toEqual([
      "component:worker:stopped-problem",
      "component:db:unhealthy",
      "component:queue:crashloop",
      "component:seed:oneshot-failed",
      "alert:db:db is unhealthy",
    ]);
  });

  it("first poll silent; a component entering a red state notifies once, titled with its name; recovery is silent", () => {
    const b = new SeenBaselines();
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("up")]))))).toEqual([]);
    const news = b.update("fx", "environment", pollResult("environment", payload("fx", body([api("crashloop", { restarts: 4, exitCode: 1 })]))));
    expect(news).toEqual([{ title: "API", body: "crash loop: 4 restarts, exit 1", ids: ["component:api:crashloop"] }]);
    // Still red: quiet. Recovered: quiet. Red again: news again.
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("crashloop")]))))).toEqual([]);
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("up")]))))).toEqual([]);
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("starting")]))))).toEqual([]);
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("unhealthy", { hint: "Probe failing" })]))))).toEqual([
      { title: "API", body: "unhealthy: Probe failing", ids: ["component:api:unhealthy"] },
    ]);
  });

  it("a red component in the first poll is the baseline, not news", () => {
    const b = new SeenBaselines();
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", fixture("environment"))))).toEqual([]);
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", fixture("environment"))))).toEqual([]);
  });

  it("a failed poll keeps the ids, so the red component is not announced again after it", () => {
    const b = new SeenBaselines();
    b.update("fx", "environment", pollResult("environment", payload("fx", body([api("unhealthy")]))));
    const failed = payload("fx", body([]), { freshness: { asOf: null, errors: [{ source: "app", message: "connection refused" }] } });
    expect(pollResult("environment", failed)).toEqual({ ok: false });
    expect(b.update("fx", "environment", pollResult("environment", failed))).toEqual([]);
    expect(b.update("fx", "environment", pollResult("environment", payload("fx", body([api("unhealthy")]))))).toEqual([]);
  });

  it("a new bad alert notifies, leading with its component's name; a warn alert does not", () => {
    const b = new SeenBaselines();
    b.update("fx", "environment", pollResult("environment", payload("fx", body([api("up")]))));
    const news = b.update(
      "fx",
      "environment",
      pollResult(
        "environment",
        payload("fx", body([api("up")], { alerts: [{ text: "certificate expires today", severity: "bad", componentId: "api" }, { text: "Disk is 85 percent full", severity: "warn" }] })),
      ),
    );
    expect(news).toEqual([{ title: "API", body: "certificate expires today", ids: ["alert:api:certificate expires today"] }]);
    const stackWide = b.update(
      "fx",
      "environment",
      pollResult("environment", payload("fx", body([api("up")], { alerts: [{ text: "Out of disk", severity: "bad" }] }), { sourceLabel: "Fixture" })),
    );
    expect(stackWide.map((n) => n.title)).toEqual(["Out of disk"]);
  });

  it("five or more at once collapse to one summary leading with a component name", () => {
    const b = new SeenBaselines();
    b.update("fx", "environment", pollResult("environment", payload("fx", body([]))));
    const many = ["a", "b", "c", "d", "e"].map((id) => ({ id, group: "app", label: id.toUpperCase(), state: "unhealthy" }));
    const news = b.update("fx", "environment", pollResult("environment", payload("fx", body(many))));
    expect(news.length).toBe(1);
    expect(news[0].title).toBe("A and 4 more need you");
  });
});
