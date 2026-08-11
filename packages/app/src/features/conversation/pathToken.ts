/**
 * When an `@` in the composer means "which file?", and how the path it writes
 * behaves once it is there.
 *
 * Unlike `/`, this one has to work mid-sentence — "compare @src/a.rs with
 * @src/b.rs" is the whole point — so it triggers at a word boundary rather than
 * only in the leading position. That boundary is what keeps an email address
 * out: `ask bob@example.com` has no space before the `@`, so nothing opens.
 *
 * A chosen path stays plain text. The token is atomic only in how it is edited:
 * Backspace at its end takes the path away whole, rather than unpicking a
 * directory a character at a time. Sending needs no serialisation step, because
 * what is on screen is already what the agent receives — a chip that had to be
 * flattened first is a chip that can be flattened wrongly.
 */

/** A trailing `@…` run at the caret, anchored to the start or to whitespace. */
const AT_TOKEN = /(?:^|\s)@(\S*)$/;

/** A whole `@…` run ending at the caret, for Backspace to remove in one go. */
const AT_TOKEN_START = /(?:^|\s)(@\S*)$/;

export interface PathToken {
  /** The text after the `@`, which is the prefix to complete. */
  query: string;
  /** Index of the `@` in the draft, so a replacement knows what to overwrite. */
  start: number;
}

/**
 * The `@` token the caret sits in, or null when there is none.
 *
 * Only text before the caret counts. Typing `@src` in front of an existing
 * sentence should offer completions for `src`, not for everything to the end of
 * the line.
 */
export function pathToken(draft: string, caret: number): PathToken | null {
  const before = draft.slice(0, caret);
  const match = AT_TOKEN.exec(before);
  if (!match) return null;
  const query = match[1] ?? "";
  return { query, start: caret - query.length - 1 };
}

/** The draft and caret after a chosen path replaces the token at `start`. */
export function replacePathToken(
  draft: string,
  start: number,
  caret: number,
  path: string,
): { text: string; caret: number } {
  // The trailing space both ends the token — so the menu closes rather than
  // reopening on the path just chosen — and puts the caret where the sentence
  // carries on. A directory keeps its slash instead, because the next thing
  // anyone types after picking one is what is inside it.
  const tail = path.endsWith("/") ? "" : " ";
  const written = `@${path}${tail}`;
  return {
    text: draft.slice(0, start) + written + draft.slice(caret),
    caret: start + written.length,
  };
}

/**
 * The draft and caret after Backspace removes a whole `@…` token, or null when
 * the caret is not at the end of one and Backspace means what it always means.
 */
export function backspacePathToken(
  draft: string,
  caret: number,
): { text: string; caret: number } | null {
  const match = AT_TOKEN_START.exec(draft.slice(0, caret));
  // A bare `@` is one character: removing it "whole" is what Backspace already
  // does, and claiming the key for it only risks getting it wrong.
  if (!match || (match[1] ?? "").length < 2) return null;
  const start = caret - (match[1] ?? "").length;
  return { text: draft.slice(0, start) + draft.slice(caret), caret: start };
}
