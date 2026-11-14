// The views' data: the cached payload of every (trusted source, view) pair
// the views read, kept current from the sources store. A pair is read when a
// trusted source starts serving it and again on every `term://sources` view
// event for it; a source that is gone or no longer trusted is dropped.
//
// Reads go through `sourcesStore.view` (Rust's cache): nothing here talks to
// a source.

import type { SourcesListing, SourcesStore } from "../sources_store";
import type { NeedsUpdate, ViewPayload } from "./prs_model";

/** The views whose payloads the app reads today. */
export const DATA_VIEWS = ["work", "prs", "attention", "environment"] as const;
export type DataView = (typeof DATA_VIEWS)[number];

const pairKey = (sourceId: string, view: string): string => `${sourceId}\u001f${view}`;

export class SourceViewsData {
  private listing: SourcesListing = { sources: [], hiddenViews: [] };
  private readonly payloads = new Map<string, ViewPayload>();
  /** Bumped per read, so a slow read never overwrites a newer one. */
  private readonly seq = new Map<string, number>();
  /** Per source, bumped whenever one of its payloads was re-read. */
  private readonly version = new Map<string, number>();
  private readonly unsubs: Array<() => void> = [];

  /**
   * `onChange(sourceId, view)` runs after a pair was (re)read, and with
   * `null` after the listing changed what is served.
   */
  constructor(
    private readonly store: SourcesStore,
    private readonly onChange: (change: { sourceId: string; view: DataView } | null) => void,
  ) {}

  /** Follow the store. The caller loads and listens on the store itself. */
  start(): void {
    this.unsubs.push(this.store.subscribe((listing) => this.adopt(listing)));
    this.unsubs.push(
      this.store.subscribeViews((sourceId, view) => {
        if ((DATA_VIEWS as readonly string[]).includes(view) && this.serves(sourceId, view)) {
          void this.read(sourceId, view as DataView);
        }
      }),
    );
    this.adopt(this.store.get());
  }

  dispose(): void {
    for (const u of this.unsubs) u();
    this.unsubs.length = 0;
  }

  /** The current listing, as last adopted. */
  get current(): SourcesListing {
    return this.listing;
  }

  /** Whether `view` is switched off in Settings. */
  hidden(view: string): boolean {
    return this.listing.hiddenViews.includes(view);
  }

  /** The trusted sources serving `view`, in listing order, each with its
   *  payload as last read (null before the first read answers). */
  payloadsFor(view: DataView): ViewPayload[] {
    return this.listing.sources
      .filter((s) => s.trusted && s.views.some((v) => v.name === view))
      .map(
        (s) =>
          this.payloads.get(pairKey(s.id, view)) ?? { sourceId: s.id, sourceLabel: s.label, payload: null, freshness: null },
      );
  }

  /** One pair's payload as last read, or undefined. */
  payload(sourceId: string, view: DataView): ViewPayload | undefined {
    return this.payloads.get(pairKey(sourceId, view));
  }

  /** Sources the app cannot read because they speak another contract. */
  needsUpdate(): NeedsUpdate[] {
    return this.listing.sources
      .filter((s) => s.status === "needs_update")
      .map((s) => ({ sourceId: s.id, sourceLabel: s.label }));
  }

  /** Per-source read counters (row action caches key on them). */
  versions(): Record<string, number> {
    return Object.fromEntries(this.version);
  }

  private serves(sourceId: string, view: string): boolean {
    const s = this.listing.sources.find((x) => x.id === sourceId);
    return !!s && s.trusted && s.views.some((v) => v.name === view);
  }

  private adopt(listing: SourcesListing): void {
    this.listing = listing;
    const wanted = new Set<string>();
    for (const s of listing.sources) {
      if (!s.trusted) continue;
      for (const view of DATA_VIEWS) {
        if (!s.views.some((v) => v.name === view)) continue;
        const k = pairKey(s.id, view);
        wanted.add(k);
        if (!this.payloads.has(k) && !this.seq.has(k)) void this.read(s.id, view);
      }
    }
    for (const k of [...this.payloads.keys()]) if (!wanted.has(k)) this.payloads.delete(k);
    for (const k of [...this.seq.keys()]) if (!wanted.has(k)) this.seq.delete(k);
    this.onChange(null);
  }

  private async read(sourceId: string, view: DataView): Promise<void> {
    const k = pairKey(sourceId, view);
    const seq = (this.seq.get(k) ?? 0) + 1;
    this.seq.set(k, seq);
    let answer;
    try {
      answer = await this.store.view(sourceId, view);
    } catch {
      return; // the source went away mid-read; the listing will say so
    }
    if (this.seq.get(k) !== seq || !this.serves(sourceId, view)) return;
    const label = this.listing.sources.find((s) => s.id === sourceId)?.label ?? sourceId;
    this.payloads.set(k, { sourceId, sourceLabel: label, payload: answer.payload ?? null, freshness: answer.freshness ?? null });
    this.version.set(sourceId, (this.version.get(sourceId) ?? 0) + 1);
    this.onChange({ sourceId, view });
  }
}
