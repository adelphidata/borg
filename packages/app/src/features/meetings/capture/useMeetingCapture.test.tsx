import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, waitFor } from "@testing-library/react";
import { meetings } from "@borg/client";
import { fakeDaemon, renderApp, type FakeDaemon } from "../../../test/harness";
import { installFakeAudio, tone, type FakeAudio } from "../../../test/audio";
import { useMeetingCapture, type MeetingCapture } from "./useMeetingCapture";

let audio: FakeAudio;
let latest: MeetingCapture;

/** Later than any capture start in these tests, so offsets count from 0 exactly. */
const STARTED_AT_MS = Date.now() + 60_000;

function Probe({ active = true }: { active?: boolean }) {
  latest = useMeetingCapture("m1", { active, startedAtMs: STARTED_AT_MS });
  return null;
}

function mount(daemon: FakeDaemon = fakeDaemon({ PushAudio: { appended: [] }, StopMeeting: {} })) {
  const view = renderApp(<Probe />, { daemon });
  return { ...view, daemon };
}

const pushes = (daemon: FakeDaemon) =>
  daemon.calls
    .filter((c) => c.method === "PushAudio")
    .map((c) => {
      const r = c.request as meetings.PushAudioRequest;
      return [r.track, r.seq, r.offsetMs];
    });

/** The capture is wired once both contexts (mic, then system) have a processor. */
const ready = async (count = 2) =>
  waitFor(() => {
    expect(audio.contexts.filter((c) => c.processor).length).toBe(count);
    expect(latest.capturing).toBe(true);
  });

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => audio.restore());

describe("useMeetingCapture", () => {
  it("pushes each track as numbered 25 s WAV chunks, Me and Them apart", async () => {
    audio = installFakeAudio();
    const { daemon } = mount();
    await ready();
    const [mic, system] = audio.contexts;
    act(() => {
      for (let i = 0; i < 3; i += 1) mic!.feed(tone(25));
      system!.feed(tone(25));
    });
    await waitFor(() => expect(pushes(daemon)).toHaveLength(4));
    expect(pushes(daemon)).toEqual([
      [meetings.Track.MIC, 0, 0],
      [meetings.Track.SYSTEM, 0, 0],
      [meetings.Track.MIC, 1, 25_000],
      [meetings.Track.MIC, 2, 50_000],
    ]);
    const wav = (daemon.calls.find((c) => c.method === "PushAudio")!.request as { wav: Uint8Array })
      .wav;
    expect(String.fromCharCode(...wav.slice(0, 4))).toBe("RIFF");
    expect(wav.byteLength).toBe(44 + 25 * 16_000 * 2);
  });

  it("drops the screen picture the moment system audio arrives", async () => {
    audio = installFakeAudio();
    mount();
    await ready();
    expect(audio.display.getVideoTracks()[0]!.stopped).toBe(true);
    expect(audio.display.getAudioTracks()[0]!.stopped).toBe(false);
    expect(latest.systemAudio).toBe("on");
  });

  it("asks twice for system audio, then carries on with the microphone alone", async () => {
    audio = installFakeAudio({ displayFails: 2 });
    const { daemon } = mount();
    await ready(1);
    expect(audio.getDisplayMedia).toHaveBeenCalledTimes(2);
    expect(latest.systemAudio).toBe("unavailable");
    act(() => audio.contexts[0]!.feed(tone(25)));
    await waitFor(() => expect(pushes(daemon)).toEqual([[meetings.Track.MIC, 0, 0]]));
  });

  it("takes the second, fuller system-audio answer when the first is refused", async () => {
    audio = installFakeAudio({ displayFails: 1 });
    mount();
    await ready();
    expect(latest.systemAudio).toBe("on");
  });

  it("sends the last partial chunk before it stops the meeting", async () => {
    audio = installFakeAudio();
    const { daemon } = mount();
    await ready();
    act(() => audio.contexts[0]!.feed(tone(3)));
    await act(() => latest.stop());
    const order = daemon.calls.map((c) => c.method).filter((m) => m !== "GetSettings");
    expect(order).toEqual(["PushAudio", "StopMeeting"]);
    expect(pushes(daemon)).toEqual([[meetings.Track.MIC, 0, 0]]);
    expect(audio.mic.tracks[0]!.stopped).toBe(true);
    expect(audio.contexts.every((c) => c.closed)).toBe(true);
  });

  it("retries a refused chunk once, then says the audio isn't arriving and keeps going", async () => {
    audio = installFakeAudio();
    const daemon = fakeDaemon({ PushAudio: new Error("transcriber down"), StopMeeting: {} });
    mount(daemon);
    await ready();
    act(() => audio.contexts[0]!.feed(tone(25)));
    await waitFor(() => expect(latest.warning).toMatch(/isn't receiving the audio/));
    expect(pushes(daemon)).toEqual([
      [meetings.Track.MIC, 0, 0],
      [meetings.Track.MIC, 0, 0],
    ]);
    expect(latest.capturing).toBe(true);
  });

  it("says why when the microphone is refused", async () => {
    audio = installFakeAudio({ micError: new Error("Permission denied") });
    mount();
    await waitFor(() =>
      expect(latest.error).toMatch(/can't use the microphone: Permission denied/),
    );
    expect(audio.getDisplayMedia).not.toHaveBeenCalled();
  });

  it("releases the devices when the page goes away", async () => {
    audio = installFakeAudio();
    const { unmount } = mount();
    await ready();
    unmount();
    expect(audio.mic.tracks[0]!.stopped).toBe(true);
    expect(audio.display.getAudioTracks()[0]!.stopped).toBe(true);
    expect(audio.contexts.every((c) => c.closed)).toBe(true);
  });

  it("records nothing while inactive", async () => {
    audio = installFakeAudio();
    renderApp(<Probe active={false} />, { daemon: fakeDaemon() });
    await act(async () => {});
    expect(audio.getUserMedia).not.toHaveBeenCalled();
  });

  it("takes frames from an AudioWorklet where the browser has one", async () => {
    audio = installFakeAudio({ worklet: true });
    const { daemon } = mount();
    await ready();
    act(() => audio.contexts[0]!.feed(tone(25)));
    await waitFor(() => expect(pushes(daemon)).toEqual([[meetings.Track.MIC, 0, 0]]));
  });

  it("records from the microphone chosen in Voice settings", async () => {
    localStorage.setItem("borg.voice.microphone", "usb-mic-1");
    audio = installFakeAudio();
    mount();
    await ready();
    expect(audio.getUserMedia).toHaveBeenCalledWith({
      audio: { deviceId: { ideal: "usb-mic-1" } },
    });
  });

  it("says so in a window that cannot record at all", async () => {
    audio = installFakeAudio();
    delete (navigator as { mediaDevices?: unknown }).mediaDevices;
    mount();
    await waitFor(() => expect(latest.error).toBe("This window can't record audio."));
  });

  it("lets go of a microphone it was granted after the page has gone", async () => {
    let release!: () => void;
    audio = installFakeAudio({ displayGate: new Promise<void>((r) => (release = r)) });
    const { unmount } = mount();
    await waitFor(() => expect(audio.getDisplayMedia).toHaveBeenCalled());
    unmount();
    await act(async () => release());
    await waitFor(() => expect(audio.mic.tracks[0]!.stopped).toBe(true));
    expect(audio.display.getAudioTracks()[0]!.stopped).toBe(true);
    expect(audio.contexts).toHaveLength(0);
  });

  it("never opens system audio after the page went away while the mic was still starting", async () => {
    let release!: () => void;
    audio = installFakeAudio({
      worklet: true,
      workletGate: new Promise<void>((r) => (release = r)),
    });
    const { unmount } = mount();
    // The mic's context exists and is loading its worklet.
    await waitFor(() => expect(audio.contexts).toHaveLength(1));
    unmount();
    await act(async () => release());
    await act(async () => {});
    expect(audio.display.getAudioTracks()[0]!.stopped).toBe(true);
    expect(audio.contexts.every((c) => c.closed)).toBe(true);
  });
});
