/**
 * How a tool call ended, read off what the daemon sent back.
 *
 * The wire has no "this call failed" flag: a result is one string. What it does
 * have is the daemon's own wording, so the verdict is read from the text. Live,
 * that text arrives bare. Stored, the daemon wraps it in a `<tool_output>`
 * element for the model; `historyCells` takes the wrapper off, and only that
 * is what makes a reloaded row read the same as the live one.
 *
 * The cancel marker only reaches a row from history, and only for a call that
 * never ran: the daemon stores it but sends no live result for it. Live, the
 * "Stopped" notice comes from the conversation's cancel flag, not from this text.
 *
 * Output on stderr is deliberately not the signal. Plenty of healthy commands
 * write progress there, and a transcript of red rows that all succeeded teaches
 * the user to ignore red.
 */

/** The slice of a tool cell the verdict depends on. */
export interface ToolOutcomeInput {
  output: string;
  done: boolean;
  stderr: boolean;
}

export type ToolOutcome = "running" | "ok" | "failed" | "stopped";

/** What the daemon stores as the result of a call the user cancelled. */
export const TOOL_CANCELLED_MARKER = "[tool call cancelled by user]";

/** Every shell result opens with this line. */
const EXIT_CODE = /^Exit code: (-?\d+)/;

/**
 * Openings the daemon gives a call that did not work, whatever the tool:
 * - a command the sandbox policy refused to run at all;
 * - a call the agent loop could not run (bad arguments, a timeout);
 * - an error the tool's handler returned;
 * - a worker that crashed before it could answer.
 */
const FAILURE_PREFIXES = [
  "Shell command denied",
  "Error: ",
  "[TOOL_ERROR] ",
  "[tool call failed — worker panicked]",
];

/** True for a call the user stopped before it produced a result. */
export function toolStopped(cell: ToolOutcomeInput): boolean {
  return cell.done && cell.output.trim() === TOOL_CANCELLED_MARKER;
}

/**
 * True for a call that really failed.
 *
 * A shell result carries its exit status, and that status is the whole answer:
 * zero is a success whatever was written to stderr. A result with no status
 * line is some other tool, where the daemon's failure wording or output it
 * marked as stderr is the only evidence there is.
 */
export function toolFailed(cell: ToolOutcomeInput): boolean {
  if (!cell.done || toolStopped(cell)) return false;
  const exit = EXIT_CODE.exec(cell.output);
  if (exit) return Number(exit[1]) !== 0;
  if (FAILURE_PREFIXES.some((prefix) => cell.output.startsWith(prefix))) return true;
  return cell.stderr;
}

export function toolOutcome(cell: ToolOutcomeInput): ToolOutcome {
  if (!cell.done) return "running";
  if (toolStopped(cell)) return "stopped";
  return toolFailed(cell) ? "failed" : "ok";
}
