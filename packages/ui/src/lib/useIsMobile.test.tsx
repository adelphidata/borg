import { describe, expect, it } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { installMatchMedia } from "../test/matchMedia";
import { useIsMobile } from "./useIsMobile";

const QUERY = "(max-width: 700px)";

// One install for the file: the hook caches its MediaQueryList so that every
// consumer's render reads the same object, exactly as the browser hands back
// the same list for a repeated query.
const media = installMatchMedia(() => false);
const breakpoint = media.controller(QUERY);

function Probe() {
  return <span>{useIsMobile() ? "mobile" : "desktop"}</span>;
}

describe("useIsMobile", () => {
  it("reports the breakpoint the window is at when it mounts", () => {
    breakpoint.set(true);
    render(<Probe />);
    expect(screen.getByText("mobile")).toBeTruthy();
  });

  it("follows the window as it is resized across the breakpoint", () => {
    // The layout swap is JS-driven — app.css has no matching media query — so a
    // hook that read the value only at mount would leave a resized window with
    // the wrong navigation.
    breakpoint.set(false);
    render(<Probe />);
    expect(screen.getByText("desktop")).toBeTruthy();

    act(() => breakpoint.set(true));
    expect(screen.getByText("mobile")).toBeTruthy();

    act(() => breakpoint.set(false));
    expect(screen.getByText("desktop")).toBeTruthy();
  });

  it("detaches its listener when the consumer unmounts", () => {
    // The MediaQueryList is module-cached and outlives every consumer, so a
    // subscribe without a working unsubscribe leaks a listener per mount and
    // fires state updates into dead components.
    const before = breakpoint.listenerCount();
    const { unmount } = render(<Probe />);
    expect(breakpoint.listenerCount()).toBe(before + 1);
    unmount();
    expect(breakpoint.listenerCount()).toBe(before);
  });
});
