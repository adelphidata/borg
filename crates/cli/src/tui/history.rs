use ansi_to_tui::IntoText;
use borg_core::types::{PlanStep, PlanStepStatus};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use throbber_widgets_tui::ThrobberState;

use super::diff_render;
use super::line_utils;
use super::line_utils::{prefix_lines, LIVE_PREFIX_COLS};
use super::markdown;
use super::theme;

/// Per-cell cache for the newline-gated streaming markdown collector. The
/// committed region (bytes up to the last newline at render time) is rendered
/// once per width change or newline advance; the pending tail is re-rendered
/// every frame.
#[derive(Default, Clone)]
pub(super) struct AssistantRenderCache {
    cached_width: Option<u16>,
    committed_lines: Vec<Line<'static>>,
    committed_byte_len: usize,
}

/// Parse a single line of tool output, honoring embedded ANSI escape
/// sequences (colors / bold / etc.) so output from `cargo`, `git`,
/// `pytest`, etc. retains its meaning. `fallback_style` is applied to
/// spans that don't carry an explicit foreground color, so plain output
/// still renders dim (or red, for errors).
///
/// On parse failure (malformed escape) falls back to a single dim/error
/// span and logs via `tracing::warn!` per the project error-handling
/// invariant.
fn parse_ansi_line(text: &str, fallback_style: Style) -> Line<'static> {
    match text.into_text() {
        Ok(parsed) => {
            // Callers split on `.lines()` so `\n` shouldn't reach us, but
            // ansi-to-tui can also break on `\r` / form-feed. If that
            // happens, log and merge — silently dropping content would hide
            // tool output.
            if parsed.lines.len() > 1 {
                tracing::warn!(
                    "parse_ansi_line: input produced {} lines, merging into one",
                    parsed.lines.len()
                );
            }
            let mut all_spans: Vec<ratatui::text::Span<'_>> = Vec::new();
            for l in parsed.lines {
                all_spans.extend(l.spans);
            }
            let spans: Vec<Span<'static>> = all_spans
                .into_iter()
                .map(|s| {
                    // ansi-to-tui emits `Color::Reset` for `\x1b[0m`. Treat
                    // Reset as "no explicit fg" so plain text after a reset
                    // still picks up the dim/error fallback.
                    let no_explicit_fg =
                        matches!(s.style.fg, None | Some(ratatui::style::Color::Reset));
                    let merged = if no_explicit_fg {
                        let mut patched = fallback_style.patch(s.style);
                        patched.fg = fallback_style.fg;
                        patched
                    } else {
                        s.style
                    };
                    // Defense-in-depth: ansi-to-tui parses SGR escapes but
                    // passes non-SGR control sequences (clear-screen, cursor
                    // moves, OSC) through to span text. Strip ESC bytes so a
                    // hostile tool can't repaint or wreck the transcript.
                    let safe: String = s.content.chars().filter(|&c| c != '\x1b').collect();
                    Span::styled(safe, merged)
                })
                .collect();
            if spans.is_empty() {
                let safe: String = text.chars().filter(|&c| c != '\x1b').collect();
                Line::from(Span::styled(safe, fallback_style))
            } else {
                Line::from(spans)
            }
        }
        Err(e) => {
            tracing::warn!("ansi-to-tui parse failed: {e}; falling back to plain text");
            let safe: String = text.chars().filter(|&c| c != '\x1b').collect();
            Line::from(Span::styled(safe, fallback_style))
        }
    }
}

/// Wrap pre-styled body lines in a `╭─╮ │ ╰─╯` box. Width is the full cell
/// width; the box itself sits inside a 2-space left indent so it visually
/// aligns with the rest of the tool result. Pre-existing line content is
/// padded to the inner width with spaces.
///
/// Used by both the `Thinking` cell and error `ToolResult` cells so the two
/// boxed shapes stay visually consistent.
fn render_boxed_lines(
    inner_lines: Vec<Line<'static>>,
    border_style: Style,
    label: Option<&str>,
    width: u16,
    indent: usize,
) -> Vec<Line<'static>> {
    // Geometry, every row totals exactly `width` columns:
    //   `{indent}{│}{ }{content of content_w cells}{│}` = indent + 3 + content_w
    //   `{indent}{╭}{──}{label}{─×top_rule_len}{╮}`     = indent + 4 + label_w + top_rule_len
    //   `{indent}{╰}{─×inner_w}{╯}`                      = indent + 2 + inner_w
    // where inner_w = width - indent - 2  and  content_w = inner_w - 1
    // (the leading space inside the box is for readability).
    let inner_w = (width as usize).saturating_sub(indent + 2).max(3);
    let content_w = inner_w.saturating_sub(1).max(1);
    let indent_str = " ".repeat(indent);
    let label = label.unwrap_or("");
    let label_w = unicode_width::UnicodeWidthStr::width(label);
    let top_rule_len = inner_w.saturating_sub(2 + label_w);
    let top = format!(
        "{indent_str}{}{}{label}{}{}",
        theme::BOX_TOP_LEFT,
        theme::SEPARATOR.repeat(2),
        theme::SEPARATOR.repeat(top_rule_len),
        theme::BOX_TOP_RIGHT,
    );
    let mut lines = vec![Line::from(Span::styled(top, border_style))];
    for line in inner_lines {
        // Truncate first so over-long inputs can't burst the box.
        let line = line_utils::truncate_line_to_width(line, content_w);
        let used = line_utils::line_width(&line);
        let pad = content_w.saturating_sub(used);
        let mut spans = vec![Span::styled(
            format!("{indent_str}{} ", theme::BOX_VERTICAL),
            border_style,
        )];
        spans.extend(line.spans);
        spans.push(Span::styled(
            format!("{}{}", " ".repeat(pad), theme::BOX_VERTICAL),
            border_style,
        ));
        lines.push(Line::from(spans));
    }
    let bottom = format!(
        "{indent_str}{}{}{}",
        theme::BOX_BOTTOM_LEFT,
        theme::SEPARATOR.repeat(inner_w),
        theme::BOX_BOTTOM_RIGHT,
    );
    lines.push(Line::from(Span::styled(bottom, border_style)));
    lines
}

/// Display *rows* of tool output shown before collapsing. Row-aware
/// (post-wrap) — a single logical line that wraps to many rows counts
/// against this budget, so a stray long URL can't blow past the preview.
/// Toggled with Ctrl+E.
pub const COLLAPSE_THRESHOLD: usize = 10;
/// Preview budget (display rows) when a tool result is collapsed.
pub const COLLAPSE_PREVIEW_LINES: usize = 8;

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
        cache: RefCell<AssistantRenderCache>,
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
        /// Name of the tool that produced this result. Enables contextual
        /// rendering (e.g. colored diff for `apply_patch`).
        tool_name: String,
        /// Raw args JSON from the originating tool call, when available.
        /// Populated from the matching `ToolStart` so render paths can pull
        /// in data that isn't in `output` (e.g. the patch text).
        args_json: Option<String>,
        /// When true, the rendered output is shown in collapsed form
        /// (first `COLLAPSE_PREVIEW_LINES` lines + "more" footer). Toggled
        /// by Ctrl+E in the App event handler.
        collapsed: bool,
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
    /// Pre-rendered styled lines — used by the shared Borg card so `/card`
    /// renders with the same teal styling as the startup header.
    Card {
        lines: Vec<Line<'static>>,
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
    /// One-line card for a client-side `@path` mention expansion.
    /// Emitted just before the assistant cell on each submit, one per
    /// mention in the user's message. Rendered dim with a tree-end prefix.
    MentionCard {
        label: String,
    },
    Separator,
}

/// Split a mention card label like `Read CLAUDE.md (275 lines)` into a
/// plain-weight `"Read "` prefix and a bold `"CLAUDE.md (275 lines)"`
/// tail. For labels without a clear verb split (`Skipped @foo (...)`),
/// the whole label stays in the prefix.
fn split_mention_label(label: &str) -> (&str, &str) {
    // Known verb prefixes that we want to keep dim (unbolded).
    for verb in &["Read ", "Listed directory "] {
        if let Some(rest) = label.strip_prefix(verb) {
            let split = label.len() - rest.len();
            return (&label[..split], &label[split..]);
        }
    }
    (label, "")
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
    /// Construct an `Assistant` cell with an empty render cache.
    pub fn assistant(text: String, streaming: bool) -> Self {
        HistoryCell::Assistant {
            text,
            streaming,
            cache: RefCell::new(AssistantRenderCache::default()),
        }
    }

    /// True when this cell should visually hug the previous cell
    /// (no blank-line spacer inserted before it by the render loop).
    pub fn is_stream_continuation(&self) -> bool {
        matches!(
            self,
            HistoryCell::ToolResult { .. } | HistoryCell::ToolStreaming { .. }
        )
    }

    /// Flip the collapsed state on a `ToolResult` cell. No-op for other cells.
    pub fn toggle_collapsed(&mut self) -> bool {
        if let HistoryCell::ToolResult { collapsed, .. } = self {
            *collapsed = !*collapsed;
            true
        } else {
            false
        }
    }

    /// True when this is a `ToolResult` whose body exceeds the collapse
    /// threshold (i.e. would benefit from expand/collapse).
    pub fn is_collapsible_result(&self) -> bool {
        match self {
            HistoryCell::ToolResult {
                output,
                tool_name,
                args_json,
                is_error,
                ..
            } => {
                // Row-aware estimate at an assumed 80-col viewport. Width isn't
                // known here, so use a conservative default so Ctrl+E activates
                // whenever the body would plausibly spill past the preview.
                const ASSUMED_WIDTH: usize = 80;
                let content_width = ASSUMED_WIDTH.saturating_sub(4).max(1);
                let source = if !*is_error && tool_name == "apply_patch" {
                    args_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .and_then(|v| v.get("patch").and_then(|p| p.as_str()).map(str::to_string))
                        .unwrap_or_else(|| output.clone())
                } else {
                    output.clone()
                };
                let rows: usize = source
                    .lines()
                    .map(|l| {
                        let w = unicode_width::UnicodeWidthStr::width(l);
                        if w == 0 {
                            1
                        } else {
                            w.div_ceil(content_width)
                        }
                    })
                    .sum();
                rows > COLLAPSE_THRESHOLD
            }
            _ => false,
        }
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
                // Wrap each logical line at (width - 2) so the "> " / "  " prefix
                // column stays intact. Continuation lines hang under column 2,
                // matching how assistant messages align under their bullet.
                let wrap_width = w.saturating_sub(2).max(20);
                let mut first = true;
                for line in text.lines() {
                    let wrapped = if line.is_empty() {
                        vec![std::borrow::Cow::Borrowed("")]
                    } else {
                        super::wrapping::wrap(line, wrap_width)
                    };
                    for wl in wrapped.iter() {
                        let prefix = if first {
                            first = false;
                            Span::styled(format!("{} ", theme::CHEVRON), prefix_style)
                        } else {
                            Span::styled("  ", bg)
                        };
                        let spans = parse_at_mentions(wl.as_ref(), bg, mention_style);
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
                }
                lines
            }
            HistoryCell::Assistant {
                text,
                streaming,
                cache,
            } => {
                let inner_width = width.saturating_sub(LIVE_PREFIX_COLS as u16);
                let commit_end = text
                    .as_bytes()
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);

                let mut cache = cache.borrow_mut();
                let cache_hit = cache.cached_width == Some(inner_width)
                    && cache.committed_byte_len == commit_end;
                if !cache_hit {
                    cache.committed_lines = if commit_end == 0 {
                        Vec::new()
                    } else {
                        markdown::render_markdown(&text[..commit_end], inner_width)
                    };
                    cache.committed_byte_len = commit_end;
                    cache.cached_width = Some(inner_width);
                }
                let pending_lines = if commit_end < text.len() {
                    markdown::render_markdown(&text[commit_end..], inner_width)
                } else {
                    Vec::new()
                };

                let prefix_span = Span::styled(format!("{} ", theme::BULLET), theme::dim());
                let all: Vec<Line<'static>> = cache
                    .committed_lines
                    .iter()
                    .cloned()
                    .chain(pending_lines)
                    .collect();
                let mut lines = if text.is_empty() && *streaming {
                    vec![]
                } else if all.is_empty() {
                    vec![Line::from(prefix_span)]
                } else {
                    prefix_lines(all, prefix_span, Span::raw("  "))
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
                tool_name,
                args_json,
                collapsed,
            } => {
                let mut lines: Vec<Line<'static>> = Vec::new();

                // Contextual: apply_patch renders the raw patch as a colored diff
                // instead of the plain success-text output. Only on success —
                // errors show the usual dim preview so the error text is visible.
                let rendered_body = if !*is_error
                    && tool_name == "apply_patch"
                    && args_json.is_some()
                {
                    args_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .and_then(|v| v.get("patch").and_then(|p| p.as_str()).map(str::to_string))
                        .map(|patch| diff_render::render_patch(&patch))
                } else {
                    None
                };

                let (body_lines, body_style) = match rendered_body {
                    Some(diff_lines) => (diff_lines, theme::dim()),
                    None => {
                        let style = if *is_error {
                            theme::error_style()
                        } else {
                            theme::dim()
                        };
                        let hard_cap = (width as usize).saturating_sub(6).max(80);
                        // If the full output is pretty-printed JSON, collapse to a
                        // compact form first so `textwrap` has `: ` / `, ` break
                        // points and doesn't have to hard-split inside objects.
                        let owned;
                        let source: &str = if !*is_error {
                            if let Some(compact) = line_utils::format_json_compact(output) {
                                owned = compact;
                                &owned
                            } else {
                                output
                            }
                        } else {
                            output
                        };
                        let raw_lines: Vec<Line<'static>> = source
                            .lines()
                            .map(|l| {
                                let parsed = parse_ansi_line(l, style);
                                line_utils::truncate_line_with_ellipsis(parsed, hard_cap)
                            })
                            .collect();
                        (raw_lines, style)
                    }
                };

                // Errors render boxed so failures stand out from successful
                // tool calls. Skip the tree-end body prefix path entirely;
                // the `✗ display_label` status line below the box keeps the
                // visual link to the tool call.
                if *is_error {
                    let boxed = render_boxed_lines(
                        body_lines,
                        theme::error_style(),
                        Some(" error "),
                        width,
                        2,
                    );
                    lines.extend(boxed);
                    let (indicator, ind_style) = (theme::CROSS, theme::cross_style());
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
                    return lines;
                }

                // Row-aware collapse: measure each logical line's post-wrap row
                // cost so one stray long URL can't burst past the preview.
                let content_width = (width as usize).saturating_sub(4).max(1);
                let row_cost = |line: &Line<'_>| -> usize {
                    let w = line_utils::line_width(line);
                    if w == 0 {
                        1
                    } else {
                        w.div_ceil(content_width)
                    }
                };
                let total_rows: usize = body_lines.iter().map(|l| row_cost(l)).sum();
                let should_collapse = *collapsed && total_rows > COLLAPSE_THRESHOLD;

                let (taken, rows_used) = if should_collapse {
                    let mut used = 0usize;
                    let mut n = 0usize;
                    for line in body_lines.iter() {
                        let cost = row_cost(line);
                        if used + cost > COLLAPSE_PREVIEW_LINES && n > 0 {
                            break;
                        }
                        used += cost;
                        n += 1;
                        if used >= COLLAPSE_PREVIEW_LINES {
                            break;
                        }
                    }
                    (n, used)
                } else {
                    (body_lines.len(), total_rows)
                };

                for (i, line) in body_lines.into_iter().take(taken).enumerate() {
                    let prefix = if i == 0 {
                        format!("  {} ", theme::TREE_END)
                    } else {
                        "    ".to_string()
                    };
                    let mut spans = vec![Span::styled(prefix, body_style)];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }

                if should_collapse {
                    let extra = total_rows.saturating_sub(rows_used);
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    {} +{extra} more rows — Ctrl+E to expand",
                            theme::ELLIPSIS
                        ),
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
            HistoryCell::Card { lines } => lines.clone(),
            HistoryCell::Thinking { text } => {
                // Don't render an empty thinking box
                if text.is_empty() {
                    return vec![];
                }
                let border = theme::thinking_border_style();
                let content_style = theme::reasoning_style();
                let inner_w = (width as usize).saturating_sub(2).max(1);
                let inner_lines: Vec<Line<'static>> = text
                    .lines()
                    .map(|l| {
                        let display = line_utils::truncate_str_to_width(l, inner_w);
                        Line::from(Span::styled(display.to_string(), content_style))
                    })
                    .collect();
                render_boxed_lines(inner_lines, border, Some(" thinking "), width, 0)
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
                let hard_cap = (width as usize).saturating_sub(6).max(80);
                for (line_text, is_stderr) in tool_lines.iter().skip(skip) {
                    let style = if *is_stderr {
                        theme::error_style()
                    } else {
                        theme::dim()
                    };
                    let parsed = parse_ansi_line(line_text, style);
                    let truncated = line_utils::truncate_line_with_ellipsis(parsed, hard_cap);
                    let prefix = if *is_stderr { "! " } else { "\u{2502} " };
                    let mut spans = vec![Span::styled(format!("  {prefix}"), style)];
                    spans.extend(truncated.spans);
                    rendered.push(Line::from(spans));
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
            HistoryCell::MentionCard { label } => {
                // Matches the reference screenshot format: dim `⌞ Read
                // CLAUDE.md (275 lines)` / `⌞ Listed directory foo/`.
                // The filename/path portion after the verb is bolded.
                let (verb, rest) = split_mention_label(label);
                let mut spans = vec![Span::styled(
                    format!("  {} ", theme::TREE_END),
                    theme::dim(),
                )];
                spans.push(Span::styled(verb.to_string(), theme::dim()));
                if !rest.is_empty() {
                    spans.push(Span::styled(
                        rest.to_string(),
                        theme::dim().add_modifier(ratatui::style::Modifier::BOLD),
                    ));
                }
                vec![Line::from(spans)]
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
    fn render_user_cell_wraps_long_line_with_hanging_indent() {
        // At width 30, a long single-line user message must wrap; continuation
        // lines start with the 2-space bg-styled prefix (under the chevron's
        // content column) instead of overflowing the terminal.
        let cell = HistoryCell::User {
            text: "alpha beta gamma delta epsilon zeta eta theta iota".to_string(),
        };
        let lines = cell.render(30, None);
        // Find first line with chevron and first continuation after it.
        let chev_idx = lines
            .iter()
            .position(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.as_ref().contains(super::theme::CHEVRON))
            })
            .expect("chevron line");
        assert!(lines.len() > chev_idx + 1, "expected wrap, got {:?}", lines);
        let cont = &lines[chev_idx + 1];
        // First span is the "  " continuation prefix, not another chevron.
        assert_eq!(cont.spans[0].content.as_ref(), "  ");
        assert!(!cont
            .spans
            .iter()
            .any(|s| s.content.as_ref().contains(super::theme::CHEVRON)));
    }

    #[test]
    fn render_assistant_streaming() {
        let cell = HistoryCell::assistant("partial".to_string(), true);
        let lines = cell.render(40, None);
        // Last line should be the cursor block
        let last = &lines[lines.len() - 1];
        let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('▊'));
    }

    #[test]
    fn render_assistant_not_streaming() {
        let cell = HistoryCell::assistant("done".to_string(), false);
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
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
        };
        assert!(tool_result.is_stream_continuation());

        let tool_streaming = HistoryCell::ToolStreaming { lines: vec![] };
        assert!(tool_streaming.is_stream_continuation());

        let assistant = HistoryCell::assistant("hi".to_string(), false);
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
    fn render_tool_result_collapsed_shows_footer() {
        let output = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell = HistoryCell::ToolResult {
            output,
            is_error: false,
            duration_ms: Some(1500),
            display_label: "Ran test".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: true,
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // 20 lines total, 8 preview → 12 hidden
        assert!(all_text.contains("+12 more rows"), "got: {all_text}");
        assert!(all_text.contains("Ctrl+E"), "footer should mention Ctrl+E");
        assert!(all_text.contains("1.5s"));
    }

    #[test]
    fn render_tool_result_expanded_shows_all_lines() {
        let output = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell = HistoryCell::ToolResult {
            output,
            is_error: false,
            duration_ms: Some(1500),
            display_label: "Ran test".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!all_text.contains("more rows"), "should not collapse");
        // All 20 lines should appear
        assert!(all_text.contains("line 0"));
        assert!(all_text.contains("line 19"));
    }

    #[test]
    fn render_tool_result_short_output_not_collapsed() {
        // Under threshold — no footer even with collapsed=true
        let cell = HistoryCell::ToolResult {
            output: "one\ntwo\nthree".to_string(),
            is_error: false,
            duration_ms: Some(100),
            display_label: "Ran".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: true,
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!all_text.contains("more rows"));
    }

    #[test]
    fn toggle_collapsed_flips_state() {
        let mut cell = HistoryCell::ToolResult {
            output: (0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            duration_ms: None,
            display_label: "ran".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: true,
        };
        assert!(cell.toggle_collapsed());
        // Should no longer show "more rows" footer
        let all_text: String = cell
            .render(80, None)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!all_text.contains("more rows"));
        // Toggle back
        assert!(cell.toggle_collapsed());
        let all_text2: String = cell
            .render(80, None)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text2.contains("more rows"));
    }

    #[test]
    fn toggle_collapsed_noop_on_non_result() {
        let mut cell = HistoryCell::Separator;
        assert!(!cell.toggle_collapsed());
    }

    #[test]
    fn is_collapsible_result_true_when_over_threshold() {
        let cell = HistoryCell::ToolResult {
            output: (0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            duration_ms: None,
            display_label: "ran".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: true,
        };
        assert!(cell.is_collapsible_result());
    }

    #[test]
    fn is_collapsible_result_false_when_under_threshold() {
        let cell = HistoryCell::ToolResult {
            output: "one\ntwo".to_string(),
            is_error: false,
            duration_ms: None,
            display_label: "ran".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
        };
        assert!(!cell.is_collapsible_result());
    }

    #[test]
    fn render_tool_result_apply_patch_uses_diff() {
        let patch = "*** Begin Patch\n*** Add File: x.rs\n+fn foo() {}\n*** End Patch";
        let args_json = serde_json::json!({ "patch": patch }).to_string();
        let cell = HistoryCell::ToolResult {
            output: "Patch applied successfully.".to_string(),
            is_error: false,
            duration_ms: Some(50),
            display_label: "Edited 1 file".to_string(),
            tool_name: "apply_patch".to_string(),
            args_json: Some(args_json),
            collapsed: false,
        };
        let lines = cell.render(80, None);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // Diff header + body rendered from args, NOT the output text
        assert!(
            all_text.contains("Add:"),
            "expected diff header in: {all_text}"
        );
        assert!(all_text.contains("fn foo()"));
    }

    #[test]
    fn render_tool_result_apply_patch_error_falls_back_to_output() {
        let cell = HistoryCell::ToolResult {
            output: "Error: patch rejected at line 3".to_string(),
            is_error: true,
            duration_ms: Some(10),
            display_label: "Edited".to_string(),
            tool_name: "apply_patch".to_string(),
            args_json: Some(r#"{"patch":"*** Begin Patch\n*** End Patch"}"#.to_string()),
            collapsed: false,
        };
        let all_text: String = cell
            .render(80, None)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // Error should keep the raw output visible, not swap for an empty diff
        assert!(all_text.contains("Error: patch rejected"));
    }

    #[test]
    fn render_tool_result_uses_display_label() {
        let cell = HistoryCell::ToolResult {
            output: "ok".to_string(),
            is_error: false,
            duration_ms: Some(200),
            display_label: "Ran `ls`".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
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
    fn parse_ansi_line_extracts_colors() {
        // Red SGR around "error", reset, then plain text.
        let raw = "\x1b[31merror\x1b[0m: failed";
        let line = parse_ansi_line(raw, theme::dim());
        // Should produce ≥2 spans with the first one carrying red fg.
        assert!(line.spans.len() >= 2, "spans: {:?}", line.spans);
        let red_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("error"))
            .expect("error span");
        // Must have an explicit fg that is NOT the Reset sentinel — that's
        // the contract the dim-fallback merging relies on.
        assert!(
            red_span.style.fg.is_some() && red_span.style.fg != Some(ratatui::style::Color::Reset),
            "expected explicit non-Reset fg on the colored span, got: {:?}",
            red_span.style
        );
        // Trailing plain text should fall back to dim.
        let plain = line
            .spans
            .iter()
            .find(|s| s.content.contains("failed"))
            .expect("plain span");
        assert_eq!(plain.style.fg, theme::dim().fg);
    }

    #[test]
    fn parse_ansi_line_strips_non_sgr_escapes() {
        // Hostile sequence: clear-screen + cursor-home embedded in output.
        // Must not survive into span text or it'll repaint the terminal.
        let raw = "safe\x1b[2J\x1b[Hhostile";
        let line = parse_ansi_line(raw, theme::dim());
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!joined.contains('\x1b'), "ESC byte leaked: {joined:?}");
        assert!(joined.contains("safe"));
        assert!(joined.contains("hostile"));
    }

    #[test]
    fn parse_ansi_line_no_escapes_uses_fallback() {
        let line = parse_ansi_line("plain output", theme::error_style());
        let all: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(all, "plain output");
        assert_eq!(line.spans[0].style.fg, theme::error_style().fg);
    }

    #[test]
    fn render_tool_result_error_is_boxed() {
        let cell = HistoryCell::ToolResult {
            output: "boom".to_string(),
            is_error: true,
            duration_ms: Some(50),
            display_label: "Ran `false`".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
        };
        let lines = cell.render(40, None);
        // First line should be the box top, last meaningful body row uses │,
        // bottom rule precedes the status line.
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains(theme::BOX_TOP_LEFT), "first: {first}");
        assert!(first.contains(theme::BOX_TOP_RIGHT));
        let has_vertical = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains(theme::BOX_VERTICAL))
        });
        assert!(has_vertical, "expected box side glyphs in body");
        let has_bottom = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains(theme::BOX_BOTTOM_LEFT))
        });
        assert!(has_bottom);
        // Status line still shows the cross + label.
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all.contains(theme::CROSS));
        assert!(all.contains("Ran `false`"));
    }

    #[test]
    fn render_boxed_lines_all_rows_equal_width() {
        // Regression for the off-by-one width-math bug in `render_boxed_lines`:
        // top, body, and bottom rows must all render at exactly `width` cells
        // so the box rules align vertically. Tests indent=0 (Thinking) and
        // indent=2 (boxed error ToolResult).
        for indent in [0usize, 2] {
            let inner = vec![
                Line::from(Span::raw("hi")),
                Line::from(Span::raw("longer body line here")),
                Line::from(Span::raw("")),
            ];
            for width in [40u16, 60, 80] {
                let lines = render_boxed_lines(
                    inner.clone(),
                    theme::error_style(),
                    Some(" error "),
                    width,
                    indent,
                );
                let widths: Vec<usize> = lines.iter().map(line_utils::line_width).collect();
                let target = width as usize;
                assert!(
                    widths.iter().all(|w| *w == target),
                    "width={width} indent={indent} → row widths {widths:?} should all equal {target}"
                );
            }
        }
    }

    #[test]
    fn render_tool_result_success_not_boxed() {
        // Sanity: success path still uses the tree-end prefix, not a box.
        let cell = HistoryCell::ToolResult {
            output: "ok".to_string(),
            is_error: false,
            duration_ms: Some(10),
            display_label: "Ran".to_string(),
            tool_name: "run_shell".to_string(),
            args_json: None,
            collapsed: false,
        };
        let lines = cell.render(40, None);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!first.contains(theme::BOX_TOP_LEFT));
        assert!(first.contains(theme::TREE_END));
    }

    #[test]
    fn style_system_line_prose_is_dim() {
        let line = style_system_line(
            "This is a longer line of regular prose text that should remain dim styled",
        );
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, theme::dim());
    }

    // --- Streaming markdown collector tests (newline-gated cache) ---

    fn assistant_cache_snapshot(cell: &HistoryCell) -> (Option<u16>, usize, usize) {
        match cell {
            HistoryCell::Assistant { cache, .. } => {
                let c = cache.borrow();
                (
                    c.cached_width,
                    c.committed_byte_len,
                    c.committed_lines.len(),
                )
            }
            _ => panic!("expected Assistant"),
        }
    }

    fn flatten_plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn streaming_pending_without_newline_is_not_committed() {
        let cell = HistoryCell::assistant("Hello, world".to_string(), true);
        let _ = cell.render(40, None);
        let (w, committed_len, committed_count) = assistant_cache_snapshot(&cell);
        assert_eq!(committed_len, 0, "no newline => nothing committed");
        assert_eq!(committed_count, 0);
        assert_eq!(w, Some(38), "width - LIVE_PREFIX_COLS");
    }

    #[test]
    fn streaming_newline_commits_and_second_render_hits_cache() {
        let cell = HistoryCell::assistant("Hello!\n".to_string(), true);
        let first = cell.render(40, None);
        let (_, committed_len, committed_count) = assistant_cache_snapshot(&cell);
        assert_eq!(committed_len, 7, "committed_byte_len == last newline + 1");
        assert!(committed_count >= 1);

        // Second render at same width: cache hit (no panic, equivalent output).
        let second = cell.render(40, None);
        assert_eq!(flatten_plain(&first), flatten_plain(&second));
        let (_, committed_len2, _) = assistant_cache_snapshot(&cell);
        assert_eq!(committed_len2, 7);
    }

    #[test]
    fn streaming_width_change_invalidates_cache() {
        let long = "This is a long line that should wrap across different widths differently to make the cache width-sensitive.\n".to_string();
        let cell = HistoryCell::assistant(long, true);
        let _ = cell.render(40, None);
        let (w1, _, _) = assistant_cache_snapshot(&cell);
        assert_eq!(w1, Some(38));

        let _ = cell.render(60, None);
        let (w2, _, _) = assistant_cache_snapshot(&cell);
        assert_eq!(w2, Some(58), "cache width reflects new render width");
    }

    #[test]
    fn streaming_mid_fence_pending_renders_without_panic() {
        // Open fence with language tag, content line, no closing fence.
        // The only newline is after "rust", so commit_end == 8; the pending
        // tail "fn main() {" is rendered per-frame.
        let cell = HistoryCell::assistant("```rust\nfn main() {".to_string(), true);
        let lines = cell.render(40, None);
        let (_, committed_len, _) = assistant_cache_snapshot(&cell);
        assert_eq!(committed_len, 8, "committed up to newline after ```rust");
        let rendered = flatten_plain(&lines);
        assert!(
            rendered.contains("fn main"),
            "pending tail should be rendered each frame, got: {rendered:?}"
        );
    }

    #[test]
    fn streaming_finalize_flushes_pending_without_cursor() {
        let cell = HistoryCell::assistant("Line A\nLine B".to_string(), false);
        let lines = cell.render(40, None);
        let rendered = flatten_plain(&lines);
        assert!(rendered.contains("Line A"));
        assert!(rendered.contains("Line B"));
        assert!(
            !rendered.contains('▊'),
            "no streaming cursor when streaming=false"
        );
    }

    #[test]
    fn streaming_empty_while_streaming_renders_cursor_only() {
        let cell = HistoryCell::assistant(String::new(), true);
        let lines = cell.render(40, None);
        assert_eq!(lines.len(), 1, "empty streaming cell => cursor line only");
        let rendered = flatten_plain(&lines);
        assert_eq!(rendered, "▊");
    }
}
