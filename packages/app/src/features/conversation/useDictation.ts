import { useEffect, useRef, useState } from "react";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { chordOf, isMacPlatform, useKeybind } from "../../lib/keybinds";
import { HOLD_MS, matchesShortcut, parseShortcut, releasesShortcut } from "./dictation";

export type DictationPhase = "idle" | "recording" | "transcribing";

/** Whether this browser can record at all. */
export function canRecord(): boolean {
  return (
    typeof MediaRecorder !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof navigator.mediaDevices?.getUserMedia === "function"
  );
}

/** The sample rate speech models expect; whisper.cpp takes nothing else. */
const WAV_RATE = 16_000;

/**
 * The recording as 16 kHz mono WAV, which every transcriber takes and the
 * on-device and command providers require. The browser records whatever it
 * likes (Opus in WebM, usually); decoding it here means the daemon never has
 * to carry a decoder. Returns the original when the browser cannot decode
 * its own recording, so a cloud provider still gets something it can read.
 */
export async function toWav16k(blob: Blob): Promise<Blob> {
  const Ctx =
    (globalThis as { AudioContext?: typeof AudioContext }).AudioContext ??
    (globalThis as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  const Offline = (globalThis as { OfflineAudioContext?: typeof OfflineAudioContext })
    .OfflineAudioContext;
  if (!Ctx || !Offline) return blob;
  try {
    const ctx = new Ctx();
    const decoded = await ctx.decodeAudioData(await blob.arrayBuffer());
    void ctx.close();
    const frames = Math.ceil(decoded.duration * WAV_RATE || 1);
    const offline = new Offline(1, frames, WAV_RATE);
    const source = offline.createBufferSource();
    source.buffer = decoded;
    source.connect(offline.destination);
    source.start();
    const mono = (await offline.startRendering()).getChannelData(0);
    return new Blob([encodeWav(mono, WAV_RATE)], { type: "audio/wav" });
  } catch {
    return blob;
  }
}

/** 16-bit PCM WAV with a 44-byte header. */
export function encodeWav(samples: Float32Array, rate: number): ArrayBuffer {
  const out = new ArrayBuffer(44 + samples.length * 2);
  const v = new DataView(out);
  const ascii = (at: number, text: string) => {
    for (let i = 0; i < text.length; i += 1) v.setUint8(at + i, text.charCodeAt(i));
  };
  ascii(0, "RIFF");
  v.setUint32(4, 36 + samples.length * 2, true);
  ascii(8, "WAVE");
  ascii(12, "fmt ");
  v.setUint32(16, 16, true);
  v.setUint16(20, 1, true);
  v.setUint16(22, 1, true);
  v.setUint32(24, rate, true);
  v.setUint32(28, rate * 2, true);
  v.setUint16(32, 2, true);
  v.setUint16(34, 16, true);
  ascii(36, "data");
  v.setUint32(40, samples.length * 2, true);
  let at = 44;
  for (const s of samples) {
    const clamped = Math.max(-1, Math.min(1, s));
    v.setInt16(at, clamped < 0 ? clamped * 0x8000 : clamped * 0x7fff, true);
    at += 2;
  }
  return out;
}

/** How often the level meter and the clock are read, in ms. */
const METER_INTERVAL_MS = 80;

/** A short tick at the start and end of a recording, when sounds are on. */
function beep(hz: number) {
  const Ctx =
    (globalThis as { AudioContext?: typeof AudioContext }).AudioContext ??
    (globalThis as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  if (!Ctx) return;
  try {
    const ctx = new Ctx();
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.frequency.value = hz;
    gain.gain.value = 0.05;
    osc.connect(gain).connect(ctx.destination);
    osc.start();
    osc.stop(ctx.currentTime + 0.08);
    osc.onended = () => void ctx.close();
  } catch {
    // No audio output is not a reason to stop recording.
  }
}

export interface DictationOptions {
  /** False keeps every control inert, as when transcription is not set up. */
  enabled: boolean;
  /** `voice.dictation_shortcut`; empty means no key. */
  shortcut: string;
  /** Hold the key to talk and release to send; a tap still toggles. */
  holdToRecord: boolean;
  /** Start/stop ticks. */
  beep: boolean;
  /** The microphone to use; empty takes the system default. */
  deviceId: string;
  /** BCP-47 hint for the transcriber; empty uses the configured language. */
  language: string;
  /** The recognized words, trimmed and non-empty. */
  onText: (text: string) => void;
  /** A plain-words failure; `undefined` clears an earlier one. */
  onError: (message: string | undefined) => void;
}

export interface Dictation {
  phase: DictationPhase;
  /** Whole seconds since the recording started. */
  seconds: number;
  /** 0–1 microphone level, for the meter. */
  level: number;
  /** Starts recording; a no-op while one is running. */
  start: () => void;
  /** Stops and transcribes. */
  stop: () => void;
  /** Stops and throws the audio away. */
  cancel: () => void;
  /** Start, or stop if recording — the click and the tap. */
  toggle: () => void;
}

/**
 * Records from the microphone and turns the recording into text through the
 * daemon's transcription chain. Audio goes to the daemon, never straight to a
 * provider, so keys stay where they are kept.
 *
 * Owns the shortcut too: the chord (or lone modifier) in Settings starts a
 * recording on keydown; a release after `HOLD_MS` stops it, a quick tap leaves
 * it running until the next tap, the mic click, or Escape.
 */
export function useDictation(options: DictationOptions): Dictation {
  const borg = useBorg();
  const [phase, setPhase] = useState<DictationPhase>("idle");
  const [seconds, setSeconds] = useState(0);
  const [level, setLevel] = useState(0);
  const recorder = useRef<MediaRecorder | null>(null);
  const meter = useRef<{
    ctx: AudioContext;
    /** Null where the page has no Web Audio: the clock runs, the bars stay flat. */
    analyser: AnalyserNode | null;
    timer: number;
  } | null>(null);
  // Set from the click until the recorder runs, so a second click during the
  // permission prompt cannot open a second stream.
  const starting = useRef(false);
  // Read by async work that outlives the render it was made in.
  const live = useRef(true);
  const opts = useRef(options);
  opts.current = options;
  const phaseRef = useRef(phase);
  phaseRef.current = phase;
  /** When the shortcut went down, to tell a hold from a tap on release. */
  const pressedAt = useRef<number | null>(null);

  const stopMeter = () => {
    const m = meter.current;
    meter.current = null;
    if (!m) return;
    clearInterval(m.timer);
    void m.ctx.close().catch(() => {});
  };

  /** Ends the recording without sending it. True when there was one. */
  const discard = () => {
    const r = recorder.current;
    recorder.current = null;
    stopMeter();
    if (!r) return false;
    r.onstop = null;
    if (r.state !== "inactive") r.stop();
    r.stream.getTracks().forEach((t) => t.stop());
    return true;
  };

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
      // Leaving the page must not leave the microphone on, nor send what it heard.
      discard();
    };
  }, []);

  // Voice input turned off mid-recording: nothing is left to stop it, so stop it here.
  useEffect(() => {
    if (!options.enabled) {
      if (discard())
        opts.current.onError("Voice input was turned off, so the recording was dropped.");
      setPhase("idle");
    }
  }, [options.enabled]);

  const transcribe = async (recorded: Blob) => {
    setPhase("transcribing");
    try {
      const blob = await toWav16k(recorded);
      const audio = new Uint8Array(await blob.arrayBuffer());
      const { text } = await borg.voice.transcribe({
        audio,
        mimeType: blob.type,
        language: opts.current.language,
      });
      if (!live.current) return;
      if (text.trim() === "") opts.current.onError("Didn't catch that. Try again a bit closer.");
      else opts.current.onText(text.trim());
    } catch (e) {
      if (live.current) opts.current.onError(asBorgError(e).message);
    } finally {
      if (live.current) setPhase("idle");
    }
  };

  const startMeter = (stream: MediaStream, startedAt: number) => {
    const Ctx =
      (globalThis as { AudioContext?: typeof AudioContext }).AudioContext ??
      (globalThis as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    // The clock runs even where the meter cannot.
    const tick = () => {
      setSeconds(Math.floor((Date.now() - startedAt) / 1000));
      const m = meter.current;
      if (!m?.analyser) return;
      const data = new Uint8Array(m.analyser.fftSize);
      m.analyser.getByteTimeDomainData(data);
      let sum = 0;
      for (const v of data) {
        const d = (v - 128) / 128;
        sum += d * d;
      }
      // RMS, lifted so quiet speech still moves the bars.
      setLevel(Math.min(1, Math.sqrt(sum / data.length) * 4));
    };
    if (!Ctx) {
      const timer = window.setInterval(tick, METER_INTERVAL_MS);
      meter.current = {
        ctx: { close: () => Promise.resolve() } as AudioContext,
        analyser: null,
        timer,
      };
      return;
    }
    try {
      const ctx = new Ctx();
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 512;
      ctx.createMediaStreamSource(stream).connect(analyser);
      const timer = window.setInterval(tick, METER_INTERVAL_MS);
      meter.current = { ctx, analyser, timer };
    } catch {
      const timer = window.setInterval(tick, METER_INTERVAL_MS);
      meter.current = {
        ctx: { close: () => Promise.resolve() } as AudioContext,
        analyser: null,
        timer,
      };
    }
  };

  const start = async () => {
    if (!opts.current.enabled || starting.current || recorder.current) return;
    starting.current = true;
    opts.current.onError(undefined);
    let stream: MediaStream;
    try {
      const deviceId = opts.current.deviceId;
      stream = await navigator.mediaDevices.getUserMedia({
        audio: deviceId ? { deviceId: { ideal: deviceId } } : true,
      });
    } catch {
      starting.current = false;
      if (live.current)
        opts.current.onError("Borg can't use the microphone. Allow it for this app and try again.");
      return;
    }
    starting.current = false;
    // The composer went away, or voice input was turned off, while the
    // permission prompt was open.
    if (!live.current || !opts.current.enabled) {
      stream.getTracks().forEach((t) => t.stop());
      return;
    }
    let r: MediaRecorder;
    try {
      r = new MediaRecorder(stream);
    } catch {
      // A device that grants the stream but cannot record it (a track that
      // ended, a codec the browser lacks) must not leave the button stuck.
      stream.getTracks().forEach((t) => t.stop());
      opts.current.onError("Borg can't record from this microphone. Try another one in Settings.");
      return;
    }
    const chunks: Blob[] = [];
    r.ondataavailable = (e) => {
      if (e.data.size > 0) chunks.push(e.data);
    };
    r.onstop = () => {
      stream.getTracks().forEach((t) => t.stop());
      recorder.current = null;
      stopMeter();
      if (opts.current.beep) beep(660);
      void transcribe(new Blob(chunks, { type: r.mimeType || "audio/webm" }));
    };
    recorder.current = r;
    r.start();
    if (opts.current.beep) beep(880);
    setSeconds(0);
    setLevel(0);
    startMeter(stream, Date.now());
    setPhase("recording");
  };

  const stop = () => {
    if (recorder.current?.state === "recording") recorder.current.stop();
  };

  const cancel = () => {
    if (discard()) {
      setPhase("idle");
      opts.current.onError(undefined);
    }
  };

  const toggle = () => {
    if (phaseRef.current === "recording") stop();
    else if (phaseRef.current === "idle") void start();
  };

  // The native menu's path, and the quick route for anything that just wants
  // "dictate" without knowing the chord.
  useKeybind("dictate", toggle, { enabled: options.enabled, inInputs: true });

  // Escape during a recording drops it. Registered while recording only, so
  // it sits above the composer's own Escape for exactly that long.
  useKeybind("cancel-turn", cancel, { enabled: phase === "recording", inInputs: true });

  const shortcut = options.shortcut;
  useEffect(() => {
    const parsed = parseShortcut(shortcut);
    if (!parsed || !options.enabled) return;
    const isMac = isMacPlatform();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || !matchesShortcut(parsed, event, (e) => chordOf(e, isMac)))
        return;
      event.preventDefault();
      // A held key repeats; the first press is the one that counts.
      if (event.repeat) return;
      if (phaseRef.current === "recording") {
        pressedAt.current = null;
        stop();
        return;
      }
      if (phaseRef.current !== "idle") return;
      pressedAt.current = Date.now();
      void start();
    };
    const onKeyUp = (event: KeyboardEvent) => {
      const at = pressedAt.current;
      if (at === null || !releasesShortcut(parsed, event)) return;
      pressedAt.current = null;
      if (!opts.current.holdToRecord) return;
      // A quick tap toggles: the recording stays on until the next press.
      if (Date.now() - at < HOLD_MS) return;
      if (phaseRef.current === "recording") stop();
      // Released during the permission prompt: nothing is recording yet, so
      // the start that follows stays on, as a tap would.
    };
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shortcut, options.enabled]);

  return { phase, seconds, level, start: () => void start(), stop, cancel, toggle };
}
