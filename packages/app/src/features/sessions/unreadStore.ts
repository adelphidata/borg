/**
 * Which conversations have changed since this device last looked at them.
 *
 * Deliberately local. "Unread" is a fact about a person in front of a screen,
 * not about the daemon — the same conversation can be read on the desktop and
 * unread on a phone, and a server-side flag would have to pick one of those to
 * be wrong about. So it lives in `localStorage` and never leaves the machine.
 *
 * The mark is derived, not stored: a session is unread when the daemon says it
 * changed more recently than the last time this device had it open. Nothing has
 * to be written when a reply lands, which means a turn that completes while the
 * app is closed still shows up on the next launch.
 */

import { STORAGE_KEYS } from "@borg/ui";

/** Session id to the wall-clock ms at which this device last read it. */
type SeenMap = Record<string, number>;

interface Persisted {
  /**
   * When this device started tracking. Sessions older than this are treated as
   * read: a first launch that lit a dot on every conversation in the history
   * would say nothing except "you have used Borg before".
   */
  since: number;
  seen: SeenMap;
}

const listeners = new Set<() => void>();
let version = 0;
let state: Persisted | undefined;

function load(now: number): Persisted {
  if (state) return state;
  try {
    const raw = localStorage.getItem(STORAGE_KEYS.unread);
    const parsed: unknown = raw === null ? undefined : JSON.parse(raw);
    if (parsed && typeof parsed === "object") {
      const { since, seen } = parsed as Partial<Persisted>;
      if (typeof since === "number" && seen && typeof seen === "object") {
        state = { since, seen };
        return state;
      }
    }
  } catch {
    // Unparseable or unavailable storage costs the dots, nothing else. Start
    // tracking from now rather than lighting up the whole list.
  }
  state = { since: now, seen: {} };
  save();
  return state;
}

function save() {
  try {
    localStorage.setItem(STORAGE_KEYS.unread, JSON.stringify(state));
  } catch {
    // Private-mode browsers throw here. Memory still carries this session.
  }
}

function notify() {
  version += 1;
  for (const listener of listeners) listener();
}

/**
 * Records that this device is looking at a session right now.
 *
 * Called while the conversation is open, including as each reply settles, so
 * reading along with a streaming answer never leaves the row marked afterwards.
 */
export function markSessionSeen(sessionId: string, now: number = Date.now()): void {
  if (sessionId === "") return;
  const loaded = load(now);
  if ((loaded.seen[sessionId] ?? 0) >= now) return;
  loaded.seen[sessionId] = now;
  save();
  notify();
}

/**
 * True when the daemon's last write to this session is newer than this
 * device's last look at it.
 *
 * `updatedAt` is the daemon's seconds-resolution timestamp. A session never
 * opened here counts as unread only if it changed after tracking began — which
 * is what makes a conversation Borg started on its own, while the user was
 * elsewhere, the case this exists for.
 */
export function isSessionUnread(
  sessionId: string,
  updatedAt: number,
  now: number = Date.now(),
): boolean {
  const loaded = load(now);
  const updatedMs = updatedAt * 1000;
  const seenAt = loaded.seen[sessionId];
  // A second of slack: the daemon stamps whole seconds, so a message written
  // at x.9s reads back as x.0s and would otherwise look newer than a mark made
  // moments after it.
  if (seenAt !== undefined) return updatedMs > seenAt + 1000;
  return updatedMs > loaded.since;
}

/** Subscribes to every change in the read state. Returns the unsubscribe. */
export function subscribeUnread(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Snapshot for `useSyncExternalStore`: bumped whenever a session is read. */
export function unreadVersion(): number {
  return version;
}

/** Test seam: drops all state so one test cannot leak into the next. */
export function resetUnread(): void {
  state = undefined;
  try {
    localStorage.removeItem(STORAGE_KEYS.unread);
  } catch {
    // Nothing to clear.
  }
  notify();
}
