use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::super::custom_terminal::Frame;
use throbber_widgets_tui::{Throbber, BRAILLE_EIGHT};
use unicode_width::UnicodeWidthStr;

/// One footer hint with a drop-priority. Higher = keep longer when the
/// terminal narrows. Hints render in original (insertion) order; the
/// width-fitter drops the lowest-priority entries first.
struct FooterHint {
    text: String,
    priority: u8,
}

const HINT_SEP: &str = "  •  ";

/// Greedy width-aware fit: keep dropping the lowest-priority hint until
/// the rendered string (including ` • ` separators) fits in `available`.
/// Hints render in their original order.
fn fit_footer_hints(mut hints: Vec<FooterHint>, available: usize) -> String {
    loop {
        let rendered = hints
            .iter()
            .map(|h| h.text.as_str())
            .collect::<Vec<_>>()
            .join(HINT_SEP);
        if hints.is_empty() || UnicodeWidthStr::width(rendered.as_str()) <= available {
            return rendered;
        }
        // Drop the lowest-priority hint (ties: drop the rightmost).
        let mut victim = 0usize;
        let mut worst = u8::MAX;
        for (i, h) in hints.iter().enumerate() {
            if h.priority <= worst {
                worst = h.priority;
                victim = i;
            }
        }
        hints.remove(victim);
    }
}

use super::super::history::HistoryCell;
use super::super::{layout, shimmer, theme};
use super::{App, AppState, BacktrackPhase};
use cortex::config::CollaborationMode;

impl<'a> App<'a> {
    fn needs_inter_cell_spacer(prev: &HistoryCell, next: &HistoryCell) -> bool {
        if next.is_stream_continuation() {
            return false;
        }
        !(prev.is_tool_flow() && next.is_tool_flow())
    }

    /// Render the inline viewport: only un-flushed cells (`[cells_emitted..]`)
    /// plus chrome (status, composer, popups, footer, toasts). Cells already
    /// emitted to scrollback via `insert_history_lines` are NEVER re-rendered
    /// here — the terminal owns that history now.
    pub fn render_viewport(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let status_height = self.desired_status_height();
        let composer_height = self.composer.height();
        let queue_preview_height = self.compute_queue_preview_height(area.width);
        let app_layout =
            layout::compute_layout(area, composer_height, status_height, queue_preview_height);

        self.render_pending_cells(frame, app_layout.transcript);
        if status_height > 0 {
            self.render_status(frame, app_layout.status);
        }
        if queue_preview_height > 0 {
            self.render_queue_preview(frame, app_layout.queue_preview);
        }
        self.composer.render(frame, app_layout.composer);
        // BottomPane: when a modal view is active, give it the bottom slice
        // of the screen (composer rect plus enough rows above to satisfy
        // its desired_height). This is the Phase 7 hook for the new
        // request_user_input picker — over-paints the transcript above the
        // composer when the picker is larger than the composer's own
        // height. The view's render path Clears its rect first.
        if self.bottom_pane.is_active() {
            let desired = self.bottom_pane.desired_height(area.width);
            // Anchor the overlay to the bottom of the composer; grow
            // upward as needed. Clamp to the screen area so we never
            // render outside the terminal.
            let bottom = app_layout
                .composer
                .y
                .saturating_add(app_layout.composer.height);
            let top = bottom.saturating_sub(desired).max(area.y);
            let overlay_rect = ratatui::layout::Rect {
                x: area.x,
                y: top,
                width: area.width,
                height: bottom.saturating_sub(top),
            };
            self.bottom_pane.render(frame, overlay_rect);
        }
        if app_layout.footer.height > 0 {
            self.render_footer(frame, app_layout.footer);
        }
        self.plan_overlay.render(frame, app_layout.composer);
        self.command_popup.render(frame, app_layout.composer);
        self.file_popup.render(frame, app_layout.composer);
        self.settings_popup.render(frame, &self.config);
        self.model_popup.render(frame);
        self.theme_popup.render(frame);
        self.plugins_popup.render(frame);
        self.mcp_popup.render(frame);
        self.pairing_popup.render(frame);
        self.sessions_popup.render(frame);
        self.schedule_popup.render(frame);
        self.migrate_popup.render(frame);
        self.status_popup.render(frame);
        self.usage_popup.render(frame);
        self.subprocesses_popup.render(frame);
        self.agents_popup.render(frame);
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
    ///
    /// The card lines are also written directly to `pending_scrollback` and
    /// `cells_emitted` is advanced past the new cell so the viewport never
    /// renders the card as its live tail. Without this, the inline viewport
    /// — bounded to a few rows by `compute_viewport_height` — clips
    /// everything past the first 2-3 lines of the card (`flush_stable_cells`
    /// deliberately skips the tail to keep streaming cells mutable). The
    /// startup banner avoided this by writing to scrollback directly; `/card`
    /// must mirror that or it appears truncated to "BORG vX.Y.Z" + name only.
    ///
    /// A leading blank `Line` is prepended when there is already a preceding
    /// cell — mirroring the inter-cell spacer that `render_cell_lines` would
    /// have inserted. Skipped when the card is the first thing in the
    /// transcript so `/card` as the opening action doesn't double-blank with
    /// the startup banner's trailing separator.
    pub(crate) fn push_borg_card(&mut self) {
        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        let ambient = if self.config.evolution.ambient_header_enabled {
            self.ambient_status.as_ref()
        } else {
            None
        };
        let lines = build_borg_card_lines(
            name,
            &self.config.llm.model,
            ambient,
            self.pending_update_version.as_deref(),
        );
        if !self.cells.is_empty() {
            self.pending_scrollback.push(Line::default());
        }
        self.pending_scrollback.extend(lines.clone());
        self.cells.push(HistoryCell::Card { lines });
        // Mark the just-pushed Card as already emitted: the viewport's
        // tail-render path would otherwise show only the first `viewport_height`
        // rows of the card. Advancing `cells_emitted` past it tells
        // `render_pending_cells` that this cell has already gone to scrollback.
        self.cells_emitted = self.cells.len();
    }

    pub(super) fn compute_context_pct(&self) -> u8 {
        // Reflect the *current* context window — the prompt size of the most
        // recent LLM request — not cumulative session usage. Summing
        // prompt+completion across turns saturates at 100% almost
        // immediately and never recovers after autocompaction, which
        // misleadingly looks like compaction never ran. See
        // crates/interface/src/tui/app/mod.rs `last_prompt_tokens`.
        let max = self.config.conversation.max_history_tokens;
        if max == 0 {
            return 0;
        }
        ((self.last_prompt_tokens as f64 / max as f64) * 100.0).min(100.0) as u8
    }

    /// Render only un-flushed cells (`[cells_emitted..]`) into the upper
    /// transcript region. Cells `< cells_emitted` are already in terminal
    /// scrollback (emitted via `insert_history_lines`) — the terminal owns
    /// scrollback now, so nothing app-side scrolls them.
    fn render_pending_cells(&mut self, frame: &mut Frame, area: Rect) {
        self.last_viewport_width = area.width;
        // Branded header is flushed exactly once into scrollback on the first
        // frame, never rendered in the viewport itself.
        self.flush_branded_header_once();

        let width = area.width;
        let throbber_state = match &self.state {
            AppState::Streaming { .. } => Some(&self.throbber_state),
            _ => None,
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        let start = self.cells_emitted.min(self.cells.len());
        for (offset, cell) in self.cells[start..].iter().enumerate() {
            // Skip the inter-cell separator on the very first cell of this
            // render pass — there's nothing above it in the viewport, even
            // if there are flushed cells in scrollback.
            if offset > 0 {
                let prev = &self.cells[start + offset - 1];
                if Self::needs_inter_cell_spacer(prev, cell) {
                    lines.push(Line::default());
                }
            }
            lines.extend(cell.render(width, throbber_state));
        }

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// Render the live streaming row (throbber + phase header + elapsed +
    /// optional details snippet). The header is driven by agent events
    /// (`stream_status.header`) — "Preparing", "Thinking", "Responding",
    /// "Running `<tool>`" — so the user has phase-level evidence the turn is
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

        // Hint priorities (higher = keep longer when terminal narrows):
        //   100 = critical action keys (esc to cancel, y/n approval)
        //    90 = primary state action (enter to confirm/resume/rewind)
        //    80 = secondary action (queue count, edit last)
        //    50 = mode label / state-bound info
        //    40 = `? for shortcuts`
        //    30 = `quit to exit`
        //    20 = `esc to rewind`
        let hints: Vec<FooterHint> = match &self.state {
            AppState::Idle if matches!(self.backtrack, BacktrackPhase::Selecting { .. }) => vec![
                FooterHint {
                    text: "↑/↓ select message".into(),
                    priority: 90,
                },
                FooterHint {
                    text: "enter to rewind".into(),
                    priority: 90,
                },
                FooterHint {
                    text: "esc to cancel".into(),
                    priority: 100,
                },
            ],
            AppState::Idle if self.last_turn_errored && !self.queued_messages.is_empty() => vec![
                FooterHint {
                    text: "enter to resume queue".into(),
                    priority: 90,
                },
                FooterHint {
                    text: "esc to clear queue".into(),
                    priority: 100,
                },
            ],
            AppState::Idle
                if self.config.conversation.collaboration_mode == CollaborationMode::Plan =>
            {
                vec![
                    FooterHint {
                        text: "[plan]".into(),
                        priority: 50,
                    },
                    FooterHint {
                        text: "shift+tab to toggle off".into(),
                        priority: 90,
                    },
                    FooterHint {
                        text: "? for shortcuts".into(),
                        priority: 40,
                    },
                ]
            }
            AppState::Idle if self.composer.is_empty() => vec![
                FooterHint {
                    text: "esc to rewind".into(),
                    priority: 20,
                },
                FooterHint {
                    text: "? for shortcuts".into(),
                    priority: 40,
                },
                FooterHint {
                    text: "quit to exit".into(),
                    priority: 30,
                },
            ],
            AppState::Idle => vec![
                FooterHint {
                    text: "? for shortcuts".into(),
                    priority: 40,
                },
                FooterHint {
                    text: "pgup/pgdn to scroll".into(),
                    priority: 30,
                },
                FooterHint {
                    text: "quit to exit".into(),
                    priority: 30,
                },
            ],
            AppState::Streaming { .. } => {
                let count = self.queued_messages.len();
                if count > 0 {
                    vec![
                        FooterHint {
                            text: "esc to cancel (queue preserved)".into(),
                            priority: 100,
                        },
                        FooterHint {
                            text: "alt+↑ edit last".into(),
                            priority: 50,
                        },
                        FooterHint {
                            text: format!("({count} queued)"),
                            priority: 80,
                        },
                    ]
                } else {
                    vec![
                        FooterHint {
                            text: "esc to cancel".into(),
                            priority: 100,
                        },
                        FooterHint {
                            text: "enter to queue".into(),
                            priority: 80,
                        },
                    ]
                }
            }
            AppState::AwaitingApproval { .. } => vec![
                FooterHint {
                    text: "y to approve".into(),
                    priority: 100,
                },
                FooterHint {
                    text: "n to deny".into(),
                    priority: 100,
                },
            ],
            AppState::PlanReview => vec![
                FooterHint {
                    text: "shift+tab: cycle".into(),
                    priority: 80,
                },
                FooterHint {
                    text: "1-3: jump".into(),
                    priority: 60,
                },
                FooterHint {
                    text: "enter: confirm".into(),
                    priority: 90,
                },
                FooterHint {
                    text: "esc: dismiss".into(),
                    priority: 100,
                },
            ],
            AppState::ConfirmingUninstall => vec![
                FooterHint {
                    text: "y to uninstall".into(),
                    priority: 100,
                },
                FooterHint {
                    text: "N / enter to cancel".into(),
                    priority: 100,
                },
            ],
            AppState::TranscriptPager => vec![
                FooterHint {
                    text: "↑/↓ scroll".into(),
                    priority: 80,
                },
                FooterHint {
                    text: "pgup/pgdn jump".into(),
                    priority: 60,
                },
                FooterHint {
                    text: "/ search".into(),
                    priority: 70,
                },
                FooterHint {
                    text: "n/N navigate".into(),
                    priority: 50,
                },
                FooterHint {
                    text: "q close".into(),
                    priority: 100,
                },
            ],
        };

        // Voice-mode level meter. When `voice_status == Listening`, render
        // a small 10-cell RMS bar so the user sees the mic is hot. The
        // bar fills proportional to peak RMS (0..=32767, but speech
        // normally peaks around 3000-8000). High-priority so it doesn't
        // get culled when the terminal is narrow.
        let mut hints = hints;
        if matches!(
            self.voice_status,
            Some(cortex::media::voice_loop::VoiceStatus::Listening)
        ) {
            if let Some(vl) = self.voice_loop.as_ref() {
                let peak = vl.peak_rms();
                // Speech normally tops out around 6000-8000; clamp the
                // bar range so a normal voice fills most of the bar
                // without quiet speech showing as zero. 6000 ≈ full.
                const BAR_FULL_RMS: u32 = 6_000;
                const BAR_CELLS: u32 = 10;
                let filled = (u32::from(peak) * BAR_CELLS / BAR_FULL_RMS).min(BAR_CELLS);
                let bar: String =
                    "█".repeat(filled as usize) + &"░".repeat((BAR_CELLS - filled) as usize);
                hints.push(FooterHint {
                    text: format!("🎤 {bar}"),
                    priority: 95,
                });
            }
        }

        // Goal-state hint. Surfaces the two cases the operator most needs to
        // notice — Paused (Ralph loop is dormant; user needs to `/goal
        // resume`) and BudgetLimited (auto-continuation stopped; user needs
        // to bump the budget or wrap up). Active and Complete are silent
        // because the regular tooling already makes them visible (the agent
        // is mid-turn or the goal is done). Pushed onto `hints` so it
        // participates in priority-based fitting when the terminal narrows.
        match self.goal_status {
            Some(cortex::goals::GoalStatus::Paused) => hints.push(FooterHint {
                text: "Goal paused (/goal resume)".into(),
                priority: 80,
            }),
            Some(cortex::goals::GoalStatus::BudgetLimited) => hints.push(FooterHint {
                text: "Goal budget reached".into(),
                priority: 75,
            }),
            _ => {}
        }

        let pct = self.compute_context_pct();
        let pct_style = if pct >= 95 {
            theme::error_style()
        } else if pct >= 80 {
            theme::warning_style()
        } else {
            theme::dim()
        };

        // Reserve room for ` ctx NN%  •  ` on the left so hints can fit.
        let prefix = format!(" ctx {pct}%");
        let prefix_w = UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(HINT_SEP);
        let total_w = area.width as usize;
        let available = total_w.saturating_sub(prefix_w);
        let left = fit_footer_hints(hints, available);
        let line = Line::from(vec![
            Span::styled(prefix, pct_style),
            Span::styled(format!("{HINT_SEP}{left}"), theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    pub(super) fn compute_queue_preview_height(&self, area_width: u16) -> u16 {
        compute_queue_preview_height_for(&self.queued_messages, area_width)
    }

    fn render_queue_preview(&self, frame: &mut Frame, area: Rect) {
        let lines = build_queue_preview_lines(&self.queued_messages, area.width);
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Compute the desired inline-viewport height. The viewport sits at the
    /// bottom of the terminal and holds chrome (status, composer, queue
    /// preview, footer) plus a bounded streaming-tail preview of in-flight
    /// cells. Capped at the terminal height.
    #[allow(dead_code)] // wired up in run_event_loop in commit 3
    pub fn compute_viewport_height(&self, terminal_size: Size) -> u16 {
        let composer = self.composer.height();
        let status = self.desired_status_height();
        let queue = self.compute_queue_preview_height(terminal_size.width);
        let footer: u16 = 1;
        let pending = self.cells.len().saturating_sub(self.cells_emitted) as u16;
        // Reserve viewport rows for the slash-command popup so it can render
        // its full slate above the composer. Without this, `popup_rect_above`
        // sees only ~1 row of slack and clips the popup to a single entry.
        let popup_rows = if self.command_popup.is_visible() {
            let items = self.command_popup.filtered().len() as u16;
            let visible = items.min(super::super::command_popup::MAX_VISIBLE_ITEMS);
            visible.saturating_add(2) // +2 for top/bottom borders
        } else {
            0
        };
        let chrome = composer
            .saturating_add(status.min(1))
            .saturating_add(if terminal_size.width < 72 { 0 } else { queue })
            .saturating_add(if terminal_size.width < 56 { 0 } else { footer })
            .saturating_add(popup_rows);
        let available_tail = terminal_size.height.saturating_sub(chrome).max(1);
        let max_tail = (terminal_size.height / 2).max(1).min(available_tail);
        let min_tail = if available_tail >= 3 { 3 } else { 1 };
        let tail = pending.min(max_tail).max(min_tail);
        let total = chrome.saturating_add(tail);
        total.min(terminal_size.height)
    }

    /// Render lines for a single cell at index `idx`, with the same
    /// inter-cell separator policy used by `render_pending_cells`.
    fn render_cell_lines(&self, idx: usize, width: u16) -> Vec<Line<'static>> {
        let cell = &self.cells[idx];
        let mut out: Vec<Line<'static>> = Vec::new();
        if idx > 0 {
            let prev = &self.cells[idx - 1];
            if Self::needs_inter_cell_spacer(prev, cell) {
                out.push(Line::default());
            }
        }
        out.extend(cell.render(width, None));
        out
    }

    pub(super) fn flush_cell_range_to_scrollback(&mut self, start: usize, end: usize, width: u16) {
        let end = end.min(self.cells.len());
        for idx in start.min(end)..end {
            let lines = self.render_cell_lines(idx, width);
            self.pending_scrollback.extend(lines);
        }
        self.cells_emitted = end;
    }

    /// Append the borg branded header to `pending_scrollback` exactly once,
    /// so it renders into terminal scrollback above the very first viewport
    /// frame and is never re-drawn.
    pub fn flush_branded_header_once(&mut self) {
        if self.banner_emitted {
            return;
        }
        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        let ambient = if self.config.evolution.ambient_header_enabled {
            self.ambient_status.as_ref()
        } else {
            None
        };
        let mut lines = build_borg_card_lines(
            name,
            &self.config.llm.model,
            ambient,
            self.pending_update_version.as_deref(),
        );
        lines.push(Line::default());
        self.pending_scrollback.extend(lines);
        self.banner_emitted = true;
    }

    /// Walk newly-stable cells out of the live viewport and into
    /// `pending_scrollback`. A cell is stable iff it can no longer be
    /// mutated by an in-flight stream (see the predicate in
    /// `inline-viewport-port.md` §"Stability predicate").
    ///
    /// The current cell at `cells.len() - 1` is always treated as the live
    /// tail and never flushed here — `handle_event_turn_complete` is the one
    /// site that finalizes the tail.
    pub fn flush_stable_cells(&mut self, width: u16) {
        let len = self.cells.len();
        if len == 0 || self.cells_emitted >= len {
            return;
        }
        // Walk forward from `cells_emitted`; stop on the first non-stable
        // cell or on the live tail. The tail (last cell) is always reserved.
        let mut stable_end = self.cells_emitted;
        while stable_end < len.saturating_sub(1) {
            if !self.is_cell_stable(stable_end) {
                break;
            }
            stable_end += 1;
        }
        // Also check whether earlier cells whose predicate fails would block
        // later stable cells: by construction we only emit a contiguous
        // prefix, so once we stop we stop.
        self.flush_cell_range_to_scrollback(self.cells_emitted, stable_end, width);
    }

    /// True if the cell at `idx` is no longer subject to mutation by an
    /// in-flight stream.
    fn is_cell_stable(&self, idx: usize) -> bool {
        use super::super::history::HistoryCell;
        match &self.cells[idx] {
            // Thinking placeholders accept further deltas.
            HistoryCell::Thinking { .. } => false,
            // An open ToolStart can still be mutated by a later ToolResult.
            HistoryCell::ToolStart { completed, .. } => *completed,
            // ToolStreaming is bound to its preceding open ToolStart and is
            // removed when the matching ToolResult arrives. It is only safe
            // to flush if every preceding ToolStart in the run has completed.
            HistoryCell::ToolStreaming { .. } => self.cells[..idx].iter().all(|c| {
                !matches!(
                    c,
                    HistoryCell::ToolStart {
                        completed: false,
                        ..
                    }
                )
            }),
            // Streaming Assistant cells (still receiving deltas) are not stable.
            HistoryCell::Assistant { streaming, .. } => !*streaming,
            _ => true,
        }
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
    pending_update_version: Option<&str>,
) -> Vec<Line<'static>> {
    let version = env!("CARGO_PKG_VERSION");
    let teal = Style::default().fg(theme::cyan());
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
    if let Some(version) = pending_update_version {
        lines.push(Line::from(vec![
            Span::styled("update: ", theme::dim()),
            Span::styled(format!("v{version} installed; restart required"), teal),
        ]));
    }
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

/// Wrap a queued message into up to `max_lines` body lines for the
/// preview, given the inner width available after the prefix indent.
/// Returns the wrapped lines and `true` if the message overflowed (the
/// caller appends a `…` line in that case).
fn wrap_queued_message(text: &str, inner_width: usize, max_lines: usize) -> (Vec<String>, bool) {
    if text.is_empty() {
        return (vec![String::new()], false);
    }
    let width = inner_width.max(1);
    let wrapped: Vec<String> = textwrap::wrap(text, width)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    if wrapped.len() > max_lines {
        (wrapped.into_iter().take(max_lines).collect(), true)
    } else {
        (wrapped, false)
    }
}

fn image_badge(count: usize) -> String {
    if count == 0 {
        String::new()
    } else if count == 1 {
        " [1 image]".to_string()
    } else {
        format!(" [{count} images]")
    }
}

const QUEUE_FIRST_PREFIX: &str = "  ↳ ";
const QUEUE_CONT_PREFIX: &str = "    ";
const QUEUE_PREFIX_WIDTH: u16 = 4;
const QUEUE_MAX_LINES_PER_MSG: usize = 3;
const QUEUE_MAX_VISIBLE: usize = 3;

pub(super) fn build_queue_preview_lines(
    queued: &std::collections::VecDeque<super::QueuedMessage>,
    area_width: u16,
) -> Vec<Line<'static>> {
    let dim_italic = Style::default()
        .fg(theme::dim_color())
        .add_modifier(Modifier::ITALIC);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if queued.is_empty() {
        return lines;
    }

    lines.push(Line::from(vec![
        Span::styled("• ", theme::dim()),
        Span::styled("Queued follow-up inputs", theme::dim()),
    ]));

    let inner = area_width.saturating_sub(QUEUE_PREFIX_WIDTH).max(1) as usize;
    for qm in queued.iter().take(QUEUE_MAX_VISIBLE) {
        let badge = image_badge(qm.images.len());
        let body = format!("{}{}", qm.text, badge);
        let (wrapped, overflowed) = wrap_queued_message(&body, inner, QUEUE_MAX_LINES_PER_MSG);
        for (i, segment) in wrapped.iter().enumerate() {
            let prefix = if i == 0 {
                QUEUE_FIRST_PREFIX
            } else {
                QUEUE_CONT_PREFIX
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), theme::dim()),
                Span::styled(segment.clone(), dim_italic),
            ]));
        }
        if overflowed {
            lines.push(Line::from(Span::styled(
                format!("{QUEUE_CONT_PREFIX}…"),
                dim_italic,
            )));
        }
    }

    lines.push(Line::from(Span::styled(
        format!("{QUEUE_CONT_PREFIX}⌥ + ↑ edit last queued message"),
        theme::dim(),
    )));

    lines
}

pub(super) fn compute_queue_preview_height_for(
    queued: &std::collections::VecDeque<super::QueuedMessage>,
    area_width: u16,
) -> u16 {
    if queued.is_empty() {
        return 0;
    }
    let inner = area_width.saturating_sub(QUEUE_PREFIX_WIDTH).max(1) as usize;
    let mut h: u16 = 1; // header
    for qm in queued.iter().take(QUEUE_MAX_VISIBLE) {
        let body = format!("{}{}", qm.text, image_badge(qm.images.len()));
        let (wrapped, overflowed) = wrap_queued_message(&body, inner, QUEUE_MAX_LINES_PER_MSG);
        h = h.saturating_add(wrapped.len() as u16);
        if overflowed {
            h = h.saturating_add(1);
        }
    }
    h.saturating_add(1) // hint
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::Config;
    use std::time::Instant;

    fn make_app() -> App<'static> {
        use crate::daemon_client::DaemonClient;
        use std::sync::Arc;
        use tokio::sync::Mutex;
        let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
        App::new(Config::default(), None, None, None, client, None)
    }

    use super::super::AmbientStatus;
    use cortex::evolution::{Archetype, Mood};

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

    /// `format_class_label` is the source of truth for the agent's
    /// title-card class line: pre-evolution always shows the "Base Borg"
    /// placeholder (archetype is hidden until evolved), post-evolution shows
    /// the chosen name plus archetype-when-set.
    #[test]
    fn class_label_per_evolution_state() {
        let cases: &[(Option<&str>, u8, Option<Archetype>, &str)] = &[
            // pre-evolution: archetype is irrelevant
            (None, 1, Some(Archetype::Builder), "Base Borg Lv.1"),
            (None, 1, None, "Base Borg Lv.1"),
            // post-evolution: archetype appears only when set
            (
                Some("Oppenborger"),
                5,
                Some(Archetype::Builder),
                "Oppenborger the Builder Lv.5",
            ),
            (Some("Oppenborger"), 5, None, "Oppenborger Lv.5"),
        ];
        for (name, level, arch, expected) in cases {
            let s = mk_ambient(*name, *level, Mood::Stable, *arch);
            assert_eq!(
                format_class_label(&s),
                *expected,
                "case: name={name:?} arch={arch:?}"
            );
        }
    }

    #[test]
    fn borg_card_renders_mood_on_its_own_line() {
        let s = mk_ambient(Some("Oppenborger"), 2, Mood::Stable, Some(Archetype::Ops));
        let lines = build_borg_card_lines("Oppenborger", "x-ai/grok", Some(&s), None);
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
    // flush_cell_range_to_scrollback — spacer between non-tool-flow cells.
    // ========================================================================

    #[test]
    fn flush_separates_doctor_warning_from_heartbeat_with_one_blank() {
        // Regression guard for the screenshot bug: doctor-warning card hugging
        // the heartbeat line in scrollback. Must produce *exactly one* blank
        // line between the two cells — zero would reproduce the original bug,
        // two would mean both `flush_cell_range_to_scrollback` and
        // `render_cell_lines` are each prepending a spacer.
        let mut app = make_app();
        app.cells.push(HistoryCell::DoctorWarning {
            warnings: vec!["A:b".into()],
        });
        app.cells.push(HistoryCell::Heartbeat {
            text: "hello".into(),
        });
        let before = app.pending_scrollback.len();
        app.flush_cell_range_to_scrollback(0, 2, 80);
        let flushed = &app.pending_scrollback[before..];

        let is_blank =
            |l: &Line<'static>| l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty());
        let heartbeat_idx = flushed
            .iter()
            .rposition(|l| l.spans.iter().any(|s| s.content.contains("[heartbeat]")))
            .expect("heartbeat line should be flushed");
        let mut consecutive_blanks = 0usize;
        for l in flushed[..heartbeat_idx].iter().rev() {
            if is_blank(l) {
                consecutive_blanks += 1;
            } else {
                break;
            }
        }
        assert_eq!(
            consecutive_blanks, 1,
            "expected exactly one blank between warning card and heartbeat, got: {flushed:?}"
        );
    }

    // ========================================================================
    // push_borg_card — `/card` bypasses the viewport flush path (the card is
    // taller than the inline viewport and would be clipped). It must mirror
    // the inter-cell spacer policy itself: leading blank when there's a prev
    // cell, no leading blank when it's the first thing in the transcript.
    // ========================================================================

    fn is_brand_line(l: &Line<'_>) -> bool {
        l.spans.iter().any(|s| s.content == "BORG")
    }

    fn is_blank_line(l: &Line<'_>) -> bool {
        l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty())
    }

    #[test]
    fn push_borg_card_prepends_spacer_after_prior_cell() {
        // Regression: screenshot showed the assistant message and `/card`
        // output running line-to-line with no separator. Any preceding cell
        // must produce exactly one blank line before the BORG brand row.
        let mut app = make_app();
        app.cells.push(HistoryCell::assistant("hi".into(), false));
        let before = app.pending_scrollback.len();
        app.push_borg_card();
        let flushed = &app.pending_scrollback[before..];

        assert!(
            !flushed.is_empty(),
            "push_borg_card should emit lines into pending_scrollback"
        );
        assert!(
            is_blank_line(&flushed[0]),
            "first appended line must be a blank spacer, got: {:?}",
            flushed[0]
        );
        let brand_idx = flushed
            .iter()
            .position(is_brand_line)
            .expect("BORG brand line must be present");
        assert_eq!(
            brand_idx, 1,
            "BORG brand should sit immediately after the single spacer; flushed = {flushed:?}"
        );
    }

    #[test]
    fn push_borg_card_no_leading_spacer_when_first() {
        // Symmetry guard: when `/card` is the very first action (no prior
        // cells), there is nothing to separate from. A leading blank here
        // would double up with the startup banner's trailing blank and look
        // wrong. The startup banner itself takes the same scrollback path
        // and intentionally omits a leading blank for the same reason.
        let mut app = make_app();
        assert!(app.cells.is_empty(), "precondition: fresh app");
        let before = app.pending_scrollback.len();
        app.push_borg_card();
        let flushed = &app.pending_scrollback[before..];

        assert!(
            !flushed.is_empty(),
            "push_borg_card should emit lines into pending_scrollback"
        );
        assert!(
            is_brand_line(&flushed[0]),
            "first appended line must be the BORG brand, not a spacer; flushed = {flushed:?}"
        );
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

    /// Render the status bar into a buffer and return each row as a string.
    fn render_status_rows(app: &App<'static>, w: u16, h: u16) -> Vec<String> {
        use ratatui::buffer::Buffer;
        let area = Rect::new(0, 0, w, h);
        let mut buffer = Buffer::empty(area);
        {
            let mut frame = super::super::super::custom_terminal::Frame::__test_new(&mut buffer);
            app.render_status(&mut frame, area);
        }
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
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
    fn fit_footer_hints_keeps_all_when_room() {
        let hints = vec![
            FooterHint {
                text: "alpha".into(),
                priority: 50,
            },
            FooterHint {
                text: "beta".into(),
                priority: 90,
            },
        ];
        let out = fit_footer_hints(hints, 80);
        assert_eq!(out, "alpha  •  beta");
    }

    #[test]
    fn fit_footer_hints_drops_lowest_priority_first() {
        let hints = vec![
            FooterHint {
                text: "low".into(),
                priority: 20,
            },
            FooterHint {
                text: "high".into(),
                priority: 100,
            },
            FooterHint {
                text: "mid".into(),
                priority: 50,
            },
        ];
        // Total joined = "low  •  high  •  mid" = 20 cols. Cap at 14 forces
        // a drop; the lowest-priority "low" must be the first to go.
        let out = fit_footer_hints(hints, 14);
        assert!(!out.contains("low"), "low should be dropped, got: {out}");
        assert!(out.contains("high"));
    }

    #[test]
    fn fit_footer_hints_preserves_original_order_after_drops() {
        // Same input, two budgets:
        // - 15 cols keeps `first` + `third` in original order; `drop me` (lowest
        //   priority) is the one eliminated.
        // - 5 cols keeps only `first` (priority 100).
        let mk = || {
            vec![
                FooterHint {
                    text: "first".into(),
                    priority: 100,
                },
                FooterHint {
                    text: "drop me".into(),
                    priority: 10,
                },
                FooterHint {
                    text: "third".into(),
                    priority: 90,
                },
            ]
        };
        assert_eq!(fit_footer_hints(mk(), 15), "first  •  third");
        assert_eq!(fit_footer_hints(mk(), 5), "first");
    }

    #[test]
    fn fit_footer_hints_zero_budget_drops_everything() {
        let hints = vec![FooterHint {
            text: "anything".into(),
            priority: 100,
        }];
        let out = fit_footer_hints(hints, 0);
        assert_eq!(out, "");
    }

    #[test]
    fn borg_card_without_ambient_omits_class_and_mood() {
        let lines = build_borg_card_lines("Borg", "x-ai/grok", None, None);
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

    #[test]
    fn borg_card_shows_pending_update_line() {
        let lines = build_borg_card_lines("Borg", "x-ai/grok", None, Some("0.6.0"));
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
            .any(|l| l == "update: v0.6.0 installed; restart required"));
    }

    #[test]
    fn push_borg_card_uses_pending_update_version() {
        let mut app = make_app();
        app.pending_update_version = Some("0.6.0".into());
        app.push_borg_card();
        let HistoryCell::Card { lines } = app.cells.last().expect("card cell") else {
            panic!("expected card cell");
        };
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
            .any(|l| l == "update: v0.6.0 installed; restart required"));
    }

    /// Regression: `/card` must put the *full* card into terminal scrollback
    /// (BORG header, blank, name, model, plus class/mood when ambient is set
    /// and update line when pending). Before the fix, the card was only
    /// pushed to `cells` and became the live-tail HistoryCell — the inline
    /// viewport clipped it to the first 2-3 rows, hiding model/class/mood.
    /// The startup banner takes the same `pending_scrollback` path; this
    /// test asserts `/card` matches that contract end-to-end.
    #[test]
    fn push_borg_card_emits_full_card_to_scrollback() {
        let mut app = make_app();
        app.config.user.agent_name = Some("Borgbusters".into());
        app.config.llm.model = "openai/gpt-5.4-nano".into();
        app.config.evolution.ambient_header_enabled = true;
        app.ambient_status = Some(mk_ambient(None, 0, Mood::Drifting, None));

        let before = app.pending_scrollback.len();
        app.push_borg_card();
        let flushed: Vec<String> = app.pending_scrollback[before..]
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(
            flushed.iter().any(|l| l.starts_with("BORG ")),
            "missing BORG header line; flushed = {flushed:?}"
        );
        assert!(
            flushed.iter().any(|l| l == "name:  Borgbusters"),
            "missing name line; flushed = {flushed:?}"
        );
        assert!(
            flushed.iter().any(|l| l == "model: openai/gpt-5.4-nano"),
            "missing model line — this was the original /card truncation bug; \
             flushed = {flushed:?}"
        );
        assert!(
            flushed.iter().any(|l| l.starts_with("class:")),
            "missing class line; flushed = {flushed:?}"
        );
        assert!(
            flushed.iter().any(|l| l.starts_with("mood:")),
            "missing mood line; flushed = {flushed:?}"
        );

        // The viewport must NOT re-render the card as its live tail —
        // `cells_emitted` should be at the very end of `cells`, otherwise
        // the small viewport area would clip the long card again.
        assert_eq!(
            app.cells_emitted,
            app.cells.len(),
            "Card cell was not marked emitted; viewport will clip the live tail"
        );
    }

    #[test]
    fn branded_header_uses_pending_update_version() {
        let mut app = make_app();
        app.pending_update_version = Some("0.6.0".into());
        app.flush_branded_header_once();
        let rendered: Vec<String> = app
            .pending_scrollback
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
            .any(|l| l == "update: v0.6.0 installed; restart required"));
    }

    #[test]
    fn compute_viewport_height_uses_collapsed_narrow_chrome() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.stream_status
            .set_details(Some("cargo test --workspace".into()));
        app.queued_messages.push_back(super::super::QueuedMessage {
            text: "queued".into(),
            images: Vec::new(),
        });

        let viewport = app.compute_viewport_height(Size::new(60, 20));

        // Narrow layout collapses the queue preview and status shrinks to one
        // row. With no pending transcript cells, the viewport is just the
        // collapsed chrome plus the 3-row minimum live tail.
        assert_eq!(viewport, 8);
    }

    #[test]
    fn compute_viewport_height_reserves_rows_for_command_popup() {
        // When the slash popup is visible, the viewport must grow enough to
        // give `popup_rect_above` slack to render the full slate. Without
        // this, the popup clips to a single visible entry above the composer.
        let mut app = make_app();
        let baseline = app.compute_viewport_height(Size::new(120, 30));

        app.command_popup.update_filter("/");
        assert!(app.command_popup.is_visible());
        let with_popup = app.compute_viewport_height(Size::new(120, 30));

        let popup_visible_items = (app.command_popup.filtered().len() as u16)
            .min(crate::tui::command_popup::MAX_VISIBLE_ITEMS);
        let expected_extra = popup_visible_items + 2; // borders
        assert_eq!(
            with_popup - baseline,
            expected_extra,
            "popup-open viewport should reserve {expected_extra} extra rows; \
             baseline={baseline}, with_popup={with_popup}"
        );
    }

    #[test]
    fn user_to_assistant_has_single_blank_spacer() {
        let mut app = make_app();
        app.cells.push(HistoryCell::User {
            text: "hello".to_string(),
        });
        app.cells
            .push(HistoryCell::assistant("hi".to_string(), false));

        let assistant_lines = app.render_cell_lines(1, 40);
        assert_eq!(assistant_lines.len(), 2);
        assert!(
            assistant_lines[0].spans.is_empty(),
            "first line must be a blank terminal-bg spacer, got: {:?}",
            assistant_lines[0]
        );
    }

    #[test]
    fn assistant_to_user_has_single_blank_spacer() {
        let mut app = make_app();
        app.cells
            .push(HistoryCell::assistant("hi".to_string(), false));
        app.cells.push(HistoryCell::User {
            text: "hello".to_string(),
        });

        let user_lines = app.render_cell_lines(1, 40);
        assert_eq!(user_lines.len(), 4);
        assert!(
            user_lines[0].spans.is_empty(),
            "first line must be a blank terminal-bg spacer, got: {:?}",
            user_lines[0]
        );
    }

    // ========================================================================
    // Queue preview rendering.
    // ========================================================================

    fn line_text(l: &Line<'static>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn make_queue(
        items: &[(&str, usize)],
    ) -> std::collections::VecDeque<super::super::QueuedMessage> {
        items
            .iter()
            .map(|(text, n_imgs)| super::super::QueuedMessage {
                text: (*text).to_string(),
                images: (0..*n_imgs)
                    .map(|_| crate::tui::composer::ImageAttachment {
                        data: vec![],
                        mime_type: "image/png".to_string(),
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn queue_preview_one_short_message() {
        let q = make_queue(&[("hi", 0)]);
        let lines = build_queue_preview_lines(&q, 60);
        assert_eq!(lines.len(), 3, "header + 1 body + hint");
        assert_eq!(line_text(&lines[0]), "• Queued follow-up inputs");
        assert_eq!(line_text(&lines[1]), "  ↳ hi");
        assert_eq!(line_text(&lines[2]), "    ⌥ + ↑ edit last queued message");

        // Body must be dim+italic.
        let body_style = lines[1].spans[1].style;
        assert!(body_style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(body_style.fg, Some(theme::dim_color()));
    }

    #[test]
    fn queue_preview_three_messages_all_visible() {
        let q = make_queue(&[("a", 0), ("b", 0), ("c", 0)]);
        let lines = build_queue_preview_lines(&q, 60);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "• Queued follow-up inputs");
        assert_eq!(texts[1], "  ↳ a");
        assert_eq!(texts[2], "  ↳ b");
        assert_eq!(texts[3], "  ↳ c");
        assert_eq!(texts[4], "    ⌥ + ↑ edit last queued message");
    }

    #[test]
    fn queue_preview_shows_only_first_three() {
        let q = make_queue(&[("aa", 0), ("bb", 0), ("cc", 0), ("dd", 0), ("ee", 0)]);
        let lines = build_queue_preview_lines(&q, 60);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t == "  ↳ aa"));
        assert!(texts.iter().any(|t| t == "  ↳ bb"));
        assert!(texts.iter().any(|t| t == "  ↳ cc"));
        assert!(!texts.iter().any(|t| t.contains("dd") || t.contains("ee")));
        // 5 lines: header + 3 bodies + hint.
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn queue_preview_truncates_long_message_with_ellipsis() {
        // inner width 16: a single 80-char message wraps to many lines and
        // must be capped at 3 wrapped lines + a "    …" overflow line.
        let long = "a ".repeat(40);
        let q = make_queue(&[(long.trim(), 0)]);
        let lines = build_queue_preview_lines(&q, 20);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.first().unwrap(), "• Queued follow-up inputs");
        assert!(
            texts.iter().any(|t| t == "    …"),
            "must include overflow ellipsis line: {texts:?}"
        );
        // First wrapped body line uses ↳ prefix; subsequent use the 4-space prefix.
        let first_body = texts.iter().find(|t| t.starts_with("  ↳ ")).unwrap();
        assert!(first_body.starts_with("  ↳ a"));
    }

    #[test]
    fn queue_preview_image_badge() {
        let q = make_queue(&[("see this", 1), ("two of them", 2)]);
        let lines = build_queue_preview_lines(&q, 60);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("[1 image]")));
        assert!(texts.iter().any(|t| t.contains("[2 images]")));
    }

    #[test]
    fn queue_preview_height_matches_rendered_lines() {
        // Height calc must agree with the actual line count produced.
        let long = "hello ".repeat(20);
        let cases: Vec<Vec<(&str, usize)>> = vec![
            vec![("a", 0)],
            vec![("a", 0), ("b", 0), ("c", 0)],
            vec![(long.trim_end(), 0)],
        ];
        for case in &cases {
            let q = make_queue(case);
            let lines = build_queue_preview_lines(&q, 30);
            let h = compute_queue_preview_height_for(&q, 30);
            assert_eq!(
                lines.len() as u16,
                h,
                "height mismatch for case {case:?}: lines={} h={}",
                lines.len(),
                h
            );
        }
    }

    #[test]
    fn queue_preview_empty_returns_no_lines() {
        let q: std::collections::VecDeque<super::super::QueuedMessage> =
            std::collections::VecDeque::new();
        let lines = build_queue_preview_lines(&q, 60);
        assert!(lines.is_empty());
        assert_eq!(compute_queue_preview_height_for(&q, 60), 0);
    }
}
