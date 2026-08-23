import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Pill } from "./Pill";

describe("Pill", () => {
  it("never submits a surrounding form by accident", () => {
    // A bare <button> inside a form defaults to type=submit; the pills sit in
    // the onboarding form, where that would send a half-filled step.
    render(<Pill>Continue</Pill>);
    expect(screen.getByRole("button", { name: "Continue" })).toHaveProperty("type", "button");
  });

  it("calls the caller's handler", () => {
    const onClick = vi.fn();
    render(<Pill onClick={onClick}>Start</Pill>);
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("passes button attributes through to the element", () => {
    const onClick = vi.fn();
    render(
      <Pill disabled onClick={onClick} aria-label="Retry send">
        Retry
      </Pill>,
    );
    const button = screen.getByRole("button", { name: "Retry send" });
    expect(button).toHaveProperty("disabled", true);
    fireEvent.click(button);
    expect(onClick).not.toHaveBeenCalled();
  });
});
