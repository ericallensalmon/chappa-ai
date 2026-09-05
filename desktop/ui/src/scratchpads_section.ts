// The read-only SCRATCHPADS subsection under the open project:
// one row per unarchived pad of the expanded project (name, updated_at,
// updated_by, revision), clicking a row opens the content in a modal with a
// Copy button. That is the whole v1 surface — agents write through the
// chappa-ai-mcp tools, you read here without leaving chappa-ai. No
// editing, no todos (MCP only in v1; both are future work).
//
// House rules (subsection precedent): uppercase muted header with a
// hairline rule and the count on the right; the ↻ affordance sits ON the
// header line. Built once and updated in place; a 15 s poll while a project
// is expanded keeps the list current as agents write (one invoke per tick,
// nothing while collapsed).

import type { ScratchpadDto, ScratchpadSummaryDto } from "./ipc";
import * as ipc from "./ipc";

/** The two reads the section needs (stubbed in tests). `includeGlobal`
 *  adds the global pads (project_id null) to a project's list — the rail
 *  shows both, since a plain Claude Code session (no CHAPPA_AI_PROJECT_ID)
 *  creates exactly those. */
export interface ScratchpadsApi {
  listScratchpads(projectId: number | null, includeGlobal: boolean): Promise<ScratchpadSummaryDto[]>;
  readScratchpad(id: number): Promise<ScratchpadDto>;
}

export const tauriScratchpadsApi: ScratchpadsApi = {
  listScratchpads: (projectId, includeGlobal) => ipc.listScratchpads(projectId, includeGlobal),
  readScratchpad: (id) => ipc.readScratchpad(id),
};

export interface ScratchpadsSectionOptions {
  api?: ScratchpadsApi;
  /** Where the modal mounts. Defaults to document.body. */
  modalContainer?: HTMLElement;
  /** Clipboard write; defaults to navigator.clipboard (the documented
   *  fallback — there is no Rust clipboard-write command). */
  copy?: (text: string) => Promise<void>;
  /** Poll interval while a project is expanded (ms); 0 disables. */
  pollMs?: number;
  now?: () => number;
}

const BACKDROP_CSS =
  "position:fixed;top:0;right:0;bottom:0;left:0;z-index:70;display:flex;align-items:center;" +
  "justify-content:center;background:rgba(0,0,0,.55);";
const BOX_CSS =
  "width:720px;max-width:calc(100vw - 32px);max-height:calc(100vh - 32px);display:flex;flex-direction:column;" +
  "background:#1b1d21;border:1px solid #2a2c31;border-radius:8px;padding:14px 16px;" +
  "color:#d6d8dc;font:13px system-ui,sans-serif;box-shadow:0 8px 32px rgba(0,0,0,.6);";
const META_CSS = "font:11px ui-monospace,monospace;color:#8b949e;padding:2px 0 8px;";
const PRE_CSS =
  "flex:1 1 auto;min-height:120px;overflow:auto;margin:0;padding:10px;background:#0d0e10;border:1px solid #2a2c31;" +
  "border-radius:4px;color:#d6d8dc;font:12px ui-monospace,monospace;white-space:pre-wrap;word-break:break-word;";
const BUTTON_ROW_CSS = "display:flex;justify-content:flex-end;gap:8px;padding-top:12px;";
const BTN_CSS =
  "border:1px solid #262a31;border-radius:4px;background:#16181d;color:#8b949e;font:12px system-ui,sans-serif;cursor:pointer;padding:4px 12px;";
const OK_BTN_CSS =
  "border:1px solid #3b6fb0;border-radius:4px;background:#244a7a;color:#e6edf3;font:12px system-ui,sans-serif;cursor:pointer;padding:4px 12px;";

let styled = false;
function injectStyles(): void {
  if (styled || typeof document === "undefined") return;
  styled = true;
  const style = document.createElement("style");
  style.textContent = `
.chappa-scratchpads{padding:0 0 4px;}
.chappa-scratchpad-refresh{flex:none;width:18px;height:18px;border:0;border-radius:4px;background:none;color:#6b7280;font:12px system-ui,sans-serif;cursor:pointer;padding:0;line-height:18px;}
.chappa-scratchpad-refresh:hover{background:#1d2127;color:#d6d8dc;}
.chappa-scratchpad-list{display:flex;flex-direction:column;}
.chappa-scratchpad-row{display:flex;flex-direction:column;gap:2px;width:100%;padding:6px 8px;border:0;border-radius:6px;background:none;color:inherit;font:13px system-ui,sans-serif;text-align:left;cursor:pointer;}
.chappa-scratchpad-row:hover{background:#171a1f;}
.chappa-scratchpad-name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-scratchpad-meta{font:11px ui-monospace,monospace;color:#6b7280;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.chappa-scratchpad-empty{padding:6px 8px;color:#6b7280;font:12px system-ui,sans-serif;}
.chappa-scratchpad-global{flex:none;margin-left:6px;padding:0 5px;border:1px solid #2a2c31;border-radius:3px;color:#6b7280;font:10px ui-monospace,monospace;line-height:14px;vertical-align:middle;}
.chappa-scratchpad-name{display:flex;align-items:center;min-width:0;}
.chappa-scratchpad-name-text{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
`;
  document.head.appendChild(style);
}

/** "just now", "3m ago", "2h ago", "5d ago" — the rail is glanceable, not a
 *  timestamp column. Exported for the tests. */
export function relativeTime(thenMs: number, nowMs: number): string {
  const s = Math.max(0, Math.round((nowMs - thenMs) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

/** "user" stays "user"; a numeric actor is a chappa-ai process id. */
export function actorLabel(actor: string): string {
  return /^\d+$/.test(actor) ? `process ${actor}` : actor;
}

export class ScratchpadsSection {
  readonly element: HTMLDivElement;
  private readonly api: ScratchpadsApi;
  private readonly modalContainer: HTMLElement;
  private readonly copyFn: (text: string) => Promise<void>;
  private readonly pollMs: number;
  private readonly now: () => number;
  private readonly count: HTMLSpanElement;
  private readonly list: HTMLDivElement;
  private projectId: number | null | undefined = undefined;
  private rows: ScratchpadSummaryDto[] = [];
  private timer: ReturnType<typeof setInterval> | null = null;
  private modal: HTMLDivElement | null = null;
  /** Guards a stale list answer landing after a project switch. */
  private fetchSeq = 0;

  constructor(opts: ScratchpadsSectionOptions = {}) {
    this.api = opts.api ?? tauriScratchpadsApi;
    this.modalContainer = opts.modalContainer ?? document.body;
    this.copyFn =
      opts.copy ??
      (async (text) => {
        await navigator.clipboard.writeText(text);
      });
    this.pollMs = opts.pollMs ?? 15_000;
    this.now = opts.now ?? Date.now;
    injectStyles();

    this.element = document.createElement("div");
    this.element.className = "chappa-scratchpads";
    this.element.style.display = "none";
    const header = document.createElement("div");
    header.className = "chappa-subsection-header";
    const label = document.createElement("span");
    label.className = "chappa-subsection-label";
    label.textContent = "SCRATCHPADS";
    this.count = document.createElement("span");
    this.count.className = "chappa-subsection-count";
    this.count.textContent = "0";
    const refresh = document.createElement("button");
    refresh.className = "chappa-scratchpad-refresh";
    refresh.textContent = "↻";
    refresh.title = "Refresh scratchpads";
    refresh.addEventListener("mousedown", (e) => e.preventDefault());
    refresh.addEventListener("click", () => void this.refresh());
    header.append(label, this.count, refresh);
    this.list = document.createElement("div");
    this.list.className = "chappa-scratchpad-list";
    this.element.append(header, this.list);
  }

  /** Show the pads of `projectId` (null = the global pads; undefined = no
   *  project expanded → hidden, polling stopped). Re-fetches only when the
   *  project actually changes — renderProject runs on every rail event. */
  setProject(projectId: number | null | undefined): void {
    if (projectId === this.projectId) return;
    this.projectId = projectId;
    this.rows = [];
    this.render();
    this.stopPolling();
    if (projectId === undefined) {
      this.element.style.display = "none";
      return;
    }
    this.element.style.display = "block";
    void this.refresh();
    if (this.pollMs > 0) {
      this.timer = setInterval(() => void this.refresh(), this.pollMs);
    }
  }

  /** Re-read the list. A failed read leaves the last rows in place. */
  async refresh(): Promise<void> {
    if (this.projectId === undefined) return;
    const seq = ++this.fetchSeq;
    const projectId = this.projectId;
    let rows: ScratchpadSummaryDto[];
    try {
      // A project's list also carries the global pads (marked in the row).
      rows = await this.api.listScratchpads(projectId, projectId !== null);
    } catch {
      return;
    }
    if (seq !== this.fetchSeq) return;
    this.rows = rows;
    this.render();
  }

  private render(): void {
    this.list.textContent = "";
    this.count.textContent = String(this.rows.length);
    if (this.rows.length === 0) {
      const empty = document.createElement("div");
      empty.className = "chappa-scratchpad-empty";
      empty.textContent = "No scratchpads yet — agents write them through chappa-ai-mcp.";
      this.list.appendChild(empty);
      return;
    }
    const now = this.now();
    for (const row of this.rows) {
      const btn = document.createElement("button");
      btn.className = "chappa-scratchpad-row";
      btn.dataset.scratchpadId = String(row.scratchpad_id);
      const name = document.createElement("span");
      name.className = "chappa-scratchpad-name";
      const nameText = document.createElement("span");
      nameText.className = "chappa-scratchpad-name-text";
      nameText.textContent = row.name;
      nameText.title = row.name;
      name.appendChild(nameText);
      if (row.project_id === null) {
        const tag = document.createElement("span");
        tag.className = "chappa-scratchpad-global";
        tag.textContent = "global";
        tag.title = "Global scratchpad (no project) — written by a session without CHAPPA_AI_PROJECT_ID";
        name.appendChild(tag);
      }
      const meta = document.createElement("span");
      meta.className = "chappa-scratchpad-meta";
      meta.textContent = `${relativeTime(row.updated_at, now)} · ${actorLabel(row.updated_by)} · r${row.revision}`;
      btn.append(name, meta);
      btn.addEventListener("mousedown", (e) => e.preventDefault());
      btn.addEventListener("click", () => void this.open(row.scratchpad_id));
      this.list.appendChild(btn);
    }
  }

  /** The content modal: name, meta line, the content, Copy + Close. */
  async open(id: number): Promise<void> {
    let pad: ScratchpadDto;
    try {
      pad = await this.api.readScratchpad(id);
    } catch (err) {
      this.showModal(null, `Could not read scratchpad ${id}: ${String(err)}`);
      return;
    }
    this.showModal(pad, pad.content);
  }

  private showModal(pad: ScratchpadDto | null, content: string): void {
    this.closeModal();
    const backdrop = document.createElement("div");
    backdrop.className = "chappa-scratchpad-modal";
    backdrop.style.cssText = BACKDROP_CSS;
    const box = document.createElement("div");
    box.style.cssText = BOX_CSS;
    const title = document.createElement("div");
    title.className = "chappa-scratchpad-modal-title";
    title.style.cssText = "font:14px system-ui,sans-serif;font-weight:600;";
    title.textContent = pad ? pad.name : "Scratchpad";
    const meta = document.createElement("div");
    meta.className = "chappa-scratchpad-modal-meta";
    meta.style.cssText = META_CSS;
    meta.textContent = pad
      ? `revision ${pad.revision} · updated ${relativeTime(pad.updated_at, this.now())} by ${actorLabel(pad.updated_by)}` +
        (pad.tags.length ? ` · ${pad.tags.join(", ")}` : "") +
        (pad.project_id === null ? " · global" : "")
      : "";
    const pre = document.createElement("pre");
    pre.className = "chappa-scratchpad-modal-content";
    pre.style.cssText = PRE_CSS;
    pre.textContent = content;
    const buttons = document.createElement("div");
    buttons.style.cssText = BUTTON_ROW_CSS;
    const copy = document.createElement("button");
    copy.className = "chappa-scratchpad-copy";
    copy.style.cssText = OK_BTN_CSS;
    copy.textContent = "Copy";
    copy.disabled = pad === null;
    copy.addEventListener("click", () => {
      void this.copyFn(content).then(
        () => {
          copy.textContent = "Copied";
        },
        () => {
          copy.textContent = "Copy failed";
        },
      );
    });
    const close = document.createElement("button");
    close.className = "chappa-scratchpad-close";
    close.style.cssText = BTN_CSS;
    close.textContent = "Close";
    close.addEventListener("click", () => this.closeModal());
    buttons.append(copy, close);
    box.append(title, meta, pre, buttons);
    backdrop.append(box);
    backdrop.addEventListener("click", (e) => {
      if (e.target === backdrop) this.closeModal();
    });
    backdrop.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.closeModal();
    });
    box.tabIndex = -1;
    this.modalContainer.appendChild(backdrop);
    this.modal = backdrop;
    close.focus();
  }

  closeModal(): void {
    this.modal?.remove();
    this.modal = null;
  }

  get modalOpen(): boolean {
    return this.modal !== null;
  }

  private stopPolling(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  dispose(): void {
    this.stopPolling();
    this.closeModal();
    this.projectId = undefined;
    this.rows = [];
    this.element.style.display = "none";
  }
}
