/**
 * What a working turn is actually doing.
 *
 * The status line used to read "Borg is thinking…" for the whole turn, whatever
 * was happening underneath it. A turn stuck forty seconds on a slow provider and
 * a turn grinding through a shell command looked identical, so the only thing
 * the line really said was "not finished". Naming the wait is the difference
 * between a spinner and an answer.
 *
 * Everything here is derived from the transcript itself rather than tracked
 * alongside it. The reducer already replaces the cell each event touches, so the
 * tail of the array *is* the current state — a second copy of it kept in a ref
 * could only ever drift.
 */

import type { Cell } from "./useConversation";
import { awaitingInput } from "./queuedTurn";

export interface TurnActivity {
  /**
   * The line to show, or undefined when something else on screen already says
   * it. Deferring matters as much as labelling: two things narrating the same
   * wait is worse than one, and a second timer beside a first is just noise.
   */
  label?: string;
  /**
   * True for a state we are only guessing at — the gap between one tool
   * finishing and the next thing starting. Held back briefly by
   * `createActivityHold`, because a run of quick tool calls would otherwise
   * flash it between every pair.
   */
  tentative?: boolean;
  /**
   * True when the label says only that the turn is busy, not what it is busy
   * with. The visible line may then show the turn's verb instead; the label
   * stays the plain words a screen reader announces.
   */
  generic?: boolean;
}

/**
 * What the visible line says while a turn is busy with nothing more specific
 * to report. One is picked per turn and kept for the whole of it, so the line
 * reads as a mood rather than a status that keeps changing its mind.
 */
export const BUSY_VERBS = [
  "Pondering",
  "Percolating",
  "Noodling",
  "Mulling",
  "Cogitating",
  "Ruminating",
  "Musing",
  "Deliberating",
  "Contemplating",
  "Brewing",
  "Simmering",
  "Marinating",
  "Stewing",
  "Fermenting",
  "Incubating",
  "Hatching",
  "Conjuring",
  "Divining",
  "Scheming",
  "Tinkering",
  "Puzzling",
  "Untangling",
  "Sleuthing",
  "Spelunking",
  "Wrangling",
  "Finagling",
  "Distilling",
  "Synthesizing",
  "Crunching",
  "Calibrating",
  "Whirring",
  "Churning",
  "Weaving",
  "Moseying",
  "Assimilating",
  "Adapting",
  "Harmonizing",
  "Consulting the collective",
] as const;

/**
 * The verb for a turn, chosen from when it started.
 *
 * Derived rather than drawn from `Math.random` on render, so a re-render
 * cannot swap it. A remount or reopen mid-turn re-arms the clock at that
 * moment, so the verb can change there. Millisecond start times differ between
 * turns, so consecutive turns usually read differently.
 */
export function busyVerb(turnStartedAt: number | undefined): string {
  const seed = Math.abs(Math.floor(turnStartedAt ?? 0));
  return BUSY_VERBS[seed % BUSY_VERBS.length];
}

/** The text the visible status line shows for an activity. */
export function visibleLabel(activity: TurnActivity, verb: string): string | undefined {
  if (activity.label === undefined) return undefined;
  return activity.generic ? `${verb}…` : activity.label;
}

/** The provider has not answered yet: the long wait, and the one to name. */
const WAITING: TurnActivity = { label: "Waiting for the model", generic: true };
/** The gap between one thing finishing and the next starting. A guess. */
const WORKING: TurnActivity = { label: "Working", tentative: true, generic: true };

/** Nothing to add: the cell at the tail is already narrating this wait. */
const DEFER: TurnActivity = {};

/**
 * Reads the current wait off the transcript tail.
 *
 * Only ever called while the turn is in flight; a settled transcript has no
 * activity to describe.
 */
export function turnActivity(cells: readonly Cell[]): TurnActivity {
  // A prompt outranks everything. The turn is not working, it is waiting on the
  // user, and the prompt cell is both the explanation and the thing to act on.
  if (awaitingInput(cells)) return DEFER;

  // A live tool row narrates its own wait, and it stays the answer even once it
  // is no longer the tail: a steer or a queued send lands a user row underneath
  // it, and the command has not stopped running because somebody typed.
  if (cells.some((c) => c.kind === "tool" && !c.done)) return DEFER;

  const last = cells[cells.length - 1];
  if (!last) return WAITING;

  // The provider dropped and the daemon is redialling. Not "Working": nothing
  // is, and the wait is the provider's.
  if (last.kind === "notice" && last.reconnecting) {
    return { label: "Provider unreachable, retrying…" };
  }

  switch (last.kind) {
    // Every live tool row was handled above, so a tool on the tail is a
    // finished one: the gap before whatever comes next.
    case "tool":
      return WORKING;
    // "Thinking…" is what the reasoning block's own header says.
    case "thinking":
      return last.streaming ? DEFER : WORKING;
    // Nothing has come back yet. This is the long one, and the one worth naming:
    // it is almost always the provider, not Borg.
    case "user":
      return WAITING;
    case "assistant":
      return { label: "Replying" };
    default:
      return WORKING;
  }
}

/** Holds a guessed state back until it has lasted long enough to be worth showing. */
export interface ActivityHold {
  /**
   * The activity to render right now, or undefined for nothing. Feed it every
   * derived activity; it decides what survives to the screen.
   */
  settle(next: TurnActivity, now: number): TurnActivity | undefined;
  /** Forget everything. Called when a turn ends, so the next one starts clean. */
  reset(): void;
}

/**
 * Keeps the status line still.
 *
 * A confident label goes up immediately. A tentative one has to last `holdMs`
 * first, and until it does the last confident label stays on screen — a turn
 * making five quick tool calls should read as five tools, not as five tools
 * interleaved with five flashes of "Working".
 */
export function createActivityHold(holdMs = 2_000): ActivityHold {
  let shown: TurnActivity | undefined;
  let pendingSince: number | undefined;

  return {
    settle(next, now) {
      if (!next.tentative) {
        // Includes the deferring case: when the tail starts narrating for
        // itself, the line must clear rather than keep a stale label up.
        shown = next.label === undefined ? undefined : next;
        pendingSince = undefined;
        return shown;
      }
      pendingSince ??= now;
      if (now - pendingSince >= holdMs) {
        shown = next;
        return shown;
      }
      return shown;
    },
    reset() {
      shown = undefined;
      pendingSince = undefined;
    },
  };
}
