import { useMemo, useState } from "react";
import { CodeBlock, StatusDot } from "@borg/ui";
import type { Cell } from "../useConversation";
import { toolOutcome, type ToolOutcome } from "./toolOutcome";

/** The dot each outcome wears. A stopped call is neither a success nor a fault. */
const TONES = {
  running: "busy",
  ok: "ok",
  failed: "alert",
  stopped: "idle",
} as const satisfies Record<ToolOutcome, string>;

/** Tools whose output is terminal text rather than structured data. */
const SHELL_TOOLS = new Set(["run_shell", "shell", "bash", "exec_shell"]);

/** Argument fields that hold a unified diff, in the order we prefer them. */
const PATCH_FIELDS = ["patch", "diff", "edits"];

/** Reads the JSON the daemon sends for a tool call. Never throws. */
export function parseArgs(argsJson: string): Record<string, unknown> | null {
  if (argsJson.trim() === "") return null;
  try {
    const parsed: unknown = JSON.parse(argsJson);
    return parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

/** The patch text carried in a tool's arguments, if it carries one. */
export function patchOf(args: Record<string, unknown> | null): string | null {
  if (!args) return null;
  for (const field of PATCH_FIELDS) {
    const value = args[field];
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  return null;
}

/** A one-line gist of the call, so a collapsed row still says what it did. */
export function summarize(args: Record<string, unknown> | null): string | null {
  if (!args) return null;
  // A desktop action reads as "click Calculator [4]": the verb, then what it
  // was pointed at. Without this every computer_use row is a bare tool name.
  if (typeof args.action === "string" && args.action.trim() !== "") {
    const parts = [args.action];
    for (const field of ["app", "text", "value", "key"]) {
      const value = args[field];
      if (typeof value === "string" && value.trim() !== "") {
        parts.push(value);
        break;
      }
    }
    if (Array.isArray(args.keys)) parts.push(args.keys.join("+"));
    if (typeof args.element_index === "number") parts.push(`[${args.element_index}]`);
    else if (typeof args.x === "number" && typeof args.y === "number") {
      parts.push(`(${args.x}, ${args.y})`);
    }
    return parts.join(" ");
  }
  // Whichever identifying field this tool happens to use.
  for (const field of ["path", "file_path", "command", "query", "url", "pattern", "name"]) {
    const value = args[field];
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  return null;
}

function ArgsView({
  name,
  args,
  raw,
}: {
  name: string;
  args: Record<string, unknown> | null;
  raw: string;
}) {
  const patch = patchOf(args);
  if (patch) return <CodeBlock code={patch} lang="diff" />;
  if (SHELL_TOOLS.has(name) && typeof args?.command === "string") {
    return <CodeBlock code={args.command} lang="bash" />;
  }
  if (raw.trim() === "") return null;
  return <CodeBlock code={args ? JSON.stringify(args, null, 2) : raw} lang="json" />;
}

/**
 * One tool call: what it was asked to do, and what came back.
 *
 * Collapsed by default once finished — a transcript full of open tool output
 * is unreadable — but stays open while running so progress is visible. The
 * arguments were previously captured and never shown, which made a failed call
 * impossible to diagnose without the CLI.
 */
export function ToolCell({ cell }: { cell: Cell & { kind: "tool" } }) {
  const [open, setOpen] = useState(false);
  // Memoised because this runs on every render, and a streaming turn
  // re-renders the transcript on every delta: without it a long tool-heavy
  // turn re-parses the same JSON thousands of times.
  const args = useMemo(() => parseArgs(cell.args), [cell.args]);
  const subtitle = useMemo(() => summarize(args), [args]);
  const expanded = open || !cell.done;
  const outcome = toolOutcome(cell);
  // The daemon's cancel marker is a record for the model, not a sentence for
  // the reader.
  const output = outcome === "stopped" ? "Stopped" : cell.output;
  const summary = output.length > 400 ? `${output.slice(0, 400)}…` : output;

  return (
    <div className="rounded-[10px] border border-line2">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-4 py-3 text-left"
      >
        <StatusDot tone={TONES[outcome]} />
        <span className="text-[11px] leading-none font-medium tracking-[0.08em] text-ink3 uppercase">
          {cell.name}
        </span>
        {subtitle && (
          <span className="min-w-0 flex-1 truncate text-[11.5px] leading-none text-ink3">
            {subtitle}
          </span>
        )}
        {!cell.done && <span className="text-[11.5px] leading-none text-ink3">running…</span>}
      </button>

      {expanded && (
        <div className="px-4 pb-3">
          {open && <ArgsView name={cell.name} args={args} raw={cell.args} />}
          {output !== "" && (
            <pre
              className={`m-0 max-h-[300px] overflow-auto text-[11.5px] leading-[1.5] whitespace-pre-wrap select-text ${
                outcome === "failed" ? "text-alert" : "text-ink2"
              }`}
            >
              {open ? output : summary}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
