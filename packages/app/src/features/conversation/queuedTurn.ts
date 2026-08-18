import type { Cell } from "./useConversation";

/**
 * True while the turn is parked on a question only the user can answer.
 *
 * Text typed at this moment has nowhere to go: the daemon's steer channel is
 * drained at tool boundaries, and the agent is not at one — it is waiting. So
 * the composer holds the text instead of sending it, and the queue drains when
 * the turn ends.
 */
export function awaitingInput(cells: readonly Cell[]): boolean {
  return cells.some(
    (c) => (c.kind === "shell-confirm" || c.kind === "input-request") && !c.answered,
  );
}

/**
 * Marks every open prompt answered.
 *
 * A prompt only means something inside the turn that raised it. Once that turn
 * is over — cancelled, finished, or failed — nobody is reading the answer, and
 * a cell left open keeps `awaitingInput` true for the rest of the session: the
 * composer would queue every later message instead of steering the turn that is
 * actually running. The daemon can end a turn with a prompt still open (a
 * confirmation that timed out, or one answered from the TUI, which this surface
 * never hears about), so this is not a theoretical state.
 */
export function retirePrompts(cells: readonly Cell[]): Cell[] {
  if (!awaitingInput(cells)) return cells as Cell[];
  return cells.map((c) =>
    (c.kind === "shell-confirm" || c.kind === "input-request") && !c.answered
      ? { ...c, answered: true }
      : c,
  );
}

/**
 * Folds a second queued submission into the first.
 *
 * One queued entry, never a stack: a queue the user cannot see the end of is
 * worse than no queue, and nothing typed is dropped either way.
 */
export function mergeQueued(existing: string, addition: string): string {
  if (existing === "") return addition;
  if (addition === "") return existing;
  return `${existing}\n${addition}`;
}
