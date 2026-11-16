import { useCallback, useEffect, useRef, useState } from "react";
import { clearRanges, findRanges, paintRanges, revealRange, wrapIndex } from "./findInTranscript";

export interface FindInTranscript {
  /** True while the bar is on screen. */
  open: boolean;
  query: string;
  setQuery: (query: string) => void;
  /** How many matches the mounted transcript holds. */
  count: number;
  /** Which match is active, 1-based for display; 0 when there are none. */
  position: number;
  next: () => void;
  previous: () => void;
  /** Opens the bar, or hands focus back to it when it is already open. */
  show: () => void;
  close: () => void;
  /** Goes up on every `show`; the bar takes focus again when it changes. */
  focusRequest: number;
}

/**
 * Find-as-you-type over the mounted transcript.
 *
 * Matches are recomputed whenever the query changes or the transcript does —
 * a `Range` into a text node React has since replaced is dead, and a reply
 * streaming in replaces one on every token. `revision` is the caller's signal
 * that the DOM moved; passing the cell count is enough.
 *
 * The active match is kept by index rather than by holding the `Range`, for the
 * same reason: the index survives a recompute and the range does not.
 */
export function useFindInTranscript(
  scope: React.RefObject<HTMLElement | null>,
  revision: unknown,
): FindInTranscript {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [count, setCount] = useState(0);
  const [at, setAt] = useState(0);
  const [focusRequest, setFocusRequest] = useState(0);
  const matches = useRef<Range[]>([]);

  // Derives the ranges for a query and records how many there are. A query
  // that narrowed to nothing takes its predecessor's paint with it, or the bar
  // says "None" over highlights still lit from the last one.
  const derive = (next: string) => {
    const root = scope.current;
    const found = next === "" || !root ? [] : findRanges(root, next);
    matches.current = found;
    setCount(found.length);
    if (found.length === 0) clearRanges();
    return found;
  };
  // The effect below re-derives when the transcript changes, so it needs the
  // query without being keyed on it: a keystroke derives in its own handler.
  // Deriving keystrokes in an effect would set state after every commit, which
  // React counts as a nested update and, typed fast enough, cuts off with
  // "Maximum update depth exceeded".
  const queryRef = useRef(query);
  queryRef.current = query;

  // Re-derives when the bar opens or the transcript underneath it changes, and
  // scrubs the paint when the bar closes. Splitting the paint from the derive
  // is how a stale highlight outlives the thing that made it.
  useEffect(() => {
    if (!open || queryRef.current === "") {
      matches.current = [];
      setCount(0);
      clearRanges();
      return;
    }
    const found = derive(queryRef.current);
    setAt((current) => wrapIndex(current, found.length));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, revision, scope]);

  // Painting reads `at`, so it is a second pass over the ranges derived above —
  // stepping between matches must not re-walk the whole transcript. `revision`
  // and `query` are dependencies too: a re-render that happens to leave the
  // count and position unchanged still hands this a fresh set of ranges, and
  // the registry would otherwise keep painting the dead ones.
  useEffect(() => {
    if (!open || matches.current.length === 0) return;
    const current = matches.current[wrapIndex(at, matches.current.length)];
    paintRanges(matches.current, current);
    if (current) revealRange(current, scope.current?.parentElement ?? null);
  }, [open, at, count, query, revision, scope]);

  useEffect(() => clearRanges, []);

  const step = useCallback((by: number) => {
    setAt((current) => wrapIndex(current + by, matches.current.length));
  }, []);

  const close = useCallback(() => {
    setOpen(false);
    setQuery("");
    setAt(0);
    clearRanges();
  }, []);

  return {
    open,
    query,
    setQuery: (next) => {
      setQuery(next);
      // A new query starts from the first match, not from wherever the last one
      // left off — the old position means nothing in the new results.
      setAt(0);
      // The transcript has not changed under a keystroke, so its ranges can be
      // derived here, in the same batch as the query itself.
      if (open) derive(next);
    },
    count,
    position: count === 0 ? 0 : wrapIndex(at, count) + 1,
    next: () => step(1),
    previous: () => step(-1),
    show: () => {
      setOpen(true);
      // Opening is a no-op on a bar that is already open, so the second ⌘F
      // needs its own signal to bring the cursor back.
      setFocusRequest((n) => n + 1);
    },
    close,
    focusRequest,
  };
}
