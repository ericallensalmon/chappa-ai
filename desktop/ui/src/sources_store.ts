// The sources store: one observable box over the source commands
// (`list_sources` and friends) plus the pure helpers the Settings tab and,
// later, the rail share. Same shape as agent_tools.ts: an injectable api
// seam for tests, a `subscribe` that fires on every change, and no view
// logic of its own — payloads come back as the source sent them.
//
// Rust owns everything that touches a source (addresses, tokens, polling,
// trust records); this store only mirrors the listing and relays answers.

import * as ipc from "./ipc";

export type Source = ipc.SourceDto;
export type SourceTrust = ipc.SourceTrustDto;
export type SourcesListing = ipc.SourcesListingDto;
export type SourceStatus = ipc.SourceStatusDto;
export type SourceView = ipc.SourceViewDto;

/** The rail views a source can serve, in rail order. */
export const RAIL_VIEWS = ["work", "prs", "attention", "environment"] as const;
export type RailView = (typeof RAIL_VIEWS)[number];

/** The user-facing name of each rail view. */
export const VIEW_LABELS: Record<RailView, string> = {
  work: "Work",
  prs: "PRs",
  attention: "Needs you",
  environment: "Environment",
};

export function isRailView(name: string): name is RailView {
  return (RAIL_VIEWS as readonly string[]).includes(name);
}

export interface SourcesApi {
  listSources(): Promise<SourcesListing>;
  sourceTrustAnswer(sourceId: string, trust: boolean, hash: string): Promise<SourcesListing>;
  sourceView(sourceId: string, view: string): Promise<SourceView>;
  setSourceViewHidden(view: string, hidden: boolean): Promise<SourcesListing>;
  attachSource(addr: string, tokenFile: string): Promise<SourcesListing>;
  detachSource(sourceId: string): Promise<SourcesListing>;
  setSourcePromptPolicy(sourceId: string, policy: ipc.SourcePromptPolicy): Promise<SourcesListing>;
  /** Absent in older test seams: the key-match controls then do nothing. */
  setSourceKeyMatch?(sourceId: string, enabled: boolean, pattern: string | null): Promise<SourcesListing>;
  /** Subscribe to `term://sources`; resolves the unlisten function. Absent
   *  outside Tauri (the store then only refreshes on demand). */
  listen?(handler: (event: ipc.SourcesEvent) => void): Promise<() => void>;
}

const tauriSourcesApi: SourcesApi = {
  listSources: ipc.listSources,
  sourceTrustAnswer: ipc.sourceTrustAnswer,
  sourceView: ipc.sourceView,
  setSourceViewHidden: ipc.setSourceViewHidden,
  attachSource: ipc.attachSource,
  detachSource: ipc.detachSource,
  setSourcePromptPolicy: ipc.setSourcePromptPolicy,
  setSourceKeyMatch: ipc.setSourceKeyMatch,
  // Decided at call time, not module load: this module is imported by code
  // that also runs outside a webview (tests, the plain-browser dev page).
  listen: async (handler) => {
    if (typeof window === "undefined" || !ipc.inTauri()) return () => {};
    return ipc.listenEvent<ipc.SourcesEvent>("term://sources", handler);
  },
};

const EMPTY: SourcesListing = { sources: [], hiddenViews: [] };

/**
 * The rail views that should appear: served by at least one TRUSTED source
 * and not hidden in Settings. A source that is down still counts (its data
 * is kept); one awaiting review or needing an update does not. Order is
 * rail order, never the sources' order.
 */
export function visibleViews(listing: SourcesListing): RailView[] {
  const hidden = new Set(listing.hiddenViews);
  return RAIL_VIEWS.filter((view) => !hidden.has(view) && servedBy(listing, view).length > 0);
}

/** The trusted sources whose manifest names `view`. */
export function servedBy(listing: SourcesListing, view: string): Source[] {
  return listing.sources.filter((s) => s.trusted && s.views.some((v) => v.name === view));
}

/** The status word the list shows. */
export function statusLabel(status: SourceStatus): string {
  switch (status) {
    case "starting":
      return "Starting";
    case "needs_review":
      return "Needs review";
    case "needs_update":
      return "Needs update";
    case "up":
      return "Up";
    case "down":
      return "Down";
  }
}

/** The trailing folder name of a path, for either separator. */
function baseName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut >= 0 ? trimmed.slice(cut + 1) : trimmed;
}

/** Where a source comes from, as one line: the command and its project, or
 *  the attached address. */
export function originLabel(source: Source): string {
  const o = source.origin;
  if (o.kind === "launched") return `${o.command} · ${baseName(o.project_path) || o.project_path}`;
  return `attached · ${o.addr}`;
}

/** One `linkHosts` entry as the trust dialog says it: `host:*` is "any
 *  port on host", anything else is shown as declared. */
export function linkHostText(entry: string): string {
  return entry.endsWith(":*") ? `any port on ${entry.slice(0, -2).toLowerCase()}` : entry;
}

/**
 * The trust dialog's text: what the source is, what it asks to be allowed
 * to do, and the hash the answer covers. The same shape as the project
 * trust prompt (a multi-line list the dialog renders with its breaks).
 */
export function trustMessage(source: Source, trust: SourceTrust): string {
  const lines = [`Trust source "${trust.label}" (${trust.id})?`, "", `From: ${originLabel(source)}`, ""];
  lines.push("Views it serves:");
  lines.push(...(trust.views.length ? trust.views.map((v) => `    ${isRailView(v) ? VIEW_LABELS[v] : v}`) : ["    (none)"]));
  lines.push("", "Hosts its links may open:");
  lines.push(...(trust.linkHosts.length ? trust.linkHosts.map((h) => `    ${linkHostText(h)}`) : ["    (none)"]));
  lines.push("", "Extra folders it asks to launch in:");
  lines.push(...(trust.roots.length ? trust.roots.map((r) => `    ${r}`) : ["    (none)"]));
  const offered = trust.itemActions ?? [];
  if (offered.length) {
    lines.push("", "Actions it offers on other sources' items:");
    for (const a of offered) {
      const where = a.sources.includes("*") ? "every other source" : a.sources.join(", ");
      const view = isRailView(a.view) ? VIEW_LABELS[a.view] : a.view;
      lines.push(`    ${a.label} (${a.mutates ? "changes something, asks first" : "read only"}) on ${view} rows of ${where}`);
    }
  }
  if (trust.ownerUpdates) {
    lines.push("", "It is sent the rows of other sources' items it adds to, each time one changes.");
  }
  lines.push("", `Trust covers exactly this: ${trust.hash.slice(0, 16)}…`, "A change to any of it asks again.");
  return lines.join("\n");
}

export class SourcesStore {
  private listing: SourcesListing = EMPTY;
  private readonly subs = new Set<(listing: SourcesListing) => void>();
  private readonly viewSubs = new Set<(sourceId: string, view: string) => void>();
  private unlisten: (() => void) | null = null;
  private listening = false;

  constructor(private readonly api: SourcesApi = tauriSourcesApi) {}

  /** The current listing (a shallow copy; sources are shared, treat as read-only). */
  get(): SourcesListing {
    return { sources: [...this.listing.sources], hiddenViews: [...this.listing.hiddenViews] };
  }

  byId(id: string): Source | undefined {
    return this.listing.sources.find((s) => s.id === id);
  }

  async load(): Promise<SourcesListing> {
    try {
      this.adopt(await this.api.listSources());
    } catch {
      this.adopt(EMPTY);
    }
    return this.get();
  }

  /** Follow `term://sources`: every change re-fetches the listing, and a
   *  `view` event also tells the view subscribers which payload moved.
   *  Idempotent. */
  async listen(): Promise<void> {
    if (this.listening || !this.api.listen) return;
    this.listening = true;
    try {
      this.unlisten = await this.api.listen((event) => {
        if (event.event === "view") for (const fn of this.viewSubs) fn(event.source_id, event.view);
        void this.load();
      });
    } catch {
      this.listening = false;
    }
  }

  stopListening(): void {
    this.unlisten?.();
    this.unlisten = null;
    this.listening = false;
  }

  /** The trust answer for `source`: `trust` records `hash`, else nothing. */
  async answerTrust(sourceId: string, trust: boolean, hash: string): Promise<void> {
    this.adopt(await this.api.sourceTrustAnswer(sourceId, trust, hash));
  }

  async setViewHidden(view: RailView, hidden: boolean): Promise<void> {
    this.adopt(await this.api.setSourceViewHidden(view, hidden));
  }

  async attach(addr: string, tokenFile: string): Promise<void> {
    this.adopt(await this.api.attachSource(addr, tokenFile));
  }

  async detach(sourceId: string): Promise<void> {
    this.adopt(await this.api.detachSource(sourceId));
  }

  /** When a source's agent prompts are shown: before every run, or once per
   *  action until accepted. */
  async setPromptPolicy(sourceId: string, policy: ipc.SourcePromptPolicy): Promise<void> {
    this.adopt(await this.api.setSourcePromptPolicy(sourceId, policy));
  }

  /** Whether a source's work item keys join PRs that mention them, and the
   *  pattern that finds them (blank = the default). */
  async setKeyMatch(sourceId: string, enabled: boolean, pattern: string | null): Promise<void> {
    if (!this.api.setSourceKeyMatch) return;
    this.adopt(await this.api.setSourceKeyMatch(sourceId, enabled, pattern));
  }

  /** One view's cached payload + freshness, straight from Rust. */
  view(sourceId: string, view: string): Promise<SourceView> {
    return this.api.sourceView(sourceId, view);
  }

  subscribe(fn: (listing: SourcesListing) => void): () => void {
    this.subs.add(fn);
    return () => this.subs.delete(fn);
  }

  /** Called with `(sourceId, view)` whenever a source's cached payload or
   *  freshness of that view changed (`term://sources` `view` events). */
  subscribeViews(fn: (sourceId: string, view: string) => void): () => void {
    this.viewSubs.add(fn);
    return () => this.viewSubs.delete(fn);
  }

  private adopt(listing: SourcesListing): void {
    this.listing = listing;
    for (const fn of this.subs) fn(this.get());
  }
}

/** The app-wide store (the Settings tab and the rail share one listing). */
export const sourcesStore = new SourcesStore();
