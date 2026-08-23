import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { Truncated } from "./Truncated";

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
    const observers: Array<() => void> = [];
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(private readonly cb: () => void) {
          observers.push(() => this.cb());
        }
        observe() {}
        disconnect() {}
      },
    );

    render(<Truncated>Session title</Truncated>);
    expect(screen.getByText("Session title").getAttribute("title")).toBeNull();

    widths();
    widths = stubWidths(400, 100);
    restores.push(() => widths());
    act(() => observers.forEach((fire) => fire()));
    expect(screen.getByText("Session title").getAttribute("title")).toBe("Session title");
  });

  it("still measures once where ResizeObserver is missing", () => {
    restores.push(stubWidths(400, 100));
    vi.stubGlobal("ResizeObserver", undefined);
    render(<Truncated>Clipped anyway</Truncated>);
    expect(screen.getByText("Clipped anyway").getAttribute("title")).toBe("Clipped anyway");
  });
});
