import { describe, expect, it, vi } from "vitest";
import {
  RAIL_VIEWS,
  SourcesStore,
  originLabel,
  servedBy,
  statusLabel,
  trustMessage,
  visibleViews,
  type Source,
  type SourcesApi,
  type SourcesListing,
} from "./sources_store";

function source(id: string, over: Partial<Source> = {}): Source {
  return {
    id,
    label: id,
    origin: { kind: "launched", project_id: 1, project_path: "C:\\code\\app\\", command: `${id}-cmd` },
    status: "up",
    lastError: null,
    trusted: true,
    trust: { id, label: id, linkHosts: ["github.com"], roots: [], views: ["work"], hash: "h".repeat(64) },
    manifestId: id,
    version: "0.1.0",
    views: [{ name: "work", pollSeconds: 60, asOf: null, nextUtc: null, refreshing: false, lastError: null, hasPayload: false }],
    warning: null,
    promptPolicy: "first_time",
    promptsShown: [],
    jobs: [],
    ...over,
  };
}

function view(name: string) {
  return { name, pollSeconds: 60, asOf: null, nextUtc: null, refreshing: false, lastError: null, hasPayload: false };
}

describe("visibleViews", () => {
  it("lists the views trusted sources serve, in rail order, each once", () => {
    const listing: SourcesListing = {
      sources: [
        source("b", { views: [view("environment"), view("prs"), view("workflows")] }),
        source("a", { views: [view("prs"), view("work")] }),
      ],
      hiddenViews: [],
    };
    expect(visibleViews(listing)).toEqual(["work", "prs", "environment"]);
  });

  it("ignores untrusted sources but keeps a trusted one that is down", () => {
    const listing: SourcesListing = {
      sources: [
        source("review", { trusted: false, status: "needs_review", views: [view("work")] }),
        source("old", { trusted: false, status: "needs_update", views: [view("prs")] }),
        source("down", { trusted: true, status: "down", views: [view("environment")] }),
      ],
      hiddenViews: [],
    };
    expect(visibleViews(listing)).toEqual(["environment"]);
    expect(servedBy(listing, "environment").map((s) => s.id)).toEqual(["down"]);
    expect(servedBy(listing, "work")).toEqual([]);
  });

  it("drops hidden views and answers nothing with no sources", () => {
    const listing: SourcesListing = {
      sources: [source("a", { views: RAIL_VIEWS.map(view) })],
      hiddenViews: ["prs", "attention"],
    };
    expect(visibleViews(listing)).toEqual(["work", "environment"]);
    expect(visibleViews({ sources: [], hiddenViews: [] })).toEqual([]);
  });
});

describe("labels", () => {
  it("names every status", () => {
    expect(statusLabel("starting")).toBe("Starting");
    expect(statusLabel("needs_review")).toBe("Needs review");
    expect(statusLabel("needs_update")).toBe("Needs update");
    expect(statusLabel("up")).toBe("Up");
    expect(statusLabel("down")).toBe("Down");
  });

  it("describes where a source comes from", () => {
    expect(originLabel(source("feed"))).toBe("feed-cmd · app");
    expect(originLabel(source("x", { origin: { kind: "launched", project_id: 2, project_path: "/srv/proj", command: "node s.mjs" } }))).toBe(
      "node s.mjs · proj",
    );
    expect(originLabel(source("x", { origin: { kind: "attached", addr: "127.0.0.1:7000", token_file: "/t" } }))).toBe(
      "attached · 127.0.0.1:7000",
    );
  });

  it("builds the trust prompt from exactly what the hash covers", () => {
    const s = source("github", {
      label: "GitHub",
      trust: {
        id: "github",
        label: "GitHub",
        linkHosts: ["github.com", "gist.github.com"],
        roots: ["/srv/worktrees"],
        views: ["prs", "work"],
        hash: "abcdef0123456789" + "0".repeat(48),
      },
    });
    const text = trustMessage(s, s.trust!);
    expect(text).toContain('Trust source "GitHub" (github)?');
    expect(text).toContain("github-cmd · app");
    expect(text).toContain("    PRs");
    expect(text).toContain("    Work");
    expect(text).toContain("    github.com");
    expect(text).toContain("    gist.github.com");
    expect(text).toContain("    /srv/worktrees");
    expect(text).toContain("abcdef0123456789…");
    // Empty lists say so rather than vanishing: an approval of "nothing"
    // must read as that.
    const bare = trustMessage(s, { ...s.trust!, linkHosts: [], roots: [], views: [] });
    expect(bare.match(/\(none\)/g)).toHaveLength(3);
  });
});

describe("SourcesStore", () => {
  function fakeApi(initial: SourcesListing) {
    let listing = initial;
    let handler: ((e: { event: "sources" }) => void) | null = null;
    const api: SourcesApi & { fire: () => void; set: (l: SourcesListing) => void } = {
      fire: () => handler?.({ event: "sources" }),
      set: (l) => {
        listing = l;
      },
      listSources: vi.fn(async () => listing),
      setSourcePromptPolicy: vi.fn(async (id: string, policy: "first_time" | "always") => {
        listing = { ...listing, sources: listing.sources.map((s) => (s.id === id ? { ...s, promptPolicy: policy } : s)) };
        return listing;
      }),
      sourceTrustAnswer: vi.fn(async (id: string, trust: boolean) => {
        listing = {
          ...listing,
          sources: listing.sources.map((s) => (s.id === id && trust ? { ...s, trusted: true, status: "up" as const } : s)),
        };
        return listing;
      }),
      sourceView: vi.fn(async () => ({ payload: { items: [] }, freshness: { asOf: null } })),
      setSourceViewHidden: vi.fn(async (view: string, hidden: boolean) => {
        const hiddenViews = listing.hiddenViews.filter((v) => v !== view);
        if (hidden) hiddenViews.push(view);
        listing = { ...listing, hiddenViews };
        return listing;
      }),
      attachSource: vi.fn(async () => listing),
      detachSource: vi.fn(async () => listing),
      listen: vi.fn(async (h: (e: { event: "sources" }) => void) => {
        handler = h;
        return () => {
          handler = null;
        };
      }),
    };
    return api;
  }

  it("loads, notifies subscribers, and re-fetches on the live event", async () => {
    const api = fakeApi({ sources: [source("a", { trusted: false, status: "needs_review" })], hiddenViews: [] });
    const store = new SourcesStore(api);
    const seen: number[] = [];
    store.subscribe((l) => seen.push(l.sources.filter((s) => s.trusted).length));
    await store.load();
    expect(store.byId("a")?.status).toBe("needs_review");
    await store.listen();
    await store.listen(); // idempotent
    expect(api.listen).toHaveBeenCalledTimes(1);
    api.set({ sources: [source("a")], hiddenViews: [] });
    api.fire();
    await new Promise((r) => setTimeout(r, 0));
    expect(store.byId("a")?.trusted).toBe(true);
    expect(seen).toEqual([0, 1]);
    store.stopListening();
    api.set({ sources: [], hiddenViews: [] });
    api.fire();
    await new Promise((r) => setTimeout(r, 0));
    expect(store.get().sources).toHaveLength(1);
  });

  it("relays the trust answer with the hash it was given and adopts the reply", async () => {
    const api = fakeApi({ sources: [source("a", { trusted: false, status: "needs_review" })], hiddenViews: [] });
    const store = new SourcesStore(api);
    await store.load();
    await store.answerTrust("a", true, "h".repeat(64));
    expect(api.sourceTrustAnswer).toHaveBeenCalledWith("a", true, "h".repeat(64));
    expect(store.byId("a")?.trusted).toBe(true);
    await store.setViewHidden("prs", true);
    expect(store.get().hiddenViews).toEqual(["prs"]);
    expect(visibleViews(store.get())).toEqual(["work"]);
  });

  it("answers an empty listing when the backend is unreachable", async () => {
    const api = fakeApi({ sources: [source("a")], hiddenViews: [] });
    api.listSources = vi.fn(async () => {
      throw new Error("no backend");
    });
    const store = new SourcesStore(api);
    expect(await store.load()).toEqual({ sources: [], hiddenViews: [] });
  });
});
