use std::time::Instant;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::Frame;
use tui_textarea::TextArea;

use super::paste_burst::PasteBurst;
use super::theme;

pub struct ImageAttachment {
    pub data: Vec<u8>,
    pub mime_type: String,
}

struct InputHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: String,
}

impl InputHistory {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            draft: String::new(),
        }
    }

    fn push(&mut self, text: &str) {
        if !text.is_empty() {
            self.entries.push(text.to_string());
        }
        self.cursor = None;
        self.draft.clear();
    }

    /// Find the newest entry at or before `start` containing `query`.
    /// Empty query matches the entry at `start` (or newest if clamped).
    fn find_backward(&self, query: &str, start: usize) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let start = start.min(self.entries.len() - 1);
        if query.is_empty() {
            return Some(start);
        }
        (0..=start).rev().find(|&i| self.entries[i].contains(query))
    }

    /// Find the oldest entry at or after `start` containing `query`.
    fn find_forward(&self, query: &str, start: usize) -> Option<usize> {
        if self.entries.is_empty() || start >= self.entries.len() {
            return None;
        }
        if query.is_empty() {
            return Some(start);
        }
        (start..self.entries.len()).find(|&i| self.entries[i].contains(query))
    }

    fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    fn newest_index(&self) -> Option<usize> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.len() - 1)
        }
    }

    fn up(&mut self, current_text: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                self.draft = current_text.to_string();
                self.cursor = Some(self.entries.len() - 1);
            }
            Some(0) => {
                // Already at oldest, clamp
            }
            Some(pos) => {
                self.cursor = Some(pos - 1);
            }
        }
        self.cursor.map(|i| self.entries[i].as_str())
    }

    /// Returns Some(text) to set, or None meaning restore the draft
    fn down(&mut self) -> Option<Option<&str>> {
        match self.cursor {
            None => None, // not browsing
            Some(pos) => {
                if pos + 1 < self.entries.len() {
                    self.cursor = Some(pos + 1);
                    Some(Some(self.entries[pos + 1].as_str()))
                } else {
                    // Past newest → restore draft
                    self.cursor = None;
                    Some(None)
                }
            }
        }
    }

    fn reset(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    fn is_browsing(&self) -> bool {
        self.cursor.is_some()
    }

    #[cfg(test)]
    fn draft(&self) -> &str {
        &self.draft
    }
}

/// Active reverse-incremental search state.
///
/// Tracks the user's query, the draft snapshot to restore on cancel, and
/// the currently-matched history index. When `match_index` is `None`, the
/// search has no match for the current query (the "failing" state in bash).
struct HistorySearch {
    query: String,
    original_draft: String,
    match_index: Option<usize>,
}

pub struct Composer<'a> {
    textarea: TextArea<'a>,
    history: InputHistory,
    search: Option<HistorySearch>,
    image_attachments: Vec<ImageAttachment>,
    paste_burst: PasteBurst,
}

impl<'a> Composer<'a> {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("Type a message...");
        textarea.set_cursor_line_style(Style::default());
        textarea.set_block(
            Block::bordered()
                .title(format!("{} ", theme::INPUT_PROMPT))
                .border_style(Style::default().fg(theme::BORDER)),
        );
        Self {
            textarea,
            history: InputHistory::new(),
            search: None,
            image_attachments: Vec::new(),
            paste_burst: PasteBurst::new(),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.history.reset();
        self.paste_burst.flush_immediate();
        self.textarea.insert_str(text);
    }

    /// Called on each tick to flush any paste-burst buffer.
    pub fn tick(&mut self) {
        if let Some(text) = self.paste_burst.flush_if_due(Instant::now()) {
            self.textarea.insert_str(&text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        use crossterm::event::{KeyCode, KeyModifiers};

        // While an incremental search is active, intercept keys before any
        // other composer routing. Returns `Some(text)` only if the user
        // accepts+submits with Enter (currently we treat Enter as "accept
        // match, keep in composer"; user presses Enter again to submit).
        if self.search.is_some() {
            return self.handle_search_key(key);
        }

        // Ctrl+R enters reverse-incremental search.
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.begin_history_search();
            return None;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => {
                // During a paste burst, treat Enter as newline instead of submit
                let now = Instant::now();
                if self.paste_burst.append_newline_if_active(now) {
                    return None;
                }

                let text: String = self.textarea.lines().join("\n");
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return None;
                }
                self.history.push(&trimmed);
                // Clear the textarea
                self.textarea.select_all();
                self.textarea.cut();
                Some(trimmed)
            }
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
            {
                self.textarea.insert_newline();
                self.history.reset();
                None
            }
            (KeyCode::Up, KeyModifiers::NONE) if self.is_single_line() => {
                let current = self.textarea.lines().join("\n");
                if let Some(entry) = self.history.up(&current) {
                    let entry = entry.to_string();
                    self.set_text(&entry);
                }
                None
            }
            (KeyCode::Down, KeyModifiers::NONE) if self.history.is_browsing() => {
                match self.history.down() {
                    Some(Some(entry)) => {
                        let entry = entry.to_string();
                        self.set_text(&entry);
                    }
                    Some(None) => {
                        // Past newest history entry: clear input instead of restoring draft
                        self.set_text("");
                    }
                    None => {}
                }
                None
            }
            // Ctrl+P — emacs-style history back (same as Up when single-line)
            (KeyCode::Char('p'), m)
                if m.contains(KeyModifiers::CONTROL) && self.is_single_line() =>
            {
                let current = self.textarea.lines().join("\n");
                if let Some(entry) = self.history.up(&current) {
                    let entry = entry.to_string();
                    self.set_text(&entry);
                }
                None
            }
            // Ctrl+N — emacs-style history forward (same as Down when browsing)
            (KeyCode::Char('n'), m)
                if m.contains(KeyModifiers::CONTROL) && self.history.is_browsing() =>
            {
                match self.history.down() {
                    Some(Some(entry)) => {
                        let entry = entry.to_string();
                        self.set_text(&entry);
                    }
                    Some(None) => {
                        // Past newest history entry: clear input instead of restoring draft
                        self.set_text("");
                    }
                    None => {}
                }
                None
            }
            // Ctrl+Backspace — delete backward word
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::CONTROL) => {
                self.history.reset();
                // Flush any paste burst before editing
                if let Some(text) = self.paste_burst.flush_immediate() {
                    self.textarea.insert_str(&text);
                }
                self.textarea
                    .input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
                None
            }
            (KeyCode::Esc, _) => {
                self.paste_burst.flush_immediate();
                self.textarea.select_all();
                self.textarea.cut();
                self.history.reset();
                self.image_attachments.clear();
                None
            }
            // Plain character — feed to paste burst detector
            (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                use super::paste_burst::CharAction;
                self.history.reset();
                match self.paste_burst.on_char(ch, Instant::now()) {
                    CharAction::Buffer => None,
                    CharAction::Insert => {
                        self.textarea.input(key);
                        None
                    }
                }
            }
            _ => {
                self.history.reset();
                // Flush any paste burst before handling other keys
                if let Some(text) = self.paste_burst.flush_immediate() {
                    self.textarea.insert_str(&text);
                }
                self.textarea.input(key);
                None
            }
        }
    }

    // ----- Reverse-incremental history search -----

    fn begin_history_search(&mut self) {
        let original_draft = self.text();
        let match_index = self.history.newest_index();
        let preview = match_index.and_then(|i| self.history.get(i).map(str::to_string));
        self.search = Some(HistorySearch {
            query: String::new(),
            original_draft,
            match_index,
        });
        if let Some(text) = preview {
            self.set_text(&text);
        } else {
            self.set_text("");
        }
    }

    /// Cancel the search: restore the original draft and clear search state.
    fn cancel_history_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.set_text(&search.original_draft);
        }
    }

    /// Accept the current match: keep the matched text as the composer draft
    /// and clear search state. If there is no match, restore the draft.
    fn accept_history_search(&mut self) {
        if let Some(search) = self.search.take() {
            if search.match_index.is_none() {
                self.set_text(&search.original_draft);
            }
            // Preview text is already in the textarea.
            self.history.reset();
        }
    }

    /// Refresh the match after the query changes, starting from the current
    /// match position (or newest entry if no active match).
    fn refresh_search_match(&mut self) {
        let Some(ref mut search) = self.search else {
            return;
        };
        let start = search
            .match_index
            .or_else(|| self.history.newest_index())
            .unwrap_or(0);
        search.match_index = self.history.find_backward(&search.query, start);
        let preview = search
            .match_index
            .and_then(|i| self.history.get(i).map(str::to_string));
        if let Some(text) = preview {
            self.set_text(&text);
        } else {
            self.set_text("");
        }
    }

    /// Step to the next older match. If none, stay put.
    fn step_search_backward(&mut self) {
        let Some(ref mut search) = self.search else {
            return;
        };
        let start = match search.match_index {
            Some(i) if i > 0 => i - 1,
            Some(_) => return, // already at oldest match
            None => self.history.newest_index().unwrap_or(0),
        };
        let next = self.history.find_backward(&search.query, start);
        if let Some(i) = next {
            search.match_index = Some(i);
            if let Some(text) = self.history.get(i).map(str::to_string) {
                self.set_text(&text);
            }
        }
    }

    /// Step to the next newer match. If none, stay put.
    fn step_search_forward(&mut self) {
        let Some(ref mut search) = self.search else {
            return;
        };
        let Some(current) = search.match_index else {
            return;
        };
        let start = current + 1;
        let next = self.history.find_forward(&search.query, start);
        if let Some(i) = next {
            search.match_index = Some(i);
            if let Some(text) = self.history.get(i).map(str::to_string) {
                self.set_text(&text);
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Option<String> {
        use crossterm::event::{KeyCode, KeyModifiers};

        match (key.code, key.modifiers) {
            // Ctrl+R — step to next older match (repeated reverse search).
            (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.step_search_backward();
                None
            }
            // Ctrl+S — step to next newer match (forward search).
            (KeyCode::Char('s'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.step_search_forward();
                None
            }
            // Enter — accept current match, stay in composer.
            (KeyCode::Enter, _) => {
                self.accept_history_search();
                None
            }
            // Esc — cancel search, restore draft.
            (KeyCode::Esc, _) => {
                self.cancel_history_search();
                None
            }
            // Backspace — shorten query and re-resolve match.
            (KeyCode::Backspace, _) => {
                if let Some(ref mut search) = self.search {
                    search.query.pop();
                    // Reset match_index so the new match is found from the newest.
                    search.match_index = self.history.newest_index();
                }
                self.refresh_search_match();
                None
            }
            // Printable characters — extend the query.
            (KeyCode::Char(ch), m)
                if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                if let Some(ref mut search) = self.search {
                    search.query.push(ch);
                    // Always search from the newest on query extension, so
                    // new keystrokes land on the most-recent match.
                    search.match_index = self.history.newest_index();
                }
                self.refresh_search_match();
                None
            }
            // Any other key: accept current match and exit search silently.
            _ => {
                self.accept_history_search();
                None
            }
        }
    }

    /// Whether the composer is currently in reverse-incremental search mode.
    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// Current search query (empty string when search was just opened).
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    /// Whether the current search query has a match. `false` means the
    /// search is in the "failing" state (bash renders `failing reverse-i-search`).
    pub fn search_has_match(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|s| s.match_index.is_some())
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(text);
    }

    /// Complete the in-progress `@query` to `@<display> ` (with trailing
    /// space). The path content is NOT stored here — the client-side
    /// mentions expander re-reads the `@` token from the composer text at
    /// submit time, so the composer only needs to track the display name.
    pub fn complete_file_mention(&mut self, display: &str) {
        let current = self.text();
        let new_text = if let Some(at_pos) = current.rfind('@') {
            format!("{}@{display} ", &current[..at_pos])
        } else {
            format!("{current}@{display} ")
        };
        self.set_text(&new_text);
    }

    /// Rewrite the in-progress `@query` to `@<display>` without appending
    /// a trailing space. Used when the user selects a directory in the
    /// file popup so they can continue drilling into it.
    pub fn set_partial_mention(&mut self, display: String) {
        let current = self.text();
        let new_text = if let Some(at_pos) = current.rfind('@') {
            format!("{}@{display}", &current[..at_pos])
        } else {
            format!("{current}@{display}")
        };
        self.set_text(&new_text);
    }

    pub fn add_image(&mut self, data: Vec<u8>, mime_type: String) {
        let n = self.image_attachments.len() + 1;
        let placeholder = format!("[Image #{n}]");
        self.textarea.insert_str(&placeholder);
        self.image_attachments
            .push(ImageAttachment { data, mime_type });
    }

    pub fn take_image_attachments(&mut self) -> Vec<ImageAttachment> {
        std::mem::take(&mut self.image_attachments)
    }

    pub fn set_image_attachments(&mut self, images: Vec<ImageAttachment>) {
        self.image_attachments = images;
    }

    pub fn height(&self) -> u16 {
        let content_lines = self.textarea.lines().len() as u16;
        // 2 for border, min 1 content line
        (content_lines.max(1) + 2).min(10)
    }

    pub fn is_empty(&self) -> bool {
        self.is_single_line() && self.textarea.lines().first().is_none_or(String::is_empty)
    }

    /// Whether the composer is currently showing a recalled history entry
    /// Returns `true` if the composer is in history-browse mode
    /// (i.e. the user pressed Up/Ctrl+P to browse previously submitted
    /// messages and has not yet edited or cleared the recalled text).
    pub fn is_browsing_history(&self) -> bool {
        self.history.is_browsing()
    }

    /// Type text and submit it (for tests). Uses set_text to avoid paste burst detection.
    #[cfg(test)]
    fn type_and_submit(&mut self, text: &str) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        self.set_text(text);
        self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    /// Type text into the composer (for tests). Uses set_text to avoid paste burst detection.
    #[cfg(test)]
    fn type_text(&mut self, text: &str) {
        self.set_text(text);
        self.history.reset();
    }

    fn is_single_line(&self) -> bool {
        self.textarea.lines().len() <= 1
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(&self.textarea, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history_push_and_up() {
        let mut h = InputHistory::new();
        h.push("first");
        h.push("second");
        h.push("third");

        assert_eq!(h.up(""), Some("third"));
        assert_eq!(h.up(""), Some("second"));
        assert_eq!(h.up(""), Some("first"));
    }

    #[test]
    fn test_history_down_returns_to_draft() {
        let mut h = InputHistory::new();
        h.push("aaa");
        h.push("bbb");

        h.up("my draft");
        h.up("my draft");
        // cursor at 0 ("aaa")
        assert_eq!(h.up("my draft"), Some("aaa"));

        // go forward
        assert_eq!(h.down(), Some(Some("bbb")));
        // past newest → restore draft
        assert_eq!(h.down(), Some(None));
        assert_eq!(h.draft(), "my draft");
        assert!(!h.is_browsing());
    }

    #[test]
    fn test_history_reset_on_new_input() {
        let mut h = InputHistory::new();
        h.push("aaa");
        h.up("");
        assert!(h.is_browsing());
        h.reset();
        assert!(!h.is_browsing());
    }

    #[test]
    fn test_history_empty() {
        let mut h = InputHistory::new();
        assert_eq!(h.up(""), None);
        assert!(!h.is_browsing());
    }

    #[test]
    fn test_history_up_clamps_at_oldest() {
        let mut h = InputHistory::new();
        h.push("only");

        assert_eq!(h.up(""), Some("only"));
        assert_eq!(h.up(""), Some("only"));
        assert_eq!(h.up(""), Some("only"));
    }

    #[test]
    fn test_down_past_newest_clears_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = Composer::new();
        c.type_and_submit("hi");

        // Up loads "hi"
        c.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(c.text(), "hi");

        // Down past newest should clear
        c.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(c.text(), "");
        assert!(!c.is_browsing_history());
    }

    #[test]
    fn test_down_past_newest_clears_even_with_draft() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = Composer::new();
        c.type_and_submit("old");

        // Type draft text
        c.type_text("wip");

        // Up saves draft and loads "old"
        c.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(c.text(), "old");

        // Down past newest should clear, NOT restore "wip"
        c.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(c.text(), "");
    }

    #[test]
    fn test_ctrl_n_past_newest_clears_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = Composer::new();
        c.type_and_submit("x");

        // Ctrl+P to go back
        c.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(c.text(), "x");

        // Ctrl+N past newest should clear
        c.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(c.text(), "");
        assert!(!c.is_browsing_history());
    }

    #[test]
    fn test_history_preserves_draft() {
        let mut h = InputHistory::new();
        h.push("old");

        let result = h.up("work in progress");
        assert_eq!(result, Some("old"));

        // down past newest restores draft
        let result = h.down();
        assert_eq!(result, Some(None));
        assert_eq!(h.draft(), "work in progress");
    }

    #[test]
    fn test_handle_paste_inserts_text() {
        let mut c = Composer::new();
        c.handle_paste("hello world");
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    fn test_handle_paste_multiline() {
        let mut c = Composer::new();
        c.handle_paste("line 1\nline 2\nline 3");
        assert_eq!(c.text(), "line 1\nline 2\nline 3");
    }

    #[test]
    fn test_handle_paste_appends_at_cursor() {
        let mut c = Composer::new();
        c.type_text("prefix ");
        c.handle_paste("pasted");
        assert_eq!(c.text(), "prefix pasted");
    }

    #[test]
    fn test_handle_paste_resets_history() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = Composer::new();
        c.type_and_submit("old");

        // Browse history
        c.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(c.is_browsing_history());

        // Paste should reset history browsing
        c.handle_paste("new text");
        assert!(!c.is_browsing_history());
    }

    #[test]
    fn test_ctrl_backspace_deletes_word() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = Composer::new();
        c.type_text("hello world");
        // Ctrl+Backspace should delete "world"
        c.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        let text = c.text();
        assert_eq!(text, "hello ");
    }

    #[test]
    fn test_handle_paste_large_block() {
        let mut c = Composer::new();
        let large_text = "fn main() {\n    println!(\"Hello, world!\");\n    let x = 42;\n    let y = x * 2;\n}\n";
        c.handle_paste(large_text);
        assert_eq!(c.text(), large_text);
    }

    // -------------- Reverse-incremental history search --------------

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn seed_history(c: &mut Composer, entries: &[&str]) {
        for e in entries {
            c.type_and_submit(e);
        }
    }

    #[test]
    fn test_search_ctrl_r_opens_mode() {
        let mut c = Composer::new();
        seed_history(&mut c, &["alpha"]);
        c.handle_key(ctrl('r'));
        assert!(c.is_searching());
        assert_eq!(c.search_query(), Some(""));
        // With empty query, preview shows newest entry.
        assert_eq!(c.text(), "alpha");
    }

    #[test]
    fn test_search_query_finds_newest_match() {
        let mut c = Composer::new();
        seed_history(&mut c, &["cargo build", "git status", "cargo test"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('c')));
        c.handle_key(plain(KeyCode::Char('a')));

        assert!(c.is_searching());
        assert_eq!(c.search_query(), Some("ca"));
        // Newest match for "ca" is "cargo test".
        assert_eq!(c.text(), "cargo test");
        assert!(c.search_has_match());
    }

    #[test]
    fn test_search_ctrl_r_steps_to_older_match() {
        let mut c = Composer::new();
        seed_history(&mut c, &["cargo build", "git status", "cargo test"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('c')));
        assert_eq!(c.text(), "cargo test");

        // Ctrl+R again steps to the next older match.
        c.handle_key(ctrl('r'));
        assert_eq!(c.text(), "cargo build");

        // Once at oldest, Ctrl+R is a no-op (stays put).
        c.handle_key(ctrl('r'));
        assert_eq!(c.text(), "cargo build");
    }

    #[test]
    fn test_search_ctrl_s_steps_forward() {
        let mut c = Composer::new();
        seed_history(&mut c, &["cargo build", "git status", "cargo test"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('c')));
        c.handle_key(ctrl('r')); // -> cargo build
        assert_eq!(c.text(), "cargo build");

        c.handle_key(ctrl('s')); // -> cargo test (newer)
        assert_eq!(c.text(), "cargo test");
    }

    #[test]
    fn test_search_enter_accepts_match() {
        let mut c = Composer::new();
        seed_history(&mut c, &["git commit"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('g')));
        assert!(c.is_searching());

        // Enter accepts: search closes, preview stays as the composer text.
        c.handle_key(plain(KeyCode::Enter));
        assert!(!c.is_searching());
        assert_eq!(c.text(), "git commit");
    }

    #[test]
    fn test_search_esc_cancels_and_restores_draft() {
        let mut c = Composer::new();
        seed_history(&mut c, &["git status"]);
        c.type_text("work in progress");

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('g')));
        // Preview overwrote the draft.
        assert_eq!(c.text(), "git status");

        c.handle_key(plain(KeyCode::Esc));
        assert!(!c.is_searching());
        assert_eq!(c.text(), "work in progress");
    }

    #[test]
    fn test_search_no_match_renders_failing_state() {
        let mut c = Composer::new();
        seed_history(&mut c, &["alpha"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('z')));
        assert!(c.is_searching());
        assert_eq!(c.search_query(), Some("z"));
        assert!(!c.search_has_match());
        assert_eq!(c.text(), ""); // preview empty when no match
    }

    #[test]
    fn test_search_backspace_resolves_match() {
        let mut c = Composer::new();
        seed_history(&mut c, &["grep", "ls", "cat"]);

        c.handle_key(ctrl('r'));
        c.handle_key(plain(KeyCode::Char('g'))); // -> "grep"
        assert_eq!(c.text(), "grep");
        c.handle_key(plain(KeyCode::Char('z'))); // "gz" fails
        assert!(!c.search_has_match());

        c.handle_key(plain(KeyCode::Backspace)); // back to "g"
        assert_eq!(c.search_query(), Some("g"));
        assert!(c.search_has_match());
        assert_eq!(c.text(), "grep");
    }

    #[test]
    fn test_search_begin_with_empty_history_has_no_match() {
        let mut c = Composer::new();
        c.handle_key(ctrl('r'));
        assert!(c.is_searching());
        assert!(!c.search_has_match());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn test_find_backward_finds_newest_containing_query() {
        let mut h = InputHistory::new();
        h.push("cargo build");
        h.push("git status");
        h.push("cargo test");
        // Search from the newest entry (index 2).
        assert_eq!(h.find_backward("cargo", 2), Some(2));
        // Search from index 1 (git status): should find cargo build at 0.
        assert_eq!(h.find_backward("cargo", 1), Some(0));
        // Query that matches nothing.
        assert_eq!(h.find_backward("nope", 2), None);
        // Empty query returns the start index.
        assert_eq!(h.find_backward("", 2), Some(2));
    }

    #[test]
    fn test_find_forward_finds_oldest_containing_query() {
        let mut h = InputHistory::new();
        h.push("cargo build");
        h.push("git status");
        h.push("cargo test");
        // From index 1, forward: cargo test at 2.
        assert_eq!(h.find_forward("cargo", 1), Some(2));
        // From start, forward: cargo build at 0.
        assert_eq!(h.find_forward("cargo", 0), Some(0));
        // Past end returns None.
        assert_eq!(h.find_forward("cargo", 3), None);
    }
}
