import { useEffect, useRef } from "react";
import { useNative } from "../../lib/platform";
import { classifyFailure } from "./sendFailure";
import type { Cell } from "./useConversation";

/** A banner shows a line or two; past this the OS cuts the text itself. */
export const NOTIFICATION_BODY_MAX = 120;

function cutShort(text: string): string {
  if (text.length <= NOTIFICATION_BODY_MAX) return text;
  return `${text.slice(0, NOTIFICATION_BODY_MAX - 1).trimEnd()}…`;
}

/** The first line with words on it, without the markup that opens it. */
function firstLine(text: string): string | undefined {
  for (const line of text.split("\n")) {
    const plain = line
      .replace(/^\s*(#{1,6}|>|[-*+]|\d+\.)\s+/, "")
      .replace(/[*_`]/g, "")
      .trim();
    if (plain !== "") return plain;
  }
  return undefined;
}

/** The newest cell that `keep` accepts. */
function lastCell(cells: Cell[], keep: (cell: Cell) => boolean): Cell | undefined {
  for (let i = cells.length - 1; i >= 0; i -= 1) {
    if (keep(cells[i]!)) return cells[i];
  }
  return undefined;
}

/**
 * What a finished turn has to say for itself.
 *
 * "Your turn is finished" makes the user open the window to learn whether it
 * worked. The turn's own last words answer that from the banner — and a turn
 * that ended in a failure says so instead of passing for a success.
 */
export function finishedBody(cells: Cell[]): string {
  // The usage footer closes every turn, so it is never the news.
  const last = lastCell(cells, (c) => c.kind !== "usage");
  if (last?.kind === "error") {
    const title = classifyFailure({ message: last.text })?.title;
    return title ? `Borg hit a problem: ${title}` : "Borg hit a problem.";
  }
  for (let i = cells.length - 1; i >= 0; i -= 1) {
    const cell = cells[i]!;
    // An older turn's reply is not what just finished.
    if (cell.kind === "user") break;
    if (cell.kind !== "assistant") continue;
    const line = firstLine(cell.text);
    if (line) return cutShort(line);
  }
  return "Finished.";
}

/** What the newest unanswered prompt is asking for. */
export function promptBody(cells: Cell[]): string {
  const prompt = lastCell(
    cells,
    (c) => (c.kind === "shell-confirm" || c.kind === "input-request") && !c.answered,
  );
  if (prompt?.kind === "shell-confirm") {
    const command = firstLine(prompt.command);
    if (command) return cutShort(`Borg wants to run: ${command}`);
  }
  if (prompt?.kind === "input-request") {
    const question = firstLine(prompt.questions?.[0]?.question ?? "");
    if (question) return cutShort(question);
  }
  return "Borg is waiting on an answer.";
}

/** Prompts the agent is waiting on, which are what a badge should count. */
export function pendingPrompts(cells: Cell[]): number {
  return cells.filter(
    (c) => (c.kind === "shell-confirm" || c.kind === "input-request") && !c.answered,
  ).length;
}

/**
 * Tells the OS when something needs the user.
 *
 * Only fires when the window is not focused: a notification for a turn you are
 * sitting and watching is pure noise. Two things qualify — a turn finishing and
 * the agent asking a question — and the second also lights the dock badge,
 * because that one blocks until it is answered. The banner is titled with the
 * conversation, since more than one can be running.
 *
 * A no-op in the browser: `useNative()` returns stubs there.
 */
export function useTurnNotifications(
  cells: Cell[],
  busy: boolean,
  sessionId: string | undefined,
  title?: string,
) {
  const native = useNative();
  const wasBusy = useRef(busy);
  const lastPromptCount = useRef(0);
  // Read when a notification fires, never a reason to fire one: a title that
  // arrives late or a token that streams in must not re-run the effects.
  const latest = useRef({ cells, title });
  latest.current = { cells, title };

  useEffect(() => {
    const finished = wasBusy.current && !busy;
    wasBusy.current = busy;
    if (!finished || document.hasFocus()) return;
    native.notify({
      title: latest.current.title || "Borg",
      body: finishedBody(latest.current.cells),
      sessionId,
      // One pending "done" per conversation; a burst of turns should not
      // stack into a wall of identical banners.
      key: `turn-complete:${sessionId ?? "current"}`,
    });
  }, [busy, native, sessionId]);

  useEffect(() => {
    const pending = pendingPrompts(cells);
    const previous = lastPromptCount.current;
    if (pending === previous) return;
    lastPromptCount.current = pending;

    // Only on an actual change: `cells` is a new array on every streamed
    // token, and `setBadgeCount` is an IPC round trip to the main process.
    // Setting the badge to the same 0 a few thousand times per turn is pure
    // cost.
    native.setBadgeCount(pending);
    if (pending <= previous || document.hasFocus()) return;
    native.notify({
      title: latest.current.title || "Borg",
      body: promptBody(cells),
      sessionId,
      key: `prompt:${sessionId ?? "current"}`,
    });
  }, [cells, native, sessionId]);

  // A badge left behind after the conversation closes would never clear.
  useEffect(() => () => native.setBadgeCount(0), [native]);
}
