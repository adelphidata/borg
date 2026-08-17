/**
 * Searching the transcript.
 *
 * A long conversation is a wall of text, and the browser's own ⌘F is gone the
 * moment the app owns the chord. This is the replacement: it finds the matches,
 * paints them, and leaves scrolling to the caller.
 *
 * Matches are painted with the CSS custom highlight registry rather than by
 * wrapping text in `<mark>` elements. That choice is the whole design: the
 * transcript is React-rendered and streams while you read it, and any highlight
 * written into the DOM would be reconciled away — or worse, survive as stale
 * markup React no longer accounts for. A highlight made of `Range` objects
 * touches no markup at all, so a re-render cannot corrupt it; it only has to be
 * recomputed, which is what the hook does.
 */

/** Elements marked with this attribute are not searched — the find bar itself. */
export const FIND_SKIP_ATTR = "data-find-skip";

const ALL_HIGHLIGHT = "borg-find";
const CURRENT_HIGHLIGHT = "borg-find-current";

/** False on a browser too old for the highlight registry; matches still scroll. */
export function supportsHighlights(): boolean {
  return (
    typeof CSS !== "undefined" &&
    "highlights" in CSS &&
    typeof (globalThis as { Highlight?: unknown }).Highlight === "function"
  );
}

/**
 * Every occurrence of `query` under `root`, in reading order.
 *
 * Case-insensitive, because nobody searching a conversation for "docker" means
 * to miss "Docker". Each match is found within a single text node: a phrase
 * split across two elements — half of it bold — is missed, which is a fair
 * trade for never having to reason about ranges that span markup.
 */
export function findRanges(root: Node, query: string): Range[] {
  const needle = query.toLowerCase();
  if (needle === "") return [];

  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode: (node) => {
      const parent = node.parentElement;
      if (!parent) return NodeFilter.FILTER_REJECT;
      if (parent.closest(`[${FIND_SKIP_ATTR}]`)) return NodeFilter.FILTER_REJECT;
      return node.nodeValue && node.nodeValue.trim() !== ""
        ? NodeFilter.FILTER_ACCEPT
        : NodeFilter.FILTER_REJECT;
    },
  });

  const ranges: Range[] = [];
  let node = walker.nextNode();
  while (node) {
    const text = (node.nodeValue ?? "").toLowerCase();
    let at = text.indexOf(needle);
    while (at !== -1) {
      const range = document.createRange();
      range.setStart(node, at);
      range.setEnd(node, at + needle.length);
      ranges.push(range);
      at = text.indexOf(needle, at + needle.length);
    }
    node = walker.nextNode();
  }
  return ranges;
}

/**
 * Paints `ranges`, with `current` in the stronger colour.
 *
 * The active match gets its own registry entry rather than a different class:
 * with two entries the stylesheet decides which wins, and moving between
 * matches repaints two highlights instead of re-deriving every range.
 */
export function paintRanges(ranges: readonly Range[], current: Range | undefined): void {
  if (!supportsHighlights()) return;
  const Ctor = (globalThis as unknown as { Highlight: new (...ranges: Range[]) => unknown })
    .Highlight;
  const registry = (CSS as unknown as { highlights: Map<string, unknown> }).highlights;
  const others = current ? ranges.filter((r) => r !== current) : ranges;
  registry.set(ALL_HIGHLIGHT, new Ctor(...others));
  registry.set(CURRENT_HIGHLIGHT, new Ctor(...(current ? [current] : [])));
}

/** Drops every painted match. Safe to call when nothing was ever painted. */
export function clearRanges(): void {
  if (!supportsHighlights()) return;
  const registry = (CSS as unknown as { highlights: Map<string, unknown> }).highlights;
  registry.delete(ALL_HIGHLIGHT);
  registry.delete(CURRENT_HIGHLIGHT);
}

/** Wraps `index` into `[0, count)`, so Next past the end returns to the top. */
export function wrapIndex(index: number, count: number): number {
  if (count === 0) return 0;
  return ((index % count) + count) % count;
}

/** Brings a match into view, centred, without yanking the page when it is already there. */
export function revealRange(range: Range, scroller: HTMLElement | null): void {
  const anchor =
    range.startContainer.nodeType === Node.ELEMENT_NODE
      ? (range.startContainer as HTMLElement)
      : range.startContainer.parentElement;
  if (!anchor || !scroller) return;
  const box = anchor.getBoundingClientRect();
  const view = scroller.getBoundingClientRect();
  if (box.top >= view.top && box.bottom <= view.bottom) return;
  anchor.scrollIntoView({ block: "center", behavior: "auto" });
}
