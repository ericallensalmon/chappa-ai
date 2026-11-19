import { describe, expect, it, vi } from "vitest";
import type * as ipc from "./ipc";
import {
  RAIL_VIEWS,
  SourcesStore,
  originLabel,
  servedBy,
  statusLabel,
  trustMessage,
  unservedText,
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
    expect(text).not.toContain("other sources' items");
    // What it offers on other sources' rows is part of what is trusted.
    const offering = trustMessage(s, {
      ...s.trust!,
      itemActions: [
        { id: "start-work", label: "Start work", view: "work", sources: ["tracker"], mutates: true, confirm: "Create {key}?" },
        { id: "later", label: "Review later", view: "prs", sources: ["*"], mutates: false, confirm: null },
      ],
    });
    expect(offering).toContain("Actions it offers on other sources' items:");
    expect(offering).toContain("    Start work (changes something, asks first) on Work rows of tracker");
    expect(offering).toContain("    Review later (read only) on PRs rows of every other source");
    expect(offering).not.toContain("each time one changes");
    // Being sent other sources' rows is part of it too.
    expect(trustMessage(s, { ...s.trust!, ownerUpdates: true })).toContain("It is sent the rows of other sources' items it adds to, each time one changes.");
    // An any-port entry says what it covers.
    const anyPort = trustMessage(s, { ...s.trust!, linkHosts: ["dev.example.test:*", "localhost:*", "dev.example.test:14001"] });
    expect(anyPort).toContain("    any port on dev.example.test\n");
    expect(anyPort).toContain("    any port on localhost\n");
    expect(anyPort).toContain("    dev.example.test:14001\n");
    expect(anyPort).not.toContain(":*");
    const bare = trustMessage(s, { ...s.trust!, linkHosts: [], roots: [], views: [] });
    expect(bare.match(/\(none\)/g)).toHaveLength(3);
    // A line break in the source's own text cannot fake a line.
    const forged = trustMessage(s, { ...s.trust!, roots: ["/srv\nHosts its links may open:\n    (none)"] });
    expect(forged).toContain("    /srv␊Hosts its links may open:␊    (none)");
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

  it("tells view subscribers which (source, view) moved, and still re-fetches the listing", async () => {
    const api = fakeApi({ sources: [source("a")], hiddenViews: [] });
    let fire: ((e: ipc.SourcesEvent) => void) | null = null;
    api.listen = vi.fn(async (h: (e: ipc.SourcesEvent) => void) => {
      fire = h;
      return () => {};
    });
    const store = new SourcesStore(api);
    const moved: string[] = [];
    const off = store.subscribeViews((id, view) => moved.push(`${id}/${view}`));
    await store.listen();
    fire!({ event: "view", source_id: "a", view: "prs" });
    fire!({ event: "sources" });
    fire!({ event: "job", source_id: "a", job_id: "j1" });
    await new Promise((r) => setTimeout(r, 0));
    expect(moved).toEqual(["a/prs"]);
    expect(api.listSources).toHaveBeenCalledTimes(3);
    off();
    fire!({ event: "view", source_id: "a", view: "work" });
    expect(moved).toEqual(["a/prs"]);
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

describe("unservedText", () => {
  const listing = (...sources: Source[]): SourcesListing => ({ sources, hiddenViews: [] });

  it("names a source that serves the view but waits for review", () => {
    const text = unservedText(listing(source("Stack scripted", { trusted: false, status: "needs_review", views: [view("environment")] })), "environment", "an environment");
    expect(text).toBe("Stack scripted serves an environment but is waiting for review.");
  });

  it("names several, and says when they wait on an update", () => {
    const a = source("A", { trusted: false, status: "needs_update", views: [view("prs")] });
    const b = source("B", { trusted: false, status: "needs_update", views: [view("prs")] });
    expect(unservedText(listing(a, b), "prs", "pull requests")).toBe("A and B serve pull requests but are waiting for you to review an update.");
  });

  it("ignores a waiting source that serves another view", () => {
    const text = unservedText(listing(source("T", { trusted: false, status: "needs_review", views: [view("work")] })), "environment", "an environment");
    expect(text).toMatch(/^No trusted source serves an environment\. Add one/);
  });

  it("says which source never answered, with its error", () => {
    const dead = source("stack", { status: "down", manifestId: null, views: [], trusted: false, trust: null, lastError: "the source process could not be started: program not found" });
    expect(unservedText(listing(dead), "environment", "an environment")).toBe(
      "No trusted source serves an environment. A source did not start (stack: the source process could not be started: program not found).",
    );
  });

  it("falls back to the general line, saying when a project's source starts", () => {
    expect(unservedText(listing(), "work", "work items")).toBe(
      "No trusted source serves work items. Add one as a source command in a project's chappa.yml (it starts once the project is open and trusted), or attach one in Settings ▸ Sources.",
    );
  });
});
