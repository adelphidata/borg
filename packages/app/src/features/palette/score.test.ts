import { describe, expect, it } from "vitest";
import { rank, score, type Command } from "./score";

const cmd = (label: string, keywords?: string): Command => ({
  id: label,
  label,
  keywords,
  run: () => {},
});

describe("score", () => {
  it("matches a subsequence, not just a substring", () => {
    expect(score("Go to Settings", "gse")).not.toBeNull();
  });

  it("rejects a letter that is absent", () => {
    expect(score("Go to Settings", "zzz")).toBeNull();
  });

  it("rejects letters that are present but out of order", () => {
    // "sog": the s of Settings comes after both o's, so there is no o left.
    expect(score("Go to Settings", "sog")).toBeNull();
  });

  it("treats an empty query as a neutral match", () => {
    expect(score("anything", "")).toBe(0);
  });

  it("is case-insensitive in both directions", () => {
    expect(score("Go To Settings", "SETTINGS")).not.toBeNull();
  });

  it("ranks word-initial letters above letters buried mid-word", () => {
    const initials = score("Go To Settings", "gts")!;
    const buried = score("Go To Settings", "oes")!;
    expect(initials).toBeGreaterThan(buried);
  });

  it("prefers the shorter of two labels matching equally", () => {
    expect(score("Usage", "usage")!).toBeGreaterThan(score("Usage and billing report", "usage")!);
  });
});

describe("rank", () => {
  it("returns every command untouched for a blank query", () => {
    const commands = [cmd("b"), cmd("a")];
    expect(rank(commands, "  ")).toEqual(commands);
  });

  it("drops non-matches and orders the rest by relevance", () => {
    const results = rank([cmd("Health"), cmd("New conversation"), cmd("Usage")], "new");
    expect(results.map((c) => c.label)).toEqual(["New conversation"]);
  });

  it("matches on keywords the user cannot see", () => {
    const results = rank([cmd("New conversation", "chat start fresh")], "chat");
    expect(results).toHaveLength(1);
  });

  it("ranks a visible-label match above an equivalent keyword-only match", () => {
    const results = rank([cmd("Something else", "usage"), cmd("Usage")], "usage");
    expect(results[0]!.label).toBe("Usage");
  });

  it("finds a dotted setting key from its words", () => {
    const results = rank([cmd("llm.thinking", "llm thinking")], "think");
    expect(results).toHaveLength(1);
  });
});
