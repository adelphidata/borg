/**
 * Turns a failed send into a sentence that names the next action.
 *
 * A raw daemon error is written for a log, not for the person who just lost a
 * message: `SQLITE_FULL: database or disk is full` says nothing about what to
 * do, and a full disk is the one failure a user can actually fix. Anything not
 * recognised here keeps its original text — a vague guess would be worse than
 * the real error, which at least a maintainer can read.
 */

interface Cause {
  /** Matched case-insensitively against the error text. */
  markers: readonly string[];
  message: string;
}

const CAUSES: readonly Cause[] = [
  {
    // libsql, the OS, and the filesystem each phrase this differently, and the
    // message reaches us as whichever layer noticed first.
    markers: ["enospc", "sqlite_full", "disk is full", "disk full", "no space left"],
    message: "Borg could not save the message — the disk is full. Free up space, then send again.",
  },
  {
    markers: ["read-only file system", "erofs", "attempt to write a readonly database"],
    message:
      "Borg could not save the message — its data folder is read-only. Check the permissions on ~/.borg, then send again.",
  },
];

/** What to show the user for a send that failed. */
export function describeSendFailure(error: { message: string } | undefined): string | undefined {
  if (!error) return undefined;
  const haystack = error.message.toLowerCase();
  const cause = CAUSES.find((c) => c.markers.some((marker) => haystack.includes(marker)));
  return cause ? cause.message : error.message;
}
