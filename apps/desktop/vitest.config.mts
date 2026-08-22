import { defineConfig } from "vitest/config";

/**
 * Main-process tests only.
 *
 * The renderer is `@borg/app`, which has its own jsdom suite; what lives here
 * is Electron-side logic — binary resolution, socket probing, boot sequencing —
 * and it runs in node.
 */
export default defineConfig({
  test: {
    environment: "node",
    include: ["electron/**/*.test.ts"],
  },
});
