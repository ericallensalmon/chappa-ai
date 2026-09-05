// The notification center drawer. A RIGHT-SIDE overlay pane — a drawer,
// not fullscreen — over the terminal stack, listing the
// center's entries newest-first with attribution and a coarse relative
// timestamp. Dismiss is Escape / × / outside-click, via the shared dismiss
// helper (dismiss.ts) the four legacy menus were migrated onto.
//
// Read semantics ("unread dot clears on open", made precise):
// OPENING the drawer marks everything read — that is what clears the rail
// footer's unread badge — but the rows STILL render an unread dot for the
// entries that were unread at open time (snapshotted), so a glance shows
// what's new. Entries arriving while the drawer sits open render unread live.
// A row click focuses the entry's terminal (expand its project + activate),
// marks it read, and closes the drawer so the terminal is actually visible.

import { registerDismiss } from "./dismiss";
import {
  attributionText,
  relativeTime,
  type CenterEntry,
  type NotifyCenter,
} from "./notify_center";

const DRAWER_CSS = `
.chappa-drawer{position:fixed;top:0;right:0;bottom:0;z-index:50;width:320px;display:none;flex-direction:column;background:#101216;border-left:1px solid #22252a;color:#d6d8dc;font:13px system-ui,sans-serif;box-shadow:-8px 0 24px rgba(0,0,0,.45);}
.chappa-drawer.open{display:flex;}
.chappa-drawer-header{flex:none;display:flex;align-items:center;gap:8px;padding:10px 12px;border-bottom:1px solid #1c1f24;}
.chappa-drawer-title{flex:1 1 auto;font:11px ui-monospace,monospace;color:#6b7280;letter-spacing:.06em;}
.chappa-drawer-clear{flex:none;border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:11px system-ui,sans-serif;cursor:pointer;padding:3px 8px;}
.chappa-drawer-clear:hover{background:#1d2127;color:#d6d8dc;}
.chappa-drawer-close{flex:none;border:0;background:none;color:#6b7280;font-size:15px;line-height:1;cursor:pointer;padding:2px 4px;border-radius:4px;}
.chappa-drawer-close:hover{color:#d6d8dc;background:#1d2127;}
.chappa-drawer-list{flex:1 1 auto;overflow-y:auto;padding:6px;}
.chappa-drawer-row{position:relative;display:block;width:100%;text-align:left;background:none;border:0;border-radius:6px;padding:7px 8px 7px 18px;color:#d6d8dc;font:13px system-ui,sans-serif;cursor:pointer;box-sizing:border-box;}
.chappa-drawer-row:hover{background:#171a1f;}
.chappa-drawer-unread{position:absolute;left:6px;top:12px;width:7px;height:7px;border-radius:50%;background:#5ea6ff;}
.chappa-drawer-row-title{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-drawer-row-body{color:#adb3bd;margin-top:1px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-drawer-row-meta{margin-top:3px;display:flex;gap:8px;font:11px ui-monospace,monospace;color:#6b7280;}
.chappa-drawer-row-attrib{flex:1 1 auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-drawer-row-time{flex:none;}
.chappa-drawer-empty{padding:16px 10px;color:#6b7280;font:12px system-ui,sans-serif;text-align:center;}
`;

let drawerStylesInjected = false;

function injectDrawerStyles(): void {
  if (drawerStylesInjected || typeof document === "undefined") return;
  drawerStylesInjected = true;
  const el = document.createElement("style");
  el.textContent = DRAWER_CSS;
  document.head.appendChild(el);
}

export interface NotifyDrawerOptions {
  center: NotifyCenter;
  /** Where the drawer attaches (the app root — it tears down with the app). */
  container: HTMLElement;
  /** Row click: focus that terminal. The drawer marks read + closes itself. */
  onFocus: (entry: CenterEntry) => void;
  projectLabel: (id: number) => string;
  /** Clock for the relative timestamps (the app's injected clock). */
  now: () => number;
  /** Toggle anchors excluded from outside-click dismissal (the footer bell;
   *  the "+N more" overflow row). Without the exclusion a toggle press would
   *  dismiss on mousedown and re-open on the same click — the ▾-menu trap. */
  anchors?: readonly Node[];
}

export class NotifyDrawer {
  readonly element: HTMLElement;
  private readonly opts: NotifyDrawerOptions;
  private readonly list: HTMLElement;
  private readonly unbindDismiss: () => void;
  private readonly unsubscribe: () => void;
  /** The entries unread at open time — they keep their dot for this open even
   *  though opening marked them read (see the header comment). */
  private unreadAtOpen = new Set<CenterEntry>();

  constructor(opts: NotifyDrawerOptions) {
    this.opts = opts;
    injectDrawerStyles();
    this.element = document.createElement("aside");
    this.element.className = "chappa-drawer";

    const header = document.createElement("div");
    header.className = "chappa-drawer-header";
    const title = document.createElement("span");
    title.className = "chappa-drawer-title";
    title.textContent = "NOTIFICATIONS";
    const clear = document.createElement("button");
    clear.className = "chappa-drawer-clear";
    clear.textContent = "Clear all";
    clear.addEventListener("mousedown", (e) => e.preventDefault());
    clear.addEventListener("click", () => {
      this.unreadAtOpen.clear();
      this.opts.center.clear();
    });
    const close = document.createElement("button");
    close.className = "chappa-drawer-close";
    close.textContent = "×";
    close.title = "Close";
    close.addEventListener("mousedown", (e) => e.preventDefault());
    close.addEventListener("click", () => this.close());
    header.append(title, clear, close);

    this.list = document.createElement("div");
    this.list.className = "chappa-drawer-list";
    this.element.append(header, this.list);
    opts.container.appendChild(this.element);

    // Store changes while open (a new entry, a Clear all) re-render the list;
    // the subscription is cheap while closed (render is gated on isOpen).
    this.unsubscribe = opts.center.subscribe(() => {
      if (this.isOpen()) this.render();
    });
    this.unbindDismiss = registerDismiss({
      isOpen: () => this.isOpen(),
      dismiss: () => this.close(),
      inside: [this.element, ...(opts.anchors ?? [])],
      escape: true,
    });
  }

  isOpen(): boolean {
    return this.element.classList.contains("open");
  }

  open(): void {
    if (this.isOpen()) return;
    // "unread dot clears on open": snapshot what was unread (the rows keep
    // their dot for this open), then mark everything read — that clears the
    // footer badge.
    this.unreadAtOpen = new Set(
      this.opts.center.entries.filter((e) => !e.read),
    );
    this.opts.center.markAllRead();
    this.element.classList.add("open");
    this.render();
  }

  close(): void {
    if (!this.isOpen()) return;
    this.element.classList.remove("open");
    this.unreadAtOpen.clear();
  }

  toggle(): void {
    if (this.isOpen()) this.close();
    else this.open();
  }

  dispose(): void {
    this.unbindDismiss();
    this.unsubscribe();
    this.element.remove();
  }

  private render(): void {
    this.list.textContent = "";
    const entries = this.opts.center.entries;
    if (entries.length === 0) {
      const empty = document.createElement("div");
      empty.className = "chappa-drawer-empty";
      empty.textContent = "No notifications";
      this.list.appendChild(empty);
      return;
    }
    const now = this.opts.now();
    for (const entry of entries) {
      this.list.appendChild(this.buildRow(entry, now));
    }
  }

  private buildRow(entry: CenterEntry, now: number): HTMLElement {
    const row = document.createElement("button");
    row.className = "chappa-drawer-row";
    row.dataset.kind = entry.kind;
    if (!entry.read || this.unreadAtOpen.has(entry)) {
      const dot = document.createElement("span");
      dot.className = "chappa-drawer-unread";
      dot.title = "unread";
      row.appendChild(dot);
    }
    const title = document.createElement("div");
    title.className = "chappa-drawer-row-title";
    title.textContent = entry.title;
    row.appendChild(title);
    if (entry.body.trim() !== "" && entry.body !== entry.title) {
      const body = document.createElement("div");
      body.className = "chappa-drawer-row-body";
      body.textContent = entry.body;
      row.appendChild(body);
    }
    const meta = document.createElement("div");
    meta.className = "chappa-drawer-row-meta";
    const attrib = document.createElement("span");
    attrib.className = "chappa-drawer-row-attrib";
    attrib.textContent = attributionText(entry, this.opts.projectLabel);
    const time = document.createElement("span");
    time.className = "chappa-drawer-row-time";
    time.textContent = relativeTime(entry.ts, now);
    meta.append(attrib, time);
    row.appendChild(meta);
    row.addEventListener("mousedown", (e) => e.preventDefault());
    row.addEventListener("click", () => {
      this.opts.center.markRead(entry);
      this.close();
      this.opts.onFocus(entry);
    });
    return row;
  }
}
