import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp, type FakeDaemon } from "../../test/harness";
import { SettingsProvider } from "../../lib/settings";
import { VoiceInputButton } from "./VoiceInputButton";

/**
 * jsdom has no microphone. A recorder that hands back one chunk on stop is
 * the whole of what the button relies on, so that is all this fakes.
 */
class FakeRecorder {
  static last: FakeRecorder | undefined;
  state: "inactive" | "recording" = "inactive";
  mimeType = "audio/webm";
  ondataavailable: ((e: { data: Blob }) => void) | null = null;
  onstop: (() => void) | null = null;
  constructor(readonly stream: MediaStream) {
    FakeRecorder.last = this;
  }
  start() {
    this.state = "recording";
  }
  stop() {
    this.state = "inactive";
    this.ondataavailable?.({ data: new Blob([new Uint8Array([1, 2, 3])], { type: "audio/webm" }) });
    this.onstop?.();
  }
}

const track = { stop: vi.fn() };
const getUserMedia = vi.fn();

beforeEach(() => {
  FakeRecorder.last = undefined;
  track.stop.mockReset();
  getUserMedia.mockReset();
  getUserMedia.mockResolvedValue({ getTracks: () => [track] });
  vi.stubGlobal("MediaRecorder", FakeRecorder);
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: { getUserMedia },
  });
});

afterEach(() => vi.unstubAllGlobals());

const catalog = (transcriptionReady: boolean) => ({
  speech: [],
  transcription: [],
  phone: [],
  speechReady: false,
  transcriptionReady,
  phoneProblem: "",
});

function mount(daemon: FakeDaemon) {
  const onText = vi.fn();
  const onError = vi.fn();
  const view = renderApp(<VoiceInputButton onText={onText} onError={onError} />, {
    daemon,
    wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
  });
  return { ...view, onText, onError };
}

function daemonWith(ready: boolean, transcribe: unknown = { text: "  call mom  " }) {
  return fakeDaemon({
    GetSettings: { entries: [] },
    GetVoiceCatalog: catalog(ready),
    Transcribe: transcribe,
  });
}

describe("VoiceInputButton", () => {
  it("stays hidden while transcription is not set up", async () => {
    const { daemon } = mount(daemonWith(false));
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "GetVoiceCatalog")).toBe(true),
    );
    expect(screen.queryByLabelText("Speak instead of typing")).toBeNull();
  });

  it("stays hidden in a browser that cannot record, even when set up", async () => {
    vi.stubGlobal("MediaRecorder", undefined);
    const { daemon } = mount(daemonWith(true));
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "GetVoiceCatalog")).toBe(true),
    );
    expect(screen.queryByLabelText("Speak instead of typing")).toBeNull();
  });

  it("records, sends the audio to the daemon, and hands back trimmed words", async () => {
    const { daemon, onText, onError } = mount(daemonWith(true));
    fireEvent.click(await screen.findByLabelText("Speak instead of typing"));
    const stop = await screen.findByLabelText("Stop recording");
    expect(stop.getAttribute("aria-pressed")).toBe("true");

    await act(async () => fireEvent.click(stop));
    await waitFor(() => expect(onText).toHaveBeenCalledWith("call mom"));

    const request = daemon.calls.find((c) => c.method === "Transcribe")?.request as {
      audio: Uint8Array;
      mimeType: string;
    };
    expect(Array.from(request.audio)).toEqual([1, 2, 3]);
    expect(request.mimeType).toBe("audio/webm");
    // The microphone is released once the recording ends.
    expect(track.stop).toHaveBeenCalled();
    expect(onError).toHaveBeenLastCalledWith(undefined);
    expect(await screen.findByLabelText("Speak instead of typing")).toBeTruthy();
  });

  it("says it heard nothing instead of inserting an empty string", async () => {
    const { onText, onError } = mount(daemonWith(true, { text: "   " }));
    fireEvent.click(await screen.findByLabelText("Speak instead of typing"));
    const target = await screen.findByLabelText("Stop recording");
    await act(async () => fireEvent.click(target));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith(expect.stringMatching(/Didn't catch that/)),
    );
    expect(onText).not.toHaveBeenCalled();
  });

  it("passes the daemon's plain-words failure through", async () => {
    const failure = new BorgError(
      GrpcStatus.Unavailable,
      "Could not transcribe the recording: 429",
    );
    const { onError } = mount(daemonWith(true, failure));
    fireEvent.click(await screen.findByLabelText("Speak instead of typing"));
    const target = await screen.findByLabelText("Stop recording");
    await act(async () => fireEvent.click(target));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith("Could not transcribe the recording: 429"),
    );
  });

  it("explains a refused microphone without starting a recording", async () => {
    getUserMedia.mockRejectedValue(new DOMException("denied", "NotAllowedError"));
    const { onError } = mount(daemonWith(true));
    const target = await screen.findByLabelText("Speak instead of typing");
    await act(async () => fireEvent.click(target));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith(expect.stringMatching(/can't use the microphone/)),
    );
    expect(FakeRecorder.last).toBeUndefined();
  });

  it("turns the microphone off when the composer goes away mid-recording", async () => {
    const { unmount } = mount(daemonWith(true));
    fireEvent.click(await screen.findByLabelText("Speak instead of typing"));
    await screen.findByLabelText("Stop recording");
    unmount();
    expect(FakeRecorder.last?.state).toBe("inactive");
    expect(track.stop).toHaveBeenCalled();
  });
});
