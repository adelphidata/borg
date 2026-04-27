/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Returns the starting index of the match or `None` if not found. Matches are attempted with
/// decreasing strictness: exact match, then ignoring trailing whitespace, then ignoring leading
/// and trailing whitespace, then Unicode normalization. When `eof` is true, we first try starting
/// at the end-of-file (so that patterns intended to match file endings are applied at the end),
/// and fall back to searching from `start` if needed.
///
/// Special cases handled defensively:
///  - Empty `pattern` -> returns `Some(start)` (no-op match)
///  - `pattern.len() > lines.len()` -> returns `None` (cannot match)
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }

    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    // Exact match first.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }

    // Then rstrip match (trailing whitespace).
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // Trim both sides.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // Unicode normalization pass — maps typographic punctuation to ASCII equivalents.
    fn normalise(s: &str) -> String {
        s.trim()
            .chars()
            .map(|c| match c {
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
                | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
                | '\u{3000}' => ' ',
                other => other,
            })
            .collect::<String>()
    }

    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if normalise(&lines[i + p_idx]) != normalise(pat) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // If EOF mode tried from end and failed, retry from `start`.
    if eof && search_start > start {
        return seek_sequence(lines, pattern, start, false);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::seek_sequence;
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }

    #[test]
    fn rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), None);
    }

    #[test]
    fn empty_pattern_returns_start() {
        let lines = to_vec(&["foo", "bar"]);
        let pattern: Vec<String> = vec![];
        assert_eq!(seek_sequence(&lines, &pattern, 1, false), Some(1));
    }

    #[test]
    fn eof_mode_finds_at_end() {
        let lines = to_vec(&["a", "b", "c", "d", "e"]);
        let pattern = to_vec(&["d", "e"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, true), Some(3));
    }

    #[test]
    fn unicode_normalization_matches_fancy_dashes() {
        let lines = to_vec(&["hello \u{2014} world"]); // em dash
        let pattern = to_vec(&["hello - world"]); // ASCII dash
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn unicode_normalization_matches_smart_quotes() {
        let lines = to_vec(&["\u{201C}hello\u{201D}"]); // smart double quotes
        let pattern = to_vec(&["\"hello\""]); // ASCII quotes
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn start_offset_respected() {
        let lines = to_vec(&["foo", "bar", "foo", "bar"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(seek_sequence(&lines, &pattern, 1, false), Some(2));
    }

    #[test]
    fn no_match_returns_none() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["qux"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), None);
    }
}
