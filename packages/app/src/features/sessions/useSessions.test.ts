import { beforeEach, describe, expect, it } from "vitest";
import { draftKeyFor, publishDraft } from "../conversation/draftStore";
import { applyActivity, resetActivity } from "./activityStore";
import {
  filterSessions,
  groupSessions,
  partitionArchived,
  resolveTitles,
  rowSubtitle,
  type SessionRow,
} from "./useSessions";

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

describe("resolveTitles", () => {
  const draft = (id: string): SessionRow => ({
    id,
    title: "New conversation",
    messageCount: 0,
    updatedAt: 0,
  });

  beforeEach(() => {
    for (const id of ["d1", "d2"]) publishDraft(draftKeyFor(id), "");
  });

  it("names an unsent row after its draft", () => {
    publishDraft(draftKeyFor("d1"), "Port the gateway to the new seam");
    expect(resolveTitles([draft("d1")])[0]!.title).toBe("Port the gateway to the new seam");
  });

  it("leaves a used conversation's title alone", () => {
    publishDraft(draftKeyFor("d1"), "half-written reply");
    const used: SessionRow = { id: "d1", title: "Deploy notes", messageCount: 6, updatedAt: 0 };
    expect(resolveTitles([used])[0]!.title).toBe("Deploy notes");
  });

  it("makes a draft findable by what it says", () => {
    publishDraft(draftKeyFor("d1"), "Cortex bug hunt");
    publishDraft(draftKeyFor("d2"), "Something else entirely");
    const found = filterSessions(resolveTitles([draft("d1"), draft("d2")]), "bug");
    expect(found.map((s) => s.id)).toEqual(["d1"]);
  });

  it("keeps row identity when nothing was renamed, so memos still hold", () => {
    const rows = [draft("d1")];
    expect(resolveTitles(rows)[0]).toBe(rows[0]);
  });

  it("stamps each row with its status", () => {
    applyActivity("d2", true);
    publishDraft(draftKeyFor("d1"), "not sent yet");
    const used: SessionRow = { id: "d3", title: "Deploy notes", messageCount: 6, updatedAt: 0 };

    const rows = resolveTitles([draft("d1"), draft("d2"), used]);

    expect(rows.map((r) => r.activity)).toEqual(["draft", "working", undefined]);
    resetActivity();
  });

  it("gives a row a new identity when only its status changed", () => {
    // The list memoises on row identity, so a turn starting elsewhere has to
    // produce a new object or the dot would never appear.
    const rows = [draft("d1")];
    const settled = resolveTitles(rows);
    applyActivity("d1", true);
    const working = resolveTitles(rows);

    expect(working[0]).not.toBe(settled[0]);
    expect(working[0]!.activity).toBe("working");
    resetActivity();
  });
});

describe("partitionArchived", () => {
  it("splits the put-away conversations out, keeping recency order", () => {
    const rows: SessionRow[] = [
      { id: "a", title: "A", messageCount: 2, updatedAt: 300 },
      { id: "b", title: "B", messageCount: 1, updatedAt: 200, archived: true },
      { id: "c", title: "C", messageCount: 4, updatedAt: 100 },
    ];

    const { active, archived } = partitionArchived(rows);

    expect(active.map((r) => r.id)).toEqual(["a", "c"]);
    expect(archived.map((r) => r.id)).toEqual(["b"]);
  });
});

describe("rowSubtitle", () => {
  const base: SessionRow = { id: "s1", title: "Deploy", messageCount: 6, updatedAt: 100 };

  it("counts up while a turn runs, ahead of anything else the row could say", () => {
    applyActivity("s1", true, 1_000);

    expect(rowSubtitle({ ...base, activity: "working", preview: "on it" }, 4_000)).toBe(
      "Working… 3.0s",
    );
  });

  it("shows the last thing said when nothing is running", () => {
    expect(rowSubtitle({ ...base, preview: "shipped it", previewRole: "assistant" }, 0)).toBe(
      "shipped it",
    );
  });

  it("says who spoke when the last word was the user's", () => {
    expect(rowSubtitle({ ...base, preview: "ship it", previewRole: "user" }, 0)).toBe(
      "You: ship it",
    );
  });

  it("falls back to the count for a session with nothing quotable", () => {
    expect(rowSubtitle(base, 0)).toBe("6 messages");
    expect(rowSubtitle({ ...base, messageCount: 1 }, 0)).toBe("1 message");
  });
});
