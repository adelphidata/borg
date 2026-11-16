/**
 * Properties shared by the web and desktop surfaces.
 *
 * Every export here is a `Formula`; the top-level specifications re-export
 * this module wholesale, so nothing else may live in it.
 */
import { always, eventually, next, now } from "@antithesishq/bombadil";
import { isAction } from "@antithesishq/bombadil/actions";
import { lastAction } from "@antithesishq/bombadil/browser/defaults/actions";
import { noResourceLeak } from "@antithesishq/bombadil/browser/extras/resources";
import { ESCAPE } from "./keys.ts";
import { MOBILE_MAX_WIDTH, expectedTheme, overlays, page, shellRendered } from "./state.ts";

/** The routed page never falls through to the error boundary. */
export const noPageCrash = always(() => !page.current.crashed);

/**
 * A screen with nothing to press is a dead end. The gate before the shell
 * renders nothing for a moment, so this is a guarantee, not an invariant.
 */
export const neverStuckWithoutAnAction = always(
  now(() => page.current.actionable === 0).implies(
    eventually(() => page.current.actionable > 0).within(10, "seconds"),
  ),
);

/** A loading state against a local daemon settles. */
export const loadingSettles = always(
  now(() => page.current.loading).implies(
    eventually(() => !page.current.loading).within(15, "seconds"),
  ),
);

/** The daemon is up for the whole run, so a lost-connection banner is a client bug. */
export const noLostConnectionBanner = always(() => !page.current.lostConnection);

/** Values reach the screen formatted, never as `undefined`, `NaN` or `[object Object]`. */
export const noRawValuesOnScreen = always(() => !page.current.rawValueOnScreen);

/** The sidebar and the tab bar are exclusive and follow the viewport width. */
export const layoutFollowsViewport = always(() => {
  const p = page.current;
  if (!shellRendered()) return true;
  return p.width <= MOBILE_MAX_WIDTH ? !p.sidebar && p.tabs > 0 : p.sidebar && p.tabs === 0;
});

/** Five tabs is the ceiling on a phone. */
export const tabBarCeiling = always(() => page.current.tabs <= 5);

/** The nav marks the page that is showing, and only that page. */
function navMatchesRoute(): boolean {
  const p = page.current;
  if (p.activeSidebarLinks.length > 1 || p.activeTabLinks.length > 1) return false;
  const hash = p.hash || "#/";
  return [...p.activeSidebarLinks, ...p.activeTabLinks].every((href) =>
    href === "#/" ? hash === "#/" || hash.startsWith("#/c/") : hash.startsWith(href),
  );
}

/**
 * The route changes before the nav re-renders, so a mismatch is allowed for a
 * moment; one that lasts is a stale highlight.
 */
export const activeNavSettlesOnRoute = always(
  now(() => !navMatchesRoute()).implies(eventually(navMatchesRoute).within(3, "seconds")),
);

/** Nothing scrolls sideways, at any viewport width. */
export const noHorizontalOverflow = always(() => page.current.scrollWidth <= page.current.width);

/** One notice slot: a new toast replaces the old one. */
export const atMostOneToast = always(() => page.current.toasts <= 1);

/** One dialog at a time; a second one is a focus and Escape conflict. */
export const atMostOneDialog = always(() => page.current.dialogs <= 1);

/** One visible heading names the page. */
export const atMostOneHeading = always(() => page.current.h1s <= 1);

/** The painted theme follows the stored preference by the app's own rules. */
export const themeFollowsPreference = always(() => {
  const p = page.current;
  return p.theme !== null && p.theme === expectedTheme();
});

/** An open dialog holds keyboard focus; a click that closes it may take a moment to land. */
export const dialogHoldsFocus = always(
  now(() => page.current.focusInDialog === false).implies(
    eventually(() => page.current.focusInDialog !== false).within(2, "seconds"),
  ),
);

/** Escape closes the topmost overlay. */
export const escapeClosesOverlay = always(() => {
  const before = overlays();
  return now(() => before > 0)
    .and(
      next(() => {
        const action = lastAction.current;
        return action !== null && isAction(action, "PressKey") && action.PressKey.code === ESCAPE;
      }),
    )
    .implies(next(() => overlays() < before));
});

/** The chat routes: the root resumes the latest conversation, `/c/…` names one. */
function onConversation(hash: string): boolean {
  return hash === "" || hash === "#/" || hash.startsWith("#/c/");
}

/** A draft typed into the composer is still there after a reload. */
export const draftSurvivesReload = always(() => {
  const { composer: draft, hash } = page.current;
  return now(() => onConversation(hash) && draft !== null && draft.trim() !== "")
    .and(next(() => lastAction.current === "Reload" && onConversation(page.current.hash)))
    .implies(next(eventually(() => page.current.composer === draft).within(5, "seconds")));
});

/**
 * Runaway growth, not churn: a keystroke in the composer re-creates a few
 * dozen listeners that the next GC reclaims, so the windows are long and the
 * limits well above anything a page legitimately builds.
 */
export const noDomLeak = noResourceLeak({
  metric: "dom_nodes",
  growthLimit: 15_000,
  windowMillis: 10_000,
});

export const noListenerLeak = noResourceLeak({
  metric: "js_event_listeners",
  growthLimit: 20_000,
  windowMillis: 10_000,
});
