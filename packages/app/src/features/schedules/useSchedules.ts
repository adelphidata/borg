import type { admin as adminPb } from "@borg/client";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

/** Schedule kinds the daemon understands (`crates/cortex/src/tasks.rs`). */
export const SCHEDULE_TYPES = ["cron", "interval", "once"] as const;
export type ScheduleType = (typeof SCHEDULE_TYPES)[number];

export interface NewSchedule {
  name: string;
  prompt: string;
  scheduleType: ScheduleType;
  scheduleExpr: string;
}

/** Renders a schedule the way a person would say it. */
export function describeSchedule(type: string, expr: string): string {
  if (type === "interval") {
    const seconds = Number(expr);
    if (!Number.isFinite(seconds) || seconds <= 0) return expr;
    if (seconds % 3600 === 0) {
      const hours = seconds / 3600;
      return `every ${hours} hour${hours === 1 ? "" : "s"}`;
    }
    if (seconds % 60 === 0) {
      const minutes = seconds / 60;
      return `every ${minutes} minute${minutes === 1 ? "" : "s"}`;
    }
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
