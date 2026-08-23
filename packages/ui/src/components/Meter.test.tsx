import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Meter } from "./Meter";

/** The painted fill, whose inline width is the whole point of the component. */
function fill(container: HTMLElement): HTMLElement {
  const el = container.firstElementChild?.firstElementChild;
  if (!(el instanceof HTMLElement)) throw new Error("Meter rendered no fill");
  return el;
}

const fillWidth = (container: HTMLElement) => fill(container).style.width;

describe("Meter", () => {
  it("paints the percentage it is given", () => {
    const { container } = render(<Meter percent={42} />);
    expect(fillWidth(container)).toBe("42%");
  });

  it("clamps an over-budget value to a full bar", () => {
    // A caller reporting 130% of a budget is the real case: the bar must fill,
    // not overflow its track and paint over the layout beside it.
    const { container } = render(<Meter percent={130} />);
    expect(fillWidth(container)).toBe("100%");
  });

  it("clamps a negative value to an empty bar", () => {
    const { container } = render(<Meter percent={-20} />);
    expect(fillWidth(container)).toBe("0%");
  });
});
