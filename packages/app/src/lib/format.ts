/**
 * Number and unit formatting shared across features.
 *
 * These live together because the same value shows up on several surfaces —
 * a token count appears in the transcript footer, the agent panel, and the
 * usage page — and three private copies of `Intl.NumberFormat` is how "1.2K"
 * on one screen becomes "1,234" on the next. Formatters are module-level
 * because constructing an `Intl` formatter is the expensive part, not using
 * one.
 */

const COMPACT = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});

const MONEY = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 2,
});

/** Sub-cent spend rounds to $0.00, which reads as "nothing happened". */
const SMALL_MONEY = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 4,
});

/** Token counts, shortened: 1234 → "1.2K". */
export function formatTokens(tokens: number): string {
  return COMPACT.format(tokens);
}

export function formatCost(costUsd: number): string {
  if (costUsd === 0) return MONEY.format(0);
  return costUsd < 0.01 ? SMALL_MONEY.format(costUsd) : MONEY.format(costUsd);
}

/** Upper-cases the first character only; the rest of the string is untouched. */
export function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** "1 warning" / "2 warnings", with the -es plural English needs for sibilants. */
export function plural(n: number, word: string): string {
  if (n === 1) return `1 ${word}`;
  return `${n} ${word.endsWith("s") ? `${word}es` : `${word}s`}`;
}

/**
 * A short elapsed time for a footer: "0.8s", "12.4s", "2m 05s", "1h 04m".
 *
 * Tighter than `formatDuration` on purpose — this one sits next to a token
 * count on one line, where "45 seconds" would be the longest thing on it.
 */
export function formatElapsed(ms: number): string {
  const exact = Math.max(0, ms) / 1000;
  // Rounded before the branch, not inside it: 59.99s formats to one decimal as
  // "60.0s", which is a minute spelled the way the sub-minute case spells it.
  if (Math.round(exact * 10) < 600) return `${exact.toFixed(1)}s`;
  // Round to whole seconds first, then split: rounding each part on its own
  // lets 59.7s become "59m 60s".
  const secs = Math.round(exact);
  if (secs < 3_600) return `${Math.floor(secs / 60)}m ${String(secs % 60).padStart(2, "0")}s`;
  const mins = Math.round((secs % 3_600) / 60);
  if (mins === 60) return `${Math.floor(secs / 3_600) + 1}h 00m`;
  return `${Math.floor(secs / 3_600)}h ${String(mins).padStart(2, "0")}m`;
}

/** A rounded, human duration: "2 hours", "45 seconds". */
export function formatDuration(secs: number): string {
  if (secs >= 86_400) return plural(Math.floor(secs / 86_400), "day");
  if (secs >= 3_600) return plural(Math.floor(secs / 3_600), "hour");
  if (secs >= 60) return plural(Math.floor(secs / 60), "minute");
  return plural(Math.max(0, Math.floor(secs)), "second");
}
