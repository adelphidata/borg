import { useState } from "react";
import { ChevronDownIcon, ChevronIcon } from "@borg/ui";
import type { Cell } from "../useConversation";

/**
 * Reasoning, folded away — closed until asked, streaming or not.
 *
 * The open flag survives streaming re-renders because ConversationPage keys
 * transcript cells by their absolute index and thinking cells are append-only;
 * switching to positional keys would reset it mid-read.
 */
export function ThinkingCell({ cell }: { cell: Cell & { kind: "thinking" } }) {
  const [open, setOpen] = useState(false);

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
        onClick={() => setOpen((v) => !v)}
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
