/**
 * How long the turn in flight has been running.
 *
 * The distinction that matters is `start` versus `arm`. A turn the user submits
 * starts its clock at the submit, because the wait for the provider to answer is
 * part of what they sat through and is often most of it. Everything else — a
 * daemon-initiated turn announcing itself, a steer landing between turns — only
 * arms the clock, which is a no-op while one is already running. Without that
 * split a mid-turn steer would silently restart the count.
 */
export interface TurnClock {
  /** Begin timing at `at`, replacing any turn already being timed. */
  start(at: number): void;
  /** Begin timing at `at` only if no turn is being timed. */
  arm(at: number): void;
  /** Finish, returning the elapsed milliseconds, or undefined if nothing ran. */
  stop(at: number): number | undefined;
  /** Drop the turn in flight without reporting a duration. */
  reset(): void;
  /** When the turn in flight started, or undefined between turns. */
  startedAt(): number | undefined;
}

export function createTurnClock(): TurnClock {
  let startedAt: number | undefined;
  return {
    start(at) {
      startedAt = at;
    },
    arm(at) {
      startedAt ??= at;
    },
    stop(at) {
      if (startedAt === undefined) return undefined;
      const elapsed = at - startedAt;
      startedAt = undefined;
      return elapsed;
    },
    reset() {
      startedAt = undefined;
    },
    startedAt() {
      return startedAt;
    },
  };
}
