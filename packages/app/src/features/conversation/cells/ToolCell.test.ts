import { describe, expect, it } from "vitest";
import { parseArgs, patchOf, summarize } from "./ToolCell";

describe("parseArgs", () => {
  it("reads an object", () => {
    expect(parseArgs('{"path":"a.txt"}')).toEqual({ path: "a.txt" });
  });

  it("returns null for empty, malformed, and non-object JSON", () => {
    for (const input of ["", "   ", "{oops", "[1,2]", '"a string"', "null"]) {
      expect(parseArgs(input)).toBeNull();
    }
  });
});

describe("patchOf", () => {
  it("finds a unified diff under any of the field names tools use", () => {
    expect(patchOf({ patch: "@@ -1 +1 @@" })).toBe("@@ -1 +1 @@");
    expect(patchOf({ diff: "d" })).toBe("d");
    expect(patchOf({ edits: "e" })).toBe("e");
  });

  it("prefers `patch` when a call carries more than one", () => {
    expect(patchOf({ diff: "second", patch: "first" })).toBe("first");
  });

  it("ignores blank and non-string values", () => {
    expect(patchOf({ patch: "   " })).toBeNull();
    expect(patchOf({ patch: 42 })).toBeNull();
    expect(patchOf(null)).toBeNull();
  });
});

describe("summarize", () => {
  it("picks whichever identifying field the tool used", () => {
    expect(summarize({ path: "src/main.rs" })).toBe("src/main.rs");
    expect(summarize({ command: "cargo test" })).toBe("cargo test");
    expect(summarize({ query: "borg" })).toBe("borg");
  });

  it("prefers the more specific field when several are present", () => {
    expect(summarize({ name: "n", path: "p" })).toBe("p");
  });

  it("has nothing to say about an argumentless call", () => {
    expect(summarize({})).toBeNull();
    expect(summarize(null)).toBeNull();
  });
});
