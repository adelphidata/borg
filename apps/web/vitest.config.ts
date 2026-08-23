import { defineConfig } from "vitest/config";

/**
 * The browser shell is two files: the token gate and the mount. Only the gate
 * has behaviour — `main.tsx` builds a transport and renders, which is covered
 * by `@borg/app`'s own suite and by the build itself.
 */
export default defineConfig({
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    setupFiles: ["src/test/setup.ts"],
    coverage: {
      provider: "v8",
      include: ["src/**"],
      exclude: ["src/test/**", "src/main.tsx", "src/vite-env.d.ts", "src/**/*.test.{ts,tsx}"],
      thresholds: { lines: 90, statements: 90, functions: 90, branches: 85 },
    },
  },
});
