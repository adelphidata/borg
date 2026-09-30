import { useCallback, useEffect, useRef } from "react";
import type { admin as adminPb } from "@borg/client";
import { useBorg, useRetryingServerStream } from "../../lib/useDaemon";
import { applyActivity, replaceActivity } from "./activityStore";
import { sessionsChanged } from "./sessionsVersion";

/**
 * How long after the last transition the lists re-read themselves. Several
 * sessions settling together — a batch of schedules, say — cost one listing,
 * not one each.
 */
export const SESSIONS_REFRESH_DEBOUNCE_MS = 300;

/**
 * The longest a renewal waits, counted from the first transition it covers. A
 * steady stream of transitions less than the debounce apart would otherwise
 * put the renewal off for as long as the stream lasts.
 */
export const SESSIONS_REFRESH_MAX_WAIT_MS = 2_000;

/**
 * Keeps [`activityStore`] fed from the daemon.
 *
 * Mount this exactly once, in the shell: one stream serves every list, and a
 * second subscriber would double the daemon's fan-out for no extra
 * information.
 *
 * Reconnects are handled by `useRetryingServerStream`, and every time the
 * stream opens we re-read a listing. That snapshot is what makes a dropped
 * connection harmless — the feed carries only transitions, so on its own it
 * would strand whatever dot was lit when the connection fell over.
 *
 * The feed is also what tells the lists to renew themselves. A turn that ends
 * in the background changes a row's figures — its unread mark reads from
 * them — and a turn that starts in a conversation nobody has listed yet is a
 * new row. `isListed` says whether a session id is already on screen; omit it
 * and only settles trigger a refresh.
 *
 * An id the lists do not have asks for one renewal, not one per transition:
 * a session the listing never returns would otherwise re-read the lists on
 * every event it sends.
 */
export function useSessionActivityFeed(
  isListed: (sessionId: string) => boolean = () => true,
): void {
  const borg = useBorg();
  const isListedRef = useRef(isListed);
  isListedRef.current = isListed;

  const refresh = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  /** When the first transition the pending renewal covers arrived. */
  const pendingSince = useRef<number | undefined>(undefined);
  const scheduleRefresh = useCallback(() => {
    clearTimeout(refresh.current);
    const now = Date.now();
    pendingSince.current ??= now;
    const deadline = pendingSince.current + SESSIONS_REFRESH_MAX_WAIT_MS;
    const delay = Math.max(0, Math.min(SESSIONS_REFRESH_DEBOUNCE_MS, deadline - now));
    refresh.current = setTimeout(() => {
      refresh.current = undefined;
      pendingSince.current = undefined;
      sessionsChanged();
    }, delay);
  }, []);
  /** Unlisted ids that have already had their one renewal. */
  const renewedFor = useRef(new Set<string>());
  useEffect(() => () => clearTimeout(refresh.current), []);

  /**
   * Transitions that arrived while a listing was in flight.
   *
   * The listing is a snapshot of a moment already past by the time it lands, so
   * anything the feed reported in between is newer and has to survive the
   * replace — otherwise a turn that starts during the reconcile is un-lit by
   * the older snapshot, and its settle is a no-op because the store already
   * reads "not working".
   */
  const sinceListing = useRef<Map<string, boolean> | undefined>(undefined);

  const reconcile = useCallback(() => {
    const pending = new Map<string, boolean>();
    sinceListing.current = pending;
    borg.admin
      .listSessions({})
      .then((list) => {
        // A newer reconcile started; that one owns the result.
        if (sinceListing.current !== pending) return;
        sinceListing.current = undefined;
        const workingIds = new Set(list.sessions.filter((s) => s.turnActive).map((s) => s.id));
        for (const [id, turnActive] of pending) {
          if (turnActive) workingIds.add(id);
          else workingIds.delete(id);
        }
        replaceActivity([...workingIds]);
      })
      .catch((error: unknown) => {
        // The listing is a correction, not the source: the previous set stays
        // and the next reconnect tries again. Still worth a line — a reconcile
        // that fails every time is otherwise invisible.
        if (sinceListing.current === pending) sinceListing.current = undefined;
        console.debug("session activity reconcile failed", error);
      });
  }, [borg]);

  const stream = useRetryingServerStream<adminPb.SessionActivityEvent>(
    (borg, handlers) => borg.admin.subscribeSessionActivity({}, handlers),
    (event) => {
      sinceListing.current?.set(event.sessionId, event.turnActive);
      // The daemon's stamp for the transition, so an elapsed count is measured
      // from when the turn really started rather than from when this client
      // heard about it.
      applyActivity(event.sessionId, event.turnActive, Number(event.emittedAtUnix) * 1000);
      if (!isListedRef.current(event.sessionId)) {
        if (renewedFor.current.has(event.sessionId)) return;
        renewedFor.current.add(event.sessionId);
        scheduleRefresh();
      } else if (!event.turnActive) {
        scheduleRefresh();
      }
    },
  );

  // Every entry into "open" is a fresh subscription — on mount, and after each
  // reconnect — which is exactly when the snapshot has to be re-read.
  const wasOpen = useRef(false);
  useEffect(() => {
    const open = stream.status === "open";
    if (open && !wasOpen.current) reconcile();
    wasOpen.current = open;
  }, [stream.status, reconcile]);
}
