import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { STORAGE_KEYS } from "@borg/ui";
import {
  isSessionUnread,
  markAllSessionsRead,
  markSessionSeen,
  observeSessions,
  resetUnread,
  subscribeUnread,
  unreadVersion,
} from "./unreadStore";

const SECOND = 1000;
/** Whole seconds, the resolution the daemon reports `updated_unix_ts` in. */
const secondsAt = (ms: number) => Math.floor(ms / 1000);

/** One row as a listing reports it. */
const row = (id: string, updatedAt: number, messageCount: number) => ({
  id,
  updatedAt,
  messageCount,
});

beforeEach(() => {
  resetUnread();
});

afterEach(() => {
  resetUnread();
});

describe("isSessionUnread", () => {
  it("marks a session that has said something since this device read it", () => {
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now - 600, 4)]);
    markSessionSeen("s1");
    // The listing fetched on the way out is what stamps the watermark, and it
    // reports what the conversation actually held when it was left.
    observeSessions([row("s1", now, 6)]);

    expect(isSessionUnread("s1", now + 60, 7)).toBe(true);
  });

  it("clears once the session is read again", () => {
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now - 600, 4)]);
    markSessionSeen("s1");
    observeSessions([row("s1", now, 6)]);
    markSessionSeen("s1");

    expect(isSessionUnread("s1", now, 6)).toBe(false);
  });

  it("does not light a dot for a rename", () => {
    // The false positive this kills: `updated_at` is bumped by title generation
    // and by plain upserts, so a renamed session used to ask to be read again.
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now - 600, 4)]);
    markSessionSeen("s1");

    expect(isSessionUnread("s1", now, 4)).toBe(false);
  });

  it("does not turn on the device clock", () => {
    // The old skew bug: the daemon stamps whole seconds and the mark used
    // wall-clock milliseconds, so reading an answer as it arrived could leave
    // the row marked. Both sides are daemon values now, so there is no gap.
    const written = 1_700_000_000;
    observeSessions([row("s1", written, 3)]);
    markSessionSeen("s1");

    expect(isSessionUnread("s1", written, 3, written * 1000 + 300)).toBe(false);
  });

  it("keeps sessions independent", () => {
    const now = secondsAt(Date.now());
    observeSessions([row("read", now, 6), row("stale", now - 600, 4)]);
    markSessionSeen("read");
    observeSessions([row("read", now, 6)]);

    expect(isSessionUnread("read", now, 6)).toBe(false);
    // Never opened, and it spoke after tracking began — so it stays lit while
    // the one beside it is cleared.
    expect(isSessionUnread("stale", now + 60, 6)).toBe(true);
  });

  it("treats conversations older than the first launch as read", () => {
    // Otherwise installing the app lights a dot on every conversation in the
    // history, which says nothing except "you have used Borg before".
    const now = Date.now();
    // The first call is what starts tracking.
    isSessionUnread("anything", secondsAt(now), 1, now);

    expect(isSessionUnread("old", secondsAt(now - 30 * 60 * SECOND), 2, now)).toBe(false);
  });

  it("marks a never-opened session that changed after tracking began", () => {
    // The case the feature exists for: Borg answers on a schedule, or in a
    // channel, and the conversation appears already having something to read.
    const start = Date.now();
    isSessionUnread("anything", secondsAt(start), 1, start);
    const later = start + 60 * SECOND;

    expect(isSessionUnread("fresh", secondsAt(later), 2, later)).toBe(true);
  });

  it("falls back to the timestamp when the mark carries no count", () => {
    // A watermark can name a moment without naming a count — the daemon did
    // not report one. The timestamp still has to decide the dot.
    const now = secondsAt(Date.now());
    markSessionSeen("s1", { ts: now - 600, count: -1 });

    expect(isSessionUnread("s1", now, 2)).toBe(true);
    expect(isSessionUnread("s1", now - 600, 2)).toBe(false);
  });
});

describe("markSessionSeen", () => {
  it("wakes subscribers so the list beside the conversation repaints", () => {
    observeSessions([row("s1", secondsAt(Date.now()), 2)]);
    const listener = vi.fn();
    subscribeUnread(listener);

    markSessionSeen("s1");

    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("does not wake subscribers when nothing moved", () => {
    // The mark is renewed on every streamed cell, so a no-op has to be free or
    // each token repaints every session list on screen.
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now, 4)]);
    markSessionSeen("s1");
    const listener = vi.fn();
    subscribeUnread(listener);

    markSessionSeen("s1");

    expect(listener).not.toHaveBeenCalled();
    expect(unreadVersion()).toBeGreaterThanOrEqual(0);
  });

  it("ignores a session with no id yet", () => {
    // A conversation opened at `/c/new` has no id until the daemon answers.
    const listener = vi.fn();
    subscribeUnread(listener);

    markSessionSeen("");

    expect(listener).not.toHaveBeenCalled();
  });

  it("survives a reload", () => {
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now, 4)]);
    markSessionSeen("s1");

    // What a fresh page load does: in-memory state gone, localStorage intact.
    const stored = localStorage.getItem(STORAGE_KEYS.unread);
    expect(stored).not.toBeNull();
    resetUnread();
    localStorage.setItem(STORAGE_KEYS.unread, stored!);

    expect(isSessionUnread("s1", now, 4)).toBe(false);
  });

  it("discards a v1 blob rather than reading its device clock as daemon time", () => {
    // v1 stored wall-clock milliseconds under the same shape. Read as a
    // daemon watermark they sit decades in the future and would suppress every
    // dot forever, so the blob is dropped and tracking re-baselines from now.
    const now = Date.now();
    localStorage.setItem(
      STORAGE_KEYS.unread,
      JSON.stringify({ since: now - 60 * SECOND, seen: { s1: now } }),
    );

    // Nothing said yet at the moment tracking restarts…
    expect(isSessionUnread("s1", secondsAt(now), 5, now)).toBe(false);
    // …but the next reply still lights the dot.
    const later = now + 60 * SECOND;
    expect(isSessionUnread("s1", secondsAt(later), 6, later)).toBe(true);
  });

  it("starts tracking from now when storage holds junk", () => {
    // A dot on every row is the failure mode to avoid; losing the marks is not.
    localStorage.setItem(STORAGE_KEYS.unread, "{not json");
    const now = Date.now();

    expect(isSessionUnread("s1", secondsAt(now - 60 * SECOND), 3, now)).toBe(false);
  });
});

describe("markAllSessionsRead", () => {
  it("clears every listed row", () => {
    const now = secondsAt(Date.now());
    const rows = [row("s1", now, 6), row("s2", now, 2)];

    markAllSessionsRead(rows);

    expect(isSessionUnread("s1", now, 6)).toBe(false);
    expect(isSessionUnread("s2", now, 2)).toBe(false);
  });

  it("leaves a later reply unread", () => {
    const now = secondsAt(Date.now());
    markAllSessionsRead([row("s1", now, 6)]);

    expect(isSessionUnread("s1", now + 30, 7)).toBe(true);
  });

  it("covers a session the listing never mentioned", () => {
    // "Mark all read" has to mean all of them, including whatever sat beyond
    // the page — otherwise the dot the user was clearing survives the click.
    const now = Date.now();
    isSessionUnread("anything", secondsAt(now), 1, now);

    markAllSessionsRead([row("s1", secondsAt(now), 6)]);

    expect(isSessionUnread("unlisted", secondsAt(now), 4, now + SECOND)).toBe(false);
  });
});

describe("reading a conversation that is still growing", () => {
  it("does not light a dot for replies read as they arrived", () => {
    // The bug this encodes: the sidebar only re-lists on navigation, so while
    // the reader sits in a conversation the observed count stays at whatever it
    // was on arrival. Stamping the mark from that stale figure meant the
    // listing fetched on the way out reported more messages than the mark held,
    // and the conversation just finished lit up as unread.
    const now = secondsAt(Date.now());
    observeSessions([row("s1", now, 6)]);
    isSessionUnread("s1", now, 6);

    markSessionSeen("s1"); // opened at 6 messages
    observeSessions([row("s1", now + 40, 8)]); // two replies streamed in and were read

    expect(isSessionUnread("s1", now + 40, 8)).toBe(false);
  });

  it("holds a conversation no listing has ever reported as read", () => {
    // A conversation begun moments ago is absent from every listing, so there
    // are no figures to mark it with. It must not read as unread while it is
    // the thing on screen.
    markSessionSeen("fresh");

    expect(isSessionUnread("fresh", secondsAt(Date.now()), 2)).toBe(false);
  });

  it("lights a dot for a reply that lands after the reader left", () => {
    // The counterpart: deferring to the next listing must not become "never
    // unread again". Once a listing has stamped the mark, the next message
    // after it is genuinely unseen.
    const now = secondsAt(Date.now());
    markSessionSeen("s1");
    observeSessions([row("s1", now, 8)]);

    expect(isSessionUnread("s1", now + 60, 9)).toBe(true);
  });
});
