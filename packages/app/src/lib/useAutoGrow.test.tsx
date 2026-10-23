import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import { useAutoGrow } from "./useAutoGrow";

function Box({ value, max }: { value: string; max: number }) {
  const ref = useAutoGrow(value, max);
  return <textarea ref={ref} value={value} readOnly />;
}

/** jsdom lays nothing out, so the content height is whatever the test says. */
function contentHeight(px: number) {
  Object.defineProperty(HTMLTextAreaElement.prototype, "scrollHeight", {
    configurable: true,
    get: () => px,
  });
}

/** jsdom has no layout either, so the box is as wide as the test says. */
function boxWidth(px: number) {
  vi.spyOn(HTMLTextAreaElement.prototype, "getBoundingClientRect").mockReturnValue({
    width: px,
  } as DOMRect);
}

/** A ResizeObserver the test fires by hand, standing in for the browser's. */
function fakeResizeObserver() {
  const callbacks: (() => void)[] = [];
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(callback: () => void) {
        callbacks.push(callback);
      }
      observe() {}
      disconnect() {}
    },
  );
  return { fire: () => act(() => callbacks.forEach((cb) => cb())) };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  delete (HTMLTextAreaElement.prototype as { scrollHeight?: unknown }).scrollHeight;
});

describe("useAutoGrow", () => {
  it("sizes the box to its content, not to its line breaks", () => {
    contentHeight(140);
    const { container } = render(<Box value="one long paragraph that wraps" max={200} />);
    expect(container.querySelector("textarea")!.style.height).toBe("140px");
  });

  it("stops at the cap so the box scrolls instead of taking the page", () => {
    contentHeight(900);
    const { container } = render(<Box value="a lot" max={200} />);
    expect(container.querySelector("textarea")!.style.height).toBe("200px");
    expect(container.querySelector("textarea")!.style.overflowY).toBe("auto");
  });

  it("offers no scrollbar while the box is as tall as its text", () => {
    // Under the cap a fraction of a pixel would otherwise show a scrollbar in
    // an empty box.
    contentHeight(40);
    const { container, rerender } = render(<Box value="" max={200} />);
    expect(container.querySelector("textarea")!.style.overflowY).toBe("hidden");
    contentHeight(900);
    rerender(<Box value="now a lot" max={200} />);
    expect(container.querySelector("textarea")!.style.overflowY).toBe("auto");
    contentHeight(40);
    rerender(<Box value="" max={200} />);
    expect(container.querySelector("textarea")!.style.overflowY).toBe("hidden");
  });

  it("re-measures when the text changes", () => {
    contentHeight(60);
    const { container, rerender } = render(<Box value="short" max={200} />);
    expect(container.querySelector("textarea")!.style.height).toBe("60px");
    contentHeight(120);
    rerender(<Box value="short and then some" max={200} />);
    expect(container.querySelector("textarea")!.style.height).toBe("120px");
  });

  it("re-measures when the box gets narrower, since the same text wraps taller", () => {
    const observer = fakeResizeObserver();
    boxWidth(600);
    contentHeight(60);
    const { container } = render(<Box value="a paragraph" max={200} />);
    const box = container.querySelector("textarea")!;
    expect(box.style.height).toBe("60px");

    boxWidth(300);
    contentHeight(120);
    observer.fire();
    expect(box.style.height).toBe("120px");
  });

  it("ignores a resize that leaves the width alone, so its own height change cannot loop", () => {
    const observer = fakeResizeObserver();
    boxWidth(600);
    contentHeight(60);
    const { container } = render(<Box value="a paragraph" max={200} />);
    const box = container.querySelector("textarea")!;

    // The content height moved without the width moving: measuring here would
    // mean reacting to the observer's own writes.
    contentHeight(120);
    observer.fire();
    expect(box.style.height).toBe("60px");
  });

  it("still sizes the box where there is no ResizeObserver", () => {
    vi.stubGlobal("ResizeObserver", undefined);
    contentHeight(80);
    const { container } = render(<Box value="text" max={200} />);
    expect(container.querySelector("textarea")!.style.height).toBe("80px");
  });
});
