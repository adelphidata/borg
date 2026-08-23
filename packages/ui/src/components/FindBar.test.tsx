import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { FindBar } from "./FindBar";

function renderBar(overrides: Partial<React.ComponentProps<typeof FindBar>> = {}) {
  const props = {
    query: "borg",
    onQueryChange: vi.fn(),
    count: 3,
    position: 1,
    onNext: vi.fn(),
    onPrevious: vi.fn(),
    onClose: vi.fn(),
    ...overrides,
  };
  render(<FindBar {...props} />);
  return props;
}

const box = () => screen.getByRole("textbox", { name: "Find in conversation" });

describe("FindBar", () => {
  it("focuses the input on open", () => {
    // Otherwise the user has to click the box they just asked for.
    renderBar();
    expect(document.activeElement).toBe(box());
  });

  it("reports the position within the matches", () => {
    renderBar({ query: "borg", count: 12, position: 4 });
    expect(screen.getByText("4/12")).toBeTruthy();
  });

  it("says nothing about matches before anything is typed", () => {
    renderBar({ query: "", count: 0, position: 0 });
    expect(screen.queryByText("None")).toBeNull();
    expect(screen.queryByText("0/0")).toBeNull();
  });

  it("says None for a query that matched nothing", () => {
    renderBar({ query: "zzz", count: 0, position: 0 });
    expect(screen.getByText("None")).toBeTruthy();
  });

  it("walks forward on Enter and backward on Shift+Enter", () => {
    const props = renderBar();
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(props.onNext).toHaveBeenCalledTimes(1);
    expect(props.onPrevious).not.toHaveBeenCalled();

    fireEvent.keyDown(box(), { key: "Enter", shiftKey: true });
    expect(props.onPrevious).toHaveBeenCalledTimes(1);
    expect(props.onNext).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape and swallows the key", () => {
    // The app's keyboard layer runs whichever handler registered last: a turn
    // starting while the bar is open puts the composer's cancel above it, so an
    // Escape that reached the layer would stop the turn and leave the bar open.
    const props = renderBar();
    const event = fireEvent.keyDown(box(), { key: "Escape" });
    expect(props.onClose).toHaveBeenCalledTimes(1);
    // fireEvent returns false when a handler called preventDefault.
    expect(event).toBe(false);
  });

  it("reports typing to the caller, which owns the query", () => {
    const props = renderBar();
    fireEvent.change(box(), { target: { value: "daemon" } });
    expect(props.onQueryChange).toHaveBeenCalledWith("daemon");
  });

  it("disables both step buttons when there is nothing to step through", () => {
    renderBar({ query: "zzz", count: 0, position: 0 });
    expect(screen.getByRole("button", { name: "Next match" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Previous match" })).toHaveProperty("disabled", true);
  });

  it("steps through matches from the buttons", () => {
    const props = renderBar();
    fireEvent.click(screen.getByRole("button", { name: "Next match" }));
    fireEvent.click(screen.getByRole("button", { name: "Previous match" }));
    expect(props.onNext).toHaveBeenCalledTimes(1);
    expect(props.onPrevious).toHaveBeenCalledTimes(1);
  });

  it("offers older history only when the caller can search it", () => {
    renderBar();
    expect(screen.queryByText("Older too")).toBeNull();

    const onSearchAll = vi.fn();
    render(
      <FindBar
        query="borg"
        onQueryChange={() => {}}
        count={1}
        position={1}
        onNext={() => {}}
        onPrevious={() => {}}
        onClose={() => {}}
        onSearchAll={onSearchAll}
      />,
    );
    fireEvent.click(screen.getByText("Older too"));
    expect(onSearchAll).toHaveBeenCalledTimes(1);
  });

  it("closes from the close button", () => {
    const props = renderBar();
    fireEvent.click(screen.getByRole("button", { name: "Close find" }));
    expect(props.onClose).toHaveBeenCalledTimes(1);
  });
});
