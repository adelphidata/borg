import { act, renderHook } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  PIN_THRESHOLD_PX,
  hasSelectionInside,
  isNearBottom,
  useFollowBottom,
  wheelReachesScroller,
} from "./followBottom";

describe("isNearBottom", () => {
  it("counts a view resting exactly at the bottom as following", () => {
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 800, clientHeight: 200 })).toBe(true);
  });

  it("tolerates a fractional gap so a sub-pixel position does not unfollow", () => {
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 799.5, clientHeight: 200 })).toBe(true);
  });

  it("still follows just inside the threshold, and stops just outside it", () => {
    const at = (gap: number) => ({ scrollHeight: 1000, scrollTop: 800 - gap, clientHeight: 200 });
    expect(isNearBottom(at(PIN_THRESHOLD_PX))).toBe(true);
    expect(isNearBottom(at(PIN_THRESHOLD_PX + 1))).toBe(false);
  });

  it("does not follow a view scrolled well up the page", () => {
    expect(isNearBottom({ scrollHeight: 4000, scrollTop: 100, clientHeight: 200 })).toBe(false);
  });
});

/**
 * A scroller and the content inside it, whose heights a test drives by hand.
 * jsdom lays nothing out, so both report whatever the test last set. The
 * position is clamped as a browser clamps it: a view cannot sit past the end.
 */
function scroller(height: number, viewport = 200) {
  const el = document.createElement("div");
  const content = document.createElement("div");
  el.append(content);
  document.body.append(el);
  Object.defineProperty(el, "clientHeight", { value: viewport, configurable: true });

  let scrollHeight = height;
  let top = 0;
  const clamp = (value: number) => Math.max(0, Math.min(value, scrollHeight - viewport));
  Object.defineProperty(el, "scrollTop", {
    get: () => top,
    set: (value: number) => (top = clamp(value)),
    configurable: true,
  });
  for (const node of [el, content]) {
    Object.defineProperty(node, "scrollHeight", { get: () => scrollHeight, configurable: true });
  }

  return {
    el,
    content,
    /** Grows the transcript, as a reply arriving does. Negative shrinks it. */
    resizeContent(by: number) {
      scrollHeight += by;
      // A page that got shorter drags a view past its end back onto it.
      if (top !== clamp(top)) {
        top = clamp(top);
        el.dispatchEvent(new Event("scroll"));
      }
      resize();
    },
    /**
     * Mounts older cells above the reading position, as "Show earlier" does:
     * the page grows and `ConversationPage` pushes `scrollTop` down by the same
     * amount in a layout effect, so the newest message stays where it was.
     */
    revealEarlier(by: number) {
      scrollHeight += by;
      el.scrollTop += by;
      resize();
    },
    scrollTo(to: number) {
      el.scrollTop = to;
      el.dispatchEvent(new Event("scroll"));
    },
    /** Turns the wheel over the transcript, or over `target` inside it. */
    wheel(deltaY: number, init: { ctrlKey?: boolean; target?: Element } = {}) {
      const event = new WheelEvent("wheel", { deltaY, ctrlKey: init.ctrlKey, bubbles: true });
      (init.target ?? el).dispatchEvent(event);
    },
    /**
     * A box inside the transcript with its own scrollbar, as a tool's output
     * is: 600px of text in a 300px window, scrolled to `top`.
     */
    nestedScroller(top: number) {
      const box = document.createElement("div");
      content.append(box);
      Object.defineProperty(box, "scrollHeight", { value: 600, configurable: true });
      Object.defineProperty(box, "clientHeight", { value: 300, configurable: true });
      Object.defineProperty(box, "scrollTop", { value: top, configurable: true });
      const line = document.createElement("span");
      box.append(line);
      return line;
    },
    /** Selects the transcript's text, or text outside it. */
    select(where: "inside" | "outside") {
      const host = where === "inside" ? content : document.body;
      const text = document.createTextNode("some words to copy");
      host.append(text);
      const range = document.createRange();
      range.setStart(text, 0);
      range.setEnd(text, 4);
      const selection = document.getSelection()!;
      selection.removeAllRanges();
      selection.addRange(range);
    },
    clearSelection() {
      document.getSelection()!.removeAllRanges();
    },
  };
}

/** Fires every live observer — jsdom has no ResizeObserver of its own. */
let observers: Array<() => void> = [];
function resize() {
  for (const notify of observers) notify();
}

beforeEach(() => {
  observers = [];
  class FakeResizeObserver {
    constructor(private callback: () => void) {}
    observe() {
      observers.push(this.callback);
    }
    disconnect() {
      observers = observers.filter((c) => c !== this.callback);
    }
    unobserve() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
});

afterEach(() => {
  vi.unstubAllGlobals();
  document.getSelection()?.removeAllRanges();
  document.body.replaceChildren();
});

function mount(resetKey?: string) {
  const scroll = scroller(1000);
  const scrollRef = createRef<HTMLElement>() as React.RefObject<HTMLElement | null>;
  const contentRef = createRef<HTMLElement>() as React.RefObject<HTMLElement | null>;
  (scrollRef as { current: HTMLElement }).current = scroll.el;
  (contentRef as { current: HTMLElement }).current = scroll.content;
  scroll.el.scrollTop = 800;

  const hook = renderHook(
    ({ key }: { key?: string }) => useFollowBottom(scrollRef, contentRef, key),
    { initialProps: { key: resetKey } },
  );
  return { ...scroll, hook };
}

describe("useFollowBottom", () => {
  it("starts at the newest message", () => {
    expect(mount().hook.result.current.pinned).toBe(true);
  });

  it("follows content that grows while the view is at the bottom", () => {
    const view = mount();
    act(() => view.resizeContent(400));
    // Following means the view moved with the content, not that it stayed put.
    expect(view.el.scrollTop).toBe(1200);
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("stops following once the user scrolls up", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    expect(view.hook.result.current.pinned).toBe(false);
  });

  it("leaves the reading position alone when content arrives mid-scrollback", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.resizeContent(400));

    // The whole point: a reply landing must not move the page you are reading.
    expect(view.el.scrollTop).toBe(100);
    expect(view.hook.result.current.missed).toBe(true);
  });

  it("offers no way back when nothing arrived while you were away", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("does not claim you missed something when the page merely got shorter", () => {
    // The thinking line disappearing, a tool run collapsing, a window resize
    // rewrapping text: height changed, but nothing arrived to catch up on.
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.resizeContent(-200));
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("does not claim you missed something when you asked for older messages", () => {
    // "Show earlier" grows the page above the reading position. Counting that
    // as arrival offers a way back to a conversation nobody has left.
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.revealEarlier(400));
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("still catches a reply that lands after older messages were revealed", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.revealEarlier(400));
    act(() => view.resizeContent(200));
    expect(view.hook.result.current.missed).toBe(true);
  });

  it("resumes following when the user scrolls back down themselves", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.resizeContent(400));
    act(() => view.scrollTo(1200));

    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("does not haul the user back when content grows in the same tick they scroll away", () => {
    // A stream resizes the transcript continuously. If the scroll handler only
    // reached the observer on the next render, that in-between resize would
    // scroll the user back down the instant they tried to leave.
    const view = mount();
    act(() => {
      view.scrollTo(100);
      view.resizeContent(400);
    });
    expect(view.el.scrollTop).toBe(100);
  });

  it("returns to the bottom and resumes following on request", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.resizeContent(400));
    act(() => view.hook.result.current.jumpToLatest());

    expect(view.el.scrollTop).toBe(1200);
    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("opens the next conversation at its newest message", () => {
    const view = mount("first");
    act(() => view.scrollTo(100));
    expect(view.hook.result.current.pinned).toBe(false);

    act(() => view.hook.rerender({ key: "second" }));
    expect(view.hook.result.current.pinned).toBe(true);
    // Re-pinning alone would leave the view mid-scrollback whenever the new
    // transcript happens to lay out at the same height as the old one.
    expect(view.el.scrollTop).toBe(800);
  });

  it("stops following on a scroll up too small to leave the threshold", () => {
    // One trackpad step. Still "near the bottom", and still a decision to read.
    const view = mount();
    act(() => view.scrollTo(780));
    act(() => view.resizeContent(400));

    expect(view.el.scrollTop).toBe(780);
    expect(view.hook.result.current.pinned).toBe(false);
    expect(view.hook.result.current.missed).toBe(true);
  });

  it("does not resume while the user is still moving up inside the threshold", () => {
    const view = mount();
    act(() => view.scrollTo(790));
    act(() => view.scrollTo(780));
    expect(view.hook.result.current.pinned).toBe(false);
  });

  it("resumes only once a scroll down lands inside the threshold", () => {
    const view = mount();
    act(() => view.scrollTo(400));
    // Down, but still well short of the newest message.
    act(() => view.scrollTo(600));
    expect(view.hook.result.current.pinned).toBe(false);

    act(() => view.scrollTo(800 - PIN_THRESHOLD_PX));
    expect(view.hook.result.current.pinned).toBe(true);
  });

  it("stops on the wheel itself, before the scroll it causes", () => {
    // The resize lands between the wheel and its scroll event.
    const view = mount();
    act(() => {
      view.wheel(-4);
      view.resizeContent(400);
    });
    expect(view.el.scrollTop).toBe(800);
    expect(view.hook.result.current.pinned).toBe(false);
  });

  it("keeps following through a downward wheel", () => {
    const view = mount();
    act(() => {
      view.wheel(4);
      view.resizeContent(400);
    });
    expect(view.el.scrollTop).toBe(1200);
  });

  it("keeps following when the wheel turns up with nothing above to read", () => {
    // No scroll can follow, so nothing would ever resume.
    const view = mount();
    act(() => view.resizeContent(-800));
    expect(view.el.scrollTop).toBe(0);
    act(() => view.wheel(-4));
    act(() => view.resizeContent(400));
    expect(view.el.scrollTop).toBe(400);
  });

  it("keeps following while a wheel scrolls a box inside the transcript", () => {
    // The tool output moves; the transcript never does, so nothing would
    // ever scroll back down to resume.
    const view = mount();
    const line = view.nestedScroller(200);
    act(() => {
      view.wheel(-4, { target: line });
      view.resizeContent(400);
    });
    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.el.scrollTop).toBe(1200);
  });

  it("stops on a wheel inside a box that is already at its own top", () => {
    // The box has nothing left above, so the wheel scrolls the transcript.
    const view = mount();
    const line = view.nestedScroller(0);
    act(() => {
      view.wheel(-4, { target: line });
      view.resizeContent(400);
    });
    expect(view.hook.result.current.pinned).toBe(false);
    expect(view.el.scrollTop).toBe(800);
  });

  it("keeps following through a trackpad pinch", () => {
    // Pinch-to-zoom arrives as an upward wheel with ctrl held.
    const view = mount();
    act(() => {
      view.wheel(-4, { ctrlKey: true });
      view.resizeContent(400);
    });
    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.el.scrollTop).toBe(1200);
  });

  it("keeps following when the page gets shorter under a view at the bottom", () => {
    // The browser pulls the view up to the new end; nobody scrolled.
    const view = mount();
    act(() => view.resizeContent(-100));
    expect(view.hook.result.current.pinned).toBe(true);
    act(() => view.resizeContent(300));
    expect(view.el.scrollTop).toBe(1000);
  });

  it("holds still under a selection, and resumes once it clears and the user is back down", () => {
    const view = mount();
    act(() => view.select("inside"));
    act(() => view.resizeContent(400));
    expect(view.el.scrollTop).toBe(800);
    expect(view.hook.result.current.missed).toBe(true);

    // Clearing the selection alone is not a request to move.
    act(() => view.clearSelection());
    act(() => view.resizeContent(100));
    expect(view.el.scrollTop).toBe(800);

    act(() => view.scrollTo(1300));
    act(() => view.resizeContent(200));
    expect(view.el.scrollTop).toBe(1500);
    expect(view.hook.result.current.missed).toBe(false);
  });

  it("lets Jump to latest win over a selection the user has left behind", () => {
    const view = mount();
    act(() => view.select("inside"));
    act(() => view.resizeContent(400));
    expect(view.hook.result.current.pinned).toBe(false);

    act(() => view.hook.result.current.jumpToLatest());
    act(() => view.resizeContent(100));
    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.el.scrollTop).toBe(1300);
  });

  it("lets scrolling back to the bottom win over a leftover selection", () => {
    const view = mount();
    act(() => view.select("inside"));
    act(() => view.resizeContent(400));
    act(() => view.scrollTo(1200));
    act(() => view.resizeContent(100));
    expect(view.hook.result.current.pinned).toBe(true);
    expect(view.el.scrollTop).toBe(1300);
  });

  it("holds still under a fresh selection made after jumping back", () => {
    const view = mount();
    act(() => view.select("inside"));
    act(() => view.resizeContent(400));
    act(() => view.hook.result.current.jumpToLatest());

    // A new selection is someone reading again.
    act(() => view.select("inside"));
    act(() => view.resizeContent(100));
    expect(view.hook.result.current.pinned).toBe(false);
    expect(view.el.scrollTop).toBe(1200);
  });

  it("follows past a selection made somewhere else on the page", () => {
    const view = mount();
    act(() => view.select("outside"));
    act(() => view.resizeContent(400));
    expect(view.el.scrollTop).toBe(1200);
  });
});

describe("wheelReachesScroller", () => {
  /** A chain of boxes: scroller > middle > leaf, with `middle` measured. */
  function nest(middle: { scrollTop: number; scrollHeight: number; clientHeight: number }) {
    const scroller = document.createElement("div");
    const box = scroller.appendChild(document.createElement("div"));
    const leaf = box.appendChild(document.createElement("span"));
    for (const [name, value] of Object.entries(middle)) {
      Object.defineProperty(box, name, { value, configurable: true });
    }
    return { scroller, leaf };
  }

  it.each([
    {
      name: "a box that can still scroll up",
      box: { scrollTop: 50, scrollHeight: 600, clientHeight: 300 },
      ctrlKey: false,
      want: false,
    },
    {
      name: "a box already at its top",
      box: { scrollTop: 0, scrollHeight: 600, clientHeight: 300 },
      ctrlKey: false,
      want: true,
    },
    {
      name: "a box with nothing to scroll",
      box: { scrollTop: 0, scrollHeight: 300, clientHeight: 300 },
      ctrlKey: false,
      want: true,
    },
    {
      name: "a pinch over plain text",
      box: { scrollTop: 0, scrollHeight: 300, clientHeight: 300 },
      ctrlKey: true,
      want: false,
    },
  ])("$name → $want", ({ box, ctrlKey, want }) => {
    const { scroller, leaf } = nest(box);
    expect(wheelReachesScroller({ ctrlKey, target: leaf }, scroller)).toBe(want);
  });

  it("does not look past the scroller itself", () => {
    // The scroller is the thing being asked to move; its own position is the
    // caller's check.
    const { scroller } = nest({ scrollTop: 0, scrollHeight: 300, clientHeight: 300 });
    Object.defineProperty(scroller, "scrollTop", { value: 500, configurable: true });
    Object.defineProperty(scroller, "scrollHeight", { value: 900, configurable: true });
    expect(wheelReachesScroller({ ctrlKey: false, target: scroller }, scroller)).toBe(true);
  });
});

describe("hasSelectionInside", () => {
  it("ignores a bare caret, which selects nothing", () => {
    const root = document.createElement("div");
    const text = document.createTextNode("words");
    root.append(text);
    document.body.append(root);
    const range = document.createRange();
    range.setStart(text, 2);
    range.collapse(true);
    document.getSelection()!.addRange(range);

    expect(hasSelectionInside(root)).toBe(false);
  });

  it("has nothing to protect without a transcript", () => {
    expect(hasSelectionInside(null)).toBe(false);
  });
});
