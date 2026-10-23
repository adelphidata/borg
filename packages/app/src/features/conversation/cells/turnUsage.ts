import type { Cell } from "../useConversation";
import { formatElapsed, formatTokens } from "../../../lib/format";

type UsageCell = Cell & { kind: "usage" };

/**
 * A turn's time and tokens in one line, or "" when there is nothing to say.
 *
 * The elapsed time leads, because "how long did that take" is the question a
 * user asks without being taught to. Token counts are omitted when the turn
 * never reported any, so the line never claims a turn cost zero.
 */
export function usageLabel(usage: Omit<UsageCell, "kind">): string {
  const parts: string[] = [];
  if (usage.elapsedMs !== undefined) parts.push(formatElapsed(usage.elapsedMs));
  if (usage.input + usage.output > 0) {
    parts.push(`${formatTokens(usage.input)} in`, `${formatTokens(usage.output)} out`);
    const cached = usage.cacheRead + usage.cacheWrite;
    if (cached > 0) parts.push(`${formatTokens(cached)} cached`);
  }
  return parts.join(" · ");
}

/**
 * Folds each turn's usage into one line on its last reply, keyed by the index
 * of the cell that shows it. A usage cell with no line renders nothing.
 *
 * The numbers are for the curious, so they stay out of the transcript and wait
 * behind the reply's timestamp. A turn reports usage once per model call, so a
 * turn that ran tools can hold several usage cells; the line is their total,
 * with the elapsed time the last one was stamped with. A turn with no reply
 * (it failed, or was stopped) keeps the line on its last usage cell instead,
 * so the numbers are never lost. Labels are strings, not objects, so a memoised
 * row re-renders only when its own line changes.
 */
export function placeTurnUsage(cells: Cell[]): Map<number, string> {
  const labels = new Map<number, string>();
  let usage: number[] = [];
  let reply = -1;

  const flush = () => {
    if (usage.length === 0) return;
    const total = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
    let elapsedMs: number | undefined;
    for (const i of usage) {
      const cell = cells[i] as UsageCell;
      total.input += cell.input;
      total.output += cell.output;
      total.cacheRead += cell.cacheRead;
      total.cacheWrite += cell.cacheWrite;
      elapsedMs = cell.elapsedMs ?? elapsedMs;
    }
    const label = usageLabel({ ...total, elapsedMs });
    if (label) labels.set(reply >= 0 ? reply : usage[usage.length - 1]!, label);
  };

  cells.forEach((cell, i) => {
    if (cell.kind === "user") {
      flush();
      usage = [];
      reply = -1;
    } else if (cell.kind === "usage") usage.push(i);
    else if (cell.kind === "assistant") reply = i;
  });
  flush();
  return labels;
}
