/**
 * Turns a failed turn into a card that names what broke and what to do next.
 *
 * A raw daemon error is written for a log, not for the person who just lost a
 * message: `SQLITE_FULL: database or disk is full` says nothing about what to
 * do, and a full disk is the one failure a user can actually fix. Worse, every
 * failure looks alike — a rate limit, a dead key, and a dropped socket all
 * arrive as one red line of jargon, so the user's only move is to try again and
 * hope.
 *
 * So each failure is sorted into the layer that broke, and the layer decides
 * which actions are worth offering. Retry leads only where repeating the same
 * request could plausibly work; a dead key or a full disk gets the fix instead,
 * because a Retry that is certain to fail is a button that teaches the user the
 * app is lying. Anything unrecognised keeps its original text and stays
 * retryable — a vague guess would be worse than the real error, which at least
 * a maintainer can read.
 *
 * Two rules keep the guessing honest, and both were learned by getting them
 * wrong. Status codes match on word boundaries, because `402` also appears in
 * `session 402a9f` and a request id must not be read as a bill. And the
 * patterns are matched against *every* error the transcript shows — a sandbox
 * denial and a tool's failed write arrive here too, not only provider replies —
 * so a marker broad enough to catch a filesystem miss ("does not exist") would
 * tell the user their model was unavailable.
 */

/** Which part of the stack failed, in the words the card puts on screen. */
export type FailureLayer =
  "disk" | "permissions" | "rate-limit" | "billing" | "auth" | "model" | "connection" | "unknown";

export interface Failure {
  layer: FailureLayer;
  /** Names the failing layer. Omitted for `unknown` — a title that says
   *  "Something went wrong" is a line of screen spent on nothing. */
  title?: string;
  /** What happened, ending in the next action. */
  message: string;
  /** Whether repeating the same request could plausibly succeed. */
  retryable: boolean;
  /** True when the fix is a setting, so the card can offer the way there. */
  settings: boolean;
}

interface Rule {
  layer: FailureLayer;
  /** Matched against the lowercased error text. */
  pattern: RegExp;
  /**
   * Vetoes a match. For words that mean one thing from the network and another
   * from a local process — "timed out" is both a dead socket and a shell
   * command that ran too long — this is what keeps the broad half honest.
   */
  unless?: RegExp;
  title: string;
  message: string;
  retryable: boolean;
  settings?: boolean;
}

const RATE_LIMIT_TITLE = "The model provider is throttling Borg";
const RATE_LIMIT_WHAT = "The provider is taking too many requests right now.";
const RATE_LIMIT_MESSAGE = `${RATE_LIMIT_WHAT} Wait a moment, then try again.`;
const BILLING_TITLE = "The model provider needs paying";
const BILLING_MESSAGE =
  "The provider refused the request for billing reasons. Add credit with the provider, or switch Borg to another one in Settings.";

/**
 * First match wins, so the order is the priority.
 *
 * Local failures lead: a full disk or a denied path is the machine's problem
 * whatever the message also mentions. Rate limiting beats billing because a
 * provider's ordinary 429 body routinely says "check your plan and billing
 * details" — read the other way round, the single most common transient
 * failure would be branded unpayable and its retry withheld.
 */
const RULES: readonly Rule[] = [
  // The daemon's own diagnosis, which it writes in brackets after the status.
  // These lead because the daemon read the response body and the bare status
  // codes below did not: a 403 from the provider's edge is not a bad key, and a
  // 429 for a spent balance is not a throttle.
  {
    layer: "connection",
    pattern: /\(request blocked by cloudflare/,
    title: "The provider blocked this network",
    message:
      "Your network or region was blocked by the provider. Try another network, or switch provider in Settings.",
    retryable: false,
    settings: true,
  },
  {
    layer: "billing",
    pattern: /\(billing error/,
    title: BILLING_TITLE,
    message: BILLING_MESSAGE,
    retryable: false,
    settings: true,
  },
  {
    // A usage window that reopens on its own, which some providers report
    // with the status others keep for an unpaid bill.
    layer: "rate-limit",
    pattern: /\(usage limit\)/,
    title: RATE_LIMIT_TITLE,
    message: RATE_LIMIT_MESSAGE,
    retryable: true,
  },
  {
    // libsql, the OS, and the filesystem each phrase this differently, and the
    // message reaches us as whichever layer noticed first.
    layer: "disk",
    pattern: /enospc|sqlite_full|disk is full|disk full|no space left/,
    title: "The disk is full",
    message: "Borg could not write to disk — the disk is full. Free up space, then try again.",
    retryable: false,
  },
  {
    // Covers a sandbox denial and a tool's failed write as well as Borg's own
    // data folder: all three are a path this machine will not let Borg touch,
    // and none of them is fixed anywhere in Settings.
    layer: "permissions",
    pattern:
      /read-only file system|erofs|attempt to write a readonly database|eacces|permission denied/,
    title: "Borg was refused access to that path",
    message:
      "The operating system refused the write. Check the permissions on the path — Borg's own data lives in ~/.borg — then try again.",
    retryable: false,
  },
  {
    layer: "rate-limit",
    pattern: /\b429\b|rate[ _-]?limit|too many requests|overloaded/,
    title: RATE_LIMIT_TITLE,
    message: RATE_LIMIT_MESSAGE,
    retryable: true,
  },
  {
    layer: "billing",
    pattern:
      /\b402\b|insufficient_quota|insufficient (credit|funds)|out of credit|payment required|billing/,
    title: BILLING_TITLE,
    message: BILLING_MESSAGE,
    retryable: false,
    settings: true,
  },
  {
    layer: "auth",
    pattern:
      /\b40[13]\b|unauthorized|unauthenticated|invalid[ _]api[ _]key|authentication failed|api key/,
    title: "The model provider rejected the key",
    message:
      "Borg's key for this provider was refused. Set a working key in Settings, then try again.",
    retryable: false,
    settings: true,
  },
  {
    // Anchored on the word "model": a bare "does not exist" is far more often a
    // missing file or a dropped database row than a retired model name. The
    // span is bounded rather than dot-free, because the name in between is
    // routinely `gemini-2.5-pro`.
    layer: "model",
    pattern: /model[ _]not[ _]found|unknown model|model.{0,40}?(does not exist|not found)/,
    title: "That model is not available",
    message:
      "The provider does not offer the model Borg asked for. Pick another model, then try again.",
    retryable: false,
    settings: true,
  },
  {
    layer: "connection",
    pattern:
      /econnrefused|econnreset|connection (reset|refused)|transport error|unavailable|network error|socket hang up|stream ended|deadline exceeded|timed out|timeout|etimedout|enotfound|\bdns\b/,
    // A shell command that overran, a stalled sub-agent, and a sandbox that
    // never started all say "timed out" too, and telling that user to check
    // their network is confidently wrong.
    unless: /run_shell|sub-?agent|sandbox|command timed out|tool (call|failed)/,
    title: "Borg could not reach the model provider",
    message: "The connection dropped before the reply arrived. Check the network, then try again.",
    retryable: true,
  },
];

/** "try again in 5 minutes", "retry after 45 seconds", "try again in 1.234s". */
const RETRY_HINT =
  /(?:try again in|retry after|retry in)\s+(?:about\s+)?(\d+(?:\.\d+)?)\s*(seconds?|secs?|s|minutes?|mins?|m)\b/;

/**
 * How long the provider said to wait, in the card's words.
 *
 * "Wait a moment" is a guess; when the provider named the wait, the guess is
 * the worse of the two — a user told "a moment" retries a five-minute window
 * ten times.
 */
export function retryHint(raw: string): string | undefined {
  const match = RETRY_HINT.exec(raw.toLowerCase());
  if (!match) return undefined;
  // Rounded up: retrying a fraction early only earns another refusal.
  const amount = Math.ceil(Number(match[1]));
  if (amount <= 0) return undefined;
  if (match[2]!.startsWith("m")) return `Try again in about ${amount} min.`;
  // Nobody counts to ninety; past that the minute is the unit people use.
  if (amount >= 90) return `Try again in about ${Math.round(amount / 60)} min.`;
  return `Try again in about ${amount} ${amount === 1 ? "second" : "seconds"}.`;
}

/** Sorts a failure into its layer. */
export function classifyFailure(error: { message: string } | undefined): Failure | undefined {
  if (!error) return undefined;
  const haystack = error.message.toLowerCase();
  const rule = RULES.find((r) => r.pattern.test(haystack) && !r.unless?.test(haystack));
  if (!rule) {
    return { layer: "unknown", message: error.message, retryable: true, settings: false };
  }
  const hint = rule.layer === "rate-limit" ? retryHint(error.message) : undefined;
  return {
    layer: rule.layer,
    title: rule.title,
    message: hint ? `${RATE_LIMIT_WHAT} ${hint}` : rule.message,
    retryable: rule.retryable,
    settings: rule.settings ?? false,
  };
}
