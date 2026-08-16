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
//! See `crates/interface/src/tui/mod.rs` for the source-level guard tests.

use super::custom_terminal::Frame;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

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

/// Identity of a cell as far as rendering is concerned: the variant, its text,
/// and the flags that change how that text is drawn.
///
/// Hashing beats comparing cell counts because the app edits cells that are not
/// the last one — thinking deltas land at `len - 2`, a tool result is marked
/// complete or collapsed long after it was pushed, a plan is restated in place,
/// and loading another session swaps the whole vector without necessarily
/// shortening it. None of that shows up in a length or a count.
///
/// It is much cheaper than the render it guards: hashing walks the bytes once,
/// where rendering re-parses, re-wraps and re-allocates every line.
fn cell_fingerprint(cell: &HistoryCell) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(cell).hash(&mut h);
    match cell {
        HistoryCell::User { text }
        | HistoryCell::Heartbeat { text }
        | HistoryCell::System { text }
        | HistoryCell::Thinking { text }
        | HistoryCell::MentionCard { label: text } => text.hash(&mut h),
        HistoryCell::Assistant {
            text, streaming, ..
        } => {
            text.hash(&mut h);
            streaming.hash(&mut h);
        }
        HistoryCell::ToolStart {
            name,
            args,
            completed,
            ..
        } => {
            name.hash(&mut h);
            args.hash(&mut h);
            completed.hash(&mut h);
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
            output.hash(&mut h);
            is_error.hash(&mut h);
            duration_ms.hash(&mut h);
            display_label.hash(&mut h);
            tool_name.hash(&mut h);
            args_json.hash(&mut h);
            collapsed.hash(&mut h);
        }
        HistoryCell::ShellApproval { command, status } => {
            command.hash(&mut h);
            std::mem::discriminant(status).hash(&mut h);
        }
        HistoryCell::Card { lines } => {
            for line in lines {
                for span in &line.spans {
                    span.content.hash(&mut h);
                }
            }
        }
        HistoryCell::DoctorWarning { warnings } => warnings.hash(&mut h),
        HistoryCell::ToolStreaming { lines, dropped } => {
            lines.hash(&mut h);
            dropped.hash(&mut h);
        }
        HistoryCell::Plan { steps } => {
            for step in steps {
                step.title.hash(&mut h);
                std::mem::discriminant(&step.status).hash(&mut h);
            }
        }
        HistoryCell::Separator { label } => label.hash(&mut h),
    }
    h.finish()
}

/// One cached cell: what it rendered to (by line count) and what it looked like
/// when it was rendered.
#[derive(Debug)]
struct CachedCell {
    fingerprint: u64,
    /// How many entries of [`LineCache::lines`] this cell contributed.
    lines: usize,
}

/// Rendered lines for the settled part of the transcript.
///
/// Re-rendering every history cell on every frame is O(transcript): each cell
/// re-wraps and re-styles its text, so a long session pays for its whole
/// history to repaint a scroll of one line. Instead every cell before the final
/// one is rendered once and kept, and each kept cell carries a
/// [`cell_fingerprint`] so a cell that changed after it was cached — collapsed,
/// completed, appended to, replaced outright — is detected and re-rendered.
/// Comparing fingerprints is cheap; re-wrapping is not.
///
/// The final cell is excluded from the cache entirely: it is the one a running
/// turn mutates most, and re-rendering one cell per frame is bounded work.
#[derive(Debug, Default)]
struct LineCache {
    lines: Vec<Line<'static>>,
    /// The leading cells `lines` was built from, in order.
    cells: Vec<CachedCell>,
    /// Width those cells were wrapped at.
    width: u16,
}

impl LineCache {
    /// Bring the cache up to `stable_cells` of `cells`.
    ///
    /// Drops everything from the first cell that no longer matches what was
    /// cached (and everything, on a width change — a rewrap invalidates every
    /// line), then renders the rest.
    fn sync(&mut self, cells: &[HistoryCell], stable_cells: usize, width: u16) {
        if self.width != width {
            self.lines.clear();
            self.cells.clear();
            self.width = width;
        }
        let checked = self.cells.len().min(stable_cells);
        let keep = (0..checked)
            .find(|&i| self.cells[i].fingerprint != cell_fingerprint(&cells[i]))
            .unwrap_or(checked);
        if keep < self.cells.len() {
            let kept_lines = self.cells[..keep].iter().map(|c| c.lines).sum();
            self.lines.truncate(kept_lines);
            self.cells.truncate(keep);
        }
        for cell in &cells[self.cells.len()..stable_cells] {
            let rendered = cell.render(width, None);
            self.cells.push(CachedCell {
                fingerprint: cell_fingerprint(cell),
                lines: rendered.len(),
            });
            self.lines.extend(rendered);
        }
    }
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
    /// Rendered lines for every cell except the last. See [`LineCache`].
    cache: LineCache,
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
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Esc, _)
            | (KeyCode::Char('t'), KeyModifiers::CONTROL) => PagerAction::Dismiss,

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

        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(self.title());
        let inner = block.inner(area);
        frame.render_widget(&block, area);

        // Everything but the final cell is rendered once and reused; the final
        // cell is re-rendered each frame because a running turn mutates it in
        // place. So a frame costs one cell plus the viewport, not the whole
        // transcript.
        let stable_cells = cells.len().saturating_sub(1);
        self.cache.sync(cells, stable_cells, inner.width);
        let tail: Vec<Line<'static>> = cells[stable_cells..]
            .iter()
            .flat_map(|cell| cell.render(inner.width, None))
            .collect();

        let total_lines = self.cache.lines.len() + tail.len();
        let line_at = |i: usize| -> &Line<'static> {
            match i.checked_sub(self.cache.lines.len()) {
                Some(tail_i) => &tail[tail_i],
                None => &self.cache.lines[i],
            }
        };

        self.last_content_height = total_lines;
        self.last_viewport_height = inner.height as usize;

        // Recompute search matches when the committed query changes.
        if let Some(state) = self.search.as_mut() {
            if !state.editing && state.last_query_committed == state.query {
                // Re-derive matches against current line set in case transcript grew.
                state.matches = Self::find_matches_in(total_lines, &line_at, &state.query);
                if state.current >= state.matches.len() {
                    state.current = 0;
                }
            }
        }

        // Resolve usize::MAX sentinel to bottom-of-content.
        let max_off = self.max_scroll_offset(total_lines);
        if self.scroll_offset > max_off {
            self.scroll_offset = max_off;
        }

        let viewport = inner.height as usize;
        let start = self.scroll_offset;
        let end = (start + viewport).min(total_lines);

        // Highlight only the visible slice — O(viewport × matches_in_window),
        // not O(full_transcript). Membership lookups go through a HashSet so
        // a transcript with thousands of matches stays cheap to render.
        let mut visible: Vec<Line<'static>> = (start..end).map(|i| line_at(i).clone()).collect();
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

    #[cfg(test)]
    fn find_matches(lines: &[Line<'static>], query: &str) -> Vec<usize> {
        Self::find_matches_in(lines.len(), &|i| &lines[i], query)
    }

    /// [`Self::find_matches`] over a line sequence that is not contiguous in
    /// memory — the pager's lines live in the cache and the live tail, and
    /// joining them into one vector every frame is exactly the allocation the
    /// cache exists to avoid.
    fn find_matches_in<'a>(
        len: usize,
        line_at: &dyn Fn(usize) -> &'a Line<'static>,
        query: &str,
    ) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        (0..len)
            .filter(|&i| {
                let text: String = line_at(i)
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                text.to_lowercase().contains(&needle)
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

    fn cell(text: &str) -> HistoryCell {
        HistoryCell::assistant(text.to_string(), false)
    }

    /// Lines a from-scratch render would produce, as plain text.
    fn expected_text(cells: &[HistoryCell], width: u16) -> Vec<String> {
        cells
            .iter()
            .flat_map(|c| c.render(width, None))
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    fn cached_text(cache: &LineCache) -> Vec<String> {
        cache
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    /// Spec: appending to the transcript must only render what was appended.
    /// The reused prefix has to be indistinguishable from a fresh render, or
    /// the pager shows stale text.
    #[test]
    fn cache_extends_without_re_rendering_the_prefix() {
        let mut cells = vec![cell("first"), cell("second")];
        let mut cache = LineCache::default();
        cache.sync(&cells, cells.len(), 40);
        let after_first = cache.lines.len();
        assert_eq!(cached_text(&cache), expected_text(&cells, 40));

        cells.push(cell("third"));
        cache.sync(&cells, cells.len(), 40);
        assert!(
            cache.lines.len() > after_first,
            "the appended cell must be rendered"
        );
        assert_eq!(cached_text(&cache), expected_text(&cells, 40));
        assert_eq!(cache.cells.len(), 3);
    }

    /// Spec: a width change rewraps every line, so the cache cannot be reused.
    /// Keeping it would leave the transcript wrapped to the old terminal width
    /// after a resize.
    #[test]
    fn cache_rebuilds_when_the_width_changes() {
        let cells = vec![cell(
            "a much longer line of assistant prose that will wrap differently at each width",
        )];
        let mut cache = LineCache::default();
        cache.sync(&cells, cells.len(), 80);
        let wide = cached_text(&cache);

        cache.sync(&cells, cells.len(), 24);
        let narrow = cached_text(&cache);

        assert_ne!(wide, narrow, "the test text must actually rewrap");
        assert_eq!(
            narrow,
            expected_text(&cells, 24),
            "after a resize the cache must match a fresh render at the new width"
        );
    }

    /// Spec: `/new` and loading another session replace the cells rather than
    /// appending. A cache that only ever grows would keep showing the previous
    /// conversation above the new one.
    #[test]
    fn cache_rebuilds_when_the_transcript_is_replaced() {
        let old = vec![cell("old one"), cell("old two"), cell("old three")];
        let mut cache = LineCache::default();
        cache.sync(&old, old.len(), 40);

        let new = vec![cell("brand new")];
        cache.sync(&new, new.len(), 40);

        assert_eq!(cached_text(&cache), expected_text(&new, 40));
        assert_eq!(cache.cells.len(), 1);
        assert!(
            !cached_text(&cache).iter().any(|l| l.contains("old")),
            "the replaced transcript must be gone: {:?}",
            cached_text(&cache)
        );
    }

    /// Spec: a cached cell that the app mutates in place must be re-rendered.
    /// Thinking deltas land at `len - 2`, so the mutated cell is inside the
    /// cached prefix — without a per-cell check the pager would freeze the
    /// reasoning block at whatever it said on the first frame.
    #[test]
    fn cache_re_renders_a_cell_mutated_in_place() {
        let mut cells = vec![
            HistoryCell::Thinking {
                text: "thinking so far".to_string(),
            },
            cell("live tail"),
        ];
        let mut cache = LineCache::default();
        cache.sync(&cells, 1, 40);
        assert_eq!(cached_text(&cache), expected_text(&cells[..1], 40));

        if let HistoryCell::Thinking { text } = &mut cells[0] {
            text.push_str(" and quite a lot more reasoning after that");
        }
        cache.sync(&cells, 1, 40);

        assert_eq!(
            cached_text(&cache),
            expected_text(&cells[..1], 40),
            "an in-place edit to a cached cell must be picked up"
        );
        assert!(
            cached_text(&cache)
                .join(" ")
                .contains("reasoning after that"),
            "the appended text must reach the pager: {:?}",
            cached_text(&cache)
        );
    }

    /// Spec: loading another session replaces the cells. When the replacement
    /// is *longer* than what was cached, nothing shrinks — so a length-only
    /// check would keep the previous conversation's lines on screen.
    #[test]
    fn cache_rebuilds_when_a_longer_transcript_replaces_the_old_one() {
        let old = vec![cell("old one"), cell("old two")];
        let mut cache = LineCache::default();
        cache.sync(&old, old.len(), 40);

        let new = vec![cell("new one"), cell("new two"), cell("new three")];
        cache.sync(&new, new.len(), 40);

        assert_eq!(cached_text(&cache), expected_text(&new, 40));
        assert!(
            !cached_text(&cache).iter().any(|l| l.contains("old")),
            "the replaced transcript must be gone: {:?}",
            cached_text(&cache)
        );
    }

    /// Spec: the final cell is excluded from the cache because a running turn
    /// mutates it in place — no cell count or width change would reveal that.
    #[test]
    fn the_final_cell_is_never_cached() {
        let cells = vec![cell("settled"), cell("still streaming")];
        let mut cache = LineCache::default();
        cache.sync(&cells, cells.len().saturating_sub(1), 40);

        assert_eq!(cache.cells.len(), 1);
        assert_eq!(cached_text(&cache), expected_text(&cells[..1], 40));
        assert!(
            !cached_text(&cache).iter().any(|l| l.contains("streaming")),
            "the live cell must be re-rendered each frame, not cached"
        );
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

    /// Three independent dismiss keys: `q`, Esc (when not in search edit
    /// mode), and Ctrl+T (the same chord that opened the pager). All must
    /// emit `PagerAction::Dismiss` so the parent app can leave the
    /// alt-screen.
    #[test]
    fn q_esc_ctrl_t_all_dismiss_pager() {
        for k_evt in [
            key(KeyCode::Char('q')),
            key(KeyCode::Esc),
            ctrl(KeyCode::Char('t')),
        ] {
            let mut p = TranscriptPager::new();
            p.show();
            assert_eq!(p.handle_key(k_evt), PagerAction::Dismiss, "key {k_evt:?}");
        }
    }

    /// `find_matches` is the in-pager search: case-insensitive substring,
    /// empty query returns no matches (avoids highlighting every line),
    /// no-hit query returns empty.
    #[test]
    fn find_matches_case_insensitive_with_empty_and_miss_branches() {
        let lines = vec![
            Line::from("Hello World"),
            Line::from("nothing here"),
            Line::from("hello again"),
        ];
        assert_eq!(TranscriptPager::find_matches(&lines, "hello"), vec![0, 2]);
        assert!(TranscriptPager::find_matches(&lines, "").is_empty());
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
