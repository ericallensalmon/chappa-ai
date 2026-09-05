
// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  ATLAS_DEFAULT_MAX_PAGES,
  AtlasIndex,
  blockElementRects,
  isColorGlyph,
  packKey,
  slotDims,
  unpackKey,
  type AtlasIndexOptions,
} from "./atlas";

function makeIndex(overrides: Partial<AtlasIndexOptions> = {}): AtlasIndex {
  return new AtlasIndex({
    cellW: 1,
    cellH: 1,
    dpr: 1,
    pageSize: 12,
    maxPages: 2,
    ...overrides,
  });
}

describe("packKey", () => {
  it("roundtrips plain codepoints", () => {
    expect(unpackKey(packKey(0x41))).toEqual({ codepoint: 0x41, bold: false, italic: false });
  });

  it("roundtrips astral-plane codepoints", () => {
    expect(unpackKey(packKey(0x1f600))).toEqual({ codepoint: 0x1f600, bold: false, italic: false });
  });

  it("roundtrips style bits", () => {
    const k = packKey(0x41, true, true);
    expect(unpackKey(k)).toEqual({ codepoint: 0x41, bold: true, italic: true });
    expect(unpackKey(packKey(0x41, true, false))).toEqual({ codepoint: 0x41, bold: true, italic: false });
  });

  it("masks codepoints to 21 bits", () => {
    expect(packKey(0x1fffff, false, false)).toBe(packKey(0x21fffff, false, false));
  });
});

describe("slotDims", () => {
  it("adds 1px pad on every side", () => {
    expect(slotDims(1, 1, 1)).toEqual({ w: 3, h: 3 });
    expect(slotDims(1, 1, 1, true)).toEqual({ w: 4, h: 3 });
  });

  it("scales by dpr and rounds to the nearest device px", () => {
    // round, not ceil: the renderer quantizes cells so cell×dpr is integral,
    // and round keeps FP epsilon from inflating the slot by a pixel (the
    // glyph region must exactly equal the device cell for 1:1 texels).
    expect(slotDims(8, 14, 2)).toEqual({ w: 18, h: 30 });
    expect(slotDims(8.4, 14.2, 1)).toEqual({ w: 10, h: 16 });
    expect(slotDims(13.6, 14.4, 1.25)).toEqual({ w: 19, h: 20 });
  });
});

describe("blockElementRects", () => {
  it("shades are full-cell fills at 25/50/75% alpha; █ is a full opaque fill", () => {
    expect(blockElementRects(0x2591, 10, 20)).toEqual([{ x: 0, y: 0, w: 10, h: 20, alpha: 0.25 }]);
    expect(blockElementRects(0x2592, 10, 20)).toEqual([{ x: 0, y: 0, w: 10, h: 20, alpha: 0.5 }]);
    expect(blockElementRects(0x2593, 10, 20)).toEqual([{ x: 0, y: 0, w: 10, h: 20, alpha: 0.75 }]);
    expect(blockElementRects(0x2588, 10, 20)).toEqual([{ x: 0, y: 0, w: 10, h: 20, alpha: 1 }]);
  });

  it("halves and eighths are exact fractional rects on the correct edge", () => {
    expect(blockElementRects(0x2580, 10, 20)).toEqual([{ x: 0, y: 0, w: 10, h: 10, alpha: 1 }]); // ▀
    expect(blockElementRects(0x2584, 10, 20)).toEqual([{ x: 0, y: 10, w: 10, h: 10, alpha: 1 }]); // ▄
    expect(blockElementRects(0x2581, 10, 16)).toEqual([{ x: 0, y: 14, w: 10, h: 2, alpha: 1 }]); // ▁
    expect(blockElementRects(0x258c, 10, 20)).toEqual([{ x: 0, y: 0, w: 5, h: 20, alpha: 1 }]); // ▌
    expect(blockElementRects(0x258f, 16, 20)).toEqual([{ x: 0, y: 0, w: 2, h: 20, alpha: 1 }]); // ▏
    expect(blockElementRects(0x2590, 10, 20)).toEqual([{ x: 5, y: 0, w: 5, h: 20, alpha: 1 }]); // ▐
    expect(blockElementRects(0x2594, 10, 16)).toEqual([{ x: 0, y: 0, w: 10, h: 2, alpha: 1 }]); // ▔
    expect(blockElementRects(0x2595, 16, 20)).toEqual([{ x: 14, y: 0, w: 2, h: 20, alpha: 1 }]); // ▕
  });

  it("quadrants are quarter-cell rects on the shared grid", () => {
    expect(blockElementRects(0x2598, 10, 20)).toEqual([{ x: 0, y: 0, w: 5, h: 10, alpha: 1 }]); // ▘
    expect(blockElementRects(0x2597, 10, 20)).toEqual([{ x: 5, y: 10, w: 5, h: 10, alpha: 1 }]); // ▗
    expect(blockElementRects(0x259a, 10, 20)).toEqual([
      { x: 0, y: 0, w: 5, h: 10, alpha: 1 },
      { x: 5, y: 10, w: 5, h: 10, alpha: 1 },
    ]); // ▚
    expect(blockElementRects(0x259f, 10, 20)).toEqual([
      { x: 5, y: 0, w: 5, h: 10, alpha: 1 },
      { x: 0, y: 10, w: 5, h: 10, alpha: 1 },
      { x: 5, y: 10, w: 5, h: 10, alpha: 1 },
    ]); // ▟
  });

  it("odd-sized cells keep every seam on one shared boundary (no 1px notches)", () => {
    // 17px tall: the half boundary is frac8(4)=9 for EVERY block — ▀ ends
    // where ▄ and the lower quadrants begin. Independent rounding put ▄ at
    // y=8 vs ▀ ending at 9 (host-run: notched outline on block art).
    const upper = blockElementRects(0x2580, 9, 17)![0];
    const lower = blockElementRects(0x2584, 9, 17)![0];
    expect(upper.h).toBe(9);
    expect(lower.y).toBe(9);
    expect(lower.h).toBe(8);
    const quadLL = blockElementRects(0x2596, 9, 17)![0];
    expect(quadLL.y).toBe(9); // same boundary as ▄
    expect(quadLL.w).toBe(5); // frac8(4, 9) — same boundary ▌/▐ use
    const right = blockElementRects(0x2590, 9, 17)![0];
    expect(right.x).toBe(5);
    expect(right.w).toBe(4);
  });

  it("returns null outside U+2580–U+259F (font glyphs, incl. box drawing)", () => {
    expect(blockElementRects(0x41, 10, 20)).toBeNull(); // A
    expect(blockElementRects(0x2500, 10, 20)).toBeNull(); // ─ box drawing stays font-drawn
    expect(blockElementRects(0x25a0, 10, 20)).toBeNull(); // ■ geometric shapes stay font-drawn
    expect(blockElementRects(0x28ff, 10, 20)).toBeNull(); // braille stays font-drawn
  });
});

describe("AtlasIndex allocation", () => {
  it("falls back to default page size and page count", () => {
    const at = new AtlasIndex({ cellW: 1, cellH: 1, dpr: 1 });
    expect(at.pageCount).toBe(ATLAS_DEFAULT_MAX_PAGES);
    // 2048×2048 page fits ~2044 narrow slots; a couple land on page 0.
    const a = at.insert(packKey(0x41)).slot;
    const b = at.insert(packKey(0x42)).slot;
    expect(a.page).toBe(0);
    expect(b.page).toBe(0);
  });

  it("packs glyphs across a shelf, then a fresh shelf", () => {
    // pageSize 12, slot 3×3 → 4 slots per row.
    const at = makeIndex({ pageSize: 12 });
    const row0 = [0, 1, 2, 3].map((i) => at.insert(packKey(0x40 + i)).slot);
    expect(row0.map((s) => s.y)).toEqual([0, 0, 0, 0]);
    expect(row0.map((s) => s.x)).toEqual([0, 3, 6, 9]);
    expect(row0.map((s) => s.page)).toEqual([0, 0, 0, 0]);

    const fresh = at.insert(packKey(0x50)).slot;
    expect(fresh.y).toBe(3);
    expect(fresh.x).toBe(0);
  });

  it("exhausts a page then spills to the next", () => {
    const at = makeIndex({ pageSize: 9 }); // 3 per row, 3 rows → 9 slots
    const first9 = [];
    for (let i = 0; i < 9; i++) first9.push(at.insert(packKey(0x40 + i)).slot);
    expect(first9.every((s) => s.page === 0)).toBe(true);

    const tenth = at.insert(packKey(0x80)).slot;
    expect(tenth.page).toBe(1);
    expect(tenth.x).toBe(0);
    expect(tenth.y).toBe(0);
  });

  it("allocates wide glyphs into a two-cell slot", () => {
    const at = makeIndex({ pageSize: 12 });
    const wide = at.insert(packKey(0x1f600), true).slot;
    const narrow = at.insert(packKey(0x41)).slot;
    const wide2 = at.insert(packKey(0x1f601), true).slot;

    expect(wide.w).toBe(4);
    expect(wide.h).toBe(3);
    expect(narrow.x).toBe(wide.x + wide.w); // narrow packs right after wide
    expect(narrow.w).toBe(3);
    expect(wide2.x).toBe(narrow.x + narrow.w); // 3+3+4=10 ≤ 12, same shelf
    expect(wide2.y).toBe(0);
  });

  it("returns the same slot for a duplicate insert", () => {
    const at = makeIndex({ pageSize: 12 });
    const a = at.insert(packKey(0x41));
    const b = at.insert(packKey(0x41));
    expect(b.slot).toEqual(a.slot);
    expect(b.evictedPages).toEqual([]);
  });
});

describe("AtlasIndex lookup", () => {
  it("counts misses and hits", () => {
    const at = makeIndex({ pageSize: 12 });
    expect(at.lookup(packKey(0x41))).toBeNull();
    expect(at.missCount).toBe(1);

    const { slot } = at.insert(packKey(0x41));
    expect(at.lookup(packKey(0x41))).toEqual(slot);
    expect(at.missCount).toBe(1); // a hit does not count

    expect(at.lookup(packKey(0x42))).toBeNull();
    expect(at.missCount).toBe(2);
  });

  it("bumps page recency on a hit", () => {
    const at = makeIndex({ pageSize: 9, maxPages: 2 });
    // Fill page 0, then page 1 — page 0 is now the coldest.
    for (let i = 0; i < 9; i++) at.insert(packKey(0x40 + i));
    for (let i = 0; i < 9; i++) at.insert(packKey(0x80 + i));

    // Touch page 0 via lookups so page 1 becomes the LRU.
    for (let i = 0; i < 9; i++) expect(at.lookup(packKey(0x40 + i))).not.toBeNull();

    const res = at.insert(packKey(0x200));
    expect(res.evictedPages).toEqual([1]);
    expect(res.slot.page).toBe(1);
  });
});

describe("AtlasIndex LRU eviction", () => {
  it("evicts the coldest page wholesale and bumps its version", () => {
    const at = makeIndex({ pageSize: 9, maxPages: 2 });
    for (let i = 0; i < 9; i++) at.insert(packKey(0x40 + i));
    for (let i = 0; i < 9; i++) at.insert(packKey(0x80 + i));
    expect(at.pageVersion(0)).toBe(0);
    expect(at.pageVersion(1)).toBe(0);

    const res = at.insert(packKey(0x200));
    expect(res.evictedPages).toEqual([0]);
    expect(at.evictions).toBe(1);
    expect(at.pageVersion(0)).toBe(1);
    expect(at.pageVersion(1)).toBe(0);
    expect(res.slot.page).toBe(0);

    // Page 0's old glyphs are gone (lazy re-entry on the next miss)…
    expect(at.lookup(packKey(0x40))).toBeNull();
    // …while page 1 survives untouched.
    expect(at.lookup(packKey(0x80))).not.toBeNull();
  });

  it("rotates: the second eviction targets the other page", () => {
    const at = makeIndex({ pageSize: 9, maxPages: 2 });
    for (let i = 0; i < 9; i++) at.insert(packKey(0x40 + i));
    for (let i = 0; i < 9; i++) at.insert(packKey(0x80 + i));

    // 18 inserts fill the evicted page 0, evict page 1, and re-fill it.
    for (let i = 0; i < 18; i++) at.insert(packKey(0x100 + i));
    expect(at.evictions).toBe(2);
    expect(at.pageVersion(0)).toBe(1);
    expect(at.pageVersion(1)).toBe(1);

    // Both pages full again — the coldest (page 0) is evicted next.
    const res = at.insert(packKey(0x200));
    expect(res.evictedPages).toEqual([0]);
    expect(at.evictions).toBe(3);
  });
});

describe("AtlasIndex reset", () => {
  it("clears pages, counters, and bumps every version", () => {
    const at = makeIndex({ pageSize: 9 });
    at.insert(packKey(0x41));
    at.lookup(packKey(0x42)); // one miss
    at.insert(packKey(0x43));
    at.insert(packKey(0x44));
    at.insert(packKey(0x45));
    at.insert(packKey(0x46));
    at.insert(packKey(0x47));
    at.insert(packKey(0x48));
    at.insert(packKey(0x49));
    at.insert(packKey(0x4a));
    at.insert(packKey(0x4b)); // page 0 is full now
    at.insert(packKey(0x4c)); // spills to page 1
    at.reset();

    // Counters are zero immediately after reset — before any lookup re-misses.
    expect(at.missCount).toBe(0);
    expect(at.evictions).toBe(0);
    expect(at.pageVersion(0)).toBe(1);
    expect(at.pageVersion(1)).toBe(1);
    expect(at.lookup(packKey(0x41))).toBeNull();
    expect(at.insert(packKey(0x41)).slot.page).toBe(0);
  });
});

describe("isColorGlyph", () => {
  it("detects emoji blocks", () => {
    expect(isColorGlyph(0x1f600)).toBe(true); // 😀
    expect(isColorGlyph(0x1f680)).toBe(true); // 🚀
    expect(isColorGlyph(0x1f4a9)).toBe(true); // 💩
    expect(isColorGlyph(0x1f0a1)).toBe(true); // 🂡 playing card
  });

  it("detects default-emoji-presentation symbols only", () => {
    expect(isColorGlyph(0x26a1)).toBe(true); // ⚡ emoji-default
    expect(isColorGlyph(0x2705)).toBe(true); // ✅ emoji-default
    expect(isColorGlyph(0x2b50)).toBe(true); // ⭐ emoji-default
  });

  it("text-presentation symbols are tinted text, not color glyphs", () => {
    // The Claude Code spinner set — the old 2600–27BF sweep made these
    // render untinted white (host-run).
    expect(isColorGlyph(0x273b)).toBe(false); // ✻
    expect(isColorGlyph(0x273d)).toBe(false); // ✽
    expect(isColorGlyph(0x2736)).toBe(false); // ✶
    expect(isColorGlyph(0x2714)).toBe(false); // ✔ text-default
    expect(isColorGlyph(0x2603)).toBe(false); // ☃ text-default
    expect(isColorGlyph(0x00a9)).toBe(false); // © text-default
    expect(isColorGlyph(0x25b6)).toBe(false); // ▶ text-default (TUI arrows)
  });

  it("rejects ASCII, CJK, and box drawing", () => {
    expect(isColorGlyph(0x41)).toBe(false); // A
    expect(isColorGlyph(0x20)).toBe(false); // space
    expect(isColorGlyph(0x4f60)).toBe(false); // 你
    expect(isColorGlyph(0x2500)).toBe(false); // ─
  });

  it("handles range boundaries", () => {
    expect(isColorGlyph(0x1f5ff)).toBe(true); // last of 1F300–1F5FF
    expect(isColorGlyph(0x1f600)).toBe(true); // first of 1F600–1F64F
    expect(isColorGlyph(0x1f650)).toBe(false); // gap after 1F600–1F64F
    expect(isColorGlyph(0x27bf)).toBe(true); // ➿ singleton (emoji-default)
    expect(isColorGlyph(0x27c0)).toBe(false); // first past it
    expect(isColorGlyph(0x1faff)).toBe(true); // last of 1FA70–1FAFF
    expect(isColorGlyph(0x1fb00)).toBe(false); // first past it
  });
});
