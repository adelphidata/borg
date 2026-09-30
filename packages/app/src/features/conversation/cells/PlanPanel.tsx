import { useEffect, useRef, useState } from "react";
import { CheckIcon, ChevronDownIcon, ChevronIcon, StatusDot, Truncated } from "@borg/ui";
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
 * Scrolls `list` just far enough to show `item`, and not at all when it is
 * already in view. `item.offsetTop` is measured from the list, which is why
 * the list is positioned.
 */
export function revealInList(
  list: Pick<HTMLElement, "scrollTop" | "clientHeight">,
  item: Pick<HTMLElement, "offsetTop" | "offsetHeight">,
): void {
  const top = item.offsetTop;
  const bottom = top + item.offsetHeight;
  if (top < list.scrollTop) list.scrollTop = top;
  else if (bottom > list.scrollTop + list.clientHeight) list.scrollTop = bottom - list.clientHeight;
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
  // The list is capped, so on a long plan the step in hand can sit below the
  // fold. Moved by hand rather than with `scrollIntoView`, which would also
  // scroll every scrollable ancestor — the page under a reader included.
  const listRef = useRef<HTMLUListElement>(null);
  const activeRef = useRef<HTMLLIElement>(null);
  const activeId = steps.find((s) => isActive(s.status))?.id;
  useEffect(() => {
    const list = listRef.current;
    const item = activeRef.current;
    if (open && activeId !== undefined && list && item) revealInList(list, item);
  }, [activeId, open]);
  if (steps.length === 0) return null;

  const done = steps.filter((s) => isDone(s.status)).length;
  // Collapsed, the count alone says how far along the plan is but not what it is
  // doing, so learning that costs an expand. Naming the step folds the answer
  // into the header the user is already looking at.
  const running = steps.find((s) => isActive(s.status));

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
        {!open && running && (
          // `Truncated` has to be the flex item itself: its ellipsis needs a
          // block box, and wrapping it in a span leaves it inline, where
          // `overflow`/`text-overflow` do not apply and a long step overflows
          // the header instead of clipping.
          <Truncated className="min-w-0 flex-1 text-[11.5px] leading-none text-ink2">
            {running.text}
          </Truncated>
        )}
        <span
          className={`text-right text-[11.5px] leading-none text-ink3 ${
            !open && running ? "flex-none" : "flex-1"
          }`}
        >
          {done}/{steps.length}
        </span>
      </button>
      {open && (
        // Capped: a twenty-step plan would otherwise take the transcript's room.
        <ul
          ref={listRef}
          className="relative m-0 max-h-[30vh] list-none overflow-y-auto px-3.5 pt-0 pb-3"
        >
          {steps.map((step) => (
            <li
              key={step.id}
              ref={step.id === activeId ? activeRef : undefined}
              className="flex items-start gap-2 py-[3px] text-[12.5px] leading-[1.5]"
            >
              <span className="mt-[3px] flex h-3 w-3 flex-none items-center justify-center">
                {isDone(step.status) ? (
                  <CheckIcon size={12} />
                ) : (
                  <StatusDot tone={isActive(step.status) ? "busy" : "idle"} />
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
