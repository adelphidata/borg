import type { ReactNode } from "react";

/**
 * Numbered setup steps — what to click where, in order.
 *
 * A real `<ol>`, so a screen reader says "list, 4 items" and which step it is
 * on. Long values inside a step (a URL with a key in it) wrap anywhere rather
 * than pushing the column wider than the page. Renders nothing for no steps.
 */
export function StepList({ steps, label }: { steps: readonly ReactNode[]; label?: string }) {
  if (steps.length === 0) return null;
  return (
    <ol aria-label={label} className="m-0 flex list-none flex-col gap-3 p-0">
      {steps.map((step, i) => (
        <li key={i} className="flex items-start gap-3">
          <span
            aria-hidden="true"
            className="flex h-[20px] w-[20px] flex-none items-center justify-center rounded-full border border-line text-[10.5px] leading-none font-medium text-ink2 tabular-nums"
          >
            {i + 1}
          </span>
          <span className="min-w-0 flex-1 pt-[2px] text-[12.5px] leading-[1.5] [overflow-wrap:anywhere] text-ink2 select-text">
            {step}
          </span>
        </li>
      ))}
    </ol>
  );
}
