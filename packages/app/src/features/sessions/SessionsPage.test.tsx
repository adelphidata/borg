import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { clearNotice, pendingNotice } from "../../lib/notice";
import { resetActivity } from "./activityStore";
import { SessionsPage } from "./SessionsPage";

const NOW = Date.UTC(2026, 7, 22, 12, 0, 0);

const summary = (over: Record<string, unknown> = {}) => ({
  id: "s1",
  title: "Ship the daemon",
  messageCount: 4,
  updatedUnixTs: Math.floor(NOW / 1000) - 60,
  archived: false,
  lastMessagePreview: "",
  lastMessageRole: "",
  ...over,
});

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <>
      <SessionsPage />
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    { daemon },
  );
}

const listing = (sessions: unknown[]) => ({ ListSessions: { sessions } });
const rows = () => screen.getAllByRole("button", { name: /./ });

beforeEach(() => {
  resetActivity();
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
  // The notice slot is module state shared with the shell. Vitest isolates
  // files by default, but that is a config setting, not a property of the code.
  clearNotice();
});

describe("SessionsPage listing", () => {
  it("lists conversations under a heading a person would use", async () => {
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    expect(screen.getByText("Today")).toBeTruthy();
  });

  it("shows the last thing said rather than a message count", async () => {
    const daemon = fakeDaemon(
      listing([summary({ lastMessagePreview: "deploy is green", lastMessageRole: "assistant" })]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("deploy is green")).toBeTruthy());
  });

  it("attributes the user's own last message", async () => {
    const daemon = fakeDaemon(
      listing([summary({ lastMessagePreview: "ship it", lastMessageRole: "user" })]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("You: ship it")).toBeTruthy());
  });

  it("falls back to the count for a conversation with nothing to quote", async () => {
    const daemon = fakeDaemon(listing([summary({ messageCount: 1 })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("1 message")).toBeTruthy());
  });

  it("says the list is empty rather than showing nothing at all", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("No conversations yet.")).toBeTruthy());
  });

  it("reports a failed listing", async () => {
    const daemon = fakeDaemon({
      ListSessions: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() =>
      expect(screen.getByText("Could not load your conversations.")).toBeTruthy(),
    );
  });

  it("opens the conversation a row names", async () => {
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => screen.getByText("Ship the daemon").click());
    expect(screen.getByTestId("path").textContent).toBe("/c/s1");
  });

  it("opens a row from the keyboard", async () => {
    // The row is a div; without the key handler it is unreachable without a
    // mouse.
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]')!;
    fireEvent.keyDown(row, { key: "Enter" });
    expect(screen.getByTestId("path").textContent).toBe("/c/s1");
  });

  it("starts a new conversation", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("No conversations yet.")).toBeTruthy());

    act(() => screen.getByRole("button", { name: "New conversation" }).click());
    expect(screen.getByTestId("path").textContent).toBe("/c/new");
  });
});

describe("SessionsPage search", () => {
  it("filters by title, case-insensitively", async () => {
    const daemon = fakeDaemon(
      listing([summary(), summary({ id: "s2", title: "Rewrite the parser" })]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Rewrite the parser")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("Search conversations"), {
      target: { value: "PARSER" },
    });
    expect(screen.queryByText("Ship the daemon")).toBeNull();
    expect(screen.getByText("Rewrite the parser")).toBeTruthy();
  });

  it("says what matched nothing, quoting the query back", async () => {
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("Search conversations"), {
      target: { value: "zzz" },
    });
    expect(screen.getByText("Nothing matches “zzz”.")).toBeTruthy();
  });

  it("asks the daemon for archived rows once a search begins", async () => {
    // Someone hunting for a conversation they archived types its name.
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    expect(daemon.calls.at(-1)?.request).toEqual({ includeArchived: false });

    fireEvent.change(screen.getByPlaceholderText("Search conversations"), {
      target: { value: "old" },
    });
    await waitFor(() => expect(daemon.calls.at(-1)?.request).toEqual({ includeArchived: true }));
  });
});

describe("SessionsPage archive", () => {
  const withArchived = () =>
    fakeDaemon({
      ListSessions: (request: unknown) =>
        (request as { includeArchived: boolean }).includeArchived
          ? { sessions: [summary(), summary({ id: "s2", title: "Old plan", archived: true })] }
          : { sessions: [summary()] },
      SetSessionArchived: {},
    });

  it("keeps archived conversations behind a disclosure", async () => {
    const daemon = withArchived();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    expect(screen.queryByText("Old plan")).toBeNull();

    act(() => screen.getByRole("button", { name: /Archived/ }).click());
    await waitFor(() => expect(screen.getByText("Old plan")).toBeTruthy());
  });

  it("says the archive is empty rather than showing a blank section", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    act(() => screen.getByRole("button", { name: /Archived/ }).click());
    await waitFor(() => expect(screen.getByText("Nothing archived.")).toBeTruthy());
  });

  it("reveals archived matches during a search, and can still be closed", async () => {
    // The header stays a control: an auto-opened section that could not be
    // closed would be a disclosure that stopped working.
    const daemon = withArchived();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("Search conversations"), {
      target: { value: "Old" },
    });
    await waitFor(() => expect(screen.getByText("Old plan")).toBeTruthy());
    expect(screen.getByRole("button", { name: /matches “Old”/ })).toBeTruthy();

    act(() => screen.getByRole("button", { name: /Archived/ }).click());
    await waitFor(() => expect(screen.queryByText("Old plan")).toBeNull());
  });

  it("un-archives a conversation as it is opened, where the move can be seen", async () => {
    const daemon = withArchived();
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    act(() => screen.getByRole("button", { name: /Archived/ }).click());
    await waitFor(() => expect(screen.getByText("Old plan")).toBeTruthy());

    act(() => screen.getByText("Old plan").click());
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSessionArchived")?.request).toEqual({
        id: "s2",
        archived: false,
      }),
    );
    expect(screen.getByTestId("path").textContent).toBe("/c/s2");
  });
});

describe("SessionsPage row actions", () => {
  it("renames a conversation and refreshes the list", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionTitle: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Rename").click());

    const input = screen.getByDisplayValue("Ship the daemon");
    fireEvent.change(input, { target: { value: "Ship it" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSessionTitle")?.request).toEqual({
        id: "s1",
        title: "Ship it",
      }),
    );
  });

  it("does not write a rename that changed nothing", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionTitle: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Rename").click());
    fireEvent.keyDown(screen.getByDisplayValue("Ship the daemon"), { key: "Enter" });

    expect(daemon.calls.some((c) => c.method === "SetSessionTitle")).toBe(false);
  });

  it("abandons a rename on Escape", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionTitle: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Rename").click());

    const input = screen.getByDisplayValue("Ship the daemon");
    fireEvent.change(input, { target: { value: "nope" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.getByText("Ship the daemon")).toBeTruthy();
    expect(daemon.calls.some((c) => c.method === "SetSessionTitle")).toBe(false);
  });

  it("archives from the row menu", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Archive").click());
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSessionArchived")?.request).toEqual({
        id: "s1",
        archived: true,
      }),
    );
  });

  it("names the row it archived, so the way back says which conversation", async () => {
    const daemon = fakeDaemon({ ...listing([summary()]), SetSessionArchived: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Archive").click());

    await waitFor(() => expect(pendingNotice()?.message).toBe("Archived “Ship the daemon”"));
    expect(pendingNotice()?.sessionId).toBe("s1");
  });

  it("offers nothing to undo when a conversation comes back", async () => {
    // Un-archiving needs no way back — the row reappears where it went, so a
    // toast would just be noise.
    const daemon = fakeDaemon({
      ListSessions: (request: unknown) =>
        (request as { includeArchived: boolean }).includeArchived
          ? { sessions: [summary(), summary({ id: "s2", title: "Old plan", archived: true })] }
          : { sessions: [summary()] },
      SetSessionArchived: {},
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());
    act(() => screen.getByRole("button", { name: /Archived/ }).click());
    await waitFor(() => expect(screen.getByText("Old plan")).toBeTruthy());

    act(() => screen.getByText("Old plan").click());
    await waitFor(() =>
      expect(daemon.calls.some((c: { method: string }) => c.method === "SetSessionArchived")).toBe(
        true,
      ),
    );
    expect(pendingNotice()).toBeUndefined();
  });

  it("surfaces a failed action instead of silently doing nothing", async () => {
    const daemon = fakeDaemon({
      ...listing([summary()]),
      SetSessionArchived: new BorgError(GrpcStatus.Internal, "database is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
    act(() => within(screen.getByRole("menu")).getByText("Archive").click());
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain("database is locked"),
    );
  });

  it("keeps the browser's own menu out of the way of the app's", async () => {
    const daemon = fakeDaemon(listing([summary()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

    const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    fireEvent(row, event);
    expect(event.defaultPrevented).toBe(true);
  });
});

describe("SessionsPage export", () => {
  it("hands the transcript to the browser as a download", async () => {
    // Both surfaces can do this, so there is no native-only path to maintain.
    const createObjectURL = vi.fn(() => "blob:transcript");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", { ...URL, createObjectURL, revokeObjectURL });
    const clicks: HTMLAnchorElement[] = [];
    const realClick = HTMLAnchorElement.prototype.click;
    HTMLAnchorElement.prototype.click = function () {
      clicks.push(this);
    };
    try {
      const daemon = fakeDaemon({
        ...listing([summary()]),
        ExportSession: {
          content: new Uint8Array([1, 2, 3]),
          suggestedFilename: "ship-the-daemon.md",
        },
      });
      mount(daemon);
      await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

      const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
      fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
      // Fake timers only from here: the export's deferred revoke is the point,
      // and everything before this needs the real clock to settle.
      vi.useFakeTimers();
      await act(async () => {
        within(screen.getByRole("menu"))
          .getByText(/Export/)
          .click();
      });
      await act(async () => {});

      expect(clicks).toHaveLength(1);
      expect(clicks[0].download).toBe("ship-the-daemon.md");
      expect(clicks[0].href).toContain("blob:transcript");

      // Revoking in the same tick can pull the blob out from under a download
      // that `click()` has only scheduled.
      expect(revokeObjectURL).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(30_000);
      });
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:transcript");
    } finally {
      HTMLAnchorElement.prototype.click = realClick;
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("names the file itself when the daemon suggests nothing", async () => {
    const createObjectURL = vi.fn(() => "blob:transcript");
    vi.stubGlobal("URL", { ...URL, createObjectURL, revokeObjectURL: vi.fn() });
    const clicks: HTMLAnchorElement[] = [];
    const realClick = HTMLAnchorElement.prototype.click;
    HTMLAnchorElement.prototype.click = function () {
      clicks.push(this);
    };

    try {
      const daemon = fakeDaemon({
        ...listing([summary()]),
        ExportSession: { content: new Uint8Array([1]), suggestedFilename: "" },
      });
      mount(daemon);
      await waitFor(() => expect(screen.getByText("Ship the daemon")).toBeTruthy());

      const row = screen.getByText("Ship the daemon").closest('[role="button"]') as HTMLElement;
      fireEvent.contextMenu(row, { clientX: 10, clientY: 10 });
      act(() =>
        within(screen.getByRole("menu"))
          .getByText(/Export/)
          .click(),
      );

      await waitFor(() => expect(clicks).toHaveLength(1));
      expect(clicks[0].download).toMatch(/^session\./);
    } finally {
      HTMLAnchorElement.prototype.click = realClick;
      vi.unstubAllGlobals();
    }
  });
});
