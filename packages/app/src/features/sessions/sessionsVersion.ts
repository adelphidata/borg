/**
 * Says "the set of listed conversations changed" to every list at once.
 *
 * The shell's sidebar and the conversations page each run their own
 * `listSessions` query, and `refetch` is per-instance — there is no shared
 * cache. So a write made on one surface left the other showing a row that is
 * no longer there, or missing one that is. Undo made that plain: the toast
 * lives in the shell, so the shell performs the restore, and a page-local
 * refetch could never have renewed the page the user was actually looking at.
 *
 * Same shape as the draft, activity, and read-mark stores beside it: a counter
 * every list subscribes to and puts in its query deps. A write bumps it, both
 * lists re-read, and neither has to know the other exists.
 */

let version = 0;
const listeners = new Set<() => void>();

/** The counter itself, as `useSyncExternalStore` wants it. */
export function sessionsVersion(): number {
  return version;
}

export function subscribeSessions(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Any write that changes which conversations are listed. */
export function sessionsChanged(): void {
  version += 1;
  for (const listener of listeners) listener();
}
