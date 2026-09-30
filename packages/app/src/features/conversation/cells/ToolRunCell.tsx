import { useMemo, useState } from "react";
import { StatusDot } from "@borg/ui";
import { parseArgs, summarize, ToolCell } from "./ToolCell";
import { runSummary, type ToolCellData } from "./toolRuns";

/**
 * A run of tool calls, told as one line.
 *
 * The alternative is what this replaces: a turn that reads twenty files
 * printing twenty rows, none of which the reader wanted and all of which push
 * the reply off screen. While the turn is live the run also carries a ticker
 * naming the call that just finished, so the transcript still shows movement;
 * once it settles the summary is the whole of it until someone asks for more.
 */
export function ToolRunCell({ tools, live }: { tools: ToolCellData[]; live: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const summary = useMemo(() => runSummary(tools, live), [tools, live]);
  const latest = tools[tools.length - 1];
  const ticker = useMemo(
    () => (live && latest ? summarize(parseArgs(latest.args)) : null),
    [live, latest],
  );

  return (
    <div className="rounded-[10px] border border-line2">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-4 py-3 text-left"
      >
        <StatusDot tone={live ? "busy" : "ok"} />
        <span className="min-w-0 flex-1 truncate text-[11.5px] leading-none text-ink3">
          {summary}
        </span>
        <span className="flex-none text-[11px] leading-none text-ink3">{tools.length}</span>
      </button>

      {live && ticker && !expanded && (
        <div className="truncate px-4 pb-3 text-[11.5px] leading-none text-ink3">{ticker}</div>
      )}

      {expanded && (
        <div className="flex flex-col gap-2 px-2 pb-2">
          {/* The position joins the key: a provider may hand out the same call
              id twice, and a run only ever grows at its end. */}
          {tools.map((tool, i) => (
            <ToolCell key={`${tool.id}:${i}`} cell={tool} />
          ))}
        </div>
      )}
    </div>
  );
}
