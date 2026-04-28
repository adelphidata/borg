//! Renders the Borg patch DSL as a colored unified-diff view.
//!
//! The patch DSL uses line prefixes: `+` add, `-` remove, ` ` context, with
//! file headers `*** Add/Update/Delete File:` and optional `@@` hunk markers.
//! This parser walks the raw patch text and emits [`Line`]s for display in
//! the transcript. It never touches the filesystem — it is a pure TUI-side
//! read of data we already have in the tool call args.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::highlight;
use super::theme;

/// Tab stops are rendered as 4 columns wide (matches codex's diff view).
const TAB_WIDTH: usize = 4;
/// Continuation rows from line wrapping are indented by this many spaces so
/// the wrap visually belongs to the row above.
const WRAP_CONTINUATION_INDENT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Add,
    Remove,
    Context,
}

struct FileHeader {
    label: &'static str,
    path: String,
}

fn parse_file_header(trimmed: &str) -> Option<FileHeader> {
    for (prefix, label) in [
        ("*** Add File: ", "Add"),
        ("*** Update File: ", "Update"),
        ("*** Delete File: ", "Delete"),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let path = rest.trim().to_string();
            if !path.is_empty() {
                return Some(FileHeader { label, path });
            }
        }
    }
    None
}

fn path_ext(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, ext)| ext.to_lowercase())
}

fn render_content_line(
    raw: &str,
    kind: DiffKind,
    ext: Option<&str>,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
) -> Line<'static> {
    // Strip the leading prefix char (+, -, space); empty lines keep a marker for alignment.
    let (marker, body) = match kind {
        DiffKind::Add => ('+', raw.get(1..).unwrap_or("")),
        DiffKind::Remove => ('-', raw.get(1..).unwrap_or("")),
        DiffKind::Context => (' ', raw.get(1..).unwrap_or("")),
    };
    let tint = match kind {
        DiffKind::Add => theme::diff_add_style(),
        DiffKind::Remove => theme::diff_remove_style(),
        DiffKind::Context => theme::dim(),
    };
    let row_bg = match kind {
        DiffKind::Add => Some(theme::diff_add_bg()),
        DiffKind::Remove => Some(theme::diff_remove_bg()),
        DiffKind::Context => None,
    };
    let gutter_bg = match kind {
        DiffKind::Add => Some(theme::diff_add_gutter_bg()),
        DiffKind::Remove => Some(theme::diff_remove_gutter_bg()),
        DiffKind::Context => None,
    };

    // 4-col line-number gutter for old + new, then 2-col sign column.
    let fmt_lineno = |n: Option<u32>| -> String {
        match n {
            Some(v) => format!("{v:>4}"),
            None => "    ".to_string(),
        }
    };
    let mut gutter_style = ratatui::style::Style::default().fg(theme::diff_gutter_fg());
    if let Some(bg) = gutter_bg {
        gutter_style = gutter_style.bg(bg);
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(fmt_lineno(old_lineno), gutter_style));
    spans.push(Span::styled(" ", gutter_style));
    spans.push(Span::styled(fmt_lineno(new_lineno), gutter_style));
    spans.push(Span::styled(format!(" {marker} "), gutter_style));

    // Syntax-highlight all three kinds when we know the file extension.
    // Add lines keep syntect's colors as-is — the green row bg + `+` sign
    // already signal insertion. Remove lines keep syntect's colors but
    // overlay Modifier::DIM so the deletion cue stays readable against the
    // red row bg without the syntax color overpowering it.
    match (kind, ext) {
        (_, Some(lang)) if !body.is_empty() => {
            let mut hl = highlight::highlight_inline(body, lang);
            if matches!(kind, DiffKind::Remove) {
                for span in hl.iter_mut() {
                    span.style = span.style.add_modifier(Modifier::DIM);
                }
            }
            spans.extend(hl);
        }
        _ => spans.push(Span::styled(body.to_string(), tint)),
    }
    let mut line = Line::from(spans);
    if let Some(bg) = row_bg {
        line = line.style(ratatui::style::Style::default().bg(bg));
    }
    line
}

/// Parse a `@@ -old_start,old_count +new_start,new_count @@` hunk header.
/// Returns `(old_start, new_start)`. Missing fields default to 1.
fn parse_hunk_header(raw: &str) -> Option<(u32, u32)> {
    let s = raw.trim();
    if !s.starts_with("@@") {
        return None;
    }
    let mut old_start = 1u32;
    let mut new_start = 1u32;
    for tok in s.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            let head = rest.split(',').next().unwrap_or(rest);
            if let Ok(v) = head.parse::<u32>() {
                old_start = v;
            }
        } else if let Some(rest) = tok.strip_prefix('+') {
            let head = rest.split(',').next().unwrap_or(rest);
            if let Ok(v) = head.parse::<u32>() {
                new_start = v;
            }
        }
    }
    Some((old_start, new_start))
}

/// Render a Borg patch DSL string as a vector of styled [`Line`]s.
///
/// The returned lines include file headers (cyan bold), `@@` hunk markers
/// (dim gray), and content lines tinted by kind. Context lines pick up
/// syntax highlighting based on the current file's extension.
///
/// Never panics: malformed patches fall through as dim raw lines and log a
/// warning; a completely empty patch produces a single "(empty patch)" line.
pub fn render_patch(patch: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_ext: Option<String> = None;
    let mut in_file = false;
    let mut recognized_anything = false;
    // Line-number tracking: we increment per row inside a hunk.
    let mut old_lineno: u32 = 1;
    let mut new_lineno: u32 = 1;

    for raw in patch.lines() {
        let trimmed = raw.trim();

        if trimmed == "*** Begin Patch" || trimmed == "*** End Patch" {
            recognized_anything = true;
            continue;
        }

        if let Some(header) = parse_file_header(trimmed) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            current_ext = path_ext(&header.path);
            in_file = true;
            // Reset line counters at each file boundary; without an explicit
            // `@@` hunk header (e.g. plain Add File: bodies), this means the
            // first body line is "1".
            old_lineno = 1;
            new_lineno = 1;
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", header.label), theme::diff_header_style()),
                Span::styled(header.path, theme::diff_header_style()),
            ]));
            recognized_anything = true;
            continue;
        }

        if trimmed.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_lineno = o;
                new_lineno = n;
            }
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                theme::diff_hunk_style(),
            )));
            recognized_anything = true;
            continue;
        }

        // Blank or whitespace-only line → blank spacer, don't count as recognized DSL.
        if trimmed.is_empty() {
            lines.push(Line::default());
            continue;
        }

        // Content lines require a file context; outside one, we can't classify
        // a leading space as "context" (would swallow arbitrary prose).
        if !in_file {
            lines.push(Line::from(Span::styled(raw.to_string(), theme::dim())));
            continue;
        }

        let kind = match raw.chars().next() {
            Some('+') => Some(DiffKind::Add),
            Some('-') => Some(DiffKind::Remove),
            Some(' ') => Some(DiffKind::Context),
            _ => None,
        };

        match kind {
            Some(k) => {
                let (o, n) = match k {
                    DiffKind::Add => (None, Some(new_lineno)),
                    DiffKind::Remove => (Some(old_lineno), None),
                    DiffKind::Context => (Some(old_lineno), Some(new_lineno)),
                };
                lines.push(render_content_line(raw, k, current_ext.as_deref(), o, n));
                match k {
                    DiffKind::Add => new_lineno += 1,
                    DiffKind::Remove => old_lineno += 1,
                    DiffKind::Context => {
                        old_lineno += 1;
                        new_lineno += 1;
                    }
                }
                recognized_anything = true;
            }
            None => {
                lines.push(Line::from(Span::styled(raw.to_string(), theme::dim())));
            }
        }
    }

    if !recognized_anything {
        tracing::warn!(bytes = patch.len(), "render_patch: no recognizable DSL");
        return vec![Line::from(Span::styled(
            "(empty patch)".to_string(),
            theme::dim(),
        ))];
    }

    lines
}

/// Render a patch and then wrap any over-width logical line into multiple
/// physical rows. Continuation rows inherit the source line's row background
/// (so a wrapped insertion keeps its green column) and are prepended with a
/// 2-space indent to visually attach them to the row above.
pub fn render_patch_wrapped(patch: &str, width: usize) -> Vec<Line<'static>> {
    wrap_lines(render_patch(patch), width)
}

/// Width of `c` in terminal columns: tab counts as 4, control/zero-width as 0.
fn char_width(c: char) -> usize {
    if c == '\t' {
        TAB_WIDTH
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

/// Total display width of a styled line's spans. Counts tabs as 4 cols.
fn line_display_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| s.content.chars().map(char_width).sum::<usize>())
        .sum()
}

/// Split a single styled line into rows ≤ `width` columns. The first row
/// uses the full width; subsequent rows use `width - WRAP_CONTINUATION_INDENT`
/// and get a 2-space indent prepended (carrying the row's background).
fn wrap_one(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line_display_width(&line) <= width {
        return vec![line];
    }
    let row_style = line.style;
    let cont_width = width.saturating_sub(WRAP_CONTINUATION_INDENT).max(1);

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut col = 0usize;
    let mut limit = width;

    for span in line.spans.into_iter() {
        let style = span.style;
        let mut buf = String::new();
        for c in span.content.chars() {
            let w = char_width(c);
            // A zero-width char (combining mark) attaches to current row.
            if w == 0 {
                buf.push(c);
                continue;
            }
            if col + w > limit && !buf.is_empty() {
                // Flush current span fragment, then break to next row.
                if let Some(last) = rows.last_mut() {
                    last.push(Span::styled(std::mem::take(&mut buf), style));
                }
            }
            if col + w > limit {
                rows.push(Vec::new());
                col = 0;
                limit = cont_width;
            }
            buf.push(c);
            col += w;
        }
        if !buf.is_empty() {
            if let Some(last) = rows.last_mut() {
                last.push(Span::styled(buf, style));
            }
        }
    }

    let indent_style = ratatui::style::Style::default();
    rows.into_iter()
        .enumerate()
        .map(|(i, mut spans)| {
            if i > 0 {
                let pad = " ".repeat(WRAP_CONTINUATION_INDENT);
                spans.insert(0, Span::styled(pad, indent_style));
            }
            let mut l = Line::from(spans);
            l.style = row_style;
            l
        })
        .collect()
}

/// Wrap each over-width line; pass short lines through unchanged.
pub fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.extend(wrap_one(line, width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// Skip the 4 gutter spans (old lineno, separator, new lineno, sign) and
    /// return the remaining content spans. Useful for asserting on body
    /// styling without coupling tests to gutter formatting.
    fn content_spans<'a>(line: &'a Line<'a>) -> &'a [Span<'a>] {
        if line.spans.len() > 4 {
            &line.spans[4..]
        } else {
            &[]
        }
    }

    fn all_text(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn renders_simple_add_file() {
        let patch = "*** Begin Patch\n*** Add File: foo.rs\n+fn main() {}\n*** End Patch";
        let lines = render_patch(patch);
        // 1 header + 1 body
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("Add:"));
        assert!(line_text(&lines[0]).contains("foo.rs"));
        assert!(line_text(&lines[1]).contains("fn main()"));
        // The body row carries the add-row background (the visual diff cue).
        assert_eq!(lines[1].style.bg, Some(theme::diff_add_bg()));
        // Sign column shows `+`.
        assert!(line_text(&lines[1]).contains(" + "));
    }

    #[test]
    fn renders_update_with_hunk() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n context line\n-old\n+new\n*** End Patch";
        let lines = render_patch(patch);
        let text = all_text(&lines);
        assert!(text.contains("Update:"));
        assert!(text.contains("@@"));
        assert!(text.contains("context line"));
        assert!(text.contains("old"));
        assert!(text.contains("new"));

        // Find the +new and -old lines; verify row backgrounds.
        let plus_line = lines
            .iter()
            .find(|l| line_text(l).contains("new"))
            .expect("plus line present");
        assert_eq!(plus_line.style.bg, Some(theme::diff_add_bg()));
        let minus_line = lines
            .iter()
            .find(|l| line_text(l).contains("old"))
            .expect("minus line present");
        assert_eq!(minus_line.style.bg, Some(theme::diff_remove_bg()));
    }

    #[test]
    fn renders_delete_file_header() {
        let patch = "*** Begin Patch\n*** Delete File: old.rs\n*** End Patch";
        let lines = render_patch(patch);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.contains("Delete:"));
        assert!(text.contains("old.rs"));
    }

    #[test]
    fn handles_malformed_patch_gracefully() {
        // Missing "*** End Patch" but otherwise valid — must render what's there.
        let patch = "*** Begin Patch\n*** Add File: x.rs\n+hello\n";
        let lines = render_patch(patch);
        assert!(
            lines.len() >= 2,
            "should render header + body, got {}",
            lines.len()
        );
        assert!(line_text(&lines[0]).contains("x.rs"));
        let body = lines
            .iter()
            .find(|l| line_text(l).contains("hello"))
            .unwrap();
        assert_eq!(body.style.bg, Some(theme::diff_add_bg()));
    }

    #[test]
    fn pure_garbage_returns_placeholder() {
        // No DSL markers at all — placeholder keeps the UI honest.
        let lines = render_patch("some garbage\nmore garbage");
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("empty patch"));
    }

    #[test]
    fn content_without_file_header_is_dimmed() {
        // Patch text with no file header; lines must not be classified as diff body.
        let patch = "*** Begin Patch\n-ghost removal\n*** End Patch";
        let lines = render_patch(patch);
        // The `-ghost removal` line should be dim, not red. (Bypasses gutter,
        // so the only span is a single dim Span.)
        let body = &lines[0];
        for span in &body.spans {
            assert_ne!(span.style, theme::diff_remove_style());
        }
    }

    #[test]
    fn empty_patch_returns_placeholder() {
        let lines = render_patch("");
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("empty patch"));
    }

    #[test]
    fn whitespace_only_patch_returns_placeholder() {
        let lines = render_patch("   \n\n   ");
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("empty patch"));
    }

    #[test]
    fn multi_file_patch_has_separator() {
        let patch = "*** Begin Patch\n\
            *** Add File: a.rs\n\
            +let a = 1;\n\
            *** Add File: b.rs\n\
            +let b = 2;\n\
            *** End Patch";
        let lines = render_patch(patch);
        // Expect: header-a, body-a, blank, header-b, body-b  (5 lines)
        assert_eq!(lines.len(), 5);
        assert!(line_text(&lines[0]).contains("a.rs"));
        assert!(lines[2].spans.is_empty()); // blank separator
        assert!(line_text(&lines[3]).contains("b.rs"));
    }

    #[test]
    fn hunk_marker_styled_with_hunk_style() {
        let patch =
            "*** Begin Patch\n*** Update File: x.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n*** End Patch";
        let lines = render_patch(patch);
        let hunk = lines
            .iter()
            .find(|l| line_text(l).starts_with("@@"))
            .expect("hunk line present");
        assert_eq!(hunk.spans[0].style, theme::diff_hunk_style());
    }

    #[test]
    fn renders_line_number_gutter() {
        // Each content line carries an old + new line-number gutter and a
        // sign column. Hunk header at line 5 means the first body line is 5.
        let patch =
            "*** Begin Patch\n*** Update File: a.rs\n@@ -5,2 +5,2 @@\n ctx\n-old\n+new\n*** End Patch";
        let lines = render_patch(patch);
        // Find the +new line; its leading text should include "5" twice (old=5 because
        // ctx advanced both, then `-old` advanced old to 6; +new keeps old col blank).
        let plus = lines
            .iter()
            .find(|l| line_text(l).contains("new"))
            .expect("plus line present");
        let leading = line_text(plus);
        // Leading gutter is "    " (blank old) + " " + "   6" (new lineno) + " + ".
        assert!(
            leading.contains("   6 + "),
            "expected gutter with new lineno 6, got: {leading:?}"
        );
        // Context line should have both linenos.
        let ctx = lines
            .iter()
            .find(|l| line_text(l).contains("ctx"))
            .expect("context line present");
        let ctx_leading = line_text(ctx);
        assert!(
            ctx_leading.starts_with("   5    5"),
            "expected matched linenos, got: {ctx_leading:?}"
        );
    }

    #[test]
    fn unknown_prefix_is_dimmed_not_panic() {
        // A content line with no valid prefix — parser keeps it as a dim line.
        let patch = "*** Begin Patch\n*** Update File: x.rs\nOHNO no prefix here\n*** End Patch";
        let lines = render_patch(patch);
        // header + dim garbage line
        assert_eq!(lines.len(), 2);
        let garbage = &lines[1];
        // Garbage lines bypass the gutter — first span is the raw text in dim.
        assert_eq!(garbage.spans[0].style, theme::dim());
    }

    #[test]
    fn blank_line_inside_hunk_preserved() {
        let patch = "*** Begin Patch\n*** Update File: x.rs\n line1\n\n+added\n*** End Patch";
        let lines = render_patch(patch);
        // header, " line1", blank, "+added"
        assert_eq!(lines.len(), 4);
        assert!(lines[2].spans.is_empty());
    }

    #[test]
    fn add_line_picks_up_syntax_highlight_color() {
        // For a known extension, an inserted code line gets syntect spans
        // (multiple distinct fgs) instead of a single flat green tint.
        let patch = "*** Begin Patch\n*** Add File: foo.rs\n+let x = 1;\n*** End Patch";
        let lines = render_patch(patch);
        let body = lines
            .iter()
            .find(|l| line_text(l).contains("let x"))
            .expect("body present");
        let spans = content_spans(body);
        assert!(
            !spans.is_empty(),
            "expected post-gutter content spans, got {:?}",
            body.spans
        );
        // syntect typically classifies `let` separately from ` x = 1;`,
        // producing >= 2 spans with at least one non-default fg.
        assert!(
            spans.len() >= 2 || spans.iter().any(|s| s.style.fg.is_some()),
            "expected syntect-classified spans, got {:?}",
            spans
        );
        // The dim modifier is reserved for remove lines — must not appear here.
        for s in spans {
            assert!(
                !s.style.add_modifier.contains(Modifier::DIM),
                "add line should not be dimmed: {:?}",
                s
            );
        }
    }

    #[test]
    fn remove_line_carries_dim_modifier() {
        let patch = "*** Begin Patch\n*** Update File: foo.rs\n@@\n-let x = 1;\n*** End Patch";
        let lines = render_patch(patch);
        let body = lines
            .iter()
            .find(|l| line_text(l).contains("let x"))
            .expect("body present");
        let spans = content_spans(body);
        assert!(!spans.is_empty(), "expected post-gutter content spans");
        for s in spans {
            assert!(
                s.style.add_modifier.contains(Modifier::DIM),
                "remove span missing DIM: {:?}",
                s
            );
        }
    }

    #[test]
    fn wraps_long_added_line_at_width() {
        // 200-char identifier on a single +line; at width 80 it must wrap.
        let big = "x".repeat(200);
        let patch = format!("*** Begin Patch\n*** Add File: foo.txt\n+{big}\n*** End Patch");
        let unwrapped = render_patch(&patch);
        let unwrapped_body_count = unwrapped
            .iter()
            .filter(|l| l.style.bg == Some(theme::diff_add_bg()))
            .count();
        assert_eq!(unwrapped_body_count, 1, "render_patch should not wrap");

        let wrapped = render_patch_wrapped(&patch, 80);
        let body_rows: Vec<&Line<'_>> = wrapped
            .iter()
            .filter(|l| l.style.bg == Some(theme::diff_add_bg()))
            .collect();
        assert!(
            body_rows.len() >= 2,
            "expected wrapped into >=2 rows, got {}",
            body_rows.len()
        );
        // Continuation rows preserve the add-row background so the green
        // column visibly continues across the wrap.
        for row in &body_rows[1..] {
            assert_eq!(
                row.style.bg,
                Some(theme::diff_add_bg()),
                "continuation row lost diff_add_bg: {:?}",
                row
            );
            // First span on continuation rows is the 2-space indent.
            assert_eq!(
                row.spans[0].content.as_ref(),
                "  ",
                "expected 2-space continuation indent, got {:?}",
                row.spans[0]
            );
        }
        // No row exceeds the target width.
        for row in &wrapped {
            assert!(
                line_display_width(row) <= 80,
                "row exceeds width 80 ({}): {:?}",
                line_display_width(row),
                line_text(row)
            );
        }
    }

    #[test]
    fn wrapping_preserves_unicode_width() {
        // East Asian char `漢` is 2 cols wide; width should be measured in
        // columns not bytes (each `漢` is 3 bytes in UTF-8).
        let body: String = "漢".repeat(50); // 100 cols, 150 bytes
        let patch = format!("*** Begin Patch\n*** Add File: foo.txt\n+{body}\n*** End Patch");
        let wrapped = render_patch_wrapped(&patch, 30);
        for row in &wrapped {
            let w = line_display_width(row);
            assert!(
                w <= 30,
                "row exceeds 30 cols (w={w}, bytes={}): {:?}",
                line_text(row).len(),
                line_text(row)
            );
        }
        // We should have produced multiple rows (100 cols / ~28 cont width).
        let body_rows = wrapped
            .iter()
            .filter(|l| line_text(l).contains('漢'))
            .count();
        assert!(
            body_rows >= 4,
            "expected >=4 wrapped rows for 100 cols at width 30, got {body_rows}"
        );
    }

    #[test]
    fn short_lines_pass_through_unchanged() {
        // At a generous width, render_patch_wrapped is idempotent w/ render_patch.
        let patch = "*** Begin Patch\n*** Update File: x.rs\n@@\n ctx\n-old\n+new\n*** End Patch";
        let plain = render_patch(patch);
        let wrapped = render_patch_wrapped(patch, 200);
        assert_eq!(plain.len(), wrapped.len());
        for (a, b) in plain.iter().zip(wrapped.iter()) {
            assert_eq!(line_text(a), line_text(b));
            assert_eq!(a.style.bg, b.style.bg);
            assert_eq!(a.spans.len(), b.spans.len());
        }
    }

    #[test]
    fn wrap_one_handles_zero_width_continuation() {
        // Combining marks (width 0) must not be split off onto their own
        // continuation row — they attach to whatever fragment carries the
        // base char. We feed a line of plain ASCII with a trailing combining
        // acute and assert the combining char is on the same row as `e`.
        let patch = "*** Begin Patch\n*** Add File: f.txt\n+abcdefe\u{0301}\n*** End Patch";
        let wrapped = render_patch_wrapped(patch, 30);
        // Find the body row (only one — short enough to fit).
        let body = wrapped.iter().find(|l| line_text(l).contains('a')).unwrap();
        let text = line_text(body);
        assert!(
            text.contains("e\u{0301}"),
            "combining mark detached: {text:?}"
        );
    }

    #[test]
    fn path_ext_extracts_extension() {
        assert_eq!(path_ext("foo.rs"), Some("rs".into()));
        assert_eq!(path_ext("dir/sub/file.py"), Some("py".into()));
        assert_eq!(path_ext("README"), None);
        assert_eq!(path_ext("archive.tar.gz"), Some("gz".into()));
    }
}
