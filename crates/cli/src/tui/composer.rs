use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::Frame;
use tui_textarea::TextArea;

use super::paste_burst::PasteBurst;
use super::theme;

pub struct FileRef {
    pub display: String,
    pub path: PathBuf,
}

pub struct ImageAttachment {
    #[allow(dead_code)]
    pub placeholder: String,
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

pub struct Composer<'a> {
    textarea: TextArea<'a>,
    history: InputHistory,
    file_refs: Vec<FileRef>,
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
            file_refs: Vec::new(),
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
                self.file_refs.clear();
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

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(text);
    }

    pub fn add_file_ref(&mut self, display: String, path: PathBuf) {
        self.file_refs.push(FileRef {
            display: display.clone(),
            path,
        });
        let current = self.text();
        // Replace the partial @query with the completed @mention
        let new_text = if let Some(at_pos) = current.rfind('@') {
            format!("{}@{display} ", &current[..at_pos])
        } else {
            format!("{current}@{display} ")
        };
        self.set_text(&new_text);
    }

    /// Rewrite the in-progress `@query` to `@<display>` without pushing a
    /// `FileRef` or appending a trailing space. Used when the user selects a
    /// directory in the file popup so they can continue drilling into it.
    pub fn set_partial_mention(&mut self, display: String) {
        let current = self.text();
        let new_text = if let Some(at_pos) = current.rfind('@') {
            format!("{}@{display}", &current[..at_pos])
        } else {
            format!("{current}@{display}")
        };
        self.set_text(&new_text);
    }

    pub fn take_file_refs(&mut self) -> Vec<FileRef> {
        std::mem::take(&mut self.file_refs)
    }

    pub fn add_image(&mut self, data: Vec<u8>, mime_type: String) {
        let n = self.image_attachments.len() + 1;
        let placeholder = format!("[Image #{n}]");
        self.textarea.insert_str(&placeholder);
        self.image_attachments.push(ImageAttachment {
            placeholder,
            data,
            mime_type,
        });
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
    #[cfg(test)]
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
}
