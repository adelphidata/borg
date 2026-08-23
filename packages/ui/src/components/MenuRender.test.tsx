import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Menu, type MenuAction } from "./Menu";

function actions(onSelect = vi.fn()): MenuAction[] {
  return [
    { id: "rename", label: "Rename", onSelect },
    { id: "archive", label: "Archive", onSelect, dividerBefore: true },
  ];
}

describe("Menu", () => {
  it("renders nothing while closed", () => {
    const { container } = render(
      <Menu open={false} onClose={vi.fn()} actions={actions()} label="Conversation actions" />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("exposes one menuitem per action, plus the divider", () => {
    // A `menu` must own its `menuitem`s directly, or a screen reader loses the
    // item count and the arrow-key walk.
    render(<Menu open onClose={vi.fn()} actions={actions()} label="Conversation actions" />);
    const menu = screen.getByRole("menu", { name: "Conversation actions" });
    expect(screen.getAllByRole("menuitem").map((i) => i.textContent?.trim())).toEqual([
      "Rename",
      "Archive",
    ]);
    expect(menu.querySelectorAll('[role="separator"]')).toHaveLength(1);
  });

  it("closes before it runs the action", () => {
    // Running first would leave the menu open over whatever the action opened.
    const order: string[] = [];
    const onClose = vi.fn(() => order.push("close"));
    const onSelect = vi.fn(() => order.push("select"));
    render(
      <Menu
        open
        onClose={onClose}
        actions={[{ id: "rename", label: "Rename", onSelect }]}
        label="Conversation actions"
      />,
    );
    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));
    expect(order).toEqual(["close", "select"]);
  });

  it("does not let a click reach the row underneath", () => {
    // The row opens the conversation; choosing a menu item must not.
    const onRowClick = vi.fn();
    render(
      <div onClick={onRowClick}>
        <Menu open onClose={vi.fn()} actions={actions()} label="Conversation actions" />
      </div>,
    );
    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));
    expect(onRowClick).not.toHaveBeenCalled();
  });

  it("closes on a pointer-down outside itself", () => {
    const onClose = vi.fn();
    render(<Menu open onClose={onClose} actions={actions()} label="Conversation actions" />);
    fireEvent.pointerDown(document.body);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("stays open for a pointer-down inside itself", () => {
    const onClose = vi.fn();
    render(<Menu open onClose={onClose} actions={actions()} label="Conversation actions" />);
    fireEvent.pointerDown(screen.getByRole("menuitem", { name: "Rename" }));
    expect(onClose).not.toHaveBeenCalled();
  });

  it("stops listening once closed, so a later click cannot close it twice", () => {
    const onClose = vi.fn();
    const { rerender } = render(
      <Menu open onClose={onClose} actions={actions()} label="Conversation actions" />,
    );
    rerender(
      <Menu open={false} onClose={onClose} actions={actions()} label="Conversation actions" />,
    );
    fireEvent.pointerDown(document.body);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("positions itself at the pointer when opened by right-click", () => {
    render(
      <Menu
        open
        onClose={vi.fn()}
        actions={actions()}
        label="Conversation actions"
        at={{ x: 120, y: 90 }}
      />,
    );
    const menu = screen.getByRole("menu");
    expect(menu.style.left).toBe("120px");
    expect(menu.style.top).toBe("90px");
  });

  it("takes no inline position when it hangs under its trigger", () => {
    render(<Menu open onClose={vi.fn()} actions={actions()} label="Conversation actions" />);
    const menu = screen.getByRole("menu");
    expect(menu.style.left).toBe("");
    expect(menu.style.top).toBe("");
  });
});
