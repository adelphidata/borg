/**
 * When a `/` in the composer means "show me what Borg can do".
 *
 * Leading position only, and only while the draft is that one unbroken token.
 * A slash anywhere else is ordinary prose — a path, a date, "and/or" — and a
 * menu that opened on those would be worse than no menu at all. The first space
 * ends it, so "/goal ship the thing" is a sentence again.
 */
const SLASH_TOKEN = /^\/(\S*)$/;

/** The query after a leading `/`, or null when the draft is not a slash token. */
export function slashQuery(draft: string): string | null {
  const match = SLASH_TOKEN.exec(draft);
  return match ? (match[1] ?? "") : null;
}
