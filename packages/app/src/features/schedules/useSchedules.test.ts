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

  it("passes a cron expression through — there is no honest short form", () => {
    expect(describeSchedule("cron", "0 8 * * *")).toBe("0 8 * * *");
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

  it("says overdue rather than showing a negative countdown", () => {
    expect(relativeTime(nowSeconds - 60, now)).toBe("overdue");
  });

  it("shows a dash when there is no next run at all", () => {
    expect(relativeTime(0, now)).toBe("—");
  });
});
