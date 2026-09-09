import { useMemo, useState, useSyncExternalStore } from "react";
import type { admin as adminPb } from "@borg/client";
import { draftsVersion, rowTitle, subscribeDrafts } from "../conversation/draftStore";
import {
  activityVersion,
  sessionActivity,
  sessionWorkingSince,
  subscribeActivity,
  type SessionActivity,
} from "./activityStore";
import { formatElapsed } from "../../lib/format";
import { offerRestore } from "../../lib/notice";
import { sessionsChanged, sessionsVersion, subscribeSessions } from "./sessionsVersion";
import { observeSessions, subscribeUnread, unreadVersion } from "./unreadStore";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

export interface SessionRow {
  id: string;
  title: string;
  messageCount: number;
  updatedAt: number;
  /** Status mark for the row, absent when the conversation is settled. */
  activity?: SessionActivity;
  /** True for a soft-hidden conversation, only ever set when they were asked for. */
  archived?: boolean;
  /** One line of the newest message, capped and redacted by the daemon. */
  preview?: string;
  /** Who said it: "user" or "assistant". */
  previewRole?: string;
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
    archived: s.archived || undefined,
    preview: s.lastMessagePreview || undefined,
    previewRole: s.lastMessageRole || undefined,
  };
}

/** Splits a listing into the conversations on offer and the ones put away. */
export function partitionArchived(rows: SessionRow[]): {
  active: SessionRow[];
  archived: SessionRow[];
} {
  const active: SessionRow[] = [];
  const archived: SessionRow[] = [];
  for (const row of rows) (row.archived ? archived : active).push(row);
  return { active, archived };
}

/**
 * The second line of a session row.
 *
 * A running turn is the news, so it wins and counts up. Otherwise the last
 * thing said says far more about which conversation this is than a message
 * count does — the count is the fallback for a session that has only tool
 * output, or none at all.
 */
export function rowSubtitle(session: SessionRow, now: number): string {
  if (session.activity === "working") {
    const since = sessionWorkingSince(session.id);
    return since === undefined ? "Working…" : `Working… ${formatElapsed(now - since)}`;
  }
  if (session.preview) {
    return session.previewRole === "user" ? `You: ${session.preview}` : session.preview;
  }
  return `${session.messageCount} message${session.messageCount === 1 ? "" : "s"}`;
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
    // An archived conversation is one nobody is waiting on: a dot asking to be
    // read on a row the user deliberately put away is noise.
    const activity = s.archived ? undefined : sessionActivity(s.id, s.messageCount, s.updatedAt);
    return title === s.title && activity === s.activity ? s : { ...s, title, activity };
  });
}

export interface Sessions {
  query: QueryResult<SessionRow[]>;
  search: string;
  setSearch: (value: string) => void;
  /** Filtered and bucketed, ready to render. */
  groups: SessionGroup[];
  rename: (id: string, title: string) => Promise<unknown>;
  /** `title` names the row in the undo offer once it has left the list. */
  archive: (id: string, title?: string) => Promise<unknown>;
  /** Brings an archived conversation back into the list. */
  restore: (id: string) => Promise<unknown>;
  /** Archived conversations, empty until they are asked for. */
  archived: SessionRow[];
  showArchived: boolean;
  setShowArchived: (value: boolean) => void;
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
  const [showArchived, setShowArchived] = useState(false);
  // Someone hunting for a conversation they archived types its name; finding
  // nothing and having to remember a disclosure exists is the wrong answer.
  const searching = search.trim() !== "";
  const includeArchived = showArchived || searching;

  // A write made from the shell — archiving a sidebar row, or the Undo behind
  // the shell's toast — must renew this list too. The two queries are
  // independent, so only a shared counter connects them.
  const listVersion = useSyncExternalStore(subscribeSessions, sessionsVersion, sessionsVersion);
  const query = useQuery(
    async (borg) => {
      const list = await borg.admin.listSessions({ includeArchived });
      const rows = list.sessions.map(toRow);
      // What the daemon says about each session right now, so a read mark can
      // be written in the daemon's units rather than this device's clock.
      observeSessions(rows);
      return rows;
    },
    [...deps, includeArchived, listVersion],
  );

  const renameMutation = useMutation(
    (borg, vars: { id: string; title: string }) => borg.admin.setSessionTitle(vars),
    { onSuccess: () => query.refetch() },
  );
  const setArchivedMutation = useMutation(
    // `title` never reaches the daemon; it rides along so the undo offer can
    // name the row after it has left the list.
    (borg, vars: { id: string; archived: boolean; title?: string }) =>
      borg.admin.setSessionArchived({ id: vars.id, archived: vars.archived }),
    {
      onSuccess: (_data, vars) => {
        sessionsChanged();
        // Archiving only shows as a row leaving the list, which is also what a
        // mis-click looks like. The shell's toast names what went and hands it
        // back, so the act needs no confirmation standing in front of it.
        // Un-archiving needs none of that: the row appears where it went.
        if (vars.archived) offerRestore(vars.id, vars.title);
      },
    },
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
  // Same shape for the working set: a turn starting elsewhere lights this row.
  const activity = useSyncExternalStore(subscribeActivity, activityVersion, activityVersion);
  // And for the read marks, so opening a conversation clears its dot in the
  // list beside it rather than on the next refetch.
  const read = useSyncExternalStore(subscribeUnread, unreadVersion, unreadVersion);
  const named = useMemo(
    () => filterSessions(resolveTitles(sessions), search),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the version
    // counters below name no value used here; they only say one changed.
    [sessions, search, drafts, activity, read],
  );
  const split = useMemo(() => partitionArchived(named), [named]);
  const groups = useMemo(() => groupSessions(split.active, Date.now()), [split.active]);

  return {
    query,
    search,
    setSearch,
    groups,
    rename: (id, title) => renameMutation.run({ id, title }),
    archive: (id, title) => setArchivedMutation.run({ id, archived: true, title }),
    restore: (id) => setArchivedMutation.run({ id, archived: false }),
    archived: split.archived,
    showArchived,
    setShowArchived,
    exportSession: (id, format) => exportMutation.run({ id, format }),
    error: (renameMutation.error ?? setArchivedMutation.error ?? exportMutation.error)?.message,
  };
}
