import { useEffect } from "react";
import type { admin as adminPb } from "@borg/client";
import { plural } from "../../lib/format";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

/**
 * How often the list is re-read while it is on screen. Runs happen without
 * anyone touching the page, and a countdown that says "overdue" for a task
 * that ran a minute ago reads as a broken scheduler.
 */
export const SCHEDULES_REFRESH_MS = 30_000;

/** Schedule kinds the daemon understands (`crates/cortex/src/tasks.rs`). */
export const SCHEDULE_TYPES = ["cron", "interval", "once"] as const;
export type ScheduleType = (typeof SCHEDULE_TYPES)[number];

export interface NewSchedule {
  name: string;
  prompt: string;
  scheduleType: ScheduleType;
  scheduleExpr: string;
}

/**
 * Parses an interval expression into seconds, mirroring `parse_interval` in
 * `crates/cortex/src/tasks.rs`: a suffix of s/m/h/d, or a bare seconds count.
 * Returns null for anything the daemon would also reject.
 */
export function intervalSeconds(expr: string): number | null {
  const match = /^(\d+)([smhd]?)$/.exec(expr.trim());
  if (!match) return null;
  const value = Number(match[1]);
  if (!Number.isFinite(value) || value <= 0) return null;
  const unit = match[2];
  const scale = unit === "m" ? 60 : unit === "h" ? 3600 : unit === "d" ? 86_400 : 1;
  return value * scale;
}

/** Renders a schedule the way a person would say it. */
export function describeSchedule(type: string, expr: string): string {
  if (type === "interval") {
    const seconds = intervalSeconds(expr);
    if (seconds === null) return expr;
    if (seconds % 86_400 === 0) return `every ${plural(seconds / 86_400, "day")}`;
    if (seconds % 3600 === 0) return `every ${plural(seconds / 3600, "hour")}`;
    if (seconds % 60 === 0) return `every ${plural(seconds / 60, "minute")}`;
    return `every ${seconds}s`;
  }
  if (type === "once") return expr === "" ? "once" : `once — ${expr}`;
  return expr;
}

/** "in 4 min", "in 2 h", or "overdue" — a clock time alone is hard to read. */
export function relativeTime(unix: number, now: number): string {
  if (unix <= 0) return "—";
  const deltaSeconds = Math.round(unix - now / 1000);
  if (deltaSeconds < 0) return "overdue";
  if (deltaSeconds < 90) return `in ${deltaSeconds}s`;
  const minutes = Math.round(deltaSeconds / 60);
  if (minutes < 90) return `in ${minutes} min`;
  const hours = Math.round(minutes / 60);
  if (hours < 36) return `in ${hours} h`;
  return `in ${Math.round(hours / 24)} d`;
}

export interface Schedules {
  query: QueryResult<adminPb.ScheduledTask[]>;
  create: (task: NewSchedule) => Promise<unknown>;
  remove: (id: string) => Promise<unknown>;
  pause: (id: string) => Promise<unknown>;
  resume: (id: string) => Promise<unknown>;
  runNow: (id: string) => Promise<unknown>;
  error: string | undefined;
}

/**
 * Scheduled tasks.
 *
 * The daemon has carried nine RPCs for these since before either client
 * existed; until now the only way to reach them was the CLI.
 */
export function useSchedules(deps: unknown[] = []): Schedules {
  const query = useQuery(async (borg) => {
    const list = await borg.admin.listScheduledTasks({});
    return list.tasks;
  }, deps);

  const { refetch } = query;
  useEffect(() => {
    const id = setInterval(refetch, SCHEDULES_REFRESH_MS);
    return () => clearInterval(id);
  }, [refetch]);

  const refresh = { onSuccess: () => query.refetch() };

  const createMutation = useMutation(
    (borg, task: NewSchedule) =>
      borg.admin.createScheduledTask({
        name: task.name,
        prompt: task.prompt,
        scheduleType: task.scheduleType,
        scheduleExpr: task.scheduleExpr,
        // Local zone, so "every day at 08:00" means the user's morning.
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
        taskType: "prompt",
        deliveryChannel: "",
        deliveryTarget: "",
        nextRunUnix: 0,
        // Sentinels the proto documents as "leave the default alone".
        maxRetries: -1,
        timeoutMs: 0,
        allowedTools: "",
        preRunCommand: "",
        preRunCwd: "",
        preRunTimeoutMs: 0,
      }),
    refresh,
  );
  const deleteMutation = useMutation(
    (borg, id: string) => borg.admin.deleteScheduledTask({ id }),
    refresh,
  );
  const pauseMutation = useMutation(
    (borg, id: string) => borg.admin.pauseScheduledTask({ id }),
    refresh,
  );
  const resumeMutation = useMutation(
    (borg, id: string) => borg.admin.resumeScheduledTask({ id }),
    refresh,
  );
  const runNowMutation = useMutation(
    (borg, id: string) => borg.admin.runScheduledTaskNow({ id }),
    refresh,
  );

  return {
    query,
    create: (task) => createMutation.run(task),
    remove: (id) => deleteMutation.run(id),
    pause: (id) => pauseMutation.run(id),
    resume: (id) => resumeMutation.run(id),
    runNow: (id) => runNowMutation.run(id),
    error: (
      createMutation.error ??
      deleteMutation.error ??
      pauseMutation.error ??
      resumeMutation.error ??
      runNowMutation.error
    )?.message,
  };
}
