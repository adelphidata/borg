import { describe, expect, it } from "vitest";
import { formatClock, formatElapsed } from "./format";

describe("formatClock", () => {
  const time = (d: Date) => d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });

  it("is just the time for a message sent today", () => {
    const at = new Date(2026, 8, 29, 21, 2, 45);
    const now = new Date(2026, 8, 29, 23, 0).getTime();
    expect(formatClock(at.getTime(), now)).toBe(time(at));
  });

  it("adds the day for an older message, and the year only when it differs", () => {
    const yesterday = new Date(2026, 8, 28, 9, 5);
    const now = new Date(2026, 8, 29, 8, 0).getTime();
    const day = yesterday.toLocaleDateString(undefined, { day: "numeric", month: "short" });
    expect(formatClock(yesterday.getTime(), now)).toBe(`${day}, ${time(yesterday)}`);

    const lastYear = new Date(2025, 11, 31, 23, 59);
    const withYear = lastYear.toLocaleDateString(undefined, {
      day: "numeric",
      month: "short",
      year: "numeric",
    });
    expect(formatClock(lastYear.getTime(), now)).toBe(`${withYear}, ${time(lastYear)}`);
  });
});

describe("formatElapsed", () => {
  it("keeps a tenth of a second under a minute", () => {
    // Most turns land here, and the difference between a 0.8s answer and a
    // 4.2s one is the whole reason the badge is on the row.
    expect(formatElapsed(800)).toBe("0.8s");
    expect(formatElapsed(12_360)).toBe("12.4s");
  });

  it("switches to minutes and whole seconds at a minute", () => {
    // The bug this guards: a tenth of a second that rounds up to the minute
    // used to print "60.0s", a minute spelled in the sub-minute form.
    expect(formatElapsed(59_990)).toBe("1m 00s");
    expect(formatElapsed(60_000)).toBe("1m 00s");
    expect(formatElapsed(125_000)).toBe("2m 05s");
  });

  it("carries a rounded-up second instead of printing 60", () => {
    // Rounding the minutes and the seconds apart is what produces "59m 60s".
    expect(formatElapsed(3_599_700)).toBe("1h 00m");
    expect(formatElapsed(119_700)).toBe("2m 00s");
  });

  it("switches to hours and minutes past an hour", () => {
    expect(formatElapsed(3_600_000)).toBe("1h 00m");
    expect(formatElapsed(3_840_000)).toBe("1h 04m");
  });

  it("shows nothing negative when the clock disagrees with itself", () => {
    expect(formatElapsed(-500)).toBe("0.0s");
  });
});
