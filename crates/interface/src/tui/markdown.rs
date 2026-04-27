use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::highlight;
use super::theme;

/// Convert a markdown string into styled ratatui Lines.
pub fn render_markdown(input: &str, width: u16) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, opts);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_lang: Option<String> = None;
    let mut list_depth: usize = 0;
    let mut ordered_index: Option<u64> = None;
    let mut in_blockquote = false;
    let mut list_item_indents: Vec<usize> = Vec::new();
    // Stack of local file paths for open links — popped on TagEnd::Link. Each
    // entry holds the pre-resolved display string to append in dim after the
    // link text (e.g. " (src/foo.rs:L12)"). `None` = non-local (URL), no suffix.
    let mut link_suffix_stack: Vec<Option<String>> = Vec::new();

    let wrap_width = width.saturating_sub(2) as usize;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_line(&mut current_spans, &mut lines);
                    let style = match level {
                        pulldown_cmark::HeadingLevel::H1 => {
                            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                        }
                        pulldown_cmark::HeadingLevel::H2 => {
                            Style::default().add_modifier(Modifier::BOLD)
                        }
                        _ => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    };
                    style_stack.push(style);
                }
                Tag::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_code_block = true;
                    code_block_lang = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let s = info.trim().to_string();
                            if s.is_empty() {
                                None
                            } else {
                                // info-string may include attributes after the lang
                                // (e.g. "rust,ignore") — take the first token.
                                Some(
                                    s.split(|c: char| c == ',' || c.is_whitespace())
                                        .next()
                                        .unwrap_or("")
                                        .to_string(),
                                )
                            }
                        }
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::Emphasis => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.add_modifier(Modifier::ITALIC));
                }
                Tag::Strong => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.add_modifier(Modifier::BOLD));
                }
                Tag::Strikethrough => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::BlockQuote(_) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_blockquote = true;
                    style_stack.push(theme::success_style());
                }
                Tag::List(start) => {
                    flush_line(&mut current_spans, &mut lines);
                    ordered_index = start;
                    list_depth += 1;
                }
                Tag::Item => {
                    flush_line(&mut current_spans, &mut lines);
                    let indent = "  ".repeat(list_depth.saturating_sub(1));
                    let bullet = if let Some(ref mut idx) = ordered_index {
                        let b = format!("{indent}{idx}. ");
                        *idx += 1;
                        current_spans.push(Span::styled(b.clone(), theme::code_style()));
                        b
                    } else {
                        let b = format!("{indent}- ");
                        current_spans.push(Span::styled(b.clone(), current_style(&style_stack)));
                        b
                    };
                    list_item_indents.push(bullet.chars().count());
                }
                Tag::Link { dest_url, .. } => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.fg(theme::CYAN).add_modifier(Modifier::UNDERLINED));
                    link_suffix_stack.push(resolve_local_link(dest_url.as_ref()));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                    lines.push(Line::default());
                }
                TagEnd::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                    lines.push(Line::default());
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    code_block_lang = None;
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                TagEnd::Link => {
                    style_stack.pop();
                    if let Some(Some(suffix)) = link_suffix_stack.pop() {
                        current_spans.push(Span::styled(format!(" ({suffix})"), theme::dim()));
                    }
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 {
                        ordered_index = None;
                    }
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Item => {
                    flush_line(&mut current_spans, &mut lines);
                    list_item_indents.pop();
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = current_style(&style_stack);
                if in_blockquote && current_spans.is_empty() {
                    current_spans.push(Span::styled("│ ", theme::success_style()));
                }
                if in_code_block {
                    // Fenced code blocks with a known language get syntax
                    // highlighted; indented blocks and unknown langs fall
                    // back to the existing cyan code-style treatment.
                    if let Some(lang) = code_block_lang.as_deref() {
                        let highlighted = highlight::highlight_code_block(&text, lang);
                        if !highlighted.is_empty() {
                            flush_line(&mut current_spans, &mut lines);
                            lines.extend(highlighted);
                        }
                    } else {
                        for line in text.lines() {
                            current_spans.push(Span::styled(line.to_string(), theme::code_style()));
                            flush_line(&mut current_spans, &mut lines);
                        }
                    }
                } else {
                    // Hang-indent wrapped continuation lines under the list-item
                    // content column (e.g. "- " → 2 cols, "1. " → 3 cols).
                    let hang = list_item_indents.last().copied().unwrap_or(0);
                    let effective_width = wrap_width.saturating_sub(hang).max(20);
                    let wrapped = super::wrapping::wrap(&text, effective_width);
                    if wrapped.len() <= 1 {
                        current_spans.push(Span::styled(text.to_string(), style));
                    } else {
                        for (i, wl) in wrapped.iter().enumerate() {
                            if i > 0 {
                                flush_line(&mut current_spans, &mut lines);
                                if hang > 0 {
                                    current_spans.push(Span::raw(" ".repeat(hang)));
                                }
                            }
                            current_spans.push(Span::styled(wl.to_string(), style));
                        }
                    }
                }
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(format!("`{code}`"), theme::code_style()));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Rule => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::from(Span::styled(
                    "─".repeat(wrap_width.min(40)),
                    theme::dim(),
                )));
                lines.push(Line::default());
            }
            _ => {}
        }
    }
    flush_line(&mut current_spans, &mut lines);

    // Remove trailing empty lines
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }

    lines
}

fn current_style(stack: &[Style]) -> Style {
    stack.last().copied().unwrap_or_default()
}

/// Detect local file links in markdown destination URLs. Returns the display
/// suffix (path + optional `:L<line>` anchor) for local paths, or `None` for
/// http(s)/mailto/anchor-only references.
///
/// When the model links to a file in the repo, show the resolved path so the
/// user sees where it points instead of the bare label.
fn resolve_local_link(dest: &str) -> Option<String> {
    if dest.is_empty() || dest.starts_with('#') {
        return None;
    }
    // Strip file:// prefix if present.
    let raw = dest.strip_prefix("file://").unwrap_or(dest);
    // Reject anything with a scheme (http:, https:, mailto:, etc.).
    if let Some(idx) = raw.find("://") {
        let scheme = &raw[..idx];
        if scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return None;
        }
    }
    if raw.starts_with("mailto:") || raw.starts_with("tel:") {
        return None;
    }
    // Split off optional `#Lnnn` line anchor. A bare fragment (`#install`)
    // means an in-document anchor reference, not a file link — skip.
    let (path_part, anchor) = if let Some((p, rest)) = raw.rsplit_once('#') {
        if let Some(n) = rest.strip_prefix('L') {
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                (p, Some(format!(":L{n}")))
            } else {
                return None;
            }
        } else {
            return None;
        }
    } else {
        (raw, None)
    };
    let path = std::path::Path::new(path_part);
    // Avoid false positives on plain label text like `[click](here)`: require
    // either an absolute path, a path separator, or a file extension.
    let looks_like_path = path.is_absolute()
        || path_part.contains('/')
        || path_part.contains('\\')
        || path.extension().is_some();
    if !looks_like_path {
        return None;
    }
    let resolved = if path.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            path.strip_prefix(&cwd)
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| path.display().to_string())
        } else {
            path.display().to_string()
        }
    } else {
        path.display().to_string()
    };
    if resolved.is_empty() {
        return None;
    }
    Some(format!("{resolved}{}", anchor.unwrap_or_default()))
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn plain_text_single_line() {
        let lines = render_markdown("Hello world", 80);
        let texts = text_of(&lines);
        assert_eq!(texts, vec!["Hello world"]);
    }

    #[test]
    fn heading_levels() {
        let lines = render_markdown("# H1\n## H2\n### H3", 80);
        // Each heading followed by blank line
        let texts = text_of(&lines);
        assert!(texts.contains(&"H1".to_string()));
        assert!(texts.contains(&"H2".to_string()));
        assert!(texts.contains(&"H3".to_string()));
        // H1 should be bold+underlined
        let h1_line = &lines[0];
        let style = h1_line.spans[0].style;
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn inline_code() {
        let lines = render_markdown("Use `foo` here", 80);
        let texts = text_of(&lines);
        assert_eq!(texts[0], "Use `foo` here");
    }

    #[test]
    fn code_block() {
        let lines = render_markdown("```\nlet x = 1;\nlet y = 2;\n```", 80);
        let texts = text_of(&lines);
        assert!(texts.iter().any(|t| t.contains("let x = 1;")));
        assert!(texts.iter().any(|t| t.contains("let y = 2;")));
    }

    #[test]
    fn unordered_list() {
        let lines = render_markdown("- one\n- two\n- three", 80);
        let texts = text_of(&lines);
        assert!(texts.iter().any(|t| t.contains("- one")));
        assert!(texts.iter().any(|t| t.contains("- two")));
    }

    #[test]
    fn ordered_list() {
        let lines = render_markdown("1. first\n2. second", 80);
        let texts = text_of(&lines);
        assert!(texts.iter().any(|t| t.contains("1. first")));
        assert!(texts.iter().any(|t| t.contains("2. second")));
    }

    #[test]
    fn bold_and_italic() {
        let lines = render_markdown("**bold** and *italic*", 80);
        // Find spans with bold/italic modifiers
        let all_spans: Vec<&Span> = lines.iter().flat_map(|l| &l.spans).collect();
        let bold_span = all_spans.iter().find(|s| s.content.as_ref() == "bold");
        assert!(bold_span.is_some());
        assert!(bold_span
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        let italic_span = all_spans.iter().find(|s| s.content.as_ref() == "italic");
        assert!(italic_span.is_some());
        assert!(italic_span
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
    }

    #[test]
    fn strikethrough() {
        let lines = render_markdown("~~deleted~~", 80);
        let all_spans: Vec<&Span> = lines.iter().flat_map(|l| &l.spans).collect();
        let struck = all_spans.iter().find(|s| s.content.as_ref() == "deleted");
        assert!(struck.is_some());
        assert!(struck
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn horizontal_rule() {
        let lines = render_markdown("above\n\n---\n\nbelow", 80);
        let texts = text_of(&lines);
        // Rule should contain repeated ─
        assert!(texts.iter().any(|t| t.contains("─")));
    }

    #[test]
    fn blockquote() {
        let lines = render_markdown("> quoted text", 80);
        let texts = text_of(&lines);
        assert!(texts.iter().any(|t| t.contains("│ ")));
        assert!(texts.iter().any(|t| t.contains("quoted text")));
    }

    #[test]
    fn empty_input() {
        let lines = render_markdown("", 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn unordered_list_wraps_with_hanging_indent() {
        // Width 30; "- " bullet = 2 cols; content wraps with 2-space hang.
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        let lines = render_markdown(&format!("- {long}"), 30);
        let texts = text_of(&lines);
        // Line 1 starts with the bullet.
        assert!(texts[0].starts_with("- "), "first line: {:?}", texts[0]);
        // Line 2 is a continuation and hangs under column 2 (under "a" of alpha).
        assert!(texts.len() >= 2, "expected wrap, got {:?}", texts);
        assert!(
            texts[1].starts_with("  ") && !texts[1].starts_with("   "),
            "expected 2-space hang, got {:?}",
            texts[1]
        );
    }

    #[test]
    fn ordered_list_wraps_with_hanging_indent() {
        // "1. " bullet = 3 cols → continuation hangs under column 3.
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        let lines = render_markdown(&format!("1. {long}"), 30);
        let texts = text_of(&lines);
        assert!(texts[0].starts_with("1. "), "first line: {:?}", texts[0]);
        assert!(texts.len() >= 2, "expected wrap, got {:?}", texts);
        assert!(
            texts[1].starts_with("   ") && !texts[1].starts_with("    "),
            "expected 3-space hang, got {:?}",
            texts[1]
        );
    }

    #[test]
    fn nested_list_wraps_with_hanging_indent() {
        // Nested unordered → "  - " = 4 cols; continuation hangs under column 4.
        let long = "alpha beta gamma delta epsilon zeta eta theta iota";
        let md = format!("- outer\n  - {long}");
        let lines = render_markdown(&md, 30);
        let texts = text_of(&lines);
        let nested_idx = texts
            .iter()
            .position(|t| t.starts_with("  - "))
            .expect("nested bullet line present");
        let cont = &texts[nested_idx + 1];
        assert!(
            cont.starts_with("    ") && !cont.starts_with("     "),
            "expected 4-space hang on nested continuation, got {:?}",
            cont
        );
    }

    #[test]
    fn trailing_empty_lines_removed() {
        let lines = render_markdown("hello\n\n\n", 80);
        // Should not end with empty lines
        if let Some(last) = lines.last() {
            assert!(!last.spans.is_empty());
        }
    }
}
