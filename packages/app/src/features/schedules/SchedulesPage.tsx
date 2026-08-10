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
import {
  SCHEDULE_TYPES,
  describeSchedule,
  relativeTime,
  useSchedules,
  type ScheduleType,
} from "./useSchedules";

/** Placeholder and helper text per schedule kind, so the field explains itself. */
const EXPR_HELP: Record<ScheduleType, { placeholder: string; hint: string }> = {
  // Six fields, seconds first — the daemon's cron parser is not the five-field
  // crontab dialect, and a five-field expression is rejected outright.
  cron: {
    placeholder: "0 0 8 * * *",
    hint: "second minute hour day month weekday — this example is 08:00 daily",
  },
  interval: { placeholder: "30m", hint: "How long between runs, e.g. 90s, 30m, 6h" },
  once: { placeholder: "2026-08-10T09:00", hint: "When to run, once" },
};

function NewSchedule({
  onCreate,
}: {
  /** Resolves true when the task was stored; false leaves the form as it is. */
  onCreate: (task: ReturnType<typeof draftOf>) => Promise<boolean>;
}) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [prompt, setPrompt] = useState("");
  const [scheduleType, setScheduleType] = useState<ScheduleType>("cron");
  const [scheduleExpr, setScheduleExpr] = useState("");
  const [saving, setSaving] = useState(false);

  const ready = name.trim() !== "" && prompt.trim() !== "" && scheduleExpr.trim() !== "";

  if (!open) {
    return (
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="mb-5 flex w-full cursor-pointer items-center justify-center gap-2 rounded-[10px] border border-line border-dashed bg-transparent py-3 text-[12.5px] leading-none text-ink3 hover:border-neon hover:text-ink2"
      >
        <PlusIcon size={14} /> New scheduled task
      </button>
    );
  }

  const field =
    "w-full rounded-[9px] border border-line bg-surface2 px-3 py-2 text-[13px] text-ink outline-none select-text placeholder:text-ink3";

  return (
    <div className="mb-5 flex flex-col gap-3 rounded-[10px] border border-line bg-surface2 p-4">
      <input
        autoFocus
        className={field}
        placeholder="Name"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <textarea
        rows={3}
        className={`${field} resize-none`}
        placeholder="What should Borg do?"
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
      />
      <div className="flex flex-wrap gap-2">
        {SCHEDULE_TYPES.map((type) => (
          <Pill
            key={type}
            square
            variant={type === scheduleType ? "primary" : undefined}
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
          value={scheduleExpr}
          onChange={(e) => setScheduleExpr(e.target.value)}
        />
        <div className="mt-1.5 px-1 text-[11px] leading-none text-ink3">
          {EXPR_HELP[scheduleType].hint}
        </div>
      </div>
      <div className="flex justify-end gap-2">
        <Pill square onClick={() => setOpen(false)}>
          Cancel
        </Pill>
        <Pill
          square
          variant="primary"
          disabled={!ready || saving}
          onClick={() => {
            setSaving(true);
            void onCreate(draftOf(name, prompt, scheduleType, scheduleExpr))
              .then((stored) => {
                // A rejected schedule keeps the form — and everything typed
                // into it — so the expression can be corrected in place.
                if (!stored) return;
                setOpen(false);
                setName("");
                setPrompt("");
                setScheduleExpr("");
              })
              .finally(() => setSaving(false));
          }}
        >
          {saving ? "Creating…" : "Create"}
        </Pill>
      </div>
    </div>
  );
}

function draftOf(name: string, prompt: string, scheduleType: ScheduleType, scheduleExpr: string) {
  return {
    name: name.trim(),
    prompt: prompt.trim(),
    scheduleType,
    scheduleExpr: scheduleExpr.trim(),
  };
}

function TaskRow({
  task,
  onPause,
  onResume,
  onRunNow,
  onDelete,
}: {
  task: adminPb.ScheduledTask;
  onPause: () => void;
  onResume: () => void;
  onRunNow: () => void;
  onDelete: () => void;
}) {
  const paused = task.status === "paused";
  const finished = task.status === "completed" || task.status === "cancelled";

  return (
    <div className="group flex items-start gap-3 rounded-[10px] border border-line2 px-4 py-3">
      <StatusDot tone={finished ? "idle" : paused ? "paused" : "ok"} className="mt-[6px]" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13.5px] leading-[1.35] text-ink">{task.name}</div>
        <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
          {describeSchedule(task.scheduleType, task.scheduleExpr)}
          {!paused && !finished && (
            <> · next {relativeTime(Number(task.nextRunUnix), Date.now())}</>
          )}
          {paused && <> · paused</>}
          {finished && <> · {task.status}</>}
        </div>
      </div>
      <div className="flex flex-none items-center gap-1.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        <Pill square onClick={onRunNow}>
          Run now
        </Pill>
        {!finished && (
          <Pill square onClick={paused ? onResume : onPause}>
            {paused ? "Resume" : "Pause"}
          </Pill>
        )}
        <button
          type="button"
          aria-label={`Delete ${task.name}`}
          onClick={onDelete}
          className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
        >
          <TrashIcon size={14} />
        </button>
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
              {tasks.map((task) => (
                <TaskRow
                  key={task.id}
                  task={task}
                  onPause={() => void schedules.pause(task.id)}
                  onResume={() => void schedules.resume(task.id)}
                  onRunNow={() => void schedules.runNow(task.id)}
                  onDelete={() => void schedules.remove(task.id)}
                />
              ))}
            </div>
          )}
        </DaemonState>
      </div>
    </PageChrome>
  );
}
