import { useCallback, useEffect, useRef, useState } from "react";
import type { meetings } from "@borg/client";
import { useBorg, useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";
import { meetingsChanged } from "./meetingsStore";
import { useMeetingsVersion } from "./useMeetingEvents";

/**
 * Every read and write the Meetings views make. Reads re-run when the shared
 * meetings counter moves; writes bump it.
 */

export type MeetingsStatus = meetings.MeetingsStatus;
export type MeetingSummary = meetings.MeetingSummary;
export type Meeting = meetings.Meeting;
export type CalendarEvent = meetings.CalendarEvent;

/** How many past meetings one page of the list holds. */
export const MEETINGS_PAGE = 30;

export function useMeetingsStatus(): QueryResult<MeetingsStatus> {
  const version = useMeetingsVersion();
  return useQuery((borg) => borg.meetings.getMeetingsStatus({}), [version]);
}

/** Calendar events overlapping `[fromUnix, toUnix)`. */
export function useUpcomingEvents(fromUnix: number, toUnix: number): QueryResult<CalendarEvent[]> {
  const version = useMeetingsVersion();
  return useQuery(
    async (borg) => (await borg.meetings.listUpcomingEvents({ fromUnix, toUnix })).events,
    [fromUnix, toUnix, version],
  );
}

export interface MeetingList {
  query: QueryResult<MeetingSummary[]>;
  /** The newest page plus every page "Show earlier" has added, newest first. */
  meetings: MeetingSummary[];
  /** The last page came back full, so there may be more before it. */
  hasMore: boolean;
  loadingMore: boolean;
  loadMore: () => void;
}

/**
 * Past meetings, newest first. The newest page follows the feed; older pages
 * are fetched on demand with the last row's start as the bound.
 */
export function useMeetingList(): MeetingList {
  const borg = useBorg();
  const version = useMeetingsVersion();
  const query = useQuery(
    async (b) => (await b.meetings.listMeetings({ limit: MEETINGS_PAGE, beforeUnix: 0 })).meetings,
    [version],
  );
  const [older, setOlder] = useState<MeetingSummary[]>([]);
  const [olderFull, setOlderFull] = useState<boolean>();
  const [loadingMore, setLoadingMore] = useState(false);

  const first = query.state === "ready" ? query.data : [];
  const seen = new Set(first.map((m) => m.id));
  const list = [...first, ...older.filter((m) => !seen.has(m.id))];
  const hasMore =
    olderFull === undefined ? first.length >= MEETINGS_PAGE : olderFull && older.length > 0;

  const last = list[list.length - 1];
  const loadMore = useCallback(() => {
    if (!last || loadingMore) return;
    setLoadingMore(true);
    borg.meetings
      .listMeetings({ limit: MEETINGS_PAGE, beforeUnix: last.startedAt })
      .then((page) => {
        setOlder((prev) => [...prev, ...page.meetings]);
        setOlderFull(page.meetings.length >= MEETINGS_PAGE);
      })
      .catch((error: unknown) => console.warn("could not load earlier meetings", error))
      .finally(() => setLoadingMore(false));
  }, [borg, last, loadingMore]);

  return { query, meetings: list, hasMore, loadingMore, loadMore };
}

/**
 * One meeting with its transcript. With `markRead`, opening a meeting whose
 * summary nobody has seen clears its unread mark, once.
 */
export function useMeeting(id: string, options: { markRead?: boolean } = {}): QueryResult<Meeting> {
  const borg = useBorg();
  const version = useMeetingsVersion();
  const query = useQuery((b) => b.meetings.getMeeting({ id }), [id, version]);
  const marked = useRef(new Set<string>());
  const unread =
    query.state === "ready" && query.data.summary?.unread === true ? query.data.summary.id : "";
  const markRead = options.markRead === true;
  useEffect(() => {
    if (!markRead || unread === "" || marked.current.has(unread)) return;
    marked.current.add(unread);
    borg.meetings
      .markMeetingRead({ id: unread })
      .then(() => meetingsChanged())
      .catch((error: unknown) => console.warn("could not mark the meeting read", error));
  }, [borg, markRead, unread]);
  return query;
}

export function useStopMeeting() {
  return useMutation((borg, id: string) => borg.meetings.stopMeeting({ id }), {
    onSuccess: () => meetingsChanged(),
  });
}

export function useDeleteMeeting() {
  return useMutation((borg, id: string) => borg.meetings.deleteMeeting({ id }), {
    onSuccess: () => meetingsChanged(),
  });
}
