# Bottom-Pane Picker Port — Multi-Session Roadmap

**Goal:** replace borg's current `request_user_input` overlay (collides with the composer, only renders one option visible, no scroll, broken `Clear`) with a faithful port of the reference TUI's bottom-pane architecture so the picker behaves identically.

**Reference source:** `reference/<external>/codex-rs/tui/src/bottom_pane/` — read-only, do not modify. Cite line numbers in commits when porting verbatim logic, but do not name the source project in code, comments, or commit messages (see `.claude/CLAUDE.md`).

**Total scope:** ~14 kLOC of new TUI code + integration. This is multi-session work. Each phase below is sized to land as one or two commits and leave the tree green.

---

## Architectural shift

Today borg renders the picker by overlaying directly onto the composer's `Rect` — small area, partial `Clear`, content clipping. The reference TUI instead has a **BottomPane** host that owns the bottom portion of the screen and routes input/render to whichever **View** is currently active (composer, list selection, multi-select picker, request-user-input overlay, etc.). When a View activates, BottomPane *displaces* the composer for the View's preferred height.

Borg needs the same host. Once it exists, every existing popup (`command_popup`, `file_popup`, `plan_overlay`, `settings_popup`, …) can incrementally migrate into the unified BottomPane, but **migration of existing popups is out of scope for this port** — we only need the new picker hosted.

---

## Type mapping

borg keeps its existing protocol types — they already align well with the reference's `RequestUserInputQuestion`/`RequestUserInputQuestionOption`:

| borg (`crates/cortex/src/tool_handlers/user_input.rs`) | reference | Notes |
|---|---|---|
| `Question` | `RequestUserInputQuestion` | borg uses `multi_select: bool`; reference uses `is_other`/`is_secret`. borg auto-appends an "Other" row in the UI — keep that behaviour. |
| `QuestionOption { label, description }` | `RequestUserInputQuestionOption` | Identical. |
| `Answer { answers: Vec<String> }` | `RequestUserInputAnswer` | Identical. |
| `HashMap<String, Answer>` | `RequestUserInputResponse.answers` | Identical. |
| `MAX_QUESTIONS = 4`, `MIN_OPTIONS = 2`, `MAX_OPTIONS = 4`, `MAX_HEADER_CHARS = 12` | (looser in reference) | Keep borg's stricter validation in `parse_questions`. |
| `USER_NOTE_PREFIX = "user_note: "` | (no analog) | borg-specific — preserve. |
| `OTHER_LABEL = "Other"` | n/a | borg auto-appends. |
| `INTERRUPTED_MARKER = "[interrupted]"` | n/a | borg-specific. |

The reference's `is_secret` is a useful future addition (mask input in notes), but **do not add it in this port** — schema change requires a separate decision and a tool_definitions.rs update.

---

## Phase inventory

### Phase 1 — Foundation (this session) ✅

Land the smallest isolated unit + write this document.

- **Files added:** `crates/interface/src/tui/scroll_state.rs` (verbatim port of `bottom_pane/scroll_state.rs`, 204 lines logic + tests). Pure logic, no UI deps.
- **mod.rs:** register `mod scroll_state;`.
- **Tests:** 4 unit tests covering wrap navigation + visibility, page/jump clamping, empty-list defensive behaviour, zero-row viewport defence.
- **Verification:** `cargo test -p interface --bin borg tui::scroll_state` passes; `cargo clippy --all-targets -- -D warnings` clean.

### Phase 2 — `BottomPane` host skeleton

Introduce the host abstraction. No active Views yet — the composer is still rendered the old way; BottomPane just *exists* as scaffolding.

- **New module:** `crates/interface/src/tui/bottom_pane/mod.rs`.
- **Types ported:**
  - `BottomPane` struct (height-arbitration, View slot, focus state)
  - `BottomPaneView` trait (`desired_height`, `render`, `handle_key_event`, `should_hide_when_inactive`)
  - `CancellationEvent` / `InputResult` event types
- **Reference:** `reference/<external>/codex-rs/tui/src/bottom_pane/mod.rs` (2,958 lines — much of it is wiring for Views we don't need yet). Port only the host plumbing.
- **Integration:** `App<'a>` grows a `bottom_pane: BottomPane` field; `render_viewport` calls `bottom_pane.render(frame, app_layout.bottom_pane_area)` after the composer. No Views attach yet.
- **Layout change:** `layout::compute_layout` returns a new `bottom_pane` rect that is *initially zero-height* (no active View) — when active it claims height above the composer, displacing the transcript.

### Phase 3 — `request_user_input/layout.rs` port

Pure layout math — the trickiest part of the reference implementation but isolated.

- **File added:** `crates/interface/src/tui/bottom_pane/request_user_input/layout.rs` (verbatim port of the 363-line reference layout module).
- **Depends on:** the still-to-be-built `RequestUserInputOverlay` struct's accessor methods (`has_options`, `notes_ui_visible`, `wrapped_question_lines`, `options_preferred_height`, `options_required_height`, `notes_input_height`, `footer_required_height`). Stub these as `todo!()` for now; Phase 6 fills them in.
- **Constant:** `DESIRED_SPACERS_BETWEEN_SECTIONS` (look up exact value in reference mod.rs).

### Phase 4 — `request_user_input/render.rs` port

Visual layer.

- **File added:** `crates/interface/src/tui/bottom_pane/request_user_input/render.rs` (582 lines).
- **Selection marker:** `›` prefix (single-byte rendering, matches reference line 322).
- **Progress header:** `Question {N}/{Total} ({M} unanswered)` format.
- **Footer hint composition:** dynamic hints based on focus + selection state. Use borg's existing `theme::*` styles, not reference's `crate::colors`.
- **Notes section:** Phase 4 stubs this as a single-line read-only text rendering. Real editable notes need ChatComposer port (deferred — see Phase 7b).

### Phase 5 — minimal notes editor (interim)

Reference reuses its full `ChatComposer` (~2 kLOC + 3.5 kLOC `textarea.rs`) for notes. Porting that wholesale is a separate multi-session subproject. **Interim:** implement a 1-line `TextLine` editor (cursor + insert + backspace + clear) inline in the overlay. Visual parity later, behavioural parity now.

- **File added:** `crates/interface/src/tui/bottom_pane/text_line.rs` (~150 lines).
- **Functions:** cursor positioning, insert_char, backspace, clear, render with `▌` left-bar styling.
- **Future:** Phase 7b swaps this for a real `ChatComposer` port.

### Phase 6 — `request_user_input/mod.rs` port

The state machine that ties it all together.

- **File added:** `crates/interface/src/tui/bottom_pane/request_user_input/mod.rs` (~1,350 impl lines).
- **Types ported:**
  - `Focus` enum (`Options`, `Notes`)
  - `RequestUserInputOverlay` struct (questions, per-question `ScrollState`, per-question selection, per-question notes, current question index, focus)
  - `BottomPaneView` impl: `desired_height`, `render`, `handle_key_event`
- **Key bindings (verbatim from reference):**
  - ↑/k, ↓/j: scroll selection (wrap)
  - PageUp/PageDown: page selection (clamp)
  - Home/End: jump top/bottom
  - 1–9: jump to numbered option
  - Space: toggle selection (or select on single-select)
  - Backspace/Delete: clear current question's selection
  - Tab: toggle Options ↔ Notes focus
  - Enter: advance to next question, or submit on last
  - ←/→ or Ctrl+P/Ctrl+N: navigate between questions
  - Esc: clear notes if focused, else interrupt batch
- **borg-specific addenda:**
  - "Other" row auto-appended (not part of `Question.options`) — keep current behaviour
  - `INTERRUPTED_MARKER` returned per unanswered question on Esc-cancel
  - `multi_select: true` enables checkbox semantics for that question only

### Phase 7 — Wire into `App<'a>`

Swap the old path for the new.

- **Remove:** `AppState::AwaitingInput`, `QuestionState`, `render_choices_overlay`, `handle_key_awaiting_input` in `crates/interface/src/tui/app/`.
- **Add:** `AppEvent::UserInputRequest` handler → `bottom_pane.set_view(RequestUserInputOverlay::new(questions, respond_tx))`.
- **Key dispatch:** when `BottomPane` has an active View, route keys to it first (View consumes or passes through to composer).
- **Submission:** when overlay completes, it sends `HashMap<String, Answer>` on the `respond_tx` oneshot — same protocol as today, no cortex-side changes.
- **Layout:** active View's `desired_height` displaces transcript height.

### Phase 7b — `ChatComposer` notes upgrade (optional, can defer)

Replace `TextLine` with a real multi-line text editor matching the reference's `ChatComposer`. Requires porting `textarea.rs` (3,505 lines) and `chat_composer/` subdir. **Recommended:** defer to a separate spec; the `TextLine` editor from Phase 5 is sufficient for the picker UX.

### Phase 8 — Test port + borg integration tests

- Port the reference's 61 unit tests for `request_user_input`, adapting struct field names + borg-specific behaviours (Other row, INTERRUPTED_MARKER, multi_select).
- Add cortex-side integration test in `crates/cortex/tests/`: invoke `handle_request_user_input`, drive the picker programmatically, assert serialized answers match the cortex contract (preserved question order, `user_note:` prefix round-trip).

### Phase 9 — Validate + ship

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test -p interface -p cortex` (scoped; full workspace only for the final ship)
- Manual: `borg` REPL → trigger `request_user_input` via onboarding → confirm visible options, scroll, multi-select, Other row, notes, navigation.
- Commit per phase. Final commit may bundle Phases 7+9 if 7 lands cleanly.

---

## Cross-cutting rules

- **No external-project name** in code, comments, commit messages, or PR descriptions. Refer to "reference TUI", "bottom pane reference", etc.
- **Borg's `Question`/`QuestionOption`/`Answer` types stay put** — do not introduce parallel types. The cortex-side contract is stable.
- **Existing popups (`command_popup`, `file_popup`, `plan_overlay`, etc.) are NOT migrated** by this port. They keep their current rendering. BottomPane is introduced specifically to host the new picker.
- **Old `AwaitingInput` path is deleted in Phase 7**, not earlier. Until Phase 7 lands, the picker still works the old (broken) way — but the new module tree compiles alongside.
- **Tests must accompany every phase.** No phase lands without exercising its new code.

---

## Resumption checklist

When picking this up in a future session:

1. Re-read this document.
2. `git log --grep="bottom-pane" --oneline` to see which phases shipped.
3. Re-inventory remaining files via `wc -l reference/<external>/codex-rs/tui/src/bottom_pane/request_user_input/*.rs` etc.
4. Mark the next phase's task as `in_progress`, port the file, write/port the tests, fmt/clippy/test, commit, push.
5. Update this document's phase-status table (below) as phases land.

### Phase status

| Phase | Status | Commit |
|---|---|---|
| 1. Foundation (ScrollState + this doc) | ✅ landed | _filled in by ship commit_ |
| 2. BottomPane host skeleton | ⏳ pending | — |
| 3. layout.rs port | ⏳ pending | — |
| 4. render.rs port | ⏳ pending | — |
| 5. TextLine editor | ⏳ pending | — |
| 6. mod.rs (overlay state machine) | ⏳ pending | — |
| 7. App wiring + old path removal | ⏳ pending | — |
| 7b. ChatComposer notes upgrade (optional) | ⏳ deferred | — |
| 8. Tests | ⏳ pending | — |
| 9. Final validate + ship | ⏳ pending | — |
