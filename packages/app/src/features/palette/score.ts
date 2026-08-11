/** One thing the palette can do. */
export interface Command {
  id: string;
  label: string;
  /** Shown right-aligned: the group, or a chord. */
  hint?: string;
  /** Extra words that should match without being displayed. */
  keywords?: string;
  /**
   * The live state the row acts on, in muted text after the label — `on` /
   * `off` for a toggle. The verb alone never says which way the row will go.
   */
  detail?: string;
  /** Renders a trailing check: this row IS the current value. */
  active?: boolean;
  /**
   * Keeps the palette open after running, for rows you pick by seeing the
   * result — theme being the one that matters.
   */
  keepOpen?: boolean;
  /**
   * Sorts before the score: a lower tier always outranks a higher one, however
   * well the higher one matches. This is how a row with a real name stays
   * above a raw subsystem key that happens to contain the same letters —
   * relevance alone would put `tts.timeout_ms` over "TTS provider enabled".
   * Defaults to 0.
   */
  tier?: number;
  run: () => void;
}

/**
 * Subsequence match with a relevance score, or null when it does not match.
 *
 * Subsequence rather than substring so "gse" finds "Go to Settings", which is
 * the whole reason to type into a palette instead of reading a menu. The score
 * rewards matches that start a word and that sit close together, so exact
 * prefixes still beat scattered letters.
 */
export function score(text: string, query: string): number | null {
  if (query === "") return 0;
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();

  let total = 0;
  let at = 0;
  let previous = -1;

  for (const char of needle) {
    const found = haystack.indexOf(char, at);
    if (found === -1) return null;

    // A letter starting a word is what the user most likely meant.
    const startsWord = found === 0 || /[\s\-_/.]/.test(haystack[found - 1] ?? "");
    if (startsWord) total += 8;
    // Consecutive letters read as a real prefix rather than a coincidence.
    if (found === previous + 1) total += 5;
    else total -= Math.min(found - previous, 6);

    previous = found;
    at = found + 1;
  }

  // Shorter labels matching the same query are the more specific answer.
  return total - haystack.length * 0.05;
}

/** How much a keyword-only match gives up to a match on the visible label. */
const KEYWORD_PENALTY = 4;

/** Ranks commands against a query, dropping the ones that do not match. */
export function rank(commands: Command[], query: string): Command[] {
  if (query.trim() === "") return commands;
  const q = query.trim();
  return commands
    .map((command) => {
      // Keywords can match but never out-rank the visible label.
      const direct = score(command.label, q);
      const viaKeywords = command.keywords ? score(command.keywords, q) : null;
      const candidates = [direct, viaKeywords === null ? null : viaKeywords - KEYWORD_PENALTY];
      const hits = candidates.filter((n): n is number => n !== null);
      return hits.length === 0 ? null : { command, best: Math.max(...hits) };
    })
    .filter((r): r is { command: Command; best: number } => r !== null)
    .sort((a, b) => (a.command.tier ?? 0) - (b.command.tier ?? 0) || b.best - a.best)
    .map((r) => r.command);
}
