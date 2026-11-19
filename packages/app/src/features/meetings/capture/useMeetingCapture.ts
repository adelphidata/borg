import { useCallback, useEffect, useRef, useState } from "react";
import { BorgError, GrpcStatus, meetings, type BorgClient } from "@borg/client";
import { readLocalVoiceChoice } from "../../../lib/voice";
import { useBorg } from "../../../lib/useDaemon";
import { WAV_RATE, encodeWav } from "../../../lib/wav";
import { meetingsChanged } from "../meetingsStore";
import {
  PcmChunker,
  PushQueue,
  firstSeqFor,
  rms,
  type PcmChunk,
  type QueueReport,
} from "./chunker";
import { PCM_WORKLET_NAME, pcmWorkletUrl } from "./pcmWorklet";

/** Whether the other side of the call is being recorded. */
export type SystemAudio = "starting" | "on" | "unavailable";

export interface MeetingCapture {
  /** True while audio is flowing to the daemon. */
  capturing: boolean;
  /** The microphone's level, 0–1, for a meter. */
  level: number;
  systemAudio: SystemAudio;
  /** Why capture could not start at all, in words for the user. */
  error?: string;
  /** Something went wrong with delivery; capture carries on. */
  warning?: string;
  /**
   * Ends the meeting: sends the audio still buffered, waits for every chunk
   * to land, releases the devices, then `StopMeeting`.
   */
  stop: () => Promise<void>;
}

/** One captured source and everything that has to be released with it. */
interface TrackPipe {
  track: meetings.Track;
  stream: MediaStream;
  context: AudioContext;
  chunker: PcmChunker;
  queue: PushQueue<PcmChunk>;
}

/**
 * What to say when a chunk could not be delivered. A refusal the daemon
 * wrote for the person (a rejected key, no credit, meetings turned off)
 * is shown as it is, since it names the fix; anything else means the
 * audio is not getting through.
 */
export function failureWarning(error: unknown): string {
  if (error instanceof BorgError && error.code === GrpcStatus.FailedPrecondition) {
    return error.message;
  }
  return "Borg isn't receiving the audio. Recording continues.";
}

/** The other side of the call, or undefined when this platform cannot record it. */
async function systemStream(): Promise<MediaStream | undefined> {
  const media = navigator.mediaDevices;
  if (typeof media?.getDisplayMedia !== "function") return undefined;
  // The desktop shell answers the first request with system audio alone; if
  // this Chromium refuses that shape, the second request gets a screen with it.
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      const stream = await media.getDisplayMedia({ audio: true, video: true });
      // Only the sound is wanted. Stopping the picture at once also takes the
      // screen-sharing indicator down.
      for (const video of stream.getVideoTracks()) video.stop();
      if (stream.getAudioTracks().length > 0) return stream;
      return undefined;
    } catch (error) {
      console.warn("system audio unavailable", error);
    }
  }
  return undefined;
}

/**
 * Turns a stream into frames: an AudioWorklet where there is one, the older
 * ScriptProcessor otherwise. Neither plays anything — the graph ends in a
 * muted gain, which is what keeps the processor pulled.
 */
async function tap(
  context: AudioContext,
  stream: MediaStream,
  onFrame: (frame: Float32Array) => void,
): Promise<void> {
  const source = context.createMediaStreamSource(stream);
  const mute = context.createGain();
  mute.gain.value = 0;
  mute.connect(context.destination);
  if (context.audioWorklet && typeof AudioWorkletNode !== "undefined") {
    try {
      await context.audioWorklet.addModule(pcmWorkletUrl());
      const node = new AudioWorkletNode(context, PCM_WORKLET_NAME);
      node.port.onmessage = (e: MessageEvent<Float32Array>) => onFrame(e.data);
      source.connect(node).connect(mute);
      return;
    } catch (error) {
      console.warn("audio worklet unavailable; using a script processor", error);
    }
  }
  const node = context.createScriptProcessor(4096, 1, 1);
  node.onaudioprocess = (e) => onFrame(e.inputBuffer.getChannelData(0).slice());
  source.connect(node).connect(mute);
}

/**
 * Records a meeting from this machine and streams it to the daemon.
 *
 * Two tracks: the microphone ("Me") and the system's audio ("Them"). Each is
 * cut into 25-second WAV chunks and pushed with `PushAudio` in order; the
 * daemon transcribes them as they land. When the system's audio cannot be
 * had, capture carries on with the microphone and says so.
 *
 * Runs while `active` is true. Going inactive — the meeting stopped
 * elsewhere — releases the devices without calling `StopMeeting` again.
 */
export function useMeetingCapture(
  meetingId: string,
  options: { active: boolean; startedAtMs: number },
): MeetingCapture {
  const borg = useBorg();
  const [capturing, setCapturing] = useState(false);
  const [level, setLevel] = useState(0);
  const [systemAudio, setSystemAudio] = useState<SystemAudio>("starting");
  const [error, setError] = useState<string>();
  const [warning, setWarning] = useState<string>();
  const pipes = useRef<TrackPipe[]>([]);
  const starting = useRef<Promise<void> | undefined>(undefined);
  const borgRef = useRef<BorgClient>(borg);
  borgRef.current = borg;

  const release = useCallback(() => {
    for (const pipe of pipes.current) {
      for (const t of pipe.stream.getTracks()) t.stop();
      void pipe.context.close().catch(() => {});
    }
    pipes.current = [];
    setCapturing(false);
  }, []);

  const { active } = options;
  // Read when capture starts, not a reason to restart it.
  const startedAtRef = useRef(options.startedAtMs);
  startedAtRef.current = options.startedAtMs;
  useEffect(() => {
    if (!active) return;
    let live = true;
    const opened: TrackPipe[] = [];
    const report = (r: QueueReport) => {
      if (!live) return;
      if (r.failed) setWarning(failureWarning(r.error));
      else if (r.dropped) setWarning("Borg is falling behind; some audio was skipped.");
      else if (r.delivered) setWarning(undefined);
    };
    const open = async (track: meetings.Track, stream: MediaStream) => {
      const Ctx = globalThis.AudioContext;
      const context = new Ctx({ sampleRate: WAV_RATE });
      const rate = context.sampleRate;
      const baseOffsetMs = Math.max(0, Date.now() - startedAtRef.current);
      const queue = new PushQueue<PcmChunk>(
        (chunk) =>
          borgRef.current.meetings.pushAudio({
            meetingId,
            track,
            seq: chunk.seq,
            offsetMs: chunk.offsetMs,
            wav: new Uint8Array(encodeWav(chunk.samples, rate)),
          }),
        report,
      );
      const chunker = new PcmChunker((chunk) => queue.enqueue(chunk), rate, {
        baseOffsetMs,
        firstSeq: firstSeqFor(baseOffsetMs),
      });
      const pipe: TrackPipe = { track, stream, context, chunker, queue };
      opened.push(pipe);
      await tap(context, stream, (frame) => {
        chunker.push(frame);
        if (track === meetings.Track.MIC && live) setLevel(Math.min(1, rms(frame) * 4));
      });
      return pipe;
    };

    const run = async () => {
      const media = navigator.mediaDevices;
      if (typeof media?.getUserMedia !== "function") {
        setError("This window can't record audio.");
        return;
      }
      const deviceId = readLocalVoiceChoice("microphone");
      let mic: MediaStream;
      try {
        mic = await media.getUserMedia({
          audio: deviceId ? { deviceId: { ideal: deviceId } } : true,
        });
      } catch (e) {
        setError(
          `Borg can't use the microphone: ${e instanceof Error ? e.message : String(e)}. Allow it in your system settings.`,
        );
        return;
      }
      const system = await systemStream();
      if (!live) {
        for (const t of [...mic.getTracks(), ...(system?.getTracks() ?? [])]) t.stop();
        return;
      }
      setSystemAudio(system ? "on" : "unavailable");
      await open(meetings.Track.MIC, mic);
      // The page may have gone while the mic was starting. Cleanup has already
      // run, so a system pipe opened now would never be released.
      if (!live) {
        for (const t of system?.getTracks() ?? []) t.stop();
        return;
      }
      if (system) await open(meetings.Track.SYSTEM, system);
      if (!live) return;
      pipes.current = opened;
      setCapturing(true);
    };
    starting.current = run().catch((e: unknown) => {
      if (live) setError(`Recording couldn't start: ${e instanceof Error ? e.message : String(e)}`);
    });

    return () => {
      live = false;
      for (const pipe of opened) {
        for (const t of pipe.stream.getTracks()) t.stop();
        void pipe.context.close().catch(() => {});
      }
      pipes.current = [];
      setCapturing(false);
    };
  }, [active, meetingId]);

  const stop = useCallback(async () => {
    await starting.current;
    const current = pipes.current;
    for (const pipe of current) pipe.chunker.flush();
    await Promise.all(current.map((p) => p.queue.drain()));
    release();
    await borgRef.current.meetings.stopMeeting({ id: meetingId });
    meetingsChanged();
  }, [meetingId, release]);

  return { capturing, level, systemAudio, error, warning, stop };
}
