/**
 * Says "something about meetings changed" to every view at once.
 *
 * The sidebar's unread dot, the Meetings list, an open meeting and the
 * Calendar page each run their own query. The app-wide event feed
 * (`useMeetingEventsFeed`) and every write bump this one counter, and each
 * query puts it in its deps — the same shape as `sessionsVersion`.
 */

let version = 0;
const listeners = new Set<() => void>();

/** The counter itself, as `useSyncExternalStore` wants it. */
export function meetingsVersion(): number {
  return version;
}

export function subscribeMeetings(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Any event or write that can change what a meetings view shows. */
export function meetingsChanged(): void {
  version += 1;
  for (const listener of listeners) listener();
}
