import { describe, expect, it } from "vitest";
import { initialsOf, savesUserName } from "./userName";

describe("initialsOf", () => {
  it("takes the first and last words' first letters", () => {
    expect(initialsOf("Ada Lovelace")).toBe("AL");
    expect(initialsOf("Ada King Lovelace")).toBe("AL");
  });

  it("takes one letter from a single word", () => {
    expect(initialsOf("ada")).toBe("A");
  });

  it("ignores the spacing around and between words", () => {
    expect(initialsOf("  grace   hopper ")).toBe("GH");
  });

  it("keeps a character outside the BMP whole", () => {
    expect(initialsOf("𝒜da")).toBe("𝒜");
  });

  it("has nothing to show for no name", () => {
    expect(initialsOf("   ")).toBe("");
  });
});

describe("savesUserName", () => {
  it("spots a memory write that carries the user's name", () => {
    expect(savesUserName("write_memory", '{"user_name":"Ada"}')).toBe(true);
    expect(
      savesUserName("write_memory", '{"filename":"n","content":"x","user_name":" Ada "}'),
    ).toBe(true);
  });

  it("ignores a blank name, other tools, and arguments that do not parse", () => {
    expect(savesUserName("write_memory", '{"user_name":"  "}')).toBe(false);
    expect(savesUserName("write_memory", '{"filename":"n","content":"x"}')).toBe(false);
    expect(savesUserName("write_memory", '{"user_name":42}')).toBe(false);
    expect(savesUserName("read_memory", '{"user_name":"Ada"}')).toBe(false);
    expect(savesUserName("write_memory", "{not json")).toBe(false);
    expect(savesUserName("write_memory", "null")).toBe(false);
  });
});
