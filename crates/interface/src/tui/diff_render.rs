//! Renders the Borg patch DSL as a colored unified-diff view.
//!
//! The patch DSL uses line prefixes: `+` add, `-` remove, ` ` context, with
//! file headers `*** Add/Update/Delete File:` and optional `@@` hunk markers.
//! This parser walks the raw patch text and emits [`Line`]s for display in
//! the transcript. It never touches the filesystem — it is a pure TUI-side
//! read of data we already have in the tool call args.

use ratatui::text::{Line, Span};

use super::highlight;
use super::theme;

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

    // Codex-style: 4-col line-number gutter for old + new, then 2-col sign column.
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

    match (kind, ext) {
        (DiffKind::Context, Some(lang)) if !body.is_empty() => {
            spans.extend(highlight::highlight_inline(body, lang));
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
    // Codex-style line-number tracking: we increment per row inside a hunk.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// Find the first content span in a diff line — i.e. the span styled
    /// with one of the diff foreground tints, skipping the line-number
    /// gutter and sign column. Returns `None` for non-content lines.
    fn first_content_span<'a>(line: &'a Line<'a>) -> Option<&'a Span<'a>> {
        line.spans.iter().find(|s| {
            s.style == theme::diff_add_style()
                || s.style == theme::diff_remove_style()
                || s.style == theme::dim()
        })
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
        // the + body should be green-tinted (span after the gutter columns)
        assert_eq!(
            first_content_span(&lines[1]).map(|s| s.style),
            Some(theme::diff_add_style())
        );
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

        // Find the +new and -old lines; verify styling.
        let plus_line = lines
            .iter()
            .find(|l| line_text(l).contains("new"))
            .expect("plus line present");
        assert_eq!(
            first_content_span(plus_line).map(|s| s.style),
            Some(theme::diff_add_style())
        );
        let minus_line = lines
            .iter()
            .find(|l| line_text(l).contains("old"))
            .expect("minus line present");
        assert_eq!(
            first_content_span(minus_line).map(|s| s.style),
            Some(theme::diff_remove_style())
        );
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
        assert_eq!(
            first_content_span(body).map(|s| s.style),
            Some(theme::diff_add_style())
        );
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
        // Codex-style: each content line carries an old + new line-number gutter
        // and a sign column. Hunk header at line 5 means the first body line is 5.
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
    fn path_ext_extracts_extension() {
        assert_eq!(path_ext("foo.rs"), Some("rs".into()));
        assert_eq!(path_ext("dir/sub/file.py"), Some("py".into()));
        assert_eq!(path_ext("README"), None);
        assert_eq!(path_ext("archive.tar.gz"), Some("gz".into()));
    }
}
