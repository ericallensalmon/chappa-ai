// The Sources tab of the settings pane: every source the app knows (its
// label, where it comes from, its status and last error, the views it
// serves with their freshness), a Trust button that opens the trust dialog
// for a source awaiting review, Detach for attached ones, the per-view hide
// switches, and the attach line (address + token file) for a server started
// elsewhere. A trusted source serving Work also gets its key-match line:
// whether its item keys join pull requests that mention them in a branch
// or title, and the pattern that finds them.
//
// House rules as the rest of the pane: toggles persist immediately through
// the store, which adopts what Rust returns; errors show inline under the
// control that caused them. The trust dialog is the shared confirm dialog
// with the same multi-line list shape the project trust prompt uses.

import { confirmDialog } from "./dialog";
import { DEFAULT_KEY_PATTERN, keyPatternError } from "./sources/work_merge";
import {
  RAIL_VIEWS,
  VIEW_LABELS,
  isRailView,
  originLabel,
  statusLabel,
  trustMessage,
  type RailView,
  type Source,
  type SourcesListing,
  type SourcesStore,
} from "./sources_store";
import {
  CHECKBOX_CSS,
  INPUT_CSS,
  SMALL_BTN_CSS,
  hint,
  settingsBlock,
  settingsRow,
  settingsSection,
} from "./settings_widgets";

const ROW_CSS = "display:flex;flex-direction:column;gap:4px;padding:8px 0;";
const HEAD_CSS = "display:flex;align-items:center;gap:8px;flex-wrap:wrap;";
const NAME_CSS = "font:13px system-ui,sans-serif;color:#d6d8dc;";
const ORIGIN_CSS = "font:11px ui-monospace,monospace;color:#8b949e;";
const META_CSS = "font:11px ui-monospace,monospace;color:#6b7280;";
const ERROR_CSS = "font:12px system-ui,sans-serif;color:#f0b429;white-space:pre-wrap;";
const EMPTY_CSS = "font:12px system-ui,sans-serif;color:#8b949e;padding:2px 0;";
const ATTACH_ROW_CSS = "display:flex;align-items:center;gap:6px;padding:4px 0;flex-wrap:wrap;";
/** Status chip tones: a problem reads amber or red, up reads green, the
 *  rest stays neutral. */
const CHIP_CSS: Record<Source["status"], string> = {
  starting: "color:#8b949e;border-color:#262a31;",
  needs_review: "color:#f0b429;border-color:#5a4a1a;",
  needs_update: "color:#f0b429;border-color:#5a4a1a;",
  up: "color:#5fd38d;border-color:#1f4a2f;",
  down: "color:#ff6b6b;border-color:#5a1f1f;",
};
const CHIP_BASE_CSS =
  "flex:none;font:11px ui-monospace,monospace;padding:1px 6px;border:1px solid;border-radius:10px;";

export interface SourcesSectionOptions {
  store: SourcesStore;
  /** Where the trust dialog mounts. Defaults to document.body. */
  modalContainer?: HTMLElement;
}

export class SourcesSection {
  readonly element: HTMLDivElement;
  private readonly store: SourcesStore;
  private readonly modalContainer: HTMLElement;
  private readonly list: HTMLDivElement;
  private readonly error: HTMLDivElement;
  private readonly hideToggles = new Map<RailView, HTMLInputElement>();
  private readonly attachAddr: HTMLInputElement;
  private readonly attachFile: HTMLInputElement;
  private readonly attachError: HTMLDivElement;
  private listSig = "\u0000never";
  private unsubscribe: (() => void) | null = null;

  constructor(opts: SourcesSectionOptions) {
    this.store = opts.store;
    this.modalContainer = opts.modalContainer ?? document.body;
    this.element = document.createElement("div");
    this.element.className = "chappa-sources-section";

    const sources = settingsSection(
      "Sources",
      "Local processes that feed the Work, PRs, Needs you and Environment views. A source is a chappa.yml " +
        "command with a source: key, or a server started elsewhere attached below. The app polls a source " +
        "only after you have reviewed what it asks for; an untrusted source gets nothing.",
    );
    this.list = document.createElement("div");
    this.list.className = "chappa-sources-list";
    this.error = document.createElement("div");
    this.error.style.cssText = ERROR_CSS;
    this.error.style.display = "none";
    this.error.dataset.role = "sourcesError";
    sources.card.appendChild(settingsBlock(this.list, this.error));

    const attachRow = document.createElement("div");
    attachRow.style.cssText = ATTACH_ROW_CSS;
    this.attachAddr = document.createElement("input");
    this.attachAddr.type = "text";
    this.attachAddr.placeholder = "127.0.0.1:7000";
    this.attachAddr.spellcheck = false;
    this.attachAddr.style.cssText = `${INPUT_CSS}flex:0 1 160px;`;
    this.attachAddr.dataset.field = "attachAddr";
    this.attachFile = document.createElement("input");
    this.attachFile.type = "text";
    this.attachFile.placeholder = "Path to its token file";
    this.attachFile.spellcheck = false;
    this.attachFile.style.cssText = `${INPUT_CSS}flex:1 1 240px;`;
    this.attachFile.dataset.field = "attachTokenFile";
    const attachBtn = document.createElement("button");
    attachBtn.type = "button";
    attachBtn.textContent = "Attach";
    attachBtn.style.cssText = SMALL_BTN_CSS;
    attachBtn.dataset.action = "attachSource";
    attachBtn.addEventListener("click", () => void this.attach());
    attachRow.append(this.attachAddr, this.attachFile, attachBtn);
    this.attachError = document.createElement("div");
    this.attachError.style.cssText = ERROR_CSS;
    this.attachError.style.display = "none";
    this.attachError.dataset.role = "attachError";
    sources.card.appendChild(
      settingsBlock(
        hint(
          "Attach a source that runs outside this app: a loopback address and the file holding its token. " +
            "The file must be readable by you alone.",
        ),
        attachRow,
        this.attachError,
      ),
    );
    this.element.appendChild(sources.element);

    const views = settingsSection(
      "Views",
      "Each view has a tab in the activity bar, drawn dimmed until a trusted source serves it; Needs you " +
        "always has something to show, since terminals and agents can need you without any source. Hiding a " +
        "view keeps its sources, their data and their trust; its tab goes and it stops feeding Needs you and " +
        "its notifications.",
    );
    for (const view of RAIL_VIEWS) {
      const toggle = document.createElement("input");
      toggle.type = "checkbox";
      toggle.style.cssText = CHECKBOX_CSS;
      toggle.dataset.view = view;
      toggle.addEventListener("change", () => {
        void this.store.setViewHidden(view, !toggle.checked).catch((e) => this.showError(String(e)));
      });
      this.hideToggles.set(view, toggle);
      views.card.appendChild(settingsRow(`Show ${VIEW_LABELS[view]}`, toggle));
    }
    this.element.appendChild(views.element);

    this.sync(this.store.get());
  }

  /** Called by the pane on open: load, subscribe, follow live changes. */
  activate(): void {
    void this.store.load();
    void this.store.listen();
    this.unsubscribe?.();
    this.unsubscribe = this.store.subscribe((listing) => this.sync(listing));
  }

  deactivate(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  dispose(): void {
    this.deactivate();
    this.element.remove();
  }

  private showError(text: string): void {
    this.error.textContent = text;
    this.error.style.display = text ? "" : "none";
  }

  private async attach(): Promise<void> {
    const addr = this.attachAddr.value.trim();
    const file = this.attachFile.value.trim();
    this.attachError.style.display = "none";
    if (!addr || !file) {
      this.attachError.textContent = "Both the address and the token file are needed.";
      this.attachError.style.display = "";
      return;
    }
    try {
      await this.store.attach(addr, file);
      this.attachAddr.value = "";
      this.attachFile.value = "";
    } catch (e) {
      this.attachError.textContent = String(e instanceof Error ? e.message : e);
      this.attachError.style.display = "";
    }
  }

  /** Open the trust dialog for `source` and send the answer. The hash shown
   *  is the hash sent, so a manifest that changed meanwhile is refused by
   *  Rust and the next listing brings a fresh prompt. */
  async review(source: Source): Promise<void> {
    const trust = source.trust;
    if (!trust) return;
    const ok = await confirmDialog(trustMessage(source, trust), {
      okLabel: "Trust",
      cancelLabel: "Not now",
      // Like every trust prompt: a stray Enter answers "Not now".
      safeDefault: true,
      container: this.modalContainer,
    });
    try {
      await this.store.answerTrust(source.id, ok, trust.hash);
      this.showError("");
    } catch (e) {
      this.showError(String(e instanceof Error ? e.message : e));
    }
  }

  private sync(listing: SourcesListing): void {
    const hidden = new Set(listing.hiddenViews);
    for (const [view, toggle] of this.hideToggles) toggle.checked = !hidden.has(view);
    const sig = listing.sources
      .map((s) =>
        [
          s.id,
          s.label,
          s.status,
          s.lastError ?? "",
          s.trusted ? 1 : 0,
          s.trust?.hash ?? "",
          s.warning ?? "",
          s.promptPolicy,
          JSON.stringify(s.keyMatch ?? null),
          s.views.map((v) => [v.name, v.asOf ?? "", v.lastError ?? "", v.refreshing ? 1 : 0, v.hasPayload ? 1 : 0].join(",")).join(";"),
        ].join("\u001f"),
      )
      .join("\u001e");
    if (sig === this.listSig) return;
    this.listSig = sig;
    this.list.textContent = "";
    if (listing.sources.length === 0) {
      const empty = document.createElement("div");
      empty.style.cssText = EMPTY_CSS;
      empty.textContent = "No sources yet. Add source: {} to a chappa.yml command, or attach one below.";
      this.list.appendChild(empty);
      return;
    }
    for (const source of listing.sources) this.list.appendChild(this.row(source));
  }

  private row(source: Source): HTMLDivElement {
    const row = document.createElement("div");
    row.className = "chappa-source-row";
    row.style.cssText = ROW_CSS;
    row.dataset.sourceId = source.id;
    row.dataset.status = source.status;

    const head = document.createElement("div");
    head.style.cssText = HEAD_CSS;
    const name = document.createElement("span");
    name.style.cssText = NAME_CSS;
    name.textContent = source.version ? `${source.label} ${source.version}` : source.label;
    name.dataset.role = "label";
    const chip = document.createElement("span");
    chip.style.cssText = CHIP_BASE_CSS + CHIP_CSS[source.status];
    chip.textContent = statusLabel(source.status);
    chip.dataset.role = "status";
    head.append(name, chip);
    if (source.status === "needs_review" && source.trust) {
      const trustBtn = document.createElement("button");
      trustBtn.type = "button";
      trustBtn.textContent = "Review and trust…";
      trustBtn.style.cssText = SMALL_BTN_CSS;
      trustBtn.dataset.action = "trustSource";
      trustBtn.addEventListener("click", () => void this.review(source));
      head.appendChild(trustBtn);
    }
    if (source.trusted) {
      // How this source's agent prompts are shown before they run: once
      // per action (the default) or before every run.
      const policy = document.createElement("select");
      policy.style.cssText = SMALL_BTN_CSS;
      policy.dataset.action = "promptPolicy";
      policy.title = "When a prompt this source wrote is shown before an agent runs with it";
      for (const [value, label] of [["first_time", "Show prompts: first time only"], ["always", "Show prompts: always"]] as const) {
        const opt = document.createElement("option");
        opt.value = value;
        opt.textContent = label;
        policy.appendChild(opt);
      }
      policy.value = source.promptPolicy;
      policy.addEventListener("change", () => {
        void this.store.setPromptPolicy(source.id, policy.value as Source["promptPolicy"]).catch((e) => this.showError(String(e)));
      });
      head.appendChild(policy);
    }
    if (source.origin.kind === "attached") {
      const detachBtn = document.createElement("button");
      detachBtn.type = "button";
      detachBtn.textContent = "Detach";
      detachBtn.style.cssText = SMALL_BTN_CSS;
      detachBtn.dataset.action = "detachSource";
      detachBtn.addEventListener("click", () => {
        void this.store.detach(source.id).catch((e) => this.showError(String(e)));
      });
      head.appendChild(detachBtn);
    }
    row.appendChild(head);

    const origin = document.createElement("div");
    origin.style.cssText = ORIGIN_CSS;
    origin.textContent = originLabel(source);
    origin.dataset.role = "origin";
    row.appendChild(origin);

    if (source.trusted && source.views.some((v) => v.name === "work")) row.appendChild(this.keyMatchLine(source));

    if (source.lastError) {
      const err = document.createElement("div");
      err.style.cssText = ERROR_CSS;
      err.textContent = source.lastError;
      err.dataset.role = "lastError";
      row.appendChild(err);
    }
    if (source.warning) {
      const warn = document.createElement("div");
      warn.style.cssText = ERROR_CSS;
      warn.textContent = source.warning;
      warn.dataset.role = "warning";
      row.appendChild(warn);
    }
    if (source.views.length > 0) {
      const views = document.createElement("div");
      views.style.cssText = META_CSS;
      views.dataset.role = "views";
      views.textContent = source.views
        .map((v) => {
          const label = isRailView(v.name) ? VIEW_LABELS[v.name] : v.name;
          const when = v.refreshing ? "refreshing" : v.asOf ? `as of ${v.asOf}` : v.hasPayload ? "cached" : "no data yet";
          return `${label}: ${when}${v.lastError ? ` (${v.lastError})` : ""}`;
        })
        .join(" · ");
      row.appendChild(views);
    }
    return row;
  }

  /** The key-match switch and pattern of a source serving Work. The pattern
   *  is checked here before it is stored; a blank one is the default. */
  private keyMatchLine(source: Source): HTMLElement {
    const rule = source.keyMatch ?? { enabled: true, pattern: null };
    const line = document.createElement("div");
    line.style.cssText = ATTACH_ROW_CSS;
    line.dataset.role = "keyMatch";
    const label = document.createElement("label");
    label.style.cssText = "display:flex;align-items:center;gap:6px;font:12px system-ui,sans-serif;color:#adb3bd;";
    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.style.cssText = CHECKBOX_CSS;
    toggle.checked = rule.enabled;
    toggle.dataset.action = "keyMatch";
    label.append(toggle, document.createTextNode("Join PRs that name an item key in their branch or title"));
    const pattern = document.createElement("input");
    pattern.type = "text";
    pattern.style.cssText = INPUT_CSS + "width:180px;font-family:ui-monospace,monospace;";
    pattern.placeholder = DEFAULT_KEY_PATTERN;
    pattern.value = rule.pattern ?? "";
    pattern.disabled = !rule.enabled;
    pattern.dataset.action = "keyPattern";
    pattern.title = "Finds candidate keys (case-sensitive); a PR joins an item only when what it finds is exactly the item's key, as a whole word";
    const err = document.createElement("span");
    err.style.cssText = ERROR_CSS;
    err.dataset.role = "keyPatternError";
    const save = (): void => {
      const why = keyPatternError(pattern.value);
      err.textContent = why ?? "";
      if (why) return;
      const text = pattern.value.trim();
      void this.store.setKeyMatch(source.id, toggle.checked, text === "" ? null : text).catch((e) => this.showError(String(e)));
    };
    toggle.addEventListener("change", save);
    pattern.addEventListener("change", save);
    line.append(label, pattern, err);
    return line;
  }
}
