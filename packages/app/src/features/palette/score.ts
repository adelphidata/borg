/** One thing the palette can do. */
export interface Command {
  id: string;
  label: string;
  /** Shown right-aligned: the group, or a chord. */
  hint?: string;
  /** Extra words that should match without being displayed. */
  keywords?: string;
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

/** Ranks commands against a query, dropping the ones that do not match. */
export function rank(commands: Command[], query: string): Command[] {
  if (query.trim() === "") return commands;
  const q = query.trim();
  return commands
    .map((command) => {
      // Keywords can match but never out-rank the visible label.
      const direct = score(command.label, q);
      const viaKeywords = command.keywords ? score(command.keywords, q) : null;
      const best =
        direct !== null && viaKeywords !== null
          ? Math.max(direct, viaKeywords - 4)
          : (direct ?? (viaKeywords === null ? null : viaKeywords - 4));
      return best === null ? null : { command, best };
    })
    .filter((r): r is { command: Command; best: number } => r !== null)
    .sort((a, b) => b.best - a.best)
    .map((r) => r.command);
}
