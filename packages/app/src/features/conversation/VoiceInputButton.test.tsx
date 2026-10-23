import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, fakeNative, renderApp, type FakeDaemon } from "../../test/harness";
import { installKeybinds } from "../../lib/keybinds";
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

let uninstallKeybinds = () => {};
beforeEach(() => {
  // Escape reaches the recording through the app-wide keybinds.
  uninstallKeybinds = installKeybinds(window, false);
});
afterEach(() => {
  uninstallKeybinds();
  vi.unstubAllGlobals();
});

const setting = (key: string, value: string) => ({
  key,
  value,
  category: "db",
  label: "",
  kind: "",
  group: "",
});

const provider = (id: string, label: string, hasKey: boolean) => ({
  id,
  label,
  needsKey: true,
  keyUrl: "",
  defaultModel: "",
  defaultVoice: "",
  requiresCommand: false,
  hasKey,
  keyLabel: `${label} API key`,
});

const catalog = (transcriptionReady: boolean, transcription: unknown[] = []) => ({
  speech: [],
  transcription,
  phone: [],
  speechReady: false,
  transcriptionReady,
  phoneProblem: "",
});

function mount(daemon: FakeDaemon, kind: "desktop" | "web" = "desktop") {
  const onText = vi.fn();
  const onError = vi.fn();
  const view = renderApp(<VoiceInputButton onText={onText} onError={onError} />, {
    daemon,
    kind,
    wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
  });
  return { ...view, onText, onError };
}

/** The shortcut's keydown or keyup, as the window sees it (Ctrl stands in for mod off a Mac). */
const chord = (type: "keydown" | "keyup", extra: KeyboardEventInit = {}) =>
  fireEvent(
    window,
    new KeyboardEvent(type, {
      key: " ",
      code: "Space",
      ctrlKey: true,
      shiftKey: true,
      bubbles: true,
      cancelable: true,
      ...extra,
    }),
  );

const MIC = { name: "Voice input" };
const findMic = () => screen.findByRole("button", MIC);
const queryMic = () => screen.queryByRole("button", MIC);
/** The microphone once the catalog has said transcription is set up. */
async function findReadyMic() {
  const mic = await findMic();
  await waitFor(() => expect(mic.getAttribute("title")).toMatch(/^Speak instead of typing/));
  return mic;
}

/** Click the microphone and wait until it is recording. */
async function startRecording() {
  const mic = await findReadyMic();
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
  it("offers one-press setup when a saved key can transcribe", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("audio.enabled", "false")] },
      GetVoiceCatalog: catalog(false, [
        provider("elevenlabs", "ElevenLabs", false),
        provider("groq", "Groq", true),
      ]),
      SetSetting: {},
    });
    mount(daemon);
    const mic = await findMic();
    expect(mic.getAttribute("title")).toBe("Set up voice input");
    expect(getUserMedia).not.toHaveBeenCalled();
    fireEvent.click(mic);
    const card = await screen.findByRole("dialog", { name: "Set up voice input" });
    expect(card.textContent).toMatch(/Your Groq key can turn speech into text/);
    fireEvent.click(screen.getByRole("button", { name: "Use Groq" }));
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
        { key: "audio.models", value: JSON.stringify([{ provider: "groq" }]) },
        { key: "audio.enabled", value: "true" },
      ]),
    );
  });

  it("points to Settings when no saved key can transcribe", async () => {
    mount(
      fakeDaemon({
        GetSettings: { entries: [] },
        GetVoiceCatalog: catalog(false, [provider("openai", "OpenAI", false)]),
      }),
    );
    fireEvent.click(await findMic());
    const card = await screen.findByRole("dialog", { name: "Set up voice input" });
    expect(card.textContent).toMatch(/Pick one in Settings/);
    expect(screen.getByRole("button", { name: "Open Settings" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Not now" }));
    expect(screen.queryByRole("dialog")).toBeNull();
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
    expect(mic.getAttribute("title")).toMatch(/^Done/);
    expect(screen.getByRole("status", { name: "Recording" })).toBeTruthy();

    await act(async () => fireEvent.click(mic));
    await waitFor(() => expect(onText).toHaveBeenCalledWith("call mom", { send: false }));

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
    expect(mic.getAttribute("title")).toBe("Speak instead of typing (Ctrl+Shift+Space)");
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("the cancel button and Escape both drop the recording unsent", async () => {
    const { daemon, onText } = mount(daemonWith(true));
    await startRecording();
    fireEvent.click(screen.getByRole("button", { name: "Cancel recording" }));
    expect(FakeRecorder.last?.state).toBe("inactive");
    expect(track.stop).toHaveBeenCalled();
    track.stop.mockReset();

    const mic = await startRecording();
    fireEvent.keyDown(window, { key: "Escape", code: "Escape" });
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("false"));
    expect(track.stop).toHaveBeenCalled();
    await act(async () => {});
    expect(daemon.calls.some((c) => c.method === "Transcribe")).toBe(false);
    expect(onText).not.toHaveBeenCalled();
  });

  it("the shortcut starts a recording; a long hold stops it on release", async () => {
    const now = vi.spyOn(Date, "now");
    now.mockReturnValue(1_000);
    const { onText } = mount(daemonWith(true));
    const mic = await findReadyMic();
    await act(async () => void chord("keydown"));
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("true"));
    // The key repeats while held; that must not stop what it started.
    await act(async () => void chord("keydown", { repeat: true }));
    expect(mic.getAttribute("aria-pressed")).toBe("true");
    now.mockReturnValue(1_000 + 600);
    await act(async () => void chord("keyup"));
    await waitFor(() => expect(onText).toHaveBeenCalledWith("call mom", { send: false }));
    now.mockRestore();
  });

  it("a quick tap of the shortcut keeps recording until the next tap", async () => {
    const now = vi.spyOn(Date, "now");
    now.mockReturnValue(1_000);
    const { onText } = mount(daemonWith(true));
    const mic = await findReadyMic();
    await act(async () => void chord("keydown"));
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("true"));
    now.mockReturnValue(1_100);
    await act(async () => void chord("keyup"));
    await act(async () => {});
    expect(mic.getAttribute("aria-pressed")).toBe("true");
    await act(async () => void chord("keydown"));
    await waitFor(() => expect(onText).toHaveBeenCalled());
    now.mockRestore();
  });

  it("a release does not stop the recording when hold-to-talk is off", async () => {
    const now = vi.spyOn(Date, "now");
    now.mockReturnValue(1_000);
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("voice.hold_to_record", "false")] },
      GetVoiceCatalog: catalog(true),
      Transcribe: { text: "x" },
    });
    mount(daemon);
    const mic = await findReadyMic();
    await act(async () => void chord("keydown"));
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("true"));
    now.mockReturnValue(5_000);
    await act(async () => void chord("keyup"));
    await act(async () => {});
    expect(mic.getAttribute("aria-pressed")).toBe("true");
    now.mockRestore();
  });

  it("follows the shortcut in Settings, and a lone modifier works as hold-to-talk", async () => {
    const now = vi.spyOn(Date, "now");
    now.mockReturnValue(1_000);
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("voice.dictation_shortcut", "altright")] },
      GetVoiceCatalog: catalog(true),
      Transcribe: { text: "x" },
    });
    const { onText } = mount(daemon);
    const mic = await findReadyMic();
    expect(mic.getAttribute("title")).toBe("Speak instead of typing (Right Alt)");
    // The old chord no longer does anything.
    await act(async () => void chord("keydown"));
    await act(async () => {});
    expect(mic.getAttribute("aria-pressed")).toBe("false");
    fireEvent.keyDown(window, { key: "Alt", code: "AltRight", altKey: true });
    await waitFor(() => expect(mic.getAttribute("aria-pressed")).toBe("true"));
    now.mockReturnValue(2_000);
    fireEvent.keyUp(window, { key: "Alt", code: "AltRight" });
    await waitFor(() => expect(onText).toHaveBeenCalledWith("x", { send: false }));
    now.mockRestore();
  });

  it("asks for the words to be sent at once when Settings says so, and copies them when asked", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    const daemon = fakeDaemon({
      GetSettings: {
        entries: [
          setting("voice.dictation_auto_send", "true"),
          setting("voice.copy_transcript", "true"),
        ],
      },
      GetVoiceCatalog: catalog(true),
      Transcribe: { text: "ship it" },
    });
    const { onText } = mount(daemon);
    const mic = await startRecording();
    await act(async () => fireEvent.click(mic));
    await waitFor(() => expect(onText).toHaveBeenCalledWith("ship it", { send: true }));
    expect(writeText).toHaveBeenCalledWith("ship it");
  });

  it("hands the desktop shell a background hotkey only when asked, and clears it on leave", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("voice.background_shortcut", "true")] },
      GetVoiceCatalog: catalog(true),
    });
    const native = fakeNative();
    const { unmount } = renderApp(<VoiceInputButton onText={vi.fn()} onError={vi.fn()} />, {
      daemon,
      native,
      wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
    });
    await findMic();
    await waitFor(() => expect(native.dictationHotkeys).toContain("CmdOrCtrl+Shift+Space"));
    unmount();
    expect(native.dictationHotkeys[native.dictationHotkeys.length - 1]).toBe("");
  });

  it("registers no background hotkey by default", async () => {
    const native = fakeNative();
    renderApp(<VoiceInputButton onText={vi.fn()} onError={vi.fn()} />, {
      daemon: daemonWith(true),
      native,
      wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
    });
    await findMic();
    await act(async () => {});
    expect(native.dictationHotkeys.filter((h) => h !== "")).toEqual([]);
  });

  it("explains a microphone the browser cannot record from", async () => {
    vi.stubGlobal(
      "MediaRecorder",
      class {
        constructor() {
          throw new DOMException("no codec", "NotSupportedError");
        }
      },
    );
    const { onError } = mount(daemonWith(true));
    const mic = await findReadyMic();
    await act(async () => fireEvent.click(mic));
    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith(expect.stringMatching(/can't record from this/)),
    );
    expect(track.stop).toHaveBeenCalled();
    expect(mic.getAttribute("aria-pressed")).toBe("false");
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
    const mic = await findReadyMic();
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
    const mic = await findReadyMic();
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
    fireEvent.click(await findReadyMic());
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
    await waitFor(() => expect(queryMic()?.getAttribute("title")).toBe("Set up voice input"));
    expect(queryMic()?.getAttribute("aria-pressed")).toBe("false");
    expect(FakeRecorder.last?.state).toBe("inactive");
    expect(track.stop).toHaveBeenCalled();
    expect(daemon.calls.some((c) => c.method === "Transcribe")).toBe(false);
  });
  it("releases a stream granted after voice input was turned off during the prompt", async () => {
    let grant: (s: unknown) => void = () => {};
    getUserMedia.mockImplementation(() => new Promise((resolve) => (grant = resolve)));
    const daemon = fakeDaemon({
      GetSettings: { entries: [] },
      GetVoiceCatalog: catalog(true),
      SetSetting: {},
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
    fireEvent.click(await findReadyMic());
    daemon.answer("GetSettings", { entries: [setting("audio.enabled", "false")] });
    daemon.answer("GetVoiceCatalog", catalog(false));
    await act(async () => void (await save("audio.enabled", "false")));
    await waitFor(() => expect(queryMic()?.getAttribute("title")).toBe("Set up voice input"));
    await act(async () => grant({ getTracks: () => [track] }));
    expect(track.stop).toHaveBeenCalled();
    expect(FakeRecorder.last).toBeUndefined();
  });
});
