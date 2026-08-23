import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { ThemeProvider } from "@borg/ui";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { dispatchKeybind } from "../../lib/keybinds";
import { SettingsProvider } from "../../lib/settings";
import { CommandPalette } from "./CommandPalette";

const SETTINGS = {
  GetSettings: {
    entries: [
      {
        key: "heartbeat.enabled",
        value: "false",
        category: "db",
        label: "Let Borg act on its own",
        kind: "bool",
        group: "Autonomy",
      },
    ],
  },
  ListSessions: { sessions: [{ id: "s1", title: "Ship the daemon" }] },
  ListSkills: { skills: [] },
};

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon = fakeDaemon(SETTINGS)) {
  const view = renderApp(
    <>
      <CommandPalette />
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    {
      daemon,
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
  return { ...view, daemon };
}

const open = () => act(() => void dispatchKeybind("command-palette"));
const box = () => screen.getByPlaceholderText("Search commands, conversations and settings");
const type = (text: string) => fireEvent.change(box(), { target: { value: text } });
const rows = () => screen.getAllByRole("button").map((b) => b.textContent);

describe("CommandPalette", () => {
  it("stays closed until the chord is used", () => {
    mount();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens on the chord and closes on the same chord", () => {
    mount();
    open();
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeTruthy();
    open();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("takes Escape from the running turn while it is open", () => {
    // Otherwise Escape would cancel the turn behind the palette.
    mount();
    open();
    act(() => void dispatchKeybind("cancel-turn"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes when the backdrop is clicked, but not the panel", () => {
    mount();
    open();
    act(() => screen.getByRole("dialog").click());
    expect(screen.getByRole("dialog")).toBeTruthy();

    act(() => (screen.getByRole("dialog").parentElement as HTMLElement).click());
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens with a usable list rather than a spinner", async () => {
    // The settings registry is already loaded by the provider.
    mount();
    open();
    await waitFor(() => expect(screen.getByText("New conversation")).toBeTruthy());
  });

  it("filters as the user types", async () => {
    mount();
    open();
    await waitFor(() => expect(screen.getByText("New conversation")).toBeTruthy());

    type("usage");
    await waitFor(() => expect(rows().some((r) => r?.includes("Usage"))).toBe(true));
    expect(rows().some((r) => r?.includes("New conversation"))).toBe(false);
  });

  it("says when nothing matches", async () => {
    mount();
    open();
    type("zzzzzz");
    await waitFor(() => expect(screen.getByText("Nothing matches.")).toBeTruthy());
  });

  it("runs the highlighted row on Enter", async () => {
    mount();
    open();
    type("usage");
    await waitFor(() => expect(rows().some((r) => r?.includes("Usage"))).toBe(true));

    fireEvent.keyDown(box(), { key: "Enter" });
    expect(screen.getByTestId("path").textContent).toBe("/usage");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("walks the list with the arrow keys", async () => {
    mount();
    open();
    await waitFor(() => expect(screen.getByText("New conversation")).toBeTruthy());

    fireEvent.keyDown(box(), { key: "ArrowDown" });
    fireEvent.keyDown(box(), { key: "ArrowDown" });
    fireEvent.keyDown(box(), { key: "ArrowUp" });
    fireEvent.keyDown(box(), { key: "Enter" });
    // Whatever the second row is, Enter must have taken a row and closed.
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("does not walk past either end of the list", async () => {
    mount();
    open();
    type("usage");
    await waitFor(() => expect(rows().some((r) => r?.includes("Usage"))).toBe(true));

    for (let i = 0; i < 5; i++) fireEvent.keyDown(box(), { key: "ArrowUp" });
    for (let i = 0; i < 5; i++) fireEvent.keyDown(box(), { key: "ArrowDown" });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(screen.getByTestId("path").textContent).toBe("/usage");
  });

  it("forgets the query when it closes", async () => {
    mount();
    open();
    type("usage");
    open();
    open();
    expect(box()).toHaveProperty("value", "");
  });

  it("runs a row picked with the mouse", async () => {
    mount();
    open();
    type("usage");
    await waitFor(() => expect(rows().some((r) => r?.includes("Usage"))).toBe(true));

    const usage = screen.getAllByRole("button").find((b) => b.textContent?.includes("Usage"))!;
    act(() => usage.click());
    expect(screen.getByTestId("path").textContent).toBe("/usage");
  });

  it("keeps a toggle row open so the change can be watched land", async () => {
    const daemon = fakeDaemon({ ...SETTINGS, SetSetting: {} });
    mount(daemon);
    open();
    await waitFor(() => expect(screen.getByText("Let Borg act on its own")).toBeTruthy());

    daemon.answer("GetSettings", {
      entries: [{ ...SETTINGS.GetSettings.entries[0], value: "true" }],
    });
    act(() => screen.getByText("Let Borg act on its own").click());

    await waitFor(() => expect(screen.getByText("on")).toBeTruthy());
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("says a toggle failed, rather than looking merely slow", async () => {
    // The row stays open to show the state change; a silent failure is
    // indistinguishable from a slow write.
    const daemon = fakeDaemon({
      ...SETTINGS,
      SetSetting: new BorgError(GrpcStatus.Unavailable, "daemon is not running."),
    });
    mount(daemon);
    open();
    await waitFor(() => expect(screen.getByText("Let Borg act on its own")).toBeTruthy());

    act(() => screen.getByText("Let Borg act on its own").click());
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain("Check that the Borg app is running"),
    );
  });

  it("drops a stale failure when it is reopened", async () => {
    const daemon = fakeDaemon({
      ...SETTINGS,
      SetSetting: new BorgError(GrpcStatus.Unavailable, "daemon is not running."),
    });
    mount(daemon);
    open();
    await waitFor(() => expect(screen.getByText("Let Borg act on its own")).toBeTruthy());
    act(() => screen.getByText("Let Borg act on its own").click());
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());

    open();
    open();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
