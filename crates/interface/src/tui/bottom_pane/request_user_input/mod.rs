// Phase 3 of the bottom-pane port: this skeleton holds the geometric
// accessors the layout module calls into. The state machine (key handling,
// focus shifts, answer submission, render) lands in Phase 6. Until then this
// module has no consumer outside its own tests + the sibling `layout` module.
// See `docs/tui-bottom-pane-port.md`.
#![allow(dead_code)]

//! Picker overlay state for `request_user_input`.
//!
//! Phase 3 ships only what the layout math needs: the question list, the
//! per-question `AnswerState`, and a `current_idx`. The actual key handling,
//! focus model, and render path arrive in Phases 4–6.
//!
//! Layout math (`layout.rs`) consumes this struct via a small set of
//! geometric accessors so the layout port can be tested with concrete
//! overlay states before the state machine exists.

mod layout;

#[allow(unused_imports)] // Re-export consumed by render.rs (Phase 4, parallel session).
pub(crate) use layout::LayoutSections;

use cortex::tool_handlers::user_input::{Question, OTHER_LABEL};
use unicode_width::UnicodeWidthStr;

use crate::tui::scroll_state::ScrollState;
use crate::tui::wrapping;

/// Vertical spacers reserved between visual sections (question → options →
/// notes/footer) when there is enough room. Matches the reference TUI
/// constant; layout.rs is a near-verbatim port and references this value
/// when collapsing under tight space.
pub(super) const DESIRED_SPACERS_BETWEEN_SECTIONS: u16 = 2;

/// Default visible rows used for selection-list density. Five rows lets the
/// reader see the highlighted row plus the two neighbours above and below
/// without consuming so much height that small terminals lose the question.
pub(super) const OPTIONS_PREFERRED_VISIBLE: u16 = 5;

/// Height the notes editor wants when active: label row + input row + cursor.
/// Phase 5 swaps the placeholder text editor for a real one; the preferred
/// height stays the same so layout doesn't need to change.
pub(super) const NOTES_DESIRED_HEIGHT: u16 = 3;

/// Per-question UI state — selection cursor + free-form notes draft.
///
/// Phase 6 wires keys into this; Phase 3 just needs the fields so layout
/// can ask "does this question want notes visible?"
#[derive(Default, Debug, Clone)]
pub(super) struct AnswerState {
    pub(super) options_state: ScrollState,
    /// Indices into the *displayed* option list (including the auto-appended
    /// "Other" row at the end). Multi-select uses every entry; single-select
    /// keeps at most one.
    pub(super) selected: Vec<usize>,
    /// Free-form notes draft typed in the Tab-focused notes field.
    pub(super) notes: String,
    /// `true` when the user has explicitly opened the notes editor for this
    /// question (Tab into Notes focus, or any key while Notes is focused).
    /// Layout collapses the notes section while this is `false` so the
    /// options list gets the room.
    pub(super) notes_visible: bool,
}

/// Which sub-region of the overlay currently owns the cursor. Phase 3 only
/// needs `Options` (default); `Notes` is wired in Phase 6.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Focus {
    #[default]
    Options,
    Notes,
}

/// Overlay state for one batch of `request_user_input` questions.
pub(crate) struct RequestUserInputOverlay {
    questions: Vec<Question>,
    answers: Vec<AnswerState>,
    current_idx: usize,
    focus: Focus,
}

impl RequestUserInputOverlay {
    /// Construct a fresh overlay for a non-empty question batch. Returns
    /// `None` for an empty batch — the agent should never produce one
    /// (`parse_questions` enforces `>= 1`), but the overlay still refuses
    /// to construct in that case rather than panicking downstream.
    pub(crate) fn new(questions: Vec<Question>) -> Option<Self> {
        if questions.is_empty() {
            return None;
        }
        let answers = questions.iter().map(|_| AnswerState::default()).collect();
        Some(Self {
            questions,
            answers,
            current_idx: 0,
            focus: Focus::Options,
        })
    }

    pub(super) fn questions(&self) -> &[Question] {
        &self.questions
    }

    pub(super) fn current(&self) -> &Question {
        &self.questions[self.current_idx]
    }

    pub(super) fn current_answer(&self) -> &AnswerState {
        &self.answers[self.current_idx]
    }

    /// Total number of rows in the options list for the current question —
    /// caller-provided options plus the auto-appended "Other" row.
    pub(super) fn current_options_len(&self) -> usize {
        self.current().options.len() + 1
    }

    // ----- Geometric accessors consumed by layout.rs ---------------------

    /// `true` when there are selectable options to render. borg's
    /// `parse_questions` enforces ≥ 2 options per question and always
    /// auto-appends "Other", so this is effectively always `true`, but the
    /// hook stays in place to mirror the reference layout's contract — a
    /// future free-form-only question variant would return `false` here.
    pub(super) fn has_options(&self) -> bool {
        !self.current().options.is_empty()
    }

    /// Whether the notes editor should render at all for the current
    /// question. The user must explicitly open it (Tab into Notes focus)
    /// before the layout reserves room — otherwise short prompts stay
    /// dense.
    pub(super) fn notes_ui_visible(&self) -> bool {
        self.current_answer().notes_visible || self.focus == Focus::Notes
    }

    /// Wrap the current question text to `width`, returning each wrapped
    /// row as an owned `String`. Layout uses `len()` to compute height and
    /// later truncates the vec in place when space runs out.
    pub(super) fn wrapped_question_lines(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let width = width as usize;
        wrapping::wrap(&self.current().question, width)
            .into_iter()
            .map(std::borrow::Cow::into_owned)
            .collect()
    }

    /// Preferred height for the options list — capped at
    /// `OPTIONS_PREFERRED_VISIBLE` so a long list doesn't push the
    /// question off-screen on tall terminals.
    pub(super) fn options_preferred_height(&self, _width: u16) -> u16 {
        let len = self.current_options_len() as u16;
        len.min(OPTIONS_PREFERRED_VISIBLE)
    }

    /// Total height needed to render every option row at once. Used by the
    /// layout's "grow into leftover space" pass after the preferred slot
    /// has been allocated.
    pub(super) fn options_required_height(&self, _width: u16) -> u16 {
        self.current_options_len() as u16
    }

    /// Preferred height for the notes editor when it's visible. Returns 0
    /// when the user hasn't opened notes for this question, so layout
    /// reserves no space.
    pub(super) fn notes_input_height(&self, _width: u16) -> u16 {
        if self.notes_ui_visible() {
            NOTES_DESIRED_HEIGHT
        } else {
            0
        }
    }

    /// Footer hint rows. Phase 4 replaces this with a real composer of
    /// hints (Enter/Tab/Esc descriptions); for now a single row is enough
    /// for layout to be exercised end-to-end.
    pub(super) fn footer_required_height(&self, _width: u16) -> u16 {
        1
    }
}

/// Width helper used by sibling modules + tests. Kept here so `unicode-width`
/// dep stays scoped to one place.
pub(super) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Label used for the auto-appended "Other" row. Public so render + tests
/// share the same string the cortex side already advertises.
pub(super) fn other_label() -> &'static str {
    OTHER_LABEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::tool_handlers::user_input::QuestionOption;

    fn q(id: &str, prompt: &str, options: usize) -> Question {
        Question {
            id: id.into(),
            header: "Test".into(),
            question: prompt.into(),
            options: (0..options)
                .map(|i| QuestionOption {
                    label: format!("Option {i}"),
                    description: format!("description {i}"),
                })
                .collect(),
            multi_select: false,
        }
    }

    /// Empty batch refuses to construct — protocol invariant the cortex side
    /// already enforces. If we ever broke `parse_questions` validation, this
    /// guards the overlay from being built into an unreachable state.
    #[test]
    fn empty_question_batch_refuses_to_construct() {
        assert!(RequestUserInputOverlay::new(Vec::new()).is_none());
    }

    /// `current_options_len` includes the auto-appended "Other" row. Layout
    /// math derives heights from this length and would under-allocate by one
    /// row if we ever miscount.
    #[test]
    fn current_options_len_includes_other_row() {
        let overlay = RequestUserInputOverlay::new(vec![q("a", "pick one", 3)]).unwrap();
        assert_eq!(overlay.current().options.len(), 3);
        assert_eq!(overlay.current_options_len(), 4);
    }

    /// Notes section is collapsed by default and expands when the user
    /// focuses Notes. Without this gate, every overlay would reserve 3 rows
    /// for notes on first frame, shrinking the options list.
    #[test]
    fn notes_visibility_defaults_collapsed_until_focused() {
        let mut overlay = RequestUserInputOverlay::new(vec![q("a", "pick", 2)]).unwrap();
        assert!(!overlay.notes_ui_visible());
        assert_eq!(overlay.notes_input_height(80), 0);

        overlay.focus = Focus::Notes;
        assert!(overlay.notes_ui_visible());
        assert_eq!(overlay.notes_input_height(80), NOTES_DESIRED_HEIGHT);

        // Sticky once opened: switching back to Options keeps notes
        // reserved (matches the reference's `notes_visible` per-question
        // sticky flag).
        overlay.focus = Focus::Options;
        overlay.answers[0].notes_visible = true;
        assert!(overlay.notes_ui_visible());
    }

    /// Question wrapping returns one row per wrapped line. A long prompt at
    /// narrow width wraps to multiple rows; a short prompt fits in one.
    #[test]
    fn wrapped_question_lines_respects_width() {
        let overlay = RequestUserInputOverlay::new(vec![q(
            "a",
            "this is a fairly long prompt that should wrap when the width is tight",
            2,
        )])
        .unwrap();
        let wide = overlay.wrapped_question_lines(120);
        assert_eq!(wide.len(), 1, "long prompt fits in one row at width 120");
        let narrow = overlay.wrapped_question_lines(24);
        assert!(
            narrow.len() >= 2,
            "narrow width should force the prompt to wrap"
        );
        // Zero width must not panic and yields no rows — layout's height-zero
        // edge case (terminal collapsed to nothing).
        assert!(overlay.wrapped_question_lines(0).is_empty());
    }

    /// Preferred option height caps at the visible-rows constant so a
    /// 4-option question (incl. Other = 5) shows fully, but a future 10-row
    /// list still hands the question priority.
    #[test]
    fn options_preferred_height_caps_at_visible_rows() {
        // 2 options + Other = 3 rows → below the cap.
        let small = RequestUserInputOverlay::new(vec![q("a", "p", 2)]).unwrap();
        assert_eq!(small.options_preferred_height(80), 3);
        assert_eq!(small.options_required_height(80), 3);

        // 4 options + Other = 5 rows → at the cap.
        let max = RequestUserInputOverlay::new(vec![q("a", "p", 4)]).unwrap();
        assert_eq!(max.options_preferred_height(80), OPTIONS_PREFERRED_VISIBLE);
        assert_eq!(max.options_required_height(80), 5);
    }
}
