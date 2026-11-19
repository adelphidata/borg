import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useParams } from "react-router-dom";
import { ThemeProvider } from "@borg/ui";
import { SettingsProvider } from "../../lib/settings";
import { fakeDaemon, renderApp, type UnaryTable } from "../../test/harness";
import { entry, event, meeting, status, voiceCatalog, voiceProvider } from "../../test/meetings";
import { MeetingsPage } from "./MeetingsPage";
import { MEETINGS_PAGE } from "./useMeetings";

/** Thu 12 Nov 2026, 3 PM local. */
const NOW = new Date(2026, 10, 12, 15, 0);
const at = (d: number, h: number, m = 0) => new Date(2026, 10, d, h, m).getTime() / 1000;

function Detail() {
  return <div>detail {useParams().id}</div>;
}

function EventDetail() {
  return <div>event {useParams().eventId}</div>;
}

function mount(table: UnaryTable = {}) {
  const daemon = fakeDaemon({
    GetSettings: { entries: [entry("meetings.enabled", "true")] },
    GetMeetingsStatus: status(),
    ListMeetings: { meetings: [] },
    ListUpcomingEvents: { events: [] },
    ...table,
  });
  const view = renderApp(
    <Routes>
      <Route path="/meetings" element={<MeetingsPage />} />
      <Route path="/meetings/event/:eventId" element={<EventDetail />} />
      <Route path="/meetings/:id" element={<Detail />} />
    </Routes>,
    {
      daemon,
      route: "/meetings",
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
  return { ...view, daemon };
}

const connected = status({
  calendar: { connected: true, kind: "google", accountLabel: "me@x.io" },
});

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(NOW);
});
afterEach(() => vi.useRealTimers());

describe("MeetingsPage", () => {
  it("offers to turn Meetings on, picking a transcriber a key is already saved for", async () => {
    const { daemon } = mount({
      GetSettings: { entries: [] },
      GetVoiceCatalog: voiceCatalog([voiceProvider("openai", "OpenAI", { hasKey: true })]),
      SetSetting: {},
    });
    const turnOn = (await screen.findByRole("button", { name: "Turn on" })) as HTMLButtonElement;
    await waitFor(() => expect(turnOn.disabled).toBe(false));
    fireEvent.click(turnOn);
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
        { key: "audio.models", value: JSON.stringify([{ provider: "openai" }]) },
        { key: "meetings.enabled", value: "true" },
      ]),
    );
  });

  it("says there is nothing more today once today's last meeting is over", async () => {
    mount({
      GetMeetingsStatus: connected,
      ListUpcomingEvents: { events: [event({ startUnix: at(12, 9), endUnix: at(12, 10) })] },
    });
    expect(await screen.findByText("No more events today")).toBeTruthy();
    expect(screen.getByText("Coming up")).toBeTruthy();
  });

  it("draws an upcoming event with its bar, title and time range", async () => {
    const { container } = mount({
      GetMeetingsStatus: connected,
      ListUpcomingEvents: {
        events: [
          event({ title: "Design review", startUnix: at(13, 9, 45), endUnix: at(13, 10, 15) }),
        ],
      },
    });
    const row = await screen.findByRole("button", { name: /Design review/ });
    expect(row.querySelector(".bg-neon")).not.toBeNull();
    expect(within(row).getByText(/9:45 – 10:15/)).toBeTruthy();
    expect(within(row).getByText("AM")).toBeTruthy();
    // Today is listed too, and only today carries the dot.
    expect(container.querySelectorAll("[aria-label='Today']")).toHaveLength(1);
  });

  it("opens an upcoming event's own page", async () => {
    mount({
      GetMeetingsStatus: connected,
      ListUpcomingEvents: {
        events: [
          event({
            id: "google:abc/1",
            title: "Design review",
            startUnix: at(13, 9, 45),
            endUnix: at(13, 10, 15),
          }),
        ],
      },
    });
    fireEvent.click(await screen.findByRole("button", { name: /Design review/ }));
    expect(await screen.findByText("event google:abc/1")).toBeTruthy();
  });

  it("pages the calendar a week at a time, never before this week", async () => {
    const { daemon } = mount({ GetMeetingsStatus: connected });
    const earlier = await screen.findByRole("button", { name: "Earlier" });
    expect((earlier as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Later" }));
    await waitFor(() => {
      const asks = daemon.calls.filter((c) => c.method === "ListUpcomingEvents");
      expect(asks.at(-1)!.request).toEqual({
        fromUnix: new Date(2026, 10, 19).getTime() / 1000,
        toUnix: new Date(2026, 10, 26).getTime() / 1000,
      });
    });
  });

  it("points to the calendar connection when none is connected", async () => {
    mount();
    const link = await screen.findByRole("link", { name: /Connect a calendar/ });
    expect(link.getAttribute("href")).toBe("/connections/calendar");
  });

  it("groups past meetings under Today and Yesterday, showing what each is doing", async () => {
    mount({
      ListMeetings: {
        meetings: [
          meeting({ id: "live", title: "Standup", startedAt: at(12, 14), state: "recording" }),
          meeting({ id: "wrap", title: "Retro", startedAt: at(12, 11), state: "finalizing" }),
          meeting({
            id: "y",
            title: "Hiring sync",
            startedAt: at(11, 16),
            unread: true,
            hasSummary: true,
            attendees: [
              { name: "Juan Pérez", email: "" },
              { name: "Jae Kim", email: "" },
            ],
          }),
          meeting({ id: "bad", title: "Board", startedAt: at(11, 9), state: "failed" }),
        ],
      },
    });
    const today = await screen.findByText("Today");
    const yesterday = screen.getByText("Yesterday");
    expect(
      today.compareDocumentPosition(yesterday) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(screen.getByText("Recording")).toBeTruthy();
    expect(screen.getByText("Writing notes…")).toBeTruthy();
    expect(screen.getByText("Didn't finish")).toBeTruthy();
    const hiring = screen.getByRole("button", { name: /Hiring sync/ });
    expect(within(hiring).getByText("Juan & Jae")).toBeTruthy();
    expect(within(hiring).getByText("4:00 PM")).toBeTruthy();
    expect(within(hiring).getByRole("img", { name: "New notes" })).toBeTruthy();
  });

  it("opens a past meeting", async () => {
    mount({
      ListMeetings: { meetings: [meeting({ id: "m 7", title: "Retro", startedAt: at(12, 9) })] },
    });
    fireEvent.click(await screen.findByRole("button", { name: /Retro/ }));
    expect(await screen.findByText("detail m 7")).toBeTruthy();
  });

  it("asks for the page before the last row when Show earlier is pressed", async () => {
    const page = Array.from({ length: MEETINGS_PAGE }, (_, i) =>
      meeting({ id: `m${i}`, title: `Meeting ${i}`, startedAt: at(12, 14) - i * 3600 }),
    );
    const { daemon } = mount({
      ListMeetings: (req: unknown) =>
        (req as { beforeUnix: number }).beforeUnix === 0
          ? { meetings: page }
          : { meetings: [meeting({ id: "old", title: "Kickoff", startedAt: at(1, 9) })] },
    });
    fireEvent.click(await screen.findByRole("button", { name: "Show earlier" }));
    expect(await screen.findByText("Kickoff")).toBeTruthy();
    const asked = daemon.calls.filter((c) => c.method === "ListMeetings").at(-1)!.request;
    expect(asked).toEqual({ limit: MEETINGS_PAGE, beforeUnix: page.at(-1)!.startedAt });
    // A short page means that was the last of them.
    expect(screen.queryByRole("button", { name: "Show earlier" })).toBeNull();
  });

  it("offers no Show earlier when the first page was not full", async () => {
    mount({ ListMeetings: { meetings: [meeting({ title: "Retro", startedAt: at(12, 9) })] } });
    await screen.findByText("Retro");
    expect(screen.queryByRole("button", { name: "Show earlier" })).toBeNull();
  });
});
