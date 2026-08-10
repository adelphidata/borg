import { describe, expect, it } from "vitest";
import { describeSendFailure } from "./sendFailure";

describe("describeSendFailure", () => {
  it("says nothing when the send succeeded", () => {
    expect(describeSendFailure(undefined)).toBeUndefined();
  });

  it.each([
    ["SQLITE_FULL: database or disk is full", "disk is full"],
    ["Error: ENOSPC: no space left on device, write", "disk is full"],
    ["write failed: No space left on device", "disk is full"],
  ])("names a full disk from %j", (raw, expected) => {
    const described = describeSendFailure({ message: raw })!;
    expect(described.toLowerCase()).toContain(expected);
    // The point is the next action, not the diagnosis.
    expect(described.toLowerCase()).toContain("free up space");
  });

  it.each([
    "EROFS: read-only file system, open '/home/u/.borg/borg.db'",
    "attempt to write a readonly database",
  ])("names a read-only data folder from %j", (raw) => {
    expect(describeSendFailure({ message: raw })!.toLowerCase()).toContain("read-only");
  });

  it("keeps an error it cannot explain, rather than guessing", () => {
    // A wrong plain-words guess would send the user after the wrong problem,
    // and would bury the only text a maintainer can act on.
    const raw = "transport error: connection reset by peer";
    expect(describeSendFailure({ message: raw })).toBe(raw);
  });

  it("matches however the layer that noticed happened to capitalise it", () => {
    expect(describeSendFailure({ message: "Disk Full" })).toBe(
      describeSendFailure({ message: "disk full" }),
    );
  });
});
