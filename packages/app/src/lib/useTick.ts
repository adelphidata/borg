import { useEffect, useState } from "react";

/**
 * A clock for anything that counts up on screen.
 *
 * Returns the current time, re-rendering the caller every `intervalMs` while
 * `enabled`. Off by default and stopped the moment nothing is counting, because
 * a timer running behind a settled screen costs a render a second for nothing.
 * A hidden tab is treated as not counting: the value is stale while nobody can
 * read it and correct again on the first tick after it comes back.
 */
export function useTick(intervalMs: number, enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!enabled) return;
    setNow(Date.now());
    const id = setInterval(() => {
      if (typeof document !== "undefined" && document.hidden) return;
      setNow(Date.now());
    }, intervalMs);
    return () => clearInterval(id);
  }, [enabled, intervalMs]);

  return now;
}
