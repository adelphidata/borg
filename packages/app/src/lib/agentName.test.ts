import { describe, expect, it } from "vitest";
import { rerollName } from "./agentName";

describe("rerollName", () => {
  it("never hands back the name on screen", () => {
    expect(rerollName(["A", "B"], "A", () => 0.99)).toBe("B");
    expect(rerollName(["A", "B", "C"], "B", () => 0)).toBe("A");
  });

  it("keeps the current name when the pool has nothing else", () => {
    expect(rerollName([], "A")).toBe("A");
    expect(rerollName(["A"], "A")).toBe("A");
  });
});
