import { useEffect, useRef, useState } from "react";
import { ChevronDownIcon, ChevronIcon } from "@borg/ui";
import type { Cell } from "../useConversation";

/**
 * Reasoning, folded away.
 *
 * Open while it streams so the user can watch, closed once the answer arrives
 * so the transcript stays readable — but a manual toggle wins from then on.
 * Without that rule the auto-collapse would fight anyone who opened it to read.
 */
export function ThinkingCell({ cell }: { cell: Cell & { kind: "thinking" } }) {
  const [open, setOpen] = useState(cell.streaming);
  const touched = useRef(false);

  useEffect(() => {
    if (!touched.current) setOpen(cell.streaming);
  }, [cell.streaming]);

  const label = cell.streaming
    ? "Thinking…"
    : cell.seconds !== undefined && cell.seconds >= 1
      ? `Thought for ${cell.seconds}s`
      : "Thought briefly";

  return (
    <div className="px-1">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => {
          touched.current = true;
          setOpen((v) => !v);
        }}
        className="flex cursor-pointer items-center gap-1.5 border-0 bg-transparent p-0 text-[11.5px] leading-none text-ink3 hover:text-ink2"
      >
        {open ? <ChevronDownIcon size={12} /> : <ChevronIcon size={12} />}
        <span className={cell.streaming ? "animate-bpulse" : undefined}>{label}</span>
      </button>
      {open && (
        <div className="mt-2 border-l border-line2 pl-3 text-[12.5px] leading-[1.55] whitespace-pre-wrap text-ink3 italic select-text">
          {cell.text}
        </div>
      )}
    </div>
  );
}
