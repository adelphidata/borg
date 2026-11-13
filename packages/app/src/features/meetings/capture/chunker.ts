/**
 * The pure half of meeting capture: cutting a track's samples into chunks and
 * getting them to the daemon in order. No Web Audio here, so all of it runs
 * under test exactly as it runs in the panel.
 */

/** How much audio one `PushAudio` carries. Long enough for a transcriber to
 * have context, short enough that the transcript trails the call by seconds. */
export const CHUNK_MS = 25_000;

/** Chunks a track may have waiting behind the one in flight before old ones are dropped. */
export const MAX_BUFFERED = 8;

export interface PcmChunk {
  /** Per-track chunk number; `PushAudio` is idempotent on it. */
  seq: number;
  /** Where the chunk starts, in ms from the start of the meeting. */
  offsetMs: number;
  samples: Float32Array;
}

/** Root-mean-square level of a frame, 0 for silence up to 1 at full scale. */
export function rms(frame: Float32Array): number {
  if (frame.length === 0) return 0;
  let sum = 0;
  for (const s of frame) sum += s * s;
  return Math.sqrt(sum / frame.length);
}

/**
 * Accumulates frames and emits a chunk every `chunkMs` of audio.
 *
 * Offsets are counted in samples, not wall-clock time, so a chunk's offset is
 * exactly where its audio sits however late the browser delivered it.
 */
export class PcmChunker {
  private buffered: Float32Array[] = [];
  private bufferedLength = 0;
  private emitted = 0;
  private seq: number;
  private readonly chunkSamples: number;

  constructor(
    private readonly emit: (chunk: PcmChunk) => void,
    private readonly rate: number,
    options: {
      /** The meeting time the first sample belongs to. */
      baseOffsetMs?: number;
      /** The first chunk's number. */
      firstSeq?: number;
      chunkMs?: number;
    } = {},
  ) {
    this.baseOffsetMs = options.baseOffsetMs ?? 0;
    this.seq = options.firstSeq ?? 0;
    this.chunkSamples = Math.round(((options.chunkMs ?? CHUNK_MS) * rate) / 1000);
  }

  private readonly baseOffsetMs: number;

  push(frame: Float32Array): void {
    let rest = frame;
    while (rest.length > 0) {
      const room = this.chunkSamples - this.bufferedLength;
      const take = rest.subarray(0, room);
      this.buffered.push(take.slice());
      this.bufferedLength += take.length;
      rest = rest.subarray(take.length);
      if (this.bufferedLength === this.chunkSamples) this.cut();
    }
  }

  /** Emits whatever is buffered as a short final chunk. */
  flush(): void {
    if (this.bufferedLength > 0) this.cut();
  }

  private cut(): void {
    const samples = new Float32Array(this.bufferedLength);
    let at = 0;
    for (const part of this.buffered) {
      samples.set(part, at);
      at += part.length;
    }
    const offsetMs = this.baseOffsetMs + Math.round((this.emitted * 1000) / this.rate);
    this.emitted += this.bufferedLength;
    this.buffered = [];
    this.bufferedLength = 0;
    this.emit({ seq: this.seq++, offsetMs, samples });
  }
}

/**
 * The first chunk number for a capture that starts `baseOffsetMs` into the
 * meeting.
 *
 * A panel that reopens mid-meeting must not reuse a number an earlier capture
 * already sent — the daemon would drop the chunk as a repeat. An earlier
 * capture that ended without Stop sent only whole chunks, each finished
 * before this one began, so every number it used is below
 * `floor(baseOffsetMs / chunkMs)`. A capture from the start numbers from 0.
 */
export function firstSeqFor(baseOffsetMs: number, chunkMs = CHUNK_MS): number {
  return Math.max(0, Math.floor(baseOffsetMs / chunkMs));
}

/** What a `PushQueue` reports as it goes. */
export interface QueueReport {
  /** A chunk failed twice and was given up on. */
  failed?: boolean;
  /** Why the last attempt failed, when `failed`. */
  error?: unknown;
  /** A chunk was dropped because too many were waiting. */
  dropped?: boolean;
  /** A chunk went through, so whatever was wrong has cleared. */
  delivered?: boolean;
}

/**
 * Sends one track's chunks one at a time, in order.
 *
 * A failed send is tried once more and then given up on: losing 25 seconds of
 * transcript is better than stopping the recording. When the daemon falls
 * behind, at most `MAX_BUFFERED` chunks wait; past that the oldest waiting
 * chunk is dropped so the transcript stays close to live.
 */
export class PushQueue<T> {
  private waiting: T[] = [];
  private running: Promise<void> | undefined;

  constructor(
    private readonly send: (item: T) => Promise<unknown>,
    private readonly report: (r: QueueReport) => void,
    private readonly max = MAX_BUFFERED,
  ) {}

  enqueue(item: T): void {
    this.waiting.push(item);
    if (this.waiting.length > this.max) {
      this.waiting.shift();
      this.report({ dropped: true });
    }
    this.running ??= this.run();
  }

  /** Resolves once everything enqueued so far has been sent or given up on. */
  drain(): Promise<void> {
    return this.running ?? Promise.resolve();
  }

  private async run(): Promise<void> {
    while (this.waiting.length > 0) {
      const item = this.waiting.shift()!;
      try {
        await this.send(item);
        this.report({ delivered: true });
      } catch {
        try {
          await this.send(item);
          this.report({ delivered: true });
        } catch (error) {
          this.report({ failed: true, error });
        }
      }
    }
    this.running = undefined;
  }
}
