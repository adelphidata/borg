import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useParams } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp, type UnaryTable } from "../../test/harness";
import { fullMeeting } from "../../test/meetings";
import { MeetingDetailPage } from "./MeetingDetailPage";
import { meetingsChanged } from "./meetingsStore";
import { NOTES_DEBOUNCE_MS } from "./useNotesDraft";

function Conversation() {
  return <div>conversation {useParams().sessionId}</div>;
}

function mount(table: UnaryTable = {}) {
  const daemon = fakeDaemon({
    GetMeeting: fullMeeting({ id: "m1", title: "Weekly sync", startedAt: 1_700_000_000 }),
    MarkMeetingRead: {},
    UpdateNotes: {},
    ...table,
  });
  const view = renderApp(
    <Routes>
      <Route path="/meetings/:id" element={<MeetingDetailPage />} />
      <Route path="/meetings" element={<div>the list</div>} />
      <Route path="/c/:sessionId" element={<Conversation />} />
    </Routes>,
    { daemon, route: "/meetings/m1" },
  );
  return { ...view, daemon };
}

const callsTo = (daemon: ReturnType<typeof fakeDaemon>, method: string) =>
  daemon.calls.filter((c) => c.method === method).map((c) => c.request);

describe("MeetingDetailPage", () => {
  it("clears the unread mark once when a new summary is opened", async () => {
    const { daemon } = mount({
      GetMeeting: fullMeeting(
        { id: "m1", unread: true, hasSummary: true },
        { summaryMarkdown: "- x" },
      ),
    });
    await screen.findByText("Summary");
    await waitFor(() => expect(callsTo(daemon, "MarkMeetingRead")).toEqual([{ id: "m1" }]));
    // The mark bumps the shared counter and the meeting is read again; a
    // second mark for the same meeting would be a wasted write.
    await waitFor(() => expect(callsTo(daemon, "GetMeeting").length).toBeGreaterThan(1));
    expect(callsTo(daemon, "MarkMeetingRead")).toHaveLength(1);
  });

  it("leaves a meeting that is already read alone", async () => {
    const { daemon } = mount();
    await screen.findByPlaceholderText("Add your thoughts…");
    expect(callsTo(daemon, "MarkMeetingRead")).toHaveLength(0);
  });

  it("saves the notes once typing pauses, with the final text", async () => {
    const { daemon } = mount();
    const box = await screen.findByPlaceholderText("Add your thoughts…");
    fireEvent.change(box, { target: { value: "a" } });
    fireEvent.change(box, { target: { value: "ab" } });
    fireEvent.change(box, { target: { value: "ab c" } });
    expect(callsTo(daemon, "UpdateNotes")).toHaveLength(0);
    await waitFor(() => expect(callsTo(daemon, "UpdateNotes")).toHaveLength(1), {
      timeout: NOTES_DEBOUNCE_MS * 3,
    });
    expect(callsTo(daemon, "UpdateNotes")).toEqual([{ meetingId: "m1", notes: "ab c" }]);
  });

  it("saves at once when the box loses focus", async () => {
    const { daemon } = mount();
    const box = await screen.findByPlaceholderText("Add your thoughts…");
    fireEvent.change(box, { target: { value: "decided: ship" } });
    fireEvent.blur(box);
    await waitFor(() =>
      expect(callsTo(daemon, "UpdateNotes")).toEqual([{ meetingId: "m1", notes: "decided: ship" }]),
    );
  });

  it("says so when the notes did not save", async () => {
    mount({ UpdateNotes: new BorgError(GrpcStatus.Unavailable, "Borg is restarting") });
    const box = await screen.findByPlaceholderText("Add your thoughts…");
    fireEvent.change(box, { target: { value: "x" } });
    fireEvent.blur(box);
    expect(await screen.findByText(/notes weren't saved: Borg is restarting/)).toBeTruthy();
  });

  it("has no summary while the meeting records, and leads with it once Borg writes one", async () => {
    mount({ GetMeeting: fullMeeting({ state: "recording" }) });
    await screen.findByPlaceholderText("Add your thoughts…");
    expect(screen.queryByRole("tab", { name: "Summary" })).toBeNull();
  });

  it("keeps the notes box in place when the recording stops under someone typing", async () => {
    const { daemon } = mount({ GetMeeting: fullMeeting({ state: "recording" }) });
    const box = await screen.findByPlaceholderText("Add your thoughts…");
    fireEvent.change(box, { target: { value: "half a thought" } });
    daemon.answer("GetMeeting", fullMeeting({ state: "finalizing" }));
    act(() => meetingsChanged());
    expect(await screen.findByRole("tab", { name: "Summary" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "My notes" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect((screen.getByPlaceholderText("Add your thoughts…") as HTMLTextAreaElement).value).toBe(
      "half a thought",
    );
  });

  it("offers no summary tab for a finished meeting whose summary came back empty", async () => {
    mount({
      GetMeeting: fullMeeting({ state: "done", hasSummary: true }, { summaryMarkdown: "" }),
    });
    await screen.findByPlaceholderText("Add your thoughts…");
    expect(screen.queryByRole("tab", { name: "Summary" })).toBeNull();
    expect(screen.queryByText(/writing the summary/)).toBeNull();
  });

  it("keeps the name box focused when a rename is refused, so it can be fixed", async () => {
    mount({ UpdateNotes: new Error("Borg is restarting") });
    fireEvent.doubleClick(await screen.findByRole("heading", { name: "Weekly sync" }));
    const name = screen.getByRole("textbox", { name: "Meeting name" });
    fireEvent.change(name, { target: { value: "Pricing review" } });
    fireEvent.keyDown(name, { key: "Enter" });
    await waitFor(() => expect((name as HTMLInputElement).readOnly).toBe(false));
    expect(document.activeElement).toBe(name);
  });

  it("opens on the summary tab while Borg is writing it, with the notes a tab away", async () => {
    mount({
      GetMeeting: fullMeeting({ state: "finalizing", hasSummary: false }, { notes: "ship it" }),
    });
    expect(await screen.findByText("Borg is writing the summary…")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Summary" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.queryByPlaceholderText("Add your thoughts…")).toBeNull();
    fireEvent.click(screen.getByRole("tab", { name: "My notes" }));
    expect(
      ((await screen.findByPlaceholderText("Add your thoughts…")) as HTMLTextAreaElement).value,
    ).toBe("ship it");
  });

  it("renders the summary's bullets", async () => {
    mount({
      GetMeeting: fullMeeting(
        { hasSummary: true },
        { summaryMarkdown: "### Decisions\n- Ship Friday" },
      ),
    });
    expect(await screen.findByText("Ship Friday")).toBeTruthy();
    expect(screen.getByText("Decisions")).toBeTruthy();
  });

  it("says why the notes could not be written", async () => {
    mount({ GetMeeting: fullMeeting({ state: "failed" }, { error: "no model configured" }) });
    expect(await screen.findByText(/couldn't write the notes: no model configured/)).toBeTruthy();
  });

  it("keeps the transcript on its own tab, listing who said what and when", async () => {
    mount({
      GetMeeting: fullMeeting(
        {},
        {
          segments: [
            { track: "mic", startMs: 4_000, endMs: 6_000, text: "Can you hear me?" },
            { track: "system", startMs: 724_000, endMs: 730_000, text: "Loud and clear." },
          ],
        },
      ),
    });
    const tab = await screen.findByRole("tab", { name: "Transcript" });
    expect(screen.queryByText("Can you hear me?")).toBeNull();
    fireEvent.click(tab);
    const rows = screen.getAllByRole("listitem");
    expect(rows.map((r) => r.textContent)).toEqual([
      "0:04MeCan you hear me?",
      "12:04ThemLoud and clear.",
    ]);
  });

  it("offers Stop only while recording, and stops the meeting", async () => {
    const { daemon } = mount({
      GetMeeting: fullMeeting({
        state: "recording",
        startedAt: Math.floor(Date.now() / 1000) - 75,
      }),
      StopMeeting: fullMeeting({ state: "finalizing" }),
    });
    expect(await screen.findByText("Listening")).toBeTruthy();
    // A running clock, like the transcript's timestamps: no tenths.
    expect(screen.getByText(/^1:1[5-7]$/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    await waitFor(() => expect(callsTo(daemon, "StopMeeting")).toEqual([{ id: "m1" }]));
  });

  it("has no Stop on a finished meeting, and deletes from its menu", async () => {
    const { daemon } = mount({ DeleteMeeting: {} });
    await screen.findByPlaceholderText("Add your thoughts…");
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Meeting actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: /Delete/ }));
    expect(await screen.findByText("the list")).toBeTruthy();
    expect(callsTo(daemon, "DeleteMeeting")).toEqual([{ id: "m1" }]);
  });

  it("asks Borg about the meeting in a new conversation that names it", async () => {
    const { daemon } = mount({ Open: { sessionId: "s9", lastEventSeq: 0 } });
    const ask = await screen.findByRole("textbox", { name: "Ask about this meeting" });
    fireEvent.change(ask, { target: { value: "What did we decide?" } });
    fireEvent.keyDown(ask, { key: "Enter" });
    await waitFor(() => expect(daemon.streams.some((s) => s.method === "Send")).toBe(true));
    const send = daemon.streams.find((s) => s.method === "Send")!;
    const text = (send.request as { sessionId: string; text: string }).text;
    expect((send.request as { sessionId: string }).sessionId).toBe("s9");
    expect(text).toMatch(/^What did we decide\?/);
    expect(text).toContain("meeting id m1");
    act(() => send.handlers.onMessage({} as never));
    expect(await screen.findByText("conversation s9")).toBeTruthy();
  });

  it("asks a suggested question in one press", async () => {
    const { daemon } = mount({ Open: { sessionId: "s9", lastEventSeq: 0 } });
    fireEvent.click(await screen.findByRole("button", { name: "Write a follow-up email" }));
    await waitFor(() => expect(daemon.streams.some((s) => s.method === "Send")).toBe(true));
    const text = (daemon.streams.find((s) => s.method === "Send")!.request as { text: string })
      .text;
    expect(text).toMatch(/^Write a follow-up email/);
    expect(text).toContain("meeting id m1");
  });

  it("shows when the meeting was, how long it ran and who came", async () => {
    const start = new Date(2026, 10, 12, 16, 0).getTime() / 1000;
    mount({
      GetMeeting: fullMeeting({
        startedAt: start,
        endedAt: start + 32 * 60,
        attendees: [
          { name: "Juan Pérez", email: "" },
          { name: "", email: "jae@example.com" },
        ],
      }),
    });
    expect(await screen.findByText("32 min")).toBeTruthy();
    const people = screen.getByTitle("Juan Pérez, jae@example.com");
    expect(people.textContent).toBe("2");
  });

  it("stays put and says why when the question could not be sent", async () => {
    const { daemon } = mount({ Open: { sessionId: "s9", lastEventSeq: 0 } });
    const ask = await screen.findByRole("textbox", { name: "Ask about this meeting" });
    fireEvent.change(ask, { target: { value: "Who owns the follow-up?" } });
    fireEvent.click(screen.getByRole("button", { name: "Ask" }));
    await waitFor(() => expect(daemon.streams.some((s) => s.method === "Send")).toBe(true));
    act(() =>
      daemon.streams.find((s) => s.method === "Send")!.handlers.onError!(
        new BorgError(GrpcStatus.FailedPrecondition, "Budget spent") as never,
      ),
    );
    expect(await screen.findByText(/Couldn't ask Borg: Budget spent/)).toBeTruthy();
    expect(screen.queryByText(/conversation/)).toBeNull();
  });

  it("renames the meeting, keeping the notes typed so far", async () => {
    const { daemon } = mount();
    const box = await screen.findByPlaceholderText("Add your thoughts…");
    fireEvent.change(box, { target: { value: "unsaved thought" } });
    fireEvent.doubleClick(screen.getByRole("heading", { name: "Weekly sync" }));
    const name = screen.getByRole("textbox", { name: "Meeting name" });
    fireEvent.change(name, { target: { value: "Pricing review" } });
    fireEvent.keyDown(name, { key: "Enter" });
    await waitFor(() =>
      expect(callsTo(daemon, "UpdateNotes")).toEqual([
        { meetingId: "m1", notes: "unsaved thought", title: "Pricing review" },
      ]),
    );
  });

  it("says plainly when the meeting is gone", async () => {
    mount({ GetMeeting: { segments: [], notes: "", summaryMarkdown: "", error: "", dir: "" } });
    expect(await screen.findByText("That meeting doesn't exist.")).toBeTruthy();
    expect(within(document.body).queryByRole("button", { name: "Meeting actions" })).toBeNull();
  });
});
