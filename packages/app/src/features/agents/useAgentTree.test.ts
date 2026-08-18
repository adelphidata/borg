import { describe, expect, it } from "vitest";
import type { admin as adminPb } from "@borg/client";
import { classifyStatus, failureReason, isLive, statusLabel } from "./useAgentTree";

/** An agent entry with only the fields these helpers read. */
function entry(status: string, error = ""): adminPb.AgentEntry {
  return { id: "a1", status, error } as adminPb.AgentEntry;
}

/** What the stall watchdog actually writes into `error_text`. */
const STALL = "stalled: no progress for 450s (threshold 450s, phase tool_call, grace exhausted)";

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

describe("classifyStatus", () => {
  it("sorts each wire status into what the reader can do about it", () => {
    const cases: [string, string][] = [
      ["running", "live"],
      ["pending_init", "live"],
      ["paused", "live"],
      ["completed", "done"],
      ["errored", "failed"],
      ["shutdown", "failed"],
    ];
    for (const [status, expected] of cases) {
      expect(classifyStatus(status)).toBe(expected);
    }
  });

  it("refuses to read an unknown status as finished cleanly", () => {
    // The failure this guards: a new daemon status rendering as a quiet
    // completed row, so a stalled agent looks exactly like a successful one.
    expect(classifyStatus("something_new")).toBe("unknown");
    expect(classifyStatus("")).toBe("unknown");
  });
});

describe("failureReason", () => {
  it("says nothing about an agent that is working or finished", () => {
    expect(failureReason(entry("running"))).toBeUndefined();
    expect(failureReason(entry("pending_init"))).toBeUndefined();
    expect(failureReason(entry("completed"))).toBeUndefined();
  });

  it("leads with what a stall means rather than the daemon's prefix", () => {
    const reason = failureReason(entry("errored", STALL));

    expect(reason?.startsWith("Stalled — ")).toBe(true);
    expect(reason).toContain("no progress for 450s");
  });

  it("passes an ordinary failure through unchanged", () => {
    expect(failureReason(entry("errored", "provider refused the request"))).toBe(
      "provider refused the request",
    );
  });

  it("names the silence when an agent errored with nothing to say", () => {
    expect(failureReason(entry("errored"))).toBe("Stopped without reporting a reason.");
  });

  it("explains a shutdown the daemon did not annotate", () => {
    expect(failureReason(entry("shutdown"))).toBe("Stopped by Borg.");
  });

  it("surfaces an unrecognised status instead of hiding it", () => {
    expect(failureReason(entry("something_new"))).toContain("something_new");
  });
});

describe("statusLabel", () => {
  it("distinguishes a stall from an ordinary failure", () => {
    expect(statusLabel(entry("errored", STALL))).toBe("Stalled");
    expect(statusLabel(entry("errored", "provider refused"))).toBe("Failed");
  });

  it("reads each known status in plain words", () => {
    expect(statusLabel(entry("running"))).toBe("Running");
    expect(statusLabel(entry("pending_init"))).toBe("Queued");
    expect(statusLabel(entry("paused"))).toBe("Paused");
    expect(statusLabel(entry("completed"))).toBe("Done");
    expect(statusLabel(entry("shutdown"))).toBe("Stopped");
  });

  it("does not pretend to recognise a new status", () => {
    expect(statusLabel(entry("something_new"))).toBe("Unknown");
  });
});
