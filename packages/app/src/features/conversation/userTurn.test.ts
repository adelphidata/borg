import { describe, expect, it } from "vitest";
import { displayTitle, parseUserTurn } from "./userTurn";

/** The shape `borgd`'s heartbeat writes, trimmed. */
const TICK = [
  "*heartbeat tick*",
  "",
  "<proactive_nudges>",
  "- Critical host security issues detected on this machine:",
  "  - Application Firewall is off — run: `sudo socketfilterfw --setglobalstate on`",
  "- Scheduled task health check:",
  "  1 task(s) carry a last_error:",
  "    - Heartbeat (last_error: missed run)",
  "</proactive_nudges>",
].join("\n");

describe("parseUserTurn", () => {
  it("names a heartbeat tick and lifts its nudges out as a block", () => {
    const turn = parseUserTurn(TICK);
    expect(turn.system).toBe("Heartbeat check-in");
    expect(turn.text).toBe("");
    expect(turn.blocks).toHaveLength(1);
    expect(turn.blocks[0]).toMatchObject({ tag: "proactive_nudges", label: "Proactive nudges" });
    // Two top-level nudges; the nested lines are their detail, not more nudges.
    expect(turn.blocks[0]!.items).toBe(2);
    expect(turn.blocks[0]!.body.startsWith("- Critical host")).toBe(true);
    expect(turn.blocks[0]!.body).not.toContain("proactive_nudges");
  });

  it("splits the session greeting into each of its blocks", () => {
    const greeting = [
      "*session start greeting*",
      "",
      "<time_context>",
      "hour=09 weekday=Mon hours_since_last_session=none",
      "</time_context>",
      "",
      "<recent_activity>",
      "(none)",
      "</recent_activity>",
    ].join("\n");
    const turn = parseUserTurn(greeting);
    expect(turn.system).toBe("Session start");
    expect(turn.blocks.map((b) => b.label)).toEqual(["Time context", "Recent activity"]);
    expect(turn.blocks[1]!.body).toBe("(none)");
  });

  it("leaves a message the user typed alone", () => {
    expect(parseUserTurn("ship it")).toEqual({ text: "ship it", blocks: [] });
  });

  it("keeps a tag that sits inside a sentence as text", () => {
    // Only a tag on its own line, closed on its own line, is Borg's.
    const raw = "why does <proactive_nudges> show up in my chat?";
    expect(parseUserTurn(raw)).toEqual({ text: raw, blocks: [] });
  });

  it("keeps an unclosed tag as text", () => {
    const raw = "<draft>\nstill writing";
    expect(parseUserTurn(raw).blocks).toEqual([]);
    expect(parseUserTurn(raw).text).toBe(raw);
  });

  it("does not treat a marker mid-message as Borg's", () => {
    expect(parseUserTurn("what is a *heartbeat tick*?").system).toBeUndefined();
  });
});

describe("displayTitle", () => {
  it.each([
    ["*heartbeat tick*", "Heartbeat check-in"],
    ["*session start greeting*", "Session start"],
    ["Deploy notes", "Deploy notes"],
    // A title that only mentions the marker is the user's own.
    ["about the *heartbeat tick*", "about the *heartbeat tick*"],
  ])("shows %j as %j", (raw, shown) => {
    expect(displayTitle(raw)).toBe(shown);
  });
});
