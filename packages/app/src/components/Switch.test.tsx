import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Switch } from "./Switch";

describe("Switch", () => {
  it("is a real switch that reports the next state and stops the click there", () => {
    const onChange = vi.fn();
    const parent = vi.fn();
    render(
      <div onClick={parent}>
        <Switch on={false} onChange={onChange} label="Enable git" />
      </div>,
    );
    const el = screen.getByRole("switch", { name: "Enable git" });
    expect(el.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(el);
    expect(onChange).toHaveBeenCalledWith(true);
    // The row behind it opens a detail page; flipping a switch must not.
    expect(parent).not.toHaveBeenCalled();
  });

  it("does nothing while disabled", () => {
    const onChange = vi.fn();
    render(<Switch on disabled onChange={onChange} label="Always on" />);
    const el = screen.getByRole("switch") as HTMLButtonElement;
    expect(el.disabled).toBe(true);
    expect(el.getAttribute("aria-checked")).toBe("true");
    fireEvent.click(el);
    expect(onChange).not.toHaveBeenCalled();
  });
});
