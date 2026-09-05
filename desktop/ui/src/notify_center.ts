// The notification center store. Session-only, in-memory.
//
// A ring of the notification events that PRODUCED something: an event is
// RECORDED iff `decide()` produced any output (badge or toast) —
// focused-suppressed events do NOT enter the center, matching the measured
// behavior ("a focused terminal's
// [OSC 9] produces
// nothing at all, center included"). The recording decision itself lives at
// the app layer's ONE routing point; this store never re-decides anything.
//
// Session-only in v1 — persisting the center across restarts is a known
// gap. Restart ⇒ empty center.
//
// No DOM, no ipc: pure state + subscribe, so the drawer/banner/badge surfaces
// and the tests all read one store.

/** What arrived (the wire vocabulary). */
export type CenterKind = "bell" | "notify";

/** What the caller knows at record time. */
export interface CenterEntryInput {
  termId: number;
  /** Owning project (process + project-shell entries), or null. */
  projectId: number | null;
  /** Attribution name: the process name for process terminals; the terminal's
   *  display label for project-bound and plain shells (the field name is
   *  historical — it is really "source name", so a plain shell can attribute
   *  by name without a second field). */
  processName: string | null;
  kind: CenterKind;
  title: string;
  body: string;
}

/** One recorded event. */
export interface CenterEntry extends CenterEntryInput {
  /** Milliseconds since epoch, from the injected clock. */
  ts: number;
  read: boolean;
}

export interface NotifyCenterOptions {
  /** Ring capacity; oldest entries fall off. */
  cap?: number;
  /** Clock injection (Date.now is defaulted at the APP layer — the app
   *  passes its own; tests pass a fake). */
  now?: () => number;
}

export class NotifyCenter {
  private readonly cap: number;
  private readonly now: () => number;
  /** Newest FIRST — the order every surface renders. */
  private ring: CenterEntry[] = [];
  private listeners = new Set<() => void>();

  constructor(opts: NotifyCenterOptions = {}) {
    this.cap = opts.cap ?? 200;
    this.now = opts.now ?? Date.now;
  }

  /** Record one non-suppressed event. Returns the stored entry (the banner
   *  layer carries it as its click payload). */
  record(input: CenterEntryInput): CenterEntry {
    const entry: CenterEntry = { ...input, ts: this.now(), read: false };
    this.ring.unshift(entry);
    if (this.ring.length > this.cap) this.ring.length = this.cap;
    this.notify();
    return entry;
  }

  /** Newest-first snapshot view. */
  get entries(): readonly CenterEntry[] {
    return this.ring;
  }

  get unread(): number {
    let n = 0;
    for (const e of this.ring) if (!e.read) n += 1;
    return n;
  }

  markRead(entry: CenterEntry): void {
    if (entry.read) return;
    entry.read = true;
    this.notify();
  }

  markAllRead(): void {
    let changed = false;
    for (const e of this.ring) {
      if (!e.read) {
        e.read = true;
        changed = true;
      }
    }
    if (changed) this.notify();
  }

  clear(): void {
    if (this.ring.length === 0) return;
    this.ring = [];
    this.notify();
  }

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private notify(): void {
    for (const fn of [...this.listeners]) fn();
  }
}

/** The "process · project" attribution line, shared by the banner cards and
 *  the drawer rows. Plain shells (no project) attribute name only. */
export function attributionText(
  entry: Pick<CenterEntry, "projectId" | "processName">,
  projectLabel: (id: number) => string,
): string {
  const name = entry.processName ?? "";
  if (entry.projectId === null) return name;
  const project = projectLabel(entry.projectId);
  return name ? `${name} · ${project}` : project;
}

/** Coarse relative timestamp for drawer rows ("2m ago"). Buckets only —
 *  nothing live-ticks, the drawer re-renders on open/change. */
export function relativeTime(ts: number, now: number): string {
  const s = Math.max(0, Math.floor((now - ts) / 1000));
  if (s < 60) return "just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}
