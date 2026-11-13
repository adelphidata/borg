import { vi } from "vitest";

/**
 * A stand-in for the browser's capture stack: `getUserMedia`,
 * `getDisplayMedia` and an `AudioContext` with only the ScriptProcessor path
 * (jsdom has no AudioWorklet, which is also what the code falls back to).
 * A test pushes frames into a context's processor as the audio thread would.
 */

export interface FakeTrack {
  kind: "audio" | "video";
  stopped: boolean;
  stop: () => void;
}

export interface FakeStream {
  tracks: FakeTrack[];
  getTracks(): FakeTrack[];
  getAudioTracks(): FakeTrack[];
  getVideoTracks(): FakeTrack[];
}

export function fakeStream(kinds: ("audio" | "video")[]): FakeStream {
  const tracks = kinds.map((kind) => {
    const t: FakeTrack = { kind, stopped: false, stop: () => (t.stopped = true) };
    return t;
  });
  return {
    tracks,
    getTracks: () => tracks,
    getAudioTracks: () => tracks.filter((t) => t.kind === "audio"),
    getVideoTracks: () => tracks.filter((t) => t.kind === "video"),
  };
}

export interface FakeContext {
  sampleRate: number;
  closed: boolean;
  stream?: FakeStream;
  processor?: { onaudioprocess: ((e: unknown) => void) | null };
  /** Delivers one frame, as the audio thread would. */
  feed(frame: Float32Array): void;
}

export interface FakeAudio {
  contexts: FakeContext[];
  mic: FakeStream;
  /** What `getDisplayMedia` resolves. */
  display: FakeStream;
  getUserMedia: ReturnType<typeof vi.fn>;
  getDisplayMedia: ReturnType<typeof vi.fn>;
  restore(): void;
}

/** Installs the fakes on `navigator` and `globalThis`; `restore()` takes them away. */
export function installFakeAudio(
  options: {
    displayFails?: number;
    micError?: Error;
    /** Holds `getDisplayMedia` until the returned release is called. */
    displayGate?: Promise<void>;
    /** Gives contexts an AudioWorklet, the path a real Chromium takes. */
    worklet?: boolean;
    /** Holds every worklet module load until it settles (needs `worklet`). */
    workletGate?: Promise<void>;
  } = {},
): FakeAudio {
  const contexts: FakeContext[] = [];
  const mic = fakeStream(["audio"]);
  const display = fakeStream(["video", "audio"]);
  let displayFailures = options.displayFails ?? 0;

  class FakeAudioContext {
    sampleRate = 16_000;
    closed = false;
    stream?: FakeStream;
    processor?: { onaudioprocess: ((e: unknown) => void) | null };
    destination = {};
    audioWorklet = options.worklet
      ? {
          addModule: async () => {
            await options.workletGate;
          },
        }
      : undefined;
    workletPort?: { onmessage: ((e: { data: Float32Array }) => void) | null };
    constructor() {
      contexts.push(this as unknown as FakeContext);
    }
    createMediaStreamSource(stream: FakeStream) {
      this.stream = stream;
      return { connect: (node: unknown) => node };
    }
    createGain() {
      return { gain: { value: 1 }, connect: () => {} };
    }
    createScriptProcessor() {
      const node = { onaudioprocess: null, connect: (next: unknown) => next };
      this.processor = node;
      return node;
    }
    close() {
      this.closed = true;
      return Promise.resolve();
    }
    feed(frame: Float32Array) {
      this.processor?.onaudioprocess?.({ inputBuffer: { getChannelData: () => frame } });
      this.workletPort?.onmessage?.({ data: frame });
    }
  }

  class FakeWorkletNode {
    port: { onmessage: ((e: { data: Float32Array }) => void) | null } = { onmessage: null };
    constructor(context: FakeAudioContext) {
      context.workletPort = this.port;
      // Counts as wired for tests that wait on a processor.
      context.processor = { onaudioprocess: null };
    }
    connect(next: unknown) {
      return next;
    }
  }

  const getUserMedia = vi.fn(async () => {
    if (options.micError) throw options.micError;
    return mic;
  });
  const getDisplayMedia = vi.fn(async () => {
    await options.displayGate;
    if (displayFailures > 0) {
      displayFailures -= 1;
      throw new Error("NotAllowedError");
    }
    return display;
  });
  const previousMedia = Object.getOwnPropertyDescriptor(navigator, "mediaDevices");
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: { getUserMedia, getDisplayMedia },
  });
  const previousCtx = (globalThis as { AudioContext?: unknown }).AudioContext;
  (globalThis as { AudioContext?: unknown }).AudioContext = FakeAudioContext;
  const g = globalThis as { AudioWorkletNode?: unknown; URL: { createObjectURL?: unknown } };
  const previousNode = g.AudioWorkletNode;
  const previousObjectUrl = g.URL.createObjectURL;
  if (options.worklet) {
    g.AudioWorkletNode = FakeWorkletNode;
    g.URL.createObjectURL = () => "blob:pcm-worklet";
  }

  return {
    contexts,
    mic,
    display,
    getUserMedia,
    getDisplayMedia,
    restore() {
      if (previousMedia) Object.defineProperty(navigator, "mediaDevices", previousMedia);
      else delete (navigator as { mediaDevices?: unknown }).mediaDevices;
      (globalThis as { AudioContext?: unknown }).AudioContext = previousCtx;
      g.AudioWorkletNode = previousNode;
      g.URL.createObjectURL = previousObjectUrl;
    },
  };
}

/** `seconds` of a quiet tone at 16 kHz. */
export function tone(seconds: number): Float32Array {
  return Float32Array.from(
    { length: Math.round(seconds * 16_000) },
    (_, i) => Math.sin(i / 10) * 0.1,
  );
}
