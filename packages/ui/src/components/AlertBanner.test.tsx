import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { AlertBanner } from "./AlertBanner";
import type { BannerAlert } from "../types";

const ALERT: BannerAlert = {
  title: "Borg is not running",
  stackedTitle: "Not running",
  sub: "Start the daemon to continue.",
  action: "Start",
};

describe("AlertBanner", () => {
  it("renders nothing when there is no alert", () => {
    // Callers pass the alert straight through from the data layer, so
    // "no alert" is the common case, not an error.
    const { container } = render(<AlertBanner alert={undefined} />);
    expect(container.firstChild).toBeNull();
  });

  it("uses the wide title in the default layout", () => {
    render(<AlertBanner alert={ALERT} />);
    expect(screen.getByText(ALERT.title)).toBeTruthy();
    expect(screen.queryByText(ALERT.stackedTitle)).toBeNull();
    expect(screen.getByText(ALERT.sub)).toBeTruthy();
  });

  it("uses the shorter title in the stacked layout", () => {
    // The stacked variant is for a narrow column, where the wide title wraps.
    render(<AlertBanner alert={ALERT} stacked />);
    expect(screen.getByText(ALERT.stackedTitle)).toBeTruthy();
    expect(screen.queryByText(ALERT.title)).toBeNull();
  });

  it("runs the action from either layout", () => {
    const onAction = vi.fn();
    const { unmount } = render(<AlertBanner alert={ALERT} onAction={onAction} />);
    fireEvent.click(screen.getByRole("button", { name: ALERT.action }));
    unmount();

    render(<AlertBanner alert={ALERT} stacked onAction={onAction} />);
    fireEvent.click(screen.getByRole("button", { name: ALERT.action }));
    expect(onAction).toHaveBeenCalledTimes(2);
  });

  it("still renders the action button when no handler is wired", () => {
    // The banner is copy-driven; a missing handler must not break the render.
    render(<AlertBanner alert={ALERT} />);
    const button = screen.getByRole("button", { name: ALERT.action });
    expect(() => fireEvent.click(button)).not.toThrow();
  });
});
