use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;
use throbber_widgets_tui::{Throbber, BRAILLE_EIGHT};

use super::super::{layout, shimmer, theme};
use super::{App, AppState, BacktrackPhase};
use borg_core::config::CollaborationMode;

impl<'a> App<'a> {
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let show_status = !matches!(self.state, AppState::Idle);
        let composer_height = self.composer.height();
        let queue_preview_height = self.compute_queue_preview_height();
        let app_layout =
            layout::compute_layout(area, composer_height, show_status, queue_preview_height);

        self.render_transcript(frame, app_layout.transcript);
        if show_status {
            self.render_status(frame, app_layout.status);
        }
        if queue_preview_height > 0 {
            self.render_queue_preview(frame, app_layout.queue_preview);
        }
        self.composer.render(frame, app_layout.composer);
        self.render_footer(frame, app_layout.footer);
        self.plan_overlay.render(frame, app_layout.composer);
        self.command_popup.render(frame, app_layout.composer);
        self.file_popup.render(frame, app_layout.composer);
        self.settings_popup.render(frame, &self.config);
        self.plugins_popup.render(frame);
        self.pairing_popup.render(frame);
        self.projects_popup.render(frame);
        self.sessions_popup.render(frame);
        self.schedule_popup.render(frame);
        self.migrate_popup.render(frame);
        self.status_popup.render(frame);

        // Transcript pager (Ctrl+T) takes the full frame when active. Drawn
        // after popups so it covers them; before toasts so toasts still float on top.
        if matches!(self.state, AppState::TranscriptPager) {
            self.transcript_pager.render(frame, area, &self.cells);
        }

        // Toasts are drawn last so they float above every other overlay.
        self.toasts.prune_expired();
        self.toasts.render(frame, area);
    }

    pub(super) fn compute_context_pct(&self) -> u8 {
        let max = self.config.conversation.max_history_tokens;
        if max == 0 {
            return 0;
        }
        let used = self.session_prompt_tokens + self.session_completion_tokens;
        ((used as f64 / max as f64) * 100.0).min(100.0) as u8
    }

    fn render_transcript(&mut self, frame: &mut Frame, area: Rect) {
        self.transcript_area = area;
        let width = area.width;
        let mut all_lines: Vec<Line<'static>> = Vec::new();

        let throbber_state = match &self.state {
            AppState::Streaming { .. } => Some(&self.throbber_state),
            _ => None,
        };

        // Always show branded header
        let version = env!("CARGO_PKG_VERSION");
        all_lines.push(Line::from(vec![
            Span::styled(
                "BORG",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::from(" "),
            Span::styled(format!("v{version}"), theme::dim()),
        ]));
        all_lines.push(Line::default());

        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        all_lines.push(Line::from(vec![
            Span::styled("name:  ", theme::dim()),
            Span::from(name.to_string()),
        ]));

        all_lines.push(Line::from(vec![
            Span::styled("model: ", theme::dim()),
            Span::from(self.config.llm.model.clone()),
        ]));

        if let Some(ref evo_title) = self.evolution_title {
            all_lines.push(Line::from(vec![
                Span::styled("class: ", theme::dim()),
                Span::styled(evo_title.clone(), Style::default().fg(theme::CYAN)),
            ]));
        }

        all_lines.push(Line::default());

        for (i, cell) in self.cells.iter().enumerate() {
            if i > 0 && !cell.is_stream_continuation() {
                all_lines.push(Line::default());
            }
            all_lines.extend(cell.render(width, throbber_state));
        }

        let paragraph = Paragraph::new(all_lines).wrap(Wrap { trim: false });
        self.total_lines = paragraph.line_count(width);

        let visible_height = area.height as usize;
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let scroll_pos = max_scroll.saturating_sub(self.scroll_offset);

        // Clamp to u16 for ratatui's scroll API
        let scroll_pos_u16 = u16::try_from(scroll_pos).unwrap_or(u16::MAX);

        let paragraph = paragraph.scroll((scroll_pos_u16, 0));

        frame.render_widget(paragraph, area);

        if self.total_lines > visible_height {
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(max_scroll - self.scroll_offset);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                area,
                &mut scrollbar_state,
            );
        }

        self.render_autoscroll_hint(frame, area);
    }

    /// Overlay a "press End to follow" hint at the bottom-right of the transcript
    /// when streaming content is arriving while the user is scrolled up.
    fn render_autoscroll_hint(&self, frame: &mut Frame, area: Rect) {
        let show_hint = !self.auto_scroll
            && self.scroll_offset > 0
            && matches!(self.state, AppState::Streaming { .. });
        if !show_hint || area.height == 0 {
            return;
        }
        let hint = " ↓ new output — End to follow ";
        let hint_width = hint.chars().count() as u16;
        if hint_width + 1 >= area.width {
            return;
        }
        let hint_area = Rect {
            x: area.x + area.width.saturating_sub(hint_width + 1),
            y: area.y + area.height - 1,
            width: hint_width,
            height: 1,
        };
        let hint_para = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::REVERSED),
        )));
        frame.render_widget(Clear, hint_area);
        frame.render_widget(hint_para, hint_area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.state {
            AppState::Streaming { start, .. } => {
                let elapsed = theme::format_elapsed(start.elapsed().as_secs());
                let throbber = Throbber::default()
                    .throbber_set(BRAILLE_EIGHT)
                    .throbber_style(theme::tool_style());
                let mut spans = vec![
                    Span::raw(" "),
                    throbber.to_symbol_span(&self.throbber_state),
                ];
                spans.extend(shimmer::shimmer_spans(
                    "Working...",
                    (2, 195, 189),
                    (180, 255, 252),
                ));
                spans.push(Span::styled(format!(" ({elapsed}"), theme::tool_style()));
                spans.push(Span::styled(" • esc to interrupt)", theme::dim()));
                Line::from(spans)
            }
            AppState::AwaitingApproval { .. } => Line::from(vec![Span::styled(
                format!(" {} Approval needed — press y or n", theme::BULLET),
                theme::error_style(),
            )]),
            AppState::AwaitingInput { .. } => Line::from(vec![Span::styled(
                format!(
                    " {} Agent needs your input — type and press enter",
                    theme::BULLET
                ),
                theme::tool_style(),
            )]),
            AppState::PlanReview => Line::from(vec![Span::styled(
                format!(" {} Plan ready — choose an action", theme::BULLET),
                theme::tool_style(),
            )]),
            AppState::ConfirmingUninstall => Line::from(vec![Span::styled(
                format!(" {} Confirm uninstall — press y or N", theme::BULLET),
                theme::error_style(),
            )]),
            AppState::TranscriptPager => Line::default(),
            AppState::Idle => Line::default(),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // Search overlay takes over the footer, matching bash/zsh's
        // `(reverse-i-search)` prompt convention.
        if self.composer.is_searching() {
            let query = self.composer.search_query().unwrap_or("");
            let failing = !self.composer.search_has_match() && !query.is_empty();
            let label = if failing {
                " failing reverse-i-search: "
            } else {
                " reverse-i-search: "
            };
            let style = if failing {
                theme::error_style()
            } else {
                theme::tool_style()
            };
            let line = Line::from(vec![
                Span::styled(label, style),
                Span::styled(format!("`{query}`"), theme::dim()),
                Span::styled(
                    "  •  Ctrl+R back  •  Ctrl+S forward  •  Enter accept  •  Esc cancel",
                    theme::dim(),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }

        let left = match &self.state {
            AppState::Idle if matches!(self.backtrack, BacktrackPhase::Selecting { .. }) => {
                "↑/↓ select message  •  enter to rewind  •  esc to cancel".to_string()
            }
            AppState::Idle if self.last_turn_errored && !self.queued_messages.is_empty() => {
                "enter to resume queue  •  esc to clear queue".to_string()
            }
            AppState::Idle
                if self.config.conversation.collaboration_mode == CollaborationMode::Plan =>
            {
                "[plan]  •  shift+tab to toggle off  •  ? for shortcuts".to_string()
            }
            AppState::Idle if self.composer.is_empty() => {
                "esc to rewind  •  ? for shortcuts  •  quit to exit".to_string()
            }
            AppState::Idle => {
                "? for shortcuts  •  pgup/pgdn to scroll  •  quit to exit".to_string()
            }
            AppState::Streaming { .. } => {
                let count = self.queued_messages.len();
                if count > 0 {
                    format!(
                        "esc to cancel (queue preserved)  •  alt+↑ edit last  •  ({count} queued)"
                    )
                } else {
                    "esc to cancel  •  enter to queue".to_string()
                }
            }
            AppState::AwaitingApproval { .. } => "y to approve  •  n to deny".to_string(),
            AppState::AwaitingInput { prompt, .. } => {
                format!("type your answer  •  enter to send  •  esc to skip  [{prompt}]")
            }
            AppState::PlanReview => {
                "shift+tab: cycle  •  1-3: jump  •  enter: confirm  •  esc: dismiss".to_string()
            }
            AppState::ConfirmingUninstall => "y to uninstall  •  N / enter to cancel".to_string(),
            AppState::TranscriptPager => {
                "↑/↓ scroll  •  pgup/pgdn jump  •  / search  •  n/N navigate  •  q close"
                    .to_string()
            }
        };

        let pct = self.compute_context_pct();
        let pct_style = if pct >= 95 {
            theme::error_style()
        } else if pct >= 80 {
            theme::warning_style()
        } else {
            theme::dim()
        };
        let line = Line::from(vec![
            Span::styled(format!(" ctx {pct}%"), pct_style),
            Span::styled(format!("  •  {left}"), theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    pub(super) fn compute_queue_preview_height(&self) -> u16 {
        let count = self.queued_messages.len();
        if count == 0 {
            return 0;
        }
        let shown = count.min(3) as u16;
        let overflow = if count > 3 { 1u16 } else { 0 };
        // header + shown messages + overflow + hint
        1 + shown + overflow + 1
    }

    fn render_queue_preview(&self, frame: &mut Frame, area: Rect) {
        let dim_italic = Style::default()
            .fg(theme::DIM_WHITE)
            .add_modifier(Modifier::ITALIC);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            " Queued messages:".to_string(),
            theme::dim(),
        )));

        let count = self.queued_messages.len();
        let shown = count.min(3);
        for (i, qm) in self.queued_messages.iter().take(shown).enumerate() {
            let is_last_shown = i + 1 == shown && count <= 3;
            let truncated = if qm.text.len() > 50 {
                let end = qm
                    .text
                    .char_indices()
                    .map(|(idx, _)| idx)
                    .take_while(|&idx| idx <= 47)
                    .last()
                    .unwrap_or(0);
                format!("{}...", &qm.text[..end])
            } else {
                qm.text.clone()
            };
            let img_badge = if !qm.images.is_empty() {
                format!(
                    " [{} image{}]",
                    qm.images.len(),
                    if qm.images.len() == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            let prefix = if is_last_shown {
                theme::TREE_END
            } else {
                theme::TREE_MID
            };
            let style = if i + 1 == count {
                // Last item overall: underline to hint Alt+Up editability
                dim_italic.add_modifier(Modifier::UNDERLINED)
            } else {
                dim_italic
            };
            lines.push(Line::from(Span::styled(
                format!("  {prefix} {}. {truncated}{img_badge}", i + 1),
                style,
            )));
        }

        if count > 3 {
            lines.push(Line::from(Span::styled(
                format!("  ... and {} more", count - 3),
                theme::dim(),
            )));
        }

        lines.push(Line::from(Span::styled(
            "  Alt+Up to edit last".to_string(),
            theme::dim(),
        )));

        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::history::HistoryCell;
    use borg_core::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Instant;

    fn make_app() -> App<'static> {
        App::new(Config::default(), None, None, None)
    }

    /// Populate the transcript with enough cells so that `total_lines > area.height`,
    /// making `max_scroll > 0` so `scroll_offset` isn't clamped to 0 on render.
    fn with_scrollable_content(app: &mut App<'static>) {
        for i in 0..40 {
            app.cells.push(HistoryCell::User {
                text: format!("user message {i}"),
            });
        }
    }

    fn render_transcript_to_string(app: &mut App<'static>, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, w, h);
                app.render_transcript(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    const HINT_SUBSTR: &str = "End to follow";

    #[test]
    fn paused_indicator_visible_while_streaming_and_scrolled_up() {
        let mut app = make_app();
        with_scrollable_content(&mut app);
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.auto_scroll = false;
        app.scroll_offset = 5;

        let rendered = render_transcript_to_string(&mut app, 80, 20);
        assert!(
            rendered.contains(HINT_SUBSTR),
            "expected hint '{HINT_SUBSTR}' in transcript, got:\n{rendered}"
        );
    }

    #[test]
    fn paused_indicator_hidden_when_autoscroll_enabled() {
        let mut app = make_app();
        with_scrollable_content(&mut app);
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.auto_scroll = true;
        app.scroll_offset = 5;

        let rendered = render_transcript_to_string(&mut app, 80, 20);
        assert!(
            !rendered.contains(HINT_SUBSTR),
            "hint must not appear when autoscroll is enabled"
        );
    }

    #[test]
    fn paused_indicator_hidden_when_not_streaming() {
        let mut app = make_app();
        with_scrollable_content(&mut app);
        app.state = AppState::Idle;
        app.auto_scroll = false;
        app.scroll_offset = 5;

        let rendered = render_transcript_to_string(&mut app, 80, 20);
        assert!(
            !rendered.contains(HINT_SUBSTR),
            "hint must not appear when idle (no streaming)"
        );
    }

    #[test]
    fn paused_indicator_hidden_when_at_bottom() {
        let mut app = make_app();
        with_scrollable_content(&mut app);
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.auto_scroll = false;
        app.scroll_offset = 0;

        let rendered = render_transcript_to_string(&mut app, 80, 20);
        assert!(
            !rendered.contains(HINT_SUBSTR),
            "hint must not appear when already at bottom"
        );
    }
}
