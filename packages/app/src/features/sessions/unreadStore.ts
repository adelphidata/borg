/**
 * Which conversations have changed since this device last looked at them.
 *
 * Deliberately local. "Unread" is a fact about a person in front of a screen,
 * not about the daemon — the same conversation can be read on the desktop and
 * unread on a phone, and a server-side flag would have to pick one of those to
 * be wrong about. So it lives in `localStorage` and never leaves the machine.
 *
 * What is stored is daemon-stamped, though, and that is the part that matters:
 * a watermark is the message count and timestamp the daemon last reported for a
 * session at the moment this device read it. Comparing two daemon values leaves
 * no room for clock skew, and a count that has not moved is proof nothing was
 * said — where a bumped `updated_at` only proves the row was written, which
 * generating a title also does.
 */

import { STORAGE_KEYS } from "@borg/ui";

/** What the daemon last reported about a session this device has read. */
interface Watermark {
  /** The daemon's `updated_unix_ts` at that moment. */
  ts: number;
  /** The daemon's message count at that moment, or -1 when it was unknown. */
  count: number;
}

interface Persisted {
  /** Storage shape. A blob from an older shape is discarded, never guessed at. */
  v: 2;
  /**
   * When this device started tracking. Sessions older than this are treated as
   * read: a first launch that lit a dot on every conversation in the history
   * would say nothing except "you have used Borg before".
   */
  since: number;
  seen: Record<string, Watermark>;
}

/** A count is only comparable when the daemon actually supplied one. */
const UNKNOWN_COUNT = -1;

const listeners = new Set<() => void>();
let version = 0;
let state: Persisted | undefined;
/**
 * The newest listing figures per session, filled by whichever list last
 * fetched. A mark is written from these rather than from the device clock,
 * which is what makes it survive a restart without drifting.
 */
const observed = new Map<string, Watermark>();
/**
 * Sessions this device is reading, or has just read.
 *
 * A conversation on screen cannot be told how many rows the daemon counts for
 * it — the transcript renders thinking, tool and usage cells that are not
 * messages, so counting what is drawn would write a confidently wrong
 * watermark. What reading it does establish is the intent: everything this
 * session has to say up to now has been seen. So the id is parked here and the
 * next listing to mention it supplies the figures, in the daemon's own units.
 *
 * This is also what covers a conversation begun moments ago, which no listing
 * has ever reported and which would otherwise be unread while being written.
 */
const readThroughNextListing = new Set<string>();

function load(now: number): Persisted {
  if (state) return state;
  try {
    const raw = localStorage.getItem(STORAGE_KEYS.unread);
    const parsed: unknown = raw === null ? undefined : JSON.parse(raw);
    if (parsed && typeof parsed === "object") {
      const { v, since, seen } = parsed as Partial<Persisted>;
      // v1 stored device wall-clock marks, which cannot be compared against a
      // daemon timestamp. Re-baseline rather than convert: an extra dot costs a
      // glance, a wrong one costs trust in every other dot.
      if (v === 2 && typeof since === "number" && seen && typeof seen === "object") {
        state = { v: 2, since, seen };
        return state;
      }
    }
  } catch {
    // Unparseable or unavailable storage costs the dots, nothing else. Start
    // tracking from now rather than lighting up the whole list.
  }
  state = { v: 2, since: now, seen: {} };
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
 * Records what the daemon currently reports about each listed session.
 *
 * Every list calls this as its fetch lands. Marking a session read then means
 * copying these figures across, so the watermark is always in the daemon's own
 * units.
 */
export function observeSessions(
  rows: readonly { id: string; updatedAt: number; messageCount: number }[],
): void {
  let marked = false;
  const loaded = load(Date.now());
  for (const row of rows) {
    observed.set(row.id, { ts: row.updatedAt, count: row.messageCount });
    // The listing that lands as the reader navigates away is what finally
    // stamps the watermark. Waiting for it is the whole point: the figures it
    // carries are the ones the row will be judged against.
    if (readThroughNextListing.delete(row.id)) {
      loaded.seen[row.id] = { ts: row.updatedAt, count: row.messageCount };
      marked = true;
    }
  }
  if (marked) {
    save();
    notify();
  }
}

/**
 * Records that this device is looking at a session right now.
 *
 * Called while the conversation is open, including as each reply settles, so
 * reading along with a streaming answer never leaves the row marked afterwards.
 *
 * Passing no figures means "read through whatever the daemon reports next",
 * which is the only claim a transcript can honestly make about itself. The
 * best currently-known watermark is written straight away so a reload is not
 * left with nothing, and the next listing replaces it with the real one.
 */
export function markSessionSeen(sessionId: string, mark?: Watermark): void {
  if (sessionId === "") return;
  const loaded = load(Date.now());
  // Without figures of its own, reading defers to the next listing rather than
  // freezing whatever the last one happened to say — which, for a conversation
  // being read as it grows, is already out of date.
  if (mark === undefined) readThroughNextListing.add(sessionId);
  const next = mark ?? observed.get(sessionId) ?? { ts: 0, count: UNKNOWN_COUNT };
  const current = loaded.seen[sessionId];
  if (current && current.ts >= next.ts && current.count >= next.count) return;
  loaded.seen[sessionId] = {
    ts: Math.max(current?.ts ?? 0, next.ts),
    count: Math.max(current?.count ?? UNKNOWN_COUNT, next.count),
  };
  save();
  notify();
}

/** Marks every listed session read, dots and all. */
export function markAllSessionsRead(
  rows: readonly { id: string; updatedAt: number; messageCount: number }[],
): void {
  const loaded = load(Date.now());
  for (const row of rows) {
    loaded.seen[row.id] = { ts: row.updatedAt, count: row.messageCount };
  }
  // Anything not listed — archived, or beyond the page — is covered by the
  // baseline instead, so "mark all read" means all of them.
  loaded.since = Math.max(loaded.since, Date.now());
  readThroughNextListing.clear();
  save();
  notify();
}

/**
 * True when the session has said something this device has not seen.
 *
 * Counts decide it wherever both sides have one: a bumped `updated_at` alone
 * means only that the row was written, which titling and plain upserts also do,
 * and reading a dot that a rename lit teaches people to ignore dots.
 */
export function isSessionUnread(
  sessionId: string,
  updatedAt: number,
  messageCount?: number,
  now: number = Date.now(),
): boolean {
  if (readThroughNextListing.has(sessionId)) return false;
  const loaded = load(now);
  const seen = loaded.seen[sessionId];
  if (seen === undefined) return updatedAt * 1000 > loaded.since;
  if (messageCount !== undefined && seen.count !== UNKNOWN_COUNT) {
    return messageCount > seen.count;
  }
  return updatedAt > seen.ts;
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
  observed.clear();
  readThroughNextListing.clear();
  try {
    localStorage.removeItem(STORAGE_KEYS.unread);
  } catch {
    // Nothing to clear.
  }
  notify();
}
