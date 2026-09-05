import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  // vitest reads this block; jsdom is opted into per-file via
  // `// @vitest-environment jsdom` where DOM APIs are needed.
  test: {
    environment: "node",
    setupFiles: ["./src/test-setup.ts"],
  },
} as never);
