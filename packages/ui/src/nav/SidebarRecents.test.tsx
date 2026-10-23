import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { Sidebar, type SidebarProps } from "./Sidebar";
import { ChatIcon } from "../components/icons";

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
      {
        id: "settings",
        path: "/settings",
        label: "Settings",
        tabLabel: "Settings",
        icon: ChatIcon,
        page: () => null,
        section: "secondary",
      },
      {
        id: "detail",
        path: "/settings/:id",
        label: "Setting",
        tabLabel: "Setting",
        icon: ChatIcon,
        page: () => null,
        section: "secondary",
        nav: { sidebar: false },
      },
    ],
    recents: [
      { id: "s1", title: "a past conversation" },
      { id: "s2", title: "one still running", activity: "working" },
    ],
    onOpenRecent: vi.fn(),
    onArchiveRecent: vi.fn(),
    onNewConversation: vi.fn(),
    onSeeAllConversations: vi.fn(),
    user: { name: "Operator", initials: "OP" },
    ...over,
  };
  render(
    <MemoryRouter>
      <Sidebar {...props} />
    </MemoryRouter>,
  );
  return props;
}

describe("Sidebar recents", () => {
  it("opens the conversation the row names", () => {
    const props = renderSidebar();
    fireEvent.click(screen.getByText("a past conversation"));
    expect(props.onOpenRecent).toHaveBeenCalledWith("s1");
  });

  it("archives without opening the conversation being closed", () => {
    // The archive button sits inside a row whose own click opens the conversation.
    const props = renderSidebar();
    fireEvent.click(screen.getByRole("button", { name: "Archive a past conversation" }));
    expect(props.onArchiveRecent).toHaveBeenCalledWith("s1");
    expect(props.onOpenRecent).not.toHaveBeenCalled();
  });

  it("pins a loose conversation and unpins a pinned one from the row", () => {
    const props = renderSidebar({
      onPinRecent: vi.fn(),
      recents: [
        { id: "a", title: "loose one" },
        { id: "b", title: "kept one", pinned: true },
      ],
    });
    fireEvent.click(screen.getByRole("button", { name: "Pin loose one" }));
    fireEvent.click(screen.getByRole("button", { name: "Unpin kept one" }));
    expect(props.onPinRecent).toHaveBeenNthCalledWith(1, "a", true);
    expect(props.onPinRecent).toHaveBeenNthCalledWith(2, "b", false);
    // Like archiving, pinning acts on the row without opening it.
    expect(props.onOpenRecent).not.toHaveBeenCalled();
  });

  it("offers no pin when the app cannot pin, and no × at all", () => {
    renderSidebar();
    expect(screen.queryByRole("button", { name: /^Pin / })).toBeNull();
    expect(screen.queryByText("×")).toBeNull();
  });

  it("hands a right-click to the app with the pointer position", () => {
    const onRecentContextMenu = vi.fn();
    renderSidebar({ onRecentContextMenu });
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clientX", { value: 210 });
    Object.defineProperty(event, "clientY", { value: 140 });
    fireEvent(screen.getByText("one still running"), event);

    expect(onRecentContextMenu).toHaveBeenCalledWith("s2", { x: 210, y: 140 });
    // The browser menu must not open on top of the app's own.
    expect(event.defaultPrevented).toBe(true);
  });

  it("leaves right-click to the browser when the app has no menu", () => {
    renderSidebar();
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    fireEvent(screen.getByText("a past conversation"), event);
    expect(event.defaultPrevented).toBe(false);
  });

  it("says why an archive failed, rather than leaving a dead button", () => {
    renderSidebar({ archiveError: "daemon not running" });
    expect(screen.getByRole("alert").textContent).toContain("daemon not running");
  });

  it("shows no failure line when nothing failed", () => {
    renderSidebar();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("marks a conversation's state for a reader who cannot see the dot", () => {
    renderSidebar({
      recents: [
        { id: "a", title: "working one", activity: "working" },
        { id: "b", title: "unread one", activity: "unread" },
        { id: "c", title: "draft one", activity: "draft" },
        { id: "d", title: "settled one" },
      ],
    });
    expect(screen.getByRole("img", { name: "Working" })).toBeTruthy();
    expect(screen.getByRole("img", { name: "Unread reply" })).toBeTruthy();
    expect(screen.getByRole("img", { name: "Unsent draft" })).toBeTruthy();
    // A settled conversation gets no mark at all — the dot picks rows out of a
    // list, it does not decorate every row.
    expect(screen.getAllByRole("img")).toHaveLength(3);
  });

  it("puts the mark after the title, at the end of the row", () => {
    renderSidebar({ recents: [{ id: "b", title: "unread one", activity: "unread" }] });
    // The accessible name follows document order, so this pins the layout too.
    expect(screen.getByRole("button", { name: /^unread one\s*Unread reply$/ })).toBeTruthy();
  });

  it("reaches the older conversations the capped list leaves out", () => {
    const props = renderSidebar();
    fireEvent.click(screen.getByText("Show all…"));
    expect(props.onSeeAllConversations).toHaveBeenCalledTimes(1);
  });

  it("starts a fresh conversation", () => {
    const props = renderSidebar();
    fireEvent.click(screen.getByRole("button", { name: "New conversation" }));
    expect(props.onNewConversation).toHaveBeenCalledTimes(1);
  });

  // Keyboard reach. A `div` with an onClick is invisible to Tab, so a person
  // who does not use a mouse could see the list and never open anything in
  // it. Both controls are native buttons: the browser turns Enter and Space
  // into the click the test fires, and the tab order comes for free.
  it.each([
    ["New conversation", "onNewConversation", undefined],
    ["a past conversation", "onOpenRecent", "s1"],
  ] as const)("reaches %s from the keyboard", (name, prop, arg) => {
    const props = renderSidebar();
    const control = screen.getByRole("button", { name });
    expect(control).toBeInstanceOf(HTMLButtonElement);
    control.focus();
    expect(document.activeElement).toBe(control);
    fireEvent.click(control);
    if (arg === undefined) expect(props[prop]).toHaveBeenCalledTimes(1);
    else expect(props[prop]).toHaveBeenCalledWith(arg);
  });

  it("keeps the row's dot inside the control that opens it", () => {
    // The state must be read out together with the title, not as a separate
    // stop in the tab order.
    renderSidebar();
    const row = screen.getByText("one still running").closest("button");
    expect(row?.querySelector('[role="img"]')).not.toBeNull();
  });

  it("keeps the archive button a separate tab stop from the row", () => {
    // A button cannot contain a button; if it were nested it would either
    // vanish from the tab order or open the conversation it is closing.
    renderSidebar();
    const open = screen.getByRole("button", { name: "a past conversation" });
    const archive = screen.getByRole("button", { name: "Archive a past conversation" });
    expect(open.contains(archive)).toBe(false);
    expect(archive.contains(open)).toBe(false);
  });

  it("lists only the routes that asked to be in the sidebar", () => {
    renderSidebar();
    expect(screen.getByText("Chat")).toBeTruthy();
    expect(screen.getByText("Settings")).toBeTruthy();
    expect(screen.queryByText("Setting")).toBeNull();
  });

  it("names the signed-in user", () => {
    renderSidebar();
    expect(screen.getByText("Operator")).toBeTruthy();
    expect(screen.getByText("OP")).toBeTruthy();
  });
});
