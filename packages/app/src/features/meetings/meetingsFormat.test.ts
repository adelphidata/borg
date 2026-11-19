import { describe, expect, it } from "vitest";
import {
  attendeeLine,
  dayLabel,
  dayRows,
  duration,
  eventWhen,
  fullNames,
  groupPast,
  initialOf,
  segmentClock,
  shortDay,
  timeRange,
} from "./meetingsFormat";
import { event, meeting } from "../../test/meetings";

/** Seconds since the epoch for a local wall-clock time. */
const at = (y: number, mo: number, d: number, h = 0, mi = 0) =>
  new Date(y, mo - 1, d, h, mi).getTime() / 1000;

const people = (...names: string[]) => names.map((name) => ({ name, email: "" }));

describe("attendeeLine", () => {
  it.each([
    [[], ""],
    [people("Juan Pérez"), "Juan"],
    [people("Juan Pérez", "Jae Kim"), "Juan & Jae"],
    [people("Juan", "Jae", "Ali"), "Juan, Jae, & Ali"],
    [people("Saad", "Ali", "C", "D"), "Saad, Ali, & 2 others"],
    [
      people("Saad", "Ali", ...Array.from({ length: 10 }, (_, i) => `P${i}`)),
      "Saad, Ali, & 10 others",
    ],
  ])("%j reads %j", (list, line) => {
    expect(attendeeLine(list)).toBe(line);
  });

  it("uses the address when a calendar gave no name", () => {
    expect(attendeeLine([{ name: "", email: "jae@example.com" }, ...people("Juan")])).toBe(
      "jae & Juan",
    );
  });
});

describe("timeRange", () => {
  it("sets a shared AM apart so the page can draw it small", () => {
    expect(timeRange(at(2026, 11, 12, 9, 45), at(2026, 11, 12, 10, 15), "en-US")).toEqual({
      text: "9:45 – 10:15",
      suffix: "AM",
    });
  });

  it("spells out both periods when the meeting crosses noon", () => {
    expect(timeRange(at(2026, 11, 12, 9, 45), at(2026, 11, 12, 13, 15), "en-US")).toEqual({
      text: "9:45 AM – 1:15 PM",
      suffix: "",
    });
  });

  it("has no suffix in a 24-hour locale", () => {
    const { text, suffix } = timeRange(at(2026, 11, 12, 13, 0), at(2026, 11, 12, 14, 30), "de-DE");
    expect(text).toBe("13:00 – 14:30");
    expect(suffix).toBe("");
  });
});

describe("dayRows", () => {
  const now = new Date(2026, 10, 12, 15, 0); // Thu 12 Nov, 3 PM

  it("says there is nothing more today once today's last event is over", () => {
    const rows = dayRows(
      [event({ startUnix: at(2026, 11, 12, 9), endUnix: at(2026, 11, 12, 10) })],
      now,
      now,
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ isToday: true, events: [], noMoreToday: true });
  });

  it("keeps what is still ahead today, and drops what is over", () => {
    const later = event({
      id: "later",
      startUnix: at(2026, 11, 12, 16),
      endUnix: at(2026, 11, 12, 17),
    });
    const done = event({
      id: "done",
      startUnix: at(2026, 11, 12, 9),
      endUnix: at(2026, 11, 12, 10),
    });
    const [today] = dayRows([later, done], now, now);
    expect(today!.events.map((e) => e.id)).toEqual(["later"]);
    expect(today!.noMoreToday).toBe(false);
  });

  it("lists only days that have something, in date order", () => {
    const rows = dayRows(
      [
        event({ id: "mon", startUnix: at(2026, 11, 16, 9), endUnix: at(2026, 11, 16, 10) }),
        event({ id: "fri", startUnix: at(2026, 11, 13, 9), endUnix: at(2026, 11, 13, 10) }),
      ],
      now,
      now,
    );
    expect(rows.map((r) => r.day.getDate())).toEqual([12, 13, 16]);
  });

  it("leaves today out of a window that starts after it", () => {
    const nextWeek = new Date(2026, 10, 19);
    const rows = dayRows(
      [event({ startUnix: at(2026, 11, 20, 9), endUnix: at(2026, 11, 20, 10) })],
      nextWeek,
      now,
    );
    expect(rows.map((r) => [r.day.getDate(), r.isToday])).toEqual([[20, false]]);
  });

  it("drops events past the end of the window", () => {
    const rows = dayRows(
      [event({ startUnix: at(2026, 11, 19, 9), endUnix: at(2026, 11, 19, 10) })],
      now,
      now,
    );
    expect(rows.map((r) => r.day.getDate())).toEqual([12]);
  });
});

describe("groupPast", () => {
  const now = new Date(2026, 10, 12, 0, 30); // just after midnight

  it("splits Today from Yesterday at local midnight, then names the date", () => {
    const groups = groupPast(
      [
        meeting({ id: "a", startedAt: at(2026, 11, 12, 0, 5) }),
        meeting({ id: "b", startedAt: at(2026, 11, 11, 23, 55) }),
        meeting({ id: "c", startedAt: at(2026, 11, 11, 9) }),
        meeting({ id: "d", startedAt: at(2026, 11, 9, 16) }),
      ],
      now,
      "en-US",
    );
    expect(groups.map((g) => [g.label, g.meetings.map((m) => m.id)])).toEqual([
      ["Today", ["a"]],
      ["Yesterday", ["b", "c"]],
      ["Monday, Nov 9", ["d"]],
    ]);
  });

  it("labels a date in the week before by name", () => {
    expect(dayLabel(new Date(2026, 10, 10, 12), now, "en-US")).toBe("Tuesday, Nov 10");
  });
});

describe("tiles", () => {
  it("puts the first attendee's initial on the tile, else the title's", () => {
    expect(initialOf({ attendees: people("juan"), title: "Standup" })).toBe("J");
    expect(initialOf({ attendees: [], title: "standup" })).toBe("S");
    expect(initialOf({ attendees: [], title: "" })).toBe("?");
  });
});

describe("segmentClock", () => {
  it.each([
    [0, "0:00"],
    [65_000, "1:05"],
    [724_000, "12:04"],
    [3_723_000, "1:02:03"],
  ])("%d ms is %s", (ms, text) => {
    expect(segmentClock(ms)).toBe(text);
  });
});

describe("duration", () => {
  const start = at(2026, 11, 12, 9);
  it.each([
    [32 * 60, "32 min"],
    [20, "1 min"],
    [3600, "1 h"],
    [5400, "1 h 30 min"],
  ])("%d s is %s", (seconds, text) => {
    expect(duration(start, start + seconds)).toBe(text);
  });

  it("is empty while the meeting is still recording", () => {
    expect(duration(start, 0)).toBe("");
  });
});

describe("shortDay", () => {
  const now = new Date(2026, 10, 13, 9, 0);
  it.each([
    [new Date(2026, 10, 13, 23, 59), "Today"],
    [new Date(2026, 10, 12, 0, 1), "Yesterday"],
    [new Date(2026, 10, 14, 8, 0), "Tomorrow"],
    [new Date(2026, 10, 10, 12, 0), "Tue, Nov 10"],
  ])("%s is %s", (day, label) => {
    expect(shortDay(day, now, "en-US")).toBe(label);
  });
});

describe("eventWhen", () => {
  const now = new Date(2026, 10, 13, 9, 0);
  it("names the day and the time range with its period", () => {
    expect(
      eventWhen(
        { startUnix: at(2026, 11, 13, 9, 45), endUnix: at(2026, 11, 13, 10, 15), allDay: false },
        now,
        "en-US",
      ),
    ).toBe("Today · 9:45 – 10:15 AM");
  });

  it("says All day instead of a time for an all-day event", () => {
    expect(
      eventWhen(
        { startUnix: at(2026, 11, 14), endUnix: at(2026, 11, 15), allDay: true },
        now,
        "en-US",
      ),
    ).toBe("Tomorrow · All day");
  });
});

describe("fullNames", () => {
  it("gives full names, an address when there is no name, and skips blanks", () => {
    expect(
      fullNames([
        { name: " Juan Pérez ", email: "juan@example.com" },
        { name: "", email: "jae@example.com" },
        { name: " ", email: "" },
      ]),
    ).toEqual(["Juan Pérez", "jae@example.com"]);
  });
});
