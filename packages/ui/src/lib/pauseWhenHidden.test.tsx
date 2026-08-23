import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { usePauseAnimationsWhenHidden } from "./pauseWhenHidden";

const HIDDEN_ATTR = "data-hidden";

function setHidden(hidden: boolean) {
  Object.defineProperty(document, "hidden", { configurable: true, value: hidden });
  act(() => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

function Probe() {
  usePauseAnimationsWhenHidden();
  return null;
}

afterEach(() => {
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
  document.documentElement.removeAttribute(HIDDEN_ATTR);
});

describe("usePauseAnimationsWhenHidden", () => {
  it("marks a page that is already hidden at mount", () => {
    // A window minimised before the app mounted would otherwise animate for the
    // whole time nobody is looking.
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    render(<Probe />);
    expect(document.documentElement.getAttribute(HIDDEN_ATTR)).toBe("true");
  });

  it("marks and unmarks the page as visibility changes", () => {
    render(<Probe />);
    expect(document.documentElement.hasAttribute(HIDDEN_ATTR)).toBe(false);

    setHidden(true);
    expect(document.documentElement.getAttribute(HIDDEN_ATTR)).toBe("true");

    setHidden(false);
    expect(document.documentElement.hasAttribute(HIDDEN_ATTR)).toBe(false);
  });

  it("clears the mark and stops listening on unmount", () => {
    // A left-behind attribute would freeze every animation for the rest of the
    // session.
    const { unmount } = render(<Probe />);
    setHidden(true);
    unmount();
    expect(document.documentElement.hasAttribute(HIDDEN_ATTR)).toBe(false);

    setHidden(true);
    expect(document.documentElement.hasAttribute(HIDDEN_ATTR)).toBe(false);
  });

  it("subscribes once, not once per render", () => {
    // A listener added on every render would pile up handlers for the life of
    // the app; the effect must run on mount alone.
    const addSpy = vi.spyOn(document, "addEventListener");
    const { rerender } = render(<Probe />);
    rerender(<Probe />);
    rerender(<Probe />);
    const visibilitySubscriptions = addSpy.mock.calls.filter(
      ([type]) => type === "visibilitychange",
    );
    expect(visibilitySubscriptions).toHaveLength(1);
    addSpy.mockRestore();
  });
});
