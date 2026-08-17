import { useEffect } from "react";

/** Marks the document so the stylesheet can stop animating an unseen page. */
const HIDDEN_ATTR = "data-hidden";

/**
 * Stops looping animations while the page is not on screen.
 *
 * A minimised window, a background tab, and a locked screen all keep an
 * `infinite` keyframe animation compositing — a pulse nobody can see, costing
 * frames and battery for the whole time a long turn runs. The stylesheet parks
 * every animation while the attribute is set; the browser resumes them mid-cycle
 * when it clears, so nothing restarts or jumps.
 *
 * Visibility, deliberately, not focus: a Borg window sitting beside the editor
 * someone is typing in is still being watched, and a frozen spinner there reads
 * as a hung app.
 */
export function usePauseAnimationsWhenHidden(): void {
  useEffect(() => {
    const sync = () => {
      if (document.hidden) document.documentElement.setAttribute(HIDDEN_ATTR, "true");
      else document.documentElement.removeAttribute(HIDDEN_ATTR);
    };
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => {
      document.removeEventListener("visibilitychange", sync);
      document.documentElement.removeAttribute(HIDDEN_ATTR);
    };
  }, []);
}
