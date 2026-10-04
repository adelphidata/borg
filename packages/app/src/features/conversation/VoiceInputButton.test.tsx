import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp, type FakeDaemon } from "../../test/harness";
import { SettingsProvider, useSettings } from "../../lib/settings";
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

const setting = (key: string, value: string) => ({
  key,
  value,
  category: "db",
  label: "",
  kind: "",
  group: "",
});

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

const MIC = { name: "Voice input" };
const findMic = () => screen.findByRole("button", MIC);
const queryMic = () => screen.queryByRole("button", MIC);

/** Click the microphone and wait until it is recording. */
async function startRecording() {
  const mic = await findMic();
  await act(async () => fireEvent.click(mic));
  await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("true"));
  return mic;
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
    expect(queryMic()).toBeNull();
  });

  it("stays hidden in a browser that cannot record, even when set up", async () => {
    vi.stubGlobal("MediaRecorder", undefined);
    const { daemon } = mount(daemonWith(true));
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "GetVoiceCatalog")).toBe(true),
    );
    expect(queryMic()).toBeNull();
  });

  it("records, sends the audio to the daemon, and hands back trimmed words", async () => {
    const { daemon, onText, onError } = mount(daemonWith(true));
    const mic = await startRecording();
    expect(mic.getAttribute("title")).toBe("Stop recording");

    await act(async () => fireEvent.click(mic));
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
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("false"));
    expect(mic.getAttribute("title")).toBe("Speak instead of typing");
  });

  it("says it heard nothing instead of inserting an empty string", async () => {
    const { onText, onError } = mount(daemonWith(true, { text: "   " }));
    const mic = await startRecording();
    await act(async () => fireEvent.click(mic));
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
    const mic = await startRecording();
    await act(async () => fireEvent.click(mic));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith("Could not transcribe the recording: 429"),
    );
  });

  it("explains a refused microphone without starting a recording", async () => {
    getUserMedia.mockRejectedValue(new DOMException("denied", "NotAllowedError"));
    const { onError } = mount(daemonWith(true));
    const mic = await findMic();
    await act(async () => fireEvent.click(mic));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith(expect.stringMatching(/can't use the microphone/)),
    );
    expect(FakeRecorder.last).toBeUndefined();
  });

  it("turns the microphone off and sends nothing when the composer goes away mid-recording", async () => {
    const { daemon, unmount } = mount(daemonWith(true));
    await startRecording();
    unmount();
    expect(FakeRecorder.last?.state).toBe("inactive");
    expect(track.stop).toHaveBeenCalled();
    await act(async () => {});
    expect(daemon.calls.some((c) => c.method === "Transcribe")).toBe(false);
  });

  it("opens one stream however often it is clicked during the permission prompt", async () => {
    let grant: (s: unknown) => void = () => {};
    getUserMedia.mockImplementation(() => new Promise((resolve) => (grant = resolve)));
    mount(daemonWith(true));
    const mic = await findMic();
    fireEvent.click(mic);
    fireEvent.click(mic);
    await act(async () => grant({ getTracks: () => [track] }));
    expect(getUserMedia).toHaveBeenCalledTimes(1);
    expect(mic.getAttribute("aria-pressed")).toBe("true");
  });

  it("releases a stream granted after the composer went away", async () => {
    let grant: (s: unknown) => void = () => {};
    getUserMedia.mockImplementation(() => new Promise((resolve) => (grant = resolve)));
    const { unmount } = mount(daemonWith(true));
    fireEvent.click(await findMic());
    unmount();
    await act(async () => grant({ getTracks: () => [track] }));
    expect(track.stop).toHaveBeenCalled();
    expect(FakeRecorder.last).toBeUndefined();
  });

  it("stops recording when voice input is turned off elsewhere", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [] },
      GetVoiceCatalog: catalog(true),
      SetSetting: {},
      Transcribe: { text: "unused" },
    });
    let save: (key: string, value: string) => Promise<unknown> = async () => undefined;
    function SaveProbe() {
      save = useSettings().save;
      return null;
    }
    renderApp(
      <>
        <VoiceInputButton onText={vi.fn()} onError={vi.fn()} />
        <SaveProbe />
      </>,
      { daemon, wrapper: (children) => <SettingsProvider>{children}</SettingsProvider> },
    );
    await startRecording();
    // Another tab turns voice input off; the settings refresh changes what the catalog reports.
    daemon.answer("GetSettings", { entries: [setting("audio.enabled", "false")] });
    daemon.answer("GetVoiceCatalog", catalog(false));
    await act(async () => void (await save("audio.enabled", "false")));
    await waitFor(() => expect(queryMic()).toBeNull());
    expect(FakeRecorder.last?.state).toBe("inactive");
    expect(track.stop).toHaveBeenCalled();
    expect(daemon.calls.some((c) => c.method === "Transcribe")).toBe(false);
  });
});
