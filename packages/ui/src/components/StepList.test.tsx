import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { StepList } from "./StepList";

describe("StepList", () => {
  it("numbers the steps in the order given, as one list", () => {
    render(
      <StepList
        label="Set up Sentry"
        steps={["Open Settings in Sentry.", "Paste the URL.", "Save."]}
      />,
    );
    const list = screen.getByRole("list", { name: "Set up Sentry" });
    const items = within(list).getAllByRole("listitem");
    expect(items.map((li) => li.textContent)).toEqual([
      "1Open Settings in Sentry.",
      "2Paste the URL.",
      "3Save.",
    ]);
  });

  it("renders nothing when there are no steps", () => {
    const { container } = render(<StepList steps={[]} />);
    expect(container.innerHTML).toBe("");
  });
});
