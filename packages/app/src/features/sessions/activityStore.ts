/**
 * Which conversations are working right now, shared by every list that shows a
 * status dot.
 *
 * The daemon publishes turn transitions on `Admin.SubscribeSessionActivity`,
 * and one subscriber in the shell folds them in here. Lists read the result;
 * they never subscribe themselves, so the dot on a sidebar row and the dot on
 * the sessions page can never disagree.
 *
 * The feed carries deltas only, so it is not trusted on its own: whenever the
 * stream (re)opens, the shell replaces the whole set from a fresh listing's
 * `turnActive` flags. Anything missed while disconnected is corrected there.
 */

import { draftKeyFor, draftTitle, readDraft } from "../conversation/draftStore";

const working = new Set<string>();
const listeners = new Set<() => void>();
let version = 0;

function notify() {
  version += 1;
  for (const listener of listeners) listener();
}

/** Folds in one transition from the activity feed. */
export function applyActivity(sessionId: string, turnActive: boolean): void {
  if (turnActive === working.has(sessionId)) return;
  if (turnActive) working.add(sessionId);
  else working.delete(sessionId);
  notify();
}

/**
 * Replaces the whole working set from a listing snapshot.
 *
 * This is the correction path: a dot lit when the stream dropped would
 * otherwise stay lit forever, because the transition that would have cleared
 * it was published while nobody was listening.
 */
export function replaceActivity(sessionIds: readonly string[]): void {
  const next = new Set(sessionIds);
  if (next.size === working.size && [...next].every((id) => working.has(id))) return;
  working.clear();
  for (const id of next) working.add(id);
  notify();
}

/** True while a turn runs in this session. */
export function isSessionWorking(sessionId: string): boolean {
  return working.has(sessionId);
}

/** Subscribes to every change in the working set. Returns the unsubscribe. */
export function subscribeActivity(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Snapshot for `useSyncExternalStore`: bumped whenever the set changes. */
export function activityVersion(): number {
  return version;
}

/** Test seam: drops all state so one test cannot leak into the next. */
export function resetActivity(): void {
  working.clear();
  notify();
}

/**
 * What a session row's dot should say, or `undefined` for no dot at all.
 *
 * Only two states earn a mark. "Working" is the one a person actually waits
 * on. "Draft" answers the question a list of same-looking rows raises — which
 * of these did I start and never send? Everything else is settled, and a dot
 * on every row would mean nothing.
 */
export type SessionActivity = "working" | "draft";

export function sessionActivity(
  sessionId: string,
  messageCount: number,
): SessionActivity | undefined {
  // Working wins: a session can hold an unsent draft in one window while a
  // turn runs in another, and the turn is the news.
  if (isSessionWorking(sessionId)) return "working";
  // A conversation with history is not a draft, whatever sits in its composer.
  if (messageCount > 0) return undefined;
  return draftTitle(readDraft(draftKeyFor(sessionId))) === "" ? undefined : "draft";
}
