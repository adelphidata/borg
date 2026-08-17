import { describe, expect, it } from "vitest";
import { FIND_SKIP_ATTR, findRanges, wrapIndex } from "./findInTranscript";

/** A detached transcript to search. */
function transcript(html: string): HTMLElement {
  const root = document.createElement("div");
  root.innerHTML = html;
  return root;
}

/** The matched text of each range, which is what a reader would see painted. */
const matched = (ranges: Range[]) => ranges.map((r) => r.toString());

describe("findRanges", () => {
  it("finds every occurrence in reading order", () => {
    const root = transcript("<p>one docker two</p><p>three docker</p>");
    const ranges = findRanges(root, "docker");
    expect(matched(ranges)).toEqual(["docker", "docker"]);
    // Reading order, not DOM-query order: Next has to walk down the page.
    expect(ranges[0]!.startContainer.nodeValue).toBe("one docker two");
  });

  it("ignores case", () => {
    // Nobody searching a conversation for "docker" means to miss "Docker".
    expect(matched(findRanges(transcript("<p>Docker and DOCKER</p>"), "docker"))).toEqual([
      "Docker",
      "DOCKER",
    ]);
  });

  it("finds repeats inside one text node without overlapping", () => {
    expect(matched(findRanges(transcript("<p>aaaa</p>"), "aa"))).toEqual(["aa", "aa"]);
  });

  it("skips the find bar's own text", () => {
    // The regression this guards: the query typed into the bar is itself text
    // on the page, so searching would always match the search box first.
    const root = transcript(`<div ${FIND_SKIP_ATTR}><span>docker</span></div><p>real docker</p>`);
    const ranges = findRanges(root, "docker");
    expect(ranges).toHaveLength(1);
    expect(ranges[0]!.startContainer.nodeValue).toBe("real docker");
  });

  it("matches nothing for an empty query", () => {
    expect(findRanges(transcript("<p>anything</p>"), "")).toEqual([]);
  });

  it("does not match across an element boundary", () => {
    // The documented trade: a phrase split by markup is missed rather than
    // handled by ranges that span elements.
    expect(findRanges(transcript("<p>doc<b>ker</b></p>"), "docker")).toEqual([]);
  });
});

describe("wrapIndex", () => {
  it("returns to the top past the last match", () => {
    expect(wrapIndex(3, 3)).toBe(0);
  });

  it("returns to the bottom before the first match", () => {
    // Shift+Enter on the first match goes to the last, not to index -1.
    expect(wrapIndex(-1, 3)).toBe(2);
  });

  it("stays at zero when there is nothing to step through", () => {
    expect(wrapIndex(-1, 0)).toBe(0);
    expect(wrapIndex(5, 0)).toBe(0);
  });
});
