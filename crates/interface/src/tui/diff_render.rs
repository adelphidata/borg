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

/// Literal tabs are replaced with this at span construction, so no rendered
/// buffer carries a `\t`. Emitting the tab instead would let the terminal
/// expand it at its own tab stops (commonly 8), which no longer matches the
/// width this module used to lay the row out — the gutter and every wrap point
/// would drift.
const TAB_REPLACEMENT: &str = "    ";
/// Tab stops are rendered as 4 columns wide. Kept in sync with
/// [`TAB_REPLACEMENT`] by construction.
const TAB_WIDTH: usize = TAB_REPLACEMENT.len();
/// Continuation rows from line wrapping are indented by this many spaces so
/// the wrap visually belongs to the row above.
const WRAP_CONTINUATION_INDENT: usize = 2;
/// Drawn between hunks within a single file. Single-character vertical
/// ellipsis so the hunk break is visible without consuming a full row of
/// horizontal space.
const HUNK_SEPARATOR: char = '⋮';

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

/// One body row buffered while we walk a hunk. The whole hunk's bodies are
/// flushed in one syntect call so multi-line strings and block comments
/// keep a single, consistent highlight color across consecutive rows
/// (per-line highlighting would re-init parser state and lose context).
struct BufferedBody {
    kind: DiffKind,
    body: String,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
}

fn render_content_line(
    kind: DiffKind,
    body: &str,
    body_spans: Option<Vec<Span<'static>>>,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
) -> Line<'static> {
    let marker = match kind {
        DiffKind::Add => '+',
        DiffKind::Remove => '-',
        DiffKind::Context => ' ',
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

    // Add lines keep syntect's colors as-is — the green row bg + `+` sign
    // already signal insertion. Remove lines keep syntect's colors but
    // overlay Modifier::DIM so the deletion cue stays readable against the
    // red row bg without the syntax color overpowering it.
    match body_spans {
        Some(mut hl) if !hl.is_empty() => {
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

/// Highlight a hunk's body lines as a single block to preserve syntect
/// parser state across consecutive lines, then emit one rendered line per
/// buffered row. With no extension or on length mismatch (defensive: the
/// highlighter should yield one Line per source line), falls back to the
/// flat per-row tint.
fn flush_body_buffer(buf: &mut Vec<BufferedBody>, ext: Option<&str>, out: &mut Vec<Line<'static>>) {
    if buf.is_empty() {
        return;
    }
    let rows = std::mem::take(buf);

    let highlighted_per_row: Option<Vec<Vec<Span<'static>>>> = match ext {
        Some(lang) => {
            let joined = rows
                .iter()
                .map(|r| r.body.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let hl_lines = highlight::highlight_code_block(&joined, lang);
            if hl_lines.len() == rows.len() {
                Some(hl_lines.into_iter().map(|l| l.spans).collect())
            } else {
                tracing::warn!(
                    rows = rows.len(),
                    got = hl_lines.len(),
                    "diff hunk highlight row-count mismatch; falling back to flat tint"
                );
                None
            }
        }
        None => None,
    };

    for (i, r) in rows.into_iter().enumerate() {
        let spans = highlighted_per_row.as_ref().map(|v| v[i].clone());
        out.push(render_content_line(
            r.kind,
            &r.body,
            spans,
            r.old_lineno,
            r.new_lineno,
        ));
    }
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
    // Hunks within a single file are separated by a dim `⋮` row so the
    // visual break between two hunks doesn't look like the file ended.
    let mut hunks_in_file: u32 = 0;
    // Line-number tracking: we increment per row inside a hunk.
    let mut old_lineno: u32 = 1;
    let mut new_lineno: u32 = 1;
    // Body rows for the current hunk; flushed (and syntax-highlighted as a
    // single block) at every hunk / file boundary so syntect parser state
    // carries across consecutive lines.
    let mut body_buf: Vec<BufferedBody> = Vec::new();

    for raw in patch.lines() {
        let trimmed = raw.trim();

        if trimmed == "*** Begin Patch" || trimmed == "*** End Patch" {
            recognized_anything = true;
            continue;
        }

        if let Some(header) = parse_file_header(trimmed) {
            flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            current_ext = path_ext(&header.path);
            in_file = true;
            hunks_in_file = 0;
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
            flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);
            // Only treat this row as a real hunk boundary when the header
            // parses; otherwise it's malformed and we fall through to the
            // generic body/dim path so we don't draw a stray ⋮ before an
            // un-parseable header.
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_lineno = o;
                new_lineno = n;
                if hunks_in_file > 0 {
                    lines.push(Line::from(Span::styled(
                        HUNK_SEPARATOR.to_string(),
                        theme::diff_hunk_style(),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    expand_tabs(raw),
                    theme::diff_hunk_style(),
                )));
                hunks_in_file += 1;
                recognized_anything = true;
                continue;
            }
        }

        // Blank or whitespace-only line → blank spacer, don't count as recognized DSL.
        if trimmed.is_empty() {
            flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);
            lines.push(Line::default());
            continue;
        }

        // Content lines require a file context; outside one, we can't classify
        // a leading space as "context" (would swallow arbitrary prose).
        if !in_file {
            flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);
            lines.push(Line::from(Span::styled(expand_tabs(raw), theme::dim())));
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
                let body = expand_tabs(raw.get(1..).unwrap_or(""));
                body_buf.push(BufferedBody {
                    kind: k,
                    body,
                    old_lineno: o,
                    new_lineno: n,
                });
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
                flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);
                lines.push(Line::from(Span::styled(expand_tabs(raw), theme::dim())));
            }
        }
    }

    flush_body_buffer(&mut body_buf, current_ext.as_deref(), &mut lines);

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
    with_patch_render_cache(patch, width, || wrap_lines(render_patch(patch), width))
}

/// Entries retained by the patch render cache. Only the handful of patch cells
/// actually on screen matter, so a small bound is enough; scrolling past an old
/// diff simply re-renders it once.
const PATCH_RENDER_CACHE_ENTRIES: usize = 16;

thread_local! {
    /// Memo for [`render_patch_wrapped`], keyed by `(patch bytes, width)`.
    ///
    /// The transcript calls it from inside a cell's render function, so an
    /// unmemoized patch is re-parsed *and* re-syntax-highlighted on every frame
    /// — during streaming that is many times per second for a diff that never
    /// changes. `thread_local` rather than a mutex because rendering is
    /// single-threaded; this keeps the hot path lock-free.
    static PATCH_RENDER_CACHE: std::cell::RefCell<Vec<((u64, usize), Vec<Line<'static>>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn with_patch_render_cache(
    patch: &str,
    width: usize,
    render: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    patch.hash(&mut hasher);
    let key = (hasher.finish(), width);

    if let Some(hit) = PATCH_RENDER_CACHE.with_borrow(|cache| {
        cache
            .iter()
            .find(|(cached_key, _)| *cached_key == key)
            .map(|(_, lines)| lines.clone())
    }) {
        return hit;
    }

    let rendered = render();
    PATCH_RENDER_CACHE.with_borrow_mut(|cache| {
        if cache.len() >= PATCH_RENDER_CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push((key, rendered.clone()));
    });
    rendered
}

/// Replace literal tabs with [`TAB_REPLACEMENT`]. `render_patch` runs every
/// piece of patch text through this before it becomes a span.
fn expand_tabs(s: &str) -> String {
    if s.contains('\t') {
        s.replace('\t', TAB_REPLACEMENT)
    } else {
        s.to_string()
    }
}

/// Width of `c` in terminal columns: tab counts as 4, control/zero-width as 0.
/// `render_patch` output no longer contains tabs, but [`wrap_lines`] is public
/// and may be handed lines built elsewhere.
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
    use ratatui::style::Color;

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

    /// Unhighlighted bodies (no syntax for the extension) still get tabs
    /// expanded, so the terminal never re-expands them at its own tab stops.
    #[test]
    fn tabs_expand_to_spaces_in_body() {
        let patch = "*** Begin Patch\n*** Add File: foo.txt\n+\tindented\n*** End Patch";
        let lines = render_patch(patch);
        let body = line_text(&lines[1]);
        assert!(
            body.contains("    indented"),
            "tab should render as 4 spaces, got {body:?}"
        );
        assert!(!body.contains('\t'), "body kept a literal tab: {body:?}");
    }

    /// The expansion must happen before the highlighter runs, or the syntect
    /// path would smuggle the literal tab back into the spans it emits.
    #[test]
    fn tabs_expand_in_highlighted_body() {
        let patch = "*** Begin Patch\n*** Add File: foo.rs\n+\tlet x = 1;\n*** End Patch";
        let lines = render_patch(patch);
        let body = line_text(&lines[1]);
        assert!(
            body.contains("    let"),
            "tab should render as 4 spaces on the highlighted path, got {body:?}"
        );
        assert!(!body.contains('\t'), "body kept a literal tab: {body:?}");
    }

    /// The invariant the fix rests on: no line `render_patch` emits — body,
    /// hunk header, or unrecognized/dim fallback — may contain a raw tab.
    #[test]
    fn no_literal_tabs_in_any_rendered_line() {
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: foo.rs\n",
            "@@ fn main() {\t\n",
            " \tcontext line\n",
            "-\tremoved line\n",
            "+\tadded line\n",
            "\tgarbage line with a tab\n",
            "*** End Patch"
        );
        for line in render_patch(patch) {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\t'),
                    "rendered span kept a literal tab: {:?}",
                    span.content
                );
            }
        }
    }

    /// With tabs expanded, the width this module models is the width the
    /// terminal actually draws — so wrapped rows genuinely fit.
    #[test]
    fn tab_expansion_wraps_at_modeled_width() {
        let patch = "*** Begin Patch\n*** Add File: foo.txt\n+\tabcdefghij\n*** End Patch";
        let width = 20;
        let wrapped = render_patch_wrapped(patch, width);
        let body_rows: Vec<&Line<'_>> = wrapped
            .iter()
            .filter(|l| l.style.bg == Some(theme::diff_add_bg()))
            .collect();
        assert!(
            body_rows.len() >= 2,
            "expected the tab-expanded body to wrap at width {width}, got {} row(s)",
            body_rows.len()
        );
        for row in &body_rows {
            assert!(
                line_display_width(row) <= width,
                "wrapped row exceeds width {width}: {:?}",
                line_text(row)
            );
            assert!(
                !line_text(row).contains('\t'),
                "wrapped row kept a literal tab: {:?}",
                line_text(row)
            );
        }
        assert!(
            line_text(body_rows[0]).contains("    "),
            "first body row should carry the expanded tab: {:?}",
            line_text(body_rows[0])
        );
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
    fn hunk_separator_between_hunks() {
        // Two `@@` hunks within one file: a `⋮` row separates them, but the
        // first hunk has no leading separator. Locks the visual cue used to
        // distinguish "two hunks in one file" from "two files".
        let patch = "*** Begin Patch\n\
            *** Update File: x.rs\n\
            @@ -1,1 +1,1 @@\n\
            -a\n\
            +b\n\
            @@ -10,1 +10,1 @@\n\
            -c\n\
            +d\n\
            *** End Patch";
        let lines = render_patch(patch);

        let sep_count = lines
            .iter()
            .filter(|l| line_text(l).trim() == HUNK_SEPARATOR.to_string())
            .count();
        assert_eq!(sep_count, 1, "expected one separator between two hunks");

        // Sanity: the separator sits between the two hunk headers, not before
        // the first one.
        let first_hunk = lines
            .iter()
            .position(|l| line_text(l).contains("@@ -1"))
            .expect("first hunk header");
        let second_hunk = lines
            .iter()
            .position(|l| line_text(l).contains("@@ -10"))
            .expect("second hunk header");
        let sep_idx = lines
            .iter()
            .position(|l| line_text(l).trim() == HUNK_SEPARATOR.to_string())
            .expect("separator present");
        assert!(
            first_hunk < sep_idx && sep_idx < second_hunk,
            "separator misplaced: first={first_hunk}, sep={sep_idx}, second={second_hunk}"
        );
    }

    #[test]
    fn first_hunk_has_no_leading_separator() {
        let patch = "*** Begin Patch\n\
            *** Update File: x.rs\n\
            @@ -1,1 +1,1 @@\n\
            +only\n\
            *** End Patch";
        let lines = render_patch(patch);
        let sep_count = lines
            .iter()
            .filter(|l| line_text(l).trim() == HUNK_SEPARATOR.to_string())
            .count();
        assert_eq!(sep_count, 0, "single-hunk file must not have a separator");
    }

    #[test]
    fn hunk_block_highlighting_preserves_state_across_lines() {
        // A Rust block comment opens on one `+` line and closes on the next.
        // Per-line highlighting would re-init syntect's parser on each row,
        // dropping the "inside a comment" context, so `line` on row 2 would
        // highlight as a plain identifier — visibly different fg from the
        // `multi` comment-body on row 1. Hunk-block highlighting carries the
        // parser state forward, so both bodies share the comment fg.
        let patch = "*** Begin Patch\n\
            *** Add File: foo.rs\n\
            +/* multi\n\
            +line */\n\
            *** End Patch";
        let lines = render_patch(patch);

        let body_rows: Vec<&Line<'_>> = lines
            .iter()
            .filter(|l| l.style.bg == Some(theme::diff_add_bg()))
            .collect();
        assert_eq!(body_rows.len(), 2, "expected two add rows");

        let span_text_fg = |spans: &[Span<'_>], needle: &str| -> Option<Color> {
            spans
                .iter()
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };

        let row1 = content_spans(body_rows[0]);
        let row2 = content_spans(body_rows[1]);
        let multi_fg = span_text_fg(row1, "multi").expect("`multi` span on row 1 has fg");
        let line_fg = span_text_fg(row2, "line").expect("`line` span on row 2 has fg");
        assert_eq!(
            multi_fg, line_fg,
            "row 2 must inherit the row 1 comment fg via hunk-block highlighting"
        );
    }

    #[test]
    fn path_ext_extracts_extension() {
        assert_eq!(path_ext("foo.rs"), Some("rs".into()));
        assert_eq!(path_ext("dir/sub/file.py"), Some("py".into()));
        assert_eq!(path_ext("README"), None);
        assert_eq!(path_ext("archive.tar.gz"), Some("gz".into()));
    }

    /// The transcript calls `render_patch_wrapped` from inside a cell's render
    /// function, so an unmemoized diff is re-parsed and re-highlighted on every
    /// frame. The memo must hit for a repeat call at the same width, and must
    /// *miss* on a width change — a stale-width hit would render a diff wrapped
    /// for the wrong terminal size.
    #[test]
    fn patch_render_is_memoized_per_width() {
        // The cache is thread-local and shared with other tests on this thread;
        // a unique patch body keeps this test independent of them.
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: memo_probe.rs\n",
            "@@ fn memo_probe() {\n",
            " context\n",
            "-old\n",
            "+new\n",
            "*** End Patch"
        );

        let first = render_patch_wrapped(patch, 60);
        let cached = render_patch_wrapped(patch, 60);
        assert_eq!(
            first.len(),
            cached.len(),
            "a repeat call at the same width must return the same render"
        );
        assert!(
            PATCH_RENDER_CACHE.with_borrow(|cache| cache.len() == 1),
            "a repeat call must reuse the single cached entry, not add another"
        );

        let narrower = render_patch_wrapped(patch, 24);
        assert!(
            PATCH_RENDER_CACHE.with_borrow(|cache| cache.len() == 2),
            "a different width must be cached separately"
        );
        assert!(
            narrower.len() >= first.len(),
            "narrower width should wrap into at least as many rows: {} vs {}",
            narrower.len(),
            first.len()
        );
    }

    /// The cache must stay bounded — it lives for the process lifetime, and the
    /// transcript can accumulate arbitrarily many distinct patches.
    #[test]
    fn patch_render_cache_is_bounded() {
        for i in 0..(PATCH_RENDER_CACHE_ENTRIES * 3) {
            let patch = format!(
                "*** Begin Patch\n*** Add File: bound_probe_{i}.rs\n+let x = {i};\n*** End Patch"
            );
            render_patch_wrapped(&patch, 80);
        }
        assert!(
            PATCH_RENDER_CACHE.with_borrow(|cache| cache.len() <= PATCH_RENDER_CACHE_ENTRIES),
            "cache grew past its bound"
        );
    }
}
