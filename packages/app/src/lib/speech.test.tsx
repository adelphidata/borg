import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { fakeDaemon, renderApp } from "../test/harness";
import { SettingsProvider } from "./settings";
import {
  hasDeviceVoice,
  resetSpeaker,
  speakableText,
  stopSpeaking,
  useAutoSpeak,
  useSpeaker,
  useSpeechSource,
} from "./speech";

/** jsdom has no voice; this is the one the browser would have. */
class FakeUtterance {
  static last: FakeUtterance | undefined;
  voice: unknown;
  onend: (() => void) | null = null;
  onerror: ((e: { error: string }) => void) | null = null;
  constructor(readonly text: string) {
    FakeUtterance.last = this;
  }
}
const synth = {
  spoken: [] as FakeUtterance[],
  cancelled: 0,
  speak(u: FakeUtterance) {
    synth.spoken.push(u);
  },
  cancel() {
    synth.cancelled += 1;
  },
  getVoices: () => [],
};

beforeEach(() => {
  synth.spoken = [];
  synth.cancelled = 0;
  FakeUtterance.last = undefined;
  resetSpeaker();
});

afterEach(() => {
  vi.unstubAllGlobals();
  resetSpeaker();
});

function withDeviceVoice() {
  vi.stubGlobal("speechSynthesis", synth);
  vi.stubGlobal("SpeechSynthesisUtterance", FakeUtterance);
}

describe("speakableText", () => {
  it("drops the markup a voice would read out loud", () => {
    expect(
      speakableText(
        "# Plan\n\n**Bold** and _soft_ with `code`, a [link](https://x.y) and\n\n```sh\nrm -rf\n```\n- one\n> quote",
      ),
    ).toBe("Plan\n\nBold and soft with code, a link and\n\none\nquote");
  });
});

describe("useSpeaker on a device with its own voice", () => {
  it("reports nothing to say without a voice anywhere", () => {
    expect(hasDeviceVoice()).toBe(false);
    const { result } = renderHook(() => useSpeaker());
    expect(result.current.canSpeak).toBe(false);
  });

  it("speaks the prose, stops when asked again by id, and clears on end", () => {
    withDeviceVoice();
    const { result } = renderHook(() => useSpeaker());
    expect(result.current.canSpeak).toBe(true);
    act(() => result.current.speak("r1", "**Hi** there"));
    expect(synth.spoken.map((u) => u.text)).toEqual(["Hi there"]);
    expect(result.current.state).toEqual({ id: "r1", phase: "speaking" });

    // The same id again is a stop, as a toggle should be.
    act(() => result.current.speak("r1", "**Hi** there"));
    expect(synth.cancelled).toBe(1);
    expect(result.current.state).toBeUndefined();

    act(() => result.current.speak("r2", "Next"));
    act(() => FakeUtterance.last?.onend?.());
    expect(result.current.state).toBeUndefined();
  });

  it("starting a second reply silences the first — one voice at a time", () => {
    withDeviceVoice();
    const { result } = renderHook(() => useSpeaker());
    act(() => result.current.speak("r1", "one"));
    act(() => result.current.speak("r2", "two"));
    expect(synth.cancelled).toBe(1);
    expect(result.current.state?.id).toBe("r2");
  });

  it("reports a device failure in words, but not its own interruption", () => {
    withDeviceVoice();
    const onError = vi.fn();
    const { result } = renderHook(() => useSpeaker(onError));
    act(() => result.current.speak("r1", "one"));
    act(() => FakeUtterance.last?.onerror?.({ error: "interrupted" }));
    expect(onError).not.toHaveBeenCalled();
    act(() => result.current.speak("r2", "two"));
    act(() => FakeUtterance.last?.onerror?.({ error: "synthesis-failed" }));
    expect(onError).toHaveBeenCalledWith(expect.stringMatching(/could not read/));
    expect(result.current.state).toBeUndefined();
  });

  it("says nothing for a reply that is only markup", () => {
    withDeviceVoice();
    const { result } = renderHook(() => useSpeaker());
    act(() => result.current.speak("r1", "```\n```"));
    expect(synth.spoken).toEqual([]);
  });
});

describe("useSpeaker through the daemon", () => {
  function Source() {
    useSpeechSource();
    return null;
  }

  it("asks the daemon to synthesize when a speech provider is ready", async () => {
    // No device voice here, so a reply can only be spoken through the daemon.
    const daemon = fakeDaemon({
      GetSettings: { entries: [] },
      GetVoiceCatalog: {
        speech: [],
        transcription: [],
        phone: [],
        speechReady: true,
        transcriptionReady: false,
        phoneProblem: "",
      },
      Synthesize: { audio: new Uint8Array([1, 2]), mimeType: "audio/mpeg" },
    });
    // The page lends the client; the button only reads the store.
    const play = vi.fn(() => Promise.resolve());
    vi.stubGlobal(
      "Audio",
      class {
        onended: (() => void) | null = null;
        onerror: (() => void) | null = null;
        src = "";
        constructor(readonly url: string) {}
        play = play;
        pause() {}
      },
    );
    Object.defineProperty(URL, "createObjectURL", { value: () => "blob:x", configurable: true });
    Object.defineProperty(URL, "revokeObjectURL", { value: () => {}, configurable: true });
    renderApp(<Source />, {
      daemon,
      wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
    });
    const { result } = renderHook(() => useSpeaker());
    await vi.waitFor(() => expect(result.current.canSpeak).toBe(true));
    await act(async () => result.current.speak("r1", "**Hello**"));
    await vi.waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "Synthesize")?.request).toEqual({
        text: "**Hello**",
        voice: "",
      }),
    );
    await vi.waitFor(() => expect(play).toHaveBeenCalled());
    expect(result.current.state).toEqual({ id: "r1", phase: "speaking" });
    act(() => stopSpeaking());
    expect(result.current.state).toBeUndefined();
  });
});

describe("useAutoSpeak", () => {
  it("reads the reply that just finished, once, and only when turned on", () => {
    withDeviceVoice();
    const reply = { id: "r1", text: "Done." };
    const { rerender } = renderHook(
      ({ enabled, busy }: { enabled: boolean; busy: boolean }) =>
        useAutoSpeak(enabled, busy, reply),
      { initialProps: { enabled: true, busy: true } },
    );
    expect(synth.spoken).toEqual([]);
    rerender({ enabled: true, busy: false });
    expect(synth.spoken.map((u) => u.text)).toEqual(["Done."]);
    // Idle renders do not repeat it.
    rerender({ enabled: true, busy: false });
    expect(synth.spoken).toHaveLength(1);
  });

  it("stays silent when off", () => {
    withDeviceVoice();
    const { rerender } = renderHook(
      ({ busy }: { busy: boolean }) => useAutoSpeak(false, busy, { id: "r1", text: "Done." }),
      { initialProps: { busy: true } },
    );
    rerender({ busy: false });
    expect(synth.spoken).toEqual([]);
  });
});
