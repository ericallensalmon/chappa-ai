// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { buildEnvironmentModel } from "./environment_model";
import { ENV_COLUMNS, EnvironmentView, environmentEmptyText, groupFoldKey, notDeployedKey, type EnvironmentViewInput } from "./environment_view";
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

type Body = Record<string, unknown> & { components: Record<string, unknown>[]; groups: Record<string, unknown>[] };
const body = (): Body => fixture("environment") as unknown as Body;
const fx = (b: unknown = body(), id = "fx", label = "Fixture") => payload(id, b, { sourceLabel: label });
function input(payloads = [fx()], over: Partial<EnvironmentViewInput> = {}): EnvironmentViewInput {
  return { model: buildEnvironmentModel(payloads), servedBy: ["Fixture"], versions: { fx: 1 }, ...over };
}

const row = (v: EnvironmentView, key: string): HTMLElement | null => v.host.querySelector<HTMLElement>(`.chappa-env-row[data-key="${key}"]`);
const rowIds = (v: EnvironmentView): string[] => [...v.host.querySelectorAll<HTMLElement>(".chappa-env-row")].map((r) => r.dataset.id!);
const toggle = (v: EnvironmentView, source = "fx"): HTMLButtonElement =>
  v.host.querySelector<HTMLButtonElement>(`.chappa-env-block[data-source="${source}"] .chappa-env-toggle`)!;
const groupHead = (v: EnvironmentView, id: string, source = "fx"): HTMLButtonElement =>
  v.host.querySelector<HTMLButtonElement>(`.chappa-env-block[data-source="${source}"] .chappa-env-group[data-group="${id}"] .chappa-env-group-toggle`)!;
const folded = (v: EnvironmentView, id: string, source = "fx"): boolean =>
  v.host.querySelector(`.chappa-env-block[data-source="${source}"] .chappa-env-group[data-group="${id}"]`)!.classList.contains("folded");
const stored = (key: string): string | null => window.localStorage.getItem(`chappa-ai.section-collapsed.${key}`);
const endpoint = (a: Record<string, unknown> & { id: string; label: string }): ipc.SourceActionDto => ({
  action: { kind: "endpoint", mutates: false, confirm: null, params: null, note: null, ...a } as unknown as ipc.SourceActionDto["action"],
  verdict: ok,
  needsPrompt: false,
});

/** The fixture with a stack-wide action and a group's action. */
function withActions(): Body {
  const b = body();
  b.actions = [{ kind: "endpoint", id: "up", label: "Bring up", mutates: true, confirm: "Bring the stack up" }];
  b.groups[1] = { ...b.groups[1], actions: [{ kind: "endpoint", id: "app:up", label: "Up", mutates: true, confirm: "Run the up script" }] };
  return b;
}

beforeEach(() => window.localStorage.clear());
afterEach(() => document.querySelectorAll(".chappa-dialog-backdrop, .chappa-dialog").forEach((d) => d.remove()));

describe("EnvironmentView", () => {
  it("draws the header: name, toned chips, the facts line, banners and alerts, with the back arrow", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(v.host.querySelector(".chappa-sv-header")?.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(v.host.querySelector(".chappa-sv-header .chappa-page-title")?.textContent).toBe("Environment");
    expect(v.host.querySelector(".chappa-sv-fresh")?.textContent).toBe("as of 3 min ago");
    expect(v.host.querySelector(".chappa-env-source")?.textContent).toBe("Fixture");
    const chips = [...v.host.querySelectorAll<HTMLElement>(".chappa-env-chips .chappa-sv-chip")].map((c) => `${c.dataset.tone}:${c.textContent}`);
    expect(chips).toEqual([
      "ok:1 up",
      "warn:1 starting",
      "bad:1 unhealthy",
      "bad:1 in a crash loop",
      "bad:1 deployed but not running",
      "muted:1 stopped",
      "muted:1 ran ok",
      "bad:1 failed",
      "muted:1 not deployed",
      "warn:endpoints 1/3",
    ]);
    // Facts as one compact line, and a way to every section and its actions.
    expect([...v.host.querySelectorAll(".chappa-env-fact")].map((f) => f.textContent)).toEqual(["Node:22", "Memory:6.1 GiB free"]);
    expect(v.host.querySelector(".chappa-env-details-link")?.textContent).toBe("Details");
    expect(v.host.querySelector(".chappa-env-banners .chappa-banner-title")?.textContent).toBe("Images are out of date");
    expect([...v.host.querySelectorAll<HTMLElement>(".chappa-env-alert")].map((a) => `${a.dataset.severity}:${a.textContent}`)).toEqual([
      "bad:db is unhealthy",
      "warn:web is slow to start",
      "warn:Disk is 85 percent full",
    ]);
    v.dispose();
  });

  it("groups in the source's order, each header '<label> (up/total up)', rows under one column header", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect([...v.host.querySelectorAll(".chappa-env-col")].map((c) => c.textContent)).toEqual(ENV_COLUMNS);
    expect([...v.host.querySelectorAll(".chappa-env-group-toggle")].map((g) => g.textContent)).toEqual([
      "▾ Application (1/4 up)",
      "▾ Data (0/2 up)",
      "▾ Jobs (0/2 up)",
    ]);
    // Not deployed hidden by default; group order, then the source's order.
    expect(rowIds(v)).toEqual(["api", "web", "worker", "mailer", "db", "queue", "migrate", "seed"]);
    v.dispose();
  });

  it("a row is one dense line: state pill, name with badges and a muted second line, mode and ref, uptime, endpoint pill, links", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    const api = row(v, "fx|api")!;
    expect(api.querySelectorAll(".chappa-env-cell").length).toBe(ENV_COLUMNS.length);
    const pill = api.querySelector<HTMLElement>(".chappa-env-c-state .chappa-env-pill")!;
    expect([pill.dataset.tone, pill.textContent]).toEqual(["ok", "up"]);
    expect(api.querySelector(".chappa-env-name")?.textContent).toBe("API");
    expect([...api.querySelectorAll(".chappa-env-badges .chappa-sv-chip")].map((c) => c.textContent)).toEqual(["dev"]);
    const sub = api.querySelector<HTMLElement>(".chappa-env-sub")!;
    expect([sub.textContent, sub.title]).toEqual(["Serving from your working copy", "Serving from your working copy"]);
    expect(api.querySelector(".chappa-env-mode")?.textContent).toBe("dev");
    expect(api.querySelector<HTMLElement>(".chappa-env-mode")!.title).toContain("working copy");
    expect(api.querySelector(".chappa-env-ref")?.textContent).toBe("1a2b3c4 abc-1-health");
    expect(api.querySelector(".chappa-env-uptime")?.textContent).toBe("3h 12m");
    const ep = api.querySelector<HTMLElement>(".chappa-env-endpoint")!;
    expect([ep.textContent, ep.dataset.tone]).toEqual(["answers", "ok"]);
    expect(ep.title).toContain("https://dev.example.test/api/health");
    expect(row(v, "fx|web")!.querySelector(".chappa-env-endpoint")?.textContent).toBe("no answer");
    expect(row(v, "fx|queue")!.querySelector(".chappa-env-endpoint")?.textContent).toBe("–");
    expect(row(v, "fx|db")!.querySelector(".chappa-env-ref")?.textContent).toBe("registry.example/db:16");
    // Links: the browser one first, each with its whole URL as the tooltip.
    const links = [...api.querySelectorAll<HTMLButtonElement>(".chappa-env-link")];
    expect(links.map((l) => [l.textContent, l.dataset.browser ?? ""])).toEqual([
      ["https://dev.example.test/api", "true"],
      ["https://dev.example.test/api/docs", ""],
    ]);
    expect(links[0].title).toBe("Open https://dev.example.test/api in the browser");
    // No action buttons on a row until it is reached.
    expect(api.querySelector(".chappa-sv-action")).toBeNull();
    // A one-shot carries its chip; component alerts mark the row.
    expect([...row(v, "fx|migrate")!.querySelectorAll(".chappa-env-badges .chappa-sv-chip")].map((c) => c.textContent)).toEqual(["one-shot"]);
    expect(row(v, "fx|db")!.dataset.alert).toBe("bad");
    expect(row(v, "fx|web")!.dataset.alert).toBe("warn");
    expect(row(v, "fx|api")!.dataset.alert).toBeUndefined();
    v.dispose();
  });

  it("a group folds from its header and stays folded for that stack only, across pages", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    groupHead(v, "data").click();
    expect(groupHead(v, "data").textContent).toBe("▸ Data (0/2 up)");
    expect(groupHead(v, "data").getAttribute("aria-expanded")).toBe("false");
    expect(folded(v, "data")).toBe(true);
    expect(stored(groupFoldKey("fx", "data"))).toBe("1");
    v.dispose();
    const w = new EnvironmentView(deps());
    w.update(input([fx(), fx(body(), "two", "Second")], { servedBy: ["Fixture", "Second"] }));
    expect(folded(w, "data")).toBe(true);
    expect(folded(w, "data", "two")).toBe(false);
    groupHead(w, "data").click();
    expect(folded(w, "data")).toBe(false);
    expect(stored(groupFoldKey("fx", "data"))).toBeNull();
    w.dispose();
  });

  it("not deployed: hidden behind the toggle; shown dimmed in their own group; the choice is remembered per stack", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    expect(toggle(v).textContent).toBe("Show components that are not deployed (1)");
    expect(toggle(v).getAttribute("aria-pressed")).toBe("false");
    expect(row(v, "fx|search")).toBeNull();
    toggle(v).click();
    expect(toggle(v).getAttribute("aria-pressed")).toBe("true");
    const search = row(v, "fx|search")!;
    expect(search.classList.contains("dim")).toBe(true);
    expect(search.closest<HTMLElement>(".chappa-env-group")?.dataset.group).toBe("data");
    expect(row(v, "fx|db")!.classList.contains("dim")).toBe(false);
    // The group counts only what is deployed.
    expect(groupHead(v, "data").textContent).toBe("▾ Data (0/2 up)");
    expect(stored(notDeployedKey("fx"))).toBe("1");
    v.update(input());
    expect(row(v, "fx|search")).not.toBeNull();
    v.dispose();
    const w = new EnvironmentView(deps());
    w.update(input([fx(), fx(body(), "two", "Second")], { servedBy: ["Fixture", "Second"] }));
    expect(row(w, "fx|search")).not.toBeNull();
    expect(row(w, "two|search")).toBeNull();
    // A group of only not-deployed components says so.
    const b = body();
    b.components = b.components.filter((c) => c.group !== "data" || c.state === "not-deployed");
    w.update(input([fx(b)]));
    expect(groupHead(w, "data").textContent).toBe("▾ Data (1 not deployed)");
    toggle(w).click();
    expect(row(w, "fx|search")).toBeNull();
    // No not-deployed components: no toggle.
    const none = body();
    none.components = none.components.filter((c) => c.state !== "not-deployed");
    w.update(input([fx(none)]));
    expect(toggle(w).hidden).toBe(true);
    w.dispose();
  });

  it("stack-wide actions sit in the header and a group's on its header, asked for with their keys; rows ask for nothing yet", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === null
        ? [endpoint({ id: "up", label: "Bring up" }), endpoint({ id: "check-images", label: "Check again" })]
        : key === "data"
          ? [endpoint({ id: "app:up", label: "Up" })]
          : [endpoint({ id: "ping", label: "Ping" })],
    );
    const v = new EnvironmentView(deps({ actions }));
    v.update(input([fx(withActions())]));
    await FLUSH();
    const keys = actions.mock.calls.map((c) => (c as unknown[])[2]);
    expect(keys.filter((k) => k === null).length).toBe(2);
    expect(keys.filter((k) => k !== null)).toEqual(["data", "jobs"]);
    expect([...v.host.querySelectorAll<HTMLElement>(".chappa-env-stack-actions .chappa-sv-action")].map((b) => b.dataset.action)).toEqual(["up"]);
    expect([...v.host.querySelectorAll<HTMLElement>('.chappa-env-group[data-group="data"] .chappa-env-group-actions .chappa-sv-action')].map((b) => b.dataset.action)).toEqual([
      "app:up",
    ]);
    expect(v.host.querySelector('.chappa-env-group[data-group="app"] .chappa-env-group-actions .chappa-sv-action')).toBeNull();
    v.dispose();
  });

  it("the header's Up opens the picker: the default, named selections or ticked groups and components, sent with the call", async () => {
    const up = endpoint({
      id: "up",
      label: "Bring up",
      mutates: true,
      confirm: "Pull api; start api, db.",
      inputs: [
        {
          name: "selection",
          label: "Bring up",
          kind: "choice",
          required: true,
          default: "default",
          max: null,
          options: [
            { value: "default", label: "Default: everything not normally off" },
            { value: "named:backend", label: "backend: api, db" },
            { value: "picked", label: "Only what is ticked below" },
          ],
        },
        { name: "pick", label: "Ticked", kind: "multi", required: false, default: null, max: null, options: [{ value: "group:data", label: "Data (group)" }, { value: "api", label: "API" }] },
      ],
    });
    const d = deps({ actions: vi.fn(async (_s: string, _v: string, key: string | null) => (key === null ? [up] : [])) });
    const v = new EnvironmentView(d);
    document.body.appendChild(v.host);
    v.update(input([fx(withActions())]));
    await FLUSH();
    v.host.querySelector<HTMLButtonElement>('.chappa-env-stack-actions .chappa-sv-action[data-action="up"]')!.click();
    await FLUSH();
    const dialog = document.querySelector<HTMLElement>(".chappa-dialog")!;
    expect(dialog.querySelector(".chappa-dialog-message")?.textContent).toBe("Pull api; start api, db.");
    const select = dialog.querySelector<HTMLSelectElement>('select[data-name="selection"]')!;
    expect([...select.options].map((o) => o.value)).toEqual(["", "default", "named:backend", "picked"]);
    expect(select.value).toBe("default");
    expect([...dialog.querySelectorAll<HTMLInputElement>('[data-name="pick"] input')].map((c) => c.value)).toEqual(["group:data", "api"]);
    select.value = "picked";
    dialog.querySelector<HTMLInputElement>('[data-name="pick"] input[value="group:data"]')!.checked = true;
    [...dialog.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Bring up")!.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "environment", null, "up", true, null, undefined, { selection: "picked", pick: ["group:data"] });
    v.host.remove();
    v.dispose();
  });

  it("row actions: shown on hover once the row is reached, and in the keyboard-reachable ⋯ menu", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === "api" ? [endpoint({ id: "ping", label: "Ping" })] : [],
    );
    const d = deps({ actions });
    const v = new EnvironmentView(d);
    document.body.appendChild(v.host);
    v.update(input());
    await FLUSH();
    expect(actions.mock.calls.some((c) => (c as unknown[])[2] === "api")).toBe(false);
    const api = row(v, "fx|api")!;
    api.dispatchEvent(new MouseEvent("mouseenter"));
    await FLUSH();
    const hover = api.querySelector<HTMLButtonElement>('.chappa-env-inline .chappa-sv-action[data-action="ping"]')!;
    expect(hover.textContent).toBe("Ping");
    hover.click();
    await FLUSH();
    expect(d.run).toHaveBeenCalledWith("fx", "environment", "api", "ping", false, null);
    // A click on an action does not open the row's page.
    expect(v.host.querySelector(".chappa-env-page")).toBeNull();
    const more = api.querySelector<HTMLButtonElement>(".chappa-env-more")!;
    expect(more.getAttribute("aria-haspopup")).toBe("menu");
    more.focus();
    more.click();
    await FLUSH();
    const menu = v.host.querySelector<HTMLElement>(".chappa-env-menu")!;
    expect(menu.dataset.key).toBe("fx|api");
    expect(more.getAttribute("aria-expanded")).toBe("true");
    expect([...menu.querySelectorAll<HTMLButtonElement>(".chappa-sv-action")].map((b) => b.textContent)).toEqual(["Open details", "Ping"]);
    expect(document.activeElement?.textContent).toBe("Open details");
    // Escape closes the menu, back on its button, without leaving the view.
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(v.host.querySelector(".chappa-env-menu")).toBeNull();
    expect(document.activeElement).toBe(more);
    expect(d.onClose).not.toHaveBeenCalled();
    more.click();
    v.host.querySelector<HTMLButtonElement>(".chappa-env-menu-open")!.click();
    expect(v.host.querySelector<HTMLElement>(".chappa-env-page")?.dataset.key).toBe("fx|api");
    v.host.remove();
    v.dispose();
  });

  it("clicking a row opens its page with everything, left by the top-left back arrow", async () => {
    const actions = vi.fn(async (_s: string, _v: string, key: string | null): Promise<ipc.SourceActionDto[]> =>
      key === "worker" ? [endpoint({ id: "restart", label: "Restart" })] : [],
    );
    const withRestart = body();
    withRestart.components.find((x) => x.id === "worker")!.actions = [{ kind: "endpoint", id: "restart", label: "Restart", mutates: false }];
    const v = new EnvironmentView(deps({ actions }));
    v.update(input([fx(withRestart)]));
    row(v, "fx|worker")!.click();
    await FLUSH();
    const page = v.host.querySelector<HTMLElement>(".chappa-env-page")!;
    expect(page.dataset.key).toBe("fx|worker");
    expect(v.host.classList.contains("covered")).toBe(true);
    const head = page.querySelector(".chappa-sv-header")!;
    expect(head.firstElementChild?.classList.contains("chappa-back-arrow")).toBe(true);
    expect(head.querySelector(".chappa-page-title")?.textContent).toBe("Worker");
    expect(page.querySelector(".chappa-env-page-state .chappa-env-pill")?.textContent).toBe("deployed but not running");
    const grid = [...page.querySelectorAll(".chappa-env-page-grid dt")].map((dt) => `${dt.textContent}=${dt.nextElementSibling?.textContent}`);
    expect(grid).toEqual(["Id=worker", "Hint=Was running; no stop was requested", "Details=exit 137"]);
    expect([...page.querySelectorAll<HTMLElement>(".chappa-env-page-actions .chappa-sv-action")].map((b) => b.textContent)).toEqual(["Restart"]);
    expect(page.textContent).not.toContain("×");
    // A poll keeps it current; a component no longer reported says so.
    const b = body();
    b.components = b.components.filter((c) => c.id !== "worker");
    v.update(input([fx(b)], { versions: { fx: 2 } }));
    expect(page.querySelector(".chappa-env-gone")?.textContent).toBe("This component is no longer reported.");
    page.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    expect(v.host.querySelector(".chappa-env-page")).toBeNull();
    expect(v.host.classList.contains("covered")).toBe(false);
    v.dispose();
  });

  it("a component's page shows its sections, links and probe; Details opens every fact section", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    row(v, "fx|api")!.querySelector<HTMLButtonElement>(".chappa-env-name")!.click();
    const page = v.host.querySelector<HTMLElement>(".chappa-env-page")!;
    expect(page.querySelector('.chappa-sec[data-section="config"]')).not.toBeNull();
    expect([...page.querySelectorAll(".chappa-env-page-links .chappa-env-link")].map((l) => l.textContent)).toEqual(["Open ↗", "Docs"]);
    expect(page.textContent).toContain("health up 200");
    page.querySelector<HTMLButtonElement>(".chappa-back-arrow")!.click();
    v.host.querySelector<HTMLButtonElement>(".chappa-env-details-link")!.click();
    const facts = v.host.querySelector<HTMLElement>(".chappa-env-facts-page")!;
    expect(facts.querySelector(".chappa-page-title")?.textContent).toBe("Fixture: details");
    expect(facts.querySelector('.chappa-sec[data-section="runtimes"]')).not.toBeNull();
    v.dispose();
  });

  it("a poll repaints in place: unchanged rows keep their nodes, a changed one updates", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    const before = row(v, "fx|api");
    const b = body();
    b.components[1] = { ...b.components[1], state: "up" };
    v.update(input([fx(b)], { versions: { fx: 2 } }));
    expect(row(v, "fx|api")).toBe(before);
    expect(row(v, "fx|web")!.querySelector(".chappa-env-pill")?.textContent).toBe("up");
    v.dispose();
  });

  it("holds sixty components with a light row each and no row action asked for until reached", async () => {
    const b = body();
    b.groups = [
      { id: "g1", label: "One" },
      { id: "g2", label: "Two" },
    ];
    b.components = Array.from({ length: 60 }, (_, i) => ({
      id: `svc-${i}`,
      group: i % 2 ? "g2" : "g1",
      state: i % 5 ? "up" : "stopped-idle",
      hint: `app-0-svc-${i} (podman)`,
      actions: [{ kind: "endpoint", id: "ping", label: "Ping", mutates: false }],
    }));
    delete b.summary;
    const actions = vi.fn(async () => []);
    const v = new EnvironmentView(deps({ actions }));
    v.update(input([fx(b)]));
    await FLUSH();
    expect(rowIds(v).length).toBe(60);
    expect(v.host.querySelectorAll(".chappa-env-groups *").length / 60).toBeLessThan(25);
    expect(actions.mock.calls.every((c) => (c as unknown[])[2] === null)).toBe(true);
    expect(groupHead(v, "g1").textContent).toBe("▾ One (24/30 up)");
    v.dispose();
  });

  it("links open through the checked opener; a refusal is said", async () => {
    const d = deps({ openLink: vi.fn(async () => Promise.reject(new Error("dev.example.test is not a trusted host"))) });
    const v = new EnvironmentView(d);
    v.update(input());
    row(v, "fx|api")!.querySelector<HTMLButtonElement>(".chappa-env-link")!.click();
    await FLUSH();
    expect(d.openLink).toHaveBeenCalledWith("fx", "https://dev.example.test/api");
    expect(d.said).toEqual(["dev.example.test is not a trusted host"]);
    expect(v.host.querySelector(".chappa-env-page")).toBeNull();
    v.dispose();
  });

  it("one block per source, each with its own toggle; another contract is a banner", () => {
    const v = new EnvironmentView(deps());
    v.update(input([fx(), fx(body(), "two", "Second"), fx({ contract: 9 }, "old", "Old")], { servedBy: ["Fixture", "Second", "Old"] }));
    expect([...v.host.querySelectorAll(".chappa-env-source")].map((s) => s.textContent)).toEqual(["Fixture", "Second"]);
    toggle(v, "two").click();
    expect(row(v, "two|search")).not.toBeNull();
    expect(row(v, "fx|search")).toBeNull();
    expect(v.host.querySelector(".chappa-sv-banner")?.textContent).toContain("Old needs an update");
    v.dispose();
  });

  it("focus scrolls to and marks a row, unfolding its group and unhiding a not-deployed one; a focus before the first answer waits", () => {
    const v = new EnvironmentView(deps());
    v.update(input());
    groupHead(v, "data").click();
    expect(v.focus("db")).toBe(true);
    expect(row(v, "fx|db")!.classList.contains("focused")).toBe(true);
    expect(folded(v, "data")).toBe(false);
    expect(v.focus("search")).toBe(true);
    expect(row(v, "fx|db")!.classList.contains("focused")).toBe(false);
    expect(row(v, "fx|search")!.classList.contains("focused")).toBe(true);
    expect(v.focus("nope")).toBe(false);
    v.dispose();

    window.localStorage.clear();
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
