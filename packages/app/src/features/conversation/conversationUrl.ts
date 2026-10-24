import { startTransition, useEffect, useState } from "react";

/**
 * Points the address bar of a conversation opened at `/c/new` at the session
 * it became, so a reload or a copied link reopens that conversation instead
 * of an empty one.
 *
 * The router is not told: a navigation would remount the page mid-turn. Its
 * own location stays `/c/new` until the next navigation, and Back still lands
 * on this entry, now by its real id. Nothing changes once the address has
 * moved on to another page.
 */
export function adoptConversationUrl(
  agentSessionId: string,
  win: Pick<Window, "history" | "location"> = window,
): void {
  if (!agentSessionId || !/^#\/c\/new(?:[?#]|$)/.test(win.location.hash)) return;
  win.history.replaceState(win.history.state, "", `#/c/${encodeURIComponent(agentSessionId)}`);
}

/**
 * Counts the browser's own navigations: Back, Forward, or an address typed by
 * hand. A page first loaded at `/c/new` and an address typed later both carry
 * the router's "default" key, so `location.key` alone cannot tell them apart
 * once the page has adopted its session id.
 */
export function usePopCount(
  win: Pick<Window, "addEventListener" | "removeEventListener"> = window,
): number {
  const [pops, setPops] = useState(0);
  useEffect(() => {
    // In the router's own transition, so the new key and the new location
    // commit together: a key alone would remount the page on the old route.
    const onPop = () => startTransition(() => setPops((n) => n + 1));
    win.addEventListener("popstate", onPop);
    return () => win.removeEventListener("popstate", onPop);
  }, [win]);
  return pops;
}
