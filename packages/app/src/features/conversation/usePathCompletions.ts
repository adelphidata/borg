import { useEffect, useRef, useState } from "react";
import { useBorg } from "../../lib/useDaemon";

/** One path the daemon offered. */
export interface PathRow {
  path: string;
  isDir: boolean;
}

/**
 * How long the box stays quiet before asking. Every keystroke inside an `@`
 * token would otherwise be a directory read on the daemon.
 */
const SETTLE_MS = 120;

/** Rows fit a menu, not a file browser; the daemon caps this again anyway. */
const LIMIT = 12;

/**
 * Workspace paths matching `prefix`, or none when there is no `@` token.
 *
 * Deliberately not `useQuery`: this fires on a prefix that changes with every
 * keystroke, so it needs the settle delay and the drop-stale-answers rule that
 * a general query hook has no reason to carry. A slow answer that arrives after
 * the user typed on would otherwise repopulate the menu with the wrong list.
 */
export function usePathCompletions(
  sessionId: string | undefined,
  prefix: string | null,
): PathRow[] {
  const borg = useBorg();
  const [rows, setRows] = useState<PathRow[]>([]);
  // Counts requests so a late answer can tell it has been overtaken.
  const generation = useRef(0);

  useEffect(() => {
    if (sessionId === undefined || prefix === null) {
      setRows([]);
      return;
    }
    const mine = ++generation.current;
    const timer = setTimeout(() => {
      void borg.session
        .listPaths({ sessionId, prefix, limit: LIMIT })
        .then((res) => {
          if (generation.current !== mine) return;
          setRows(res.paths.map((p) => ({ path: p.path, isDir: p.isDir })));
        })
        .catch((error: unknown) => {
          if (generation.current !== mine) return;
          // A completion menu is an offer, not an answer: a daemon that cannot
          // list the workspace should leave the box alone rather than put an
          // error where the file names go. Logged so it is not invisible.
          console.warn("path completion failed", error);
          setRows([]);
        });
    }, SETTLE_MS);
    return () => clearTimeout(timer);
  }, [borg, sessionId, prefix]);

  return rows;
}
