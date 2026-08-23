import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { StatusDot } from "./StatusDot";

describe("StatusDot", () => {
  it("stays out of the accessibility tree when it has no label", () => {
    // The dot beside a row that already says "Running" would otherwise be
    // announced twice.
    const { container } = render(<StatusDot tone="ok" />);
    const dot = container.firstElementChild;
    expect(dot?.getAttribute("role")).toBeNull();
    expect(dot?.getAttribute("aria-label")).toBeNull();
  });

  it("announces itself as an image when it carries meaning of its own", () => {
    render(<StatusDot tone="alert" label="Needs attention" />);
    expect(screen.getByRole("img", { name: "Needs attention" })).toBeTruthy();
  });
});
