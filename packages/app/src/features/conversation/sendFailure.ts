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
    title: "The model provider is throttling Borg",
    message: "The provider is taking too many requests right now. Wait a moment, then try again.",
    retryable: true,
  },
  {
    layer: "billing",
    pattern:
      /\b402\b|insufficient_quota|insufficient (credit|funds)|out of credit|payment required|billing/,
    title: "The model provider needs paying",
    message:
      "The provider refused the request for billing reasons. Add credit with the provider, or switch Borg to another one in Settings.",
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

/** Sorts a failure into its layer. */
export function classifyFailure(error: { message: string } | undefined): Failure | undefined {
  if (!error) return undefined;
  const haystack = error.message.toLowerCase();
  const rule = RULES.find((r) => r.pattern.test(haystack) && !r.unless?.test(haystack));
  if (!rule) {
    return { layer: "unknown", message: error.message, retryable: true, settings: false };
  }
  return {
    layer: rule.layer,
    title: rule.title,
    message: rule.message,
    retryable: rule.retryable,
    settings: rule.settings ?? false,
  };
}
