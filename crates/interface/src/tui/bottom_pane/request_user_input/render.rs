//! Picker overlay render path.
//!
//! Five stacked sections (see `layout.rs`) get painted top-to-bottom:
//! `progress`, `question`, `options`, `notes`, `footer`. Heights come from
//! `RequestUserInputOverlay::layout_sections`; this module only decides
//! *what pixels go where* once the rects are handed to it.
//!
//! Helpers are inlined per-popup rather than shared (option-row line
//! construction, footer hint composition); the unanswered-confirmation
//! overlay and composer-backed notes editor are intentionally absent.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::theme;

use super::display_width;
use super::other_label;
use super::{Focus, RequestUserInputOverlay};

/// `›` chevron marker drawn on the selected option row (see `theme::CHEVRON`).
const SELECTION_MARKER: &str = theme::CHEVRON;
/// Filled checkbox glyph for a chosen multi-select option.
const MULTI_SELECTED: &str = "[x]";
/// Empty checkbox glyph for an unchosen multi-select option.
const MULTI_UNSELECTED: &str = "[ ]";
/// Width allocated for the leading marker column ("› " or "  ").
const MARKER_COL_WIDTH: usize = 2;

impl RequestUserInputOverlay {
    /// Paint the entire picker into `area`. Caller has already issued a
    /// `Clear` for the rect; this function fills it.
    pub(super) fn render_ui(&self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let sections = self.layout_sections(area);

        if sections.progress_area.height > 0 {
            self.render_progress(buf, sections.progress_area);
        }
        if sections.question_area.height > 0 {
            self.render_question(buf, sections.question_area, &sections.question_lines);
        }
        if self.has_options() && sections.options_area.height > 0 {
            self.render_options(buf, sections.options_area);
        }
        if self.notes_ui_visible() && sections.notes_area.height > 0 {
            self.render_notes(buf, sections.notes_area);
        }
        if sections.footer_lines > 0 {
            // Footer hugs the bottom of the laid-out region: tile starts
            // just past the notes area (which is the last allocated rect
            // even when it's zero-height).
            let footer_y = sections
                .notes_area
                .y
                .saturating_add(sections.notes_area.height);
            let footer_area = Rect {
                x: area.x,
                y: footer_y,
                width: area.width,
                height: sections.footer_lines,
            };
            self.render_footer(buf, footer_area);
        }
    }

    fn render_progress(&self, buf: &mut Buffer, area: Rect) {
        let total = self.questions.len();
        if total == 0 {
            return;
        }
        let idx = self.current_idx.saturating_add(1);
        let unanswered = self.unanswered_count_for_progress();
        let text = if unanswered > 0 {
            format!("Question {idx}/{total} ({unanswered} unanswered)")
        } else {
            format!("Question {idx}/{total}")
        };
        let line = Line::from(Span::styled(text, theme::dim()));
        Paragraph::new(line).render(area, buf);
    }

    fn render_question(&self, buf: &mut Buffer, area: Rect, lines: &[String]) {
        // Render each wrapped row on its own line so styling is uniform and
        // we don't accidentally re-wrap. Unanswered questions get a cyan
        // tint to draw the eye; once an answer is locked in, the question
        // returns to the default foreground so the user knows it's settled.
        let answered = self.current_question_is_answered();
        let style = if answered {
            Style::default()
        } else {
            Style::default().fg(theme::cyan())
        };
        for (offset, raw) in lines.iter().enumerate() {
            if offset as u16 >= area.height {
                break;
            }
            let row = Rect {
                x: area.x,
                y: area.y.saturating_add(offset as u16),
                width: area.width,
                height: 1,
            };
            let line = Line::from(Span::styled(raw.clone(), style));
            Paragraph::new(line).render(row, buf);
        }
    }

    fn render_options(&self, buf: &mut Buffer, area: Rect) {
        let rows = self.option_rows();
        if rows.is_empty() || area.height == 0 {
            return;
        }
        let visible = area.height as usize;
        let selected = self.current_answer().options_state.selected_idx;
        let other_idx = rows.len() - 1;

        // When the list fits, render every row top-to-bottom; no pinning
        // needed. The common case (≤ 4 options + Other = 5 rows, visible
        // cap = 5) takes this branch.
        if rows.len() <= visible {
            for (paint_idx, row) in rows.iter().enumerate() {
                let is_selected_cursor = Some(paint_idx) == selected;
                let line = build_option_line(row, is_selected_cursor, area.width as usize);
                let rect = Rect {
                    x: area.x,
                    y: area.y.saturating_add(paint_idx as u16),
                    width: area.width,
                    height: 1,
                };
                Paragraph::new(line).render(rect, buf);
            }
            return;
        }

        // Tight area: pin "Other" to the last visible line so the user
        // always sees it (otherwise it'd hide behind the scroll window
        // until they navigated past every caller-supplied option). Regular
        // options scroll inside the remaining rows.
        let scroll_height = area.height.saturating_sub(1) as usize;
        if scroll_height > 0 {
            let regular_count = rows.len() - 1;
            let mut state = self.current_answer().options_state;
            // Confine scrolling to the non-Other rows; when the cursor is
            // on Other we don't move the window — Other is rendered in the
            // pinned slot regardless.
            if let Some(i) = state.selected_idx {
                if i == other_idx {
                    state.selected_idx = None;
                }
            }
            state.ensure_visible(regular_count, scroll_height);
            let start = state.scroll_top;
            let end = (start + scroll_height).min(regular_count);
            for (paint_idx, row_idx) in (start..end).enumerate() {
                let row = &rows[row_idx];
                let is_selected_cursor = Some(row_idx) == selected;
                let line = build_option_line(row, is_selected_cursor, area.width as usize);
                let rect = Rect {
                    x: area.x,
                    y: area.y.saturating_add(paint_idx as u16),
                    width: area.width,
                    height: 1,
                };
                Paragraph::new(line).render(rect, buf);
            }
        }

        // Pinned Other row, always rendered on the last visible line.
        let other_row = &rows[other_idx];
        let is_other_selected = Some(other_idx) == selected;
        let line = build_option_line(other_row, is_other_selected, area.width as usize);
        let rect = Rect {
            x: area.x,
            y: area.y.saturating_add(area.height.saturating_sub(1)),
            width: area.width,
            height: 1,
        };
        Paragraph::new(line).render(rect, buf);
    }

    /// Render the notes editor — a `Notes` label row plus the editable
    /// body row. Body delegates to `TextLine::render`, which paints the
    /// `▌` bar, the typed text, and a reversed cursor block at the
    /// insertion point when the field is focused.
    fn render_notes(&self, buf: &mut Buffer, area: Rect) {
        let label_row = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        let label = Line::from(Span::styled("Notes", theme::dim()));
        Paragraph::new(label).render(label_row, buf);

        if area.height < 2 {
            return;
        }
        let body_row = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: 1,
        };
        let focused = self.focus == Focus::Notes;
        self.current_answer()
            .notes
            .render(buf, body_row, focused, Some("press Tab to add a note"));
    }

    fn render_footer(&self, buf: &mut Buffer, area: Rect) {
        let multi = self.current().multi_select;
        let in_notes = self.focus == Focus::Notes;
        let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
        hints.push(("↑↓", "select"));
        if multi {
            hints.push(("space", "toggle"));
        }
        hints.push((
            "enter",
            if self.is_last_question() {
                "submit"
            } else {
                "next"
            },
        ));
        hints.push(("tab", if in_notes { "options" } else { "notes" }));
        hints.push(("esc", "cancel"));

        let mut spans: Vec<Span<'static>> = Vec::new();
        for (idx, (key, label)) in hints.into_iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled("  ·  ", theme::dim()));
            }
            spans.push(Span::styled(
                key.to_string(),
                Style::default()
                    .fg(theme::cyan())
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label.to_string(), theme::dim()));
        }
        let row = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        Paragraph::new(Line::from(spans)).render(row, buf);
    }

    // ----- Helpers consumed only by render ------------------------------

    /// `true` when the current question already has at least one selection
    /// or a non-empty note. Used to drop the question's "unanswered" cyan
    /// tint once the user has committed something.
    fn current_question_is_answered(&self) -> bool {
        let ans = self.current_answer();
        !ans.selected.is_empty() || !ans.notes.is_empty()
    }

    /// Count of questions in this batch that have neither a selection nor
    /// a note. Drives the "(N unanswered)" badge in the progress row.
    fn unanswered_count_for_progress(&self) -> usize {
        self.answers
            .iter()
            .filter(|a| a.selected.is_empty() && a.notes.is_empty())
            .count()
    }

    fn is_last_question(&self) -> bool {
        self.current_idx + 1 == self.questions.len()
    }

    /// Build the displayed option rows for the current question, including
    /// the auto-appended "Other" row. Each row carries enough state for the
    /// renderer to draw selection markers + multi-select checkboxes.
    fn option_rows(&self) -> Vec<OptionRow> {
        let q = self.current();
        let multi = q.multi_select;
        let selected = &self.current_answer().selected;
        let mut rows: Vec<OptionRow> = Vec::with_capacity(q.options.len() + 1);
        for (idx, opt) in q.options.iter().enumerate() {
            rows.push(OptionRow {
                label: opt.label.clone(),
                description: Some(opt.description.clone()),
                checked: multi && selected.contains(&idx),
                multi,
            });
        }
        // "Other" row — auto-appended; matches the borg-side `OTHER_LABEL`
        // contract so the cortex/UI agree on the chosen option.
        let other_idx = q.options.len();
        rows.push(OptionRow {
            label: other_label().to_string(),
            description: None,
            checked: multi && selected.contains(&other_idx),
            multi,
        });
        rows
    }
}

/// Single row in the rendered options list. Owned strings so we can hand
/// it to ratatui's `Paragraph` without lifetime gymnastics.
struct OptionRow {
    label: String,
    description: Option<String>,
    /// `true` when this row is in the user's selection set (multi-select
    /// only — single-select shows the cursor via the chevron marker).
    checked: bool,
    /// `true` when the parent question is multi-select. Controls whether
    /// the leading checkbox glyph is drawn.
    multi: bool,
}

/// Compose one rendered option row.
///
/// Format: `<marker> <checkbox?> <label>  <description-dimmed>` where the
/// chevron column is always 2 cells wide (so non-selected rows align), the
/// checkbox is only present in multi-select mode, and the description is
/// dropped when there's not enough width for it.
fn build_option_line(row: &OptionRow, cursor: bool, max_width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let marker = if cursor {
        Span::styled(
            format!("{SELECTION_MARKER} "),
            Style::default()
                .fg(theme::cyan())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ".to_string())
    };
    spans.push(marker);
    let mut used = MARKER_COL_WIDTH;

    if row.multi {
        let glyph = if row.checked {
            MULTI_SELECTED
        } else {
            MULTI_UNSELECTED
        };
        spans.push(Span::styled(
            format!("{glyph} "),
            if row.checked {
                Style::default()
                    .fg(theme::green())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ));
        used += display_width(glyph) + 1;
    }

    let label_style = if cursor {
        theme::popup_selected()
    } else {
        Style::default()
    };
    spans.push(Span::styled(row.label.clone(), label_style));
    used += display_width(&row.label);

    if let Some(desc) = row.description.as_ref() {
        // Only attach the description when it leaves at least four cells
        // of breathing room — otherwise it clips and reads as visual noise.
        let separator = "  ";
        let available = max_width
            .saturating_sub(used)
            .saturating_sub(display_width(separator));
        if available >= 4 {
            let trimmed = truncate_to_width(desc, available);
            spans.push(Span::styled(separator.to_string(), theme::dim()));
            spans.push(Span::styled(trimmed, theme::dim()));
        }
    }

    Line::from(spans)
}

/// Truncate `s` so its display width is at most `max_width`, replacing the
/// dropped tail with an ellipsis when truncation actually happens. Returns
/// the original string when it already fits.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(s) <= max_width {
        return s.to_string();
    }
    let ellipsis = theme::ELLIPSIS;
    let ellipsis_w = display_width(ellipsis);
    if ellipsis_w >= max_width {
        return ellipsis.to_string();
    }
    let limit = max_width - ellipsis_w;
    let mut out = String::new();
    let mut width = 0usize;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > limit {
            break;
        }
        width += w;
        out.push(ch);
    }
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::tool_handlers::user_input::{Question, QuestionOption};

    fn q(prompt: &str, options: usize, multi: bool) -> Question {
        Question {
            id: "q".into(),
            header: "Test".into(),
            question: prompt.into(),
            options: (0..options)
                .map(|i| QuestionOption {
                    label: format!("Option {i}"),
                    description: format!("desc {i}"),
                })
                .collect(),
            multi_select: multi,
        }
    }

    /// Render into a fresh `Buffer` of the given size. Mirrors the path
    /// `App` hits — `BottomPane::render` issues a Clear and then calls
    /// `render_ui` with the underlying buffer.
    fn render_to_buffer(overlay: &RequestUserInputOverlay, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        overlay.render_ui(&mut buf, area);
        buf
    }

    fn buffer_to_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Progress row prefixes the overlay and reflects the question count.
    /// Regression guard: forgetting to render progress (or rendering "1/1"
    /// with stale indices) would hide multi-question batches from the user.
    #[test]
    fn progress_row_shows_current_index_and_total() {
        let overlay =
            RequestUserInputOverlay::new(vec![q("first", 2, false), q("second", 2, false)])
                .unwrap();
        let buf = render_to_buffer(&overlay, 60, 12);
        let text = buffer_to_text(&buf);
        assert!(
            text.contains("Question 1/2"),
            "expected progress row in:\n{text}"
        );
        // Both questions start unanswered → "(2 unanswered)" badge.
        assert!(
            text.contains("2 unanswered"),
            "expected unanswered badge in:\n{text}"
        );
    }

    /// The selection chevron paints next to the currently focused option.
    /// Without this, the user can't tell which row is highlighted.
    #[test]
    fn cursor_chevron_marks_selected_option_row() {
        let mut overlay = RequestUserInputOverlay::new(vec![q("pick", 3, false)]).unwrap();
        // Move the cursor onto row 1 (the second caller option).
        overlay.answers[0].options_state.selected_idx = Some(1);
        let buf = render_to_buffer(&overlay, 50, 14);
        let text = buffer_to_text(&buf);
        // Find the line that contains "Option 1" and assert it carries the
        // chevron — the other option rows must NOT.
        let mut chevron_lines = 0usize;
        for line in text.lines() {
            if line.contains(SELECTION_MARKER) {
                chevron_lines += 1;
                assert!(
                    line.contains("Option 1"),
                    "chevron landed on wrong row: {line:?}"
                );
            }
        }
        assert_eq!(
            chevron_lines, 1,
            "exactly one row should carry the cursor chevron:\n{text}"
        );
    }

    /// Multi-select rows render a checkbox glyph; selected rows are filled.
    /// Single-select questions must NOT show checkboxes (visual contract).
    #[test]
    fn multi_select_question_renders_checkboxes() {
        let mut overlay = RequestUserInputOverlay::new(vec![q("pick many", 3, true)]).unwrap();
        overlay.answers[0].selected = vec![0, 2];
        let buf = render_to_buffer(&overlay, 60, 14);
        let text = buffer_to_text(&buf);
        assert!(
            text.contains(MULTI_SELECTED),
            "expected filled checkbox in:\n{text}"
        );
        assert!(
            text.contains(MULTI_UNSELECTED),
            "expected empty checkbox for unchosen rows:\n{text}"
        );

        // Single-select question: checkboxes must be absent.
        let single = RequestUserInputOverlay::new(vec![q("pick one", 3, false)]).unwrap();
        let buf = render_to_buffer(&single, 60, 14);
        let text = buffer_to_text(&buf);
        assert!(
            !text.contains(MULTI_SELECTED) && !text.contains(MULTI_UNSELECTED),
            "single-select must not render checkbox glyphs:\n{text}"
        );
    }

    /// The auto-appended "Other" row appears in the rendered output even
    /// though it isn't part of `Question.options`. This is the borg
    /// contract that lets the agent receive a free-form response.
    #[test]
    fn other_row_is_appended_in_rendered_options() {
        let overlay = RequestUserInputOverlay::new(vec![q("pick", 2, false)]).unwrap();
        let buf = render_to_buffer(&overlay, 50, 14);
        let text = buffer_to_text(&buf);
        assert!(
            text.contains(other_label()),
            "Other row missing from render:\n{text}"
        );
    }

    /// When the options area is too short to fit every row, the "Other"
    /// row stays pinned to the last visible line even with the cursor on
    /// the first option. Without this, the user wouldn't know Other is
    /// available until they navigated past every caller-supplied option.
    #[test]
    fn other_row_stays_pinned_when_options_area_is_tight() {
        let mut overlay = RequestUserInputOverlay::new(vec![q("pick", 4, false)]).unwrap();
        // Cursor on the first option so the scroll window naturally
        // shows the head of the list. With 4 caller options + Other = 5
        // rows and the layout-allocated options area shorter than 5, the
        // pin keeps Other visible.
        overlay.answers[0].options_state.selected_idx = Some(0);
        let buf = render_to_buffer(&overlay, 60, 8);
        let text = buffer_to_text(&buf);
        assert!(
            text.contains(other_label()),
            "Other row must stay pinned when cursor is on first option:\n{text}"
        );
        assert!(
            text.contains("Option 0"),
            "first option must still render alongside pinned Other:\n{text}"
        );
    }

    /// Zero-sized area is a no-op — the layout host can hand us a collapsed
    /// rect mid-resize. Render must not panic.
    #[test]
    fn zero_area_render_is_a_noop_not_a_panic() {
        let overlay = RequestUserInputOverlay::new(vec![q("pick", 2, false)]).unwrap();
        // Render into a 1×1 terminal so the inside layout collapses to
        // basically nothing — exercises the early-return guards in every
        // sub-render method.
        let _ = render_to_buffer(&overlay, 1, 1);
    }

    /// Description truncation: when the row is wider than the area, the
    /// trailing description must be elided with an ellipsis and the label
    /// must remain readable. Without this guard, long descriptions wrap
    /// or clip into the next visual row.
    #[test]
    fn long_description_truncates_with_ellipsis() {
        let mut q1 = q("pick", 1, false);
        q1.options[0].label = "Short".into();
        q1.options[0].description =
            "this description is intentionally far too long to fit in a narrow row".into();
        let overlay = RequestUserInputOverlay::new(vec![q1]).unwrap();
        let buf = render_to_buffer(&overlay, 30, 12);
        let text = buffer_to_text(&buf);
        assert!(text.contains("Short"), "label must remain visible:\n{text}");
        assert!(
            text.contains(theme::ELLIPSIS),
            "long description must end with ellipsis:\n{text}"
        );
    }

    /// `truncate_to_width` shrinks strings to their cell width budget and
    /// returns the original when it already fits. Direct test of the
    /// helper because the rendered-buffer assertion above can't easily
    /// distinguish "trimmed by 1 char" from "trimmed by 50".
    #[test]
    fn truncate_to_width_respects_display_cells() {
        assert_eq!(truncate_to_width("abc", 10), "abc");
        let trimmed = truncate_to_width("abcdefghij", 5);
        // 5 cells = 4 chars + ellipsis (1 cell).
        assert_eq!(display_width(&trimmed), 5);
        assert!(trimmed.ends_with(theme::ELLIPSIS));
        assert_eq!(truncate_to_width("anything", 0), "");
    }
}
