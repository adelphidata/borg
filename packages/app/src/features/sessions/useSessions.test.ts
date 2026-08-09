import { describe, expect, it } from "vitest";
import { filterSessions, groupSessions, type SessionRow } from "./useSessions";

/** 2026-08-09 14:00 local — the reference "now" for every case below. */
const NOW = new Date(2026, 7, 9, 14, 0, 0).getTime();
const DAY = 86_400;

function at(secondsAgoFromMidnight: number, id = "s"): SessionRow {
  const midnight = new Date(NOW).setHours(0, 0, 0, 0) / 1000;
  return { id, title: id, messageCount: 1, updatedAt: midnight + secondsAgoFromMidnight };
}

describe("groupSessions", () => {
  it("buckets by how a person would describe the date", () => {
    const groups = groupSessions(
      [
        at(3600, "today"),
        at(-1, "yesterday"),
        at(-3 * DAY, "thisWeek"),
        at(-10 * DAY, "thisMonth"),
        at(-90 * DAY, "ancient"),
      ],
      NOW,
    );

    expect(groups.map((g) => g.label)).toEqual([
      "Today",
      "Yesterday",
      "Previous 7 days",
      "Previous 30 days",
      "Older",
    ]);
  });

  it("puts a session from one minute after midnight in Today, not Yesterday", () => {
    const groups = groupSessions([at(60)], NOW);
    expect(groups[0]!.label).toBe("Today");
  });

  it("puts a session from one second before midnight in Yesterday", () => {
    const groups = groupSessions([at(-1)], NOW);
    expect(groups[0]!.label).toBe("Yesterday");
  });

  it("collapses a run of same-day sessions into one group, order preserved", () => {
    const groups = groupSessions([at(3600, "a"), at(1800, "b"), at(600, "c")], NOW);
    expect(groups).toHaveLength(1);
    expect(groups[0]!.sessions.map((s) => s.id)).toEqual(["a", "b", "c"]);
  });

  it("returns nothing for an empty list rather than empty groups", () => {
    expect(groupSessions([], NOW)).toEqual([]);
  });
});

describe("filterSessions", () => {
  const rows = [
    { id: "1", title: "Deploy notes", messageCount: 3, updatedAt: 0 },
    { id: "2", title: "Cortex bug hunt", messageCount: 9, updatedAt: 0 },
  ];

  it("keeps everything for a blank or whitespace query", () => {
    expect(filterSessions(rows, "")).toHaveLength(2);
    expect(filterSessions(rows, "   ")).toHaveLength(2);
  });

  it("matches case-insensitively on any part of the title", () => {
    expect(filterSessions(rows, "DEPLOY").map((s) => s.id)).toEqual(["1"]);
    expect(filterSessions(rows, "bug").map((s) => s.id)).toEqual(["2"]);
  });

  it("returns nothing when the query matches no title", () => {
    expect(filterSessions(rows, "kubernetes")).toEqual([]);
  });
});
