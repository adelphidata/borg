import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    setupFiles: ["src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    coverage: {
      provider: "v8",
      include: ["src/**"],
      exclude: [
        "src/test/**",
        "src/index.ts",
        "src/**/*.test.{ts,tsx}",
        // Runs only inside an AudioWorklet, which jsdom does not have.
        "src/features/meetings/capture/pcmWorklet.ts",
      ],
      thresholds: { lines: 90, statements: 90, functions: 90, branches: 85 },
    },
  },
});
