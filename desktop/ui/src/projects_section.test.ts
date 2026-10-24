// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { ProjectsSection, type ProjectRowInfo, type ProjectsSettingsApi } from "./projects_section";

const FLUSH = async (): Promise<void> => {
  await Promise.resolve();
  await Promise.resolve();
};

function fakeApi(initial: ProjectRowInfo[]) {
  const rows = initial.map((r) => ({ ...r }));
  const api: ProjectsSettingsApi & { rows: ProjectRowInfo[] } = {
    rows,
    list: () => rows.map((r) => ({ ...r })),
    rename: vi.fn(async (id: number, name: string) => {
      if (name === "taken") throw new Error("name in use");
      rows.find((r) => r.id === id)!.name = name;
    }),
    setIdentity: vi.fn(async (id: number, initials: string | null, color: string | null) => {
      const row = rows.find((r) => r.id === id)!;
      row.initials = initials;
      row.color = color;
    }),
    remove: vi.fn(async (id: number) => {
      const i = rows.findIndex((r) => r.id === id);
      if (i >= 0) rows.splice(i, 1);
    }),
    add: vi.fn(async () => {}),
  };
  return api;
}

const row = (id: number, name: string, extra: Partial<ProjectRowInfo> = {}): ProjectRowInfo => ({
  id,
  name,
  path: `/p/${name}`,
  initials: null,
  color: null,
  open: false,
  ...extra,
});

const field = (root: ParentNode, name: string, index = 0): HTMLInputElement =>
  [...root.querySelectorAll<HTMLInputElement>(`[data-field="${name}"]`)][index];

describe("Projects settings section", () => {
  it("lists every project with its badge, name, initials placeholder and open marker", () => {
    const api = fakeApi([row(1, "chappa-ai", { open: true }), row(2, "Demo Project 2", { initials: "D2", color: "teal" })]);
    const s = new ProjectsSection(api);
    const rows = [...s.element.querySelectorAll<HTMLElement>(".chappa-projects-row")];
    expect(rows.map((r) => r.dataset.projectId)).toEqual(["1", "2"]);
    expect(rows[0].querySelector('[data-role="badge"]')?.textContent).toBe("CA");
    expect(rows[1].querySelector('[data-role="badge"]')?.textContent).toBe("D2");
    expect(rows[1].querySelector<HTMLElement>('[data-role="badge"]')?.dataset.color).toBe("teal");
    expect(field(rows[0], "name").value).toBe("chappa-ai");
    expect(field(rows[0], "initials").value).toBe("");
    expect(field(rows[0], "initials").placeholder).toBe("CA");
    expect(field(rows[1], "initials").value).toBe("D2");
    expect(rows[0].textContent).toContain("/p/chappa-ai · open");
    expect(rows[1].textContent).not.toContain("· open");
    // The chosen swatch is checked; auto is checked when nothing is stored.
    expect(rows[0].querySelector('.chappa-projects-color[data-color="auto"]')?.getAttribute("aria-checked")).toBe("true");
    expect(rows[1].querySelector('.chappa-projects-color[data-color="teal"]')?.getAttribute("aria-checked")).toBe("true");
    expect(s.element.querySelector('[data-action="addProject"]')).not.toBeNull();
    s.dispose();
  });

  it("commits a rename on change, ignores blank/unchanged, and snaps back on a refused one", async () => {
    const api = fakeApi([row(1, "chappa-ai")]);
    const s = new ProjectsSection(api);
    const name = field(s.element, "name");
    name.value = "   ";
    name.dispatchEvent(new Event("change"));
    name.value = "chappa-ai";
    name.dispatchEvent(new Event("change"));
    expect(api.rename).not.toHaveBeenCalled();
    name.value = "taken";
    name.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.rename).toHaveBeenCalledWith(1, "taken");
    expect(name.value).toBe("chappa-ai");
    name.value = "chappa";
    name.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.rows[0].name).toBe("chappa");
    s.dispose();
  });

  it("initials and colour write through setIdentity, each keeping the other half", async () => {
    const api = fakeApi([row(1, "chappa-ai", { color: "green" })]);
    const s = new ProjectsSection(api);
    const initials = field(s.element, "initials");
    initials.value = "ch";
    initials.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.setIdentity).toHaveBeenLastCalledWith(1, "CH", "green");
    s.refresh();
    s.element.querySelector<HTMLButtonElement>('[data-color="pink"]')!.click();
    await FLUSH();
    expect(api.setIdentity).toHaveBeenLastCalledWith(1, "CH", "pink");
    s.refresh();
    // Auto clears the colour, a blank field clears the initials.
    s.element.querySelector<HTMLButtonElement>('[data-color="auto"]')!.click();
    await FLUSH();
    expect(api.setIdentity).toHaveBeenLastCalledWith(1, "CH", null);
    s.refresh();
    const again = field(s.element, "initials");
    again.value = "";
    again.dispatchEvent(new Event("change"));
    await FLUSH();
    expect(api.setIdentity).toHaveBeenLastCalledWith(1, null, null);
    s.dispose();
  });

  it("remove and add ask the app; refresh rebuilds only when something changed", async () => {
    const api = fakeApi([row(1, "a"), row(2, "b")]);
    const s = new ProjectsSection(api);
    const before = s.element.querySelector(".chappa-projects-row");
    s.refresh(); // nothing changed → the same nodes stay
    expect(s.element.querySelector(".chappa-projects-row")).toBe(before);
    s.element.querySelector<HTMLButtonElement>('[data-action="removeProject"]')!.click();
    await FLUSH();
    expect(api.remove).toHaveBeenCalledWith(1);
    s.refresh();
    expect([...s.element.querySelectorAll<HTMLElement>(".chappa-projects-row")].map((r) => r.dataset.projectId)).toEqual(["2"]);
    s.element.querySelector<HTMLButtonElement>('[data-action="addProject"]')!.click();
    expect(api.add).toHaveBeenCalledTimes(1);
    api.rows.splice(0);
    s.refresh();
    expect(s.element.textContent).toContain("No projects yet.");
    s.dispose();
  });
});
