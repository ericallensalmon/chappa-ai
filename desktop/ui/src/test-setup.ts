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
