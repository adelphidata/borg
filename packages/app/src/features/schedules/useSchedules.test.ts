import { describe, expect, it } from "vitest";
import { describeSchedule, intervalSeconds, relativeTime } from "./useSchedules";

describe("describeSchedule", () => {
  it("says intervals in the largest whole unit that fits", () => {
    expect(describeSchedule("interval", "3600")).toBe("every 1 hour");
    expect(describeSchedule("interval", "7200")).toBe("every 2 hours");
    expect(describeSchedule("interval", "300")).toBe("every 5 minutes");
    expect(describeSchedule("interval", "60")).toBe("every 1 minute");
    expect(describeSchedule("interval", "45")).toBe("every 45s");
  });

  it("says the common cron shapes in words, in every field count the daemon stores", () => {
    const cases: [string, string][] = [
      ["* * * * *", "every minute"],
      ["0 * * * * * *", "every minute"],
      ["*/5 * * * *", "every 5 minutes"],
      ["15 * * * *", "hourly at :15"],
      ["0 8 * * *", "daily at 08:00"],
      ["0 0 3 * * *", "daily at 03:00"],
      ["0 0 9 * * 1-5", "weekdays at 09:00"],
      ["30 9 * * MON-FRI", "weekdays at 09:30"],
      ["0 10 * * 0,6", "weekends at 10:00"],
      // Crontab numbering: 7 and 0 are both Sunday.
      ["0 0 4 * * 7", "every Sunday at 04:00"],
      ["0 4 * * 1", "every Monday at 04:00"],
      ["0 0 9 1 * *", "monthly on day 1 at 09:00"],
    ];
    for (const [expr, want] of cases) {
      expect(describeSchedule("cron", expr), expr).toBe(want);
    }
  });

  it("shows anything it cannot say honestly as crontab fields", () => {
    expect(describeSchedule("cron", "0 9 * * 1,3")).toBe("0 9 * * 1,3");
    expect(describeSchedule("cron", "0 0 9 * 1 *")).toBe("0 9 * 1 *");
    // A non-zero seconds field is kept rather than silently dropped.
    expect(describeSchedule("cron", "30 0 9 * * *")).toBe("30 0 9 * * *");
    expect(describeSchedule("cron", "garbage")).toBe("garbage");
  });

  it("labels a one-shot task", () => {
    expect(describeSchedule("once", "2026-08-10T09:00")).toBe("once — 2026-08-10T09:00");
    expect(describeSchedule("once", "")).toBe("once");
  });

  it("understands the suffix grammar its own form suggests", () => {
    // The daemon's `parse_interval` takes s/m/h/d; the form's placeholder is
    // "30m", so these are the expressions this page actually creates.
    expect(describeSchedule("interval", "30m")).toBe("every 30 minutes");
    expect(describeSchedule("interval", "6h")).toBe("every 6 hours");
    expect(describeSchedule("interval", "90s")).toBe("every 90s");
    expect(describeSchedule("interval", "1d")).toBe("every 1 day");
  });

  it("falls back to the raw expression when an interval is not a number", () => {
    expect(describeSchedule("interval", "soon")).toBe("soon");
    expect(describeSchedule("interval", "-30")).toBe("-30");
  });
});

describe("intervalSeconds", () => {
  it("mirrors the daemon's units", () => {
    expect(intervalSeconds("45")).toBe(45);
    expect(intervalSeconds("30m")).toBe(1_800);
    expect(intervalSeconds("6h")).toBe(21_600);
    expect(intervalSeconds("2d")).toBe(172_800);
  });

  it("rejects what the daemon would also reject", () => {
    for (const bad of ["", "soon", "-30", "0", "30 m", "1w", "1.5h"]) {
      expect(intervalSeconds(bad)).toBeNull();
    }
  });
});

describe("relativeTime", () => {
  const now = 1_800_000_000_000; // ms
  const nowSeconds = now / 1000;

  it("counts seconds, then minutes, then hours, then days", () => {
    expect(relativeTime(nowSeconds + 30, now)).toBe("in 30s");
    expect(relativeTime(nowSeconds + 600, now)).toBe("in 10 min");
    expect(relativeTime(nowSeconds + 7200, now)).toBe("in 2 h");
    expect(relativeTime(nowSeconds + 5 * 86400, now)).toBe("in 5 d");
  });

  it("says due now while the once-a-minute check has not picked a task up yet", () => {
    expect(relativeTime(nowSeconds, now)).toBe("due now");
    expect(relativeTime(nowSeconds - 60, now)).toBe("due now");
  });

  it("says overdue only past that grace, rather than a negative countdown", () => {
    expect(relativeTime(nowSeconds - 91, now)).toBe("overdue");
    expect(relativeTime(nowSeconds - 3600, now)).toBe("overdue");
  });

  it("shows a dash when there is no next run at all", () => {
    expect(relativeTime(0, now)).toBe("—");
  });
});
