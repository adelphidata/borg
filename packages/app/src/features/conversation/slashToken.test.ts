import { describe, expect, it } from "vitest";
import { slashQuery } from "./slashToken";

describe("slashQuery", () => {
  it("opens on a bare slash, with everything on offer", () => {
    expect(slashQuery("/")).toBe("");
  });

  it("narrows to what has been typed after the slash", () => {
    expect(slashQuery("/sett")).toBe("sett");
  });

  it("closes at the first space, because that is prose again", () => {
    // "/goal ship the thing" is a sentence, not a query.
    expect(slashQuery("/goal ship")).toBeNull();
    expect(slashQuery("/goal ")).toBeNull();
  });

  it("ignores a slash that is not leading", () => {
    // The cases a mid-text trigger would break: a URL, a path, a fraction.
    expect(slashQuery("see https://example.com/x")).toBeNull();
    expect(slashQuery("read src/lib/mod.rs")).toBeNull();
    expect(slashQuery("and/or")).toBeNull();
  });

  it("stays shut on an empty box and on ordinary text", () => {
    expect(slashQuery("")).toBeNull();
    expect(slashQuery("what can you do")).toBeNull();
  });

  it("does not treat leading whitespace as leading position", () => {
    // Otherwise a pasted block starting with an indented slash pops a menu.
    expect(slashQuery(" /new")).toBeNull();
  });
});
