import { describe, expect, it, vi } from "vitest";
import {
  CHUNK_MS,
  MAX_BUFFERED,
  PcmChunker,
  PushQueue,
  firstSeqFor,
  rms,
  type PcmChunk,
} from "./chunker";

const RATE = 16_000;
const seconds = (s: number) => new Float32Array(Math.round(s * RATE));

function chunker(options?: ConstructorParameters<typeof PcmChunker>[2]) {
  const out: PcmChunk[] = [];
  return { c: new PcmChunker((chunk) => out.push(chunk), RATE, options), out };
}

describe("PcmChunker", () => {
  it("cuts every 25 s of audio into a numbered chunk with its offset in the meeting", () => {
    const { c, out } = chunker();
    // Delivered in uneven frames, the way an audio graph hands them over.
    for (let i = 0; i < 76; i += 1) c.push(seconds(1));
    expect(out.map((ch) => [ch.seq, ch.offsetMs, ch.samples.length])).toEqual([
      [0, 0, 25 * RATE],
      [1, 25_000, 25 * RATE],
      [2, 50_000, 25 * RATE],
    ]);
  });

  it("splits a frame that straddles a cut, losing nothing", () => {
    const { c, out } = chunker();
    const frame = Float32Array.from({ length: 30 * RATE }, (_, i) => i);
    c.push(frame);
    c.flush();
    expect(out).toHaveLength(2);
    expect(out[0]!.samples[25 * RATE - 1]).toBe(25 * RATE - 1);
    expect(out[1]!.samples[0]).toBe(25 * RATE);
    expect(out[1]!.samples.length).toBe(5 * RATE);
  });

  it("sends the partial chunk on flush, and nothing when there is nothing", () => {
    const { c, out } = chunker();
    c.push(seconds(25));
    c.push(seconds(3));
    c.flush();
    expect(out.map((ch) => [ch.seq, ch.offsetMs, ch.samples.length])).toEqual([
      [0, 0, 25 * RATE],
      [1, 25_000, 3 * RATE],
    ]);
    c.flush();
    expect(out).toHaveLength(2);
  });

  it("places a late start's chunks where they sit in the meeting", () => {
    const { c, out } = chunker({ baseOffsetMs: 70_000, firstSeq: firstSeqFor(70_000) });
    c.push(seconds(26));
    expect(out.map((ch) => [ch.seq, ch.offsetMs])).toEqual([[2, 70_000]]);
  });
});

describe("firstSeqFor", () => {
  it("numbers from zero within the first chunk of the meeting", () => {
    expect(firstSeqFor(0)).toBe(0);
    expect(firstSeqFor(1_500)).toBe(0);
  });

  it("starts a reopened capture past every whole chunk an earlier one could have sent", () => {
    // An earlier capture's chunks all finished before this moment, so the
    // highest number it can have used is one below floor(offset / 25 s).
    expect(firstSeqFor(CHUNK_MS)).toBe(1);
    expect(firstSeqFor(3 * CHUNK_MS - 1)).toBe(2);
    expect(firstSeqFor(3 * CHUNK_MS)).toBe(3);
  });
});

describe("rms", () => {
  it("reads a full-scale sine at 1/√2 and silence at 0", () => {
    const sine = Float32Array.from({ length: 1600 }, (_, i) => Math.sin((2 * Math.PI * i) / 160));
    expect(rms(sine)).toBeCloseTo(Math.SQRT1_2, 3);
    expect(rms(new Float32Array(64))).toBe(0);
    expect(rms(new Float32Array(0))).toBe(0);
  });
});

describe("PushQueue", () => {
  it("sends one at a time, in order", async () => {
    const sent: number[] = [];
    let inFlight = 0;
    let most = 0;
    const q = new PushQueue<number>(async (n) => {
      inFlight += 1;
      most = Math.max(most, inFlight);
      await Promise.resolve();
      sent.push(n);
      inFlight -= 1;
    }, vi.fn());
    [0, 1, 2].forEach((n) => q.enqueue(n));
    await q.drain();
    expect(sent).toEqual([0, 1, 2]);
    expect(most).toBe(1);
  });

  it("tries a failed chunk once more, then gives up on it and carries on", async () => {
    const reports: unknown[] = [];
    const attempts: number[] = [];
    const q = new PushQueue<number>(
      async (n) => {
        attempts.push(n);
        if (n === 0) throw new Error("daemon away");
      },
      (r) => reports.push(r),
    );
    q.enqueue(0);
    q.enqueue(1);
    await q.drain();
    expect(attempts).toEqual([0, 0, 1]);
    expect(reports).toEqual([{ failed: true }, { delivered: true }]);
  });

  it("recovers on the retry without reporting a failure", async () => {
    const reports: unknown[] = [];
    let first = true;
    const q = new PushQueue<number>(
      async () => {
        if (first) {
          first = false;
          throw new Error("blip");
        }
      },
      (r) => reports.push(r),
    );
    q.enqueue(0);
    await q.drain();
    expect(reports).toEqual([{ delivered: true }]);
  });

  it(`drops the oldest waiting chunk past ${MAX_BUFFERED} and says so`, async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const sent: number[] = [];
    const reports: unknown[] = [];
    const q = new PushQueue<number>(
      async (n) => {
        await gate;
        sent.push(n);
      },
      (r) => reports.push(r),
    );
    // 0 goes in flight at once; 1..9 wait — one more than the cap.
    for (let n = 0; n <= MAX_BUFFERED + 1; n += 1) q.enqueue(n);
    expect(reports).toContainEqual({ dropped: true });
    release();
    await q.drain();
    expect(sent).toEqual([0, 2, 3, 4, 5, 6, 7, 8, 9]);
  });
});
