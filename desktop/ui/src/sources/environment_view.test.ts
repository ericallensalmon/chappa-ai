// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildEnvironmentModel } from "./environment_model";
import { EnvironmentView, environmentEmptyText, type EnvironmentViewInput } from "./environment_view";
import { fixture, payload } from "./test-utils";
import type { SourceViewDeps } from "./view_common";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const NOW = Date.parse("2026-01-15T10:03:00Z");
const ok: ipc.ActionVerdictDto = { enabled: true, reason: null, hidden: false, cwd: null, tool: null };

function deps(over: Partial<SourceViewDeps> = {}): SourceViewDeps & { said: string[] } {
  const said: string[] = [];
  return {
    actions: vi.fn(async () => []),
    run: vi.fn(async () => ({ kind: "done", message: "pong" }) as unknown as ipc.ActionOutcomeDto),
    openLink: vi.fn(async () => {}),
    onClose: vi.fn(),
    say: (t) => void said.push(t),
    now: () => NOW,
    said,
    ...over,
  };
}

const fx = (body: unknown = fixture("environment"), id = "fx", label = "Fixture") => payload(id, body, { sourceLabel: label });
function input(payloads = [fx()], over: Partial<EnvironmentViewInput> = {}): EnvironmentViewInput {
  return { model: buildEnvironmentModel(payloads), servedBy: ["Fixture"], versions: { fx: 1 }, ...over };
}

const row = (v: EnvironmentView, key: string): HTMLElement | null => v.host.querySelector<HTMLElement>(`.chappa-env-row[data-key="${key}"]`);
const rowIds = (v: EnvironmentView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-env-row")].map((r) => r.dataset.id!);
const toggle = (v: EnvironmentView, source = "fx"): HTMLButtonElement =>
  v.host.querySelector<HTMLButtonElement>(`.chappa-env-block[data-source="${source}"] .chappa-env-toggle`)!;

describe("EnvironmentView", () => {
  it("draws the block head with its counts, banners, alerts, groups in order and facts", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-back-arrow")?.textContent).toBe("←");
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("Environment");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect(v.host.querySelector(".chappa-env-source")?.textContent).toBe("Fixture");
    expect(v.host.querySelector(".chappa-env-counts")?.textContent).toContain("1 deployed but not running · 1 stopped");
    // The banner through the shared renderer.
    expect(v.host.querySelector(".chappa-env-banners .chappa-banner")?.getAttribute("data-tone")).toBe("warn");
    expect(v.host.querySelector(".chappa-env-banners .chappa-banner-title")?.textContent).toBe("Images are out of date");
    expect([...v.host.querySelectorAll<HTMLElement>(".chappa-env-alert")].map((a) => `${a.dataset.severity}:${a.textContent}`)).toEqual([
      "bad:db is unhealthy",
      "warn:web is slow to start",
      "warn:Disk is 85 percent full",
    ]);
    expect([...v.host.querySelectorAll(".chappa-sv-group-label")].map((g) => g.textContent)).toEqual(["Application", "Data", "Jobs"]);
    // Not deployed is hidden by default.
    expect(rowIds(v)).toEqual(["api", "web", "worker", "mailer", "db", "queue", "migrate", "seed"]);
    // Facts through the shared section renderer, `more` folded.
    const facts = v.host.querySelector('.chappa-env-facts .chappa-sec[data-section="runtimes"]')!;
    expect(facts.querySelector(".chappa-sec-head")?.textContent).toBe("▾ Runtimes");
    expect([...facts.querySelectorAll(".chappa-sec-body > .chappa-sec-row")].map((f) => f.textContent)).toEqual(["Node22", "Memory6.1 GiB free"]);
    const more = facts.querySelector<HTMLButtonElement>(".chappa-sec-more")!;
    expect(more.textContent).toBe("▸ Show disks");
    expect(facts.querySelector(".chappa-sec-more-rows")?.classList.contains("folded")).toBe(true);
    more.click();
    expect(v.host.querySelector('.chappa-env-facts .chappa-sec-more-rows')?.classList.contains("folded")).toBe(false);
    // A component's sections are folded inside its row.
    const config = row(v, "fx|api")!.querySelector('.chappa-sec[data-section="config"]')!;
    expect(config.classList.contains("collapsed")).toBe(true);
    config.querySelector<HTMLButtonElement>(".chappa-sec-head")!.click();
    expect(row(v, "fx|api")!.querySelector('.chappa-sec[data-section="config"]')?.classList.contains("collapsed")).toBe(false);
    v.dispose();
  });

  it("a row: dot and state in the state's tone, kind, details, source cell, probe cell, hint, badges", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    const api = row(v, "fx|api")!;
    expect(api.querySelector<HTMLElement>(".chappa-env-dot")?.dataset.tone).toBe("ok");
    expect(api.querySelector(".chappa-env-label")?.textContent).toBe("API");
    expect(api.querySelector(".chappa-env-state")?.textContent).toBe("up");
    expect(api.querySelector<HTMLElement>(".chappa-env-kind")?.hidden).toBe(true);
    expect(api.querySelector(".chappa-env-details")?.textContent).toBe("up 3h 12m");
    const src = api.querySelector<HTMLElement>(".chappa-env-source-cell")!;
    expect([src.dataset.mode, src.textContent]).toEqual(["dev", "dev · abc-1-health @ 1a2b3c4"]);
    expect(src.title).toContain("working copy");
    const probe = api.querySelector<HTMLElement>(".chappa-env-probe")!;
    expect([probe.dataset.up, probe.textContent]).toEqual(["true", "health up 200"]);
    expect(probe.title).toContain("https://dev.example.test/api/health");
    expect(api.querySelector(".chappa-env-hint")?.textContent).toBe("Serving from your working copy");
    expect([...api.querySelectorAll(".chappa-sv-chip:not(.chappa-env-kind)")].map((c) => c.textContent)).toEqual(["dev"]);

    const worker = row(v, "fx|worker")!;
    expect([worker.dataset.tone, worker.querySelector(".chappa-env-state")?.textContent]).toEqual(["bad", "deployed but not running"]);
    expect(worker.querySelector(".chappa-env-details")?.textContent).toBe("exit 137");
    const mailer = row(v, "fx|mailer")!;
    expect([mailer.dataset.tone, mailer.querySelector(".chappa-env-details")?.textContent]).toEqual(["muted", "exit 0"]);
    expect(row(v, "fx|migrate")!.querySelector<HTMLElement>(".chappa-env-kind")?.hidden).toBe(false);
    const web = row(v, "fx|web")!.querySelector<HTMLElement>(".chappa-env-probe")!;
    expect([web.dataset.up, web.textContent]).toEqual(["false", "web down: no response"]);
    expect(row(v, "fx|db")!.querySelector(".chappa-env-probe")?.textContent).toBe("health down: 503 fails the health rule");
    expect(row(v, "fx|db")!.querySelector(".chappa-env-source-cell")?.textContent).toBe("prebuilt · registry.example/db:16");
    v.dispose();
  });

  it("component-scoped alerts mark their row", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(row(v, "fx|db")!.dataset.alert).toBe("bad");
    expect(row(v, "fx|web")!.dataset.alert).toBe("warn");
    expect(row(v, "fx|api")!.dataset.alert).toBeUndefined();
    expect([...row(v, "fx|db")!.querySelectorAll<HTMLElement>(".chappa-env-row-alert")].map((c) => `${c.dataset.tone}:${c.textContent}`)).toEqual(["bad:db is unhealthy"]);
    v.dispose();
  });

  it("the not-deployed toggle shows and hides them, and the choice survives a poll", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(toggle(v).textContent).toBe("Show components that are not deployed (1)");
    expect(row(v, "fx|search")).toBeNull();
    toggle(v).click();
    expect(toggle(v).textContent).toBe("Hide components that are not deployed (1)");
    expect(row(v, "fx|search")?.querySelector(".chappa-env-state")?.textContent).toBe("not deployed");
    v.update(input());
    expect(row(v, "fx|search")).not.toBeNull();
    toggle(v).click();
    expect(row(v, "fx|search")).toBeNull();
    // No not-deployed components: no toggle.
    const body = fixture("environment") as unknown as Record<string, unknown> & { components: { state: string }[] };
    body.components = body.components.filter((c) => c.state !== "not-deployed");
    v.update(input([fx(body)]));
    expect(toggle(v).hidden).toBe(true);
    v.dispose();
  });

  it("a poll repaints in place: unchanged rows keep their nodes, a changed one updates", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    const before = row(v, "fx|api");
    const body = fixture("environment") as unknown as Record<string, unknown> & { components: Record<string, unknown>[] };
    body.components[1] = { ...body.components[1], state: "up" };
    v.update(input([fx(body)], { versions: { fx: 2 } }));
    expect(row(v, "fx|api")).toBe(before);
    expect(row(v, "fx|web")!.querySelector(".chappa-env-state")?.textContent).toBe("up");
    v.dispose();
  });

  it("links open through the checked opener, the browser one first and marked; a refusal is said", async () => {
    const d = deps({ openLink: vi.fn(async () => Promise.reject(new Error("dev.example.test is not a trusted host"))) });
    const v = new EnvironmentView(d);
    v.update(input());
    const links = [...row(v, "fx|api")!.querySelectorAll<HTMLButtonElement>(".chappa-env-link")];
    expect(links.map((l) => [l.textContent, l.dataset.browser ?? ""])).toEqual([["Open ↗", "true"], ["Docs", ""]]);
    expect(links[0].title).toBe("Open https://dev.example.test/api in the browser");
    links[0].click();
    await FLUSH();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://dev.example.test/api");
    expect(d.said).toEqual(["dev.example.test is not a trusted host"]);
    v.dispose();
  });

  it("actions come from Rust per component and run through runAction", async () => {
    const actions = vi.fn(async (): Promise<ipc.SourceActionDto[]> => [
      { action: { kind: "endpoint", id: "ping", label: "Ping", mutates: false, confirm: null, params: null, note: null }, verdict: ok, needsPrompt: false },
    ]);
    const d = deps({ actions });
    const v = new EnvironmentView(d);
    v.update(input());
    await FLUSH();
    // The view's own (its banner's), then api and seed; the rest never ask.
    expect(actions.mock.calls.map((c) => (c as unknown[]).slice(0, 3))).toEqual([
      ["fx", "environment", null],
      ["fx", "environment", "api"],
      ["fx", "environment", "seed"],
    ]);
    const btn = row(v, "fx|api")!.querySelector<HTMLButtonElement>(".chappa-sv-action")!;
    expect(btn.textContent).toBe("Ping");
    btn.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "environment", "api", "ping", false, null);
    v.dispose();
  });

  it("one block per source, each with its own toggle; another contract is a banner", () => {
    const v = new EnvironmentView(deps());
    v.update(input([fx(), fx(fixture("environment"), "two", "Second"), fx({ contract: 9 }, "old", "Old")], { servedBy: ["Fixture", "Second", "Old"] }));
    expect([...v.host.querySelectorAll(".chappa-env-source")].map((s) => s.textContent)).toEqual(["Fixture", "Second"]);
    expect(row(v, "two|api")).not.toBeNull();
    toggle(v, "two").click();
    expect(row(v, "two|search")).not.toBeNull();
    expect(row(v, "fx|search")).toBeNull();
    expect(v.host.querySelector(".chappa-sv-banner")?.textContent).toContain("Old needs an update");
    v.dispose();
  });

  it("focus scrolls to and marks a row, unhiding a not-deployed one; a focus before the first answer waits for it", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(v.focus("db")).toBe(true);
    expect(row(v, "fx|db")!.classList.contains("focused")).toBe(true);
    expect(v.focus("search")).toBe(true);
    expect(row(v, "fx|db")!.classList.contains("focused")).toBe(false);
    expect(row(v, "fx|search")!.classList.contains("focused")).toBe(true);
    expect(v.focus("nope")).toBe(false);
    v.dispose();

    const w = new EnvironmentView(deps());
    w.update(input([fx(null)]));
    expect(w.focus("queue")).toBe(false);
    w.update(input());
    expect(row(w, "fx|queue")!.classList.contains("focused")).toBe(true);
    w.dispose();
  });

  it("empty states: no source, and waiting for the first answer", () => {
    expect(environmentEmptyText({ model: buildEnvironmentModel([]), servedBy: [], versions: {} })).toContain("No trusted source serves an environment");
    const v = new EnvironmentView(deps());
    v.update(input([fx(null)]));
    expect(v.host.querySelector(".chappa-sv-empty")?.textContent).toBe("Waiting for the first answer from Fixture.");
    expect(v.host.querySelector(".chappa-env-wait")?.textContent).toBe("Waiting for the first answer from Fixture.");
    v.dispose();
  });
});
