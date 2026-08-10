import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  draftKeyFor,
  draftTitle,
  draftsVersion,
  moveDraft,
  publishDraft,
  readDraft,
  rowTitle,
  subscribeDrafts,
} from "./draftStore";

beforeEach(() => {
  sessionStorage.clear();
  // The module keeps drafts in memory; clear every key these tests touch.
  for (const key of ["a", "b", "s1", "s2", "new"]) publishDraft(draftKeyFor(key), "");
});

describe("draftTitle", () => {
  it("matches the daemon's rule so the name survives the first send", () => {
    // crates/cortex/src/session.rs: first paragraph, 60 chars, "..." if longer.
    const sixty = "x".repeat(60);
    expect(draftTitle(sixty)).toBe(sixty);
    expect(draftTitle("x".repeat(61))).toBe(`${sixty}...`);
  });

  it("keeps only the first paragraph", () => {
    expect(draftTitle("Fix the parser\n\nSome trailing context")).toBe("Fix the parser");
  });

  it("counts an emoji as one character, not two", () => {
    // A UTF-16 slice would cut this in half at the 60th unit and truncate early.
    const text = `${"🙂".repeat(60)}tail`;
    expect(draftTitle(text)).toBe(`${"🙂".repeat(60)}...`);
  });

  it("trims first, because that is what the composer submits", () => {
    expect(draftTitle("  hello  ")).toBe("hello");
  });

  it("has no name for an empty or whitespace draft", () => {
    expect(draftTitle("")).toBe("");
    expect(draftTitle("   \n  ")).toBe("");
  });
});

describe("publishDraft", () => {
  it("notifies subscribers and bumps the version", () => {
    const listener = vi.fn();
    const unsubscribe = subscribeDrafts(listener);
    const before = draftsVersion();

    publishDraft(draftKeyFor("a"), "hello");

    expect(listener).toHaveBeenCalledTimes(1);
    expect(draftsVersion()).toBeGreaterThan(before);
    unsubscribe();
  });

  it("stays silent when the text has not changed", () => {
    publishDraft(draftKeyFor("a"), "hello");
    const listener = vi.fn();
    const unsubscribe = subscribeDrafts(listener);

    publishDraft(draftKeyFor("a"), "hello");

    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
  });

  it("stops notifying an unsubscribed listener", () => {
    const listener = vi.fn();
    subscribeDrafts(listener)();
    publishDraft(draftKeyFor("a"), "hello");
    expect(listener).not.toHaveBeenCalled();
  });
});

describe("readDraft", () => {
  it("falls back to what survived a reload", () => {
    sessionStorage.setItem(draftKeyFor("a"), "from storage");
    expect(readDraft(draftKeyFor("a"))).toBe("from storage");
  });

  it("prefers the live value over stale storage", () => {
    sessionStorage.setItem(draftKeyFor("a"), "stale");
    publishDraft(draftKeyFor("a"), "typed");
    expect(readDraft(draftKeyFor("a"))).toBe("typed");
  });
});

describe("moveDraft", () => {
  it("re-keys a draft typed before the session had an id", () => {
    publishDraft(draftKeyFor("new"), "hello there");
    sessionStorage.setItem(draftKeyFor("new"), "hello there");

    moveDraft(draftKeyFor("new"), draftKeyFor("s1"));

    // Storage too, not just memory: the composer restores by reading it.
    expect(readDraft(draftKeyFor("s1"))).toBe("hello there");
    expect(sessionStorage.getItem(draftKeyFor("s1"))).toBe("hello there");
    expect(readDraft(draftKeyFor("new"))).toBe("");
    expect(sessionStorage.getItem(draftKeyFor("new"))).toBeNull();
  });

  it("does nothing when there is no draft to carry over", () => {
    moveDraft(draftKeyFor("new"), draftKeyFor("s1"));
    expect(readDraft(draftKeyFor("s1"))).toBe("");
  });

  it("is idempotent, so a repeated render cannot double-move", () => {
    publishDraft(draftKeyFor("new"), "text");
    moveDraft(draftKeyFor("new"), draftKeyFor("s1"));
    publishDraft(draftKeyFor("s1"), "edited since");
    moveDraft(draftKeyFor("new"), draftKeyFor("s1"));
    expect(readDraft(draftKeyFor("s1"))).toBe("edited since");
  });
});

describe("rowTitle", () => {
  it("names an unsent session after what has been typed into it", () => {
    publishDraft(draftKeyFor("s1"), "Rewrite the scheduler");
    expect(rowTitle("New conversation", 0, "s1")).toBe("Rewrite the scheduler");
    expect(rowTitle("", 0, "s1")).toBe("Rewrite the scheduler");
  });

  it("never renames a conversation that has messages", () => {
    publishDraft(draftKeyFor("s1"), "a draft reply");
    expect(rowTitle("New conversation", 4, "s1")).toBe("New conversation");
  });

  it("leaves a real title alone even with a draft pending", () => {
    publishDraft(draftKeyFor("s1"), "a draft reply");
    expect(rowTitle("Deploy notes", 0, "s1")).toBe("Deploy notes");
  });

  it("falls back to the placeholder when the draft is cleared", () => {
    publishDraft(draftKeyFor("s1"), "typed then deleted");
    publishDraft(draftKeyFor("s1"), "");
    expect(rowTitle("", 0, "s1")).toBe("Untitled");
  });

  it("reads each session's own draft", () => {
    publishDraft(draftKeyFor("s1"), "first");
    publishDraft(draftKeyFor("s2"), "second");
    expect(rowTitle("", 0, "s1")).toBe("first");
    expect(rowTitle("", 0, "s2")).toBe("second");
  });
});
