// The Projects tab of the settings pane: every stored project as one row —
// its badge, a name field, the initials field, the colour swatches and a
// remove button — plus an "Add project…" line. The same edits the
// Edit-project pane offers per project, side by side for all of them, so a
// badge that came out wrong is fixed without leaving Settings.
//
// The section owns no project state: the app hands it a small adapter (list
// + the four mutations) and repaints it through `refresh()` from the same
// hook that repaints the switcher. Rows only rebuild when the project set
// or an identity actually changes, so a name being typed is never torn down
// under the cursor.

import { PROJECT_COLORS, deriveInitials, projectBadge, type ProjectColor } from "./project_badge";
import { INPUT_CSS, SMALL_BTN_CSS, settingsBlock, settingsSection } from "./settings_widgets";

/** One project as the section reads it. */
export interface ProjectRowInfo {
  id: number;
  name: string;
  path: string;
  initials: string | null;
  color: string | null;
  /** Open in the rail right now (the row says so; nothing else changes). */
  open: boolean;
}

/** What the app lends the section. Every mutation is the app's own flow
 *  (validation, confirms, repaint) — the section only asks. */
export interface ProjectsSettingsApi {
  list(): ProjectRowInfo[];
  rename(id: number, name: string): Promise<void>;
  setIdentity(id: number, initials: string | null, color: string | null): Promise<void>;
  remove(id: number): Promise<void>;
  add(): Promise<void>;
}

const ROW_CSS = "display:flex;align-items:center;gap:12px;flex-wrap:wrap;";
const FIELD_CSS = "display:flex;flex-direction:column;gap:3px;";
const FIELD_LABEL_CSS = "font:11px system-ui,sans-serif;color:#8b949e;";
const PATH_CSS =
  "flex:1 1 100%;font:11px ui-monospace,monospace;color:#6b7280;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";
const SWATCH_CSS =
  "flex:none;width:20px;height:20px;border-radius:50%;border:2px solid transparent;cursor:pointer;padding:0;" +
  "display:inline-flex;align-items:center;justify-content:center;font:11px system-ui,sans-serif;color:#8b949e;";
const EMPTY_CSS = "font:12px system-ui,sans-serif;color:#8b949e;padding:2px 0;";

/** Unique-id source for the `<label for>` associations. */
let ids = 0;

export class ProjectsSection {
  readonly element: HTMLDivElement;
  private readonly api: ProjectsSettingsApi;
  private readonly card: HTMLDivElement;
  private sig = "\u0000never";

  constructor(api: ProjectsSettingsApi) {
    this.api = api;
    this.element = document.createElement("div");
    this.element.className = "chappa-projects-section";
    const section = settingsSection(
      "Projects",
      "Every project this app knows. The badge (initials on a colour) marks the project's rows wherever they " +
        "appear outside its own section; blank fields derive it from the name.",
    );
    this.card = section.card;
    this.element.appendChild(section.element);
    this.refresh();
  }

  /** Re-read the list and rebuild the rows when something changed. */
  refresh(): void {
    const rows = this.api.list();
    const sig = rows
      .map((r) => [r.id, r.name, r.path, r.initials ?? "", r.color ?? "", r.open ? 1 : 0].join("\u001f"))
      .join("\u001e");
    if (sig === this.sig) return;
    this.sig = sig;
    this.card.textContent = "";
    const addBtn = document.createElement("button");
    addBtn.type = "button";
    addBtn.className = "chappa-projects-add";
    addBtn.textContent = "＋ Add project…";
    addBtn.style.cssText = SMALL_BTN_CSS;
    addBtn.dataset.action = "addProject";
    addBtn.addEventListener("click", () => void this.api.add());
    this.card.appendChild(settingsBlock(addBtn));
    if (rows.length === 0) {
      const empty = document.createElement("div");
      empty.style.cssText = EMPTY_CSS;
      empty.textContent = "No projects yet.";
      this.card.appendChild(settingsBlock(empty));
      return;
    }
    for (const row of rows) this.card.appendChild(this.buildRow(row));
  }

  dispose(): void {
    this.element.remove();
  }

  private buildRow(row: ProjectRowInfo): HTMLDivElement {
    const line = document.createElement("div");
    line.style.cssText = ROW_CSS;

    const badge = projectBadge(row);
    badge.dataset.role = "badge";

    const name = document.createElement("input");
    name.type = "text";
    name.value = row.name;
    name.spellcheck = false;
    name.style.cssText = `${INPUT_CSS}width:220px;`;
    name.dataset.field = "name";
    // Commit on change (blur/Enter): blank or unchanged is a no-op, a
    // refused rename snaps the field back.
    name.addEventListener("change", () => {
      const typed = name.value.trim();
      if (typed === "" || typed === row.name) {
        name.value = row.name;
        return;
      }
      void this.api.rename(row.id, typed).catch(() => {
        name.value = row.name;
      });
    });

    const initials = document.createElement("input");
    initials.type = "text";
    initials.maxLength = 3;
    initials.spellcheck = false;
    initials.value = row.initials ?? "";
    initials.placeholder = deriveInitials(row.name);
    initials.style.cssText = `${INPUT_CSS}width:52px;text-align:center;text-transform:uppercase;`;
    initials.dataset.field = "initials";
    initials.addEventListener("change", () => {
      const typed = initials.value.trim().toUpperCase();
      const next = typed === "" ? null : typed;
      if (next === (row.initials ?? null)) return;
      void this.api.setIdentity(row.id, next, row.color).catch(() => {
        initials.value = row.initials ?? "";
      });
    });

    const swatches = document.createElement("div");
    swatches.style.cssText = "display:flex;align-items:center;gap:4px;";
    swatches.setAttribute("role", "radiogroup");
    swatches.setAttribute("aria-label", `Badge colour for ${row.name}`);
    const pick = (color: ProjectColor | "auto"): void => {
      const next = color === "auto" ? null : color;
      if (next === (row.color ?? null)) return;
      void this.api.setIdentity(row.id, row.initials, next).catch(() => {});
    };
    const chosen = row.color ?? "auto";
    const swatch = (key: ProjectColor | "auto", title: string, bg: string, glyph = ""): void => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "chappa-projects-color";
      b.dataset.color = key;
      b.title = title;
      b.setAttribute("aria-label", title);
      b.setAttribute("aria-checked", String(key === chosen));
      b.textContent = glyph;
      b.style.cssText = `${SWATCH_CSS}background:${bg};border-color:${key === chosen ? "#e6e8ec" : "transparent"};`;
      b.addEventListener("click", () => pick(key));
      swatches.appendChild(b);
    };
    swatch("auto", "Colour from the name", "#16181d", "∅");
    for (const c of PROJECT_COLORS) swatch(c.key, c.label, c.bg);

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "chappa-projects-remove";
    remove.textContent = "Remove";
    remove.title = `Remove "${row.name}" from the app (files stay on disk)`;
    remove.style.cssText = `${SMALL_BTN_CSS}margin-left:auto;`;
    remove.dataset.action = "removeProject";
    remove.addEventListener("click", () => void this.api.remove(row.id));

    const path = document.createElement("div");
    path.style.cssText = PATH_CSS;
    path.textContent = row.open ? `${row.path} · open` : row.path;
    path.title = row.path;

    line.append(
      badge,
      this.field("Name", name),
      this.field("Initials", initials),
      this.field("Color", swatches),
      remove,
      path,
    );
    const block = settingsBlock(line);
    block.className += " chappa-projects-row";
    block.dataset.projectId = String(row.id);
    return block;
  }

  private field(label: string, control: HTMLElement): HTMLDivElement {
    const wrap = document.createElement("div");
    wrap.style.cssText = FIELD_CSS;
    const text = document.createElement("label");
    text.style.cssText = FIELD_LABEL_CSS;
    text.textContent = label;
    if (control instanceof HTMLInputElement) {
      if (!control.id) control.id = `chappa-projects-${label.toLowerCase()}-${++ids}`;
      text.htmlFor = control.id;
    }
    wrap.append(text, control);
    return wrap;
  }
}

