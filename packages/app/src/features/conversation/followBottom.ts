/**
 * Whether the transcript is following the newest message.
 *
 * A chat surface should stay at the bottom while you watch a reply arrive, and
 * stop the moment you scroll up to read something. Doing only the first half —
 * scrolling to the bottom whenever content changes — means every new cell
 * yanks the page out from under whoever is reading.
 */

import { useCallback, useEffect, useRef, useState } from "react";

/**
 * How close to the bottom still counts as following. Wide enough to survive a
 * sub-pixel scroll position and the last line's descender, narrow enough that
 * a deliberate scroll away registers immediately.
 */
export const PIN_THRESHOLD_PX = 48;

/** The measurements the predicate needs, so a test can supply them directly. */
export interface ScrollMetrics {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
}

export function isNearBottom(el: ScrollMetrics, threshold = PIN_THRESHOLD_PX): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
}

export interface FollowBottom {
  /** True while the view is following the newest content. */
  pinned: boolean;
  /** True when content grew while the view was not following. */
  missed: boolean;
  /** Returns to the bottom and resumes following. */
  jumpToLatest: () => void;
}

/**
 * Keeps `scrollRef` at the bottom while the user is there.
 *
 * Growth is observed on the content element rather than counted in cells: a
 * reply streaming into one existing message grows the page without adding a
 * cell, and that is exactly the case where following matters most.
 *
 * `resetKey` re-pins — opening a conversation always starts at the newest
 * message, whatever the previous one was doing.
 */
export function useFollowBottom(
  scrollRef: React.RefObject<HTMLElement | null>,
  contentRef: React.RefObject<HTMLElement | null>,
  resetKey?: string,
): FollowBottom {
  const [pinned, setPinned] = useState(true);
  const [missed, setMissed] = useState(false);
  // The observer callback must read the live value without re-subscribing on
  // every scroll. Written from the event handlers rather than during render:
  // a render-phase write lands after the scroll that caused it, so an observer
  // firing in between would read `true` and haul the user back to the bottom
  // at the exact moment they scrolled away.
  const pinnedRef = useRef(true);
  const setPinnedNow = useCallback((next: boolean) => {
    pinnedRef.current = next;
    setPinned(next);
  }, []);

  /**
   * Last distance from the bottom, not last height: `ConversationPage` reveals
   * older messages by mounting them *above* the reading position and pushing
   * `scrollTop` down by the same amount, so the page grows while the distance
   * to the newest message does not. Measuring height alone would call that
   * arrival and offer a way back to content the reader never left.
   */
  const lastGap = useRef(0);

  const gapNow = useCallback(() => {
    const el = scrollRef.current;
    return el ? el.scrollHeight - el.scrollTop - el.clientHeight : 0;
  }, [scrollRef]);

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    lastGap.current = gapNow();
  }, [scrollRef, gapNow]);

  useEffect(() => {
    setPinnedNow(true);
    setMissed(false);
    // Re-pinning is not enough: if the incoming transcript happens to lay out
    // at the same height as the outgoing one, no resize fires and the view
    // would sit mid-scrollback while claiming to follow.
    scrollToBottom();
  }, [resetKey, setPinnedNow, scrollToBottom]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const onScroll = () => {
      // Scrolling away widens the gap without anything arriving, so the new
      // gap becomes the baseline the next resize is measured against.
      lastGap.current = gapNow();
      const near = isNearBottom(el);
      setPinnedNow(near);
      if (near) setMissed(false);
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, [scrollRef, setPinnedNow, gapNow]);

  useEffect(() => {
    const content = contentRef.current;
    // Absent in some test environments, and in any browser old enough not to
    // have it the transcript simply stops auto-following rather than breaking.
    if (!content || typeof ResizeObserver === "undefined") return;
    // Seed from what is already on screen, so the first observation is measured
    // against the current transcript rather than against nothing — otherwise
    // any first change at all, including a shrink, reads as growth.
    lastGap.current = gapNow();
    const observer = new ResizeObserver(() => {
      const gap = gapNow();
      const arrived = gap > lastGap.current;
      if (pinnedRef.current) {
        scrollToBottom();
        return;
      }
      lastGap.current = gap;
      // Only a widening gap means something landed below the reading position.
      // Revealing earlier messages, a window resize rewrapping text, or the
      // thinking line disappearing leave that distance alone or shrink it.
      if (arrived) setMissed(true);
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [contentRef, scrollToBottom, gapNow]);

  const jumpToLatest = useCallback(() => {
    setPinnedNow(true);
    setMissed(false);
    scrollToBottom();
  }, [scrollToBottom, setPinnedNow]);

  return { pinned, missed, jumpToLatest };
}
