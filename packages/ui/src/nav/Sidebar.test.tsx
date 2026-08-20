import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { Sidebar } from "./Sidebar";
import { ChatIcon } from "../components/icons";
import type { SidebarProps } from "./Sidebar";

afterEach(cleanup);

function renderSidebar(over: Partial<SidebarProps> = {}) {
  const props: SidebarProps = {
    routes: [
      {
        id: "chat",
        path: "/",
        label: "Chat",
        tabLabel: "Chat",
        icon: ChatIcon,
        page: () => null,
        section: "primary",
      },
    ],
    recents: [{ id: "s1", title: "a past conversation" }],
    onOpenRecent: () => {},
    onArchiveRecent: () => {},
    onNewConversation: () => {},
    onSeeAllConversations: () => {},
    user: { name: "Operator", initials: "OP" },
    ...over,
  };
  return render(
    <MemoryRouter>
      <Sidebar {...props} />
    </MemoryRouter>,
  );
}

describe("Sidebar collapse", () => {
  it("keeps the way back on screen once collapsed", () => {
    const onToggleCollapsed = vi.fn();
    renderSidebar({ collapsed: true, onToggleCollapsed });

    // The whole failure mode: a collapsed sidebar whose only reopen lives
    // somewhere else is a sidebar the user has lost.
    fireEvent.click(screen.getByLabelText("Expand sidebar"));
    expect(onToggleCollapsed).toHaveBeenCalledOnce();
  });

  it("drops the nav and the recents while collapsed", () => {
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {} });

    expect(screen.queryByText("Chat")).toBeNull();
    expect(screen.queryByText("a past conversation")).toBeNull();
    expect(screen.queryByText("Show all…")).toBeNull();
  });

  it("shows the nav and offers to collapse when expanded", () => {
    renderSidebar({ collapsed: false, onToggleCollapsed: () => {} });

    expect(screen.getByText("Chat")).toBeTruthy();
    expect(screen.getByText("a past conversation")).toBeTruthy();
    expect(screen.getByLabelText("Collapse sidebar")).toBeTruthy();
  });

  it("offers no collapse control when the app does not handle it", () => {
    // Both surfaces mount this; one adopting collapse must not put a dead
    // button in the other.
    renderSidebar();

    expect(screen.queryByLabelText("Collapse sidebar")).toBeNull();
    expect(screen.getByText("Chat")).toBeTruthy();
  });
});
