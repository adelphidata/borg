import { useMemo, useState, useSyncExternalStore } from "react";
import type { admin as adminPb } from "@borg/client";
import { draftsVersion, rowTitle, subscribeDrafts } from "../conversation/draftStore";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

export interface SessionRow {
  id: string;
  title: string;
  messageCount: number;
  updatedAt: number;
}

/** A run of sessions sharing a heading, newest group first. */
export interface SessionGroup {
  label: string;
  sessions: SessionRow[];
}

const DAY_MS = 86_400_000;

/**
 * Buckets sessions the way a person thinks about them.
 *
 * `now` is a parameter so the boundaries are testable — "yesterday" depends
 * entirely on when you ask.
 */
export function groupSessions(sessions: SessionRow[], now: number): SessionGroup[] {
  const startOfToday = new Date(now).setHours(0, 0, 0, 0);
  const groups: SessionGroup[] = [];

  const labelFor = (updatedAt: number): string => {
    const ms = updatedAt * 1000;
    if (ms >= startOfToday) return "Today";
    if (ms >= startOfToday - DAY_MS) return "Yesterday";
    if (ms >= startOfToday - 7 * DAY_MS) return "Previous 7 days";
    if (ms >= startOfToday - 30 * DAY_MS) return "Previous 30 days";
    return "Older";
  };

  // Already newest-first from the daemon, so a single pass keeps the order.
  for (const session of sessions) {
    const label = labelFor(session.updatedAt);
    const tail = groups[groups.length - 1];
    if (tail && tail.label === label) tail.sessions.push(session);
    else groups.push({ label, sessions: [session] });
  }
  return groups;
}

/** Case-insensitive title match. Empty query keeps everything. */
export function filterSessions(sessions: SessionRow[], query: string): SessionRow[] {
  const needle = query.trim().toLowerCase();
  if (needle === "") return sessions;
  return sessions.filter((s) => s.title.toLowerCase().includes(needle));
}

/** Carries the daemon's title through untouched; naming happens at render. */
function toRow(s: adminPb.SessionSummary): SessionRow {
  return {
    id: s.id,
    title: s.title,
    messageCount: Number(s.messageCount),
    updatedAt: Number(s.updatedUnixTs),
  };
}

/**
 * Names each row.
 *
 * A session nobody has written to yet is named after whatever sits unsent in
 * its composer, so a row of drafts says which is which instead of repeating
 * "Untitled". Applied here rather than at fetch time so typing renames the row
 * live, and applied before the search filter so a draft is findable by what it
 * says.
 */
export function resolveTitles(sessions: SessionRow[]): SessionRow[] {
  return sessions.map((s) => {
    const title = rowTitle(s.title, s.messageCount, s.id);
    return title === s.title ? s : { ...s, title };
  });
}

export interface Sessions {
  query: QueryResult<SessionRow[]>;
  search: string;
  setSearch: (value: string) => void;
  /** Filtered and bucketed, ready to render. */
  groups: SessionGroup[];
  rename: (id: string, title: string) => Promise<unknown>;
  archive: (id: string) => Promise<unknown>;
  /** Downloads the transcript as a file. */
  exportSession: (id: string, format: string) => Promise<unknown>;
  error: string | undefined;
}

/**
 * The session list, with the actions the daemon supports on a stored session.
 *
 * There is deliberately no delete: the daemon models removal as archiving, a
 * reversible soft hide that `borg sessions unarchive` undoes. A destructive
 * delete has no RPC and no undo, so offering one would be a worse promise than
 * the product can keep.
 */
/**
 * Stable empty list. A fresh `[]` while the query is loading or errored would
 * give `groups` a new dependency every render and defeat its memo.
 */
const NO_SESSIONS: SessionRow[] = [];

export function useSessions(deps: unknown[] = []): Sessions {
  const [search, setSearch] = useState("");

  const query = useQuery(async (borg) => {
    const list = await borg.admin.listSessions({});
    return list.sessions.map(toRow);
  }, deps);

  const renameMutation = useMutation(
    (borg, vars: { id: string; title: string }) => borg.admin.setSessionTitle(vars),
    { onSuccess: () => query.refetch() },
  );
  const archiveMutation = useMutation(
    (borg, id: string) => borg.admin.setSessionArchived({ id, archived: true }),
    { onSuccess: () => query.refetch() },
  );
  const exportMutation = useMutation(async (borg, vars: { id: string; format: string }) => {
    const result = await borg.admin.exportSession({
      sessionId: vars.id,
      format: vars.format,
      noRedact: false,
    });
    // Hand the bytes to the browser as a download; both surfaces can do this,
    // so there is no native-only path to maintain.
    const blob = new Blob([result.content as BlobPart]);
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = result.suggestedFilename || `session.${vars.format}`;
    link.click();
    // `click()` only *schedules* the download; revoking in the same tick can
    // pull the blob out from under it and produce a zero-byte or failed file.
    setTimeout(() => URL.revokeObjectURL(url), 30_000);
    return result;
  });

  const sessions = query.state === "ready" ? query.data : NO_SESSIONS;
  // Re-runs the naming pass whenever a composer publishes, so an unsent row is
  // renamed as it is typed.
  const drafts = useSyncExternalStore(subscribeDrafts, draftsVersion, draftsVersion);
  const groups = useMemo(
    () => groupSessions(filterSessions(resolveTitles(sessions), search), Date.now()),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `drafts` is a
    // version counter: it names no value used here, it only says one changed.
    [sessions, search, drafts],
  );

  return {
    query,
    search,
    setSearch,
    groups,
    rename: (id, title) => renameMutation.run({ id, title }),
    archive: (id) => archiveMutation.run(id),
    exportSession: (id, format) => exportMutation.run({ id, format }),
    error: (renameMutation.error ?? archiveMutation.error ?? exportMutation.error)?.message,
  };
}
