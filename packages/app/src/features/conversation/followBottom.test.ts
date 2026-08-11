import { act, renderHook } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PIN_THRESHOLD_PX, isNearBottom, useFollowBottom } from "./followBottom";

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
 * jsdom lays nothing out, so both report whatever the test last set.
 */
function scroller(height: number, viewport = 200) {
  const el = document.createElement("div");
  const content = document.createElement("div");
  Object.defineProperty(el, "clientHeight", { value: viewport, configurable: true });

  let scrollHeight = height;
  for (const node of [el, content]) {
    Object.defineProperty(node, "scrollHeight", { get: () => scrollHeight, configurable: true });
  }

  return {
    el,
    content,
    /** Grows the transcript, as a reply arriving does. Negative shrinks it. */
    resizeContent(by: number) {
      scrollHeight += by;
      resize();
    },
    scrollTo(top: number) {
      el.scrollTop = top;
      el.dispatchEvent(new Event("scroll"));
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
    expect(view.el.scrollTop).toBe(1400);
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

  it("resumes following when the user scrolls back down themselves", () => {
    const view = mount();
    act(() => view.scrollTo(100));
    act(() => view.resizeContent(400));
    act(() => view.scrollTo(1400));

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

    expect(view.el.scrollTop).toBe(1400);
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
    expect(view.el.scrollTop).toBe(1000);
  });
});
