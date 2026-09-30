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

describe("word separators", () => {
  it("ranks a hyphenated name first for the same words typed with a space", () => {
    const results = rank(
      [cmd("Core overview"), cmd("Decode previews"), cmd("code-review")],
      "code review",
    );
    expect(results[0]!.label).toBe("code-review");
  });

  const pairs: [string, string][] = [
    ["gpt-4o", "gpt.4o"],
    ["gpt-4o", "gpt 4o"],
    ["gpt-4o", "gpt_4o"],
    ["tts.timeout_ms", "tts timeout ms"],
    ["code review", "code-review"],
  ];

  it.each(pairs)("scores %s the same for %s as for its own spelling", (text, query) => {
    const own = score(text, text);
    expect(own).not.toBeNull();
    expect(score(text, query)).toBe(own);
  });

  it("still tells a separator from a letter", () => {
    // Folding is between separators only: "gpt4o" has no gap to match.
    expect(score("gpt4o", "gpt 4o")).toBeNull();
  });

  it("leaves the score of a query without separators as it was", () => {
    // 8 for a word-initial first letter, less 5 for the distance to it, 5 for
    // each letter that follows it, less 0.05 per character of the text.
    expect(score("llm.thinking", "think")).toBeCloseTo(8 - 5 + 5 * 4 - 12 * 0.05, 10);
    expect(score("code-review", "review")).toBe(score("code review", "review"));
  });
});
