import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Menu } from "@borg/ui";
import { resetKeybinds } from "../../lib/keybinds";
import { SessionRowMenu, sessionRowActions, useMenuEscape } from "./SessionRowMenu";

afterEach(() => {
  cleanup();
  resetKeybinds();
});

const handlers = () => ({
  onRename: vi.fn(),
  onArchive: vi.fn(),
  onExport: vi.fn(),
});

describe("sessionRowActions", () => {
  it("is the one list both the kebab and a right-click render", () => {
    // The point of the shared builder: two gestures, never two sets of choices.
    const actions = sessionRowActions(handlers());
    expect(actions.map((a) => a.label)).toEqual(["Rename", "Export…", "Archive"]);
  });

  it("exports markdown, the format the row's label promises", () => {
    const h = handlers();
    sessionRowActions(h)
      .find((a) => a.id === "export")!
      .onSelect();
    expect(h.onExport).toHaveBeenCalledWith("md");
  });

  it("runs the caller's handler for a destructive action", () => {
    const h = handlers();
    sessionRowActions(h)
      .find((a) => a.id === "archive")!
      .onSelect();
    expect(h.onArchive).toHaveBeenCalled();
  });
});

/** A row wired the way the session rows are: right-click opens the shared menu. */
function Row({ onArchive }: { onArchive: () => void }) {
  const [at, setAt] = useState<{ x: number; y: number }>();
  useMenuEscape(at !== undefined, () => setAt(undefined));

  return (
    <div
      data-testid="row"
      onContextMenu={(e) => {
        e.preventDefault();
        setAt({ x: 10, y: 20 });
      }}
    >
      <Menu
        open={at !== undefined}
        onClose={() => setAt(undefined)}
        label="Conversation actions"
        at={at}
        actions={[{ id: "archive", label: "Archive", onSelect: onArchive }]}
      />
    </div>
  );
}

describe("right-click on a session row", () => {
  it("opens the menu, which right-click did nothing to do before", () => {
    render(<Row onArchive={vi.fn()} />);
    expect(screen.queryByRole("menu")).toBeNull();

    fireEvent.contextMenu(screen.getByTestId("row"));
    expect(screen.getByRole("menu")).toBeTruthy();
  });

  it("suppresses the browser menu, so the two do not both appear", () => {
    render(<Row onArchive={vi.fn()} />);
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    screen.getByTestId("row").dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
  });

  it("runs the action it names, then closes", () => {
    const onArchive = vi.fn();
    render(<Row onArchive={onArchive} />);
    fireEvent.contextMenu(screen.getByTestId("row"));
    fireEvent.click(screen.getByRole("menuitem", { name: "Archive" }));

    expect(onArchive).toHaveBeenCalled();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes when the pointer goes down outside it", () => {
    render(<Row onArchive={vi.fn()} />);
    fireEvent.contextMenu(screen.getByTestId("row"));
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("stays open when the pointer goes down inside it", () => {
    render(<Row onArchive={vi.fn()} />);
    fireEvent.contextMenu(screen.getByTestId("row"));
    fireEvent.pointerDown(screen.getByRole("menu"));
    expect(screen.queryByRole("menu")).toBeTruthy();
  });
});

describe("the kebab", () => {
  it("stays shut until it is asked for", () => {
    render(<SessionRowMenu {...handlers()} />);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("opens the same actions the right-click menu offers", () => {
    render(<SessionRowMenu {...handlers()} />);
    fireEvent.click(screen.getByLabelText("Conversation actions"));
    const items = screen.getAllByRole("menuitem").map((el) => el.textContent?.trim());
    expect(items).toEqual(["Rename", "Export…", "Archive"]);
  });

  it("does not open the conversation underneath it", () => {
    const onRowClick = vi.fn();
    render(
      <div onClick={onRowClick}>
        <SessionRowMenu {...handlers()} />
      </div>,
    );
    fireEvent.click(screen.getByLabelText("Conversation actions"));
    expect(onRowClick).not.toHaveBeenCalled();
  });
});
