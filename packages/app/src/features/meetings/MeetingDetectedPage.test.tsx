import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, fakeNative, renderApp, type UnaryTable } from "../../test/harness";
import { fullMeeting, status } from "../../test/meetings";
import { MeetingDetectedPage } from "./MeetingDetectedPage";

function mount(table: UnaryTable = {}) {
  const native = fakeNative();
  const daemon = fakeDaemon({
    GetMeetingsStatus: status(),
    StartMeeting: fullMeeting({ id: "m42", state: "recording" }),
    ...table,
  });
  renderApp(<MeetingDetectedPage />, { daemon, native });
  return { daemon, native };
}

describe("MeetingDetectedPage", () => {
  it("shows nothing until the shell names a call", () => {
    mount();
    expect(screen.queryByText("Meeting detected")).toBeNull();
  });

  it("names the app the call is in, and the calendar event when there is one", async () => {
    const { native } = mount();
    act(() =>
      native.emitDetection({
        sourceApp: "Chrome",
        calendarEventId: "evt1",
        calendarTitle: "Design review",
      }),
    );
    expect(screen.getByText("Meeting detected")).toBeTruthy();
    expect(screen.getByText("Design review · Chrome")).toBeTruthy();
  });

  it("starts the meeting from Take notes and hands it to the notes panel", async () => {
    const { daemon, native } = mount();
    act(() => native.emitDetection({ sourceApp: "Zoom", calendarEventId: "evt1" }));
    fireEvent.click(screen.getByRole("button", { name: "Take notes" }));
    await waitFor(() => expect(native.openedPanels).toEqual(["m42"]));
    expect(daemon.calls.find((c) => c.method === "StartMeeting")!.request).toEqual({
      title: "",
      sourceApp: "Zoom",
      calendarEventId: "evt1",
    });
  });

  it("says why when the meeting could not start, and opens no panel", async () => {
    const { native } = mount({
      StartMeeting: new BorgError(GrpcStatus.FailedPrecondition, "Another meeting is recording."),
    });
    act(() => native.emitDetection({ sourceApp: "Teams" }));
    fireEvent.click(screen.getByRole("button", { name: "Take notes" }));
    expect(await screen.findByText("Another meeting is recording.")).toBeTruthy();
    expect(native.openedPanels).toEqual([]);
  });

  it("puts itself away from its close button, and from Escape", () => {
    const { native } = mount();
    act(() => native.emitDetection({ sourceApp: "Zoom" }));
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(native.hidden).toBe(1);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(native.hidden).toBe(2);
  });

  it("sends the user to set up transcription instead of starting a meeting it can't transcribe", async () => {
    const { daemon, native } = mount({ GetMeetingsStatus: status({ transcriptionReady: false }) });
    act(() => native.emitDetection({ sourceApp: "Zoom" }));
    fireEvent.click(await screen.findByRole("button", { name: "Set up transcription" }));
    expect(native.revealedPaths).toEqual(["/settings/meetings"]);
    expect(native.hidden).toBe(1);
    expect(daemon.calls.some((c) => c.method === "StartMeeting")).toBe(false);
  });

  it("asks again whether transcription is ready for each new call", async () => {
    const { daemon, native } = mount({ GetMeetingsStatus: status({ transcriptionReady: false }) });
    act(() => native.emitDetection({ sourceApp: "Zoom" }));
    await screen.findByRole("button", { name: "Set up transcription" });
    daemon.answer("GetMeetingsStatus", status({ transcriptionReady: true }));
    act(() => native.emitDetection(null));
    act(() => native.emitDetection({ sourceApp: "Teams" }));
    expect(await screen.findByRole("button", { name: "Take notes" })).toBeTruthy();
  });

  it("goes blank again when the call ends", () => {
    const { native } = mount();
    act(() => native.emitDetection({ sourceApp: "Zoom" }));
    act(() => native.emitDetection(null));
    expect(screen.queryByText("Meeting detected")).toBeNull();
  });
});
