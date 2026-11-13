import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useParams } from "react-router-dom";
import { fakeDaemon, fakeNative, renderApp, type UnaryTable } from "../../test/harness";
import { event, fullMeeting, meeting, status } from "../../test/meetings";
import { MeetingEventPage, eventPath } from "./MeetingEventPage";

/** Fri 13 Nov 2026, 9 AM local. */
const NOW = new Date(2026, 10, 13, 9, 0);
const at = (h: number, m = 0) => new Date(2026, 10, 13, h, m).getTime() / 1000;

const STANDUP = event({
  id: "google:standup/20261113",
  title: "Daily stand-up",
  startUnix: at(9, 45),
  endUnix: at(10, 15),
  attendees: [
    { name: "Juan Pérez", email: "juan@example.com" },
    { name: "", email: "jae@example.com" },
  ],
  joinUrl: "https://meet.google.com/abc-defg-hij",
  provider: "meet",
});

function Detail() {
  return <div>meeting {useParams().id}</div>;
}

function mount(table: UnaryTable = {}, options: { kind?: "desktop" | "web"; id?: string } = {}) {
  const daemon = fakeDaemon({
    GetMeetingsStatus: status(),
    ListUpcomingEvents: { events: [STANDUP] },
    ListMeetings: { meetings: [] },
    StartMeeting: fullMeeting({ id: "m9", state: "recording" }),
    ...table,
  });
  const native = fakeNative();
  const view = renderApp(
    <Routes>
      <Route path="/meetings/event/:eventId" element={<MeetingEventPage />} />
      <Route path="/meetings/:id" element={<Detail />} />
    </Routes>,
    { daemon, native, kind: options.kind, route: eventPath(options.id ?? STANDUP.id) },
  );
  return { ...view, daemon, native };
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(NOW);
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("MeetingEventPage", () => {
  it("shows when the event is, who is coming, and joins the call", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    mount();
    expect(await screen.findByRole("heading", { name: "Daily stand-up" })).toBeTruthy();
    expect(screen.getByText("Today · 9:45 – 10:15 AM")).toBeTruthy();
    expect(screen.getByTitle("Juan Pérez, jae@example.com").textContent).toBe("2");
    expect(screen.getByText("juan@example.com")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Join Google Meet" }));
    expect(open).toHaveBeenCalledWith(
      "https://meet.google.com/abc-defg-hij",
      "_blank",
      "noopener,noreferrer",
    );
  });

  it("takes notes on the desktop: starts the meeting for this event and opens its panel", async () => {
    const { daemon, native } = mount();
    fireEvent.click(await screen.findByRole("button", { name: "Take notes" }));
    expect(await screen.findByText("meeting m9")).toBeTruthy();
    expect(daemon.calls.filter((c) => c.method === "StartMeeting").map((c) => c.request)).toEqual([
      { title: "", sourceApp: "", calendarEventId: STANDUP.id },
    ]);
    expect(native.openedPanels).toEqual(["m9"]);
  });

  it("stays and says why when the meeting could not start", async () => {
    mount({ StartMeeting: new Error("Borg is already recording a meeting") });
    fireEvent.click(await screen.findByRole("button", { name: "Take notes" }));
    expect(await screen.findByText(/already recording a meeting/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Take notes" })).toBeTruthy();
  });

  it("offers no Take notes in a browser, where nothing can record", async () => {
    const { daemon } = mount({}, { kind: "web" });
    await screen.findByRole("heading", { name: "Daily stand-up" });
    expect(screen.queryByRole("button", { name: "Take notes" })).toBeNull();
    expect(daemon.calls.some((c) => c.method === "StartMeeting")).toBe(false);
  });

  it("sends the user to set up transcription before it offers to record", async () => {
    mount({ GetMeetingsStatus: status({ transcriptionReady: false }) });
    expect(await screen.findByRole("link", { name: /Set one up/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Take notes" })).toBeNull();
  });

  it("opens the notes Borg already took in this event instead of starting again", async () => {
    mount({ ListMeetings: { meetings: [meeting({ id: "m3", calendarEventId: STANDUP.id })] } });
    fireEvent.click(await screen.findByRole("button", { name: "Open notes" }));
    expect(await screen.findByText("meeting m3")).toBeTruthy();
  });

  it("opens an event two weeks out, as far as Coming up can page", async () => {
    const { daemon } = mount();
    await screen.findByRole("heading", { name: "Daily stand-up" });
    const [request] = daemon.calls
      .filter((c) => c.method === "ListUpcomingEvents")
      .map((c) => c.request as { toUnix: number });
    const inTwoWeeks = new Date(2026, 10, 27, 17, 0).getTime() / 1000;
    expect(request!.toUnix).toBeGreaterThan(inTwoWeeks);
  });

  it("offers no Take notes while Meetings is off", async () => {
    mount({ GetMeetingsStatus: status({ enabled: false }) });
    await screen.findByRole("heading", { name: "Daily stand-up" });
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Join Google Meet" })).toBeTruthy(),
    );
    expect(screen.queryByRole("button", { name: "Take notes" })).toBeNull();
  });

  it("offers no Take notes until it knows whether notes already exist", async () => {
    mount({ ListMeetings: new Error("Borg is restarting") });
    await screen.findByRole("heading", { name: "Daily stand-up" });
    await waitFor(() => expect(screen.getByText("juan@example.com")).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Take notes" })).toBeNull();
  });

  it("can try again when the daemon starts nothing", async () => {
    const { native } = mount({ StartMeeting: { segments: [], notes: "", summaryMarkdown: "" } });
    fireEvent.click(await screen.findByRole("button", { name: "Take notes" }));
    await waitFor(() =>
      expect(
        (screen.getByRole("button", { name: "Take notes" }) as HTMLButtonElement).disabled,
      ).toBe(false),
    );
    expect(native.openedPanels).toEqual([]);
  });

  it("lists only the people the calendar named", async () => {
    mount({
      ListUpcomingEvents: {
        events: [
          {
            ...STANDUP,
            attendees: [...STANDUP.attendees, { name: " ", email: "" }],
          },
        ],
      },
    });
    await screen.findByText("juan@example.com");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
  });

  it("says so when the event is no longer on the calendar", async () => {
    mount({}, { id: "google:gone" });
    await waitFor(() =>
      expect(screen.getByText("That event isn't on your calendar any more.")).toBeTruthy(),
    );
  });
});
