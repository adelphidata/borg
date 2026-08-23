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
    // The highlight is the sighted-user half of the same signal: the cursor
    // row is filled, the others are transparent.
    expect(options[1].className).toContain("bg-surface3");
    for (const other of [options[0], options[2]]) {
      expect(other.className).not.toContain("bg-surface3");
      expect(other.className).toContain("bg-transparent");
    }
  });

  it("gives each row the DOM id the input points at", () => {
    // aria-activedescendant is built from rowDomId; focus stays in the textarea,
    // so a mismatch means a screen reader never learns what Enter will take.
    renderList();
    const options = screen.getAllByRole("option");
    // Literal ids, not rowDomId() again: comparing the helper to itself would
    // pass even if it returned garbage on both sides.
    expect(options.map((o) => o.id)).toEqual(["cmd-row-0", "cmd-row-1", "cmd-row-2"]);
    expect(rowDomId("cmd", 1)).toBe("cmd-row-1");
  });

  it("names the listbox so the input can label it", () => {
    renderList();
    const box = screen.getByRole("listbox", { name: "Commands" });
    expect(box.id).toBe("cmd");
  });

  it("caps its height so a long list scrolls instead of covering the screen", () => {
    renderList();
    const box = screen.getByRole("listbox");
    // The cap only scrolls if overflow is on; a cap without it clips rows.
    expect(box.style.maxHeight).toBe("232px");
    expect(box.classList.contains("overflow-y-auto")).toBe(true);
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
    // Each lives in its own element — not raw text spliced into the row.
    expect(screen.getByText("opus").tagName).toBe("SPAN");
    expect(screen.getByText("Session").tagName).toBe("SPAN");
    const options = screen.getAllByRole("option");
    expect(options[2].textContent).toBe("/quit");
    // A row without detail or hint renders neither slot, not empty spans:
    // just the label and the flexible spacer.
    expect(options[2].querySelectorAll("span")).toHaveLength(2);
    // Hint present, detail absent → exactly one extra span.
    expect(options[1].querySelectorAll("span")).toHaveLength(3);
  });

  it("tolerates a cursor past the end of the rows", () => {
    // The caller owns the cursor; a filter that shrinks the rows can leave it
    // out of range for a frame. The scroll effect must not throw on that frame.
    expect(() => renderList({ cursor: 7 })).not.toThrow();
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
