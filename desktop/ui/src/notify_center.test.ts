import { describe, expect, it, vi } from "vitest";
import {
  NotifyCenter,
  attributionText,
  relativeTime,
  type CenterEntryInput,
} from "./notify_center";

function input(over: Partial<CenterEntryInput> = {}): CenterEntryInput {
  return {
    termId: 1,
    projectId: null,
    processName: "shell",
    kind: "notify",
    title: "t",
    body: "b",
    ...over,
  };
}

describe("NotifyCenter", () => {
  it("records newest-first with the injected clock", () => {
    let t = 100;
    const center = new NotifyCenter({ now: () => t });
    center.record(input({ title: "first" }));
    t = 200;
    center.record(input({ title: "second" }));
    expect(center.entries.map((e) => e.title)).toEqual(["second", "first"]);
    expect(center.entries.map((e) => e.ts)).toEqual([200, 100]);
    expect(center.entries.every((e) => !e.read)).toBe(true);
  });

  it("caps the ring, dropping the OLDEST", () => {
    const center = new NotifyCenter({ cap: 3, now: () => 0 });
    for (let i = 1; i <= 5; i++) center.record(input({ title: `n${i}` }));
    expect(center.entries.map((e) => e.title)).toEqual(["n5", "n4", "n3"]);
    expect(center.unread).toBe(3);
  });

  it("tracks unread through markRead / markAllRead / clear", () => {
    const center = new NotifyCenter({ now: () => 0 });
    const a = center.record(input({ title: "a" }));
    center.record(input({ title: "b" }));
    expect(center.unread).toBe(2);
    center.markRead(a);
    expect(center.unread).toBe(1);
    expect(a.read).toBe(true);
    center.markAllRead();
    expect(center.unread).toBe(0);
    center.clear();
    expect(center.entries.length).toBe(0);
  });

  it("notifies subscribers on record/markRead/markAllRead/clear, not on no-ops", () => {
    const center = new NotifyCenter({ now: () => 0 });
    const fn = vi.fn();
    const unsub = center.subscribe(fn);
    const e = center.record(input());
    expect(fn).toHaveBeenCalledTimes(1);
    center.markRead(e);
    expect(fn).toHaveBeenCalledTimes(2);
    center.markRead(e); // already read: silent
    center.markAllRead(); // nothing unread: silent
    expect(fn).toHaveBeenCalledTimes(2);
    center.clear();
    expect(fn).toHaveBeenCalledTimes(3);
    center.clear(); // already empty: silent
    expect(fn).toHaveBeenCalledTimes(3);
    unsub();
    center.record(input());
    expect(fn).toHaveBeenCalledTimes(3);
  });
});

describe("attributionText", () => {
  const label = (id: number): string => (id === 1 ? "chappa-ai" : `project ${id}`);

  it("process/project-shell entries render name · project", () => {
    expect(attributionText({ projectId: 1, processName: "server" }, label)).toBe("server · chappa-ai");
  });

  it("plain shells attribute name only", () => {
    expect(attributionText({ projectId: null, processName: "shell" }, label)).toBe("shell");
  });

  it("a nameless project-scoped entry falls back to the project alone", () => {
    expect(attributionText({ projectId: 1, processName: null }, label)).toBe("chappa-ai");
  });
});

describe("relativeTime", () => {
  it("uses coarse buckets", () => {
    const now = 1_000_000_000;
    expect(relativeTime(now, now)).toBe("just now");
    expect(relativeTime(now - 59_000, now)).toBe("just now");
    expect(relativeTime(now - 120_000, now)).toBe("2m ago");
    expect(relativeTime(now - 59 * 60_000, now)).toBe("59m ago");
    expect(relativeTime(now - 3 * 3_600_000, now)).toBe("3h ago");
    expect(relativeTime(now - 48 * 3_600_000, now)).toBe("2d ago");
    // A clock skew (ts in the future) clamps to "just now", never negative.
    expect(relativeTime(now + 60_000, now)).toBe("just now");
  });
});
