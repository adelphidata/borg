import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { draftKeyFor, publishDraft } from "../conversation/draftStore";
import {
  activityVersion,
  applyActivity,
  isSessionWorking,
  replaceActivity,
  resetActivity,
  sessionActivity,
  subscribeActivity,
  sessionWorkingSince,
} from "./activityStore";
import { markSessionSeen, resetUnread } from "./unreadStore";

/**
 * A session last written long before this device started tracking reads, so it
 * is settled rather than unread. Most of these cases are about the other two
 * marks and only need the unread one out of the way.
 */
const LONG_AGO = 1_600_000_000;

beforeEach(() => {
  resetActivity();
  resetUnread();
});

afterEach(() => {
  resetActivity();
  resetUnread();
});

describe("applyActivity", () => {
  it("tracks a session from start to settle", () => {
    applyActivity("s1", true);
    expect(isSessionWorking("s1")).toBe(true);
    applyActivity("s1", false);
    expect(isSessionWorking("s1")).toBe(false);
  });

  it("keeps sessions independent", () => {
    applyActivity("s1", true);
    applyActivity("s2", true);
    applyActivity("s1", false);
    expect(isSessionWorking("s1")).toBe(false);
    expect(isSessionWorking("s2")).toBe(true);
  });

  it("does not wake subscribers when nothing changed", () => {
    // Every subscriber re-renders a session list, so a repeated transition
    // (the daemon settles a host that already drained) must be free.
    const listener = vi.fn();
    subscribeActivity(listener);
    applyActivity("s1", true);
    expect(listener).toHaveBeenCalledTimes(1);
    applyActivity("s1", true);
    expect(listener).toHaveBeenCalledTimes(1);
  });
});

describe("replaceActivity", () => {
  it("clears a dot the feed never got to settle", () => {
    // The reconnect case: the turn ended while the stream was down, so the
    // only thing that can correct the dot is the next listing.
    applyActivity("stranded", true);
    replaceActivity([]);
    expect(isSessionWorking("stranded")).toBe(false);
  });

  it("lights a session that started working while disconnected", () => {
    replaceActivity(["s9"]);
    expect(isSessionWorking("s9")).toBe(true);
  });

  it("does not wake subscribers when the set is unchanged", () => {
    // A listing is refetched on every navigation; an identical snapshot must
    // not repaint every list.
    applyActivity("s1", true);
    const listener = vi.fn();
    subscribeActivity(listener);
    replaceActivity(["s1"]);
    expect(listener).not.toHaveBeenCalled();
  });

  it("bumps the version when the set really changes", () => {
    const before = activityVersion();
    replaceActivity(["s1"]);
    expect(activityVersion()).toBeGreaterThan(before);
  });
});

describe("sessionActivity", () => {
  it("reports a running turn", () => {
    applyActivity("s1", true);
    expect(sessionActivity("s1", 4, LONG_AGO)).toBe("working");
  });

  it("reports an unsent draft", () => {
    publishDraft(draftKeyFor("s2"), "half a thought");
    expect(sessionActivity("s2", 0, LONG_AGO)).toBe("draft");
  });

  it("prefers the running turn over the draft", () => {
    // Same conversation open in two windows: one is typing, the other is
    // waiting on a turn. The turn is the news.
    publishDraft(draftKeyFor("s3"), "typing here");
    applyActivity("s3", true);
    expect(sessionActivity("s3", 0, LONG_AGO)).toBe("working");
  });

  it("does not call a conversation with history a draft", () => {
    publishDraft(draftKeyFor("s4"), "a follow-up I have not sent");
    expect(sessionActivity("s4", 6, LONG_AGO)).toBeUndefined();
  });

  it("ignores whitespace typed into an empty composer", () => {
    publishDraft(draftKeyFor("s5"), "   \n  ");
    expect(sessionActivity("s5", 0, LONG_AGO)).toBeUndefined();
  });

  it("marks nothing on a settled conversation", () => {
    expect(sessionActivity("s6", 3, LONG_AGO)).toBeUndefined();
  });

  it("reports a reply that landed since this device last looked", () => {
    const now = Math.floor(Date.now() / 1000);
    markSessionSeen("s7", { ts: now - 60, count: 2 });
    expect(sessionActivity("s7", 4, now)).toBe("unread");
  });

  it("prefers the running turn over the unread mark", () => {
    // Mid-turn the row should say Borg is working, not that there is something
    // to read: the answer is still arriving.
    const now = Math.floor(Date.now() / 1000);
    markSessionSeen("s8", { ts: now - 60, count: 2 });
    applyActivity("s8", true);
    expect(sessionActivity("s8", 4, now)).toBe("working");
  });

  it("prefers the unread mark over the draft", () => {
    // A conversation with an unsent line in its composer that Borg has since
    // answered: the answer is the news, not the half-typed follow-up.
    const now = Math.floor(Date.now() / 1000);
    publishDraft(draftKeyFor("s9"), "half a thought");
    // Read before any listing covered it, so the timestamp is what decides.
    markSessionSeen("s9", { ts: now - 60, count: -1 });
    expect(sessionActivity("s9", 0, now)).toBe("unread");
  });
});

describe("how long a turn has been running", () => {
  it("reports the daemon's own start time, not when the client heard about it", () => {
    applyActivity("s10", true, 1_700_000_000_000);

    expect(sessionWorkingSince("s10")).toBe(1_700_000_000_000);
  });

  it("falls back to now when the event carried no stamp", () => {
    // An older daemon leaves `emitted_at_unix` at zero; a turn dated 1970 would
    // render as an elapsed time of fifty-six years.
    const before = Date.now();
    applyActivity("s11", true, 0);

    expect(sessionWorkingSince("s11")).toBeGreaterThanOrEqual(before);
  });

  it("keeps a running turn's start time across a reconnect", () => {
    // The regression this guards: the reconcile listing replaces the whole set
    // on every reconnect, and refilling from scratch blinked a long turn's
    // elapsed count back to zero whenever the socket hiccupped.
    applyActivity("s12", true, 1_700_000_000_000);

    replaceActivity(["s12", "s13"]);

    expect(sessionWorkingSince("s12")).toBe(1_700_000_000_000);
    expect(sessionWorkingSince("s13")).toBeGreaterThan(1_700_000_000_000);
  });

  it("forgets a turn that ended", () => {
    applyActivity("s14", true, 1_700_000_000_000);
    applyActivity("s14", false);

    expect(sessionWorkingSince("s14")).toBeUndefined();
  });
});
