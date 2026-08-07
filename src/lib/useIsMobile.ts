import { useSyncExternalStore } from "react";

// Sole mobile breakpoint — the layout swap is JS-driven; app.css has no matching media query.
const QUERY = "(max-width: 700px)";

function subscribe(callback: () => void) {
  const mql = window.matchMedia(QUERY);
  mql.addEventListener("change", callback);
  return () => mql.removeEventListener("change", callback);
}

/** True when the window is phone-sized; the shell swaps sidebar for bottom tabs. */
export function useIsMobile(): boolean {
  return useSyncExternalStore(subscribe, () => window.matchMedia(QUERY).matches);
}
