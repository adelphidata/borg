import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { Sidebar } from "./Sidebar";
import { ChatIcon } from "../components/icons";
import type { AppRoute } from "../types";
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

const route = (over: Partial<AppRoute> & Pick<AppRoute, "id" | "path" | "label">): AppRoute => ({
  tabLabel: over.label,
  icon: ChatIcon,
  page: () => null,
  section: "primary",
  ...over,
});

const NAV_ROUTES: AppRoute[] = [
  route({ id: "chat", path: "/", label: "Chat" }),
  route({ id: "sessions", path: "/sessions", label: "Sessions" }),
  route({ id: "settings", path: "/settings", label: "Settings", section: "secondary" }),
];

function renderNavAt(path: string, routes: AppRoute[] = NAV_ROUTES) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Sidebar
        routes={routes}
        recents={[]}
        onOpenRecent={() => {}}
        onArchiveRecent={() => {}}
        onNewConversation={() => {}}
        onSeeAllConversations={() => {}}
        user={{ name: "Operator", initials: "OP" }}
      />
    </MemoryRouter>,
  );
}

const link = (name: string) => screen.getByRole("link", { name });

describe("Sidebar nav", () => {
  it("matches the root exactly and every other route by prefix", () => {
    // The root would otherwise prefix-match every path; a parent route must
    // stay lit on its own detail pages (matches TabBar).
    renderNavAt("/sessions/abc123");
    expect(link("Chat").getAttribute("aria-current")).toBeNull();
    expect(link("Sessions").getAttribute("aria-current")).toBe("page");
  });

  it("marks the root active at home", () => {
    renderNavAt("/");
    expect(link("Chat").getAttribute("aria-current")).toBe("page");
    expect(link("Sessions").getAttribute("aria-current")).toBeNull();
  });

  it("paints the active item differently from the rest", () => {
    renderNavAt("/sessions");
    // Tokens, not the whole class string: the contract is that active and
    // inactive rows are visually distinct, not the exact styling.
    expect(link("Sessions").classList.contains("bg-nav-on-bg")).toBe(true);
    expect(link("Chat").classList.contains("bg-nav-on-bg")).toBe(false);
    expect(link("Chat").classList.contains("text-ink2")).toBe(true);
  });

  it("brackets the active item and only the active item", () => {
    renderNavAt("/sessions");
    // Brackets render as four <i> corner marks.
    expect(link("Sessions").querySelectorAll("i").length).toBeGreaterThan(0);
    expect(link("Chat").querySelectorAll("i").length).toBe(0);
  });

  it("shows the alert dot only on routes that carry an alert", () => {
    renderNavAt("/", [
      route({ id: "chat", path: "/", label: "Chat" }),
      route({ id: "sessions", path: "/sessions", label: "Sessions", hasAlert: true }),
    ]);
    expect(link("Sessions").querySelector(".bg-alert")).not.toBeNull();
    expect(link("Chat").querySelector(".bg-alert")).toBeNull();
  });

  it("keeps each route in its own section, primary above secondary", () => {
    renderNavAt("/");
    // Cross-wired section filtering would reorder or duplicate rows.
    expect(screen.getAllByRole("link").map((l) => l.textContent)).toEqual([
      "Chat",
      "Sessions",
      "Settings",
    ]);
  });
});

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

  it("carries the unread count on the rail, where no row can", () => {
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {}, unreadCount: 3 });

    // The failure mode: a reply lands while the sidebar is narrow, the rows
    // that would have marked it are hidden, and nothing on screen says so.
    expect(screen.getByLabelText("Expand sidebar, 3 unread")).toBeTruthy();
    expect(screen.getByText("3")).toBeTruthy();
  });

  it("caps the count so the badge stays a badge", () => {
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {}, unreadCount: 12 });

    expect(screen.getByText("9+")).toBeTruthy();
  });

  it("caps only past nine — nine itself still fits", () => {
    // 9 is the last count the badge can say exactly; 10 is the first it cannot.
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {}, unreadCount: 9 });
    expect(screen.getByText("9")).toBeTruthy();
    expect(screen.queryByText("9+")).toBeNull();

    cleanup();
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {}, unreadCount: 10 });
    expect(screen.getByText("9+")).toBeTruthy();
  });

  it("tells assistive tech whether the sidebar is open", () => {
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {} });
    expect(screen.getByLabelText("Expand sidebar").getAttribute("aria-expanded")).toBe("false");

    cleanup();
    renderSidebar({ collapsed: false, onToggleCollapsed: () => {} });
    expect(screen.getByLabelText("Collapse sidebar").getAttribute("aria-expanded")).toBe("true");
  });

  it("points the chevron toward what clicking it will do", () => {
    // Expanded, the chevron is flipped to point at the collapse direction;
    // collapsed, it points back out. Same glyph, state carried by rotation.
    renderSidebar({ collapsed: false, onToggleCollapsed: () => {} });
    const expandedChevron = screen.getByLabelText("Collapse sidebar").querySelector("svg");
    expect(expandedChevron?.classList.contains("rotate-180")).toBe(true);

    cleanup();
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {} });
    const collapsedChevron = screen.getByLabelText("Expand sidebar").querySelector("svg");
    expect(collapsedChevron?.classList.contains("rotate-180")).toBe(false);
  });

  it("says nothing when nothing is unread", () => {
    renderSidebar({ collapsed: true, onToggleCollapsed: () => {}, unreadCount: 0 });

    expect(screen.getByLabelText("Expand sidebar")).toBeTruthy();
  });

  it("loses the count when a surface takes the badge without the toggle", () => {
    // The badge rides the toggle button, so a surface that adopts the count but
    // not collapse silently gets nothing. Pinned so that stays a decision.
    renderSidebar({ collapsed: true, unreadCount: 3 });

    expect(screen.queryByText("3")).toBeNull();
  });

  it("leaves the count off the expanded sidebar, where the rows already mark it", () => {
    renderSidebar({ collapsed: false, onToggleCollapsed: () => {}, unreadCount: 3 });

    expect(screen.queryByText("3")).toBeNull();
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

describe("Sidebar theme switch", () => {
  it("names the theme a click goes to, and flips it", () => {
    const onToggleTheme = vi.fn();
    const { rerender } = renderSidebar({ theme: "dark", onToggleTheme });
    fireEvent.click(screen.getByLabelText("Switch to light mode"));
    expect(onToggleTheme).toHaveBeenCalledTimes(1);
    rerender(
      <MemoryRouter>
        <Sidebar
          routes={[]}
          recents={[]}
          onOpenRecent={() => {}}
          onArchiveRecent={() => {}}
          onNewConversation={() => {}}
          onSeeAllConversations={() => {}}
          user={{ name: "Operator", initials: "OP" }}
          theme="light"
          onToggleTheme={onToggleTheme}
        />
      </MemoryRouter>,
    );
    expect(screen.getByLabelText("Switch to dark mode")).toBeTruthy();
    expect(screen.queryByLabelText("Switch to light mode")).toBeNull();
  });

  it("has no switch when the app does not wire one", () => {
    renderSidebar({ theme: "dark" });
    expect(screen.queryByLabelText(/Switch to/)).toBeNull();
  });
});
