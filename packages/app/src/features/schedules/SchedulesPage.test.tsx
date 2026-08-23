import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { SchedulesPage } from "./SchedulesPage";

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

describe("SchedulesPage actions", () => {
  const withActions = (over: Record<string, unknown> = {}) =>
    fakeDaemon({
      ...listing([task(over)]),
      PauseScheduledTask: {},
      ResumeScheduledTask: {},
      RunScheduledTaskNow: {},
      DeleteScheduledTask: {},
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

  it("deletes a task from a control that names it", async () => {
    const daemon = withActions();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Morning briefing")).toBeTruthy());

    click(screen.getByRole("button", { name: "Delete Morning briefing" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "DeleteScheduledTask")?.request).toEqual({
        id: "t1",
      }),
    );
  });

  it("surfaces a failed action", async () => {
    const daemon = fakeDaemon({
      ...listing([task()]),
      PauseScheduledTask: new BorgError(GrpcStatus.NotFound, "no such task"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Pause")).toBeTruthy());

    click("Pause");
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("no such task"));
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
    const exprBox = screen.getByPlaceholderText(/0 0 8|30m|2026-08/);
    fireEvent.change(exprBox, { target: { value: expr } });
  };

  it("keeps the form closed until it is asked for", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Nothing scheduled yet.")).toBeTruthy());
    expect(screen.queryByPlaceholderText("Name")).toBeNull();
  });

  it("explains the expression field per schedule kind", async () => {
    // The daemon's cron parser is six-field, seconds first; a crontab-shaped
    // expression is rejected outright.
    const daemon = fakeDaemon(listing([]));
    await openForm(daemon);
    expect(screen.getByText(/second minute hour day month weekday/)).toBeTruthy();

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
