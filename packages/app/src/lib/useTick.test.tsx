import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { useTick } from "./useTick";

function Probe({ enabled, interval = 1000 }: { enabled: boolean; interval?: number }) {
  return <span data-testid="now">{useTick(interval, enabled)}</span>;
}

const shown = () => Number(screen.getByTestId("now").textContent);

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
});

describe("useTick", () => {
  it("counts up while something is counting", () => {
    render(<Probe enabled />);
    const first = shown();

    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(shown()).toBeGreaterThan(first);
  });

  it("stops the moment nothing is counting", () => {
    // A timer behind a settled screen costs a render a second for nothing.
    const { rerender } = render(<Probe enabled />);
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    const settled = shown();

    rerender(<Probe enabled={false} />);
    act(() => {
      vi.advanceTimersByTime(5000);
    });
    expect(shown()).toBe(settled);
  });

  it("re-reads the clock the moment counting starts, without waiting a tick", () => {
    // Otherwise a counter opens showing a time from before the turn began.
    const { rerender } = render(<Probe enabled={false} />);
    const before = shown();
    act(() => {
      vi.advanceTimersByTime(10_000);
    });

    rerender(<Probe enabled />);
    expect(shown()).toBeGreaterThan(before);
  });

  it("holds still while the tab is hidden, and catches up when it returns", () => {
    render(<Probe enabled />);
    const start = shown();

    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    act(() => {
      vi.advanceTimersByTime(5000);
    });
    expect(shown()).toBe(start);

    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(shown()).toBe(start + 6000);
  });

  it("uses the interval it was given", () => {
    render(<Probe enabled interval={250} />);
    const start = shown();
    act(() => {
      vi.advanceTimersByTime(250);
    });
    expect(shown()).toBe(start + 250);
  });
});
