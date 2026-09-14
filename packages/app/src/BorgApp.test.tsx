import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { BorgError, GrpcStatus } from "@borg/client";
import { BorgApp } from "./BorgApp";
import { QUICK_ENTRY_HASH } from "./lib/chords";
import { dispatchKeybind } from "./lib/keybinds";
import { clearNotice } from "./lib/notice";
import { STORAGE_KEYS } from "@borg/ui";
import { fakeDaemon, fakeNative } from "./test/harness";
import { setupStatus } from "./test/setupStatus";
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
  GetSetupStatus: setupStatus(),
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
  // The undo slot is module state shared by the whole shell; a leftover offer
  // from the previous test would put an Undo button on a fresh mount.
  clearNotice();
});

afterEach(() => {
  mobile.set(false);
  window.location.hash = "";
});

describe("BorgApp shell", () => {
  it("opens on the conversation with the navigation around it", async () => {
    mount();
    await waitFor(() =>
      // By role: the page keeps the same word as its title until the first
      // turn loads, so plain text matching is ambiguous for a moment.
      expect(screen.getByRole("link", { name: "Conversation" })).toBeTruthy(),
    );
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
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

  it("hands an archived conversation back without a confirmation dialog", async () => {
    // The archive goes through immediately — Borg does not put a dialog in
    // front of a reversible act — so the way back has to be on screen after it.
    const daemon = fakeDaemon({ ...BASE, SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    // Named, so two archives in a row are told apart.
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("Ship the daemon"),
    );

    await act(async () => screen.getByRole("button", { name: "Undo" }).click());
    await waitFor(() =>
      expect(
        daemon.calls.filter((c) => c.method === "SetSessionArchived").map((c) => c.request),
      ).toEqual([
        { id: "s1", archived: true },
        { id: "s1", archived: false },
      ]),
    );
    // The offer is spent; leaving it up would invite a second restore.
    expect(screen.queryByRole("button", { name: "Undo" })).toBeNull();
  });

  it("puts the row back on the conversations page, not just in the daemon", async () => {
    // The shell owns the toast, so the shell performs the restore — but the
    // page runs its own listing query. Without a shared invalidation the RPC
    // lands and the user, standing on /sessions, sees nothing change at all.
    // On a phone there is no sidebar, so that would be the whole outcome.
    const archived = new Set<string>();
    const daemon = fakeDaemon({
      ...BASE,
      ListSessions: (request: unknown) => ({
        sessions: [session()].filter(
          (s) =>
            !archived.has(s.id) ||
            (request as { includeArchived?: boolean }).includeArchived === true,
        ),
      }),
      SetSessionArchived: (request: unknown) => {
        const { id, archived: want } = request as { id: string; archived: boolean };
        if (want) archived.add(id);
        else archived.delete(id);
        return {};
      },
    });
    window.location.hash = "#/sessions";
    mount(daemon);

    // One row in the sidebar, one on the page. Counting both is what makes the
    // assertions discriminating: a count of 1 after archiving means exactly one
    // surface refreshed, which is the bug.
    const shown = () => screen.queryAllByText("Ship the daemon").length;
    await waitFor(() => expect(shown()).toBe(2));

    // Archived from the sidebar, so both directions of the cross-surface
    // refresh are under test: the page must drop the row, then take it back.
    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    await waitFor(() => expect(shown()).toBe(0));

    await act(async () => screen.getByRole("button", { name: "Undo" }).click());
    await waitFor(() => expect(shown()).toBe(2));
  });

  it("says so when the way back fails instead of letting it pass for success", async () => {
    // The row is already gone from the list. A silent failure here leaves the
    // user believing the conversation came back when it did not.
    const daemon = fakeDaemon({
      ...BASE,
      // The archive lands; only the restore behind Undo fails.
      SetSessionArchived: (request: unknown) =>
        (request as { archived: boolean }).archived
          ? {}
          : new BorgError(GrpcStatus.Internal, "database is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Undo" })).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Undo" }).click());
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("database is locked"),
    );
    // Nothing left to press — the offer was spent and the restore failed.
    expect(screen.queryByRole("button", { name: "Undo" })).toBeNull();
  });

  it("offers no way back when the archive never happened", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      SetSessionArchived: new BorgError(GrpcStatus.Internal, "database is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    await act(async () => screen.getByRole("button", { name: "Archive Ship the daemon" }).click());
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Undo" })).toBeNull();
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
    await waitFor(() =>
      // By role: the page keeps the same word as its title until the first
      // turn loads, so plain text matching is ambiguous for a moment.
      expect(screen.getByRole("link", { name: "Conversation" })).toBeTruthy(),
    );
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
    await waitFor(() =>
      // By role: the page keeps the same word as its title until the first
      // turn loads, so plain text matching is ambiguous for a moment.
      expect(screen.getByRole("link", { name: "Conversation" })).toBeTruthy(),
    );
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
