import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { HealthPage, splitWarning } from "./HealthPage";

const NOW = Date.UTC(2026, 7, 22, 12, 0, 0);

const HEALTHY = {
  GetVitals: {
    stage: "Companion",
    xp: 4200,
    stability: 98,
    focus: 91,
    sync: 88,
    growth: 73,
    happiness: 84,
  },
  GetDaemonStatus: {
    version: "0.4.1",
    uptimeSecs: 7_200,
    llmProvider: "anthropic",
    llmModel: "claude-opus-5",
    activeTasks: 2,
    pausedTasks: 0,
  },
  GetPersistentWarnings: { warnings: [] },
  GetSubprocesses: { running: [] },
  GetPosture: { posture: "Default", xpMultiplier: 1 },
  GetHeartbeat: { nextInSeconds: 900, lastUnixTs: Math.floor(NOW / 1000) - 600 },
  GetMonthlyTokenTotal: { total: 1_200_000 },
  GetMonthlyTotalCost: { present: true, cost: 12.5 },
};

const mount = (daemon: ReturnType<typeof fakeDaemon>) => renderApp(<HealthPage />, { daemon });
const click = (name: string) => act(() => screen.getByText(name).click());
/** The status dot in the row named `name`. */
const dotOf = (name: string) => {
  const dot = screen.getByText(name).parentElement?.querySelector("span.rounded-full");
  if (!dot) throw new Error(`no dot in the ${name} row`);
  return dot;
};
let warn: ReturnType<typeof vi.spyOn>;

beforeEach(() => {
  vi.setSystemTime(NOW);
  warn = vi.spyOn(console, "warn").mockImplementation(() => {});
});

afterEach(() => {
  vi.useRealTimers();
  warn.mockRestore();
});

describe("splitWarning", () => {
  it("splits a category off the front", () => {
    expect(splitWarning("Sandbox: seatbelt profile missing")).toEqual({
      label: "Sandbox",
      text: "seatbelt profile missing",
    });
  });

  it("keeps a warning that carries no category whole", () => {
    expect(splitWarning("disk almost full")).toEqual({ label: "", text: "disk almost full" });
  });

  it("does not treat a leading colon as a category", () => {
    expect(splitWarning(":odd")).toEqual({ label: "", text: ":odd" });
  });
});

describe("HealthPage", () => {
  it("says Borg is healthy when every check passes", async () => {
    mount(fakeDaemon(HEALTHY));
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());
  });

  it("names the checks that need a look", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetPersistentWarnings: { warnings: ["Sandbox: profile missing"] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg needs a look.")).toBeTruthy());
    expect(screen.getByText(/Needs you: Warnings/)).toBeTruthy();
  });

  it("shows the vitals the daemon reports", async () => {
    mount(fakeDaemon(HEALTHY));
    await waitFor(() => expect(screen.getByText("Companion")).toBeTruthy());
    expect(screen.getByText("4200")).toBeTruthy();
  });

  it("keeps each check's explanation behind a click", async () => {
    // Progressive disclosure: the row is the answer, the detail is for when it
    // is not enough.
    mount(fakeDaemon(HEALTHY));
    await waitFor(() => expect(screen.getByText("Borg is running")).toBeTruthy());
    expect(screen.queryByText(/Version 0.4.1/)).toBeNull();

    click("Borg is running");
    expect(screen.getByText(/Version 0.4.1, up for 2 hours/)).toBeTruthy();
    expect(screen.getByText(/Thinking with claude-opus-5 on anthropic/)).toBeTruthy();
  });

  it("makes each check a button that says whether it is open", async () => {
    // A clickable div was unreachable by keyboard and silent to screen readers.
    mount(fakeDaemon(HEALTHY));
    await waitFor(() => expect(screen.getByText("Borg is running")).toBeTruthy());
    const row = screen.getByRole("button", { name: /Borg is running/ });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    click("Borg is running");
    expect(row.getAttribute("aria-expanded")).toBe("true");
  });

  it("closes an expanded check again", async () => {
    mount(fakeDaemon(HEALTHY));
    await waitFor(() => expect(screen.getByText("Borg is running")).toBeTruthy());
    click("Borg is running");
    click("Borg is running");
    expect(screen.queryByText(/Version 0.4.1/)).toBeNull();
  });

  it("says no provider is configured rather than naming a model that is not there", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetDaemonStatus: { ...HEALTHY.GetDaemonStatus, llmProvider: "", llmModel: "" },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is running")).toBeTruthy());
    click("Borg is running");
    expect(screen.getByText(/No model provider is configured yet./)).toBeTruthy();
  });

  it("lists the warnings, split into category and message", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetPersistentWarnings: { warnings: ["Sandbox: profile missing", "disk almost full"] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("2 warnings")).toBeTruthy());

    click("Warnings");
    expect(screen.getByText("profile missing")).toBeTruthy();
    expect(screen.getByText("disk almost full")).toBeTruthy();
    expect(screen.getByText(/Sandbox/)).toBeTruthy();
  });

  it("marks a reduced posture as needing attention", async () => {
    // Reduced means secrets sit in a file rather than the keychain.
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetPosture: { posture: "Reduced", xpMultiplier: 0.85 },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Reduced")).toBeTruthy());
    expect(screen.getByText(/Needs you: Security posture/)).toBeTruthy();

    click("Security posture");
    expect(screen.getByText(/reduced protections/)).toBeTruthy();
  });

  it("says check-ins are off rather than counting down to one that never comes", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetHeartbeat: { nextInSeconds: 0, lastUnixTs: 0 },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("off")).toBeTruthy());

    click("Proactive check-ins");
    expect(screen.getByText(/only acts when you ask/)).toBeTruthy();
  });

  it("counts paused schedules alongside the active ones", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetDaemonStatus: { ...HEALTHY.GetDaemonStatus, activeTasks: 3, pausedTasks: 1 },
      GetSubprocesses: { running: [{ id: 1 }, { id: 2 }] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("3 schedules · 2 processes")).toBeTruthy());

    click("Background work");
    expect(screen.getByText(/3 scheduled tasks active, 1 paused/)).toBeTruthy();
  });

  it("says there is no cost estimate rather than showing a false zero", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetMonthlyTotalCost: { present: false, cost: 0 },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Usage this month")).toBeTruthy());

    click("Usage this month");
    expect(screen.getByText(/No price data yet/)).toBeTruthy();
  });

  it("re-runs both queries on demand", async () => {
    const daemon = fakeDaemon(HEALTHY);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());
    const before = daemon.calls.filter((c) => c.method === "GetVitals").length;

    click("Check again");
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "GetVitals").length).toBe(before + 1),
    );
  });

  it("reports what a tidy-up actually did", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      Heal: {
        skipped: false,
        prunedLogs: 120,
        prunedActivity: 0,
        evictedEmbeddings: 4,
        healedTasks: 0,
        messagesPurged: 0,
        wedgedRunsRecovered: 0,
        backupsPruned: 0,
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());

    click("Tidy up now");
    await waitFor(() =>
      expect(
        screen.getByText("Tidy-up done: 120 log entries pruned, 4 embeddings evicted."),
      ).toBeTruthy(),
    );
  });

  it("says a sweep that found nothing found nothing", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      Heal: {
        skipped: false,
        prunedLogs: 0,
        prunedActivity: 0,
        evictedEmbeddings: 0,
        healedTasks: 0,
        messagesPurged: 0,
        wedgedRunsRecovered: 0,
        backupsPruned: 0,
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());

    click("Tidy up now");
    await waitFor(() =>
      expect(screen.getByText("Tidy-up done. Nothing needed cleaning.")).toBeTruthy(),
    );
  });

  it("says when a sweep was skipped because one just ran", async () => {
    const daemon = fakeDaemon({ ...HEALTHY, Heal: { skipped: true } });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());

    click("Tidy up now");
    await waitFor(() =>
      expect(
        screen.getByText("Nothing to tidy — the last sweep already ran recently."),
      ).toBeTruthy(),
    );
  });

  it("reports a failed tidy-up", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      Heal: new BorgError(GrpcStatus.Internal, "database is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is healthy.")).toBeTruthy());

    click("Tidy up now");
    await waitFor(() =>
      expect(screen.getByText("Tidy-up failed: database is locked")).toBeTruthy(),
    );
  });

  // One dropped call must not blank the page: the other six answers are still
  // true, and the row that could not be checked says so in its own words.
  it.each([
    ["GetPersistentWarnings", "Warnings"],
    ["GetPosture", "Security posture"],
    ["GetHeartbeat", "Proactive check-ins"],
    ["GetSubprocesses", "Background work"],
    ["GetMonthlyTokenTotal", "Usage this month"],
  ])("keeps the other rows when %s fails", async (method, row) => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      [method]: new BorgError(GrpcStatus.Unavailable, "timed out"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Borg is running")).toBeTruthy());
    expect(screen.getByText("2 hours without a restart")).toBeTruthy();
    expect(screen.queryByText("Could not load health checks.")).toBeNull();
    expect(screen.getAllByText("Couldn't check")).toHaveLength(1);

    click(row);
    expect(screen.getByText(/Borg did not answer this check/)).toBeTruthy();
    // Nothing is known to be wrong, but nothing is known to be right either:
    // the headline must not call Borg healthy, nor raise an alarm.
    expect(screen.queryByText("Borg is healthy.")).toBeNull();
    expect(screen.queryByText("Borg needs a look.")).toBeNull();
    expect(screen.getByText("Some checks did not answer.")).toBeTruthy();
    expect(screen.getByText(new RegExp(`No answer from: ${row}\\. Try again`))).toBeTruthy();
    // Its dot is neutral grey, neither the healthy green nor the alert red.
    const dot = dotOf(row);
    expect(dot.className).toContain("bg-ink3");
    expect(dot.className).not.toMatch(/bg-neon|bg-alert/);
    expect(warn).toHaveBeenCalledWith(expect.stringContaining("did not answer"), expect.anything());
  });

  it("paints answered rows green and a failing one red", async () => {
    mount(fakeDaemon({ ...HEALTHY, GetPosture: { posture: "Reduced", xpMultiplier: 0.85 } }));
    await waitFor(() => expect(screen.getByText("Borg needs a look.")).toBeTruthy());
    expect(dotOf("Security posture").className).toContain("bg-alert");
    expect(dotOf("Warnings").className).toContain("bg-neon");
  });

  it("puts an alert ahead of a check that did not answer", async () => {
    mount(
      fakeDaemon({
        ...HEALTHY,
        GetPosture: { posture: "Reduced", xpMultiplier: 0.85 },
        GetHeartbeat: new BorgError(GrpcStatus.Unavailable, "timed out"),
      }),
    );
    await waitFor(() => expect(screen.getByText("Borg needs a look.")).toBeTruthy());
    expect(
      screen.getByText(/Needs you: Security posture\. No answer from: Proactive/),
    ).toBeTruthy();
  });

  it("still counts tokens when only the price call fails", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetMonthlyTotalCost: new BorgError(GrpcStatus.Unavailable, "timed out"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("1,200,000 tokens")).toBeTruthy());
    expect(screen.queryByText("Couldn't check")).toBeNull();
  });

  it("reports a failed health load without hiding the vitals it did get", async () => {
    const daemon = fakeDaemon({
      ...HEALTHY,
      GetDaemonStatus: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load health checks.")).toBeTruthy());
    expect(screen.getByText("Companion")).toBeTruthy();
  });
});
