use ansi_to_tui::IntoText;
use cortex::output_redaction::{current_policy, OutputSink};
use cortex::types::{PlanStep, PlanStepStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use throbber_widgets_tui::ThrobberState;

use super::constants::{
    ASSISTANT_COLLAPSE_THRESHOLD, ASSISTANT_PREVIEW_LINES, COLLAPSE_PREVIEW_LINES,
    COLLAPSE_THRESHOLD, SHELL_PREVIEW_LINES,
};
use super::diff_render;
use super::line_utils;
use super::line_utils::{prefix_lines, LIVE_PREFIX_COLS};
use super::markdown;
use super::theme;

/// Columns reserved for the tool/shell body gutter (indent + trailing slack).
const BODY_GUTTER_COLS: usize = 6;

/// Floor for the body wrap cap. Below this a narrow terminal would hard-split
/// nearly every line mid-token, so we let the body overflow instead — the
/// terminal's own wrapping degrades more gracefully than character chopping.
const BODY_MIN_WRAP_COLS: usize = 80;

/// Hard wrap cap for tool and shell output bodies.
fn body_hard_cap(width: u16) -> usize {
    (width as usize)
        .saturating_sub(BODY_GUTTER_COLS)
        .max(BODY_MIN_WRAP_COLS)
}

/// Footer line shown under a collapsed tool result, nudging toward the
/// Ctrl+T transcript pager for the hidden `extra` rows.
fn collapse_footer(extra: usize) -> String {
    format!(
        "    {} +{extra} lines (ctrl + t to view transcript)",
        theme::ELLIPSIS
    )
}

/// Collapse a finalized assistant cell's rendered lines into
/// `head + ellipsis + tail` when the body exceeds the threshold. Streaming
/// cells skip this so the user sees live tokens in full.
fn collapse_long_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() <= ASSISTANT_COLLAPSE_THRESHOLD {
        return lines;
    }
    let total = lines.len();
    let omitted = total - 2 * ASSISTANT_PREVIEW_LINES;
    let style = theme::dim().add_modifier(Modifier::ITALIC);
    let ellipsis = Line::from(Span::styled(
        format!("  … +{omitted} lines (Ctrl+T to view transcript)"),
        style,
    ));
    let mut out = Vec::with_capacity(ASSISTANT_COLLAPSE_THRESHOLD + 1);
    out.extend(lines.iter().take(ASSISTANT_PREVIEW_LINES).cloned());
    out.push(ellipsis);
    out.extend(lines.iter().skip(total - ASSISTANT_PREVIEW_LINES).cloned());
    out
}

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

/// Convert every line ending in pasted text to a single `\n`.
///
/// Terminals disagree on what a pasted line break looks like: some send `\r\n`,
/// some send a bare `\r`. [`sanitize_user_text`] drops `\r` outright, which is
/// right when rendering a stored message but wrong at the paste boundary — a
/// bare `\r` would silently weld two pasted lines together. Normalizing first
/// means `\r\n` collapses to one `\n` (not two) and a lone `\r` survives as a
/// line break.
///
/// Returns a borrow when there is no `\r` to rewrite, which is the common case.
pub(super) fn normalize_pasted_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    // `\r\n` before bare `\r`, or the pair would become two line breaks.
    std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Strip terminal control sequences from user-authored text so a pasted or
/// persisted CSI sequence can't repaint the terminal or corrupt native
/// scrollback. Removes CSI sequences (`\x1b[` … final byte in `@..=~`) and any
/// other non-whitespace control characters, while preserving `\n` and `\t`.
///
/// Returns a borrow when the text needs no changes (the common case — this runs
/// on every render of a user message), allocating only when something is
/// actually stripped.
///
/// Tool output is already guarded (see `parse_ansi_line`); this closes the same
/// hole on the user-message path, which is otherwise rendered verbatim.
pub(super) fn sanitize_user_text(text: &str) -> std::borrow::Cow<'_, str> {
    // Fast path: nothing to strip → borrow without allocating. The predicate is
    // exactly the set the loop below removes (ESC or a non-`\n`/`\t` control).
    if !text
        .chars()
        .any(|c| c == '\x1b' || (c.is_control() && c != '\n' && c != '\t'))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // For a CSI (`ESC [`), consume only valid parameter/intermediate
            // bytes (`0x20..=0x3f`) followed by a single final byte
            // (`0x40..=0x7e`). Stop at the first non-CSI byte *without*
            // consuming it, so a truncated or malformed sequence can't swallow
            // the printable text that follows. Any other ESC form (a lone ESC)
            // is simply dropped.
            if chars.next_if_eq(&'[').is_some() {
                while chars
                    .next_if(|c| ('\u{20}'..='\u{3f}').contains(c))
                    .is_some()
                {}
                chars.next_if(|c| ('@'..='~').contains(c));
            }
        } else if matches!(ch, '\n' | '\t') || !ch.is_control() {
            out.push(ch);
        }
        // Any other control char (lone ESC, `\r`, `\x08`, C1, …) is dropped.
    }
    std::borrow::Cow::Owned(out)
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

/// Build the amber warning card shown at startup when the daemon reports
/// persistent doctor warnings. Returns an empty vec for no warnings (the
/// render arm collapses to a no-op cell). `warnings` is a list of
/// `category:name` payloads as emitted by the daemon's `WarningList`.
///
/// The card lists the *categories* affected — deduplicated, first-seen
/// order, max 3 shown then `and N more` — rather than the raw IDs. This is
/// designed to be a one-line scan: "you have issues over here, here, and
/// here; run `/doctor` if you want the detail." Detail lives in the doctor
/// command, not the startup card.
pub(super) fn build_doctor_warning_lines(warnings: &[String], width: u16) -> Vec<Line<'static>> {
    if warnings.is_empty() {
        return Vec::new();
    }

    // Extract the category prefix (text before the first `:`); fall back to
    // the whole string for malformed entries. Dedupe preserving first-seen
    // order so a category with N failing checks appears once.
    let mut categories: Vec<String> = Vec::new();
    for w in warnings {
        let cat = match w.split_once(':') {
            Some((c, _)) => c.trim().to_string(),
            None => w.trim().to_string(),
        };
        if cat.is_empty() {
            continue;
        }
        if !categories.iter().any(|c| c == &cat) {
            categories.push(cat);
        }
    }
    if categories.is_empty() {
        return Vec::new();
    }

    const MAX_SHOWN: usize = 3;
    let total_categories = categories.len();
    let shown = total_categories.min(MAX_SHOWN);
    let extra = total_categories - shown;

    // Build the summary line: dim "Issues with " + bold-white categories
    // separated by dim commas + dim period (or "and N more.").
    let mut summary: Vec<Span<'static>> = Vec::new();
    let lead = if total_categories == 1 {
        "Issue with "
    } else {
        "Issues with "
    };
    summary.push(Span::styled(lead.to_string(), theme::dim()));
    for (i, cat) in categories.iter().take(shown).enumerate() {
        if i > 0 {
            // Oxford-comma style: ", " between, ", and " before the last
            // when there's no overflow tail. With overflow, the tail uses
            // ", and N more" so each gap is a plain ", ".
            let sep = if extra == 0 && i == shown - 1 {
                if shown == 2 {
                    " and "
                } else {
                    ", and "
                }
            } else {
                ", "
            };
            summary.push(Span::styled(sep.to_string(), theme::dim()));
        }
        summary.push(Span::styled(cat.clone(), theme::header_style()));
    }
    if extra > 0 {
        summary.push(Span::styled(format!(", and {extra} more"), theme::dim()));
    }
    summary.push(Span::styled(".".to_string(), theme::dim()));

    // Footer with `/doctor` highlighted.
    let footer = Line::from(vec![
        Span::styled("Run ".to_string(), theme::dim()),
        Span::styled("/doctor".to_string(), theme::header_style()),
        Span::styled(" for more details.".to_string(), theme::dim()),
    ]);

    let inner = vec![Line::from(summary), Line::default(), footer];

    let total_warnings = warnings.len();
    let label = if total_warnings == 1 {
        " ⚠ 1 Warning ".to_string()
    } else {
        format!(" ⚠ {total_warnings} Warnings ")
    };
    render_boxed_lines(inner, theme::warning_style(), Some(&label), width, 0)
}

// Collapse / preview thresholds live in `super::constants`; imported above.

/// Parsed view of `run_shell`'s success-path output envelope:
/// `"Exit code: {n}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"`.
struct ShellOutput<'a> {
    exit_code: &'a str,
    stdout: &'a str,
    stderr: &'a str,
}

/// Split the `run_shell` envelope into its three pieces. Returns `None`
/// if any sentinel is missing so callers can fall back to the generic
/// body renderer instead of producing a misleading layout.
fn parse_shell_output(raw: &str) -> Option<ShellOutput<'_>> {
    let after_prefix = raw.strip_prefix("Exit code: ")?;
    let (exit_code, rest) = after_prefix.split_once("\n--- stdout ---\n")?;
    let (stdout, stderr) = rest.split_once("\n--- stderr ---\n")?;
    Some(ShellOutput {
        exit_code,
        stdout,
        stderr,
    })
}

fn strip_reply_directive_prefix(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("[[reply_to_current]]") {
        return rest.trim_start_matches([' ', '\n', '\r']);
    }

    if let Some(rest) = text.strip_prefix("[[reply_to:") {
        if let Some(end) = rest.find("]]") {
            return rest[end + 2..].trim_start_matches([' ', '\n', '\r']);
        }
        return "";
    }

    if text.starts_with("[[reply_to_current") || text.starts_with("[[reply_to:") {
        return "";
    }

    text
}

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
    /// Amber warning card shown at startup when the daemon reports
    /// persistent doctor warnings. Lines are computed on each render so the
    /// box matches the current viewport width — unlike `Card`, which is
    /// baked once. Each `String` is a `category:name` payload from the
    /// daemon's `WarningList`.
    DoctorWarning {
        warnings: Vec<String>,
    },
    Thinking {
        text: String,
    },
    ToolStreaming {
        lines: Vec<(String, bool)>,
        /// Lines already evicted from the front of `lines` to honor
        /// [`super::constants::TOOL_STREAM_MAX_LINES`]; folded into the
        /// "(N lines above)" notice so the elision stays visible.
        dropped: u64,
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
    /// Turn divider rendered as a full-width dim rule. When `label` is set it
    /// renders as `─ {label} ─` padded to the viewport width;
    /// otherwise a plain full-width rule. Emitted after turns that did concrete
    /// work (tool calls / edits).
    Separator {
        label: Option<String>,
    },
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

/// Style paired-backtick runs in `text` with the inline-code colour; surrounding prose stays at `base`. Unmatched trailing backticks pass through unstyled.
fn style_inline_backticks(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let (before, after_open) = rest.split_at(open);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), base));
        }
        let after = &after_open[1..];
        if let Some(close) = after.find('`') {
            let code = &after[..close];
            spans.push(Span::styled(format!("`{code}`"), theme::code_style()));
            rest = &after[close + 1..];
        } else {
            spans.push(Span::styled(format!("`{after}"), base));
            return spans;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base));
    }
    spans
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
    if trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && trimmed.get(..20).is_some_and(|p| p.contains('T'))
    {
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

/// Render a parsed `run_shell` result in a compact two-line layout:
/// `└ Exit code: N`, then per-stream `--- stdout ---` / `--- stderr ---`
/// sections (omitted when the stream is empty), each truncated to
/// `SHELL_PREVIEW_LINES` post-wrap rows when `collapsed` is true,
/// followed by the standard `✓ <label> • Ns` status row.
fn render_shell_result(
    parsed: ShellOutput<'_>,
    display_label: &str,
    duration_ms: Option<u64>,
    collapsed: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let dim = theme::dim();
    let hard_cap = body_hard_cap(width);
    let content_width = (width as usize).saturating_sub(4).max(1);
    let row_cost = |line: &Line<'_>| -> usize {
        let w = line_utils::line_width(line);
        if w == 0 {
            1
        } else {
            w.div_ceil(content_width)
        }
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // └ Exit code: N — matches the styling of the `└ $ cmd` detail row
    // emitted by `tool_detail_line` for ToolStart, so the two halves of
    // the call read as a single block.
    lines.push(Line::from(vec![
        Span::styled(format!("  {} ", theme::TREE_END), dim),
        Span::styled(format!("Exit code: {}", parsed.exit_code), dim),
    ]));

    // Section: lays down a `--- {label} ---` header, then up to
    // `SHELL_PREVIEW_LINES` post-wrap rows when collapsed, plus a
    // `… +N lines (ctrl + t to view transcript)` footer if anything
    // was elided. Empty streams render nothing.
    let mut push_section = |label: &str, body: &str| {
        let trimmed = body.trim_end_matches('\n');
        if trimmed.is_empty() {
            return;
        }
        lines.push(Line::from(Span::styled(
            format!("    --- {label} ---"),
            dim,
        )));

        let body_lines: Vec<Line<'static>> = trimmed
            .lines()
            .map(|l| {
                let parsed = parse_ansi_line(l, dim);
                line_utils::truncate_line_with_ellipsis(parsed, hard_cap)
            })
            .collect();
        let total_rows: usize = body_lines.iter().map(&row_cost).sum();

        let (taken, rows_used) = if collapsed && total_rows > SHELL_PREVIEW_LINES {
            let mut used = 0usize;
            let mut n = 0usize;
            for line in body_lines.iter() {
                let cost = row_cost(line);
                if used + cost > SHELL_PREVIEW_LINES && n > 0 {
                    break;
                }
                used += cost;
                n += 1;
                if used >= SHELL_PREVIEW_LINES {
                    break;
                }
            }
            (n, used)
        } else {
            (body_lines.len(), total_rows)
        };

        for line in body_lines.iter().take(taken) {
            let mut spans = vec![Span::styled("    ".to_string(), dim)];
            spans.extend(line.spans.clone());
            lines.push(Line::from(spans));
        }

        if collapsed && total_rows > SHELL_PREVIEW_LINES {
            let extra = total_rows.saturating_sub(rows_used);
            lines.push(Line::from(Span::styled(collapse_footer(extra), dim)));
        }
    };

    push_section("stdout", parsed.stdout);
    push_section("stderr", parsed.stderr);

    // Status footer mirrors the success branch of the generic ToolResult
    // renderer (check + label + " • Ns"). Non-zero exit codes land in the
    // generic error path (is_error = true), so we only need ✓ here.
    let duration_str = match duration_ms {
        Some(ms) => {
            let secs_f = ms as f64 / 1000.0;
            format!(" • {secs_f:.1}s")
        }
        None => String::new(),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {} ", theme::CHECK), theme::check_style()),
        Span::styled(format!("{display_label}{duration_str}"), dim),
    ]));

    lines
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

    /// Build the turn divider for a completed working turn. The
    /// label aggregates elapsed time (only when notable, i.e. over a minute)
    /// and the local tool-call count; when neither applies the divider renders
    /// as a plain full-width rule. `tool_calls` is the number of `ToolResult`
    /// cells in the turn; `elapsed_secs` is the turn's wall-clock duration when
    /// known.
    pub fn turn_separator(tool_calls: usize, elapsed_secs: Option<u64>) -> Self {
        let mut parts: Vec<String> = Vec::new();
        if let Some(secs) = elapsed_secs.filter(|s| *s > 60) {
            parts.push(format!("Worked for {}", theme::format_elapsed(secs)));
        }
        if tool_calls > 0 {
            let noun = if tool_calls == 1 { "call" } else { "calls" };
            parts.push(format!("Local tools: {tool_calls} {noun}"));
        }
        HistoryCell::Separator {
            label: (!parts.is_empty()).then(|| parts.join(" • ")),
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

    /// True when this cell is part of a tool-flow block (the call header,
    /// its streaming output, or its result). Used by the render loop to
    /// suppress the separator blank line between adjacent tool-flow cells
    /// so back-to-back command runs read as a tight cluster.
    pub fn is_tool_flow(&self) -> bool {
        matches!(
            self,
            HistoryCell::ToolStart { .. }
                | HistoryCell::ToolStreaming { .. }
                | HistoryCell::ToolResult { .. }
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
            HistoryCell::User { text } => render_user_message(text, width),
            HistoryCell::Assistant {
                text,
                streaming,
                cache,
            } => render_assistant_message(text, *streaming, cache, width),
            HistoryCell::ToolStart {
                name,
                args,
                completed,
                ..
            } => render_tool_start(name, args, *completed),
            HistoryCell::ToolResult {
                output,
                is_error,
                duration_ms,
                display_label,
                tool_name,
                args_json,
                collapsed,
            } => {
                if *is_error {
                    render_tool_result_error(output, display_label, *duration_ms, width)
                } else {
                    render_tool_result_ok(
                        output,
                        display_label,
                        *duration_ms,
                        tool_name,
                        args_json.as_deref(),
                        *collapsed,
                        width,
                    )
                }
            }
            HistoryCell::ShellApproval { command, status } => {
                render_shell_approval(command, status)
            }
            HistoryCell::Heartbeat { text } => {
                let mut spans = vec![Span::styled("[heartbeat] ", theme::code_style())];
                spans.extend(style_inline_backticks(text, Style::default()));
                vec![Line::from(spans)]
            }
            HistoryCell::System { text } => text.lines().map(style_system_line).collect(),
            HistoryCell::Card { lines } => lines.clone(),
            HistoryCell::DoctorWarning { warnings } => build_doctor_warning_lines(warnings, width),
            HistoryCell::Thinking { text } => render_thinking(text, width),
            HistoryCell::ToolStreaming {
                lines: tool_lines,
                dropped,
            } => render_tool_streaming(tool_lines, *dropped, width),
            HistoryCell::Plan { steps } => render_plan(steps),
            HistoryCell::MentionCard { label } => render_mention_card(label),
            HistoryCell::Separator { label } => render_separator(label.as_deref(), width),
        }
    }
}

/// Render a user message as a flat filled block, not an outlined bubble.
/// Every rendered row owns the full viewport width.
fn render_user_message(text: &str, width: u16) -> Vec<Line<'static>> {
    let bg = theme::user_message_style();
    let prefix_style =
        bg.add_modifier(ratatui::style::Modifier::BOLD | ratatui::style::Modifier::DIM);
    let mention_style = bg.patch(theme::file_mention_style());
    let w = width as usize;
    let inset = 0usize;
    let wrap_width = w.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    if w > 0 {
        lines.push(Line::from(Span::styled(" ".repeat(w), bg)).style(bg));
    }
    // Sanitize at render so raw CSI sequences in pasted or
    // replayed history never reach native scrollback. Done here
    // (not on the stored value) so the transcript stays faithful.
    let text = sanitize_user_text(text);
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
            let mut row: Vec<Span<'static>> = Vec::new();
            if inset > 0 {
                row.push(Span::styled(" ".repeat(inset), bg));
            }
            row.push(prefix);
            row.extend(spans);
            let used_width: usize = row
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = w.saturating_sub(used_width);
            if pad > 0 {
                row.push(Span::styled(" ".repeat(pad), bg));
            }
            lines.push(Line::from(row).style(bg));
        }
    }
    if w > 0 {
        lines.push(Line::from(Span::styled(" ".repeat(w), bg)).style(bg));
    }
    lines
}

/// Byte offset of the `(` that opens the last still-unclosed inline link
/// destination in `s` (`[label](dest` with no matching `)` yet), or `None`
/// when every destination is closed.
///
/// A small scanner, not a markdown parser: it skips backslash escapes and
/// code spans (a backtick run up to the next run of the same length) so
/// `` `[a](` `` and `\[a\](` are never mistaken for links. An unclosed
/// backtick run ends the scan — mid-stream it is most likely a code span
/// still arriving, and cutting inside it would hide code text. Parentheses
/// nest inside a destination the way URLs such as `wiki/Foo_(bar)` need.
pub(super) fn unclosed_link_destination_start(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut open_brackets = 0usize;
    // Set only when the previous byte was a `]` that closed a `[`.
    let mut after_label = false;
    // `(` offset and nesting depth of the destination being scanned.
    let mut dest: Option<(usize, usize)> = None;
    while i < b.len() {
        let byte = b[i];
        let was_after_label = std::mem::take(&mut after_label);
        match (byte, dest.as_mut()) {
            (b'\\', _) => {
                i += 2;
                continue;
            }
            (b'(', Some((_, depth))) => *depth += 1,
            (b')', Some((_, 0))) => dest = None,
            (b')', Some((_, depth))) => *depth -= 1,
            (_, Some(_)) => {}
            (b'`', None) => {
                let run = b[i..].iter().take_while(|&&c| c == b'`').count();
                let mut j = i + run;
                loop {
                    if j >= b.len() {
                        return None;
                    }
                    if b[j] == b'`' {
                        let close = b[j..].iter().take_while(|&&c| c == b'`').count();
                        j += close;
                        if close == run {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                i = j;
                continue;
            }
            (b'[', None) => open_brackets += 1,
            (b']', None) if open_brackets > 0 => {
                open_brackets -= 1;
                after_label = true;
            }
            (b'(', None) if was_after_label => dest = Some((i, 0)),
            _ => {}
        }
        i += 1;
    }
    dest.map(|(start, _)| start)
}

/// Render an assistant message with the newline-gated streaming markdown
/// cache. Finalized (non-streaming) cells collapse long bodies.
fn render_assistant_message(
    text: &str,
    streaming: bool,
    cache: &RefCell<AssistantRenderCache>,
    width: u16,
) -> Vec<Line<'static>> {
    // Output-redaction chokepoint for the TUI transcript.
    // Assistant text is stripped of detected secrets before it
    // enters markdown rendering, so a shared-screen viewer
    // never sees a raw API key the agent legitimately read
    // from disk. Clean text takes a zero-allocation fast path
    // (Cow::Borrowed). User-input cells are intentionally not
    // redacted — the user sees their own input verbatim.
    let redacted;
    let text: &str = match current_policy().apply(OutputSink::Tui, None, text) {
        std::borrow::Cow::Borrowed(b) => b,
        std::borrow::Cow::Owned(o) => {
            redacted = o;
            &redacted
        }
    };
    let visible_text = strip_reply_directive_prefix(text);
    let inner_width = width.saturating_sub(LIVE_PREFIX_COLS as u16);
    let commit_end = visible_text
        .as_bytes()
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut cache = cache.borrow_mut();
    let cache_hit =
        cache.cached_width == Some(inner_width) && cache.committed_byte_len == commit_end;
    if !cache_hit {
        cache.committed_lines = if commit_end == 0 {
            Vec::new()
        } else {
            markdown::render_markdown(&visible_text[..commit_end], inner_width)
        };
        cache.committed_byte_len = commit_end;
        cache.cached_width = Some(inner_width);
    }
    // While streaming, hold back an unfinished link destination: rendered
    // early, `[label](https://parti` shows as raw markdown and then snaps to
    // a styled link once `)` arrives. The held-back bytes appear on the next
    // frame that closes the link (or at finalize).
    let pending_end = if streaming {
        unclosed_link_destination_start(&visible_text[commit_end..])
            .map_or(visible_text.len(), |cut| commit_end + cut)
    } else {
        visible_text.len()
    };
    let pending_lines = if commit_end < pending_end {
        markdown::render_markdown(&visible_text[commit_end..pending_end], inner_width)
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
    let lines = if visible_text.is_empty() {
        vec![]
    } else if all.is_empty() {
        vec![Line::from(prefix_span)]
    } else {
        prefix_lines(all, prefix_span, Span::raw("  "))
    };
    if streaming {
        lines
    } else {
        collapse_long_lines(lines)
    }
}

/// Render a tool-call header (`• verb target`) plus an optional detail line.
fn render_tool_start(name: &str, args: &str, completed: bool) -> Vec<Line<'static>> {
    let cat = super::tool_display::classify_tool(name, args);
    let bullet_style = if completed {
        theme::tool_bullet_done()
    } else {
        theme::tool_bullet_active()
    };
    let mut header_spans = vec![Span::styled(format!("{} ", theme::BULLET), bullet_style)];
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

/// Apply the TUI output-redaction policy to tool result text. A `run_shell`
/// that cats `~/.aws/credentials` or `read_file` on `.env` would otherwise
/// echo the raw value into the transcript. Applied once at the top of each
/// result render path so every downstream formatter (shell, diff, generic
/// body) sees the redacted form.
fn redact_tool_output(output: &str) -> std::borrow::Cow<'_, str> {
    current_policy().apply(OutputSink::Tui, None, output)
}

/// Status line with check/cross indicator, display label, and duration.
fn tool_status_line(
    is_error: bool,
    display_label: &str,
    duration_ms: Option<u64>,
) -> Line<'static> {
    let (indicator, ind_style) = if is_error {
        (theme::CROSS, theme::cross_style())
    } else {
        (theme::CHECK, theme::check_style())
    };
    let duration_str = match duration_ms {
        Some(ms) => {
            let secs_f = ms as f64 / 1000.0;
            format!(" • {secs_f:.1}s")
        }
        None => String::new(),
    };
    Line::from(vec![
        Span::styled(format!("  {indicator} "), ind_style),
        Span::styled(format!("{display_label}{duration_str}"), theme::dim()),
    ])
}

/// Generic tool result body: one rendered line per source line, ANSI-parsed
/// and hard-capped. When `compact_json` is set and the full output is
/// pretty-printed JSON, it is collapsed to a compact form first so `textwrap`
/// has `: ` / `, ` break points and doesn't have to hard-split inside objects.
fn plain_tool_body(
    output: &str,
    style: Style,
    compact_json: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let hard_cap = body_hard_cap(width);
    let owned;
    let source: &str = if compact_json {
        if let Some(compact) = line_utils::format_json_compact(output) {
            owned = compact;
            &owned
        } else {
            output
        }
    } else {
        output
    };
    source
        .lines()
        .map(|l| {
            let parsed = parse_ansi_line(l, style);
            line_utils::truncate_line_with_ellipsis(parsed, hard_cap)
        })
        .collect()
}

/// Render a failed tool result. Errors render boxed so failures stand out
/// from successful tool calls; the `✗ display_label` status line below the
/// box keeps the visual link to the tool call.
fn render_tool_result_error(
    output: &str,
    display_label: &str,
    duration_ms: Option<u64>,
    width: u16,
) -> Vec<Line<'static>> {
    let output = redact_tool_output(output);
    let body_lines = plain_tool_body(&output, theme::error_style(), false, width);
    let mut lines = render_boxed_lines(body_lines, theme::error_style(), Some(" error "), width, 2);
    lines.push(tool_status_line(true, display_label, duration_ms));
    lines
}

/// Post-wrap row cost of one logical line at `content_width`, so one stray
/// long URL can't burst past the collapse preview.
fn wrapped_row_cost(line: &Line<'_>, content_width: usize) -> usize {
    let w = line_utils::line_width(line);
    if w == 0 {
        1
    } else {
        w.div_ceil(content_width)
    }
}

/// How many leading body lines a collapsed preview keeps, and the row cost
/// they occupy, honoring `COLLAPSE_PREVIEW_LINES` in wrapped rows.
fn collapsed_preview_take(body_lines: &[Line<'_>], content_width: usize) -> (usize, usize) {
    let mut used = 0usize;
    let mut n = 0usize;
    for line in body_lines.iter() {
        let cost = wrapped_row_cost(line, content_width);
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
}

/// Render a successful tool result.
///
/// Contextual: `run_shell` uses a compact two-line layout (separate
/// `└ Exit code` row, per-stream `--- stdout/stderr ---` sections omitted
/// when empty, capped preview per stream), falling through to the generic
/// body path if the envelope can't be parsed. `apply_patch` renders the raw
/// patch as a colored diff instead of the plain success-text output.
fn render_tool_result_ok(
    output: &str,
    display_label: &str,
    duration_ms: Option<u64>,
    tool_name: &str,
    args_json: Option<&str>,
    collapsed: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let output = redact_tool_output(output);
    let output: &str = &output;

    if tool_name == "run_shell" {
        if let Some(parsed) = parse_shell_output(output) {
            return render_shell_result(parsed, display_label, duration_ms, collapsed, width);
        }
    }

    let rendered_body = if tool_name == "apply_patch" {
        args_json
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("patch").and_then(|p| p.as_str()).map(str::to_string))
            .map(|patch| {
                // Reserve room for the cell's left bullet/indent
                // (~6 cols) so wrapped diff rows don't overrun.
                let inner = (width as usize).saturating_sub(6).max(20);
                diff_render::render_patch_wrapped(&patch, inner)
            })
    } else {
        None
    };

    let body_lines = match rendered_body {
        Some(diff_lines) => diff_lines,
        None => plain_tool_body(output, theme::dim(), true, width),
    };
    let body_style = theme::dim();

    // Row-aware collapse: measure each logical line's post-wrap row
    // cost so one stray long URL can't burst past the preview.
    let content_width = (width as usize).saturating_sub(4).max(1);
    let total_rows: usize = body_lines
        .iter()
        .map(|l| wrapped_row_cost(l, content_width))
        .sum();
    let should_collapse = collapsed && total_rows > COLLAPSE_THRESHOLD;

    let (taken, rows_used) = if should_collapse {
        collapsed_preview_take(&body_lines, content_width)
    } else {
        (body_lines.len(), total_rows)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
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
            collapse_footer(extra),
            theme::dim(),
        )));
    }

    lines.push(tool_status_line(false, display_label, duration_ms));
    lines
}

/// Render the shell-approval prompt card with its current status.
fn render_shell_approval(command: &str, status: &ApprovalStatus) -> Vec<Line<'static>> {
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
            Span::styled(command.to_string(), theme::error_style()),
        ]),
        Line::from(vec![
            Span::raw("  Allow? "),
            Span::styled(status_text.to_string(), status_style),
        ]),
    ]
}

/// Render reasoning as a dim + italic bullet block, no box.
/// Header line: "• thinking" (dim, italic) — bullet matches assistant
/// bullet width so wrapped continuation lines hang under the body.
fn render_thinking(text: &str, width: u16) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![];
    }
    let content_style = theme::reasoning_style();
    let bullet = Span::styled(format!("{} ", theme::BULLET), theme::dim());
    let header = Line::from(vec![
        bullet.clone(),
        Span::styled("thinking", content_style),
    ]);
    let inner_w = (width as usize).saturating_sub(LIVE_PREFIX_COLS).max(1);
    let mut body: Vec<Line<'static>> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            body.push(Line::default());
            continue;
        }
        for wrapped in super::wrapping::wrap(line, inner_w) {
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(wrapped.to_string(), content_style),
            ]));
        }
    }
    let mut out = vec![header];
    out.extend(body);
    out
}

/// Render the live tail of a streaming tool's output: at most the last 8
/// lines, with an elision notice folding in both the skipped and the
/// already-dropped lines.
fn render_tool_streaming(
    tool_lines: &[(String, bool)],
    dropped: u64,
    width: u16,
) -> Vec<Line<'static>> {
    let mut rendered: Vec<Line<'static>> = Vec::new();
    let total = tool_lines.len();
    let max_visible = 8;
    let skip = total.saturating_sub(max_visible);
    let hidden = skip as u64 + dropped;
    if hidden > 0 {
        rendered.push(Line::from(Span::styled(
            format!("  ... ({hidden} lines above)"),
            theme::dim(),
        )));
    }
    let hard_cap = body_hard_cap(width);
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

/// Render the plan card: `• Updated Plan` header plus per-step marker lines.
fn render_plan(steps: &[PlanStep]) -> Vec<Line<'static>> {
    // Header: `• Updated Plan` (dim bullet + bold title).
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} ", theme::BULLET), theme::dim()),
        Span::styled("Updated Plan", theme::bold()),
    ])];
    // Step markers: `✔ ` crossed-out+dim (done),
    // `□ ` teal+bold (in-progress), `□ ` dim (pending).
    let mut step_lines: Vec<Line<'static>> = Vec::new();
    for step in steps {
        let (marker, style) = match step.status {
            PlanStepStatus::Completed => ("✔ ", theme::dim().add_modifier(Modifier::CROSSED_OUT)),
            PlanStepStatus::InProgress => (
                "□ ",
                ratatui::style::Style::default()
                    .fg(theme::cyan())
                    .add_modifier(Modifier::BOLD),
            ),
            PlanStepStatus::Pending => ("□ ", theme::dim()),
        };
        step_lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(step.title.clone(), style),
        ]));
    }
    lines.extend(prefix_lines(
        step_lines,
        Span::styled("  └ ", theme::dim()),
        Span::raw("    "),
    ));
    lines
}

/// Render a one-line mention card: dim `⌞ Read CLAUDE.md (275 lines)` /
/// `⌞ Listed directory foo/`. The filename/path portion after the verb is
/// bolded.
fn render_mention_card(label: &str) -> Vec<Line<'static>> {
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

/// Render a turn divider as a full-width dim rule, with an optional
/// `─ {label} ─` inline label.
fn render_separator(label: Option<&str>, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let line = match label {
        Some(l) if !l.is_empty() => {
            // Labeled format: `─ {label} ─` then fill the remainder.
            // If the label can't fit the viewport, fall back to a
            // plain full-width rule rather than overflow/truncate it.
            let labeled = format!("{0} {l} {0}", theme::SEPARATOR);
            let used = unicode_width::UnicodeWidthStr::width(labeled.as_str());
            if used > w {
                theme::SEPARATOR.repeat(w)
            } else {
                format!("{labeled}{}", theme::SEPARATOR.repeat(w - used))
            }
        }
        _ => theme::SEPARATOR.repeat(w),
    };
    vec![Line::from(Span::styled(line, theme::dim()))]
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
mod tests;
