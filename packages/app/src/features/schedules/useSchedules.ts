import { useEffect } from "react";
import type { admin as adminPb, BorgClient } from "@borg/client";
import { plural } from "../../lib/format";
import { offerOpen, showNotice } from "../../lib/notice";
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

/**
 * A stored cron expression as crontab fields. The daemon stores 6 (seconds
 * first) or 7 (plus year) fields; both are dropped when they are the
 * defaults, so the rest reads as the five-field line a person typed.
 */
export function crontabFields(expr: string): string[] {
  let fields = expr.trim().split(/\s+/);
  if (fields.length === 7 && fields[6] === "*") fields = fields.slice(0, 6);
  if (fields.length === 6 && fields[0] === "0") fields = fields.slice(1);
  return fields;
}

/** The schedule as the form shows it for editing. */
export function editableExpr(type: string, expr: string): string {
  if (type !== "cron") return expr;
  const fields = crontabFields(expr);
  return fields.length === 5 ? fields.join(" ") : expr;
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
  return describeCron(expr);
}

const DAY_NAMES = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const DAY_ABBR = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/** Crontab day-of-week (0/7 = Sunday, or a name) to an index, else null. */
function weekdayIndex(field: string): number | null {
  if (/^[0-7]$/.test(field)) return Number(field) % 7;
  const i = DAY_ABBR.indexOf(field.toUpperCase().slice(0, 3));
  return i >= 0 && /^[a-z]+$/i.test(field) ? i : null;
}

/**
 * The common cron shapes in words ("weekdays at 09:00", "every 5 minutes").
 * Anything else is shown as its five crontab fields — a sentence that guessed
 * would be worse than the expression. Weekdays read the way the daemon now
 * parses them: crontab numbering, 1-5 is Monday to Friday.
 */
export function describeCron(expr: string): string {
  const fields = crontabFields(expr);
  if (fields.length !== 5) return expr;
  const [min, hour, dom, mon, dow] = fields as [string, string, string, string, string];
  const crontab = fields.join(" ");
  const isNum = (s: string) => /^\d+$/.test(s);

  if (dom !== "*" || mon !== "*") {
    if (isNum(min) && isNum(hour) && isNum(dom) && mon === "*" && dow === "*") {
      return `monthly on day ${dom} at ${clock(hour, min)}`;
    }
    return crontab;
  }
  if (hour === "*" && dow === "*") {
    if (min === "*") return "every minute";
    const step = /^\*\/(\d+)$/.exec(min);
    if (step) return `every ${plural(Number(step[1]), "minute")}`;
    if (isNum(min)) return `hourly at :${min.padStart(2, "0")}`;
    return crontab;
  }
  if (!isNum(min) || !isNum(hour)) return crontab;
  const at = clock(hour, min);
  if (dow === "*") return `daily at ${at}`;
  if (dow === "1-5" || dow.toUpperCase() === "MON-FRI") return `weekdays at ${at}`;
  if (dow === "0,6" || dow === "6,0" || dow.toUpperCase() === "SAT,SUN") return `weekends at ${at}`;
  const day = weekdayIndex(dow);
  return day === null ? crontab : `every ${DAY_NAMES[day]} at ${at}`;
}

function clock(hour: string, min: string): string {
  return `${hour.padStart(2, "0")}:${min.padStart(2, "0")}`;
}

/**
 * How far past its slot a task can be and still be on schedule: the daemon
 * checks for due tasks once a minute, so a task can sit up to a minute past
 * its time before it starts.
 */
const DUE_GRACE_SECONDS = 90;

/** "in 4 min", "in 2 h", "due now" or "overdue" — a clock time alone is hard to read. */
export function relativeTime(unix: number, now: number): string {
  if (unix <= 0) return "—";
  const deltaSeconds = Math.round(unix - now / 1000);
  if (deltaSeconds < -DUE_GRACE_SECONDS) return "overdue";
  if (deltaSeconds <= 0) return "due now";
  if (deltaSeconds < 90) return `in ${deltaSeconds}s`;
  const minutes = Math.round(deltaSeconds / 60);
  if (minutes < 90) return `in ${minutes} min`;
  const hours = Math.round(minutes / 60);
  if (hours < 36) return `in ${hours} h`;
  return `in ${Math.round(hours / 24)} d`;
}

/** How often a run with no conversation to watch is checked on. */
export const RUN_POLL_MS = 2_000;
/** How long it is checked on before the page stops waiting to report it. */
export const RUN_WATCH_MS = 15 * 60_000;

/** What a finished run says, or null for a row that is not finished yet. */
export function runOutcome(name: string, run: adminPb.ScheduledTaskRun): string | null {
  switch (run.status) {
    case "running":
      return null;
    case "success":
      return `${name} finished`;
    case "delivery_failed":
      return `${name} finished, but its result could not be delivered`;
    default:
      return `${name} failed${run.error ? `: ${run.error}` : ""}`;
  }
}

/**
 * Reports how a manual run of a task with no conversation ends. Such a task
 * (a maintenance job, a command) has nothing to open, so without this "Run
 * now" answers with "Running…" and then nothing at all.
 *
 * Keyed on the newest run id from before the press, not on a clock: the run
 * the press started is the first one after it. Module-level rather than in a
 * component, because the toast outlives the page that was on screen.
 */
export async function reportWhenDone(
  borg: BorgClient,
  id: string,
  name: string,
  afterRunId: number,
  sleep: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
): Promise<void> {
  for (let waited = 0; waited < RUN_WATCH_MS; waited += RUN_POLL_MS) {
    await sleep(RUN_POLL_MS);
    let runs: adminPb.ScheduledTaskRun[];
    try {
      runs = (await borg.admin.getScheduledTaskRuns({ id, limit: 1 })).runs;
    } catch {
      // The daemon restarting mid-run is no news worth a toast; try again.
      continue;
    }
    const run = runs[0];
    if (!run || run.id <= afterRunId) continue;
    const outcome = runOutcome(name, run);
    if (outcome !== null) {
      showNotice(outcome);
      return;
    }
  }
}

export interface Schedules {
  query: QueryResult<adminPb.ScheduledTask[]>;
  create: (task: NewSchedule) => Promise<unknown>;
  /**
   * Rewrites a task's name and prompt, and its schedule when
   * `scheduleChanged`; its zone stays as stored. A schedule that is sent
   * moves the next run, so an unchanged one is left out.
   */
  update: (id: string, task: NewSchedule, scheduleChanged: boolean) => Promise<unknown>;
  remove: (id: string) => Promise<unknown>;
  pause: (id: string) => Promise<unknown>;
  resume: (id: string) => Promise<unknown>;
  runNow: (id: string) => Promise<unknown>;
  /** Why the last create or edit failed, shown beside the form that sent it. */
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

  // Every action here reports through the shell's toast: a row that pauses or
  // runs changes little on screen, and a failure far from the button is one
  // nobody reads. Named by the task, so two in a row are told apart.
  const tasks = query.state === "ready" ? query.data : [];
  const nameOf = (id: string) => `“${tasks.find((t) => t.id === id)?.name || "task"}”`;
  const report = (done: (id: string) => string, failed: (id: string) => string) => ({
    onSuccess: (_: unknown, id: string) => {
      query.refetch();
      showNotice(done(id));
    },
    onError: (e: { message: string }, id: string) => showNotice(`${failed(id)}: ${e.message}`),
  });

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
    {
      onSuccess: (_: unknown, task: NewSchedule) => {
        query.refetch();
        showNotice(`Scheduled “${task.name}”`);
      },
    },
  );
  const updateMutation = useMutation(
    (borg, vars: { id: string; task: NewSchedule; scheduleChanged: boolean }) =>
      borg.admin.updateScheduledTask({
        id: vars.id,
        name: vars.task.name,
        prompt: vars.task.prompt,
        // Empty leaves the stored value alone.
        scheduleType: vars.scheduleChanged ? vars.task.scheduleType : "",
        scheduleExpr: vars.scheduleChanged ? vars.task.scheduleExpr : "",
        timezone: "",
      }),
    {
      onSuccess: (_: unknown, vars: { task: NewSchedule }) => {
        query.refetch();
        showNotice(`Saved “${vars.task.name}”`);
      },
    },
  );
  const deleteMutation = useMutation(
    (borg, id: string) => borg.admin.deleteScheduledTask({ id }),
    report(
      (id) => `Deleted ${nameOf(id)}`,
      (id) => `Could not delete ${nameOf(id)}`,
    ),
  );
  const pauseMutation = useMutation(
    (borg, id: string) => borg.admin.pauseScheduledTask({ id }),
    report(
      (id) => `Paused ${nameOf(id)}`,
      (id) => `Could not pause ${nameOf(id)}`,
    ),
  );
  const resumeMutation = useMutation(
    (borg, id: string) => borg.admin.resumeScheduledTask({ id }),
    report(
      (id) => `Resumed ${nameOf(id)}`,
      (id) => `Could not resume ${nameOf(id)}`,
    ),
  );
  const runNowMutation = useMutation(
    async (borg, id: string) => {
      const watch = !tasks.find((t) => t.id === id)?.sessionId;
      const afterRunId = watch
        ? ((await borg.admin.getScheduledTaskRuns({ id, limit: 1 })).runs[0]?.id ?? 0)
        : 0;
      await borg.admin.runScheduledTaskNow({ id });
      if (watch) void reportWhenDone(borg, id, nameOf(id), afterRunId);
    },
    {
      onSuccess: (_: unknown, id: string) => {
        query.refetch();
        const session = tasks.find((t) => t.id === id)?.sessionId;
        const message = `Running ${nameOf(id)}`;
        // The run happens in the task's own conversation, so the toast offers
        // the way to watch it — and to reply once it is done.
        if (session) offerOpen(message, `/c/${encodeURIComponent(session)}`);
        else showNotice(message);
      },
      onError: (e: { message: string }, id: string) =>
        showNotice(`Could not run ${nameOf(id)}: ${e.message}`),
    },
  );

  return {
    query,
    create: (task) => createMutation.run(task),
    update: (id, task, scheduleChanged) => updateMutation.run({ id, task, scheduleChanged }),
    remove: (id) => deleteMutation.run(id),
    pause: (id) => pauseMutation.run(id),
    resume: (id) => resumeMutation.run(id),
    runNow: (id) => runNowMutation.run(id),
    // Only the forms' own failures stay on the page, next to the form; a row
    // action reports through the toast.
    error: (createMutation.error ?? updateMutation.error)?.message,
  };
}
