import { useSyncExternalStore } from "react";
import type { meetings } from "@borg/client";
import { useRetryingServerStream } from "../../lib/useDaemon";
import { meetingsChanged, meetingsVersion, subscribeMeetings } from "./meetingsStore";

/** The meetings counter, for a query's deps. */
export function useMeetingsVersion(): number {
  return useSyncExternalStore(subscribeMeetings, meetingsVersion, meetingsVersion);
}

const idle = () => () => {};

/**
 * Keeps every meetings view current from `SubscribeMeetingEvents`.
 *
 * Mount once, in the shell. Each event bumps the shared counter and the views
 * re-read; the events carry ids, not the rows, so a re-read is what makes a
 * dropped connection harmless. While the feature is off nothing subscribes.
 */
export function useMeetingEventsFeed(enabled: boolean): void {
  useRetryingServerStream<meetings.MeetingEvent>(
    enabled ? (borg, handlers) => borg.meetings.subscribeMeetingEvents({}, handlers) : idle,
    () => meetingsChanged(),
    [enabled],
  );
}
