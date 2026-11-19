import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { fakeDaemon, fakeNative, renderApp, type UnaryTable } from "../../test/harness";
import { installFakeAudio, tone, type FakeAudio } from "../../test/audio";
import { fullMeeting } from "../../test/meetings";
import { MEETING_PANEL_HASH } from "../../lib/chords";
import { AUTO_STOP_MS, MeetingPanelPage, panelMeetingId } from "./MeetingPanelPage";

let audio: FakeAudio;

const recording = fullMeeting({
  id: "m1",
  title: "Weekly sync",
  state: "recording",
  startedAt: Math.floor(Date.now() / 1000) + 60,
});

function mount(table: UnaryTable = {}) {
  const native = fakeNative();
  const daemon = fakeDaemon({
    GetMeeting: recording,
    PushAudio: { appended: [] },
    UpdateNotes: {},
    ...table,
  });
  const view = renderApp(<MeetingPanelPage meetingId="m1" />, { daemon, native });
  const feed = () => daemon.streams.filter((s) => s.method === "SubscribeMeetingEvents").at(-1)!;
  const emit = (event: unknown) => act(() => feed().handlers.onMessage(event as never));
  return { ...view, daemon, native, emit };
}

const stops = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.calls.filter((c) => c.method === "StopMeeting").length;

beforeEach(() => {
  audio = installFakeAudio();
});
afterEach(() => {
  audio.restore();
  vi.useRealTimers();
});

describe("panelMeetingId", () => {
  it("reads the meeting id the shell put in the hash", () => {
    expect(panelMeetingId(`${MEETING_PANEL_HASH}${encodeURIComponent("m/1 2")}`)).toBe("m/1 2");
    expect(panelMeetingId("#/quick")).toBeUndefined();
    expect(panelMeetingId(`${MEETING_PANEL_HASH}%E0%A4%A`)).toBeUndefined();
  });
});

describe("MeetingPanelPage", () => {
  it("listens while the meeting records, with a Stop beside the title", async () => {
    mount();
    expect(await screen.findByText("Listening")).toBeTruthy();
    expect(screen.getByText("Weekly sync")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
    await waitFor(() => expect(audio.getUserMedia).toHaveBeenCalledTimes(1));
  });

  it("stops: last audio first, then the meeting, then writes notes", async () => {
    const { daemon } = mount({ StopMeeting: fullMeeting({ state: "finalizing" }) });
    await waitFor(() => expect(audio.contexts.length).toBe(2));
    await waitFor(() => expect(audio.contexts[0]!.processor).toBeTruthy());
    act(() => audio.contexts[0]!.feed(tone(2)));
    daemon.answer("GetMeeting", fullMeeting({ id: "m1", state: "finalizing" }));
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(await screen.findByText("Writing notes…")).toBeTruthy();
    const order = daemon.calls.map((c) => c.method).filter((m) => m !== "GetMeeting");
    expect(order).toEqual(["PushAudio", "StopMeeting"]);
    expect(audio.mic.tracks[0]!.stopped).toBe(true);
  });

  it("shows the summary when it is ready, and opens the meeting in the main window", async () => {
    const { daemon, native, emit } = mount({
      GetMeeting: fullMeeting({ id: "m1", state: "finalizing" }),
    });
    expect(await screen.findByText("Writing notes…")).toBeTruthy();
    daemon.answer(
      "GetMeeting",
      fullMeeting({ id: "m1", state: "done", hasSummary: true }, { summaryMarkdown: "- Ship it" }),
    );
    emit({ summaryReady: { id: "m1" } });
    expect(await screen.findByText("Ship it")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open in Borg" }));
    expect(native.revealedPaths).toEqual(["/meetings/m1"]);
    expect(native.hidden).toBe(1);
  });

  it("ignores events about other meetings", async () => {
    const { daemon, emit } = mount({ GetMeeting: fullMeeting({ id: "m1", state: "finalizing" }) });
    await screen.findByText("Writing notes…");
    const reads = daemon.calls.filter((c) => c.method === "GetMeeting").length;
    emit({ summaryReady: { id: "other" } });
    emit({ summaryFailed: { id: "other", message: "nope" } });
    expect(daemon.calls.filter((c) => c.method === "GetMeeting").length).toBe(reads);
    expect(screen.queryByText("nope")).toBeNull();
  });

  it("says the notes failed, and why, when the summary could not be written", async () => {
    const { emit } = mount({ GetMeeting: fullMeeting({ id: "m1", state: "finalizing" }) });
    await screen.findByText("Writing notes…");
    emit({ summaryFailed: { id: "m1", message: "The model is out of credit." } });
    expect(await screen.findByText("Borg couldn't write the notes")).toBeTruthy();
    expect(screen.getByText("The model is out of credit.")).toBeTruthy();
    expect(screen.queryByText("Writing notes…")).toBeNull();
    expect(screen.getByRole("button", { name: "Open in Borg" })).toBeTruthy();
  });

  it("resumes a finished meeting: listening again, the microphone back on", async () => {
    const { daemon } = mount({
      GetMeeting: fullMeeting({ id: "m1", state: "done", hasSummary: true }),
      ResumeMeeting: fullMeeting({ id: "m1", state: "recording" }),
    });
    const resume = await screen.findByRole("button", { name: "Resume" });
    expect(audio.getUserMedia).not.toHaveBeenCalled();
    daemon.answer("GetMeeting", recording);
    fireEvent.click(resume);
    expect(await screen.findByText("Listening")).toBeTruthy();
    expect(daemon.calls.filter((c) => c.method === "ResumeMeeting").map((c) => c.request)).toEqual([
      { id: "m1" },
    ]);
    await waitFor(() => expect(audio.getUserMedia).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("offers Resume after a failed summary, but not while the notes are being written", async () => {
    const { emit } = mount({ GetMeeting: fullMeeting({ id: "m1", state: "finalizing" }) });
    await screen.findByText("Writing notes…");
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
    emit({ summaryFailed: { id: "m1", message: "The model is out of credit." } });
    expect(await screen.findByRole("button", { name: "Resume" })).toBeTruthy();
  });

  it("stops by itself 30 s after the call ends, unless the call comes back", async () => {
    const { daemon, emit } = mount({ StopMeeting: fullMeeting({ state: "finalizing" }) });
    await screen.findByText("Listening");
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    emit({ detectionEnded: {} });
    act(() => void vi.advanceTimersByTime(AUTO_STOP_MS - 1_000));
    emit({ detected: { sourceApp: "Zoom" } });
    act(() => void vi.advanceTimersByTime(AUTO_STOP_MS * 2));
    expect(stops(daemon)).toBe(0);

    emit({ detectionEnded: {} });
    act(() => void vi.advanceTimersByTime(AUTO_STOP_MS));
    vi.useRealTimers();
    await waitFor(() => expect(stops(daemon)).toBe(1));
  });

  it("lets go of the devices when the meeting is stopped from elsewhere", async () => {
    const { daemon, emit } = mount();
    await waitFor(() => expect(audio.contexts.filter((c) => c.processor).length).toBe(2));
    daemon.answer("GetMeeting", fullMeeting({ id: "m1", state: "finalizing" }));
    emit({ stopped: { id: "m1" } });
    expect(await screen.findByText("Writing notes…")).toBeTruthy();
    expect(audio.mic.tracks[0]!.stopped).toBe(true);
    expect(stops(daemon)).toBe(0);
  });

  it("says when only the microphone can be recorded", async () => {
    audio.restore();
    audio = installFakeAudio({ displayFails: 2 });
    mount();
    expect(await screen.findByText(/microphone only/)).toBeTruthy();
  });
});
