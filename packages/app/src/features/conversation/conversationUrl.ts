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
