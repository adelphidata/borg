import { useEffect, useLayoutEffect, useRef } from "react";

/**
 * Sizes a textarea to its content, up to `maxPx`.
 *
 * Height follows what the text actually occupies rather than a count of line
 * breaks: one long paragraph wraps to many rows and contains no newline at all.
 * Past the cap the box stops growing and scrolls instead.
 *
 * The same text wraps to a different height at a different width, so a resized
 * window or a panel opening beside the box measures it again too.
 */
export function useAutoGrow(value: string, maxPx: number) {
  const ref = useRef<HTMLTextAreaElement>(null);
  // Before paint, so a box that opens already full never shows at the wrong
  // height for a frame.
  useLayoutEffect(() => {
    if (ref.current) fit(ref.current, maxPx);
  }, [value, maxPx]);

  useEffect(() => {
    const el = ref.current;
    // Not every environment the app renders in has it; there the box simply
    // keeps the height it last measured until the text changes.
    if (!el || typeof ResizeObserver === "undefined") return;
    let width = el.getBoundingClientRect().width;
    const observer = new ResizeObserver(() => {
      // Setting the height fires the observer too; only a new width can change
      // how the text wraps, and ignoring the rest is what stops a loop.
      const next = el.getBoundingClientRect().width;
      if (next === width) return;
      width = next;
      fit(el, maxPx);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [maxPx]);
  return ref;
}

function fit(el: HTMLTextAreaElement, maxPx: number) {
  // Collapse first: scrollHeight only shrinks if the box is not already tall.
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, maxPx)}px`;
}
