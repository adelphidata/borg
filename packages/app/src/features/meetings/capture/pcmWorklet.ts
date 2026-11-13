/**
 * The audio-thread half of meeting capture: an `AudioWorkletProcessor` that
 * posts its input to the main thread in ~100 ms batches.
 *
 * Shipped as source text and loaded from a Blob URL, so neither app's bundler
 * needs a worklet entry point. Excluded from coverage: it only runs inside an
 * AudioWorklet, which the test environment does not have.
 */
export const PCM_WORKLET_NAME = "pcm-chunker";

const SOURCE = `
class PcmChunker extends AudioWorkletProcessor {
  constructor() {
    super();
    this.parts = [];
    this.length = 0;
    this.batch = Math.round(sampleRate / 10);
  }
  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (channel && channel.length > 0) {
      this.parts.push(channel.slice(0));
      this.length += channel.length;
      if (this.length >= this.batch) {
        const out = new Float32Array(this.length);
        let at = 0;
        for (const part of this.parts) {
          out.set(part, at);
          at += part.length;
        }
        this.parts = [];
        this.length = 0;
        this.port.postMessage(out, [out.buffer]);
      }
    }
    return true;
  }
}
registerProcessor(${JSON.stringify(PCM_WORKLET_NAME)}, PcmChunker);
`;

let url: string | undefined;

/** A URL `audioWorklet.addModule` can load the processor from. */
export function pcmWorkletUrl(): string {
  url ??= URL.createObjectURL(new Blob([SOURCE], { type: "text/javascript" }));
  return url;
}
