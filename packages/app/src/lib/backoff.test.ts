import { describe, expect, it } from "vitest";
import {
  FIRST_DELAY_MS,
  MAX_DELAY_MS,
  MIN_DELAY_MS,
  OUTAGE_ESCALATION_MS,
  retryDelayMs,
} from "./backoff";

/** Draws the top of the window, so the ceiling itself is observable. */
const nearlyOne = () => 0.999999;

describe("retryDelayMs", () => {
  it("doubles the window each failure until the cap", () => {
    const ceilings = [0, 1, 2, 3, 4, 5].map((n) => retryDelayMs(n, nearlyOne));
    expect(ceilings[0]).toBeLessThanOrEqual(FIRST_DELAY_MS);
    expect(ceilings[1]).toBeGreaterThan(ceilings[0]!);
    expect(ceilings[2]).toBeGreaterThan(ceilings[1]!);
    // Roughly 2x per step, allowing for the floor of the jitter draw.
    expect(ceilings[3]! / ceilings[2]!).toBeCloseTo(2, 1);
  });

  it("never exceeds the cap, however long the outage runs", () => {
    for (const failures of [6, 10, 50, 1000]) {
      expect(retryDelayMs(failures, nearlyOne)).toBeLessThanOrEqual(MAX_DELAY_MS);
    }
  });

  it("stays finite when the failure count runs away", () => {
    // 2 ** 5000 is Infinity; an unclamped exponent would schedule a timer that
    // never fires, wedging the reconnect loop forever.
    expect(Number.isFinite(retryDelayMs(5000, nearlyOne))).toBe(true);
  });

  it("floors the delay so an instantly-failing stream cannot spin", () => {
    expect(retryDelayMs(0, () => 0)).toBeGreaterThanOrEqual(MIN_DELAY_MS);
    expect(retryDelayMs(9, () => 0)).toBeGreaterThanOrEqual(MIN_DELAY_MS);
  });

  it("spreads simultaneous clients across the window instead of stacking them", () => {
    // The point of jitter: N surfaces that dropped together must not all
    // redial at the same moment. Same failure count, different draws.
    const draws = [0.05, 0.3, 0.55, 0.8].map((r) => retryDelayMs(4, () => r));
    expect(new Set(draws).size).toBe(draws.length);
    expect(Math.max(...draws) - Math.min(...draws)).toBeGreaterThan(1_000);
  });

  it("treats a negative or fractional count as the first attempt", () => {
    expect(retryDelayMs(-3, nearlyOne)).toBeLessThanOrEqual(FIRST_DELAY_MS);
    expect(retryDelayMs(0.7, nearlyOne)).toBeLessThanOrEqual(FIRST_DELAY_MS);
  });
});

describe("escalation threshold", () => {
  it("sits past several redials, so a blip never escalates", () => {
    // Worst case for reaching the user quickly is the largest possible delays;
    // even then the outage must survive more than one retry before the notice
    // changes, or a single slow reconnect would read as an outage.
    expect(OUTAGE_ESCALATION_MS).toBeGreaterThan(MAX_DELAY_MS);
  });
});
