// In-app banner stack. Bottom-right INSIDE the window.
//
// The routing rule's focused half: when `decide()` says osNotify and the chappa-ai
// WINDOW has focus, an OS toast is the wrong tool (the case: I'm working in
// project B and project A needs a permission; I can quickly look at it —
// and dev-build toasts attribute to PowerShell with dead clicks). The event's
// terminal is necessarily NON-active (an active+focused terminal's event was
// suppressed before it got here), so the banner's click-to-focus always has
// somewhere to go — the click the OS toast can't deliver (plugin limitation
// recorded).
//
// Behaviour: transient card, house palette, title + body +
// attribution line, auto-dismiss ~5s with hover-pause, stack up to 3 with the
// OLDER cards collapsing into a "+N more" that opens the center.
//
// Decisions made here (documented, not hidden):
//  - hover-pause clears the card's timer; leaving re-arms the FULL duration
//    (remaining-time bookkeeping buys nothing for a 5s card);
//  - a card collapsed into "+N more" is not resurrected when a visible card
//    expires — it already lives in the center, which "+N more" opens; the
//    counter resets when the stack empties.
//
// Timers are the globals; tests drive them with vi.useFakeTimers() instead
// of a seam.

import { attributionText, type CenterEntry } from "./notify_center";

const BANNER_CSS = `
.chappa-banner-stack{position:fixed;right:12px;bottom:12px;z-index:55;display:flex;flex-direction:column;gap:8px;width:300px;}
.chappa-banner-more{border:1px solid #2a2c31;border-radius:6px;background:#16181d;color:#8b949e;font:12px system-ui,sans-serif;text-align:center;padding:5px 8px;cursor:pointer;box-shadow:0 4px 16px rgba(0,0,0,.5);}
.chappa-banner-more:hover{background:#1d2127;color:#d6d8dc;}
.chappa-banner{background:#1b1d21;border:1px solid #2a2c31;border-radius:6px;padding:8px 10px;box-shadow:0 4px 16px rgba(0,0,0,.5);cursor:pointer;color:#d6d8dc;font:13px system-ui,sans-serif;text-align:left;display:block;width:100%;box-sizing:border-box;}
.chappa-banner:hover{background:#1d2127;}
.chappa-banner-title{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-banner-body{color:#adb3bd;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-banner-attrib{margin-top:4px;font:11px ui-monospace,monospace;color:#6b7280;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
`;

let bannerStylesInjected = false;

function injectBannerStyles(): void {
  if (bannerStylesInjected || typeof document === "undefined") return;
  bannerStylesInjected = true;
  const el = document.createElement("style");
  el.textContent = BANNER_CSS;
  document.head.appendChild(el);
}

interface BannerCard {
  entry: CenterEntry;
  el: HTMLButtonElement;
  timer: ReturnType<typeof setTimeout> | null;
}

export interface NotifyBannersOptions {
  /** Where the stack attaches (the app root — it tears down with the app). */
  container: HTMLElement;
  /** Card click: focus that terminal (activate + project switch) + mark read.
   *  The banner dismisses itself around the callback. */
  onFocus: (entry: CenterEntry) => void;
  /** "+N more" click: open the notification center drawer. */
  onOpenCenter: () => void;
  /** Attribution rendering needs project names. */
  projectLabel: (id: number) => string;
  /** Auto-dismiss delay; default ~5s. */
  autoDismissMs?: number;
}

export class NotifyBanners {
  readonly element: HTMLElement;
  private readonly opts: NotifyBannersOptions;
  private readonly delay: number;
  private readonly more: HTMLButtonElement;
  /** Visible cards, oldest first (render order top→bottom, newest nearest
   *  the corner). */
  private cards: BannerCard[] = [];
  private overflow = 0;

  constructor(opts: NotifyBannersOptions) {
    this.opts = opts;
    this.delay = opts.autoDismissMs ?? 5000;
    injectBannerStyles();
    this.element = document.createElement("div");
    this.element.className = "chappa-banner-stack";
    this.element.style.display = "none";
    this.more = document.createElement("button");
    this.more.className = "chappa-banner-more";
    this.more.style.display = "none";
    this.more.addEventListener("mousedown", (e) => e.preventDefault());
    this.more.addEventListener("click", () => this.opts.onOpenCenter());
    this.element.appendChild(this.more);
    opts.container.appendChild(this.element);
  }

  /** Show one card for a just-recorded center entry. */
  show(entry: CenterEntry): void {
    const el = document.createElement("button");
    el.className = "chappa-banner";
    const title = document.createElement("div");
    title.className = "chappa-banner-title";
    title.textContent = entry.title;
    el.appendChild(title);
    if (entry.body.trim() !== "" && entry.body !== entry.title) {
      const body = document.createElement("div");
      body.className = "chappa-banner-body";
      body.textContent = entry.body;
      el.appendChild(body);
    }
    const attrib = document.createElement("div");
    attrib.className = "chappa-banner-attrib";
    attrib.textContent = attributionText(entry, this.opts.projectLabel);
    el.appendChild(attrib);

    const card: BannerCard = { entry, el, timer: null };
    // preventDefault keeps the terminal textarea focused (the rail pattern);
    // the click still lands.
    el.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", () => {
      this.remove(card);
      this.opts.onFocus(entry);
    });
    // Hover-pause: entering clears the timer, leaving re-arms it in full.
    el.addEventListener("mouseenter", () => {
      if (card.timer !== null) clearTimeout(card.timer);
      card.timer = null;
    });
    el.addEventListener("mouseleave", () => this.arm(card));

    this.cards.push(card);
    this.element.appendChild(el);
    this.element.style.display = "flex";
    this.arm(card);
    // Over 3: the OLDEST visible card collapses into "+N more".
    while (this.cards.length > 3) {
      const oldest = this.cards.shift()!;
      if (oldest.timer !== null) clearTimeout(oldest.timer);
      oldest.el.remove();
      this.overflow += 1;
    }
    this.renderMore();
  }

  /** Visible card count (tests). */
  get visibleCount(): number {
    return this.cards.length;
  }

  dispose(): void {
    for (const c of this.cards) if (c.timer !== null) clearTimeout(c.timer);
    this.cards = [];
    this.element.remove();
  }

  private arm(card: BannerCard): void {
    if (card.timer !== null) clearTimeout(card.timer);
    card.timer = setTimeout(() => this.remove(card), this.delay);
  }

  private remove(card: BannerCard): void {
    if (card.timer !== null) clearTimeout(card.timer);
    card.timer = null;
    card.el.remove();
    this.cards = this.cards.filter((c) => c !== card);
    if (this.cards.length === 0) {
      // Stack emptied: the collapsed remainder lives in the center, which is
      // where "+N more" pointed all along.
      this.overflow = 0;
      this.element.style.display = "none";
    }
    this.renderMore();
  }

  private renderMore(): void {
    if (this.overflow > 0) {
      this.more.textContent = `+${this.overflow} more`;
      this.more.title = "Open notifications";
      this.more.style.display = "block";
    } else {
      this.more.style.display = "none";
    }
  }
}
