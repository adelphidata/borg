import { afterEach, describe, expect, it, vi } from "vitest";
import {
  archivedMessage,
  clearNotice,
  offerRestore,
  pendingNotice,
  showNotice,
  subscribeNotice,
  takeNotice,
} from "./notice";

afterEach(() => {
  clearNotice();
});

describe("the shell's notice slot", () => {
  it("holds the newest notice only", () => {
    // Two toasts would mean two "Undo" buttons with nothing to tell them
    // apart. The earlier action stays done — that is what was asked for.
    offerRestore("s1", "One");
    offerRestore("s2", "Two");

    expect(pendingNotice()?.sessionId).toBe("s2");
    expect(pendingNotice()?.message).toBe("Archived “Two”");
  });

  it("gives every notice its own identity, even when they read the same", () => {
    // Two untitled conversations produce identical words. The toast keys its
    // dismissal clock on the id, so without this the second offer would
    // inherit the first one's nearly-expired deadline.
    offerRestore("s1", undefined);
    const first = pendingNotice();
    offerRestore("s2", undefined);
    const second = pendingNotice();

    expect(first?.message).toBe(second?.message);
    expect(second?.id).not.toBe(first?.id);
  });

  it("names an untitled conversation something a person can read", () => {
    expect(archivedMessage(undefined)).toBe("Archived “conversation”");
    expect(archivedMessage("")).toBe("Archived “conversation”");
    expect(archivedMessage("Trip planning")).toBe("Archived “Trip planning”");
  });

  it("carries the session to restore, not a callback to run", () => {
    // The page that archived is often unmounted by the time Undo is pressed,
    // so the offer names the work and the shell does it.
    offerRestore("s1", "One");
    const taken = takeNotice();
    expect(taken?.sessionId).toBe("s1");
  });

  it("empties the slot as the offer is taken", () => {
    // A double-click on Undo must not un-archive twice — the second press has
    // to find an empty slot.
    offerRestore("s1", "One");
    expect(takeNotice()?.sessionId).toBe("s1");
    expect(takeNotice()).toBeUndefined();
    expect(pendingNotice()).toBeUndefined();
  });

  it("offers no way back on a notice that only reports", () => {
    // A failure has nothing to undo; the toast must not show a dead button.
    showNotice("Could not bring the conversation back: database is locked");
    expect(pendingNotice()?.sessionId).toBeUndefined();
    expect(pendingNotice()?.message).toContain("database is locked");
  });

  it("notifies subscribers when the slot fills and empties", () => {
    const listener = vi.fn();
    const unsubscribe = subscribeNotice(listener);

    offerRestore("s1", "One");
    expect(listener).toHaveBeenCalledTimes(1);
    clearNotice();
    expect(listener).toHaveBeenCalledTimes(2);

    // Clearing an empty slot is not a change, and re-notifying would make
    // `useSyncExternalStore` re-render the whole shell for nothing.
    clearNotice();
    expect(listener).toHaveBeenCalledTimes(2);

    unsubscribe();
    offerRestore("s2", "Two");
    expect(listener).toHaveBeenCalledTimes(2);
  });

  it("keeps a stable snapshot between changes", () => {
    // `useSyncExternalStore` compares by identity; a fresh object per read
    // would loop the render.
    offerRestore("s1", "One");
    expect(pendingNotice()).toBe(pendingNotice());
  });
});
