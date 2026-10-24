import { useState } from "react";
import { useLocation } from "react-router-dom";
import {
  PageChrome,
  Pill,
  PlusIcon,
  StatusDot,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  TrashIcon,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import type { admin as adminPb } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { formatClock } from "../../lib/format";
import { useQuery } from "../../lib/useDaemon";
import {
  SCHEDULE_TYPES,
  describeSchedule,
  editableExpr,
  relativeTime,
  useSchedules,
  type ScheduleType,
} from "./useSchedules";

/** Placeholder and helper text per schedule kind, so the field explains itself. */
const EXPR_HELP: Record<ScheduleType, { placeholder: string; hint: string }> = {
  // Crontab's five fields — the daemon widens them and reads weekdays the
  // crontab way (0 or 7 is Sunday, 1-5 is Monday to Friday).
  cron: {
    placeholder: "0 8 * * 1-5",
    hint: "minute hour day month weekday — this example is 08:00 on weekdays",
  },
  interval: { placeholder: "30m", hint: "How long between runs, e.g. 90s, 30m, 6h" },
  once: { placeholder: "2026-08-10T09:00", hint: "When to run, once" },
};

function NewSchedule({
  onCreate,
}: {
  /** Resolves true when the task was stored; false leaves the form as it is. */
  onCreate: (task: NewScheduleDraft) => Promise<boolean>;
}) {
  const [open, setOpen] = useState(false);
  // Remounting the form on each open is what empties it after a create.
  const [opens, setOpens] = useState(0);

  if (!open) {
    return (
      <button
        type="button"
        onClick={() => {
          setOpens((n) => n + 1);
          setOpen(true);
        }}
        className="mb-5 flex w-full cursor-pointer items-center justify-center gap-2 rounded-[10px] border border-line border-dashed bg-transparent py-3 text-[12.5px] leading-none text-ink3 hover:border-neon hover:text-ink2"
      >
        <PlusIcon size={14} /> New scheduled task
      </button>
    );
  }

  return (
    <ScheduleForm
      key={opens}
      className="mb-5"
      submitLabel="Create"
      savingLabel="Creating…"
      onCancel={() => setOpen(false)}
      onSubmit={(task) =>
        onCreate(task).then((stored) => {
          if (stored) setOpen(false);
          return stored;
        })
      }
    />
  );
}

type NewScheduleDraft = ReturnType<typeof draftOf>;

/** The name, prompt and schedule of a task, for a new one or an edit. */
function ScheduleForm({
  initial,
  className = "",
  submitLabel,
  savingLabel,
  onSubmit,
  onCancel,
}: {
  initial?: NewScheduleDraft;
  className?: string;
  submitLabel: string;
  savingLabel: string;
  /** Resolves true when the task was stored; false leaves the form as it is. */
  onSubmit: (task: NewScheduleDraft) => Promise<boolean>;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [prompt, setPrompt] = useState(initial?.prompt ?? "");
  const [scheduleType, setScheduleType] = useState<ScheduleType>(initial?.scheduleType ?? "cron");
  const [scheduleExpr, setScheduleExpr] = useState(initial?.scheduleExpr ?? "");
  const [saving, setSaving] = useState(false);

  const ready = name.trim() !== "" && prompt.trim() !== "" && scheduleExpr.trim() !== "";

  const field =
    "w-full rounded-[9px] border border-field bg-surface2 px-3 py-2 text-[13px] text-ink outline-none select-text placeholder:text-ink3 focus:border-focus";

  return (
    <div
      className={`flex flex-col gap-3 rounded-[10px] border border-line bg-surface2 p-4 ${className}`}
    >
      <input
        autoFocus
        className={field}
        placeholder="Name"
        aria-label="Name"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <textarea
        rows={3}
        className={`${field} resize-none`}
        placeholder="What should Borg do?"
        aria-label="What should Borg do?"
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
      />
      <div className="flex flex-wrap gap-2">
        {SCHEDULE_TYPES.map((type) => (
          <Pill
            key={type}
            square
            variant={type === scheduleType ? "primary" : undefined}
            aria-pressed={type === scheduleType}
            onClick={() => setScheduleType(type)}
          >
            {type}
          </Pill>
        ))}
      </div>
      <div>
        <input
          className={field}
          placeholder={EXPR_HELP[scheduleType].placeholder}
          aria-label="Schedule"
          value={scheduleExpr}
          onChange={(e) => setScheduleExpr(e.target.value)}
        />
        <div className="mt-1.5 px-1 text-[11px] leading-none text-ink3">
          {EXPR_HELP[scheduleType].hint}
        </div>
      </div>
      <div className="flex justify-end gap-2">
        <Pill square onClick={onCancel}>
          Cancel
        </Pill>
        <Pill
          square
          variant="primary"
          disabled={!ready || saving}
          onClick={() => {
            setSaving(true);
            // A rejected schedule keeps the form — and everything typed into
            // it — so the expression can be corrected in place.
            void onSubmit(draftOf(name, prompt, scheduleType, scheduleExpr)).finally(() =>
              setSaving(false),
            );
          }}
        >
          {saving ? savingLabel : submitLabel}
        </Pill>
      </div>
    </div>
  );
}

function asScheduleType(type: string): ScheduleType {
  return (SCHEDULE_TYPES as readonly string[]).includes(type) ? (type as ScheduleType) : "cron";
}

function draftOf(name: string, prompt: string, scheduleType: ScheduleType, scheduleExpr: string) {
  return {
    name: name.trim(),
    prompt: prompt.trim(),
    scheduleType,
    scheduleExpr: scheduleExpr.trim(),
  };
}

/** Run outcomes in words; anything else shows as the daemon stored it. */
const RUN_STATUS: Record<string, string> = {
  success: "Finished",
  failed: "Failed",
  error: "Failed",
  timeout: "Timed out",
  cancelled: "Cancelled",
  misconfigured: "Could not be delivered",
  quiet_hours: "Skipped for quiet hours",
};

/**
 * Results the daemon stores for a run that sent nothing
 * (`HEARTBEAT_SKIP_MARKERS` in `crates/borgd/src/task_runner.rs`). The code
 * reads them back, so they stay codes on disk; people get the sentence.
 */
const SKIP_RESULT: Record<string, string> = {
  quiet_hours: "Skipped — it fell in quiet hours.",
  min_interval: "Skipped — it ran a short time ago.",
  silent: "Nothing to report.",
  zero_info: "Nothing to report.",
  dedup_suppressed: "Same as last time, so it was not sent again.",
  interactive_input_unavailable:
    "Skipped — it needed an answer from you, and a scheduled run cannot ask.",
};

/**
 * What the task did last time.
 *
 * A task with no chat channel to deliver to keeps its results here, so this
 * is where "what did it say?" gets answered. Re-read whenever the list is,
 * so a run that lands while it is open shows up.
 */
function LastRun({ task }: { task: adminPb.ScheduledTask }) {
  const detail = useQuery((borg) => borg.admin.getScheduledTaskDetail({ id: task.id }), [task]);
  return (
    <div className="mt-2.5">
      <DaemonState query={detail} label="the last run">
        {(d) => {
          const run = d.hasLastRun ? d.lastRun : undefined;
          if (!run) {
            return <div className="text-[12px] leading-[1.45] text-ink3">Not run yet.</div>;
          }
          const text = run.error || (SKIP_RESULT[run.result] ?? run.result);
          return (
            <div className="flex flex-col gap-1.5">
              <div className="text-[11.5px] leading-[1.4] text-ink3">
                Last run {formatClock(Number(run.startedAtUnix) * 1000)} ·{" "}
                {RUN_STATUS[run.status] ?? run.status}
              </div>
              {text && (
                <div
                  className={`max-h-[240px] overflow-y-auto rounded-[8px] bg-surface2 px-3 py-2 text-[12.5px] leading-[1.5] whitespace-pre-wrap select-text ${
                    run.error ? "text-alert" : "text-ink2"
                  }`}
                >
                  {text}
                </div>
              )}
            </div>
          );
        }}
      </DaemonState>
    </div>
  );
}

/** "next in 4 min", but "due now" / "overdue" — "next due now" reads as a typo. */
function nextRunLabel(unix: number): string {
  const when = relativeTime(unix, Date.now());
  return when.startsWith("in ") ? `next ${when}` : when;
}

function TaskRow({
  task,
  expanded,
  onToggle,
  onPause,
  onResume,
  onRunNow,
  onEdit,
  onDelete,
}: {
  task: adminPb.ScheduledTask;
  expanded: boolean;
  onToggle: () => void;
  onPause: () => void;
  onResume: () => void;
  onRunNow: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const paused = task.status === "paused";
  const finished = task.status === "completed" || task.status === "cancelled";

  return (
    <div className="group flex items-start gap-3 rounded-[10px] border border-line2 px-4 py-3">
      <StatusDot tone={finished ? "idle" : paused ? "paused" : "ok"} className="mt-[6px]" />
      <div className="min-w-0 flex-1">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={onToggle}
          className="block w-full min-w-0 cursor-pointer border-0 bg-transparent p-0 text-left"
        >
          <div className="truncate text-[13.5px] leading-[1.35] text-ink">{task.name}</div>
          <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
            {describeSchedule(task.scheduleType, task.scheduleExpr)}
            {!paused && !finished && <> · {nextRunLabel(Number(task.nextRunUnix))}</>}
            {paused && <> · paused</>}
            {finished && <> · {task.status}</>}
          </div>
        </button>
        {expanded && <LastRun task={task} />}
      </div>
      <div className="flex flex-none items-center gap-1.5 hover-reveal">
        <Pill square onClick={onRunNow}>
          Run now
        </Pill>
        {!finished && (
          <Pill square onClick={paused ? onResume : onPause}>
            {paused ? "Resume" : "Pause"}
          </Pill>
        )}
        {/* Borg's own loops (heartbeat, maintenance…) can be paused, not
            edited or deleted: a deleted one stays gone until the next
            migration, and their prompts are what the loop runs on. */}
        {!task.system && (
          <Pill square onClick={onEdit}>
            Edit
          </Pill>
        )}
        {!task.system && (
          <button
            type="button"
            aria-label={`Delete ${task.name}`}
            onClick={onDelete}
            className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
          >
            <TrashIcon size={14} />
          </button>
        )}
      </div>
    </div>
  );
}

/**
 * Scheduled tasks — the things Borg does without being asked.
 *
 * Deliberately the short version of what the daemon supports: name, prompt,
 * and when. Retries, timeouts, tool allowlists, delivery channels and shell
 * preambles all exist on the RPC and all stay on the CLI, because a form with
 * twelve fields is how a scheduler stops getting used.
 */
export function SchedulesPage() {
  const isMobile = useIsMobile();
  const location = useLocation();
  const schedules = useSchedules([location.key]);
  const [open, setOpen] = useState<string>();
  const [editing, setEditing] = useState<string>();
  const [confirm, setConfirm] = useState<string>();

  return (
    <PageChrome title="Scheduled" mobileBrandHeader>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <NewSchedule onCreate={(task) => schedules.create(task).then((r) => r !== undefined)} />

        {schedules.error && (
          <div role="alert" className="mb-3 text-[11.5px] leading-[1.4] text-alert">
            {schedules.error}
          </div>
        )}

        <DaemonState query={schedules.query} label="your scheduled tasks">
          {(tasks) => (
            <div className="flex flex-col gap-2">
              {tasks.length === 0 && (
                <div className="pt-6 text-center text-[13.5px] leading-[1.6] text-ink3">
                  Nothing scheduled yet.
                </div>
              )}
              {tasks.map((task) =>
                editing === task.id ? (
                  <ScheduleForm
                    key={task.id}
                    initial={{
                      name: task.name,
                      prompt: task.prompt,
                      scheduleType: asScheduleType(task.scheduleType),
                      scheduleExpr: editableExpr(task.scheduleType, task.scheduleExpr),
                    }}
                    submitLabel="Save"
                    savingLabel="Saving…"
                    onCancel={() => setEditing(undefined)}
                    onSubmit={(draft) =>
                      schedules
                        .update(
                          task.id,
                          draft,
                          draft.scheduleType !== asScheduleType(task.scheduleType) ||
                            draft.scheduleExpr !==
                              editableExpr(task.scheduleType, task.scheduleExpr),
                        )
                        .then((r) => {
                          if (r === undefined) return false;
                          setEditing(undefined);
                          return true;
                        })
                    }
                  />
                ) : confirm === task.id ? (
                  <div
                    key={task.id}
                    className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3"
                  >
                    <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
                      Delete {task.name}?
                    </div>
                    <Pill square onClick={() => setConfirm(undefined)}>
                      Keep
                    </Pill>
                    <Pill
                      square
                      variant="danger"
                      onClick={() => {
                        setConfirm(undefined);
                        void schedules.remove(task.id);
                      }}
                    >
                      Delete
                    </Pill>
                  </div>
                ) : (
                  <TaskRow
                    key={task.id}
                    task={task}
                    expanded={open === task.id}
                    onToggle={() => setOpen((id) => (id === task.id ? undefined : task.id))}
                    onPause={() => void schedules.pause(task.id)}
                    onResume={() => void schedules.resume(task.id)}
                    onRunNow={() => void schedules.runNow(task.id)}
                    onEdit={() => setEditing(task.id)}
                    onDelete={() => setConfirm(task.id)}
                  />
                ),
              )}
            </div>
          )}
        </DaemonState>
      </div>
    </PageChrome>
  );
}
