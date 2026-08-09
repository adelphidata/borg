import { useState } from "react";
import { CodeBlock } from "@borg/ui";
import type { Cell } from "../useConversation";

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
export function summarize(name: string, args: Record<string, unknown> | null): string | null {
  if (!args) return null;
  // Whichever identifying field this tool happens to use.
  for (const field of ["path", "file_path", "command", "query", "url", "pattern", "name"]) {
    const value = args[field];
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  void name;
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
  const args = parseArgs(cell.args);
  const subtitle = summarize(cell.name, args);
  const expanded = open || !cell.done;
  const summary = cell.output.length > 400 ? `${cell.output.slice(0, 400)}…` : cell.output;

  return (
    <div className="rounded-[10px] border border-line2">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-4 py-3 text-left"
      >
        <span
          className={`h-1.5 w-1.5 flex-none rounded-full ${
            cell.done ? (cell.stderr ? "bg-alert" : "bg-neon") : "animate-bpulse bg-ink3"
          }`}
        />
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
          {cell.output !== "" && (
            <pre
              className={`m-0 max-h-[300px] overflow-auto text-[11.5px] leading-[1.5] whitespace-pre-wrap select-text ${
                cell.stderr ? "text-alert" : "text-ink2"
              }`}
            >
              {open ? cell.output : summary}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
