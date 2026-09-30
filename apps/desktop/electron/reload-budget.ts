/**
 * Limits how often a crashed renderer is brought back.
 *
 * One reload recovers from a stray out-of-memory kill. A page that dies on
 * every load would otherwise reload forever, so the reloads inside a sliding
 * window are counted and refused past the limit.
 */

export interface ReloadBudgetOptions {
  /** Reloads allowed inside one window. */
  limit?: number;
  /** Length of the sliding window, in milliseconds. */
  windowMs?: number;
  /** Clock, injectable so a test does not have to wait a minute. */
  now?: () => number;
}

export interface ReloadBudget {
  /** Spends one reload. `false` means the budget is used up: do not reload. */
  take(): boolean;
}

export function createReloadBudget({
  limit = 3,
  windowMs = 60_000,
  now = () => Date.now(),
}: ReloadBudgetOptions = {}): ReloadBudget {
  let spent: number[] = [];
  return {
    take() {
      const at = now();
      spent = spent.filter((t) => at - t < windowMs);
      if (spent.length >= limit) return false;
      spent.push(at);
      return true;
    },
  };
}
