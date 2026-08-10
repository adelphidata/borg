import { useCallback, useEffect, useRef } from "react";
import type { admin as adminPb } from "@borg/client";
import { useBorg, useRetryingServerStream } from "../../lib/useDaemon";
import { applyActivity, replaceActivity } from "./activityStore";

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
 */
export function useSessionActivityFeed(): void {
  const borg = useBorg();

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
      applyActivity(event.sessionId, event.turnActive);
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
