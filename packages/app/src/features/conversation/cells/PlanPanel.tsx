import { useState } from "react";
import { CheckIcon, ChevronDownIcon, ChevronIcon } from "@borg/ui";
import type { PlanStep } from "../useConversation";

/** Status strings the agent publishes, normalised for display. */
function isDone(status: string): boolean {
  const s = status.toLowerCase();
  return s === "done" || s === "completed" || s === "complete";
}

function isActive(status: string): boolean {
  const s = status.toLowerCase();
  return s === "in_progress" || s === "active" || s === "running";
}

/**
 * The agent's todo list, pinned above the composer.
 *
 * Hoisted out of the transcript on purpose: a plan is current state, not
 * history, and re-reading five stale copies of it while scrolling is noise.
 * `PlanUpdated` events used to be dropped entirely.
 */
export function PlanPanel({ steps }: { steps: PlanStep[] }) {
  const [open, setOpen] = useState(true);
  if (steps.length === 0) return null;

  const done = steps.filter((s) => isDone(s.status)).length;

  return (
    <div className="rounded-[10px] border border-line2 bg-surface2">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-3.5 py-2.5 text-left"
      >
        {open ? <ChevronDownIcon size={12} /> : <ChevronIcon size={12} />}
        <span className="text-[11px] leading-none font-medium tracking-[0.12em] text-ink3 uppercase">
          Plan
        </span>
        <span className="flex-1 text-right text-[11.5px] leading-none text-ink3">
          {done}/{steps.length}
        </span>
      </button>
      {open && (
        <ul className="m-0 list-none px-3.5 pt-0 pb-3">
          {steps.map((step) => (
            <li
              key={step.id}
              className="flex items-start gap-2 py-[3px] text-[12.5px] leading-[1.5]"
            >
              <span className="mt-[3px] flex h-3 w-3 flex-none items-center justify-center">
                {isDone(step.status) ? (
                  <CheckIcon size={12} />
                ) : (
                  <span
                    className={`h-1.5 w-1.5 rounded-full ${
                      isActive(step.status) ? "animate-bpulse bg-neon" : "bg-ink3"
                    }`}
                  />
                )}
              </span>
              <span className={isDone(step.status) ? "text-ink3 line-through" : "text-ink2"}>
                {step.text}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
