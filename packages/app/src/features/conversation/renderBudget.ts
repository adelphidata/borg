/**
 * How much of a long transcript to mount at once.
 *
 * A conversation that has run all day can hold thousands of cells, and opening
 * it mounts every one of them before the first paint. Counting *turns* is the
 * obvious budget and the wrong one: one turn can be a two-line exchange or
 * forty tool calls with their output, so a fixed turn count either shows almost
 * nothing or hangs the tab.
 *
 * So the budget is paid in render weight — a rough estimate of what each item
 * actually costs the DOM. The units are arbitrary; only their ratios matter.
 */

import type { Cell } from "./useConversation";
import type { RenderItem } from "./cells/toolRuns";

/** Roughly a few screens of history: enough to scroll back into, not a wall. */
export const INITIAL_BUDGET = 200;
/** One press of "Show earlier" reveals about as much again. */
export const PAGE_BUDGET = 200;
/**
 * Never hide these, whatever they weigh. A conversation made of enormous turns
 * would otherwise open showing nothing but its last reply, and "Show earlier"
 * would be the only way to see the question it answered.
 */
export const MIN_USER_TURNS = 2;

/** Text long enough to matter costs more; this is where it starts counting. */
const CHARS_PER_UNIT = 1_000;
/** Tool output is usually the bulkiest thing in a turn, and often preformatted. */
const TOOL_CHARS_PER_UNIT = 2_000;

/** What mounting one cell costs, in budget units. */
export function cellWeight(cell: Cell): number {
  switch (cell.kind) {
    case "user":
    case "assistant":
    case "thinking":
    case "notice":
    case "error":
      return 1 + Math.ceil(cell.text.length / CHARS_PER_UNIT);
    case "tool":
      // A tool row carries a header, an argument line, and a scrollable output
      // block, so it starts well above a paragraph of prose.
      return 4 + Math.ceil((cell.args.length + cell.output.length) / TOOL_CHARS_PER_UNIT);
    default:
      // Usage lines, sub-agent rows, and prompts are all one small fixed row.
      return 1;
  }
}

/** What mounting one render item costs. */
export function itemWeight(item: RenderItem): number {
  if (item.type === "cell") return cellWeight(item.cell);
  // A collapsed run paints one line, but it is one click from mounting every
  // call inside it — so it costs more than a line and much less than its cells.
  return 3 + item.tools.length;
}

/**
 * The first item to render, as an index into the cell array.
 *
 * Walks back from the newest item, spending `budget`, and stops at an item
 * boundary — which is what keeps a collapsed tool run whole, since the run is
 * one item however many calls it holds. Returns 0 when everything fits, which
 * is the signal that there is nothing left to reveal.
 */
export function budgetCutIndex(items: RenderItem[], budget: number): number {
  let spent = 0;
  let userTurns = 0;
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i]!;
    if (item.type === "cell" && item.cell.kind === "user") userTurns += 1;
    spent += itemWeight(item);
    // Over budget, and the floor is satisfied: everything before this item is
    // hidden. `i + 1` because this item is the first one over the line.
    if (spent > budget && userTurns >= MIN_USER_TURNS && i > 0) return items[i + 1]?.index ?? 0;
  }
  return 0;
}

/**
 * Where the cut moves when the user asks for more.
 *
 * Spends one page of budget on the hidden prefix alone, so each press reveals a
 * comparable amount of history rather than an ever-growing one.
 */
export function revealEarlier(items: RenderItem[], cutIndex: number): number {
  const hidden = items.filter((item) => item.index < cutIndex);
  if (hidden.length === 0) return 0;
  return budgetCutIndex(hidden, PAGE_BUDGET);
}

/**
 * True when `cutIndex` would hide every item — the transcript rendering as
 * nothing but its own "Show earlier messages" button.
 *
 * A cut is a *cell* index, but items are grouped: a collapsed tool run carries
 * the index of its first cell, so a cut inside that run's range leaves no item
 * at or after it. Editing a message rewinds the transcript, and a turn settling
 * folds live tool cells into one run — both can drop the last surviving item
 * from under a cut that is still well inside the cell count.
 */
export function cutHidesEverything(items: RenderItem[], cutIndex: number): boolean {
  return cutIndex > 0 && !items.some((item) => item.index >= cutIndex);
}

/**
 * How close to the top of the scroller counts as asking for more history.
 *
 * Wider than the follow-bottom threshold: a reader flicking upward on a
 * trackpad overshoots, and the reveal should land before the scroller hits its
 * hard stop and the flick reads as "nothing more here".
 */
export const TOP_EDGE_PX = 120;

/**
 * Whether a scroll should reveal earlier history on its own.
 *
 * Only an upward scroll that arrives at the top edge counts. Position alone
 * would fire on the reveal itself — mounting older cells pushes `scrollTop`
 * down, which reads as a downward move and is ignored — and on a short
 * transcript that opens with nothing to scroll. Whether anything is left to
 * reveal is the caller's check; the listener is not even attached otherwise.
 */
export function shouldRevealOnScroll(previousTop: number, top: number): boolean {
  return top < previousTop && top <= TOP_EDGE_PX;
}
