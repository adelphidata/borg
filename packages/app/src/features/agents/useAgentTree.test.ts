import { describe, expect, it } from "vitest";
import { isLive } from "./useAgentTree";

describe("isLive", () => {
  it("treats an agent that is still doing something as live", () => {
    // Paused counts: it is still holding its slot and can be resumed.
    for (const status of ["running", "pending_init", "paused"]) {
      expect(isLive(status)).toBe(true);
    }
  });

  it("treats every terminal state as finished", () => {
    for (const status of ["completed", "errored", "shutdown"]) {
      expect(isLive(status)).toBe(false);
    }
  });

  it("does not guess about an unrecognised status", () => {
    expect(isLive("")).toBe(false);
    expect(isLive("something_new")).toBe(false);
  });
});
