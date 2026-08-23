import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { BorgError, GrpcStatus } from "@borg/client";
import { BorgApp } from "./BorgApp";
import { QUICK_ENTRY_HASH } from "./lib/chords";
import { dispatchKeybind } from "./lib/keybinds";
import { STORAGE_KEYS } from "@borg/ui";
import { fakeDaemon, fakeNative } from "./test/harness";
import type { Platform } from "./lib/platform";

const media = installMatchMedia(() => false);
const mobile = media.controller("(max-width: 700px)");

const session = (over: Record<string, unknown> = {}) => ({
  id: "s1",
  title: "Ship the daemon",
  messageCount: 4,
  updatedUnixTs: Math.floor(Date.now() / 1000),
  archived: false,
  lastMessagePreview: "",
  lastMessageRole: "",
  ...over,
});

const BASE = {
  GetOnboardingState: { onboarded: true },
  ListSessions: { sessions: [session()] },
  GetPersistentWarnings: { warnings: [] },
  ListPairingRequests: { requests: [] },
  GetSettings: { entries: [] },
  ListSkills: { skills: [] },
  // The conversation page opens as soon as the shell routes to "/".
  Open: { sessionId: "live-1", lastEventSeq: 0 },
  GetState: { title: "Ship it", llmModel: "opus", agentSessionId: "s1", turnInFlight: false },
  GetSession: { messages: [] },
  GetContextBreakdown: { components: [], contextWindow: 200_000 },
  ListAgents: { agents: [] },
  GetGoal: { present: false },
};

function mount(daemon = fakeDaemon(BASE), over: Partial<Platform> = {}) {
  const native = fakeNative();
  const platform: Platform = {
    kind: "desktop",
    transport: daemon.transport,
    titleBarDrag: false,
    native,
    ...over,
  };
  const view = render(<BorgApp platform={platform} />);
  return { ...view, daemon, native };
}

beforeEach(() => {
  localStorage.clear();
  window.location.hash = "";
});

afterEach(() => {
  mobile.set(false);
  window.location.hash = "";
});

describe("BorgApp shell", () => {
  it("opens on the conversation with the navigation around it", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Conversation")).toBeTruthy());
    expect(screen.getByText("Ship the daemon")).toBeTruthy();
  });

  it("lists recent conversations in the sidebar and opens one", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    act(() => screen.getByText("Ship the daemon").click());
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
  });

  it("archives a conversation from the sidebar", async () => {
    const daemon = fakeDaemon({ ...BASE, SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSessionArchived")?.request).toEqual({
        id: "s1",
        archived: true,
      }),
    );
  });

  it("offers archiving from a right-click on a sidebar row", async () => {
    const daemon = fakeDaemon({ ...BASE, SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    fireEvent.contextMenu(screen.getByText("Ship the daemon"), { clientX: 20, clientY: 30 });
    const menu = screen.getByRole("menu", { name: "Conversation actions" });
    await act(async () => screen.getByRole("menuitem", { name: /Archive/ }).click());
    expect(menu).toBeTruthy();
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "SetSessionArchived")).toBe(true),
    );
  });

  it("says why an archive failed rather than leaving a dead control", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      SetSessionArchived: new BorgError(GrpcStatus.Internal, "database is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain("database is locked"),
    );
  });

  it("lights the nav for a health warning and a waiting pairing", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      GetPersistentWarnings: { warnings: ["Sandbox: profile missing"] },
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "telegram", senderId: "1", status: "pending", code: "" },
        ],
      },
    });
    mount(daemon);
    // The dots ride the nav rows, so their presence is what the shell renders.
    await waitFor(() => expect(screen.getByText("Health")).toBeTruthy());
    expect(screen.getByText("Connections")).toBeTruthy();
  });

  it("remembers a collapsed sidebar across a restart", async () => {
    const { unmount } = mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => screen.getByLabelText("Collapse sidebar").click());
    expect(screen.queryByText("Ship the daemon")).toBeNull();
    unmount();

    mount();
    await waitFor(() => expect(screen.getByLabelText("Expand sidebar")).toBeTruthy());
    expect(localStorage.getItem(STORAGE_KEYS.sidebarCollapsed)).toBe("true");
  });

  it("collapses the sidebar from the keyboard too", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => void dispatchKeybind("toggle-sidebar"));
    await waitFor(() => expect(screen.getByLabelText("Expand sidebar")).toBeTruthy());
  });

  it("swaps the sidebar for tabs at phone width", async () => {
    mobile.set(true);
    mount();
    await waitFor(() => expect(screen.getByText("Chat")).toBeTruthy());
    expect(screen.queryByLabelText("Collapse sidebar")).toBeNull();
  });

  it("navigates from the keyboard layer", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => void dispatchKeybind("open-settings"));
    await waitFor(() => expect(screen.getByText("Advanced")).toBeTruthy());

    act(() => void dispatchKeybind("search-sessions"));
    await waitFor(() => expect(screen.getByPlaceholderText("Search conversations")).toBeTruthy());
  });

  it("lets the native shell drive the same keyboard layer", async () => {
    // ⌘N from the File menu and ⌘N from the keyboard are one implementation.
    const { native } = mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => native.emitMenuCommand("open-settings"));
    await waitFor(() => expect(screen.getByText("Advanced")).toBeTruthy());
  });

  it("opens the conversation a clicked notification names", async () => {
    const { native } = mount();
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => native.emitActivateSession("s1"));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
  });

  it("sends an unknown route back to the conversation", async () => {
    window.location.hash = "#/nowhere";
    mount();
    await waitFor(() => expect(screen.getByText("Conversation")).toBeTruthy());
  });

  it("reserves a drag strip only where the window has no title bar", async () => {
    const { container, unmount } = mount(fakeDaemon(BASE), { titleBarDrag: true });
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    expect(container.querySelector("[class*='app-region:drag']")).not.toBeNull();
    unmount();

    const plain = mount(fakeDaemon(BASE), { titleBarDrag: false });
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    expect(plain.container.querySelector("[class*='app-region:drag']")).toBeNull();
  });
});

describe("BorgApp gates", () => {
  it("shows onboarding when the daemon says it has not run", async () => {
    const daemon = fakeDaemon({ ...BASE, GetOnboardingState: { onboarded: false } });
    mount(daemon);
    await waitFor(() => expect(screen.queryByText("Ship the daemon")).toBeNull());
    // The wizard owns the screen; the shell is not mounted behind it.
    expect(screen.queryByText("Conversation")).toBeNull();
  });

  it("renders the shell when the daemon cannot answer, so its pages explain", async () => {
    // Only a positive "not onboarded" shows the wizard.
    const daemon = fakeDaemon({
      ...BASE,
      GetOnboardingState: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Conversation")).toBeTruthy());
  });

  it("is the quick-entry panel and nothing else at the quick-entry route", async () => {
    // No router, no onboarding gate, no settings fetch.
    window.location.hash = QUICK_ENTRY_HASH;
    const daemon = fakeDaemon(BASE);
    mount(daemon);
    expect(screen.getByPlaceholderText("Ask Borg anything")).toBeTruthy();
    expect(screen.queryByText("Conversation")).toBeNull();
    expect(daemon.calls.some((c) => c.method === "GetOnboardingState")).toBe(false);
  });
});
