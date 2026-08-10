import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ToolRunCell } from "./ToolRunCell";
import type { ToolCellData } from "./toolRuns";

function tool(id: string, path: string, name = "read_file"): ToolCellData {
  return {
    kind: "tool",
    id,
    name,
    args: JSON.stringify({ path }),
    output: `contents of ${path}`,
    done: true,
    stderr: false,
  };
}

const RUN = [tool("a", "src/one.ts"), tool("b", "src/two.ts"), tool("c", "src/three.ts")];

afterEach(cleanup);

describe("ToolRunCell", () => {
  it("stands in for the whole run with one line and a count", () => {
    render(<ToolRunCell tools={RUN} live={false} />);
    expect(screen.getByRole("button", { expanded: false }).textContent).toContain("Read 3 files");
    expect(screen.getByText("3")).toBeTruthy();
  });

  it("keeps the tool output out of the transcript until asked", () => {
    render(<ToolRunCell tools={RUN} live={false} />);
    expect(screen.queryByText(/contents of/)).toBeNull();

    fireEvent.click(screen.getByRole("button", { expanded: false }));
    // Every call in the run is now its own row, openable as it always was.
    // (The name is uppercased in CSS, so the text itself stays as sent.)
    expect(screen.getAllByText("read_file")).toHaveLength(3);
  });

  it("collapses again on a second click", () => {
    render(<ToolRunCell tools={RUN} live={false} />);
    const summary = screen.getByRole("button", { expanded: false });
    fireEvent.click(summary);
    fireEvent.click(screen.getByRole("button", { expanded: true }));
    expect(screen.queryAllByText("read_file")).toHaveLength(0);
  });

  it("tickers the newest call while the run is still going", () => {
    render(<ToolRunCell tools={RUN} live />);
    expect(screen.getByRole("button", { expanded: false }).textContent).toContain(
      "Reading 3 files",
    );
    // The call that just landed, so a live run still shows movement.
    expect(screen.getByText("src/three.ts")).toBeTruthy();
  });

  it("drops the ticker once the run is opened, so it cannot double the row", () => {
    render(<ToolRunCell tools={RUN} live />);
    // Collapsed, the newest call shows once — as the ticker.
    expect(screen.getAllByText("src/three.ts")).toHaveLength(1);

    fireEvent.click(screen.getByRole("button", { expanded: false }));
    // Opened, it still shows once — as its own row, with no ticker echoing it.
    expect(screen.getAllByText("src/three.ts")).toHaveLength(1);
  });
});
