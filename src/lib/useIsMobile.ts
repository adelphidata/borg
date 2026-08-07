import { useSyncExternalStore } from "react";

// Sole mobile breakpoint — the layout swap is JS-driven; app.css has no matching media query.
const QUERY = "(max-width: 700px)";

// One shared MediaQueryList — getSnapshot runs on every render of every consumer,
// so re-parsing the query each time would be pure repeated work.
let mql: MediaQueryList | undefined;
const getMql = () => (mql ??= window.matchMedia(QUERY));

function subscribe(callback: () => void) {
  getMql().addEventListener("change", callback);
  return () => getMql().removeEventListener("change", callback);
}

/** True when the window is phone-sized; the shell swaps sidebar for bottom tabs. */
export function useIsMobile(): boolean {
  return useSyncExternalStore(subscribe, () => getMql().matches);
}
