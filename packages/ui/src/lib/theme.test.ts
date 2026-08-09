import { describe, expect, it } from "vitest";
import { readPreference, resolveTheme } from "./theme";

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
