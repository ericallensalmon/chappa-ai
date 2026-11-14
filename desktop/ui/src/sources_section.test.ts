// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { SettingsPane } from "./settings_pane";
import { SourcesSection } from "./sources_section";
import { SourcesStore, type Source, type SourcesApi, type SourcesListing } from "./sources_store";
import { stubSettings } from "./term/test-utils";

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

function source(id: string, over: Partial<Source> = {}): Source {
  return {
    id,
    label: id,
    origin: { kind: "launched", project_id: 1, project_path: "/code/app", command: `${id}-cmd` },
    status: "up",
    lastError: null,
    trusted: true,
    trust: { id, label: id, linkHosts: ["github.com"], roots: ["/srv/x"], views: ["work", "prs"], hash: "c0ffee".padEnd(64, "0") },
    manifestId: id,
    version: "1.2.3",
    views: [
      { name: "work", pollSeconds: 60, asOf: "2026-01-02T03:04:05.000Z", nextUtc: null, refreshing: false, lastError: null, hasPayload: true },
      { name: "prs", pollSeconds: 300, asOf: null, nextUtc: null, refreshing: true, lastError: "the source answered 500", hasPayload: false },
    ],
    warning: null,
    ...over,
  };
}

function fakeApi(initial: SourcesListing) {
  let listing = initial;
  const api: SourcesApi & { set: (l: SourcesListing) => void } = {
    set: (l) => {
      listing = l;
    },
    listSources: vi.fn(async () => listing),
    sourceTrustAnswer: vi.fn(async (id: string, trust: boolean) => {
      listing = {
        ...listing,
        sources: listing.sources.map((s) => (s.id === id && trust ? { ...s, trusted: true, status: "up" as const } : s)),
      };
      return listing;
    }),
    sourceView: vi.fn(async () => ({ payload: null, freshness: { asOf: null } })),
    setSourceViewHidden: vi.fn(async (view: string, hidden: boolean) => {
      const hiddenViews = listing.hiddenViews.filter((v) => v !== view);
      if (hidden) hiddenViews.push(view);
      listing = { ...listing, hiddenViews };
      return listing;
    }),
    attachSource: vi.fn(async (addr: string) => {
      if (!addr.startsWith("127.")) throw new Error(`\`${addr}\` is not a loopback address`);
      listing = {
        ...listing,
        sources: [...listing.sources, source("att", { origin: { kind: "attached", addr, token_file: "/t" }, status: "starting", trusted: false, trust: null })],
      };
      return listing;
    }),
    detachSource: vi.fn(async (id: string) => {
      listing = { ...listing, sources: listing.sources.filter((s) => s.id !== id) };
      return listing;
    }),
  };
  return api;
}

async function mount(initial: SourcesListing) {
  const api = fakeApi(initial);
  const store = new SourcesStore(api);
  const section = new SourcesSection({ store });
  document.body.appendChild(section.element);
  section.activate();
  await FLUSH();
  return { api, store, section };
}

function rows(section: SourcesSection): HTMLDivElement[] {
  return [...section.element.querySelectorAll<HTMLDivElement>(".chappa-source-row")];
}

describe("SourcesSection", () => {
  it("lists every source with its origin, status, last error and views", async () => {
    const { section } = await mount({
      sources: [
        source("github", { label: "GitHub" }),
        source("broken", { status: "down", lastError: "the source process exited with code 1", trusted: true }),
        source("att", {
          origin: { kind: "attached", addr: "127.0.0.1:7000", token_file: "/t" },
          warning: "the permissions of /t were not checked on this platform; keep it in a folder only your account can read",
        }),
      ],
      hiddenViews: [],
    });
    const [gh, broken, att] = rows(section);
    expect(gh.querySelector('[data-role="label"]')?.textContent).toBe("GitHub 1.2.3");
    expect(gh.querySelector('[data-role="status"]')?.textContent).toBe("Up");
    expect(gh.querySelector('[data-role="origin"]')?.textContent).toBe("github-cmd · app");
    const views = gh.querySelector('[data-role="views"]')?.textContent ?? "";
    expect(views).toContain("Work: as of 2026-01-02T03:04:05.000Z");
    expect(views).toContain("PRs: refreshing (the source answered 500)");
    expect(broken.querySelector('[data-role="status"]')?.textContent).toBe("Down");
    expect(broken.querySelector('[data-role="lastError"]')?.textContent).toBe("the source process exited with code 1");
    expect(att.querySelector('[data-role="origin"]')?.textContent).toBe("attached · 127.0.0.1:7000");
    expect(att.querySelector('[data-role="warning"]')?.textContent).toMatch(/not checked/);
    expect(att.querySelector('[data-action="detachSource"]')).not.toBeNull();
    expect(gh.querySelector('[data-action="detachSource"]')).toBeNull();
    section.dispose();
  });

  it("says how to add one when there are none", async () => {
    const { section } = await mount({ sources: [], hiddenViews: [] });
    expect(section.element.textContent).toContain("source: {}");
    section.dispose();
  });

  it("offers Trust only for a source awaiting review, and the dialog shows what the hash covers", async () => {
    const { api, section } = await mount({
      sources: [source("a", { trusted: false, status: "needs_review" }), source("b")],
      hiddenViews: [],
    });
    const [a, b] = rows(section);
    expect(a.querySelector('[data-action="trustSource"]')).not.toBeNull();
    expect(b.querySelector('[data-action="trustSource"]')).toBeNull();
    a.querySelector<HTMLButtonElement>('[data-action="trustSource"]')!.click();
    await FLUSH();
    const dialog = document.querySelector(".chappa-dialog")!;
    const text = dialog.querySelector(".chappa-dialog-message")!.textContent ?? "";
    expect(text).toContain('Trust source "a" (a)?');
    expect(text).toContain("github.com");
    expect(text).toContain("/srv/x");
    expect(text).toContain("Work");
    expect(text).toContain("PRs");
    expect(text).toContain("c0ffee0000000000…");
    expect(dialog.querySelector(".chappa-dialog-ok")?.textContent).toBe("Trust");
    dialog.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    await FLUSH();
    expect(api.sourceTrustAnswer).toHaveBeenCalledWith("a", true, "c0ffee".padEnd(64, "0"));
    // The repaint dropped the button: the source is trusted now.
    expect(rows(section)[0].querySelector('[data-action="trustSource"]')).toBeNull();
    expect(rows(section)[0].querySelector('[data-role="status"]')?.textContent).toBe("Up");
    section.dispose();
  });

  it("sends a declined review as trust=false with the same hash", async () => {
    const { api, section } = await mount({
      sources: [source("a", { trusted: false, status: "needs_review" })],
      hiddenViews: [],
    });
    rows(section)[0].querySelector<HTMLButtonElement>('[data-action="trustSource"]')!.click();
    await FLUSH();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-cancel")!.click();
    await FLUSH();
    await FLUSH();
    expect(api.sourceTrustAnswer).toHaveBeenCalledWith("a", false, "c0ffee".padEnd(64, "0"));
    expect(rows(section)[0].querySelector('[data-action="trustSource"]')).not.toBeNull();
    section.dispose();
  });

  it("shows Rust's refusal inline when the manifest moved on under the dialog", async () => {
    const { api, section } = await mount({
      sources: [source("a", { trusted: false, status: "needs_review" })],
      hiddenViews: [],
    });
    api.sourceTrustAnswer = vi.fn(async () => {
      throw new Error("the source's manifest changed after the prompt was shown; nothing was trusted");
    });
    rows(section)[0].querySelector<HTMLButtonElement>('[data-action="trustSource"]')!.click();
    await FLUSH();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!.click();
    await FLUSH();
    await FLUSH();
    const err = section.element.querySelector<HTMLDivElement>('[data-role="sourcesError"]')!;
    expect(err.style.display).not.toBe("none");
    expect(err.textContent).toMatch(/nothing was trusted/);
    section.dispose();
  });

  it("hide switches are per view, default on, and persist through the store", async () => {
    const { api, section, store } = await mount({ sources: [source("a")], hiddenViews: ["prs"] });
    const toggle = (view: string) => section.element.querySelector<HTMLInputElement>(`input[data-view="${view}"]`)!;
    expect(toggle("work").checked).toBe(true);
    expect(toggle("prs").checked).toBe(false);
    expect(toggle("attention").checked).toBe(true);
    expect(toggle("environment").checked).toBe(true);
    toggle("work").checked = false;
    toggle("work").dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.setSourceViewHidden).toHaveBeenCalledWith("work", true);
    expect(store.get().hiddenViews.sort()).toEqual(["prs", "work"]);
    toggle("prs").checked = true;
    toggle("prs").dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.setSourceViewHidden).toHaveBeenCalledWith("prs", false);
    // The row and its data are untouched by hiding.
    expect(rows(section)).toHaveLength(1);
    section.dispose();
  });

  it("attaches with both fields, reports Rust's refusal, and detaches", async () => {
    const { api, section } = await mount({ sources: [], hiddenViews: [] });
    const addr = section.element.querySelector<HTMLInputElement>('[data-field="attachAddr"]')!;
    const file = section.element.querySelector<HTMLInputElement>('[data-field="attachTokenFile"]')!;
    const attach = section.element.querySelector<HTMLButtonElement>('[data-action="attachSource"]')!;
    const err = section.element.querySelector<HTMLDivElement>('[data-role="attachError"]')!;
    attach.click();
    await FLUSH();
    expect(api.attachSource).not.toHaveBeenCalled();
    expect(err.textContent).toMatch(/Both/);
    addr.value = "10.0.0.5:7000";
    file.value = "/tmp/token";
    attach.click();
    await FLUSH();
    expect(api.attachSource).toHaveBeenCalledWith("10.0.0.5:7000", "/tmp/token");
    expect(err.textContent).toMatch(/not a loopback address/);
    addr.value = "127.0.0.1:7000";
    attach.click();
    await FLUSH();
    await FLUSH();
    expect(err.style.display).toBe("none");
    expect(addr.value).toBe("");
    expect(rows(section)).toHaveLength(1);
    rows(section)[0].querySelector<HTMLButtonElement>('[data-action="detachSource"]')!.click();
    await FLUSH();
    await FLUSH();
    expect(api.detachSource).toHaveBeenCalledWith("att");
    expect(rows(section)).toHaveLength(0);
    section.dispose();
  });
});

describe("SettingsPane — Sources tab", () => {
  it("renders the Sources tab between Agents and Timers and activates it on open", async () => {
    const harness = stubSettings();
    await harness.store.load();
    const api = fakeApi({ sources: [source("a")], hiddenViews: [] });
    const pane = new SettingsPane({ store: harness.store, sources: new SourcesStore(api) });
    const labels = [...pane.element.querySelectorAll<HTMLButtonElement>(".chappa-settings-tab")].map((b) => b.textContent);
    expect(labels).toEqual(["Terminal", "Agents", "Sources", "Timers"]);
    pane.open("sources");
    await FLUSH();
    expect(pane.activeTab).toBe("sources");
    expect(api.listSources).toHaveBeenCalled();
    expect(pane.sourcesSection.element.querySelectorAll(".chappa-source-row")).toHaveLength(1);
    pane.dispose();
  });
});
