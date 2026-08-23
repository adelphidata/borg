import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { renderToString } from "react-dom/server";
import { Truncated } from "./Truncated";

/**
 * A ResizeObserver that only fires for elements actually handed to `observe`,
 * and stops after `disconnect` — the two calls a leak would skip.
 */
class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  observing = false;
  constructor(private readonly cb: () => void) {
    FakeResizeObserver.instances.push(this);
  }
  observe() {
    this.observing = true;
  }
  disconnect() {
    this.observing = false;
  }
  fire() {
    if (this.observing) this.cb();
  }
}

/**
 * jsdom reports every element as 0x0, so clipping has to be simulated. These
 * stubs stand in for the layout the browser would do.
 */
function stubWidths(scrollWidth: number, clientWidth: number) {
  const scroll = Object.getOwnPropertyDescriptor(Element.prototype, "scrollWidth");
  const client = Object.getOwnPropertyDescriptor(Element.prototype, "clientWidth");
  Object.defineProperty(Element.prototype, "scrollWidth", {
    configurable: true,
    value: scrollWidth,
  });
  Object.defineProperty(Element.prototype, "clientWidth", {
    configurable: true,
    value: clientWidth,
  });
  return () => {
    if (scroll) Object.defineProperty(Element.prototype, "scrollWidth", scroll);
    if (client) Object.defineProperty(Element.prototype, "clientWidth", client);
  };
}

const restores: Array<() => void> = [];
afterEach(() => {
  while (restores.length) restores.pop()?.();
  vi.unstubAllGlobals();
  FakeResizeObserver.instances = [];
});

describe("Truncated", () => {
  it("offers the full text as a tip when the text is clipped", () => {
    // Reading the withheld text back is the only way to learn it.
    restores.push(stubWidths(400, 100));
    render(<Truncated>A very long session title</Truncated>);
    expect(screen.getByText("A very long session title").getAttribute("title")).toBe(
      "A very long session title",
    );
  });

  it("offers no tip when the text fits", () => {
    // A label repeated to itself teaches nothing.
    restores.push(stubWidths(100, 100));
    render(<Truncated>Short</Truncated>);
    expect(screen.getByText("Short").getAttribute("title")).toBeNull();
  });

  it("treats a one-pixel overhang as fitting, not as clipped", () => {
    // Sub-pixel rounding otherwise puts a tip on every second title.
    restores.push(stubWidths(101, 100));
    render(<Truncated>Edge</Truncated>);
    expect(screen.getByText("Edge").getAttribute("title")).toBeNull();
  });

  it("re-measures when the container resizes without the text changing", () => {
    // A collapsing sidebar changes the width that clips; the text does not.
    let widths = stubWidths(100, 100);
    restores.push(() => widths());
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);

    render(<Truncated>Session title</Truncated>);
    expect(screen.getByText("Session title").getAttribute("title")).toBeNull();

    widths();
    widths = stubWidths(400, 100);
    restores.push(() => widths());
    // Only an observer actually watching the span fires — an unobserved one
    // would mean resizes never reach the measurement.
    act(() => FakeResizeObserver.instances.forEach((o) => o.fire()));
    expect(screen.getByText("Session title").getAttribute("title")).toBe("Session title");
  });

  it("stops watching the span when it unmounts", () => {
    restores.push(stubWidths(100, 100));
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);
    const { unmount } = render(<Truncated>Session title</Truncated>);
    expect(FakeResizeObserver.instances.some((o) => o.observing)).toBe(true);
    unmount();
    expect(FakeResizeObserver.instances.some((o) => o.observing)).toBe(false);
  });

  it("re-measures when the text changes, not only when the box does", () => {
    let widths = stubWidths(100, 100);
    restores.push(() => widths());
    // An observer that never fires: any re-measure must come from the effect
    // re-running for the new children.
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);

    const { rerender } = render(<Truncated>Short</Truncated>);
    expect(screen.getByText("Short").getAttribute("title")).toBeNull();

    widths();
    widths = stubWidths(400, 100);
    restores.push(() => widths());
    rerender(<Truncated>A replacement that clips</Truncated>);
    expect(screen.getByText("A replacement that clips").getAttribute("title")).toBe(
      "A replacement that clips",
    );
  });

  it("carries no tip in server-rendered markup, before anything is measured", () => {
    // Before layout there is no evidence of clipping; markup that guessed
    // "clipped" would flash a tip and mismatch on hydration.
    const html = renderToString(<Truncated>Some long session title</Truncated>);
    expect(html).not.toContain("title=");
  });

  it("keeps the classes that do the clipping, plus the caller's and nothing else", () => {
    restores.push(stubWidths(100, 100));
    const tokensOf = (className?: string) => {
      const { container, unmount } = render(<Truncated className={className}>abc</Truncated>);
      const tokens = new Set(Array.from(container.firstElementChild?.classList ?? []));
      unmount();
      return tokens;
    };

    const custom = tokensOf("custom-marker");
    // Without these the component does not ellipsize at all.
    for (const t of ["overflow-hidden", "text-ellipsis", "whitespace-nowrap"]) {
      expect(custom.has(t), `missing ${t}`).toBe(true);
    }
    expect(custom.has("custom-marker")).toBe(true);

    const plain = tokensOf();
    expect([...custom].filter((t) => !plain.has(t))).toEqual(["custom-marker"]);
    expect([...plain].filter((t) => !custom.has(t))).toEqual([]);
  });

  it("still measures once where ResizeObserver is missing", () => {
    restores.push(stubWidths(400, 100));
    vi.stubGlobal("ResizeObserver", undefined);
    render(<Truncated>Clipped anyway</Truncated>);
    expect(screen.getByText("Clipped anyway").getAttribute("title")).toBe("Clipped anyway");
  });
});
