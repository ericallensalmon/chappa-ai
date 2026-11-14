import { describe, expect, it, vi } from "vitest";
import type * as ipc from "../ipc";
import { SourcesStore, type SourcesApi, type SourcesListing } from "../sources_store";
import { source } from "./test-utils";
import { SourceViewsData } from "./views_data";

const FLUSH = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

function harness(initial: SourcesListing) {
  let listing = initial;
  let fire: ((e: ipc.SourcesEvent) => void) | null = null;
  let n = 0;
  const api: SourcesApi = {
    listSources: vi.fn(async () => listing),
    sourceTrustAnswer: vi.fn(async () => listing),
    sourceView: vi.fn(async (id: string, view: string) => ({ payload: { read: `${id}/${view}#${++n}` }, freshness: { asOf: null } })),
    setSourceViewHidden: vi.fn(async () => listing),
    attachSource: vi.fn(async () => listing),
    detachSource: vi.fn(async () => listing),
    setSourcePromptPolicy: vi.fn(async () => listing),
    listen: vi.fn(async (h: (e: ipc.SourcesEvent) => void) => {
      fire = h;
      return () => {};
    }),
  };
  const store = new SourcesStore(api);
  const changes: Array<string | null> = [];
  const data = new SourceViewsData(store, (c) => changes.push(c ? `${c.sourceId}/${c.view}` : null));
  return {
    api,
    store,
    data,
    changes,
    set: (l: SourcesListing) => {
      listing = l;
    },
    fire: (e: ipc.SourcesEvent) => fire?.(e),
  };
}

describe("SourceViewsData", () => {
  it("reads every view the app shows from each trusted source, once, then again on its view event", async () => {
    const h = harness({
      sources: [source("a", ["prs", "attention", "environment"]), source("b", ["prs"], { trusted: false, status: "needs_review" })],
      hiddenViews: [],
    });
    h.data.start();
    await h.store.load();
    await h.store.listen();
    await FLUSH();
    expect((h.api.sourceView as ReturnType<typeof vi.fn>).mock.calls).toEqual([
      ["a", "prs"],
      ["a", "attention"],
    ]);
    expect(h.data.payloadsFor("prs").map((p) => [p.sourceId, (p.payload as { read: string }).read])).toEqual([["a", "a/prs#1"]]);
    expect(h.data.versions()).toEqual({ a: 2 });
    h.fire({ event: "view", source_id: "a", view: "prs" });
    h.fire({ event: "view", source_id: "a", view: "environment" });
    h.fire({ event: "view", source_id: "b", view: "prs" });
    await FLUSH();
    expect((h.data.payload("a", "prs")?.payload as { read: string }).read).toBe("a/prs#3");
    expect(h.changes.filter((c) => c !== null)).toEqual(["a/prs", "a/attention", "a/prs"]);
  });

  it("a source that leaves (or loses trust) drops out; a newly trusted one is read", async () => {
    const h = harness({ sources: [source("a", ["prs"])], hiddenViews: [] });
    h.data.start();
    await h.store.load();
    await FLUSH();
    expect(h.data.payloadsFor("prs").map((p) => p.sourceId)).toEqual(["a"]);
    h.set({ sources: [source("b", ["prs"]), source("a", ["prs"], { trusted: false })], hiddenViews: ["prs"] });
    await h.store.load();
    await FLUSH();
    expect(h.data.payloadsFor("prs").map((p) => [p.sourceId, p.payload !== null])).toEqual([["b", true]]);
    expect(h.data.payload("a", "prs")).toBeUndefined();
    expect(h.data.hidden("prs")).toBe(true);
    h.set({ sources: [source("c", [], { status: "needs_update", trusted: false })], hiddenViews: [] });
    await h.store.load();
    expect(h.data.needsUpdate()).toEqual([{ sourceId: "c", sourceLabel: "c" }]);
    h.data.dispose();
  });
});
