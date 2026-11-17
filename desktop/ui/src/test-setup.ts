// Vitest setup (see vite.config.ts `test.setupFiles`).
//
// jsdom has no canvas implementation: every `getContext()` call returns null
// and logs a "Not implemented" line to stderr. The code under test already
// handles a null context (atlas/metrics fall back to measured defaults), so
// the behavior is right — only the noise is wrong. Replace the method with a
// quiet stub that returns the same null. Guarded so node-environment files
// (no `HTMLCanvasElement`) load this setup file harmlessly.

if (typeof HTMLCanvasElement !== "undefined") {
  HTMLCanvasElement.prototype.getContext = function getContext(): null {
    return null;
  } as unknown as typeof HTMLCanvasElement.prototype.getContext;
}

// The activity bar remembers the last tab, project and source picks in
// localStorage, and a mounted app restores them: drop them before every test
// so one test's navigation never leaks into the next one's start.
import { beforeEach } from "vitest";

/** `NAV_STORAGE_PREFIX` in nav_bar.ts (not imported: this file loads before
 *  every test file and stays free of app modules). */
const NAV_STORAGE_PREFIX = "chappa-ai.nav.";

beforeEach(() => {
  if (typeof window === "undefined") return;
  try {
    const storage = window.localStorage;
    for (const key of Object.keys(storage)) if (key.startsWith(NAV_STORAGE_PREFIX)) storage.removeItem(key);
  } catch {
    // No storage in this environment: nothing to leak.
  }
});
