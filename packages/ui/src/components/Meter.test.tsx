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

  // `warnAt` is opt-in and inclusive: reaching the threshold is already worth
  // the alert colour, and a bar with no threshold never turns red.
  it.each([
    { name: "no threshold", percent: 95, warnAt: undefined, warns: false },
    { name: "below the threshold", percent: 79, warnAt: 80, warns: false },
    { name: "exactly at the threshold", percent: 80, warnAt: 80, warns: true },
    { name: "above the threshold", percent: 90, warnAt: 80, warns: true },
  ])("paints the $name bar in the right colour", ({ percent, warnAt, warns }) => {
    const { container } = render(<Meter percent={percent} warnAt={warnAt} />);
    const classes = fill(container).className;
    expect(classes.includes("bg-alert")).toBe(warns);
    expect(classes.includes("bg-neon")).toBe(!warns);
  });

  it("appends the caller's className to the track, nothing else", () => {
    const bare = render(<Meter percent={10} />);
    const styled = render(<Meter percent={10} className="mt-2" />);
    const tokens = (c: HTMLElement) => Array.from((c.firstElementChild as HTMLElement).classList);
    expect(tokens(styled.container).sort()).toEqual([...tokens(bare.container), "mt-2"].sort());
  });
});
