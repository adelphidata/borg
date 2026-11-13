import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp, type UnaryTable } from "../../test/harness";
import { status } from "../../test/meetings";
import { meetingsChanged } from "../meetings/meetingsStore";
import { CalendarPage } from "./CalendarPage";

function mount(table: UnaryTable = {}) {
  const daemon = fakeDaemon({ GetMeetingsStatus: status(), ...table });
  renderApp(<CalendarPage />, { daemon, route: "/connections/calendar" });
  return daemon;
}

const connects = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.calls.filter((c) => c.method === "ConnectCalendar").map((c) => c.request);

const google = status({
  calendar: { connected: true, kind: "google", accountLabel: "me@example.com" },
});

afterEach(() => vi.restoreAllMocks());

describe("CalendarPage", () => {
  it("connects Google with the user's own client, opens the sign-in, and waits", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const daemon = mount({
      ConnectCalendar: {
        authUrl: "https://accounts.google.com/o/oauth2/v2/auth?x",
        statusMessage: "",
      },
    });
    fireEvent.change(await screen.findByLabelText("Client ID"), {
      target: { value: " 123.apps.googleusercontent.com " },
    });
    fireEvent.change(screen.getByLabelText("Client secret"), { target: { value: "GOCSPX-s" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    expect(await screen.findByText("Finish signing in in your browser…")).toBeTruthy();
    expect(connects(daemon)).toEqual([
      {
        kind: "google",
        icsUrl: "",
        googleClientId: "123.apps.googleusercontent.com",
        googleClientSecret: "GOCSPX-s",
      },
    ]);
    expect(open).toHaveBeenCalledWith(
      "https://accounts.google.com/o/oauth2/v2/auth?x",
      "_blank",
      "noopener,noreferrer",
    );
  });

  it("shows the account once the daemon reports the calendar connected", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    const daemon = mount({
      ConnectCalendar: { authUrl: "https://accounts.google.com/x", statusMessage: "" },
    });
    fireEvent.change(await screen.findByLabelText("Client ID"), { target: { value: "id" } });
    fireEvent.change(screen.getByLabelText("Client secret"), { target: { value: "secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    await screen.findByText("Finish signing in in your browser…");
    // The feed's CalendarChanged bumps the shared counter; the page re-reads.
    daemon.answer("GetMeetingsStatus", google);
    act(() => meetingsChanged());
    expect(await screen.findByText("me@example.com")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Google Calendar" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Disconnect" })).toBeTruthy();
  });

  it("stops waiting and says why when the sign-in fails after the browser step", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    const daemon = mount({
      ConnectCalendar: { authUrl: "https://accounts.google.com/x", statusMessage: "" },
    });
    fireEvent.change(await screen.findByLabelText("Client ID"), { target: { value: "id" } });
    fireEvent.change(screen.getByLabelText("Client secret"), { target: { value: "typo" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    await screen.findByText("Finish signing in in your browser…");
    const feed = daemon.streams.filter((s) => s.method === "SubscribeMeetingEvents").at(-1)!;
    // A plain change (another window connected or disconnected) is not a failure.
    act(() =>
      feed.handlers.onMessage({ calendarChanged: { connected: false, error: "" } } as never),
    );
    expect(screen.getByText("Finish signing in in your browser…")).toBeTruthy();
    act(() =>
      feed.handlers.onMessage({
        calendarChanged: { connected: false, error: "Check the client ID and secret." },
      } as never),
    );
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "Check the client ID and secret.",
    );
    // The form is back, with what was typed, so the person can fix the typo.
    expect((screen.getByLabelText("Client secret") as HTMLInputElement).value).toBe("typo");
    expect(screen.queryByText("Finish signing in in your browser…")).toBeNull();
  });

  it("lets the person give up on a sign-in they never finished", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    mount({ ConnectCalendar: { authUrl: "https://accounts.google.com/x", statusMessage: "" } });
    fireEvent.change(await screen.findByLabelText("Client ID"), { target: { value: "id" } });
    fireEvent.change(screen.getByLabelText("Client secret"), { target: { value: "s" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    await screen.findByText("Finish signing in in your browser…");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.getByRole("button", { name: "Connect Google Calendar" })).toBeTruthy();
  });

  it("asks for both halves of the Google client before calling the daemon", async () => {
    const daemon = mount();
    fireEvent.change(await screen.findByLabelText("Client ID"), { target: { value: "id" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    expect(await screen.findByText(/Paste both the client ID and the client secret/)).toBeTruthy();
    expect(connects(daemon)).toEqual([]);
  });

  it("refuses a sign-in address that isn't a web page, and says what the daemon said", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    mount({
      ConnectCalendar: {
        authUrl: "file:///etc/passwd",
        statusMessage: "Google refused the client.",
      },
    });
    const clientId = (await screen.findByLabelText("Client ID")) as HTMLInputElement;
    const clientSecret = screen.getByLabelText("Client secret") as HTMLInputElement;
    // The client ID is not a secret; the secret is masked until "Show".
    expect(clientId.type).toBe("text");
    expect(clientSecret.type).toBe("password");
    fireEvent.change(clientId, { target: { value: "id" } });
    fireEvent.change(clientSecret, { target: { value: "s" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect Google Calendar" }));
    expect(await screen.findByText("Google refused the client.")).toBeTruthy();
    expect(open).not.toHaveBeenCalled();
  });

  it("uses an ICS feed", async () => {
    const daemon = mount({ ConnectCalendar: { authUrl: "", statusMessage: "" } });
    const feed = (await screen.findByLabelText("Feed URL")) as HTMLInputElement;
    expect(feed.type).toBe("password");
    fireEvent.change(feed, { target: { value: "https://calendar.example.com/private/basic.ics" } });
    fireEvent.click(screen.getByRole("button", { name: "Use this feed" }));
    await waitFor(() =>
      expect(connects(daemon)).toEqual([
        {
          kind: "ics",
          icsUrl: "https://calendar.example.com/private/basic.ics",
          googleClientId: "",
          googleClientSecret: "",
        },
      ]),
    );
  });

  it("shows a secret while Show is pressed", async () => {
    mount();
    const feed = (await screen.findByLabelText("Feed URL")) as HTMLInputElement;
    const shows = screen.getAllByRole("button", { name: "Show" });
    fireEvent.click(shows.at(-1)!);
    expect(feed.type).toBe("text");
  });

  it("says why a feed was not accepted", async () => {
    mount({
      ConnectCalendar: new BorgError(
        GrpcStatus.Unavailable,
        "That address didn't return a calendar.",
      ),
    });
    fireEvent.change(await screen.findByLabelText("Feed URL"), { target: { value: "https://x" } });
    fireEvent.click(screen.getByRole("button", { name: "Use this feed" }));
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toBe("That address didn't return a calendar.");
  });

  it("disconnects", async () => {
    const daemon = mount({ GetMeetingsStatus: google, DisconnectCalendar: {} });
    fireEvent.click(await screen.findByRole("button", { name: "Disconnect" }));
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "DisconnectCalendar")).toBe(true),
    );
  });
});
