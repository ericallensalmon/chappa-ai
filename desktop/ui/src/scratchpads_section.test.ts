// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { ScratchpadsSection, actorLabel, relativeTime, type ScratchpadsApi } from "./scratchpads_section";
import type { ScratchpadDto, ScratchpadSummaryDto } from "./ipc";

const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const NOW = 1_700_000_000_000;

function summary(over: Partial<ScratchpadSummaryDto> = {}): ScratchpadSummaryDto {
  return {
    scratchpad_id: 5,
    project_id: 6,
    name: "eval · worker models",
    revision: 3,
    tags: ["eval"],
    archived: false,
    created_at: NOW - 3_600_000,
    updated_at: NOW - 120_000,
    updated_by: "12",
    line_count: 4,
    bytes: 40,
    ...over,
  };
}

function stubApi(pads: Record<number, ScratchpadSummaryDto[]> = {}): {
  api: ScratchpadsApi;
  lists: Array<number | null>;
  reads: number[];
} {
  const lists: Array<number | null> = [];
  const reads: number[] = [];
  const api: ScratchpadsApi = {
    listScratchpads: vi.fn(async (projectId, includeGlobal) => {
      lists.push(projectId);
      const own = pads[projectId ?? -1] ?? [];
      // Mirror the Rust command: a project scope + includeGlobal adds the
      // pads with project_id null (kept under key -1 here).
      return includeGlobal && projectId !== null ? [...own, ...(pads[-1] ?? [])] : own;
    }),
    readScratchpad: vi.fn(async (id) => {
      reads.push(id);
      const row = Object.values(pads).flat().find((p) => p.scratchpad_id === id);
      if (!row) throw new Error("no such scratchpad");
      const { line_count: _l, bytes: _b, ...rest } = row;
      const pad: ScratchpadDto = { ...rest, content: "# eval\n\n## Log\n\nrun 1: ok\n" };
      return pad;
    }),
  };
  return { api, lists, reads };
}

describe("relativeTime / actorLabel", () => {
  it("renders glanceable ages and process actors", () => {
    expect(relativeTime(NOW - 10_000, NOW)).toBe("just now");
    expect(relativeTime(NOW - 120_000, NOW)).toBe("2m ago");
    expect(relativeTime(NOW - 3 * 3_600_000, NOW)).toBe("3h ago");
    expect(relativeTime(NOW - 5 * 86_400_000, NOW)).toBe("5d ago");
    expect(actorLabel("user")).toBe("user");
    expect(actorLabel("12")).toBe("process 12");
  });
});

describe("ScratchpadsSection", () => {
  it("is hidden with no project, lists the expanded project's pads with name / age / actor / revision", async () => {
    const { api, lists } = stubApi({ 6: [summary(), summary({ scratchpad_id: 9, name: "plan", updated_by: "user", revision: 1 })] });
    const section = new ScratchpadsSection({ api, pollMs: 0, now: () => NOW });
    expect(section.element.style.display).toBe("none");
    expect(lists).toEqual([]);

    section.setProject(6);
    await FLUSH();
    expect(section.element.style.display).toBe("block");
    expect(lists).toEqual([6]);
    expect(section.element.querySelector(".chappa-subsection-label")?.textContent).toBe("SCRATCHPADS");
    expect(section.element.querySelector(".chappa-subsection-count")?.textContent).toBe("2");
    const rows = section.element.querySelectorAll(".chappa-scratchpad-row");
    expect(rows.length).toBe(2);
    expect(rows[0].querySelector(".chappa-scratchpad-name")?.textContent).toBe("eval · worker models");
    expect(rows[0].querySelector(".chappa-scratchpad-meta")?.textContent).toBe("2m ago · process 12 · r3");
    expect(rows[1].querySelector(".chappa-scratchpad-meta")?.textContent).toBe("2m ago · user · r1");

    // Re-rendering the same project does NOT re-fetch (renderProject runs on
    // every rail event); a different project does; collapsing hides.
    section.setProject(6);
    await FLUSH();
    expect(lists).toEqual([6]);
    section.setProject(7);
    await FLUSH();
    expect(lists).toEqual([6, 7]);
    expect(section.element.querySelector(".chappa-subsection-count")?.textContent).toBe("0");
    expect(section.element.querySelector(".chappa-scratchpad-empty")).not.toBeNull();
    section.setProject(undefined);
    expect(section.element.style.display).toBe("none");
    section.dispose();
  });

  it("lists the global pads under an open project with a muted 'global' tag; dispose hides the section", async () => {
    const { api } = stubApi({
      6: [summary()],
      [-1]: [summary({ scratchpad_id: 11, project_id: null, name: "eval · worker models (global)", updated_by: "user" })],
    });
    const section = new ScratchpadsSection({ api, pollMs: 0, now: () => NOW });
    section.setProject(6);
    await FLUSH();
    expect((api.listScratchpads as ReturnType<typeof vi.fn>).mock.calls[0]).toEqual([6, true]);
    const rows = section.element.querySelectorAll(".chappa-scratchpad-row");
    expect(rows.length).toBe(2);
    expect(section.element.querySelector(".chappa-subsection-count")?.textContent).toBe("2");
    expect(rows[0].querySelector(".chappa-scratchpad-global")).toBeNull();
    expect(rows[0].querySelector(".chappa-scratchpad-name-text")?.textContent).toBe("eval · worker models");
    const globalRow = rows[1];
    expect(globalRow.querySelector(".chappa-scratchpad-name-text")?.textContent).toBe("eval · worker models (global)");
    expect(globalRow.querySelector(".chappa-scratchpad-global")?.textContent).toBe("global");
    expect((globalRow as HTMLElement).dataset.scratchpadId).toBe("11");

    expect(section.element.style.display).toBe("block");
    section.dispose();
    expect(section.element.style.display).toBe("none");
    expect(section.modalOpen).toBe(false);
  });

  it("opens the content in a modal with a working Copy button and closes", async () => {
    const { api, reads } = stubApi({ 6: [summary()] });
    const copied: string[] = [];
    const container = document.createElement("div");
    document.body.appendChild(container);
    const section = new ScratchpadsSection({
      api,
      pollMs: 0,
      now: () => NOW,
      modalContainer: container,
      copy: async (text) => {
        copied.push(text);
      },
    });
    section.setProject(6);
    await FLUSH();
    (section.element.querySelector(".chappa-scratchpad-row") as HTMLButtonElement).click();
    await FLUSH();
    expect(reads).toEqual([5]);
    expect(section.modalOpen).toBe(true);
    const modal = container.querySelector(".chappa-scratchpad-modal")!;
    expect(modal.querySelector(".chappa-scratchpad-modal-title")?.textContent).toBe("eval · worker models");
    expect(modal.querySelector(".chappa-scratchpad-modal-meta")?.textContent).toBe(
      "revision 3 · updated 2m ago by process 12 · eval",
    );
    expect(modal.querySelector(".chappa-scratchpad-modal-content")?.textContent).toBe("# eval\n\n## Log\n\nrun 1: ok\n");
    const copy = modal.querySelector(".chappa-scratchpad-copy") as HTMLButtonElement;
    copy.click();
    await FLUSH();
    expect(copied).toEqual(["# eval\n\n## Log\n\nrun 1: ok\n"]);
    expect(copy.textContent).toBe("Copied");
    (modal.querySelector(".chappa-scratchpad-close") as HTMLButtonElement).click();
    expect(section.modalOpen).toBe(false);
    expect(container.querySelector(".chappa-scratchpad-modal")).toBeNull();
    section.dispose();
    container.remove();
  });

  it("polls while a project is expanded, stops when collapsed, and drops stale answers", async () => {
    vi.useFakeTimers();
    try {
      let answer: ScratchpadSummaryDto[] = [];
      const lists: Array<number | null> = [];
      const api: ScratchpadsApi = {
        listScratchpads: async (projectId) => {
          lists.push(projectId);
          return answer;
        },
        readScratchpad: async () => {
          throw new Error("unused");
        },
      };
      const section = new ScratchpadsSection({ api, pollMs: 1000, now: () => NOW });
      section.setProject(6);
      await Promise.resolve();
      expect(lists).toEqual([6]);
      answer = [summary()];
      await vi.advanceTimersByTimeAsync(1000);
      expect(lists).toEqual([6, 6]);
      expect(section.element.querySelector(".chappa-subsection-count")?.textContent).toBe("1");
      section.setProject(undefined);
      await vi.advanceTimersByTimeAsync(3000);
      expect(lists).toEqual([6, 6]); // no polling while collapsed
      // The ↻ on the header line refreshes on demand.
      section.setProject(6);
      await Promise.resolve();
      (section.element.querySelector(".chappa-scratchpad-refresh") as HTMLButtonElement).click();
      await Promise.resolve();
      expect(lists).toEqual([6, 6, 6, 6]);
      section.dispose();
      await vi.advanceTimersByTimeAsync(3000);
      expect(lists.length).toBe(4);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a failed read shows the error in the modal with Copy disabled; a failed list keeps the last rows", async () => {
    const { api } = stubApi({ 6: [summary()] });
    const container = document.createElement("div");
    document.body.appendChild(container);
    const section = new ScratchpadsSection({ api, pollMs: 0, now: () => NOW, modalContainer: container });
    section.setProject(6);
    await FLUSH();
    await section.open(404);
    const modal = container.querySelector(".chappa-scratchpad-modal")!;
    expect(modal.querySelector(".chappa-scratchpad-modal-content")?.textContent).toContain("Could not read scratchpad 404");
    expect((modal.querySelector(".chappa-scratchpad-copy") as HTMLButtonElement).disabled).toBe(true);
    section.closeModal();
    (api.listScratchpads as unknown as { mockImplementationOnce: (f: () => Promise<never>) => void }).mockImplementationOnce(
      async () => {
        throw new Error("app gone");
      },
    );
    await section.refresh();
    expect(section.element.querySelectorAll(".chappa-scratchpad-row").length).toBe(1);
    section.dispose();
    container.remove();
  });
});
