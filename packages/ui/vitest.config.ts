import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The pure helpers here still take their DOM input as an argument
    // (`readPreference(stored)`, `resolveTheme(pref, systemDark)`) and need no
    // environment. Sidebar's collapse contract is the first thing that can only
    // be checked by rendering it, so jsdom is on.
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    setupFiles: ["src/test/setup.ts"],
    coverage: {
      provider: "v8",
      include: ["src/**"],
      exclude: [
        // Test-only shims, the barrel, and the type declarations are wiring.
        "src/test/**",
        "src/index.ts",
        "src/types.ts",
        "src/**/*.test.{ts,tsx}",
        // Purely presentational: these pick a class string and render it. There
        // is no behaviour under them to assert without asserting on CSS.
        "src/components/Badge.tsx",
        "src/components/Brackets.tsx",
        "src/components/pageClasses.ts",
        "src/components/icons.tsx",
      ],
      thresholds: { lines: 90, statements: 90, functions: 90, branches: 85 },
    },
  },
});
