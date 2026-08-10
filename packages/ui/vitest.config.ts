import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // Node, not jsdom: every exported helper here takes its DOM input as an
    // argument (`readPreference(stored)`, `resolveTheme(pref, systemDark)`)
    // precisely so it can be tested without one. Add jsdom back when a real
    // component test lands.
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
