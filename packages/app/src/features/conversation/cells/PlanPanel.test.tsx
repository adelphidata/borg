import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { PlanPanel } from "./PlanPanel";
import type { PlanStep } from "../useConversation";

afterEach(cleanup);

const STEPS: PlanStep[] = [
  { id: "1", text: "read the config", status: "done" },
  { id: "2", text: "rewrite the parser", status: "in_progress" },
  { id: "3", text: "run the tests", status: "pending" },
];

/** The panel opens expanded, so collapsing it is what the header tests need. */
function collapse() {
  fireEvent.click(screen.getByRole("button"));
}

describe("PlanPanel", () => {
  it("names the running step once collapsed", () => {
    render(<PlanPanel steps={STEPS} />);
    collapse();

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "false");
    // The step the agent is on, without paying an expand for it.
    expect(screen.getByText("rewrite the parser")).toBeTruthy();
    expect(screen.getByText("1/3")).toBeTruthy();
    // Still collapsed: the steps either side stay out of the DOM.
    expect(screen.queryByText("run the tests")).toBeNull();
  });

  it("shows only the count when no step is running", () => {
    const settled = STEPS.map((s) => (s.status === "in_progress" ? { ...s, status: "done" } : s));
    render(<PlanPanel steps={settled} />);
    collapse();

    expect(screen.getByText("2/3")).toBeTruthy();
    expect(screen.queryByText("rewrite the parser")).toBeNull();
  });

  it("keeps the running step out of the header while expanded", () => {
    render(<PlanPanel steps={STEPS} />);

    // Expanded, the list already names it — a header copy would say it twice.
    expect(screen.getAllByText("rewrite the parser")).toHaveLength(1);
    expect(screen.getByText("run the tests")).toBeTruthy();
  });

  it("renders nothing without steps", () => {
    const { container } = render(<PlanPanel steps={[]} />);

    expect(container.firstChild).toBeNull();
  });
});
