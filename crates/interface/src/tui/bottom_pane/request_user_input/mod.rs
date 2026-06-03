//! Picker overlay state for `request_user_input`.
//!
//! Functional overlay state: key bindings (navigation, toggle, notes,
//! submit, cancel), focus transitions between options and notes, and
//! answer serialization back to cortex via the `respond` oneshot.
//!
//! Layout math (`layout.rs`) and render path (`render.rs`) are pure
//! visual modules — every behavioural decision lives here.

mod layout;
mod render;

use ratatui::layout::Rect;

use crate::tui::custom_terminal::Frame;

use super::{BottomPaneView, CancellationEvent};

use std::collections::HashMap;

use cortex::tool_handlers::user_input::{
    Answer, Question, INTERRUPTED_MARKER, OTHER_LABEL, USER_NOTE_PREFIX,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthStr;

use crate::tui::scroll_state::ScrollState;
use crate::tui::wrapping;

use super::text_line::TextLine;

/// Vertical spacers reserved between visual sections (question → options →
/// notes/footer) when there is enough room. `layout.rs` references this value
/// when collapsing under tight space.
pub(super) const DESIRED_SPACERS_BETWEEN_SECTIONS: u16 = 2;

/// Default visible rows used for selection-list density. Five rows lets the
/// reader see the highlighted row plus the two neighbours above and below
/// without consuming so much height that small terminals lose the question.
pub(super) const OPTIONS_PREFERRED_VISIBLE: u16 = 5;

/// Height the notes editor wants when active: label row + input row + cursor.
pub(super) const NOTES_DESIRED_HEIGHT: u16 = 3;

/// Per-question UI state — selection cursor + free-form notes draft.
#[derive(Default, Debug, Clone)]
pub(super) struct AnswerState {
    pub(super) options_state: ScrollState,
    /// Indices into the *displayed* option list (including the auto-appended
    /// "Other" row at the end). Multi-select uses every entry; single-select
    /// keeps at most one.
    pub(super) selected: Vec<usize>,
    /// Free-form notes draft typed in the Tab-focused notes field.
    pub(super) notes: TextLine,
    /// `true` when the user has explicitly opened the notes editor for this
    /// question (Tab into Notes focus, or any key while Notes is focused).
    /// Layout collapses the notes section while this is `false` so the
    /// options list gets the room.
    pub(super) notes_visible: bool,
}

/// Which sub-region of the overlay currently owns the cursor.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Focus {
    #[default]
    Options,
    Notes,
}

/// Outcome of routing a key into the overlay. Lets the host decide whether
/// to keep dispatching to the overlay or peel it off.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum OverlayKeyOutcome {
    /// Key consumed; overlay stays active.
    Consumed,
    /// Overlay finished (Enter on the last question, or Esc cancel). Ctrl-C
    /// is delivered via the `on_ctrl_c` trait path instead — the host
    /// short-circuits it before the trait-level key dispatch so it can also
    /// cancel the in-flight turn.
    Done,
}

/// Overlay state for one batch of `request_user_input` questions.
pub(crate) struct RequestUserInputOverlay {
    questions: Vec<Question>,
    answers: Vec<AnswerState>,
    current_idx: usize,
    focus: Focus,
    /// Channel back to cortex. Taken on submit/cancel so a double-send
    /// (which would panic on `oneshot::Sender::send`) is impossible.
    respond: Option<oneshot::Sender<HashMap<String, Answer>>>,
    /// `true` once the overlay has either submitted answers or surfaced a
    /// cancellation. Host drops the overlay after the next dispatch.
    done: bool,
}

impl RequestUserInputOverlay {
    /// Construct a fresh overlay for a non-empty question batch with no
    /// responder attached — used by render/layout tests that don't drive
    /// the state machine. For real use call [`new_with_responder`].
    #[cfg(test)]
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
            respond: None,
            done: false,
        })
    }

    /// Construct an overlay bound to a oneshot responder. If the question
    /// batch is empty (cortex validates ≥ 1 question, but the overlay
    /// refuses defensively rather than panicking on `[0]`), the responder
    /// is given back to the caller via `Err` so the agent doesn't stall
    /// on a dropped channel.
    pub(crate) fn new_with_responder(
        questions: Vec<Question>,
        respond: oneshot::Sender<HashMap<String, Answer>>,
    ) -> Result<Self, oneshot::Sender<HashMap<String, Answer>>> {
        if questions.is_empty() {
            return Err(respond);
        }
        let answers = questions.iter().map(|_| AnswerState::default()).collect();
        Ok(Self {
            questions,
            answers,
            current_idx: 0,
            focus: Focus::Options,
            respond: Some(respond),
            done: false,
        })
    }

    /// `true` once the user has submitted or cancelled the batch. The host
    /// reads completion via the `BottomPaneView::is_complete` trait method;
    /// this accessor is for direct callers (tests) and stays cfg-test.
    #[cfg(test)]
    pub(crate) fn is_done(&self) -> bool {
        self.done
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

    /// `true` when there are selectable options to render. `parse_questions`
    /// enforces ≥ 2 options per question and always auto-appends "Other", so
    /// this is effectively always `true`, but the hook stays in place so a
    /// future free-form-only question variant can return `false` here.
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

    /// Footer hint rows reserved by layout. A single row carries the
    /// Enter/Tab/Esc legend; render.rs composes the actual text.
    pub(super) fn footer_required_height(&self, _width: u16) -> u16 {
        1
    }

    // ----- State machine -------------------------------------------------

    /// Index of the auto-appended "Other" row for the current question.
    /// Always `current().options.len()`.
    fn other_row(&self) -> usize {
        self.current().options.len()
    }

    /// Route a key event into the overlay. Returns whether the overlay
    /// consumed the key, finished, or finished with an in-flight turn
    /// cancellation request (Ctrl-C).
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> OverlayKeyOutcome {
        if self.done {
            // Defensive: host should have dropped us already, but ignore
            // late events rather than double-submitting.
            return OverlayKeyOutcome::Done;
        }

        // Esc cancels the entire batch with INTERRUPTED_MARKER per question.
        // Ctrl-C is intercepted by the host *before* the trait-level
        // key dispatch and routed through `on_ctrl_c`, so the trait path
        // only sees Esc here. We still match Ctrl-C defensively in case
        // a future caller invokes `handle_key` directly.
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || ctrl_c {
            self.cancel_with_interrupt();
            return OverlayKeyOutcome::Done;
        }

        if self.focus == Focus::Notes {
            self.handle_key_in_notes(key);
            return if self.done {
                OverlayKeyOutcome::Done
            } else {
                OverlayKeyOutcome::Consumed
            };
        }

        self.handle_key_in_options(key)
    }

    fn handle_key_in_notes(&mut self, key: KeyEvent) {
        let answer = &mut self.answers[self.current_idx];
        answer.notes_visible = true;
        match key.code {
            KeyCode::Tab => {
                // Tab toggles the notes editor closed (back to Options
                // focus). Sticky visibility is preserved so layout keeps
                // the notes section room for the next reopen.
                self.focus = Focus::Options;
            }
            KeyCode::Backspace => answer.notes.backspace(),
            KeyCode::Delete => answer.notes.delete(),
            KeyCode::Left => answer.notes.move_left(),
            KeyCode::Right => answer.notes.move_right(),
            KeyCode::Home => answer.notes.move_home(),
            KeyCode::End => answer.notes.move_end(),
            KeyCode::Enter => {
                // Enter inside Notes commits + advances the same way it
                // does in Options focus.
                self.advance_or_submit();
            }
            KeyCode::Char(c) => answer.notes.insert_char(c),
            _ => {}
        }
    }

    fn handle_key_in_options(&mut self, key: KeyEvent) -> OverlayKeyOutcome {
        let total_rows = self.current_options_len();
        let other_row = self.other_row();
        let multi = self.current().multi_select;

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.answers[self.current_idx]
                    .options_state
                    .move_up_wrap(total_rows);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.answers[self.current_idx]
                    .options_state
                    .move_down_wrap(total_rows);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::Left => {
                if self.current_idx > 0 {
                    self.current_idx -= 1;
                }
            }
            KeyCode::Right => {
                if self.current_idx + 1 < self.questions.len() {
                    self.current_idx += 1;
                }
            }
            KeyCode::Char(' ') if multi => {
                let selected_row = self.cursor_row();
                let answer = &mut self.answers[self.current_idx];
                if let Some(pos) = answer.selected.iter().position(|&i| i == selected_row) {
                    answer.selected.remove(pos);
                } else {
                    answer.selected.push(selected_row);
                    if selected_row == other_row {
                        answer.notes_visible = true;
                        self.focus = Focus::Notes;
                    }
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as u8 - b'1') as usize;
                if idx < total_rows {
                    self.answers[self.current_idx].options_state.selected_idx = Some(idx);
                    self.auto_open_notes_on_other(multi);
                }
            }
            KeyCode::PageUp => {
                self.answers[self.current_idx]
                    .options_state
                    .page_up_clamped(total_rows, OPTIONS_PREFERRED_VISIBLE as usize);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::PageDown => {
                self.answers[self.current_idx]
                    .options_state
                    .page_down_clamped(total_rows, OPTIONS_PREFERRED_VISIBLE as usize);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::Home => {
                self.answers[self.current_idx]
                    .options_state
                    .jump_top(total_rows, OPTIONS_PREFERRED_VISIBLE as usize);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::End => {
                self.answers[self.current_idx]
                    .options_state
                    .jump_bottom(total_rows, OPTIONS_PREFERRED_VISIBLE as usize);
                self.auto_open_notes_on_other(multi);
            }
            KeyCode::Tab => {
                self.answers[self.current_idx].notes_visible = true;
                self.focus = Focus::Notes;
            }
            KeyCode::Backspace | KeyCode::Delete => {
                // Clear the current question's selection — both single-select
                // cursor and multi-select committed set. Notes are untouched
                // (Backspace inside Notes focus has its own handler).
                let answer = &mut self.answers[self.current_idx];
                answer.options_state.selected_idx = None;
                answer.selected.clear();
            }
            KeyCode::Enter => {
                self.advance_or_submit();
            }
            _ => {}
        }
        if self.done {
            OverlayKeyOutcome::Done
        } else {
            OverlayKeyOutcome::Consumed
        }
    }

    /// Auto-open the notes editor when the user lands on the Other row in
    /// single-select mode — "Other" alone is meaningless without a free-form
    /// note, so the editor opens proactively. Multi-select waits for an
    /// explicit toggle so users can review all options first.
    fn auto_open_notes_on_other(&mut self, multi: bool) {
        if multi {
            return;
        }
        if self.cursor_row() == self.other_row() {
            self.answers[self.current_idx].notes_visible = true;
        }
    }

    /// Current cursor row for the active question. Defaults to 0 when no
    /// row is selected (matches the visual "first row highlighted by
    /// default" UX).
    fn cursor_row(&self) -> usize {
        self.answers[self.current_idx]
            .options_state
            .selected_idx
            .unwrap_or(0)
    }

    fn advance_or_submit(&mut self) {
        if self.current_idx + 1 < self.questions.len() {
            self.current_idx += 1;
            self.focus = Focus::Options;
        } else {
            self.submit();
        }
    }

    /// Build the per-question answer map and send it through the responder.
    /// Marks the overlay done unconditionally — even if the receiver was
    /// dropped (agent already cancelled), we don't want to keep dispatching.
    fn submit(&mut self) {
        let map = self.collect_answers();
        if let Some(tx) = self.respond.take() {
            if tx.send(map).is_err() {
                tracing::warn!("user-input: agent dropped receiver before submit");
            }
        }
        self.done = true;
    }

    /// Cancel with `[interrupted]` per question. Mirrors the legacy
    /// behaviour so the LLM's cancellation handling stays identical.
    fn cancel_with_interrupt(&mut self) {
        let mut map: HashMap<String, Answer> = HashMap::with_capacity(self.questions.len());
        for question in &self.questions {
            map.insert(
                question.id.clone(),
                Answer {
                    answers: vec![INTERRUPTED_MARKER.to_string()],
                },
            );
        }
        if let Some(tx) = self.respond.take() {
            if tx.send(map).is_err() {
                tracing::warn!("user-input: agent dropped receiver before cancel");
            }
        }
        self.done = true;
    }

    /// Assemble the answer map from per-question state. For single-select
    /// questions the cursor row is the answer; for multi-select every
    /// committed row is included in selection order. "Other" is reported as
    /// the literal `OTHER_LABEL`; notes (when present) are appended with
    /// the `user_note:` prefix so the LLM receives a flat `Vec<String>`.
    fn collect_answers(&self) -> HashMap<String, Answer> {
        let mut out = HashMap::with_capacity(self.questions.len());
        for (q, st) in self.questions.iter().zip(self.answers.iter()) {
            let mut answers: Vec<String> = Vec::new();
            let other_row = q.options.len();
            if q.multi_select {
                // Preserve option declaration order for deterministic
                // round-trip into cortex's serializer.
                for (i, opt) in q.options.iter().enumerate() {
                    if st.selected.contains(&i) {
                        answers.push(opt.label.clone());
                    }
                }
                if st.selected.contains(&other_row) {
                    answers.push(OTHER_LABEL.to_string());
                }
            } else {
                let cursor = st.options_state.selected_idx.unwrap_or(0);
                if cursor == other_row {
                    answers.push(OTHER_LABEL.to_string());
                } else if let Some(opt) = q.options.get(cursor) {
                    answers.push(opt.label.clone());
                }
            }
            let trimmed = st.notes.text().trim();
            if !trimmed.is_empty() {
                answers.push(format!("{USER_NOTE_PREFIX}{trimmed}"));
            }
            out.insert(q.id.clone(), Answer { answers });
        }
        out
    }
}

impl BottomPaneView for RequestUserInputOverlay {
    /// Preferred overlay height: question + options + (optional) notes + 1
    /// progress row + 1 footer hint. The layout module's collapse rules
    /// take over when the host hands us less; this is a "give me what
    /// you'd give me at full size" estimate the host arbitrates against.
    fn desired_height(&self, width: u16) -> u16 {
        let question = self.wrapped_question_lines(width).len() as u16;
        let options = self.options_preferred_height(width);
        let notes = self.notes_input_height(width);
        let footer = self.footer_required_height(width);
        // +1 progress row, +1 spacer between question and options when both
        // are present. Matches the layout module's preferred allocation.
        let spacers = if question > 0 && options > 0 { 1 } else { 0 };
        question
            .saturating_add(options)
            .saturating_add(notes)
            .saturating_add(footer)
            .saturating_add(1) // progress row
            .saturating_add(spacers)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        // Delegates to the visual layer in `render.rs`, which takes a
        // `&mut Buffer` so the call site stays Frame-agnostic. BottomPane
        // has already issued a `Clear` over `area`.
        self.render_ui(frame.buffer_mut(), area);
    }

    fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) {
        // The host inspects `is_complete()` after dispatch; the outcome
        // enum itself is only relevant to direct callers (tests). Ctrl-C
        // is delivered via `on_ctrl_c` so the host can ALSO cancel the
        // in-flight turn.
        self.handle_key(key);
    }

    fn is_complete(&self) -> bool {
        self.done
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if self.done {
            return CancellationEvent::Handled;
        }
        self.cancel_with_interrupt();
        // `cancel_with_interrupt` sets done=true, so the host will drop
        // the view after this call. We return `Handled` so the App-level
        // cancel path (which would tear down the turn) does NOT also fire
        // — Esc-style cancel keeps the turn streaming. Ctrl-C-style cancel
        // is delivered via `handle_key_event` so the App can disambiguate
        // from the key path.
        CancellationEvent::Handled
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

    /// Empty batch through the responder constructor returns the sender via
    /// `Err` so the caller can reply with an empty/interrupt map instead of
    /// silently dropping the channel — which would deadlock the agent on
    /// `recv()`. This is the contract `events.rs` relies on.
    #[test]
    fn empty_batch_with_responder_returns_sender_via_err() {
        let (tx, mut rx) = oneshot::channel();
        let err_tx = match RequestUserInputOverlay::new_with_responder(Vec::new(), tx) {
            Ok(_) => panic!("empty batch must not construct an overlay"),
            Err(tx) => tx,
        };
        // The recovered sender still works — agent can be unblocked, and the
        // exact (empty) payload arrives on the original receiver.
        err_tx
            .send(HashMap::new())
            .expect("recovered sender must still send");
        let received = rx.try_recv().expect("payload must arrive on the receiver");
        assert!(received.is_empty(), "empty-batch reply must carry an empty map");
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

    fn multi_q(id: &str, options: usize) -> Question {
        let mut question = q(id, "pick any", options);
        question.multi_select = true;
        question
    }

    fn key_code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Submit on a single-select question with cursor on first option
    /// produces that option's label. Receiver gets the answer map keyed
    /// by question id; overlay flips to `done`.
    #[test]
    fn single_select_enter_submits_selected_option() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay =
            RequestUserInputOverlay::new_with_responder(vec![q("timezone", "tz?", 3)], tx)
                .ok()
                .unwrap();
        // Cursor defaults to None; pressing Down once selects row 0 (wrap
        // semantics of move_down_wrap on an empty selection).
        let outcome = overlay.handle_key(key_code(KeyCode::Down));
        assert_eq!(outcome, OverlayKeyOutcome::Consumed);
        let outcome = overlay.handle_key(key_code(KeyCode::Enter));
        assert_eq!(outcome, OverlayKeyOutcome::Done);
        assert!(overlay.is_done());
        let answers = rx.try_recv().expect("submit should send a response");
        let answer = answers.get("timezone").expect("answer keyed by id");
        assert_eq!(answer.answers, vec!["Option 0".to_string()]);
    }

    /// Multi-select: Space toggles a row into `selected`, second Space
    /// untoggles. Submission emits every toggled option in declaration order
    /// — order independence is the contract `collect_answers` is supposed
    /// to deliver to the LLM.
    #[test]
    fn multi_select_space_toggles_and_preserves_declaration_order() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay =
            RequestUserInputOverlay::new_with_responder(vec![multi_q("flavors", 3)], tx)
                .ok()
                .unwrap();
        // Toggle rows 2, 0, 1 in that order; expect declaration order back.
        overlay.handle_key(key_char('3'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        overlay.handle_key(key_char('1'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        overlay.handle_key(key_char('2'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        // Untoggle row 1 to ensure remove path works.
        overlay.handle_key(key_char('1'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        overlay.handle_key(key_code(KeyCode::Enter));
        let answers = rx.try_recv().expect("submit");
        assert_eq!(
            answers.get("flavors").unwrap().answers,
            vec!["Option 1".to_string(), "Option 2".to_string()],
            "selected options must surface in declaration order regardless of toggle order"
        );
    }

    /// Tab switches focus to Notes; typed chars append to the notes
    /// buffer; submission appends them with the `user_note:` prefix. This
    /// is the round-trip the LLM relies on to receive free-form context.
    #[test]
    fn tab_into_notes_round_trips_typed_text_with_user_note_prefix() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(vec![q("style", "?", 2)], tx)
            .ok()
            .unwrap();
        overlay.handle_key(key_code(KeyCode::Down)); // pick option 0
        overlay.handle_key(key_code(KeyCode::Tab));
        assert_eq!(overlay.focus, Focus::Notes);
        for ch in "hello".chars() {
            overlay.handle_key(key_char(ch));
        }
        overlay.handle_key(key_code(KeyCode::Backspace));
        // Enter inside Notes submits when on the last question.
        overlay.handle_key(key_code(KeyCode::Enter));
        let answers = rx.try_recv().expect("submit");
        assert_eq!(
            answers.get("style").unwrap().answers,
            vec!["Option 0".to_string(), "user_note: hell".to_string()]
        );
    }

    /// Esc on any question cancels the entire batch with INTERRUPTED_MARKER
    /// per question. Receiver MUST get the marker map so the LLM never
    /// hangs on a dropped channel.
    #[test]
    fn esc_cancels_batch_with_interrupt_marker_per_question() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay =
            RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 2), q("b", "?", 2)], tx)
                .ok()
                .unwrap();
        let outcome = overlay.handle_key(key_code(KeyCode::Esc));
        assert_eq!(outcome, OverlayKeyOutcome::Done);
        assert!(overlay.is_done());
        let answers = rx.try_recv().expect("cancel must send marker map");
        assert_eq!(answers.len(), 2);
        for id in ["a", "b"] {
            assert_eq!(
                answers.get(id).unwrap().answers,
                vec!["[interrupted]".to_string()],
                "every question must carry the interrupt marker"
            );
        }
    }

    /// Ctrl-C delivered via the `on_ctrl_c` trait path cancels the batch
    /// with INTERRUPTED_MARKER per question AND returns `Handled` so the
    /// host can ALSO tear down the in-flight LLM turn. Esc through the
    /// key path returns Done (overlay dismissed, turn resumes).
    #[test]
    fn ctrl_c_cancels_via_on_ctrl_c_and_esc_via_handle_key() {
        let (tx_a, _rx_a) = oneshot::channel();
        let mut esc_overlay =
            RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 2)], tx_a)
                .ok()
                .unwrap();
        assert_eq!(
            esc_overlay.handle_key(key_code(KeyCode::Esc)),
            OverlayKeyOutcome::Done
        );
        assert!(esc_overlay.is_done());

        let (tx_b, mut rx_b) = oneshot::channel();
        let mut ctrl_overlay =
            RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 2)], tx_b)
                .ok()
                .unwrap();
        assert_eq!(
            ctrl_overlay.on_ctrl_c(),
            super::super::CancellationEvent::Handled
        );
        assert!(ctrl_overlay.is_done());
        let answers = rx_b.try_recv().expect("on_ctrl_c must send interrupt map");
        assert_eq!(answers.get("a").unwrap().answers, vec!["[interrupted]"]);
    }

    /// Enter on a non-last question advances to the next question instead
    /// of submitting. Submission happens only on the final question's
    /// Enter. Without this, multi-question batches could never complete.
    #[test]
    fn enter_advances_through_batch_then_submits_on_last_question() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(
            vec![q("a", "?", 2), q("b", "?", 2), q("c", "?", 2)],
            tx,
        )
        .ok()
        .unwrap();
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Enter));
        assert_eq!(overlay.current_idx, 1, "Enter advances on non-last");
        assert!(!overlay.is_done());

        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Enter));
        assert_eq!(overlay.current_idx, 2);
        assert!(!overlay.is_done());

        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Enter));
        assert!(overlay.is_done(), "Enter on last question submits");
        let answers = rx.try_recv().expect("submit");
        assert_eq!(answers.len(), 3);
    }

    /// Single-select cursor landing on the "Other" row auto-opens the
    /// notes editor — "Other" without context is useless to the LLM.
    /// Regression here breaks the onboarding UX where Other is common.
    #[test]
    fn single_select_other_row_auto_opens_notes() {
        let (tx, _rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 2)], tx)
            .ok()
            .unwrap();
        // 3 rows total (2 options + Other). Press Down once (cursor 0),
        // Down again (cursor 1), Down again (cursor 2 = Other).
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Down));
        assert_eq!(overlay.cursor_row(), 2);
        assert!(
            overlay.answers[0].notes_visible,
            "single-select on Other row must auto-open notes"
        );
    }

    /// `1`-`9` keys jump the cursor to that 1-indexed option. Out-of-range
    /// digits are ignored rather than panicking on a stale index.
    #[test]
    fn numeric_keys_jump_cursor_in_range_only() {
        let (tx, _rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 3)], tx)
            .ok()
            .unwrap();
        // 3 options + Other = 4 rows (indices 0-3).
        overlay.handle_key(key_char('2'));
        assert_eq!(overlay.cursor_row(), 1);
        overlay.handle_key(key_char('4'));
        assert_eq!(overlay.cursor_row(), 3);
        overlay.handle_key(key_char('9'));
        assert_eq!(
            overlay.cursor_row(),
            3,
            "out-of-range digit must be ignored"
        );
    }

    /// Late keys after `done` must be no-ops. The host normally drops the
    /// overlay on the dispatch that flipped `done`, but a stray event
    /// between that dispatch and the drop must not re-submit on a taken
    /// oneshot sender (which would panic with "send on closed channel"
    /// after some refactors).
    #[test]
    fn keys_after_done_are_no_op_and_dont_double_send() {
        let (tx, mut rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(vec![q("a", "?", 2)], tx)
            .ok()
            .unwrap();
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Enter));
        assert!(overlay.is_done());
        // Drain the legitimate submission.
        let _ = rx.try_recv().expect("first submit");
        // Subsequent keys must report Done and not panic.
        let outcome = overlay.handle_key(key_code(KeyCode::Enter));
        assert_eq!(outcome, OverlayKeyOutcome::Done);
    }

    /// End-to-end integration: drive the overlay programmatically through a
    /// multi-question batch, then serialize the answers through cortex's
    /// `serialize_answers` and assert the JSON matches the cortex-contract
    /// shape. Exercises the same code path the running app would, including
    /// the user_note prefix, declaration order for multi-select, and the
    /// preserved-question-order JSON output that the LLM sees.
    #[test]
    fn end_to_end_overlay_drives_cortex_serialize_round_trip() {
        use cortex::tool_handlers::user_input::serialize_answers;
        let questions = vec![q("color", "?", 2), multi_q("flavors", 3), q("size", "?", 2)];
        let (tx, mut rx) = oneshot::channel();
        let mut overlay = RequestUserInputOverlay::new_with_responder(questions.clone(), tx)
            .ok()
            .unwrap();

        // Q1: pick Option 0 (single-select).
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Enter)); // advance to Q2

        // Q2: multi-select; toggle Option 2 then Option 0 (out of order)
        // to prove declaration-order is preserved through serialization.
        overlay.handle_key(key_char('3'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        overlay.handle_key(key_char('1'));
        overlay.handle_key(key_code(KeyCode::Char(' ')));
        overlay.handle_key(key_code(KeyCode::Enter)); // advance to Q3

        // Q3: select Option 0, open notes, type "fits well", submit.
        overlay.handle_key(key_code(KeyCode::Down));
        overlay.handle_key(key_code(KeyCode::Tab));
        for ch in "fits well".chars() {
            overlay.handle_key(key_char(ch));
        }
        overlay.handle_key(key_code(KeyCode::Enter)); // submits

        assert!(overlay.is_done());
        let answers = rx.try_recv().expect("submit");
        let json = serialize_answers(&questions, &answers);

        // Question order is preserved (color, flavors, size) — that's the
        // cortex contract: the LLM gets a stable-keyed map keyed by the
        // declared question ids.
        let color_pos = json.find("\"color\"").expect("color key");
        let flavors_pos = json.find("\"flavors\"").expect("flavors key");
        let size_pos = json.find("\"size\"").expect("size key");
        assert!(
            color_pos < flavors_pos && flavors_pos < size_pos,
            "serialized order must follow question declaration order, got:\n{json}"
        );

        // Multi-select payload is declaration-ordered even though toggles
        // happened in (2, 0) order in the UI.
        assert!(
            json.contains(r#""flavors":{"answers":["Option 0","Option 2"]}"#),
            "multi-select must serialize options in declaration order:\n{json}"
        );

        // Single-select with notes appends user_note: prefix.
        assert!(
            json.contains(r#""size":{"answers":["Option 0","user_note: fits well"]}"#),
            "single-select with notes must round-trip with user_note prefix:\n{json}"
        );

        // Plain single-select with no notes is just the chosen label.
        assert!(
            json.contains(r#""color":{"answers":["Option 0"]}"#),
            "plain single-select must be just the label:\n{json}"
        );
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
