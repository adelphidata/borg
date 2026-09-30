import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { PageErrorBoundary } from "./PageErrorBoundary";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function Crash(): never {
  throw new Error("cells is undefined");
}

describe("PageErrorBoundary", () => {
  it("keeps a crash inside the page, with a way out and the reason one step deeper", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <div>
        <nav>Sidebar</nav>
        <PageErrorBoundary resetKey="/usage">
          <Crash />
        </PageErrorBoundary>
      </div>,
    );
    // The rest of the shell is still there to leave by.
    expect(screen.getByText("Sidebar")).toBeTruthy();
    expect(screen.getByText("This page ran into a problem.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reload" })).toBeTruthy();
    expect(screen.getByText("cells is undefined").closest("details")).toBeTruthy();
  });

  it("gives the next page a fresh try", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <PageErrorBoundary resetKey="/usage">
        <Crash />
      </PageErrorBoundary>,
    );
    expect(screen.getByText("This page ran into a problem.")).toBeTruthy();

    rerender(
      <PageErrorBoundary resetKey="/settings">
        <div>settings page</div>
      </PageErrorBoundary>,
    );
    expect(screen.getByText("settings page")).toBeTruthy();
  });

  it("stays on the failure while the same page renders again", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <PageErrorBoundary resetKey="/usage">
        <Crash />
      </PageErrorBoundary>,
    );
    rerender(
      <PageErrorBoundary resetKey="/usage">
        <div>recovered by accident</div>
      </PageErrorBoundary>,
    );
    expect(screen.getByText("This page ran into a problem.")).toBeTruthy();
  });
});
