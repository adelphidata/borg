import { useEffect, useRef, useState } from "react";

/**
 * One line of text that ellipsizes, and reveals the rest on hover.
 *
 * The tip is the legitimate kind: a clipped title is text the screen is
 * withholding, so reading it back is the only way to learn it. It appears only
 * when the text really is clipped — a title that fits gets no tip, because a
 * label repeated to itself teaches nothing (see this package's `AGENTS.md`).
 *
 * Clipping is measured rather than guessed from length: the same 40 characters
 * fit or do not depending on the glyphs, the pane width, and the font the user
 * ended up with.
 */
export function Truncated({ children, className = "" }: { children: string; className?: string }) {
  const ref = useRef<HTMLSpanElement>(null);
  const [clipped, setClipped] = useState(false);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => setClipped(el.scrollWidth > el.clientWidth + 1);
    measure();
    // The width that clips is the container's, and a sidebar or pane can change
    // it without the text changing at all.
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [children]);

  return (
    <span
      ref={ref}
      // `title` rather than a painted tip: the browser already knows how to
      // place one, delay it, and keep it on screen, and a hand-rolled one is a
      // popover layer to maintain for a string.
      title={clipped ? children : undefined}
      className={`overflow-hidden text-ellipsis whitespace-nowrap ${className}`}
    >
      {children}
    </span>
  );
}
