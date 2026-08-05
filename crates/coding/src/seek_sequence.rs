/// Result of searching for a hunk's search lines in file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MatchOutcome {
    /// Exactly one match was found at this line index.
    Unique(usize),
    /// More than one match was found for the same matching strategy.
    Ambiguous {
        /// Number of candidate matches found.
        count: usize,
        /// Matching strategy that produced the ambiguous candidates.
        strategy: &'static str,
        /// Line indices (0-based) of the first candidates, capped at
        /// [`MAX_AMBIGUOUS_CANDIDATES`] so error messages stay bounded.
        positions: Vec<usize>,
    },
    /// No match was found.
    NotFound,
}

/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Matching gets progressively more tolerant, but ambiguity is never resolved silently.
/// When `eof` is true, the search first checks the end-of-file anchor and falls back to the full
/// search range only if no end-of-file match exists.
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> MatchOutcome {
    if pattern.is_empty() {
        return MatchOutcome::Unique(start);
    }

    if pattern.len() > lines.len() {
        return MatchOutcome::NotFound;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    let outcome = seek_sequence_in_range(lines, pattern, search_start, eof);
    if !matches!(outcome, MatchOutcome::NotFound) {
        return outcome;
    }

    if eof && search_start > start {
        return seek_sequence(lines, pattern, start, false);
    }

    MatchOutcome::NotFound
}

fn seek_sequence_in_range(
    lines: &[String],
    pattern: &[String],
    start: usize,
    anchored_to_start: bool,
) -> MatchOutcome {
    type Predicate = fn(&str, &str) -> bool;

    let strategies: [(&str, Predicate); 4] = [
        ("exact", |line, pat| line == pat),
        ("trim_trailing_whitespace", |line, pat| {
            line.trim_end() == pat.trim_end()
        }),
        ("trim_surrounding_whitespace", |line, pat| {
            line.trim() == pat.trim()
        }),
        ("unicode_normalized", |line, pat| {
            normalise(line) == normalise(pat)
        }),
    ];

    for (strategy, predicate) in strategies {
        let matches = collect_matches(lines, pattern, start, anchored_to_start, predicate);
        let outcome = positions_to_outcome(strategy, &matches);
        if !matches!(outcome, MatchOutcome::NotFound) {
            return outcome;
        }
    }

    let indentation_matches = collect_matches(
        lines,
        pattern,
        start,
        anchored_to_start,
        indentation_flexible_match,
    );
    let outcome = positions_to_outcome("indentation_flexible", &indentation_matches);
    if !matches!(outcome, MatchOutcome::NotFound) {
        return outcome;
    }

    let block_anchor_matches =
        collect_block_anchor_matches(lines, pattern, start, anchored_to_start);
    positions_to_outcome("block_anchor", &block_anchor_matches)
}

/// Cap on candidate positions carried by [`MatchOutcome::Ambiguous`] — enough
/// for the model to pick an anchor, bounded so a pathological pattern can't
/// balloon the error message.
pub(crate) const MAX_AMBIGUOUS_CANDIDATES: usize = 5;

fn positions_to_outcome(strategy: &'static str, matches: &[usize]) -> MatchOutcome {
    match matches {
        [] => MatchOutcome::NotFound,
        [idx] => MatchOutcome::Unique(*idx),
        many => MatchOutcome::Ambiguous {
            count: many.len(),
            strategy,
            positions: many
                .iter()
                .copied()
                .take(MAX_AMBIGUOUS_CANDIDATES)
                .collect(),
        },
    }
}

fn collect_matches(
    lines: &[String],
    pattern: &[String],
    start: usize,
    anchored_to_start: bool,
    predicate: fn(&str, &str) -> bool,
) -> Vec<usize> {
    let end = lines.len().saturating_sub(pattern.len());
    let mut matches = Vec::new();
    let positions = start..=end;
    for i in positions {
        if anchored_to_start && i != start {
            break;
        }
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if !predicate(&lines[i + p_idx], pat) {
                ok = false;
                break;
            }
        }
        if ok {
            matches.push(i);
        }
    }
    matches
}

fn collect_block_anchor_matches(
    lines: &[String],
    pattern: &[String],
    start: usize,
    anchored_to_start: bool,
) -> Vec<usize> {
    if pattern.len() < 2 {
        return Vec::new();
    }

    let end = lines.len().saturating_sub(pattern.len());
    let mut matches = Vec::new();
    for i in start..=end {
        if anchored_to_start && i != start {
            break;
        }
        if !boundary_match(&lines[i], &pattern[0])
            || !boundary_match(&lines[i + pattern.len() - 1], &pattern[pattern.len() - 1])
        {
            continue;
        }

        let mut ok = true;
        for middle_idx in 1..pattern.len() - 1 {
            if !middle_block_match(&lines[i + middle_idx], &pattern[middle_idx]) {
                ok = false;
                break;
            }
        }
        if ok {
            matches.push(i);
        }
    }
    matches
}

fn indentation_flexible_match(line: &str, pat: &str) -> bool {
    normalise(line.trim_start()) == normalise(pat.trim_start())
}

fn boundary_match(line: &str, pat: &str) -> bool {
    normalise(line) == normalise(pat)
}

fn middle_block_match(line: &str, pat: &str) -> bool {
    indentation_flexible_match(line, pat)
}

/// Best-effort hint for failed hunks: find the file line that most closely
/// matches the first non-empty line of `pattern`, and return a 3-line snippet
/// of the file centered on it. Returned snippet is `(1-based-line-number, text)`
/// pairs so callers can render `at line N`-style messages without recomputing.
///
/// Returns `None` if `pattern` has no non-empty lines or no file line shares
/// any prefix with the pattern's anchor under unicode-normalized comparison.
/// Pure heuristic; not used during matching itself.
pub(crate) fn find_nearest_context(
    lines: &[String],
    pattern: &[String],
) -> Option<Vec<(usize, String)>> {
    let anchor = pattern.iter().find(|l| !l.trim().is_empty())?;
    let anchor_norm = normalise(anchor);
    if anchor_norm.is_empty() {
        return None;
    }

    let mut best: Option<(usize, usize)> = None; // (score, idx)
    for (idx, line) in lines.iter().enumerate() {
        // Stream normalization on the line rather than allocating a fresh
        // String per candidate — the score only needs char-by-char
        // equality against the (already-normalized) anchor prefix.
        let score = anchor_norm
            .chars()
            .zip(normalise_iter(line))
            .take_while(|(a, b)| a == b)
            .count();
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, idx));
        }
    }

    let (_, center) = best?;
    let start = center.saturating_sub(1);
    let end = (center + 2).min(lines.len());
    Some((start..end).map(|i| (i + 1, lines[i].clone())).collect())
}

fn normalise(s: &str) -> String {
    normalise_iter(s).collect()
}

fn normalise_char(c: char) -> char {
    match c {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    }
}

fn normalise_iter(s: &str) -> impl Iterator<Item = char> + '_ {
    s.trim().chars().map(normalise_char)
}

#[cfg(test)]
mod tests {
    use super::{seek_sequence, MatchOutcome};
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(1)
        );
    }

    #[test]
    fn exact_ambiguity_is_reported() {
        let lines = to_vec(&["foo", "bar", "foo", "bar"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Ambiguous {
                count: 2,
                strategy: "exact",
                positions: vec![0, 2],
            }
        );
    }

    #[test]
    fn rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn indentation_flexible_match_ignores_indent_only() {
        let lines = to_vec(&["if true {", "        inner();", "}"]);
        let pattern = to_vec(&["if true {", "  inner();", "}"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn block_anchor_match_handles_interior_indent_drift() {
        let lines = to_vec(&["fn alpha() {", "        run();", "}"]);
        let pattern = to_vec(&["fn alpha() {", "  run();", "}"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn pattern_longer_than_input_returns_not_found() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::NotFound
        );
    }

    #[test]
    fn empty_pattern_returns_start() {
        let lines = to_vec(&["foo", "bar"]);
        let pattern: Vec<String> = vec![];
        assert_eq!(
            seek_sequence(&lines, &pattern, 1, false),
            MatchOutcome::Unique(1)
        );
    }

    #[test]
    fn eof_mode_finds_at_end() {
        let lines = to_vec(&["a", "b", "c", "d", "e"]);
        let pattern = to_vec(&["d", "e"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, true),
            MatchOutcome::Unique(3)
        );
    }

    #[test]
    fn unicode_normalization_matches_fancy_dashes() {
        let lines = to_vec(&["hello \u{2014} world"]);
        let pattern = to_vec(&["hello - world"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn unicode_normalization_matches_smart_quotes() {
        let lines = to_vec(&["\u{201C}hello\u{201D}"]);
        let pattern = to_vec(&["\"hello\""]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::Unique(0)
        );
    }

    #[test]
    fn start_offset_respected_for_unique_later_match() {
        let lines = to_vec(&["foo", "bar", "foo", "bar"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 1, false),
            MatchOutcome::Unique(2)
        );
    }

    #[test]
    fn no_match_returns_not_found() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["qux"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false),
            MatchOutcome::NotFound
        );
    }
}
