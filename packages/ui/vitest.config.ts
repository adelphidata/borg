import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The pure helpers here still take their DOM input as an argument
    // (`readPreference(stored)`, `resolveTheme(pref, systemDark)`) and need no
    // environment. Sidebar's collapse contract is the first thing that can only
    // be checked by rendering it, so jsdom is on.
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
