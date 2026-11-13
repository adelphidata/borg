import { useCallback, useEffect, useRef, useState } from "react";
import { asBorgError, useBorg } from "../../lib/useDaemon";

/** How long typing has to pause before the notes are saved. */
export const NOTES_DEBOUNCE_MS = 600;

export interface NotesDraft {
  text: string;
  change: (next: string) => void;
  /** Saves now whatever is waiting on the debounce. */
  flush: () => void;
  /**
   * Renames the meeting. `UpdateNotes` replaces the notes too, so the rename
   * carries the box's current text and goes through the same queue as the
   * saves — a rename can never put older notes back.
   */
  rename: (title: string) => Promise<boolean>;
  /** Why the last save failed, in words for the user. */
  error: string | undefined;
}

/**
 * The user's notes for one meeting, saved as they type.
 *
 * One `UpdateNotes` per pause rather than per keystroke, and the pending text
 * is flushed on blur and on unmount so closing the page never loses the last
 * words. Saves go out one at a time in the order they were made, so the last
 * thing typed is the last thing stored.
 *
 * `saved` is the daemon's copy. It seeds the box once it arrives; after that
 * the box is the source of truth, because a re-read while someone is typing
 * would otherwise throw their newest words away.
 */
export function useNotesDraft(meetingId: string, saved: string | undefined): NotesDraft {
  const borg = useBorg();
  const [text, setText] = useState(saved ?? "");
  const [error, setError] = useState<string>();
  const latest = useRef(saved ?? "");
  const seeded = useRef(saved !== undefined);
  const pending = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const queue = useRef<Promise<unknown>>(Promise.resolve());

  useEffect(() => {
    if (seeded.current || saved === undefined) return;
    seeded.current = true;
    latest.current = saved;
    setText(saved);
  }, [saved]);

  const write = useCallback(
    (title?: string): Promise<boolean> => {
      clearTimeout(timer.current);
      timer.current = undefined;
      pending.current = false;
      const notes = latest.current;
      const done = queue.current.then(() =>
        borg.meetings
          .updateNotes({ meetingId, notes, ...(title === undefined ? {} : { title }) })
          .then(
            () => {
              setError(undefined);
              return true;
            },
            (e: unknown) => {
              setError(`Your notes weren't saved: ${asBorgError(e).message}`);
              return false;
            },
          ),
      );
      queue.current = done;
      return done;
    },
    [borg, meetingId],
  );

  const flush = useCallback(() => {
    if (pending.current) void write();
  }, [write]);

  const flushRef = useRef(flush);
  flushRef.current = flush;
  useEffect(() => () => flushRef.current(), []);

  const change = useCallback(
    (next: string) => {
      latest.current = next;
      pending.current = true;
      setText(next);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => void write(), NOTES_DEBOUNCE_MS);
    },
    [write],
  );

  return { text, change, flush, rename: write, error };
}
