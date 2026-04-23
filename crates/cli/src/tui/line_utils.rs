//! Reusable line/span utilities for the TUI.
//!
//! Gathers the prefix/truncation helpers that were previously inlined across
//! `history.rs` and popups.

use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width of the per-cell prefix column (bullet + space). Used everywhere we
/// need to compute usable content width. Replaces inline `width - 2` magic.
pub const LIVE_PREFIX_COLS: usize = 2;

/// Display width of a [`Line`] (sum of its span contents, unicode-width aware).
pub fn line_width(line: &Line<'_>) -> usize {
    line.iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Truncate a styled line so its display width does not exceed `max_width`.
/// Preserves span styles and stops at grapheme-correct char boundaries.
/// Used by `render_boxed_lines` so over-long inputs can't burst the box.
pub fn truncate_line_to_width(line: Line<'static>, max_width: usize) -> Line<'static> {
    if max_width == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }
    let Line {
        style,
        alignment,
        spans,
    } = line;
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());

    for span in spans {
        let span_w = UnicodeWidthStr::width(span.content.as_ref());
        if span_w == 0 {
            out.push(span);
            continue;
        }
        if used >= max_width {
            break;
        }
        if used + span_w <= max_width {
            used += span_w;
            out.push(span);
            continue;
        }
        let style_span = span.style;
        let text = span.content.as_ref();
        let mut end = 0usize;
        for (idx, ch) in text.char_indices() {
            let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_w > max_width {
                break;
            }
            end = idx + ch.len_utf8();
            used += ch_w;
        }
        if end > 0 {
            out.push(Span::styled(text[..end].to_string(), style_span));
        }
        break;
    }

    Line {
        style,
        alignment,
        spans: out,
    }
}

/// Truncate `line` to `max_width` with a trailing `…` when it overflows.
/// Fast path: if the line already fits, return it unchanged.
pub fn truncate_line_with_ellipsis(line: Line<'static>, max_width: usize) -> Line<'static> {
    if max_width == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }
    if line_width(&line) <= max_width {
        return line;
    }
    let truncated = truncate_line_to_width(line, max_width.saturating_sub(1));
    let Line {
        style,
        alignment,
        mut spans,
    } = truncated;
    let ellipsis_style = spans.last().map(|s| s.style).unwrap_or_default();
    spans.push(Span::styled("…", ellipsis_style));
    Line {
        style,
        alignment,
        spans,
    }
}

/// Truncate a plain `&str` to `max_width` display columns (grapheme-safe,
/// width-aware). Returns a borrowed slice pointing at a char boundary.
pub fn truncate_str_to_width(s: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut used = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > max_width {
            break;
        }
        end = idx + ch.len_utf8();
        used += ch_w;
    }
    &s[..end]
}

/// Compact pretty-printed JSON into a single line with spaces after `:` and
/// `,` (outside of string values). Leaves non-JSON input untouched.
///
/// The spaces are kept because `textwrap` only breaks at whitespace; a compact
/// `{"key":"value","next":1}` stream would otherwise become one unwrappable
/// blob and force hard character splitting in narrow viewports.
pub fn format_json_compact(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return None;
    }
    // Only rewrite valid JSON; leave error blobs / truncated output alone.
    let parsed: serde_json::Value = serde_json::from_str(s).ok()?;
    let raw = serde_json::to_string(&parsed).ok()?;
    // `serde_json::to_string` already omits whitespace. Reinsert a single
    // space after `:` and `,` outside of strings so wrap has break points.
    let mut out = String::with_capacity(raw.len() + raw.len() / 16);
    let mut in_string = false;
    let mut escape = false;
    for ch in raw.chars() {
        out.push(ch);
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            ':' | ',' if !in_string => out.push(' '),
            _ => {}
        }
    }
    Some(out)
}

/// Prepend `prefix` to the first line and `subsequent` to the remaining lines.
/// Used by the streaming assistant cell and tool-result cells to give the first
/// row a bullet/glyph and indent continuation rows.
pub fn prefix_lines(
    lines: Vec<Line<'static>>,
    prefix: Span<'static>,
    subsequent: Span<'static>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let leading = if i == 0 {
                prefix.clone()
            } else {
                subsequent.clone()
            };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(leading);
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    #[test]
    fn line_width_sums_unicode_spans() {
        let l = Line::from(vec![Span::raw("ab"), Span::raw("汉字")]);
        assert_eq!(line_width(&l), 2 + 4);
    }

    #[test]
    fn truncate_str_wide_chars_never_splits_glyph() {
        assert_eq!(truncate_str_to_width("汉字abc", 3), "汉");
        assert_eq!(truncate_str_to_width("汉字abc", 4), "汉字");
        assert_eq!(truncate_str_to_width("汉字abc", 5), "汉字a");
    }

    #[test]
    fn truncate_line_with_ellipsis_appends_marker() {
        // Style preserved on the ellipsis so it inherits the colour of the
        // last visible span (matters for truncated coloured tool output).
        let red = ratatui::style::Style::default().fg(ratatui::style::Color::Red);
        let l = Line::from(vec![Span::raw("ok "), Span::styled("ERROR MESSAGE", red)]);
        let out = truncate_line_with_ellipsis(l, 6);
        assert!(out.spans.last().unwrap().content.contains('…'));
        assert_eq!(out.spans.last().unwrap().style, red);
        assert_eq!(line_width(&out), 6);
    }

    #[test]
    fn format_json_compact_inserts_break_points() {
        let pretty = r#"{
  "name": "foo",
  "nested": { "x": 1, "y": [1, 2] }
}"#;
        let out = format_json_compact(pretty).unwrap();
        assert!(out.contains(": "));
        assert!(out.contains(", "));
        // Still single-line
        assert!(!out.contains('\n'));
        // Strings with embedded colons are not mangled.
        let s = r#"{"url": "https://example.com:8080/path"}"#;
        let out2 = format_json_compact(s).unwrap();
        assert!(out2.contains("https://example.com:8080/path"));
    }

    #[test]
    fn format_json_compact_returns_none_for_non_json() {
        assert!(format_json_compact("hello world").is_none());
        assert!(format_json_compact("error: something broke").is_none());
    }

    #[test]
    fn prefix_lines_applies_first_and_subsequent() {
        let lines = vec![
            Line::from(vec![Span::raw("a")]),
            Line::from(vec![Span::raw("b")]),
            Line::from(vec![Span::raw("c")]),
        ];
        let out = prefix_lines(lines, Span::raw("└ "), Span::raw("  "));
        assert_eq!(out[0].spans[0].content.as_ref(), "└ ");
        assert_eq!(out[1].spans[0].content.as_ref(), "  ");
        assert_eq!(out[2].spans[0].content.as_ref(), "  ");
    }
}
