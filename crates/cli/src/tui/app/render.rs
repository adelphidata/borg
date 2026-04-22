use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;
use throbber_widgets_tui::{Throbber, BRAILLE_EIGHT};

use super::super::history::HistoryCell;
use super::super::{layout, shimmer, theme};
use super::{App, AppState, BacktrackPhase};
use borg_core::config::CollaborationMode;

impl<'a> App<'a> {
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let status_height = self.desired_status_height();
        let composer_height = self.composer.height();
        let queue_preview_height = self.compute_queue_preview_height();
        let app_layout =
            layout::compute_layout(area, composer_height, status_height, queue_preview_height);

        self.render_transcript(frame, app_layout.transcript);
        if status_height > 0 {
            self.render_status(frame, app_layout.status);
        }
        if queue_preview_height > 0 {
            self.render_queue_preview(frame, app_layout.queue_preview);
        }
        self.composer.render(frame, app_layout.composer);
        self.render_choices_overlay(frame, app_layout.composer);
        self.render_footer(frame, app_layout.footer);
        self.plan_overlay.render(frame, app_layout.composer);
        self.command_popup.render(frame, app_layout.composer);
        self.file_popup.render(frame, app_layout.composer);
        self.settings_popup.render(frame, &self.config);
        self.model_popup.render(frame);
        self.plugins_popup.render(frame);
        self.pairing_popup.render(frame);
        self.projects_popup.render(frame);
        self.sessions_popup.render(frame);
        self.schedule_popup.render(frame);
        self.migrate_popup.render(frame);
        self.status_popup.render(frame);
        self.btw_popup.render(frame);

        // Transcript pager (Ctrl+T) takes the full frame when active. Drawn
        // after popups so it covers them; before toasts so toasts still float on top.
        if matches!(self.state, AppState::TranscriptPager) {
            self.transcript_pager.render(frame, area, &self.cells);
        }

        // Toasts are drawn last so they float above every other overlay.
        self.toasts.prune_expired();
        self.toasts.render(frame, area);
    }

    /// Push the shared Borg card (the same one rendered at the top of the
    /// transcript) into the cell history. Used by `/card`.
    pub(crate) fn push_borg_card(&mut self) {
        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        let ambient = if self.config.evolution.ambient_header_enabled {
            self.ambient_status.as_ref()
        } else {
            None
        };
        let lines = build_borg_card_lines(name, &self.config.llm.model, ambient);
        self.cells.push(HistoryCell::Card { lines });
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

        // Always show branded header — same component as `/card`.
        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        let ambient = if self.config.evolution.ambient_header_enabled {
            self.ambient_status.as_ref()
        } else {
            None
        };
        all_lines.extend(build_borg_card_lines(name, &self.config.llm.model, ambient));
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

    /// Render the live streaming row (throbber + phase header + elapsed +
    /// optional details snippet). The header is driven by agent events
    /// (`stream_status.header`) — "Preparing", "Thinking", "Responding",
    /// "Running <tool>" — so the user has phase-level evidence the turn is
    /// still making progress even when text output hasn't started yet.
    fn render_streaming_status(&self, frame: &mut Frame, area: Rect, start: std::time::Instant) {
        use ratatui::text::Text;

        let elapsed = theme::format_elapsed(start.elapsed().as_secs());
        let throbber = Throbber::default()
            .throbber_set(BRAILLE_EIGHT)
            .throbber_style(theme::tool_style());
        let mut header_spans = vec![
            Span::raw(" "),
            throbber.to_symbol_span(&self.throbber_state),
        ];
        // Shimmer the live header so the eye has a visual cue distinct from
        // the static "esc to interrupt" hint. Colors come from the
        // terminal-adaptive palette so light terminals get a legible gradient.
        header_spans.extend(shimmer::shimmer_spans_auto(&format!(
            "{}...",
            self.stream_status.header
        )));
        header_spans.push(Span::styled(format!(" ({elapsed}"), theme::tool_style()));
        header_spans.push(Span::styled(" • esc to interrupt)", theme::dim()));

        let mut lines = vec![Line::from(header_spans)];
        if area.height >= 2 {
            if let Some(details) = self.stream_status.details.as_deref() {
                lines.push(Line::from(vec![
                    Span::styled("   └ ", theme::dim()),
                    Span::styled(details.to_string(), theme::dim()),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
    }

    /// How many rows the status bar wants for the current state. 0 when idle,
    /// 1 for single-line status, 2 when a live `stream_status.details` tail
    /// should be shown underneath the header.
    pub(super) fn desired_status_height(&self) -> u16 {
        match &self.state {
            AppState::Idle => 0,
            AppState::Streaming { .. } if self.stream_status.details.is_some() => 2,
            _ => 1,
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        if let AppState::Streaming { start, .. } = &self.state {
            // Streaming is the one state with variable height (header + optional
            // details), so render it through a dedicated path instead of
            // shoehorning both rows into a single `Line`.
            self.render_streaming_status(frame, area, *start);
            return;
        }
        let line = match &self.state {
            AppState::Streaming { .. } => unreachable!("handled above"),
            AppState::AwaitingApproval { .. } => Line::from(vec![Span::styled(
                format!(" {} Approval needed — press y or n", theme::BULLET),
                theme::error_style(),
            )]),
            AppState::AwaitingInput {
                choices,
                custom_mode,
                ..
            } => {
                let msg = if !choices.is_empty() && !custom_mode {
                    format!(" {} Agent needs your input — pick an option", theme::BULLET)
                } else {
                    format!(
                        " {} Agent needs your input — type and press enter",
                        theme::BULLET
                    )
                };
                Line::from(vec![Span::styled(msg, theme::tool_style())])
            }
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
            AppState::AwaitingInput {
                prompt,
                choices,
                custom_mode,
                allow_custom,
                ..
            } => {
                if !choices.is_empty() && !*custom_mode {
                    let tab_hint = if *allow_custom {
                        "  •  tab to type answer"
                    } else {
                        ""
                    };
                    format!(
                        "↑/↓ select  •  1–{} quick pick  •  enter to confirm{tab_hint}  •  esc to skip  [{prompt}]",
                        choices.len().min(9)
                    )
                } else if !choices.is_empty() && *custom_mode {
                    format!(
                        "type your answer  •  enter to send  •  esc back to options  [{prompt}]"
                    )
                } else {
                    format!("type your answer  •  enter to send  •  esc to skip  [{prompt}]")
                }
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

    /// Overlay a selectable choice list on top of the composer area when the
    /// agent has asked `request_user_input` with `choices` and the user hasn't
    /// switched to free-text (custom) mode. A `Clear` widget hides the composer
    /// underneath so the options are the focal point.
    fn render_choices_overlay(&self, frame: &mut Frame, area: Rect) {
        let AppState::AwaitingInput {
            choices,
            cursor,
            custom_mode,
            ..
        } = &self.state
        else {
            return;
        };
        if choices.is_empty() || *custom_mode {
            return;
        }
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(choices.len());
        for (i, c) in choices.iter().enumerate() {
            let selected = i == *cursor;
            let marker = if selected { "▌ " } else { "  " };
            let number = format!("{}. ", i + 1);
            let base_style = if selected {
                theme::tool_style().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(number, theme::dim()),
                Span::styled(c.label.clone(), base_style),
            ];
            if let Some(desc) = c.description.as_deref() {
                spans.push(Span::styled(format!("  — {desc}"), theme::dim()));
            }
            lines.push(Line::from(spans));
        }
        let needed = lines.len() as u16;
        let h = needed.min(area.height);
        let overlay = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(h),
            width: area.width,
            height: h,
        };
        frame.render_widget(Clear, overlay);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), overlay);
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

/// Format the value portion of the `class:` line.
///
/// Shape: `{name} [the {Archetype}] Lv.{level}`. Pre-evolution
/// (`evolution_name` is `None`), `{name}` is the literal `"Base Borg"` and
/// the archetype epithet is suppressed entirely — archetype only attaches
/// to a real evolved name. Mood is rendered on its own `mood:` line, not
/// appended here.
pub(super) fn format_class_label(status: &super::AmbientStatus) -> String {
    let mut out = String::new();
    match status.evolution_name.as_deref() {
        Some(name) => {
            out.push_str(name);
            if let Some(arch) = status.archetype {
                let arch_str = arch.to_string();
                let mut chars = arch_str.chars();
                let titled = match chars.next() {
                    Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                };
                out.push_str(" the ");
                out.push_str(&titled);
            }
        }
        None => out.push_str("Base Borg"),
    }
    out.push_str(&format!(" Lv.{}", status.level));
    out
}

/// Build the styled Borg card lines used by both the startup header and the
/// `/card` slash command — a single component so the two surfaces stay in
/// sync. `ambient` is `None` when evolution data is unavailable or the
/// ambient header is disabled, in which case `class:` / `mood:` are omitted.
pub(super) fn build_borg_card_lines(
    name: &str,
    model: &str,
    ambient: Option<&super::AmbientStatus>,
) -> Vec<Line<'static>> {
    let version = env!("CARGO_PKG_VERSION");
    let teal = Style::default().fg(theme::CYAN);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("BORG", teal.add_modifier(Modifier::BOLD)),
        Span::from(" "),
        Span::styled(format!("v{version}"), theme::dim()),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("name:  ", theme::dim()),
        Span::styled(name.to_string(), teal),
    ]));
    lines.push(Line::from(vec![
        Span::styled("model: ", theme::dim()),
        Span::styled(model.to_string(), teal),
    ]));
    if let Some(ambient) = ambient {
        lines.push(Line::from(vec![
            Span::styled("class: ", theme::dim()),
            Span::styled(format_class_label(ambient), teal),
        ]));
        lines.push(Line::from(vec![
            Span::styled("mood:  ", theme::dim()),
            Span::styled(ambient.mood.to_string(), teal),
        ]));
    }
    lines
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

    use super::super::AmbientStatus;
    use borg_core::evolution::{Archetype, Mood};

    fn mk_ambient(
        name: Option<&str>,
        level: u8,
        mood: Mood,
        arch: Option<Archetype>,
    ) -> AmbientStatus {
        AmbientStatus {
            evolution_name: name.map(String::from),
            level,
            mood,
            archetype: arch,
        }
    }

    #[test]
    fn class_label_pre_evolution_hides_archetype() {
        let s = mk_ambient(None, 1, Mood::Stable, Some(Archetype::Builder));
        assert_eq!(format_class_label(&s), "Base Borg Lv.1");
    }

    #[test]
    fn class_label_pre_evolution_no_archetype() {
        let s = mk_ambient(None, 1, Mood::Drifting, None);
        assert_eq!(format_class_label(&s), "Base Borg Lv.1");
    }

    #[test]
    fn class_label_post_evolution_with_archetype() {
        let s = mk_ambient(
            Some("Oppenborger"),
            5,
            Mood::Focused,
            Some(Archetype::Builder),
        );
        assert_eq!(format_class_label(&s), "Oppenborger the Builder Lv.5");
    }

    #[test]
    fn class_label_post_evolution_no_archetype() {
        let s = mk_ambient(Some("Oppenborger"), 5, Mood::Focused, None);
        assert_eq!(format_class_label(&s), "Oppenborger Lv.5");
    }

    #[test]
    fn borg_card_renders_mood_on_its_own_line() {
        let s = mk_ambient(Some("Oppenborger"), 2, Mood::Stable, Some(Archetype::Ops));
        let lines = build_borg_card_lines("Oppenborger", "x-ai/grok", Some(&s));
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(rendered
            .iter()
            .any(|l| l == "class: Oppenborger the Ops Lv.2"));
        assert!(rendered.iter().any(|l| l == "mood:  stable"));
        // Mood must NOT appear on the class line — it lives on its own.
        assert!(rendered.iter().all(|l| !l.contains("(stable)")));
    }

    // ========================================================================
    // desired_status_height — governs whether a second row is allocated for
    // the live details snippet; layout depends on this number directly.
    // ========================================================================

    #[test]
    fn desired_status_height_zero_when_idle() {
        let app = make_app();
        assert_eq!(app.desired_status_height(), 0);
    }

    #[test]
    fn desired_status_height_one_while_streaming_without_details() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        assert!(app.stream_status.details.is_none());
        assert_eq!(app.desired_status_height(), 1);
    }

    #[test]
    fn desired_status_height_two_when_streaming_has_details() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.stream_status
            .set_details(Some("cargo build --release".into()));
        // A second row is required so the ellipsized details tail has
        // somewhere to render; without this the layout module silently
        // clamps the details to invisible.
        assert_eq!(app.desired_status_height(), 2);
    }

    #[test]
    fn desired_status_height_one_for_non_streaming_states() {
        let mut app = make_app();
        app.state = AppState::PlanReview;
        assert_eq!(app.desired_status_height(), 1);
        app.state = AppState::ConfirmingUninstall;
        assert_eq!(app.desired_status_height(), 1);
    }

    /// Render the status bar into a TestBackend and return each row as a string.
    fn render_status_rows(app: &App<'static>, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, w, h);
                app.render_status(frame, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn render_status_shows_live_header_text_while_streaming() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.stream_status.set_header("Thinking");
        let rows = render_status_rows(&app, 80, 1);
        // Liveness check: the phase label must actually appear on-screen.
        // "Working..." alone would not be evidence of the header field
        // flowing through to the renderer.
        assert!(
            rows[0].contains("Thinking"),
            "expected 'Thinking' in status row, got: {rows:?}"
        );
    }

    #[test]
    fn render_status_renders_details_on_second_row_when_height_allows() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.stream_status.set_header("Running run_shell");
        app.stream_status
            .set_details(Some("cargo test --all".into()));
        let rows = render_status_rows(&app, 80, 2);
        assert!(
            rows[0].contains("Running run_shell"),
            "header row: {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains("cargo test --all"),
            "details row must show the tool-arg snippet, got: {:?}",
            rows[1]
        );
    }

    #[test]
    fn render_status_suppresses_details_when_only_one_row_available() {
        // If the layout gave us only one row, we must not silently clip the
        // details line into the header row — that would mangle the header.
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.stream_status.set_header("Thinking");
        app.stream_status
            .set_details(Some("should not be rendered".into()));
        let rows = render_status_rows(&app, 80, 1);
        assert!(rows[0].contains("Thinking"));
        assert!(
            !rows[0].contains("should not be rendered"),
            "details must not bleed into the header row"
        );
    }

    #[test]
    fn borg_card_without_ambient_omits_class_and_mood() {
        let lines = build_borg_card_lines("Borg", "x-ai/grok", None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(rendered.iter().any(|l| l.starts_with("BORG ")));
        assert!(rendered.iter().any(|l| l == "name:  Borg"));
        assert!(rendered.iter().all(|l| !l.starts_with("class:")));
        assert!(rendered.iter().all(|l| !l.starts_with("mood:")));
    }
}
