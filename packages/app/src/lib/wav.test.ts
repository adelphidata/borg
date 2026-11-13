import { describe, expect, it } from "vitest";
import { WAV_RATE, encodeWav } from "./wav";

const text = (v: DataView, at: number, n: number) =>
  String.fromCharCode(...Array.from({ length: n }, (_, i) => v.getUint8(at + i)));

describe("encodeWav", () => {
  it("writes a mono 16-bit PCM header a transcriber can read", () => {
    const v = new DataView(encodeWav(new Float32Array(10), WAV_RATE));
    expect(v.byteLength).toBe(44 + 20);
    expect(text(v, 0, 4)).toBe("RIFF");
    expect(v.getUint32(4, true)).toBe(36 + 20);
    expect(text(v, 8, 8)).toBe("WAVEfmt ");
    expect(v.getUint16(20, true)).toBe(1); // PCM
    expect(v.getUint16(22, true)).toBe(1); // mono
    expect(v.getUint32(24, true)).toBe(16_000);
    expect(v.getUint32(28, true)).toBe(32_000); // bytes per second
    expect(v.getUint16(32, true)).toBe(2); // block align
    expect(v.getUint16(34, true)).toBe(16); // bits per sample
    expect(text(v, 36, 4)).toBe("data");
    expect(v.getUint32(40, true)).toBe(20);
  });

  it("clamps samples past full scale instead of wrapping them", () => {
    // A wrapped overload turns a loud word into a click at the opposite rail.
    const v = new DataView(encodeWav(Float32Array.from([2, -2, 0.5, 0]), WAV_RATE));
    expect(v.getInt16(44, true)).toBe(0x7fff);
    expect(v.getInt16(46, true)).toBe(-0x8000);
    expect(v.getInt16(48, true)).toBe(Math.trunc(0.5 * 0x7fff));
    expect(v.getInt16(50, true)).toBe(0);
  });
});
