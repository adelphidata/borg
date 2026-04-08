use borg_core::types::{PlanStep, PlanStepStatus};
use ratatui::text::{Line, Span};
use throbber_widgets_tui::ThrobberState;

use super::markdown;
use super::theme;

#[derive(Clone)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone)]
pub enum HistoryCell {
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
    },
    ToolStart {
        name: String,
        args: String,
        completed: bool,
        start_time: Option<std::time::Instant>,
    },
    ToolResult {
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
        display_label: String,
    },
    ShellApproval {
        command: String,
        status: ApprovalStatus,
    },
    Heartbeat {
        text: String,
    },
    System {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolStreaming {
        lines: Vec<(String, bool)>,
    },
    /// Structured plan with step tracking.
    Plan {
        steps: Vec<PlanStep>,
    },
    Separator,
}

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Style a system message line with visual hierarchy:
/// - Icons (✓, ●) → cyan, (⚠) → amber, (✗) → red
/// - Explicit headers/titles → bold white
/// - Lines with `Label:` prefix → label bold white, rest dim
/// - History lines `[HH:MM] Role: ...` → styled role labels
/// - Log lines → timestamp+level bold white, message dim
/// - Help lines with `/command` → command bold white
/// - Everything else → dim
fn style_system_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();

    // Empty lines
    if trimmed.is_empty() {
        return Line::from(Span::styled(line.to_string(), theme::dim()));
    }

    // Separator lines (all ─ characters)
    if trimmed.chars().all(|c| c == '─') {
        return Line::from(Span::styled(line.to_string(), theme::dim()));
    }

    // Icon lines: ✓ or ● → cyan icon
    if trimmed.starts_with('✓') || trimmed.starts_with('●') {
        let indent_len = line.len() - trimmed.len();
        let icon_len = trimmed.chars().next().map_or(0, char::len_utf8);
        return Line::from(vec![
            Span::styled(line[..indent_len].to_string(), theme::dim()),
            Span::styled(trimmed[..icon_len].to_string(), theme::icon_style()),
            Span::styled(trimmed[icon_len..].to_string(), theme::dim()),
        ]);
    }

    // Icon lines: ⚠ → amber
    if trimmed.starts_with('⚠') {
        let indent_len = line.len() - trimmed.len();
        let icon_len = '⚠'.len_utf8();
        return Line::from(vec![
            Span::styled(line[..indent_len].to_string(), theme::dim()),
            Span::styled(trimmed[..icon_len].to_string(), theme::warning_style()),
            Span::styled(trimmed[icon_len..].to_string(), theme::dim()),
        ]);
    }

    // Icon lines: ✗ → red
    if trimmed.starts_with('✗') {
        let indent_len = line.len() - trimmed.len();
        let icon_len = '✗'.len_utf8();
        return Line::from(vec![
            Span::styled(line[..indent_len].to_string(), theme::dim()),
            Span::styled(trimmed[..icon_len].to_string(), theme::error_style()),
            Span::styled(trimmed[icon_len..].to_string(), theme::dim()),
        ]);
    }

    // History lines: [HH:MM] Role: content
    if trimmed.starts_with('[') {
        if let Some(bracket_end) = trimmed.find("] ") {
            let after_bracket = &trimmed[bracket_end + 2..];
            // Detect role label
            let (role_end, role_style) = if after_bracket.starts_with("You:") {
                (
                    4,
                    theme::icon_style().add_modifier(ratatui::style::Modifier::BOLD),
                )
            } else if after_bracket.starts_with("Assistant:") {
                (10, theme::header_style())
            } else if after_bracket.starts_with("Tool ") {
                // "Tool (id):" — find the colon
                if let Some(colon) = after_bracket.find(':') {
                    (colon + 1, theme::header_style())
                } else {
                    (0, theme::dim())
                }
            } else {
                (0, theme::dim())
            };

            if role_end > 0 {
                let ts_end = bracket_end + 2; // includes "] "
                return Line::from(vec![
                    Span::styled(trimmed[..ts_end].to_string(), theme::dim()),
                    Span::styled(after_bracket[..role_end].to_string(), role_style),
                    Span::styled(after_bracket[role_end..].to_string(), theme::dim()),
                ]);
            }
        }
    }

    // Log lines: "2026-04-03T17:05:25...Z  WARN ..." or "2026-04-03T17:05:25...Z ERROR ..."
    if trimmed.len() > 20 && trimmed.as_bytes()[0].is_ascii_digit() && trimmed[..20].contains('T') {
        for level in &[" ERROR ", " WARN ", " INFO ", " DEBUG ", " TRACE "] {
            if let Some(pos) = trimmed.find(level) {
                let prefix_end = pos + level.len();
                return Line::from(vec![
                    Span::styled(trimmed[..prefix_end].to_string(), theme::header_style()),
                    Span::styled(trimmed[prefix_end..].to_string(), theme::dim()),
                ]);
            }
        }
        return Line::from(Span::styled(line.to_string(), theme::dim()));
    }

    // Indented lines — check for /command patterns (for /help)
    if line.starts_with("  ") {
        return style_indented_line(line);
    }

    // XML tags → bold white
    if trimmed.starts_with('<') {
        return Line::from(Span::styled(line.to_string(), theme::header_style()));
    }

    // Markdown headers → bold white
    if trimmed.starts_with('#') {
        return Line::from(Span::styled(line.to_string(), theme::header_style()));
    }

    // Short title-like lines (no spaces, or very short) → bold white
    // Matches: "Browser", "Security", "Host Security", "Borg Doctor", "Borg Vitals",
    //          "Commands:", "Built-in tools:", "Pending Requests", "Approved Senders"
    if !trimmed.contains(':') && trimmed.len() <= 40 && !trimmed.starts_with('(') {
        return Line::from(Span::styled(line.to_string(), theme::header_style()));
    }

    // "Label: content" pattern — bold the label, dim the content
    // Matches: "Session: 58 messages", "LLM usage: 0 prompt...", "24h: 1 user...",
    //          "Budget: 0/1000000", "Summary: 36 passed...", "Tip: ..."
    if let Some(colon_pos) = trimmed.find(':') {
        let label = &trimmed[..colon_pos];
        // Only treat as label if it's short and looks like a title (no long prose before colon)
        if label.len() <= 30 && !label.contains("  ") {
            return Line::from(vec![
                Span::styled(trimmed[..colon_pos + 1].to_string(), theme::header_style()),
                Span::styled(trimmed[colon_pos + 1..].to_string(), theme::dim()),
            ]);
        }
    }

    // Default: dim
    Line::from(Span::styled(line.to_string(), theme::dim()))
}

/// Style indented lines: `/command` patterns (for /help) and `name  desc` tool listings.
fn style_indented_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    // Slash commands: trimmed starts with "/" (actual commands from /help)
    if let Some(after_slash) = trimmed.strip_prefix('/') {
        let word_len = after_slash
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after_slash.len());
        if word_len > 0 {
            let cmd_len = 1 + word_len; // include the '/'
            return Line::from(vec![
                Span::styled(indent.to_string(), theme::dim()),
                Span::styled(trimmed[..cmd_len].to_string(), theme::header_style()),
                Span::styled(trimmed[cmd_len..].to_string(), theme::dim()),
            ]);
        }
    }

    // Tool listing: "name<2+ spaces>description" — make name white
    if let Some(gap) = trimmed.find("  ") {
        let name = &trimmed[..gap];
        // Tool names are single words with underscores, no spaces
        if !name.is_empty()
            && !name.contains(' ')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            let rest = &trimmed[gap..];
            return Line::from(vec![
                Span::styled(indent.to_string(), theme::dim()),
                Span::styled(
                    name.to_string(),
                    ratatui::style::Style::default().fg(ratatui::style::Color::White),
                ),
                Span::styled(rest.to_string(), theme::dim()),
            ]);
        }
    }

    Line::from(Span::styled(line.to_string(), theme::dim()))
}

impl HistoryCell {
    /// True when this cell should visually hug the previous cell
    /// (no blank-line spacer inserted before it by the render loop).
    pub fn is_stream_continuation(&self) -> bool {
        matches!(
            self,
            HistoryCell::ToolResult { .. } | HistoryCell::ToolStreaming { .. }
        )
    }

    pub fn render(
        &self,
        width: u16,
        _throbber_state: Option<&ThrobberState>,
    ) -> Vec<Line<'static>> {
        match self {
            HistoryCell::User { text } => {
                let bg = theme::user_message_style();
                let prefix_style =
                    bg.add_modifier(ratatui::style::Modifier::BOLD | ratatui::style::Modifier::DIM);
                let mention_style = theme::file_mention_style();
                let w = width as usize;
                let mut lines = Vec::new();
                // Unstyled spacer — must NOT use `bg` or it creates a double-background
                // band above the user message (the render loop already inserts a separator).
                lines.push(Line::default());
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 {
                        Span::styled(format!("{} ", theme::CHEVRON), prefix_style)
                    } else {
                        Span::styled("  ", bg)
                    };
                    let spans = parse_at_mentions(line, bg, mention_style);
                    let mut all_spans = vec![prefix];
                    all_spans.extend(spans);
                    let content_width: usize = all_spans
                        .iter()
                        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                        .sum();
                    if content_width < w {
                        all_spans.push(Span::styled(" ".repeat(w - content_width), bg));
                    }
                    lines.push(Line::from(all_spans).style(bg));
                }
                lines
            }
            HistoryCell::Assistant { text, streaming } => {
                let mut lines = if text.is_empty() && *streaming {
                    vec![]
                } else {
                    let prefix_span = Span::styled(format!("{} ", theme::BULLET), theme::dim());
                    let md_lines = markdown::render_markdown(text, width.saturating_sub(2));
                    if md_lines.is_empty() {
                        vec![Line::from(prefix_span)]
                    } else {
                        let mut result = Vec::with_capacity(md_lines.len());
                        for (i, mut line) in md_lines.into_iter().enumerate() {
                            if i == 0 {
                                let mut spans = vec![prefix_span.clone()];
                                spans.extend(line.spans);
                                result.push(Line::from(spans));
                            } else {
                                let mut spans = vec![Span::raw("  ")];
                                spans.append(&mut line.spans);
                                result.push(Line::from(spans));
                            }
                        }
                        result
                    }
                };
                if *streaming {
                    lines.push(Line::from(Span::styled("▊", theme::dim())));
                }
                lines
            }
            HistoryCell::ToolStart {
                name,
                args,
                completed,
                ..
            } => {
                let cat = super::tool_display::classify_tool(name, args);
                let bullet_style = if *completed {
                    theme::tool_bullet_done()
                } else {
                    theme::tool_bullet_active()
                };
                let mut header_spans =
                    vec![Span::styled(format!("{} ", theme::BULLET), bullet_style)];
                header_spans.extend(super::tool_display::tool_header_spans(&cat));
                let mut lines = vec![Line::from(header_spans)];
                if let Some(detail_spans) = super::tool_display::tool_detail_line(&cat) {
                    let mut spans = vec![Span::styled(
                        format!("  {} ", theme::TREE_END),
                        theme::dim(),
                    )];
                    spans.extend(detail_spans);
                    lines.push(Line::from(spans));
                }
                lines
            }
            HistoryCell::ToolResult {
                output,
                is_error,
                duration_ms,
                display_label,
                ..
            } => {
                let style = if *is_error {
                    theme::error_style()
                } else {
                    theme::dim()
                };
                let preview_lines: Vec<&str> = output.lines().take(5).collect();
                let total_count = output.lines().count();
                let truncated = total_count > 5;
                let mut lines: Vec<Line<'static>> = Vec::new();
                for (i, pl) in preview_lines.iter().enumerate() {
                    let prefix = if i == 0 {
                        format!("  {} ", theme::TREE_END)
                    } else {
                        "    ".to_string()
                    };
                    let text = if pl.len() > 200 {
                        format!("{}...", truncate_str(pl, 197))
                    } else {
                        pl.to_string()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(text, style),
                    ]));
                }
                if truncated {
                    let extra = total_count - 5;
                    lines.push(Line::from(Span::styled(
                        format!("    {} +{extra} more lines", theme::ELLIPSIS),
                        theme::dim(),
                    )));
                }
                // Status line with check/cross and duration
                let (indicator, ind_style) = if *is_error {
                    (theme::CROSS, theme::cross_style())
                } else {
                    (theme::CHECK, theme::check_style())
                };
                let duration_str = match duration_ms {
                    Some(ms) => {
                        let secs_f = *ms as f64 / 1000.0;
                        format!(" • {secs_f:.1}s")
                    }
                    None => String::new(),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {indicator} "), ind_style),
                    Span::styled(format!("{display_label}{duration_str}"), theme::dim()),
                ]));
                lines
            }
            HistoryCell::ShellApproval { command, status } => {
                let status_text = match status {
                    ApprovalStatus::Pending => "[y/N]",
                    ApprovalStatus::Approved => "[approved]",
                    ApprovalStatus::Denied => "[denied]",
                };
                let status_style = match status {
                    ApprovalStatus::Pending => theme::error_style(),
                    ApprovalStatus::Approved => theme::success_style(),
                    ApprovalStatus::Denied => theme::dim(),
                };
                vec![
                    Line::from(vec![
                        Span::styled("  [run_shell] ", theme::error_style()),
                        Span::styled(command.clone(), theme::error_style()),
                    ]),
                    Line::from(vec![
                        Span::raw("  Allow? "),
                        Span::styled(status_text.to_string(), status_style),
                    ]),
                ]
            }
            HistoryCell::Heartbeat { text } => {
                vec![Line::from(vec![
                    Span::styled("[heartbeat] ", theme::code_style()),
                    Span::styled(text.clone(), theme::code_style()),
                ])]
            }
            HistoryCell::System { text } => text.lines().map(style_system_line).collect(),
            HistoryCell::Thinking { text } => {
                // Don't render an empty thinking box
                if text.is_empty() {
                    return vec![];
                }
                let border = theme::thinking_border_style();
                let content_style = ratatui::style::Style::default()
                    .fg(ratatui::style::Color::DarkGray)
                    .add_modifier(ratatui::style::Modifier::ITALIC);
                let inner_w = (width as usize).saturating_sub(4);
                let label = " thinking ";
                let top_rule_len = inner_w.saturating_sub(label.len());
                let top = format!(
                    "{}{}{label}{}{}",
                    theme::BOX_TOP_LEFT,
                    theme::SEPARATOR.repeat(2),
                    theme::SEPARATOR.repeat(top_rule_len),
                    theme::BOX_TOP_RIGHT,
                );
                let mut lines = vec![Line::from(Span::styled(top, border))];
                for l in text.lines() {
                    let display = truncate_str(l, inner_w);
                    let display_len = unicode_width::UnicodeWidthStr::width(display);
                    let pad = inner_w.saturating_sub(display_len);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{} ", theme::BOX_VERTICAL), border),
                        Span::styled(display.to_string(), content_style),
                        Span::styled(
                            format!("{}{}", " ".repeat(pad), theme::BOX_VERTICAL),
                            border,
                        ),
                    ]));
                }
                let bottom = format!(
                    "{}{}{}",
                    theme::BOX_BOTTOM_LEFT,
                    theme::SEPARATOR.repeat(inner_w + 2),
                    theme::BOX_BOTTOM_RIGHT,
                );
                lines.push(Line::from(Span::styled(bottom, border)));
                lines
            }
            HistoryCell::ToolStreaming {
                lines: tool_lines, ..
            } => {
                let _ = width;
                let mut rendered: Vec<Line<'static>> = Vec::new();
                let total = tool_lines.len();
                let max_visible = 8;
                let skip = total.saturating_sub(max_visible);
                if skip > 0 {
                    rendered.push(Line::from(Span::styled(
                        format!("  ... ({skip} lines above)"),
                        theme::dim(),
                    )));
                }
                for (line_text, is_stderr) in tool_lines.iter().skip(skip) {
                    let style = if *is_stderr {
                        theme::error_style()
                    } else {
                        theme::dim()
                    };
                    let display = if line_text.len() > 200 {
                        format!("{}...", truncate_str(line_text, 197))
                    } else {
                        line_text.clone()
                    };
                    let prefix = if *is_stderr { "! " } else { "\u{2502} " };
                    rendered.push(Line::from(vec![
                        Span::styled(format!("  {prefix}"), style),
                        Span::styled(display, style),
                    ]));
                }
                rendered
            }
            HistoryCell::Plan { steps } => {
                let mut lines = vec![Line::from(Span::styled(
                    "Plan:".to_string(),
                    theme::dim().add_modifier(ratatui::style::Modifier::BOLD),
                ))];
                for step in steps {
                    let (icon, style) = match step.status {
                        PlanStepStatus::Completed => {
                            (theme::CHECK.to_string(), theme::check_style())
                        }
                        PlanStepStatus::InProgress => (
                            "~".to_string(),
                            ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
                        ),
                        PlanStepStatus::Pending => (" ".to_string(), theme::dim()),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  [{icon}] "), style),
                        Span::styled(step.title.clone(), style),
                    ]));
                }
                lines
            }
            HistoryCell::Separator => {
                let rule_width = ((width as usize) * 2 / 3).min(80);
                vec![Line::from(Span::styled(
                    theme::SEPARATOR.repeat(rule_width),
                    theme::dim(),
                ))]
            }
        }
    }
}

/// Split a line into spans, highlighting `@path` tokens with the mention style.
fn parse_at_mentions(
    line: &str,
    normal: ratatui::style::Style,
    mention: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;

    while let Some(at_pos) = rest.find('@') {
        // Check word boundary: must be at start or preceded by whitespace
        let at_word_boundary = at_pos == 0 || rest.as_bytes()[at_pos - 1] == b' ';
        if !at_word_boundary {
            // Not a mention, consume up to and including the '@'
            spans.push(Span::styled(rest[..at_pos + 1].to_string(), normal));
            rest = &rest[at_pos + 1..];
            continue;
        }

        // Push text before the @
        if at_pos > 0 {
            spans.push(Span::styled(rest[..at_pos].to_string(), normal));
        }

        // Find end of mention (next space or end of string)
        let after_at = &rest[at_pos + 1..];
        let end = after_at.find(' ').unwrap_or(after_at.len());
        if end == 0 {
            // Bare '@' with no path
            spans.push(Span::styled("@".to_string(), normal));
            rest = after_at;
            continue;
        }

        let mention_text = format!("@{}", &after_at[..end]);
        spans.push(Span::styled(mention_text, mention));
        rest = &after_at[end..];
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), normal));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn truncate_str_ascii() {
        assert_eq!(truncate_str("hello", 3), "hel");
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello", 5), "hello");
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn truncate_str_multibyte() {
        // '€' is 3 bytes in UTF-8
        let s = "€€€";
        assert_eq!(s.len(), 9);
        // Truncate at 4 bytes: can only fit 1 '€' (3 bytes), bytes 4-5 are mid-char
        assert_eq!(truncate_str(s, 4), "€");
        assert_eq!(truncate_str(s, 6), "€€");
        assert_eq!(truncate_str(s, 3), "€");
        assert_eq!(truncate_str(s, 1), "");
    }

    #[test]
    fn parse_at_mentions_basic() {
        let normal = Style::default();
        let mention = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = parse_at_mentions("hello @file.rs world", normal, mention);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "hello ");
        assert_eq!(spans[1].content.as_ref(), "@file.rs");
        assert_eq!(spans[2].content.as_ref(), " world");
        assert_eq!(spans[1].style, mention);
    }

    #[test]
    fn parse_at_mentions_no_mention() {
        let normal = Style::default();
        let mention = Style::default();
        let spans = parse_at_mentions("no mentions here", normal, mention);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "no mentions here");
    }

    #[test]
    fn parse_at_mentions_email_not_mention() {
        let normal = Style::default();
        let mention = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = parse_at_mentions("user@example.com", normal, mention);
        // '@' preceded by non-space, so not a mention
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "user@");
        assert_eq!(spans[1].content.as_ref(), "example.com");
    }

    #[test]
    fn parse_at_mentions_at_start() {
        let normal = Style::default();
        let mention = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = parse_at_mentions("@path rest", normal, mention);
        assert_eq!(spans[0].content.as_ref(), "@path");
        assert_eq!(spans[0].style, mention);
    }

    #[test]
    fn parse_at_mentions_bare_at() {
        let normal = Style::default();
        let mention = Style::default();
        let spans = parse_at_mentions("@ alone", normal, mention);
        assert_eq!(spans[0].content.as_ref(), "@");
        assert_eq!(spans[1].content.as_ref(), " alone");
    }

    #[test]
    fn render_user_cell() {
        let cell = HistoryCell::User {
            text: "hello".to_string(),
        };
        let lines = cell.render(40, None);
        // 1 unstyled spacer + 1 content
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn render_user_cell_padded_box() {
        let cell = HistoryCell::User {
            text: "hello world".to_string(),
        };
        let lines = cell.render(80, None);
        // The chevron should appear on the content line (after the unstyled spacer).
        let chevron_line_idx = lines.iter().position(|l| {
            l.spans
                .iter()
                .any(|s| s.content.as_ref().contains(super::theme::CHEVRON))
        });
        assert!(chevron_line_idx.is_some(), "chevron must be present");
        let idx = chevron_line_idx.unwrap();
        assert!(idx > 0, "chevron should not be on the first (spacer) line");
    }

    #[test]
    fn render_assistant_streaming() {
        let cell = HistoryCell::Assistant {
            text: "partial".to_string(),
            streaming: true,
        };
        let lines = cell.render(40, None);
        // Last line should be the cursor block
        let last = &lines[lines.len() - 1];
        let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('▊'));
    }

    #[test]
    fn render_assistant_not_streaming() {
        let cell = HistoryCell::Assistant {
            text: "done".to_string(),
            streaming: false,
        };
        let lines = cell.render(40, None);
        // No trailing blank line; last line should contain the rendered text.
        let last = &lines[lines.len() - 1];
        let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("done"),
            "last line should contain rendered text, got: {text:?}"
        );
    }

    #[test]
    fn stream_continuation_classification() {
        let tool_start = HistoryCell::ToolStart {
            name: "run_shell".to_string(),
            args: "{}".to_string(),
            completed: false,
            start_time: None,
        };
        assert!(!tool_start.is_stream_continuation());

        let tool_result = HistoryCell::ToolResult {
            output: "ok".to_string(),
            is_error: false,
            duration_ms: Some(10),
            display_label: "run_shell".to_string(),
        };
        assert!(tool_result.is_stream_continuation());

        let tool_streaming = HistoryCell::ToolStreaming { lines: vec![] };
        assert!(tool_streaming.is_stream_continuation());

        let assistant = HistoryCell::Assistant {
            text: "hi".to_string(),
            streaming: false,
        };
        assert!(!assistant.is_stream_continuation());

        let user = HistoryCell::User {
            text: "hey".to_string(),
        };
        assert!(!user.is_stream_continuation());
    }

    #[test]
    fn render_tool_start_patch_summary() {
        let cell = HistoryCell::ToolStart {
            name: "apply_patch".to_string(),
            args: r#"{"patch":"*** Begin Patch\n*** Add File: a.rs\n+x\n*** Update File: b.rs\n@@\n*** End Patch"}"#.to_string(),
            completed: true,
            start_time: None,
        };
        let lines = cell.render(80, None);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            header.contains("Edited"),
            "header should say Edited, got: {header}"
        );
        // Detail line should show files
        assert!(lines.len() >= 2, "should have detail line");
        let detail: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            detail.contains("a.rs"),
            "detail should contain filenames, got: {detail}"
        );
        assert!(
            detail.contains("2 file(s)"),
            "detail should show count, got: {detail}"
        );
    }

    #[test]
    fn render_tool_start_run_shell() {
        let cell = HistoryCell::ToolStart {
            name: "run_shell".to_string(),
            args: r#"{"command":"cargo test --all"}"#.to_string(),
            completed: false,
            start_time: None,
        };
        let lines = cell.render(80, None);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Ran"), "should say Ran, got: {text}");
        assert!(
            text.contains("cargo test --all"),
            "should show command, got: {text}"
        );
    }

    #[test]
    fn render_tool_start_read_file() {
        let cell = HistoryCell::ToolStart {
            name: "read_file".to_string(),
            args: r#"{"path":"src/main.rs"}"#.to_string(),
            completed: true,
            start_time: None,
        };
        let lines = cell.render(80, None);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            header.contains("Explored"),
            "should say Explored, got: {header}"
        );
        assert!(lines.len() >= 2, "should have detail line");
        let detail: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(detail.contains("Read"), "detail should have Read label");
        assert!(detail.contains("main.rs"), "detail should have basename");
    }

    #[test]
    fn render_tool_start_generic_truncates() {
        let cell = HistoryCell::ToolStart {
            name: "custom_tool".to_string(),
            args: format!(r#"{{"data":"{}"}}"#, "a".repeat(100)),
            completed: false,
            start_time: None,
        };
        let lines = cell.render(80, None);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("custom_tool"),
            "should show tool name, got: {text}"
        );
        assert!(
            text.contains("..."),
            "should truncate long args, got: {text}"
        );
    }

    #[test]
    fn render_tool_result_truncates_output() {
        let output = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell = HistoryCell::ToolResult {
            output,
            is_error: false,
            duration_ms: Some(1500),
            display_label: "Ran test".to_string(),
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("+5 more lines"));
        assert!(all_text.contains("1.5s"));
    }

    #[test]
    fn render_tool_result_uses_display_label() {
        let cell = HistoryCell::ToolResult {
            output: "ok".to_string(),
            is_error: false,
            duration_ms: Some(200),
            display_label: "Ran `ls`".to_string(),
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all_text.contains("Ran `ls`"),
            "should use display_label, got: {all_text}"
        );
    }

    #[test]
    fn render_separator() {
        let cell = HistoryCell::Separator;
        let lines = cell.render(60, None);
        // Only the rule line; surrounding spacing is handled by the render loop.
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn render_thinking_empty() {
        let cell = HistoryCell::Thinking {
            text: String::new(),
        };
        let lines = cell.render(40, None);
        assert!(lines.is_empty());
    }

    #[test]
    fn render_thinking_content() {
        let cell = HistoryCell::Thinking {
            text: "pondering".to_string(),
        };
        let lines = cell.render(40, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("thinking"));
        assert!(all_text.contains("pondering"));
    }

    #[test]
    fn render_heartbeat() {
        let cell = HistoryCell::Heartbeat {
            text: "check-in".to_string(),
        };
        let lines = cell.render(40, None);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("[heartbeat]"));
        assert!(text.contains("check-in"));
    }

    #[test]
    fn render_plan_cell_with_steps() {
        let cell = HistoryCell::Plan {
            steps: vec![
                PlanStep {
                    title: "Read files".into(),
                    status: PlanStepStatus::Completed,
                },
                PlanStep {
                    title: "Write code".into(),
                    status: PlanStepStatus::InProgress,
                },
                PlanStep {
                    title: "Run tests".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("Plan:"));
        assert!(all_text.contains("Read files"));
        assert!(all_text.contains("Write code"));
        assert!(all_text.contains("Run tests"));
    }

    #[test]
    fn render_plan_cell_empty() {
        let cell = HistoryCell::Plan { steps: vec![] };
        let lines = cell.render(80, None);
        // Should have at least the "Plan:" header
        assert!(!lines.is_empty());
    }

    #[test]
    fn plan_cell_variant_exists() {
        let cell = HistoryCell::Plan {
            steps: vec![PlanStep {
                title: "A".into(),
                status: PlanStepStatus::Completed,
            }],
        };
        assert!(matches!(cell, HistoryCell::Plan { .. }));
    }

    #[test]
    fn render_tool_streaming_truncates() {
        let tool_lines: Vec<(String, bool)> = (0..12)
            .map(|i| (format!("output line {i}"), i % 3 == 0))
            .collect();
        let cell = HistoryCell::ToolStreaming { lines: tool_lines };
        let rendered = cell.render(80, None);
        let all_text: String = rendered
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // Should show "lines above" indicator for truncated lines
        assert!(all_text.contains("lines above"));
    }

    // -- style_system_line tests --

    #[test]
    fn style_system_line_header() {
        let line = style_system_line("Borg Doctor");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::header_style());
    }

    #[test]
    fn style_system_line_separator() {
        let line = style_system_line("───────────");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::dim());
    }

    #[test]
    fn style_system_line_check_icon() {
        let line = style_system_line("  ✓ sandbox enabled");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "✓");
        assert_eq!(line.spans[1].style, theme::icon_style());
    }

    #[test]
    fn style_system_line_warning_icon() {
        let line = style_system_line("  ⚠ updates available");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "⚠");
        assert_eq!(line.spans[1].style, theme::warning_style());
    }

    #[test]
    fn style_system_line_fail_icon() {
        let line = style_system_line("  ✗ provider missing");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "✗");
        assert_eq!(line.spans[1].style, theme::error_style());
    }

    #[test]
    fn style_system_line_history_you() {
        let line = style_system_line("[13:05] You: hello");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content.as_ref(), "[13:05] ");
        assert_eq!(line.spans[1].content.as_ref(), "You:");
        assert!(line.spans[1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn style_system_line_history_assistant() {
        let line = style_system_line("[13:05] Assistant: hi there");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "Assistant:");
        assert_eq!(line.spans[1].style, theme::header_style());
    }

    #[test]
    fn style_system_line_history_tool() {
        let line = style_system_line("[13:13] Tool (toolu_vr): Exit code: 0");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "Tool (toolu_vr):");
        assert_eq!(line.spans[1].style, theme::header_style());
    }

    #[test]
    fn style_system_line_help_command() {
        let line = style_system_line("  /help      - Show this help");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "/help");
        assert_eq!(line.spans[1].style, theme::header_style());
    }

    #[test]
    fn style_system_line_empty() {
        let line = style_system_line("");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::dim());
    }

    #[test]
    fn style_system_line_log_warn() {
        let line =
            style_system_line("2026-04-03T17:05:25.065853Z  WARN Failed to resolve credential");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].style, theme::header_style());
        assert!(line.spans[0].content.contains("WARN "));
        assert_eq!(line.spans[1].style, theme::dim());
    }

    #[test]
    fn style_system_line_log_error() {
        let line = style_system_line("2026-04-03T17:05:25.096265Z ERROR Gateway exited with error");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].style, theme::header_style());
        assert!(line.spans[0].content.contains("ERROR "));
        assert_eq!(line.spans[1].style, theme::dim());
    }

    #[test]
    fn style_system_line_plain_indented() {
        let line = style_system_line("  some body text");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::dim());
    }

    #[test]
    fn style_system_line_label_content() {
        let line = style_system_line("Session: 58 messages, ~11399 estimated tokens");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "Session:");
        assert_eq!(line.spans[0].style, theme::header_style());
        assert_eq!(line.spans[1].style, theme::dim());
    }

    #[test]
    fn style_system_line_xml_tag() {
        let line = style_system_line("<memory_file name=\"notes.md\">");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::header_style());
    }

    #[test]
    fn style_system_line_markdown_header() {
        let line = style_system_line("# My Memory Topic");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::header_style());
    }

    #[test]
    fn style_system_line_tool_listing() {
        let line = style_system_line("  write_memory       Write/append to memory files");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "write_memory");
        assert_eq!(line.spans[1].style.fg, Some(ratatui::style::Color::White));
        // Description stays dim — no bold/white on the slash in "Write/append"
        assert_eq!(line.spans[2].style, theme::dim());
    }

    #[test]
    fn style_system_line_prose_is_dim() {
        let line = style_system_line(
            "This is a longer line of regular prose text that should remain dim styled",
        );
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::dim());
    }
}
