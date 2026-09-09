import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { Toast } from "./Toast";

afterEach(() => {
  vi.useRealTimers();
});

describe("Toast", () => {
  it("renders nothing when there is nothing to say", () => {
    // The shell mounts one of these permanently and feeds it whatever the undo
    // store holds, so "no message" is the normal state, not an error.
    const { container } = render(<Toast message={undefined} onDismiss={vi.fn()} />);
    expect(container.firstChild).toBeNull();
  });

  it("offers the action and reports it politely", () => {
    const onAction = vi.fn();
    render(
      <Toast
        message="Archived “Trip planning”"
        actionLabel="Undo"
        onAction={onAction}
        onDismiss={vi.fn()}
      />,
    );
    // `status`, not `alert`: it must not cut across a screen reader mid-sentence.
    expect(screen.getByRole("status").getAttribute("aria-live")).toBe("polite");
    fireEvent.click(screen.getByRole("button", { name: "Undo" }));
    expect(onAction).toHaveBeenCalledTimes(1);
  });

  it("is a bare notice when there is nothing to take back", () => {
    render(<Toast message="Saved" onDismiss={vi.fn()} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("dismisses itself once the offer has stood long enough", () => {
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    render(<Toast message="Archived “Trip planning”" onDismiss={onDismiss} durationMs={7_000} />);

    act(() => void vi.advanceTimersByTime(6_999));
    expect(onDismiss).not.toHaveBeenCalled();
    act(() => void vi.advanceTimersByTime(1));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("gives a replacement notice its own full clock, even when it reads the same", () => {
    // Two untitled conversations archived in quick succession produce the same
    // words. Keyed on the message alone, the second offer would inherit the
    // first one's nearly-expired deadline and vanish before it could be used.
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    const message = "Archived “conversation”";
    const { rerender } = render(<Toast id={1} message={message} onDismiss={onDismiss} />);
    act(() => void vi.advanceTimersByTime(6_500));

    rerender(<Toast id={2} message={message} onDismiss={onDismiss} />);
    act(() => void vi.advanceTimersByTime(6_500));
    expect(onDismiss).not.toHaveBeenCalled();

    act(() => void vi.advanceTimersByTime(500));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("dismisses through the callback it has now, not the one it mounted with", () => {
    // A caller passing an inline arrow gets a fresh identity every render; the
    // timer must not fire a closure captured seven seconds ago.
    vi.useFakeTimers();
    const stale = vi.fn();
    const fresh = vi.fn();
    const { rerender } = render(<Toast id={1} message="Archived “One”" onDismiss={stale} />);
    rerender(<Toast id={1} message="Archived “One”" onDismiss={fresh} />);

    act(() => void vi.advanceTimersByTime(7_000));
    expect(stale).not.toHaveBeenCalled();
    expect(fresh).toHaveBeenCalledTimes(1);
  });

  it("clears the mobile tab bar when the surface asks it to", () => {
    // The tab bar is 72px of chrome in normal flow; a notice at the default
    // inset lands inside it and cannot be pressed.
    const { rerender } = render(<Toast message="Archived “One”" onDismiss={vi.fn()} />);
    expect(screen.getByRole("status").style.bottom).toBe("1.5rem");

    rerender(<Toast message="Archived “One”" onDismiss={vi.fn()} bottomInset="88px" />);
    expect(screen.getByRole("status").style.bottom).toBe("88px");
  });

  it("stops the clock when the notice goes away", () => {
    // Undo unmounts the message; a pending timer firing afterwards would clear
    // a *later* offer that has since taken the slot.
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    const { rerender } = render(<Toast message="Archived “One”" onDismiss={onDismiss} />);
    rerender(<Toast message={undefined} onDismiss={onDismiss} />);

    act(() => void vi.advanceTimersByTime(60_000));
    expect(onDismiss).not.toHaveBeenCalled();
  });
});
