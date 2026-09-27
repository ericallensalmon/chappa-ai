// A project's badge: two or three letters on a coloured square, the mark
// that says "this project" wherever a row is not already under its own
// heading (the workspace AGENTS list, the ACTIVE list, the collapsed
// summaries, the switcher, the project header). Both halves are derived from
// the name unless the project stores an override, so every project has a
// badge from the moment it is added and the user only edits the ones that
// came out wrong.

/** The palette a project colour is chosen from. Keys are what the store
 *  persists (the Rust side validates against the same list); the fills are
 *  muted so a badge sits in the rail without shouting. */
export const PROJECT_COLORS = [
  { key: "gray", label: "Gray", bg: "#3a3f47", fg: "#e2e5ea" },
  { key: "blue", label: "Blue", bg: "#274a7a", fg: "#d6e7ff" },
  { key: "green", label: "Green", bg: "#22553a", fg: "#d0f2dd" },
  { key: "red", label: "Red", bg: "#6b2a2a", fg: "#ffd6d6" },
  { key: "purple", label: "Purple", bg: "#4a3573", fg: "#e4d8ff" },
  { key: "teal", label: "Teal", bg: "#1f5252", fg: "#cdf2ef" },
  { key: "orange", label: "Orange", bg: "#6b4a1f", fg: "#ffe4bd" },
  { key: "pink", label: "Pink", bg: "#6b2a52", fg: "#ffd6ee" },
] as const;

export type ProjectColor = (typeof PROJECT_COLORS)[number]["key"];
export type PaletteEntry = (typeof PROJECT_COLORS)[number];

/** What a badge is built from: the name, plus the stored overrides when the
 *  project has them. */
export interface ProjectIdentitySource {
  name: string;
  initials?: string | null;
  color?: string | null;
}

export function isProjectColor(value: unknown): value is ProjectColor {
  return PROJECT_COLORS.some((c) => c.key === value);
}

export function paletteEntry(color: ProjectColor): PaletteEntry {
  return PROJECT_COLORS.find((c) => c.key === color) ?? PROJECT_COLORS[0];
}

/** Initials from a name: the first letter of each of the first three words
 *  ("lab cluster" → LC, "chappa-ai" → CA, "Demo Project 2" → DP2), or the
 *  first two characters of a one-word name ("twofivesix" → TW). Uppercased. */
export function deriveInitials(name: string): string {
  const words = name.split(/[^\p{L}\p{N}]+/u).filter((w) => w.length > 0);
  if (words.length >= 2) return words.slice(0, 3).map((w) => w[0]).join("").toUpperCase();
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return "?";
}

/** A stable colour for a name: a small string hash into the palette, so the
 *  same project gets the same colour on every machine and every launch. */
export function deriveColor(name: string): ProjectColor {
  let hash = 5381;
  for (let i = 0; i < name.length; i++) hash = ((hash << 5) + hash + name.charCodeAt(i)) | 0;
  // Skip gray for derived colours: it is the "no colour" look, reserved for
  // an explicit pick.
  const colored = PROJECT_COLORS.slice(1);
  return colored[Math.abs(hash) % colored.length].key;
}

/** The badge as it should paint: overrides first, derived otherwise. */
export function resolveIdentity(src: ProjectIdentitySource): { initials: string; color: PaletteEntry } {
  const initials = src.initials?.trim() ? src.initials.trim() : deriveInitials(src.name);
  const color = isProjectColor(src.color) ? src.color : deriveColor(src.name);
  return { initials, color: paletteEntry(color) };
}

const BADGE_CSS =
  "display:inline-flex;align-items:center;justify-content:center;flex:none;min-width:22px;height:18px;" +
  "padding:0 4px;box-sizing:border-box;border-radius:4px;font:600 10px system-ui,sans-serif;" +
  "letter-spacing:.04em;line-height:1;white-space:nowrap;";

/** Build a badge element. The colours are inline so the badge paints the
 *  same wherever it lands (and is observable without a layout engine). */
export function projectBadge(src: ProjectIdentitySource): HTMLSpanElement {
  const { initials, color } = resolveIdentity(src);
  const el = document.createElement("span");
  el.className = "chappa-project-badge";
  el.style.cssText = `${BADGE_CSS}background:${color.bg};color:${color.fg};`;
  el.textContent = initials;
  el.title = src.name;
  el.dataset.color = color.key;
  return el;
}
