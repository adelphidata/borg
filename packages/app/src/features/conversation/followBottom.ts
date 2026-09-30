/**
 * Whether the transcript is following the newest message.
 *
 * A chat surface should stay at the bottom while you watch a reply arrive, and
 * stop the moment you scroll up to read something or select text to copy. Doing only the first half —
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

/**
 * Whether the user holds a text selection that starts inside `root`.
 *
 * Moving the page under a selection in progress drags its far end along with
 * it, so the text someone meant to copy is no longer the text they have.
 */
export function hasSelectionInside(root: Node | null): boolean {
  if (!root || typeof document.getSelection !== "function") return false;
  const selection = document.getSelection();
  if (!selection || selection.isCollapsed || !selection.anchorNode) return false;
  return root.contains(selection.anchorNode);
}

/** Where a selection starts and ends, enough to tell one selection from another. */
interface SelectionMark {
  anchorNode: Node | null;
  anchorOffset: number;
  focusNode: Node | null;
  focusOffset: number;
}

function selectionMark(root: Node | null): SelectionMark | undefined {
  if (!hasSelectionInside(root)) return undefined;
  const { anchorNode, anchorOffset, focusNode, focusOffset } = document.getSelection()!;
  return { anchorNode, anchorOffset, focusNode, focusOffset };
}

function sameMark(a: SelectionMark, b: SelectionMark | undefined): boolean {
  return (
    b !== undefined &&
    a.anchorNode === b.anchorNode &&
    a.anchorOffset === b.anchorOffset &&
    a.focusNode === b.focusNode &&
    a.focusOffset === b.focusOffset
  );
}

/**
 * Whether a wheel over `scroller` is asking the scroller itself to move up.
 *
 * A wheel bubbles, so one turned inside a tool's output box arrives here too —
 * and while that box still has room to scroll up, it is the box that moves and
 * the transcript stays put. A trackpad pinch arrives as a wheel with `ctrlKey`
 * set, and zooms rather than scrolls.
 */
export function wheelReachesScroller(
  event: Pick<WheelEvent, "ctrlKey" | "target">,
  scroller: Element,
): boolean {
  if (event.ctrlKey) return false;
  let node = event.target instanceof Element ? event.target : null;
  for (; node && node !== scroller; node = node.parentElement) {
    if (node.scrollHeight > node.clientHeight && node.scrollTop > 0) return false;
  }
  return true;
}

/** A clamped view can sit a sub-pixel off the bottom without having left it. */
const AT_BOTTOM_SLACK_PX = 1;

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
   * The selection that was already there when the view last re-pinned. Asking
   * to follow again is a decision about *that* selection: a leftover highlight
   * the user has moved on from must not stop the view the moment it resumes.
   * Only a selection made since then is someone reading.
   */
  const leftover = useRef<SelectionMark | undefined>(undefined);
  const repin = useCallback(() => {
    leftover.current = selectionMark(contentRef.current);
    setPinnedNow(true);
  }, [contentRef, setPinnedNow]);

  /**
   * Last distance from the bottom, not last height: `ConversationPage` reveals
   * older messages by mounting them *above* the reading position and pushing
   * `scrollTop` down by the same amount, so the page grows while the distance
   * to the newest message does not. Measuring height alone would call that
   * arrival and offer a way back to content the reader never left.
   */
  const lastGap = useRef(0);
  /** Where the view last was, which is what tells a move up from a move down. */
  const lastTop = useRef(0);

  const gapNow = useCallback(() => {
    const el = scrollRef.current;
    return el ? el.scrollHeight - el.scrollTop - el.clientHeight : 0;
  }, [scrollRef]);

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      // Our own move must not read as the user's on the next scroll event.
      lastTop.current = el.scrollTop;
    }
    lastGap.current = gapNow();
  }, [scrollRef, gapNow]);

  useEffect(() => {
    repin();
    setMissed(false);
    // Re-pinning is not enough: if the incoming transcript happens to lay out
    // at the same height as the outgoing one, no resize fires and the view
    // would sit mid-scrollback while claiming to follow.
    scrollToBottom();
  }, [resetKey, repin, scrollToBottom]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    lastTop.current = el.scrollTop;
    const onScroll = () => {
      // Scrolling away widens the gap without anything arriving, so the new
      // gap becomes the baseline the next resize is measured against.
      const gap = gapNow();
      lastGap.current = gap;
      const top = el.scrollTop;
      const movedUp = top < lastTop.current;
      lastTop.current = top;
      // Any move up is a decision to read, however small: inside the threshold
      // the next streamed token would otherwise snap the view straight back.
      // A view still resting on the bottom moved because the page got shorter,
      // not because anyone scrolled.
      if (movedUp && gap > AT_BOTTOM_SLACK_PX) {
        setPinnedNow(false);
        return;
      }
      const near = isNearBottom(el);
      if (!near) setPinnedNow(false);
      // Only the move back down re-pins; a scroll while already following
      // must not bless a selection made in the meantime.
      else if (!pinnedRef.current) repin();
      if (near) setMissed(false);
    };
    // The wheel arrives before the scroll it causes, and a resize can land in
    // between. With nothing above to scroll to, no scroll follows and there is
    // nothing to read, so the view keeps following.
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0 && el.scrollTop > 0 && wheelReachesScroller(event, el)) {
        setPinnedNow(false);
      }
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    el.addEventListener("wheel", onWheel, { passive: true });
    return () => {
      el.removeEventListener("scroll", onScroll);
      el.removeEventListener("wheel", onWheel);
    };
  }, [scrollRef, setPinnedNow, repin, gapNow]);

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
      // A selection in progress is reading too: stop following rather than
      // move the page under it. Reaching the bottom again resumes.
      const mark = selectionMark(content);
      if (!mark) leftover.current = undefined;
      else if (pinnedRef.current && !sameMark(mark, leftover.current)) setPinnedNow(false);
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
  }, [contentRef, scrollToBottom, gapNow, setPinnedNow]);

  const jumpToLatest = useCallback(() => {
    repin();
    setMissed(false);
    scrollToBottom();
  }, [scrollToBottom, repin]);

  return { pinned, missed, jumpToLatest };
}
