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
  show: () => void;
  close: () => void;
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
  const matches = useRef<Range[]>([]);

  // One effect owns the painted state: it derives the ranges, paints them, and
  // scrubs them when the bar closes. Splitting the paint from the derive is how
  // a stale highlight outlives the thing that made it.
  useEffect(() => {
    if (!open || query === "") {
      matches.current = [];
      setCount(0);
      clearRanges();
      return;
    }
    const root = scope.current;
    const found = root ? findRanges(root, query) : [];
    matches.current = found;
    setCount(found.length);
    setAt((current) => wrapIndex(current, found.length));
    // A query that narrowed to nothing must take its predecessor's paint with
    // it, or the bar says "None" over highlights still lit from the last one.
    if (found.length === 0) clearRanges();
  }, [open, query, revision, scope]);

  // Painting reads `at`, so it is a second pass over the ranges the effect above
  // derived — stepping between matches must not re-walk the whole transcript.
  // `revision` is a dependency here too: a re-render that happens to leave the
  // count unchanged still hands the effect above a fresh set of ranges, and the
  // registry would otherwise keep painting the dead ones.
  useEffect(() => {
    if (!open || matches.current.length === 0) return;
    const current = matches.current[wrapIndex(at, matches.current.length)];
    paintRanges(matches.current, current);
    if (current) revealRange(current, scope.current?.parentElement ?? null);
  }, [open, at, count, revision, scope]);

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
    },
    count,
    position: count === 0 ? 0 : wrapIndex(at, count) + 1,
    next: () => step(1),
    previous: () => step(-1),
    show: () => setOpen(true),
    close,
  };
}
