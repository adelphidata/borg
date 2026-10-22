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

  it("reads a desktop action as its verb and target", () => {
    expect(summarize({ action: "capture", app: "Calculator", mode: "som" })).toBe(
      "capture Calculator",
    );
    expect(summarize({ action: "click", pid: 1, window_id: 2, element_index: 4 })).toBe(
      "click [4]",
    );
    expect(summarize({ action: "click", pid: 1, x: 10, y: 20 })).toBe("click (10, 20)");
    expect(summarize({ action: "type", pid: 1, text: "hello" })).toBe("type hello");
    expect(summarize({ action: "key", pid: 1, keys: ["cmd", "n"] })).toBe("key cmd+n");
    expect(summarize({ action: "list_windows" })).toBe("list_windows");
  });

  it("keeps naming the target of other action-shaped tools", () => {
    // Regression: the desktop branch once swallowed a browser row's URL.
    expect(summarize({ action: "navigate", url: "https://x" })).toBe("navigate https://x");
    expect(summarize({ action: "call", to: "+15550100" })).toBe("call +15550100");
    expect(summarize({ action: "analyze", path: "clip.mp4" })).toBe("analyze clip.mp4");
  });

  it("has nothing to say about an argumentless call", () => {
    expect(summarize({})).toBeNull();
    expect(summarize(null)).toBeNull();
  });
});
