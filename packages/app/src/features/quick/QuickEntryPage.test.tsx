import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, fakeNative, renderApp } from "../../test/harness";
import { QuickEntryPage } from "./QuickEntryPage";

const SESSIONS = {
  ListSessions: { sessions: [{ id: "recent-1" }] },
  Open: { sessionId: "recent-1" },
};

function mount(daemon = fakeDaemon(SESSIONS)) {
  const native = fakeNative();
  const view = renderApp(<QuickEntryPage />, { daemon, native });
  return { ...view, native, daemon };
}

const box = () => screen.getByPlaceholderText("Ask Borg anything");
const type = (text: string) => fireEvent.change(box(), { target: { value: text } });
const send = () => fireEvent.keyDown(box(), { key: "Enter" });

describe("QuickEntryPage", () => {
  it("takes focus so the hotkey lands in the box", () => {
    mount();
    expect(document.activeElement).toBe(box());
  });

  it("reclaims focus each time the panel comes back", () => {
    // The window is shown and hidden, not rebuilt, so mount-only focus would
    // work exactly once.
    mount();
    act(() => box().blur());
    expect(document.activeElement).not.toBe(box());

    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(document.activeElement).toBe(box());
  });

  it("resumes the most recent conversation rather than starting a new one", async () => {
    // A capture that opened a fresh session each time would shred the history.
    const { daemon } = mount();
    type("remember the milk");
    await act(async () => {
      send();
    });

    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "Open")?.request).toEqual({
        resumeId: "recent-1",
      }),
    );
  });

  it("opens a fresh conversation when there is nothing to resume", async () => {
    const daemon = fakeDaemon({ ListSessions: { sessions: [] }, Open: { sessionId: "new-1" } });
    mount(daemon);
    type("first thought");
    await act(async () => {
      send();
    });

    await waitFor(() => expect(daemon.calls.find((c) => c.method === "Open")?.request).toEqual({}));
  });

  it("sends the trimmed text, then gets out of the way", async () => {
    const { daemon, native } = mount();
    type("  ship it  ");
    await act(async () => {
      send();
    });

    await waitFor(() => expect(native.hidden).toBe(1));
    expect(daemon.streams.find((s) => s.method === "Send")?.request).toEqual({
      sessionId: "recent-1",
      text: "ship it",
      images: [],
    });
    // The reply belongs where the transcript is.
    expect(native.revealed).toEqual(["recent-1"]);
    expect(box()).toHaveProperty("value", "");
  });

  it("ignores an empty send", async () => {
    const { daemon } = mount();
    type("   ");
    await act(async () => {
      send();
    });
    expect(daemon.calls).toEqual([]);
  });

  it("leaves Shift+Enter to insert a newline", async () => {
    const { daemon } = mount();
    type("first line");
    await act(async () => {
      fireEvent.keyDown(box(), { key: "Enter", shiftKey: true });
    });
    expect(daemon.calls).toEqual([]);
  });

  it("does not send mid-composition", async () => {
    // An IME's Enter commits the candidate; sending there would truncate it.
    const { daemon } = mount();
    type("にほんご");
    await act(async () => {
      fireEvent.keyDown(box(), { key: "Enter", isComposing: true });
    });
    expect(daemon.calls).toEqual([]);
  });

  it("dismisses on Escape without sending", async () => {
    const { daemon, native } = mount();
    type("never mind");
    fireEvent.keyDown(box(), { key: "Escape" });
    expect(native.hidden).toBe(1);
    expect(daemon.calls).toEqual([]);
  });

  it("sends from the button too", async () => {
    const { daemon } = mount();
    type("ship it");
    await act(async () => {
      screen.getByRole("button", { name: "Send" }).click();
    });
    await waitFor(() => expect(daemon.streams.some((s) => s.method === "Send")).toBe(true));
  });

  it("keeps the text and says why when the send fails", async () => {
    // The panel closing on failure would lose what was typed.
    const daemon = fakeDaemon({
      ListSessions: new BorgError(GrpcStatus.Unavailable, "Borg is not running"),
    });
    const { native } = mount(daemon);
    type("ship it");
    await act(async () => {
      send();
    });

    await waitFor(() => expect(screen.getByText("Borg is not running")).toBeTruthy());
    expect(native.hidden).toBe(0);
    expect(box()).toHaveProperty("value", "ship it");
  });

  it("clears a stale failure when the panel is summoned again", async () => {
    const daemon = fakeDaemon({
      ListSessions: new BorgError(GrpcStatus.Unavailable, "Borg is not running"),
    });
    mount(daemon);
    type("ship it");
    await act(async () => {
      send();
    });
    await waitFor(() => expect(screen.getByText("Borg is not running")).toBeTruthy());

    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(screen.queryByText("Borg is not running")).toBeNull();
    expect(screen.getByText("Enter to send · Esc to dismiss")).toBeTruthy();
  });
});
