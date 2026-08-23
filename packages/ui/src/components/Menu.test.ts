import { describe, expect, it } from "vitest";
import { clampToViewport } from "./Menu";

const VIEWPORT = { width: 1000, height: 800 };

describe("clampToViewport", () => {
  it("opens where the pointer is when there is room", () => {
    expect(clampToViewport({ x: 300, y: 200 }, 3, VIEWPORT)).toEqual({ left: 300, top: 200 });
  });

  it("pulls a menu opened near the right edge back on screen", () => {
    const { left } = clampToViewport({ x: 980, y: 200 }, 3, VIEWPORT);
    expect(left + 150).toBeLessThanOrEqual(VIEWPORT.width);
  });

  it("pulls a menu opened near the bottom edge back on screen", () => {
    const { top } = clampToViewport({ x: 300, y: 790 }, 3, VIEWPORT);
    expect(top).toBeLessThan(790);
    expect(top).toBeGreaterThan(0);
  });

  it("lifts a taller menu further, so its last item stays reachable", () => {
    const short = clampToViewport({ x: 300, y: 790 }, 1, VIEWPORT);
    const tall = clampToViewport({ x: 300, y: 790 }, 6, VIEWPORT);
    expect(tall.top).toBeLessThan(short.top);
  });

  it("clamps with the estimated height: items plus a gap at each edge", () => {
    // 3 items * 28px + 2 * 8px gap = 100px tall; 150px wide.
    // left: 1000 - 150 - 8 = 842; top: 800 - 100 - 8 = 692.
    expect(clampToViewport({ x: 980, y: 790 }, 3, VIEWPORT)).toEqual({ left: 842, top: 692 });
  });

  it("never leaves the menu off the top or left, even in a tiny window", () => {
    const tiny = { width: 120, height: 60 };
    const { left, top } = clampToViewport({ x: 100, y: 50 }, 6, tiny);
    expect(left).toBeGreaterThanOrEqual(0);
    expect(top).toBeGreaterThanOrEqual(0);
  });
});
