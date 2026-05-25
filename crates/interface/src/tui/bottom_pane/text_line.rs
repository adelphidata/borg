//! Single-line text editor used by the notes field of the
//! `request_user_input` overlay.
//!
//! Owns a byte-aligned cursor, supports insert/delete at the cursor,
//! code-point movement, and renders itself with a `▌` left-bar plus a
//! reversed block cursor when focused.
//!
//! Code-point movement (not grapheme cluster) is intentional: borg's notes
//! field is a chat-style single-line input that doesn't need to handle
//! combining marks or ZWJ sequences. If that ever changes, swap
//! `char_indices` for a grapheme cursor.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme;

const LEFT_BAR: &str = "▌ ";

/// Single-line editable text buffer with a code-point-aligned cursor.
#[derive(Debug, Default, Clone)]
pub(super) struct TextLine {
    text: String,
    /// Byte index into `text`. Always points to a UTF-8 char boundary or
    /// `text.len()`.
    cursor: usize,
}

impl TextLine {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Display column of the cursor — sum of unicode widths of chars before
    /// the cursor. CJK / wide chars take 2 columns, ASCII 1, zero-width 0.
    pub fn cursor_col(&self) -> u16 {
        self.text[..self.cursor].width() as u16
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the code point immediately before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Forward-delete the code point at the cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(idx, _)| self.cursor + idx)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..next, "");
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(idx, _)| self.cursor + idx)
            .unwrap_or(self.text.len());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Render into `area`. Paints the `▌` bar in cyan and the text. When
    /// `focused`, paints the cell at `cursor_col` (after the bar) with a
    /// reversed style so the insertion point is visible — including when
    /// the buffer is empty (cursor sits at col 0 of the body).
    ///
    /// `placeholder` is rendered in dim style when the buffer is empty AND
    /// the editor is NOT focused — `None` skips placeholder painting and
    /// leaves the body row empty (or showing just the cursor block when
    /// focused).
    pub fn render(&self, buf: &mut Buffer, area: Rect, focused: bool, placeholder: Option<&str>) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let bar = Span::styled(LEFT_BAR, Style::default().fg(theme::cyan()));
        let body = if self.text.is_empty() {
            match placeholder {
                Some(p) if !focused => Span::styled(p.to_string(), theme::dim()),
                _ => Span::raw(String::new()),
            }
        } else {
            Span::raw(self.text.clone())
        };
        Paragraph::new(Line::from(vec![bar, body])).render(area, buf);

        if !focused {
            return;
        }
        let bar_cols = LEFT_BAR.width() as u16;
        let cursor_x = area
            .x
            .saturating_add(bar_cols)
            .saturating_add(self.cursor_col());
        if cursor_x >= area.x + area.width {
            return;
        }
        let cell = &mut buf[(cursor_x, area.y)];
        if cell.symbol().is_empty() || cell.symbol() == " " {
            cell.set_symbol(" ");
        }
        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> TextLine {
        let mut t = TextLine::default();
        for c in s.chars() {
            t.insert_char(c);
        }
        t
    }

    /// Insertion appends to the buffer and advances the cursor by the
    /// inserted code point's byte length. Required so multi-byte input
    /// doesn't desync the cursor and produce a UTF-8 panic on the next
    /// `replace_range`.
    #[test]
    fn insert_advances_cursor_by_utf8_byte_length() {
        let mut t = TextLine::default();
        t.insert_char('a');
        assert_eq!(t.text(), "a");
        assert_eq!(t.cursor, 1);
        t.insert_char('é');
        assert_eq!(t.text(), "aé");
        assert_eq!(t.cursor, 3);
        t.insert_char('🦀');
        assert_eq!(t.text(), "aé🦀");
        assert_eq!(t.cursor, 7);
    }

    /// Insert mid-string splits at the cursor position. A naive
    /// `String::push` implementation would lose this.
    #[test]
    fn insert_mid_string_splits_at_cursor() {
        let mut t = line("abc");
        t.move_home();
        t.move_right();
        t.insert_char('X');
        assert_eq!(t.text(), "aXbc");
        assert_eq!(t.cursor, 2);
    }

    /// Backspace removes the code point before the cursor — not a byte.
    /// The pre-port stub used `String::pop` which is end-only; this guards
    /// the property when inserting mid-string.
    #[test]
    fn backspace_removes_previous_code_point() {
        let mut t = line("aé🦀");
        t.backspace();
        assert_eq!(t.text(), "aé");
        t.backspace();
        assert_eq!(t.text(), "a");
        t.backspace();
        assert_eq!(t.text(), "");
        // Backspace at start is a no-op — would panic on naive byte arithmetic.
        t.backspace();
        assert_eq!(t.text(), "");
    }

    /// Forward delete removes the code point AT the cursor.
    #[test]
    fn delete_removes_code_point_at_cursor() {
        let mut t = line("a🦀b");
        t.move_home();
        t.delete();
        assert_eq!(t.text(), "🦀b");
        t.delete();
        assert_eq!(t.text(), "b");
        t.move_end();
        t.delete();
        assert_eq!(t.text(), "b", "delete at end must be a no-op");
    }

    /// Left/right cursor moves are code-point sized, not byte sized.
    /// Without this, navigating across multi-byte chars would land on a
    /// non-boundary and the next insert/delete would panic.
    #[test]
    fn move_left_right_traverse_code_points() {
        let mut t = line("aé🦀");
        t.move_left();
        assert_eq!(t.cursor, 3, "left past 🦀 (4 bytes) lands on é boundary");
        t.move_left();
        assert_eq!(t.cursor, 1, "left past é (2 bytes) lands on a boundary");
        t.move_left();
        assert_eq!(t.cursor, 0);
        t.move_left();
        assert_eq!(t.cursor, 0, "left at start is a no-op");
        t.move_right();
        assert_eq!(t.cursor, 1);
        t.move_right();
        assert_eq!(t.cursor, 3);
    }

    #[test]
    fn home_and_end_jump_to_boundaries() {
        let mut t = line("hello");
        t.move_home();
        assert_eq!(t.cursor, 0);
        t.move_end();
        assert_eq!(t.cursor, 5);
    }

    /// `cursor_col` reflects DISPLAY width, not byte offset. CJK chars are
    /// 2 columns; the renderer reverse-paints at that column.
    #[test]
    fn cursor_col_uses_display_width() {
        let t = line("aé");
        assert_eq!(t.cursor_col(), 2);
        let t = line("a中");
        assert_eq!(t.cursor_col(), 3);
    }

    /// Render paints the left bar and the typed text. Focused + non-empty
    /// paints a reversed cursor cell at `bar_cols + cursor_col`.
    #[test]
    fn render_paints_cursor_at_display_column_when_focused() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        let mut t = TextLine::default();
        for c in "hi".chars() {
            t.insert_char(c);
        }
        t.render(&mut buf, area, true, None);
        let symbol_at = |x: u16| buf[(x, 0)].symbol().to_string();
        assert_eq!(symbol_at(2), "h");
        assert_eq!(symbol_at(3), "i");
        let cursor_cell = &buf[(4, 0)];
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "focused cursor cell must be reversed; got {:?}",
            cursor_cell.style()
        );
    }

    /// Unfocused empty editor renders the caller-supplied placeholder.
    /// Without this the notes pane would render a blank row and the user
    /// wouldn't know they can Tab into it.
    #[test]
    fn render_shows_placeholder_when_empty_and_unfocused() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let t = TextLine::default();
        t.render(&mut buf, area, false, Some("press Tab to add a note"));
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            row.contains("press Tab to add a note"),
            "expected placeholder in unfocused empty row, got: {row:?}"
        );
    }

    /// Focused empty editor shows the cursor block at column 0 of the
    /// body (right after the left bar) so the user can see where they're
    /// about to type, even before any input. The placeholder is suppressed
    /// to avoid overlaying text under the cursor.
    #[test]
    fn render_paints_cursor_at_body_origin_when_focused_and_empty() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        let t = TextLine::default();
        t.render(&mut buf, area, true, Some("press Tab"));
        let cursor_cell = &buf[(2, 0)];
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "focused-empty cursor must reverse the body's first column"
        );
        // Placeholder must NOT have been painted — focused state suppresses it.
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            !row.contains("press Tab"),
            "placeholder must be suppressed when focused, got: {row:?}"
        );
    }
}
