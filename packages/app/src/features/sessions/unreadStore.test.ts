import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { STORAGE_KEYS } from "@borg/ui";
import {
  isSessionUnread,
  markSessionSeen,
  resetUnread,
  subscribeUnread,
  unreadVersion,
} from "./unreadStore";

const SECOND = 1000;
/** Whole seconds, the resolution the daemon reports `updated_unix_ts` in. */
const secondsAt = (ms: number) => Math.floor(ms / 1000);

beforeEach(() => {
  resetUnread();
});

afterEach(() => {
  resetUnread();
});

describe("isSessionUnread", () => {
  it("marks a session written after this device last read it", () => {
    const now = Date.now();
    markSessionSeen("s1", now - 10 * 60 * SECOND);
    expect(isSessionUnread("s1", secondsAt(now), now)).toBe(true);
  });

  it("clears once the session is read again", () => {
    const now = Date.now();
    markSessionSeen("s1", now - 10 * 60 * SECOND);
    markSessionSeen("s1", now);
    expect(isSessionUnread("s1", secondsAt(now), now)).toBe(false);
  });

  it("tolerates the daemon's second-resolution timestamp", () => {
    // The regression this exists for: a reply persisted at 12:00:00.900 reads
    // back as 12:00:00, and the mark made a moment later at 12:00:01.100 would
    // look older than it. Reading an answer as it arrives must not leave the
    // row asking to be read.
    const written = 1_700_000_000_900;
    markSessionSeen("s1", written + 200);
    expect(isSessionUnread("s1", secondsAt(written), written + 300)).toBe(false);
  });

  it("keeps sessions independent", () => {
    const now = Date.now();
    markSessionSeen("read", now);
    markSessionSeen("stale", now - 10 * 60 * SECOND);
    expect(isSessionUnread("read", secondsAt(now), now)).toBe(false);
    expect(isSessionUnread("stale", secondsAt(now), now)).toBe(true);
  });

  it("treats conversations older than the first launch as read", () => {
    // Otherwise installing the app lights a dot on every conversation in the
    // history, which says nothing except "you have used Borg before".
    const now = Date.now();
    // The first call is what starts tracking.
    isSessionUnread("anything", secondsAt(now), now);
    expect(isSessionUnread("old", secondsAt(now - 30 * 60 * SECOND), now)).toBe(false);
  });

  it("marks a never-opened session that changed after tracking began", () => {
    // The case the feature exists for: Borg answers on a schedule, or in a
    // channel, and the conversation appears already having something to read.
    const start = Date.now();
    isSessionUnread("anything", secondsAt(start), start);
    const later = start + 60 * SECOND;
    expect(isSessionUnread("fresh", secondsAt(later), later)).toBe(true);
  });
});

describe("markSessionSeen", () => {
  it("wakes subscribers so the list beside the conversation repaints", () => {
    const listener = vi.fn();
    subscribeUnread(listener);
    markSessionSeen("s1", Date.now());
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("does not wake subscribers when the mark did not move backwards", () => {
    // The mark is renewed on every streamed cell, so a no-op has to be free or
    // each token repaints every session list on screen.
    const now = Date.now();
    markSessionSeen("s1", now);
    const listener = vi.fn();
    subscribeUnread(listener);
    markSessionSeen("s1", now - SECOND);
    expect(listener).not.toHaveBeenCalled();
    expect(unreadVersion()).toBeGreaterThanOrEqual(0);
  });

  it("ignores a session with no id yet", () => {
    // A conversation opened at `/c/new` has no id until the daemon answers.
    const listener = vi.fn();
    subscribeUnread(listener);
    markSessionSeen("", Date.now());
    expect(listener).not.toHaveBeenCalled();
  });

  it("survives a reload", () => {
    const now = Date.now();
    markSessionSeen("s1", now);
    // What a fresh page load does: in-memory state gone, localStorage intact.
    const stored = localStorage.getItem(STORAGE_KEYS.unread);
    expect(stored).not.toBeNull();
    resetUnread();
    localStorage.setItem(STORAGE_KEYS.unread, stored!);
    expect(isSessionUnread("s1", secondsAt(now), now)).toBe(false);
  });

  it("starts tracking from now when storage holds junk", () => {
    // A dot on every row is the failure mode to avoid; losing the marks is not.
    localStorage.setItem(STORAGE_KEYS.unread, "{not json");
    const now = Date.now();
    expect(isSessionUnread("s1", secondsAt(now - 60 * SECOND), now)).toBe(false);
  });
});
