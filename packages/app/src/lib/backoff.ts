/**
 * Retry timing for streams that reconnect on their own.
 *
 * Two properties matter and neither is obvious:
 *
 * 1. **Jitter, not a fixed interval.** When borgd restarts, every open surface
 *    loses its stream in the same instant. A fixed delay marches them all back
 *    in lockstep and they collide on each round. Full jitter — a uniform pick
 *    from `[0, ceiling)` where the ceiling doubles per failure — spreads the
 *    redials out on the very first round.
 *
 * 2. **Elapsed time, not a failure count.** With jitter, "six attempts" can be
 *    two seconds or two minutes, so it says nothing about how long the user has
 *    been staring at a frozen screen. Escalation is timed from the start of the
 *    outage instead.
 */

/** Ceiling of the first retry window. */
export const FIRST_DELAY_MS = 1_000;
/** The window stops doubling here — a long outage still redials twice a minute. */
export const MAX_DELAY_MS = 30_000;
/**
 * Floor under a jittered pick. Full jitter can legitimately draw ~0ms, and a
 * stream that fails the instant it opens would then spin at full speed.
 */
export const MIN_DELAY_MS = 150;
/**
 * How long an outage runs before the UI stops calling it a blip. Chosen to sit
 * past two or three redials, so a genuinely transient drop never escalates.
 */
export const OUTAGE_ESCALATION_MS = 45_000;

/**
 * Delay before the next attempt. `failures` counts consecutive failures so far
 * (0 for the first retry). `random` is injectable so tests can pin the draw.
 */
export function retryDelayMs(failures: number, random: () => number = Math.random): number {
  // 2 ** 20 already dwarfs the cap; clamping keeps a wedged loop from
  // overflowing the exponent into Infinity.
  const exponent = Math.min(Math.max(Math.floor(failures), 0), 20);
  const ceiling = Math.min(MAX_DELAY_MS, FIRST_DELAY_MS * 2 ** exponent);
  return Math.max(MIN_DELAY_MS, Math.floor(random() * ceiling));
}
