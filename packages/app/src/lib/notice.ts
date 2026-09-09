/**
 * The one transient notice at the foot of the shell, and the way back it may
 * carry.
 *
 * Borg does not put a confirmation dialog in front of a reversible action —
 * the action happens, and the way back stands here for a few seconds. A
 * conversation can be archived from the sidebar, from its right-click menu, or
 * from the conversations page; all three publish here, and the shell renders
 * one toast for whichever fired last.
 *
 * A notice carries **data, never a callback**. The page that archived a
 * conversation can be unmounted by the time Undo is pressed — the toast lives
 * in the shell and survives navigation — so a closure captured from that page
 * would run its restore against a dead component: the error would land in
 * discarded state and the list would never refetch. Naming the session instead
 * lets the shell, which outlives every page, do the work.
 *
 * Deliberately a single slot rather than a queue. Two notices on screen means
 * two "Undo" buttons and no way to tell which is which, so a new notice simply
 * replaces the old one — the older action stays done, which is what the user
 * asked for in the first place.
 *
 * The accepted cost of one slot: a notice can displace a live offer. Archive
 * A, press Undo, archive B while A's restore is still in flight, and A's
 * failure notice takes B's offer away — B stays archived with no way back on
 * screen. That needs three acts inside a few seconds, and the alternative is a
 * queue, which is the thing this is not.
 */

import { useSyncExternalStore } from "react";

export interface Notice {
  /**
   * Identity of this notice. Two archives in a row can produce the same words
   * — an untitled conversation falls back to a fixed name — and the toast
   * keys its dismissal clock on this, so the second notice gets its own full
   * few seconds instead of inheriting the first one's remaining deadline.
   */
  id: number;
  message: string;
  /** The conversation to bring back. Absent on a notice with no way back. */
  sessionId?: string;
}

let pending: Notice | undefined;
let lastId = 0;
const listeners = new Set<() => void>();

function notify() {
  for (const listener of listeners) listener();
}

function publish(message: string, sessionId?: string): void {
  lastId += 1;
  pending = { id: lastId, message, sessionId };
  notify();
}

/** What a row leaving the list says, named so two archives are told apart. */
export function archivedMessage(title: string | undefined): string {
  return `Archived “${title || "conversation"}”`;
}

/** Offers to bring `sessionId` back, replacing whatever was on offer. */
export function offerRestore(sessionId: string, title: string | undefined): void {
  publish(archivedMessage(title), sessionId);
}

/** Says something happened, with no way back — a failure, usually. */
export function showNotice(message: string): void {
  publish(message);
}

/** Takes the notice away, whether it was used, dismissed, or timed out. */
export function clearNotice(): void {
  if (pending === undefined) return;
  pending = undefined;
  notify();
}

/**
 * Hands back the pending notice and clears the slot in one step.
 *
 * Clearing as it is taken is what makes a double-click harmless: the second
 * press finds nothing and cannot un-archive the same conversation twice.
 */
export function takeNotice(): Notice | undefined {
  const offer = pending;
  clearNotice();
  return offer;
}

export function subscribeNotice(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function pendingNotice(): Notice | undefined {
  return pending;
}

/** What the shell's toast is showing, if anything. */
export function useNotice(): Notice | undefined {
  return useSyncExternalStore(subscribeNotice, pendingNotice, pendingNotice);
}
