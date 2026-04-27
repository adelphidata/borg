//! Full-screen scrollable transcript pager with text search.
//!
//! Activated by Ctrl+T from the idle state. Renders the full session transcript
//! in its own state (`AppState::TranscriptPager`) so all keys route here while
//! it's open and the composer is bypassed.
//!
//! KEYBOARD-ONLY by hard invariant. This module must never reference
//! `Event::Mouse`, `MouseEventKind`, `EnableMouseCapture`, or any of the
//! `?1000h/?1002h/?1003h/?1006h` escape sequences — doing so would regress
//! native click+drag text selection in the terminal. Mouse-wheel scrolling
//! still works through xterm Alternate Scroll Mode (`?1007h`) which the
//! terminal translates into Up/Down arrow keys (handled here as scroll input).
//! See `crates/cli/src/tui/mod.rs` for the source-level guard tests.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::history::HistoryCell;
use super::theme;

/// Number of lines a PageUp/PageDown jump moves.
const PAGE_JUMP: usize = 20;

/// Outcome of routing a key event into the pager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerAction {
    /// Key consumed; no caller action required.
    None,
    /// Pager should be closed and the host should restore the prior state.
    Dismiss,
}

/// Active text-search state, present only while the user is searching.
#[derive(Debug, Clone, Default)]
struct SearchState {
    /// Query string (case folded only at compare time).
    query: String,
    /// True while the user is still typing the query (before Enter).
    editing: bool,
    /// Indices into the rendered line vector that match `query`.
    matches: Vec<usize>,
    /// Index into `matches` of the currently focused hit.
    current: usize,
    /// Cached version of the rendered lines used to compute matches; cleared
    /// on each render so a transcript update naturally re-runs the search.
    last_query_committed: String,
}

/// Scrollable transcript overlay with `/`-search.
#[derive(Debug, Default)]
pub struct TranscriptPager {
    pub visible: bool,
    /// Top-of-viewport line index into the rendered transcript.
    scroll_offset: usize,
    /// Last total content height seen — used to clamp scroll on resize/refresh.
    last_content_height: usize,
    /// Last viewport height (paragraph inner area) for PageUp/PageDown jumps.
    last_viewport_height: usize,
    search: Option<SearchState>,
}

impl TranscriptPager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the pager. Resets scroll to the bottom (most recent content) so the
    /// first thing the user sees is what they were looking at in the composer.
    pub fn show(&mut self) {
        self.visible = true;
        // Sentinel — `render` will clamp this to bottom-of-content on first paint.
        self.scroll_offset = usize::MAX;
        self.search = None;
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.search = None;
    }

    #[cfg(test)]
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    #[cfg(test)]
    pub fn is_searching(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.editing)
    }

    /// Route a key event. Returns whether the pager should be dismissed.
    pub fn handle_key(&mut self, key: KeyEvent) -> PagerAction {
        // ── Search-input mode: typed characters edit the query. ──
        if self.search.as_ref().is_some_and(|s| s.editing) {
            return self.handle_key_in_search_edit(key);
        }

        // ── Regular pager navigation. ──
        match (key.code, key.modifiers) {
            // Dismiss
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Esc, _)
            | (KeyCode::Char('t'), KeyModifiers::CONTROL) => PagerAction::Dismiss,

            // Vertical scroll
            (KeyCode::Up, _) => {
                self.scroll_up_by(1);
                PagerAction::None
            }
            (KeyCode::Down, _) => {
                self.scroll_down_by(1);
                PagerAction::None
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                self.scroll_up_by(PAGE_JUMP);
                PagerAction::None
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                self.scroll_down_by(PAGE_JUMP);
                PagerAction::None
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.scroll_offset = 0;
                PagerAction::None
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                self.scroll_offset = usize::MAX; // clamped by render
                PagerAction::None
            }

            // Search
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                self.search = Some(SearchState {
                    editing: true,
                    ..Default::default()
                });
                PagerAction::None
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) => {
                self.advance_match(1);
                PagerAction::None
            }
            (KeyCode::Char('N'), _) | (KeyCode::Char('p'), KeyModifiers::NONE) => {
                self.advance_match(-1);
                PagerAction::None
            }

            _ => PagerAction::None,
        }
    }

    fn handle_key_in_search_edit(&mut self, key: KeyEvent) -> PagerAction {
        let Some(state) = self.search.as_mut() else {
            return PagerAction::None;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                // Esc clears query but leaves pager open.
                self.search = None;
                PagerAction::None
            }
            (KeyCode::Enter, _) => {
                state.editing = false;
                state.last_query_committed = state.query.clone();
                PagerAction::None
            }
            (KeyCode::Backspace, _) => {
                state.query.pop();
                PagerAction::None
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
                PagerAction::None
            }
            _ => PagerAction::None,
        }
    }

    fn scroll_up_by(&mut self, n: usize) {
        // Resolve usize::MAX sentinel against last known height.
        let cur = self
            .scroll_offset
            .min(self.max_scroll_offset(self.last_content_height));
        self.scroll_offset = cur.saturating_sub(n);
    }

    fn scroll_down_by(&mut self, n: usize) {
        let max = self.max_scroll_offset(self.last_content_height);
        // Resolve usize::MAX sentinel: pin to bottom first, then attempt advance.
        let cur = self.scroll_offset.min(max);
        self.scroll_offset = cur.saturating_add(n).min(max);
    }

    fn max_scroll_offset(&self, content_height: usize) -> usize {
        content_height.saturating_sub(self.last_viewport_height.max(1))
    }

    fn advance_match(&mut self, delta: isize) {
        let Some(state) = self.search.as_mut() else {
            return;
        };
        if state.matches.is_empty() {
            return;
        }
        let len = state.matches.len() as isize;
        let next = ((state.current as isize) + delta).rem_euclid(len) as usize;
        state.current = next;
        // Pin scroll so the new match sits in the viewport.
        let line = state.matches[next];
        self.scroll_offset = line.saturating_sub(self.last_viewport_height / 2);
    }

    /// Build the rendered line vector from the host's history, run a search
    /// match pass if a query is committed, and paint the overlay.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, cells: &[HistoryCell]) {
        if !self.visible {
            return;
        }

        // Full-screen overlay.
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(self.title());
        let inner = block.inner(area);
        frame.render_widget(&block, area);

        // Render every history cell into a flat line vector.
        let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(cells.len() * 4);
        for cell in cells {
            for line in cell.render(inner.width, None) {
                all_lines.push(line);
            }
        }

        self.last_content_height = all_lines.len();
        self.last_viewport_height = inner.height as usize;

        // Recompute search matches when the committed query changes.
        if let Some(state) = self.search.as_mut() {
            if !state.editing && state.last_query_committed == state.query {
                // Re-derive matches against current line set in case transcript grew.
                state.matches = Self::find_matches(&all_lines, &state.query);
                if state.current >= state.matches.len() {
                    state.current = 0;
                }
            }
        }

        // Resolve usize::MAX sentinel to bottom-of-content.
        let max_off = self.max_scroll_offset(all_lines.len());
        if self.scroll_offset > max_off {
            self.scroll_offset = max_off;
        }

        let viewport = inner.height as usize;
        let start = self.scroll_offset;
        let end = (start + viewport).min(all_lines.len());

        // Highlight only the visible slice — O(viewport × matches_in_window),
        // not O(full_transcript). Membership lookups go through a HashSet so
        // a transcript with thousands of matches stays cheap to render.
        let mut visible: Vec<Line<'static>> = all_lines[start..end].to_vec();
        if let Some(state) = self.search.as_ref() {
            if !state.query.is_empty() {
                let q_lower = state.query.to_lowercase();
                let match_set: std::collections::HashSet<usize> =
                    state.matches.iter().copied().collect();
                let current_line = state.matches.get(state.current).copied();
                for (offset, line) in visible.iter_mut().enumerate() {
                    let abs = start + offset;
                    if !match_set.contains(&abs) {
                        continue;
                    }
                    let style = if Some(abs) == current_line {
                        theme::popup_selected().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::REVERSED)
                    };
                    *line = highlight_line(line.clone(), &q_lower, style);
                }
            }
        }

        let paragraph = Paragraph::new(visible);
        frame.render_widget(paragraph, inner);
    }

    fn title(&self) -> String {
        match &self.search {
            Some(state) if state.editing => format!(" Transcript — search: /{} ", state.query),
            Some(state) if !state.matches.is_empty() => format!(
                " Transcript — {} match{} for '{}' ({}/{}) ",
                state.matches.len(),
                if state.matches.len() == 1 { "" } else { "es" },
                state.query,
                state.current + 1,
                state.matches.len()
            ),
            Some(state) if !state.query.is_empty() => {
                format!(" Transcript — no matches for '{}' ", state.query)
            }
            _ => " Transcript (q/Esc/Ctrl+T close · / search · n/N nav) ".to_string(),
        }
    }

    fn find_matches(lines: &[Line<'static>], query: &str) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                if text.to_lowercase().contains(&needle) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Replace the line's spans with new spans where every case-insensitive match
/// of `needle_lower` is wrapped in `match_style`. Non-matching segments inherit
/// the original span style by re-using the leading span's style as a base.
///
/// Walks the text char-by-char so byte-length asymmetry between the original
/// text and its lowercased form (e.g. Turkish `İ` → 2 bytes vs `i\u{307}` →
/// 3 bytes) cannot produce non-UTF-8 slice boundaries or panic.
fn highlight_line(line: Line<'static>, needle_lower: &str, match_style: Style) -> Line<'static> {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let base_style = line.spans.first().map(|s| s.style).unwrap_or_default();

    if needle_lower.is_empty() {
        return Line::from(vec![Span::styled(text, base_style)]);
    }

    // Pre-compute (orig_byte_offset, lower_byte_offset) pairs at every char
    // boundary so we can map a substring in the lowered string back to byte
    // ranges in the original. Last entry is the (text.len(), lower.len()) end.
    let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(text.len() + 1);
    let mut lower = String::with_capacity(text.len());
    for (orig_pos, ch) in text.char_indices() {
        boundaries.push((orig_pos, lower.len()));
        for lc in ch.to_lowercase() {
            lower.push(lc);
        }
    }
    boundaries.push((text.len(), lower.len()));

    // Map a lower-byte index to the closest orig-byte index at or before it.
    // Falls back to text.len() if the index is past the end of the lower text.
    let lower_to_orig = |lower_idx: usize| -> usize {
        match boundaries.binary_search_by_key(&lower_idx, |&(_, l)| l) {
            Ok(i) => boundaries[i].0,
            Err(i) => boundaries[i.saturating_sub(1)].0,
        }
    };

    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut cursor_orig = 0usize;
    let mut search_from_lower = 0usize;
    while let Some(rel) = lower[search_from_lower..].find(needle_lower) {
        let lower_match_start = search_from_lower + rel;
        let lower_match_end = lower_match_start + needle_lower.len();
        let orig_start = lower_to_orig(lower_match_start);
        let mut orig_end = lower_to_orig(lower_match_end);

        // The lower-side match may end inside the lowered form of a single
        // original char (e.g. 'İ' → "i\u{307}" — searching for "i" lands at
        // lower offset 0 with end at offset 1, but both snap to the same
        // boundary in the original). In that case widen orig_end to the next
        // char boundary so we highlight the whole original grapheme.
        if orig_end <= orig_start {
            orig_end = boundaries
                .iter()
                .find(|&&(o, _)| o > orig_start)
                .map(|&(o, _)| o)
                .unwrap_or(text.len());
        }

        if orig_start > cursor_orig {
            new_spans.push(Span::styled(
                text[cursor_orig..orig_start].to_string(),
                base_style,
            ));
        }
        // It's possible an earlier match already covered through orig_end (e.g.
        // overlapping widened ranges). Skip if so.
        if orig_end > cursor_orig.max(orig_start) {
            new_spans.push(Span::styled(
                text[orig_start..orig_end].to_string(),
                match_style,
            ));
            cursor_orig = orig_end;
        }
        // Always advance the lower-side cursor so we don't loop on the same hit.
        search_from_lower = lower_match_end.max(search_from_lower + 1);
    }
    if cursor_orig < text.len() {
        new_spans.push(Span::styled(text[cursor_orig..].to_string(), base_style));
    }
    if new_spans.is_empty() {
        new_spans.push(Span::styled(text, base_style));
    }
    Line::from(new_spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn new_pager_not_visible() {
        let p = TranscriptPager::new();
        assert!(!p.visible);
        assert_eq!(p.scroll_offset, 0);
        assert!(p.search.is_none());
    }

    #[test]
    fn show_dismiss_toggle_visibility() {
        let mut p = TranscriptPager::new();
        p.show();
        assert!(p.visible);
        p.dismiss();
        assert!(!p.visible);
    }

    #[test]
    fn show_after_dismiss_clears_search() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        let _ = p.handle_key(key(KeyCode::Char('x')));
        assert!(p.is_searching());
        p.dismiss();
        p.show();
        assert!(p.search.is_none());
    }

    #[test]
    fn arrow_keys_scroll_one_line() {
        let mut p = TranscriptPager::new();
        p.show();
        // Pretend the renderer ran with 100 lines of content, 20-line viewport.
        p.last_content_height = 100;
        p.last_viewport_height = 20;
        p.scroll_offset = 50;

        let _ = p.handle_key(key(KeyCode::Up));
        assert_eq!(p.scroll_offset, 49);
        let _ = p.handle_key(key(KeyCode::Down));
        assert_eq!(p.scroll_offset, 50);
    }

    #[test]
    fn pageup_pagedown_jump_twenty_lines() {
        let mut p = TranscriptPager::new();
        p.show();
        p.last_content_height = 200;
        p.last_viewport_height = 20;
        p.scroll_offset = 100;

        let _ = p.handle_key(key(KeyCode::PageUp));
        assert_eq!(p.scroll_offset, 80);
        let _ = p.handle_key(key(KeyCode::PageDown));
        assert_eq!(p.scroll_offset, 100);
    }

    #[test]
    fn ctrl_b_ctrl_f_match_pageup_pagedown() {
        let mut p = TranscriptPager::new();
        p.show();
        p.last_content_height = 200;
        p.last_viewport_height = 20;
        p.scroll_offset = 100;

        let _ = p.handle_key(ctrl(KeyCode::Char('b')));
        assert_eq!(p.scroll_offset, 80);
        let _ = p.handle_key(ctrl(KeyCode::Char('f')));
        assert_eq!(p.scroll_offset, 100);
    }

    #[test]
    fn home_jumps_to_top_end_jumps_to_bottom() {
        let mut p = TranscriptPager::new();
        p.show();
        p.last_content_height = 200;
        p.last_viewport_height = 20;
        p.scroll_offset = 100;

        let _ = p.handle_key(key(KeyCode::Home));
        assert_eq!(p.scroll_offset, 0);

        let _ = p.handle_key(key(KeyCode::End));
        // End uses sentinel; before render it's usize::MAX, scroll_down_by clamps.
        assert_eq!(p.scroll_offset, usize::MAX);
        let _ = p.handle_key(key(KeyCode::Up));
        // Up after End must clamp to a valid offset.
        assert!(p.scroll_offset <= 200);
    }

    #[test]
    fn scroll_clamps_at_top_and_bottom() {
        let mut p = TranscriptPager::new();
        p.show();
        p.last_content_height = 50;
        p.last_viewport_height = 20;
        p.scroll_offset = 0;

        let _ = p.handle_key(key(KeyCode::Up));
        assert_eq!(p.scroll_offset, 0, "should clamp at top");

        // 50 lines / 20 viewport → max_scroll_offset = 30
        p.scroll_offset = 30;
        let _ = p.handle_key(key(KeyCode::Down));
        assert_eq!(p.scroll_offset, 30, "should clamp at bottom");
    }

    #[test]
    fn slash_enters_search_mode() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        assert!(p.is_searching());
        assert_eq!(p.search_query(), Some(""));
    }

    #[test]
    fn typing_appends_to_search_query() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        for c in "hello".chars() {
            let _ = p.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.search_query(), Some("hello"));
    }

    #[test]
    fn backspace_deletes_search_char() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        let _ = p.handle_key(key(KeyCode::Char('a')));
        let _ = p.handle_key(key(KeyCode::Char('b')));
        let _ = p.handle_key(key(KeyCode::Backspace));
        assert_eq!(p.search_query(), Some("a"));
    }

    #[test]
    fn enter_commits_search_and_exits_edit_mode() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        let _ = p.handle_key(key(KeyCode::Char('x')));
        let _ = p.handle_key(key(KeyCode::Enter));
        assert!(!p.is_searching(), "edit mode should exit on Enter");
        assert_eq!(p.search_query(), Some("x"));
    }

    #[test]
    fn esc_in_search_clears_query_keeps_pager_open() {
        let mut p = TranscriptPager::new();
        p.show();
        let _ = p.handle_key(key(KeyCode::Char('/')));
        let _ = p.handle_key(key(KeyCode::Char('q'))); // would otherwise dismiss
        assert!(p.is_searching());
        assert_eq!(p.search_query(), Some("q"));
        let action = p.handle_key(key(KeyCode::Esc));
        assert_eq!(action, PagerAction::None);
        assert!(p.search.is_none());
        assert!(p.visible, "pager must stay open");
    }

    #[test]
    fn q_dismisses_pager() {
        let mut p = TranscriptPager::new();
        p.show();
        assert_eq!(p.handle_key(key(KeyCode::Char('q'))), PagerAction::Dismiss);
    }

    #[test]
    fn esc_outside_search_dismisses_pager() {
        let mut p = TranscriptPager::new();
        p.show();
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PagerAction::Dismiss);
    }

    #[test]
    fn ctrl_t_dismisses_pager() {
        let mut p = TranscriptPager::new();
        p.show();
        assert_eq!(p.handle_key(ctrl(KeyCode::Char('t'))), PagerAction::Dismiss);
    }

    #[test]
    fn find_matches_case_insensitive() {
        let lines = vec![
            Line::from("Hello World"),
            Line::from("nothing here"),
            Line::from("hello again"),
        ];
        let m = TranscriptPager::find_matches(&lines, "hello");
        assert_eq!(m, vec![0, 2]);
    }

    #[test]
    fn find_matches_empty_query_returns_empty() {
        let lines = vec![Line::from("Hello World")];
        assert!(TranscriptPager::find_matches(&lines, "").is_empty());
    }

    #[test]
    fn find_matches_no_hits() {
        let lines = vec![Line::from("Hello World")];
        assert!(TranscriptPager::find_matches(&lines, "zzz").is_empty());
    }

    #[test]
    fn n_advances_to_next_match_wraps_at_end() {
        let mut p = TranscriptPager::new();
        p.show();
        let mut state = SearchState::default();
        state.matches = vec![0, 5, 10];
        state.current = 0;
        state.query = "x".to_string();
        state.last_query_committed = "x".to_string();
        p.search = Some(state);
        p.last_viewport_height = 20;
        p.last_content_height = 100;

        let _ = p.handle_key(key(KeyCode::Char('n')));
        assert_eq!(p.search.as_ref().unwrap().current, 1);
        let _ = p.handle_key(key(KeyCode::Char('n')));
        assert_eq!(p.search.as_ref().unwrap().current, 2);
        // Wrap.
        let _ = p.handle_key(key(KeyCode::Char('n')));
        assert_eq!(p.search.as_ref().unwrap().current, 0);
    }

    #[test]
    fn shift_n_goes_to_previous_match() {
        let mut p = TranscriptPager::new();
        p.show();
        let mut state = SearchState::default();
        state.matches = vec![0, 5, 10];
        state.current = 1;
        state.query = "x".to_string();
        p.search = Some(state);

        let _ = p.handle_key(shift(KeyCode::Char('N')));
        assert_eq!(p.search.as_ref().unwrap().current, 0);
        let _ = p.handle_key(shift(KeyCode::Char('N')));
        assert_eq!(p.search.as_ref().unwrap().current, 2);
    }

    #[test]
    fn match_navigation_pins_scroll_to_match() {
        let mut p = TranscriptPager::new();
        p.show();
        p.last_viewport_height = 20;
        p.last_content_height = 200;
        let mut state = SearchState::default();
        state.matches = vec![100];
        state.current = 0;
        state.query = "x".to_string();
        p.search = Some(state);

        let _ = p.handle_key(key(KeyCode::Char('n')));
        // Should center the match in the viewport (line 100 - 10 = 90).
        assert_eq!(p.scroll_offset, 90);
    }

    #[test]
    fn navigation_with_no_matches_is_noop() {
        let mut p = TranscriptPager::new();
        p.show();
        p.scroll_offset = 5;
        p.search = Some(SearchState::default());
        let _ = p.handle_key(key(KeyCode::Char('n')));
        assert_eq!(p.scroll_offset, 5);
    }

    #[test]
    fn highlight_line_wraps_matches_only() {
        let line = Line::from("hello world hello");
        let highlighted = highlight_line(
            line,
            "hello",
            Style::default().add_modifier(Modifier::REVERSED),
        );
        // 3 spans: "hello", " world ", "hello"
        assert_eq!(highlighted.spans.len(), 3);
        assert!(highlighted.spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED));
        assert!(!highlighted.spans[1]
            .style
            .add_modifier
            .contains(Modifier::REVERSED));
        assert!(highlighted.spans[2]
            .style
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn highlight_line_no_match_returns_single_base_span() {
        let line = Line::from("hello world");
        let highlighted = highlight_line(line, "zzz", Style::default());
        assert_eq!(highlighted.spans.len(), 1);
        assert_eq!(highlighted.spans[0].content, "hello world");
    }

    #[test]
    fn highlight_line_handles_multibyte_text_without_panic() {
        // Turkish dotted-İ (U+0130, 2 bytes) lowercases to 'i\u{307}' (3 bytes),
        // so naive `text[lower_index..]` slicing panics on a non-UTF-8 boundary.
        // Searching for 'i' must find both the lowered İ and the trailing 'i'.
        let line = Line::from("hELLO İSTANBUL hello");
        let highlighted =
            highlight_line(line, "i", Style::default().add_modifier(Modifier::REVERSED));
        // Concatenated content must equal the original (no garbled slices).
        let concatenated: String = highlighted
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(concatenated, "hELLO İSTANBUL hello");
        // At least one span must carry the highlight style.
        assert!(highlighted
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn highlight_line_handles_emoji_without_panic() {
        let line = Line::from("hello 🌟 world");
        let highlighted = highlight_line(
            line,
            "world",
            Style::default().add_modifier(Modifier::REVERSED),
        );
        let concatenated: String = highlighted
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(concatenated, "hello 🌟 world");
    }

    #[test]
    fn highlight_line_empty_needle_returns_text_unchanged() {
        let line = Line::from("hello world");
        let highlighted = highlight_line(line, "", Style::default());
        assert_eq!(highlighted.spans.len(), 1);
        assert_eq!(highlighted.spans[0].content, "hello world");
    }

    #[test]
    fn unhandled_keys_return_none() {
        let mut p = TranscriptPager::new();
        p.show();
        assert_eq!(
            p.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            PagerAction::None
        );
    }
}
