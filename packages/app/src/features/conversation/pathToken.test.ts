import { describe, expect, it } from "vitest";
import { backspacePathToken, pathToken, replacePathToken } from "./pathToken";

describe("pathToken", () => {
  it("offers completions for a bare @ so the menu can teach what is there", () => {
    expect(pathToken("@", 1)).toEqual({ query: "", start: 0 });
  });

  it("reads the prefix typed after the @", () => {
    expect(pathToken("@src/lib", 8)).toEqual({ query: "src/lib", start: 0 });
  });

  it("triggers mid-sentence, because most file references are not the first word", () => {
    const draft = "compare @src/a.rs";
    expect(pathToken(draft, draft.length)).toEqual({ query: "src/a.rs", start: 8 });
  });

  it("stays shut on an email address", () => {
    // No whitespace before the @, so it is part of a word, not a reference.
    expect(pathToken("ask bob@example.com", 19)).toBeNull();
  });

  it("stays shut once the reference ends, so prose after it is prose", () => {
    expect(pathToken("@src/a.rs and then", 18)).toBeNull();
  });

  it("completes the token the caret is in, not the rest of the line", () => {
    // Caret sits right after "@src", ahead of text already typed.
    expect(pathToken("@src and more", 4)).toEqual({ query: "src", start: 0 });
  });

  it("does not open on an @ the caret has not reached", () => {
    expect(pathToken("later @src", 5)).toBeNull();
  });
});

describe("replacePathToken", () => {
  it("writes the path and leaves the caret where the sentence carries on", () => {
    const token = pathToken("read @src", 9)!;
    const next = replacePathToken("read @src", token.start, 9, "src/main.rs");
    expect(next.text).toBe("read @src/main.rs ");
    expect(next.caret).toBe(next.text.length);
  });

  it("keeps the text that follows the token", () => {
    const draft = "read @src and stop";
    const token = pathToken(draft, 9)!;
    const next = replacePathToken(draft, token.start, 9, "src/main.rs");
    expect(next.text).toBe("read @src/main.rs  and stop");
    expect(next.caret).toBe("read @src/main.rs ".length);
  });

  it("leaves a directory open, because the next thing wanted is inside it", () => {
    const next = replacePathToken("@sr", 0, 3, "src/");
    expect(next.text).toBe("@src/");
    // No trailing space: the token is still live, so the menu can go deeper.
    expect(pathToken(next.text, next.caret)).toEqual({ query: "src/", start: 0 });
  });
});

describe("backspacePathToken", () => {
  it("removes a chosen path whole, not one directory character at a time", () => {
    const draft = "read @src/main.rs";
    expect(backspacePathToken(draft, draft.length)).toEqual({ text: "read ", caret: 5 });
  });

  it("leaves the text after the token alone", () => {
    const draft = "read @src/main.rs and stop";
    expect(backspacePathToken(draft, 17)).toEqual({ text: "read  and stop", caret: 5 });
  });

  it("gives Backspace back once the caret leaves the token", () => {
    expect(backspacePathToken("read @src/main.rs and", 21)).toBeNull();
  });

  it("does not claim Backspace for a bare @, which is one character anyway", () => {
    expect(backspacePathToken("read @", 6)).toBeNull();
  });

  it("does not fire in ordinary prose", () => {
    expect(backspacePathToken("hello world", 11)).toBeNull();
  });
});
