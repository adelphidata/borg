/**
 * 16-bit PCM WAV, the one audio shape every transcriber Borg drives accepts.
 *
 * Shared by dictation (which re-encodes whatever the browser recorded) and
 * meeting capture (which builds chunks from raw samples).
 */

/** The sample rate speech models expect; whisper.cpp takes nothing else. */
export const WAV_RATE = 16_000;

/** 16-bit PCM WAV with a 44-byte header. Samples outside [-1, 1] are clamped. */
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
