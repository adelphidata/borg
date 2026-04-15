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

fn render_content_line(raw: &str, kind: DiffKind, ext: Option<&str>) -> Line<'static> {
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

    let prefix = Span::styled(format!("{marker} "), tint);
    let mut spans = vec![prefix];

    match (kind, ext) {
        (DiffKind::Context, Some(lang)) if !body.is_empty() => {
            // Highlight context lines using the file's language
            spans.extend(highlight::highlight_inline(body, lang));
        }
        _ => spans.push(Span::styled(body.to_string(), tint)),
    }
    Line::from(spans)
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
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", header.label), theme::diff_header_style()),
                Span::styled(header.path, theme::diff_header_style()),
            ]));
            recognized_anything = true;
            continue;
        }

        if trimmed.starts_with("@@") {
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
                lines.push(render_content_line(raw, k, current_ext.as_deref()));
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
        // the + body should be green-tinted
        assert_eq!(lines[1].spans[0].style, theme::diff_add_style());
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
        assert_eq!(plus_line.spans[0].style, theme::diff_add_style());
        let minus_line = lines
            .iter()
            .find(|l| line_text(l).contains("old"))
            .expect("minus line present");
        assert_eq!(minus_line.spans[0].style, theme::diff_remove_style());
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
        assert_eq!(body.spans[0].style, theme::diff_add_style());
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
        // The `-ghost removal` line should be dim, not red.
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
    fn unknown_prefix_is_dimmed_not_panic() {
        // A content line with no valid prefix — parser keeps it as a dim line.
        let patch = "*** Begin Patch\n*** Update File: x.rs\nOHNO no prefix here\n*** End Patch";
        let lines = render_patch(patch);
        // header + dim garbage line
        assert_eq!(lines.len(), 2);
        let garbage = &lines[1];
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
