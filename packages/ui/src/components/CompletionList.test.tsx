import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { CompletionList, rowDomId, type CompletionRow } from "./CompletionList";

const ROWS: CompletionRow[] = [
  { id: "a", label: "/model", detail: "opus", hint: "Session" },
  { id: "b", label: "/plan", hint: "Mode" },
  { id: "c", label: "/quit" },
];

function renderList(overrides: Partial<React.ComponentProps<typeof CompletionList>> = {}) {
  const onHover = vi.fn();
  const onChoose = vi.fn();
  const view = render(
    <CompletionList
      rows={ROWS}
      cursor={0}
      onHover={onHover}
      onChoose={onChoose}
      label="Commands"
      id="cmd"
      {...overrides}
    />,
  );
  return { ...view, onHover, onChoose };
}

describe("CompletionList", () => {
  it("renders nothing at all when it is empty and has no empty text", () => {
    // Rows arriving over the wire: an empty box above the input reads as a list
    // that failed to load.
    const { container } = renderList({ rows: [] });
    expect(container.firstChild).toBeNull();
  });

  it("says why it is empty when the caller gave it words", () => {
    renderList({ rows: [], emptyText: "No matching commands" });
    expect(screen.getByText("No matching commands")).toBeTruthy();
    expect(screen.queryAllByRole("option")).toHaveLength(0);
  });

  it("marks only the cursor row as selected", () => {
    renderList({ cursor: 1 });
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.getAttribute("aria-selected"))).toEqual(["false", "true", "false"]);
  });

  it("gives each row the DOM id the input points at", () => {
    // aria-activedescendant is built from rowDomId; focus stays in the textarea,
    // so a mismatch means a screen reader never learns what Enter will take.
    renderList();
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.id)).toEqual([
      rowDomId("cmd", 0),
      rowDomId("cmd", 1),
      rowDomId("cmd", 2),
    ]);
  });

  it("names the listbox so the input can label it", () => {
    renderList();
    const box = screen.getByRole("listbox", { name: "Commands" });
    expect(box.id).toBe("cmd");
  });

  it("chooses on pointer-down and keeps focus in the input", () => {
    const { onChoose } = renderList();
    const second = screen.getAllByRole("option")[1];
    const event = new MouseEvent("pointerdown", { bubbles: true, cancelable: true });
    fireEvent(second, event);
    expect(onChoose).toHaveBeenCalledWith(1);
    // Default prevented is what stops the textarea losing focus, which would
    // strip the trigger token before the row could run.
    expect(event.defaultPrevented).toBe(true);
  });

  it("moves the cursor on hover rather than choosing", () => {
    const { onHover, onChoose } = renderList();
    fireEvent.mouseEnter(screen.getAllByRole("option")[2]);
    expect(onHover).toHaveBeenCalledWith(2);
    expect(onChoose).not.toHaveBeenCalled();
  });

  it("shows detail and hint only for the rows that have them", () => {
    renderList();
    expect(screen.getByText("opus")).toBeTruthy();
    expect(screen.getByText("Session")).toBeTruthy();
    expect(screen.getAllByRole("option")[2].textContent).toBe("/quit");
  });

  it("scrolls the highlighted row into view when the cursor moves past the fold", () => {
    const scrollIntoView = vi.fn();
    const original = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = scrollIntoView;
    try {
      const { rerender } = renderList({ cursor: 0 });
      scrollIntoView.mockClear();
      rerender(
        <CompletionList
          rows={ROWS}
          cursor={2}
          onHover={() => {}}
          onChoose={() => {}}
          label="Commands"
          id="cmd"
        />,
      );
      expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
    } finally {
      Element.prototype.scrollIntoView = original;
    }
  });
});
