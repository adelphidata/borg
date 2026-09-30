import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { clearNotice, pendingNotice } from "../../lib/notice";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { SchedulesPage } from "./SchedulesPage";
import { reportWhenDone, runOutcome, SCHEDULES_REFRESH_MS } from "./useSchedules";

const NOW = Date.UTC(2026, 7, 22, 12, 0, 0);

const task = (over: Record<string, unknown> = {}) => ({
  id: "t1",
  name: "Morning briefing",
  prompt: "summarise my mail",
  scheduleType: "cron",
  scheduleExpr: "0 0 8 * * *",
  timezone: "UTC",
  status: "active",
  nextRunUnix: Math.floor(NOW / 1000) + 3600,
  createdAt: 0,
  taskType: "prompt",
  deliveryChannel: "",
  deliveryTarget: "",
  ...over,
});

const listing = (tasks: unknown[]) => ({ ListScheduledTasks: { tasks } });

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(<SchedulesPage />, { daemon });
}

const click = (name: string | HTMLElement) =>
  act(() => (typeof name === "string" ? screen.getByText(name) : name).click());

beforeEach(() => {
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
  clearNotice();
});

describe("SchedulesPage listing", () => {
  it("says a schedule the way a person would say it", async () => {
    const daemon = fakeDaemon(listing([task({ scheduleType: "interval", scheduleExpr: "30m" })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());
    expect(screen.getByText(/every 30 minutes/)).toBeTruthy();
  });

  it("counts down to the next run rather than printing a clock time", async () => {
    const daemon = fakeDaemon(listing([task()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText(/next in 60 min/)).toBeTruthy());
  });

  it("offers pause but not delete on a task Borg runs for itself", async () => {
    const daemon = fakeDaemon(
      listing([
        task({ id: "sys", name: "Heartbeat", system: true }),
        task({ id: "mine", name: "Morning briefing" }),
      ]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Heartbeat")).toBeTruthy());
    expect(screen.queryByLabelText("Delete Heartbeat")).toBeNull();
    expect(screen.getByLabelText("Delete Morning briefing")).toBeTruthy();
    expect(screen.getAllByText("Pause")).toHaveLength(2);
  });

  it("says due now, not 'next due now', for a task waiting on the next check", async () => {
    const daemon = fakeDaemon(listing([task({ nextRunUnix: Math.floor(NOW / 1000) - 10 })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText(/· due now/)).toBeTruthy());
    expect(screen.queryByText(/next due/)).toBeNull();
  });

  it("says a task is paused instead of counting down to a run that will not happen", async () => {
    const daemon = fakeDaemon(listing([task({ status: "paused" })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText(/· paused/)).toBeTruthy());
    expect(screen.queryByText(/next in/)).toBeNull();
  });

  it("names a finished task's outcome", async () => {
    const daemon = fakeDaemon(listing([task({ status: "completed" })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText(/· completed/)).toBeTruthy());
  });

  it("says the list is empty rather than showing nothing", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Nothing scheduled yet.")).toBeTruthy());
  });

  it("reports a failed listing", async () => {
    const daemon = fakeDaemon({
      ListScheduledTasks: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() =>
      expect(screen.getByText("Could not load your scheduled tasks.")).toBeTruthy(),
    );
  });
});

describe("SchedulesPage last run", () => {
  const run = (over: Record<string, unknown> = {}) => ({
    id: 1,
    taskId: "t1",
    startedAtUnix: Math.floor(NOW / 1000) - 60,
    durationMs: 900,
    status: "success",
    result: "The time is 12:00.",
    error: "",
    ...over,
  });

  it("shows what a task said last time, one click deep", async () => {
    // A task with no chat channel keeps its results with the task; without
    // this there was nowhere in the app to read them.
    const daemon = fakeDaemon({
      ...listing([task()]),
      GetScheduledTaskDetail: { task: task(), hasLastRun: true, lastRun: run() },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());
    expect(screen.queryByText("The time is 12:00.")).toBeNull();

    const row = screen.getByRole("button", { name: /^Morning briefing/ });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    click(row);
    await waitFor(() => expect(screen.getByText("The time is 12:00.")).toBeTruthy());
    expect(screen.getByText(/Last run .* · Finished/)).toBeTruthy();

    click(row);
    expect(screen.queryByText("The time is 12:00.")).toBeNull();
  });

  it("shows a failed run's reason instead of its empty result", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      GetScheduledTaskDetail: {
        task: task(),
        hasLastRun: true,
        lastRun: run({ status: "misconfigured", result: "", error: "no delivery target" }),
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());
    click(screen.getByRole("button", { name: /^Morning briefing/ }));
    await waitFor(() => expect(screen.getByText("no delivery target")).toBeTruthy());
    expect(screen.getByText(/Could not be delivered/)).toBeTruthy();
  });

  it("says a skipped heartbeat run in words, never the stored reason code", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      GetScheduledTaskDetail: {
        task: task(),
        hasLastRun: true,
        lastRun: run({ result: "interactive_input_unavailable" }),
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());
    click(screen.getByRole("button", { name: /^Morning briefing/ }));
    await waitFor(() => expect(screen.getByText(/a scheduled run cannot ask/)).toBeTruthy());
    expect(screen.queryByText("interactive_input_unavailable")).toBeNull();
  });

  it("says a task has not run yet", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      GetScheduledTaskDetail: { task: task(), hasLastRun: false },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());
    click(screen.getByRole("button", { name: /^Morning briefing/ }));
    await waitFor(() => expect(screen.getByText("Not run yet.")).toBeTruthy());
  });

  it("re-reads the list on its own, so a countdown does not go stale", async () => {
    vi.useFakeTimers({ now: NOW });
    const daemon = fakeDaemon(listing([task()]));
    mount(daemon);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    const reads = () => daemon.calls.filter((c) => c.method === "ListScheduledTasks").length;
    const before = reads();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(SCHEDULES_REFRESH_MS);
    });
    expect(reads()).toBe(before + 1);
  });
});

describe("SchedulesPage actions", () => {
  const withActions = (over: Record<string, unknown> = {}) =>
    fakeDaemon({
      ...listing([task(over)]),
      PauseScheduledTask: {},
      ResumeScheduledTask: {},
      RunScheduledTaskNow: {},
      DeleteScheduledTask: {},
      GetScheduledTaskRuns: { runs: [] },
    });

  it("pauses a running task", async () => {
    const daemon = withActions();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Pause")).toBeTruthy());

    click("Pause");
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PauseScheduledTask")?.request).toEqual({
        id: "t1",
      }),
    );
  });

  it("offers Resume, not Pause, for a paused task", async () => {
    const daemon = withActions({ status: "paused" });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Resume")).toBeTruthy());
    expect(screen.queryByText("Pause")).toBeNull();

    click("Resume");
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "ResumeScheduledTask")).toBe(true),
    );
  });

  it("offers no pause control for a task that has already finished", async () => {
    const daemon = withActions({ status: "cancelled" });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Run now")).toBeTruthy());
    expect(screen.queryByText("Pause")).toBeNull();
    expect(screen.queryByText("Resume")).toBeNull();
  });

  it("runs a task on demand", async () => {
    const daemon = withActions();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Run now")).toBeTruthy());

    click("Run now");
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "RunScheduledTaskNow")).toBe(true),
    );
  });

  it("deletes a task from a control that names it, once confirmed", async () => {
    const daemon = withActions();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());

    click(screen.getByRole("button", { name: "Delete Morning briefing" }));
    expect(screen.getByText("Delete Morning briefing?")).toBeTruthy();
    expect(daemon.calls.some((c) => c.method === "DeleteScheduledTask")).toBe(false);
    click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "DeleteScheduledTask")?.request).toEqual({
        id: "t1",
      }),
    );
  });

  it("keeps a task when the delete is called off", async () => {
    const daemon = withActions();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());

    click(screen.getByRole("button", { name: "Delete Morning briefing" }));
    click(screen.getByRole("button", { name: "Keep" }));
    expect(screen.getByText("Morning briefing")).toBeTruthy();
    expect(daemon.calls.some((c) => c.method === "DeleteScheduledTask")).toBe(false);
  });

  it("surfaces a failed action", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      PauseScheduledTask: new BorgError(GrpcStatus.NotFound, "no such task"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Pause")).toBeTruthy());

    click("Pause");
    await waitFor(() =>
      expect(pendingNotice()?.message).toBe("Could not pause “Morning briefing”: no such task"),
    );
  });

  it("says a pause went through, naming the task", async () => {
    mount(withActions());
    await waitFor(() => expect(screen.getByText("Pause")).toBeTruthy());

    click("Pause");
    await waitFor(() => expect(pendingNotice()?.message).toBe("Paused “Morning briefing”"));
  });

  it("offers to open the conversation a run now happens in", async () => {
    const daemon = fakeDaemon({
      ...listing([task({ sessionId: "task:t1" })]),
      RunScheduledTaskNow: {},
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Run now")).toBeTruthy());

    click("Run now");
    await waitFor(() =>
      expect(pendingNotice()).toMatchObject({
        message: "Running “Morning briefing”",
        openPath: "/c/task%3At1",
      }),
    );
  });
});

describe("SchedulesPage creation", () => {
  const openForm = async (daemon: ReturnType<typeof fakeDaemon>) => {
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Nothing scheduled yet.")).toBeTruthy());
    click("New scheduled task");
  };

  const fill = (name: string, prompt: string, expr: string) => {
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: name } });
    fireEvent.change(screen.getByPlaceholderText("What should Borg do?"), {
      target: { value: prompt },
    });
    const exprBox = screen.getByPlaceholderText(/0 8 \* \* 1-5|30m|2026-08/);
    fireEvent.change(exprBox, { target: { value: expr } });
  };

  it("keeps the form closed until it is asked for", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Nothing scheduled yet.")).toBeTruthy());
    expect(screen.queryByPlaceholderText("Name")).toBeNull();
  });

  it("explains the expression field per schedule kind", async () => {
    // The daemon reads crontab's five fields, weekdays numbered the crontab way.
    const daemon = fakeDaemon(listing([]));
    await openForm(daemon);
    expect(screen.getByText(/^minute hour day month weekday/)).toBeTruthy();

    click("interval");
    expect(screen.getByText(/How long between runs/)).toBeTruthy();

    click("once");
    expect(screen.getByText("When to run, once")).toBeTruthy();
  });

  it("refuses to submit until every field is filled", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateScheduledTask: {} });
    await openForm(daemon);
    expect(screen.getByText("Create")).toHaveProperty("disabled", true);

    fill("Briefing", "summarise my mail", "0 0 8 * * *");
    expect(screen.getByText("Create")).toHaveProperty("disabled", false);
  });

  it("creates the task, trimmed, in the user's own time zone", async () => {
    // "every day at 08:00" has to mean the user's morning.
    const daemon = fakeDaemon({ ...listing([]), CreateScheduledTask: {} });
    await openForm(daemon);
    fill("  Briefing  ", "  summarise my mail  ", "  0 0 8 * * *  ");
    click("Create");

    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "CreateScheduledTask")?.request).toMatchObject({
        name: "Briefing",
        prompt: "summarise my mail",
        scheduleType: "cron",
        scheduleExpr: "0 0 8 * * *",
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
        taskType: "prompt",
      }),
    );
  });

  it("closes and clears the form once the task is stored", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateScheduledTask: {} });
    await openForm(daemon);
    fill("Briefing", "summarise my mail", "30m");
    click("Create");

    await waitFor(() => expect(screen.queryByPlaceholderText("Name")).toBeNull());
    click("New scheduled task");
    expect(screen.getByPlaceholderText("Name")).toHaveProperty("value", "");
  });

  it("keeps everything typed when the daemon rejects the schedule", async () => {
    // The expression has to be correctable in place.
    const daemon = fakeDaemon({
      ...listing([]),
      CreateScheduledTask: new BorgError(INVALID_ARGUMENT, "invalid cron expression"),
    });
    await openForm(daemon);
    fill("Briefing", "summarise my mail", "0 8 * * *");
    click("Create");

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("invalid cron"));
    expect(screen.getByPlaceholderText("Name")).toHaveProperty("value", "Briefing");
    expect(screen.getByDisplayValue("0 8 * * *")).toBeTruthy();
  });

  it("abandons the form on cancel", async () => {
    const daemon = fakeDaemon(listing([]));
    await openForm(daemon);
    fill("Briefing", "summarise", "30m");
    click("Cancel");
    expect(screen.queryByPlaceholderText("Name")).toBeNull();
  });
});

describe("SchedulesPage editing", () => {
  it("opens a task's own values, the cron as the five fields a person types", async () => {
    const daemon = fakeDaemon({ ...listing([task()]), UpdateScheduledTask: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Edit")).toBeTruthy());

    click("Edit");
    expect(screen.getByPlaceholderText("Name")).toHaveProperty("value", "Morning briefing");
    expect(screen.getByDisplayValue("summarise my mail")).toBeTruthy();
    expect(screen.getByDisplayValue("0 8 * * *")).toBeTruthy();
  });

  it("saves the changes and leaves the stored time zone alone", async () => {
    const daemon = fakeDaemon({ ...listing([task()]), UpdateScheduledTask: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Edit")).toBeTruthy());
    click("Edit");
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: " Briefing " } });
    fireEvent.change(screen.getByDisplayValue("0 8 * * *"), { target: { value: "0 9 * * 1-5" } });
    click("Save");

    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UpdateScheduledTask")?.request).toEqual({
        id: "t1",
        name: "Briefing",
        prompt: "summarise my mail",
        scheduleType: "cron",
        scheduleExpr: "0 9 * * 1-5",
        timezone: "",
      }),
    );
    await waitFor(() => expect(screen.queryByText("Save")).toBeNull());
  });

  it("leaves the schedule out when only the name changed, so the next run stays", async () => {
    const daemon = fakeDaemon({ ...listing([task()]), UpdateScheduledTask: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Edit")).toBeTruthy());
    click("Edit");
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Briefing" } });
    click("Save");

    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UpdateScheduledTask")?.request).toEqual({
        id: "t1",
        name: "Briefing",
        prompt: "summarise my mail",
        scheduleType: "",
        scheduleExpr: "",
        timezone: "",
      }),
    );
  });

  it("keeps the edit open when the daemon rejects it", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      UpdateScheduledTask: new BorgError(INVALID_ARGUMENT, "invalid cron expression"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Edit")).toBeTruthy());
    click("Edit");
    fireEvent.change(screen.getByDisplayValue("0 8 * * *"), { target: { value: "nope" } });
    click("Save");

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("invalid cron"));
    expect(screen.getByDisplayValue("nope")).toBeTruthy();
  });

  it("offers no edit on a task Borg runs for itself", async () => {
    const daemon = fakeDaemon(listing([task({ name: "Heartbeat", system: true })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Heartbeat")).toBeTruthy());
    expect(screen.queryByText("Edit")).toBeNull();
  });
});

describe("a run with no conversation to open", () => {
  const run = (over: Record<string, unknown> = {}) => ({
    id: 8,
    taskId: "t1",
    startedAtUnix: 0,
    durationMs: 0,
    status: "success",
    result: "",
    error: "",
    ...over,
  });

  it("says how it ended", () => {
    expect(runOutcome("“Tidy”", run())).toBe("“Tidy” finished");
    expect(runOutcome("“Tidy”", run({ status: "failed", error: "disk full" }))).toBe(
      "“Tidy” failed: disk full",
    );
    expect(runOutcome("“Tidy”", run({ status: "running" }))).toBeNull();
  });

  it("waits for the run the press started, then reports it once", async () => {
    // The run from before the press, then the new one running, then done.
    const answers = [[run({ id: 7 })], [run({ status: "running" })], [run()]];
    const getScheduledTaskRuns = vi.fn(async () => ({ runs: answers.shift() ?? [] }));
    const borg = { admin: { getScheduledTaskRuns } } as never;

    await reportWhenDone(borg, "t1", "“Tidy”", 7, async () => {});

    expect(getScheduledTaskRuns).toHaveBeenCalledTimes(3);
    expect(pendingNotice()?.message).toBe("“Tidy” finished");
  });

  it("asks where the runs stand before pressing, so it knows which one is new", async () => {
    const daemon = fakeDaemon({
      ...listing([task({ taskType: "maintenance" })]),
      RunScheduledTaskNow: {},
      GetScheduledTaskRuns: { runs: [run({ id: 3 })] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Run now")).toBeTruthy());

    click("Run now");
    await waitFor(() => expect(pendingNotice()?.message).toBe("Running “Morning briefing”"));
    const methods = daemon.calls.map((c) => c.method);
    expect(methods.indexOf("GetScheduledTaskRuns")).toBeLessThan(
      methods.indexOf("RunScheduledTaskNow"),
    );
    expect(pendingNotice()?.openPath).toBeUndefined();
  });
});
