import { describe, expect, it } from "vitest";
import { THEME_PREFERENCES, readPreference, resolveTheme } from "./theme";

describe("THEME_PREFERENCES", () => {
  it("offers exactly the choices readPreference accepts, in menu order", () => {
    // This list is what a settings menu renders. An entry readPreference
    // rejects would be a menu item that silently becomes "dark" on reload.
    expect(THEME_PREFERENCES).toEqual(["light", "dark", "system"]);
    for (const p of THEME_PREFERENCES) expect(readPreference(p)).toBe(p);
  });
});

describe("readPreference", () => {
  it("accepts the three real preferences", () => {
    expect(readPreference("light")).toBe("light");
    expect(readPreference("dark")).toBe("dark");
    expect(readPreference("system")).toBe("system");
  });

  it("falls back to dark for a missing or corrupt stored value", () => {
    // A garbage value reaching dataset.theme is what this guards against.
    expect(readPreference(null)).toBe("dark");
    expect(readPreference("")).toBe("dark");
    expect(readPreference("neon")).toBe("dark");
  });
});

describe("resolveTheme", () => {
  it("honours an explicit choice regardless of the OS", () => {
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
  });

  it("follows the OS when the preference is system", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
  });
});
