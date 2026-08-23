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

  it("reports the position within the matches in the quiet tone", () => {
    renderBar({ query: "borg", count: 12, position: 4 });
    const counter = screen.getByText("4/12");
    expect(counter.className).toContain("text-ink3");
    expect(counter.className).not.toContain("text-alert");
  });

  it("says nothing about matches before anything is typed", () => {
    renderBar({ query: "", count: 0, position: 0 });
    // Zero matches for an empty query is not a failure, so the counter slot is
    // blank and stays in the quiet tone, not the alert one.
    const counter = document.querySelector("span[class*='tabular-nums']");
    expect(counter).not.toBeNull();
    expect(counter?.textContent).toBe("");
    expect(counter?.className).toContain("text-ink3");
    expect(counter?.className).not.toContain("text-alert");
  });

  it("says None, in the alert tone, for a query that matched nothing", () => {
    renderBar({ query: "zzz", count: 0, position: 0 });
    expect(screen.getByText("None").className).toContain("text-alert");
  });

  it("walks forward on Enter and backward on Shift+Enter", () => {
    const props = renderBar();
    // fireEvent returns false when a handler called preventDefault — Enter
    // must not also submit a form the bar happens to sit inside.
    expect(fireEvent.keyDown(box(), { key: "Enter" })).toBe(false);
    expect(props.onNext).toHaveBeenCalledTimes(1);
    expect(props.onPrevious).not.toHaveBeenCalled();
    expect(props.onClose).not.toHaveBeenCalled();

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

  it("lets ordinary typing keys pass through untouched", () => {
    // Only Enter and Escape are special; a letter must neither walk the
    // results nor close the bar — it just types.
    const props = renderBar();
    fireEvent.keyDown(box(), { key: "a" });
    expect(props.onNext).not.toHaveBeenCalled();
    expect(props.onPrevious).not.toHaveBeenCalled();
    expect(props.onClose).not.toHaveBeenCalled();
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

  it("points the previous arrow up and the next arrow down", () => {
    // One chevron glyph, flipped for "previous" only — both pointing the same
    // way would leave the pair indistinguishable.
    renderBar();
    const arrow = (name: string) =>
      screen.getByRole("button", { name }).querySelector("span")?.className ?? "";
    expect(arrow("Previous match")).toContain("rotate-180");
    expect(arrow("Next match")).not.toContain("rotate-180");
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
