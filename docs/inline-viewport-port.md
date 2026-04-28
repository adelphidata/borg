# Inline Viewport Port — End-to-End Implementation Spec

Goal: switch borg's TUI from fullscreen alt-screen + manual `scroll_offset` to an inline viewport with terminal-native scrollback. Outcome: trackpad/mouse-wheel scrolls the terminal's own scrollback (no app-side scroll model). Native click+drag text selection still works (hard invariant). Up/Down arrows always navigate composer history.

Reference implementation lives at `reference/codex/codex-rs/tui/`. Port the architecture verbatim where possible; adapt where borg's app/event model differs.

---

## Status

| Step | Commit | Status |
|---|---|---|
| 1. `custom_terminal.rs` port + ratatui feature flags | `afef448` | ✅ Done |
| 2. `wrapping.rs` enrichment (URL-aware adaptive wrap) | `9e38dc0` | ✅ Done |
| 3. `insert_history.rs` port | — | Todo |
| 4. `tui_lifecycle.rs` (set_modes / restore / enter_alt / leave_alt / update_inline_viewport / draw) | — | Todo |
| 5. `App` struct: drop scroll state, add `cells_emitted` watermark | — | Todo |
| 6. `App::render` rewrite — viewport chrome only, flush stable cells via `insert_history_lines` | — | Todo |
| 7. Streaming/finalize model in `events.rs` — define stable predicate | — | Todo |
| 8. `run_event_loop` rewire — use new `Tui::draw` pipeline | — | Todo |
| 9. Drop scroll keys in `key_handlers.rs`; collapse Up/Down to composer history | — | Todo |
| 10. Wrap transcript pager + large popups in `enter_alt_screen` / `leave_alt_screen` | — | Todo |
| 11. Update guard tests + delete obsolete scroll tests | — | Todo |
| 12. Update `CLAUDE.md` mouse-interaction section + `docs/getting-started.md` | — | Todo |
| 13. End-to-end code review pass; fix every issue found | — | Todo |

---

---

## Parallelization plan — running multiple sessions simultaneously

The remaining work splits cleanly into 4 tracks. Tracks A and C have **zero overlap** and can start immediately in parallel sessions. Tracks B and D depend on A's deliverable (`Tui::draw` signature + `enter_alt_screen`).

```
Step 1  (custom_terminal.rs)  ✅ done
Step 2  (wrapping.rs)         ✅ done
        │
        ├────────────┬──────────────┬───────────────┐
        ▼            ▼              ▼               ▼
     TRACK A      TRACK C       TRACK Docs      TRACK Plan
   (port-rest)  (scroll-rip)    (CLAUDE.md)   (no-op now)
   Steps 3, 4    Steps 9 +       Step 12
                 part of 11
        │            │              │
        └────┬───────┘              │
             ▼                      │
          TRACK B                   │
       (rewire app)                 │
       Steps 5–8, 11                │
             │                      │
             └──────────┬───────────┘
                        ▼
                     TRACK D
                  (popup audit)
                     Step 10
                        │
                        ▼
                    Step 13 (code review) — single session, last
```

### TRACK A — Port the rest of the reference rendering pipeline

**Touches**: `crates/interface/src/tui/insert_history.rs` (new), `crates/interface/src/tui/tui_lifecycle.rs` (new), `crates/interface/src/tui/mod.rs` (only the `mod` declarations).

**Conflict surface**: only the module-list at the top of `tui/mod.rs` (lines 1–35). One-line additions; merge-clean against any other track.

**Deliverable**: a new `pub struct Tui` exposing `Tui::new(custom_terminal::Terminal<...>)`, `Tui::draw(height, |frame| ...)`, `Tui::insert_history_lines(Vec<Line<'static>>)`, `Tui::enter_alt_screen()`, `Tui::leave_alt_screen()`, `Tui::clear_pending_history_lines()`. Plus a `set_modes()` / `restore()` pair.

**Prompt for the agent**:
> Continue from `docs/inline-viewport-port.md` Step 3 + Step 4. Port `reference/codex/codex-rs/tui/src/insert_history.rs` (843 lines) verbatim into `crates/interface/src/tui/insert_history.rs`, fixing imports (`crate::custom_terminal` → `super::custom_terminal`, `crate::wrapping` → `super::wrapping`). Then port the SUBSET of `reference/codex/codex-rs/tui/src/tui.rs` listed under "Subset of `tui.rs` to port" in the spec into a new `crates/interface/src/tui/tui_lifecycle.rs`. Skip event broker, frame requester, suspension, notifications, broadcast streams. Add `mod insert_history;` and `mod tui_lifecycle;` to `crates/interface/src/tui/mod.rs`. Run `cargo build -p interface` after each file lands; fix any let-chain or other unstable syntax (Rust 1.87). Commit each as `feat: port scrollback insert_history` and `feat: add Tui lifecycle wrapper`. Do NOT touch `app/`, `key_handlers.rs`, `events.rs`, `render.rs`, or any test files — those belong to other tracks.

### TRACK C — Strip the app-side scroll model

**Touches**: `crates/interface/src/tui/app/key_handlers.rs`, `crates/interface/src/tui/app/mod.rs` (only the scroll fields + scroll method bodies), all scroll-related tests in `app/mod.rs:1266–1640`.

**Conflict surface**: `App` struct field list. Track C **adds** `cells_emitted: usize` (no value yet) and **removes** `scroll_offset`, `total_lines`, `auto_scroll`. Track B then uses `cells_emitted`. Coordinate by having Track C land first (smaller diff).

**Crucial constraint**: Track C must **stub** the scroll method bodies to no-ops (don't delete the methods yet), because `app/render.rs` and `app/events.rs` still call them. Track B will delete the stubs once render is rewired.

**Prompt for the agent**:
> Continue from `docs/inline-viewport-port.md` Step 9 + the scroll-test deletions in Step 11. In `crates/interface/src/tui/app/mod.rs`: remove the fields `scroll_offset`, `total_lines`, `auto_scroll`, `transcript_area` from `pub struct App`. Add `pub cells_emitted: usize` and `pub pending_scrollback: Vec<ratatui::text::Line<'static>>`. Update `App::new` accordingly. Replace `scroll_up_by`, `scroll_down_by`, `scroll_to_top`, `scroll_to_bottom`, `max_scroll`, `viewport_height` method bodies with no-ops (`{}` for setters; `0` for getters) — do NOT delete the methods, callers in render.rs/events.rs still reference them and Track B will clean those up. In `crates/interface/src/tui/app/key_handlers.rs`: delete the End/Home/PageUp/PageDown match arms and Rules A and B for Up/Down (lines ~730–785), keeping only the fallthrough to `self.composer.handle_key(key)`. Delete every test in `app/mod.rs:1266–1640` whose name contains `scroll` or `wheel`. Add ONE new test: `up_arrow_with_empty_composer_recalls_history`. Run `cargo build -p interface && cargo test -p interface --lib --bins`. Commit as `refactor: drop app-side transcript scroll model`. Do NOT touch `mod.rs` (the file owning `run_event_loop`), `events.rs`, `render.rs`, `transcript_pager.rs`, popup files, or anything in `tui/` outside of `app/`.

### TRACK Docs — Update CLAUDE.md + getting-started

**Touches**: `CLAUDE.md`, `docs/getting-started.md`.

**Conflict surface**: zero with code tracks.

**Prompt for the agent**:
> Continue from `docs/inline-viewport-port.md` Step 12. Update the `Mouse Interaction — Hard Invariant` section in `CLAUDE.md`: keep the FORBIDDEN list and the guard-test list verbatim, replace the **Strategy** and **Behavior** paragraphs per the spec. Audit `docs/getting-started.md` for any mention of "scrollbar", "scroll up/down with arrows", "PageUp", "PageDown" in the TUI section; update wording to "scroll the terminal natively (trackpad/wheel/your terminal's scrollbar)". Commit as `docs: update mouse-interaction model for inline viewport`.

### TRACK B — Rewire the app render pipeline (depends on A and C)

**Touches**: `crates/interface/src/tui/app/render.rs`, `crates/interface/src/tui/app/events.rs`, `crates/interface/src/tui/mod.rs` (the `run_event_loop` function only).

**Prerequisite**: Tracks A and C must be merged. Pull both before starting.

**Prompt for the agent**:
> Continue from Steps 5 (rest), 6, 7, 8, 11 (rest) of `docs/inline-viewport-port.md`. Tracks A and C are merged at this point — `Tui` is available, `App` has `cells_emitted` and `pending_scrollback`, and scroll methods are stubbed. Now: implement `App::render_viewport` (rename of `App::render`); render only un-flushed cells `[cells_emitted..]` + chrome + popups + toasts; remove the `Scrollbar` widget render and the all-cells `Paragraph::scroll` math from `render.rs`. Implement `App::flush_stable_cells(width)` per the stability predicate in Step 5. Add `flush_stable_cells` calls at the end of every `process_*` handler in `events.rs`. In `handle_event_turn_complete`, force `cells_emitted = cells.len() - 1`. Add debug-only assertions guarding mutations of cells with index `< cells_emitted`. Rewire `run_event_loop` in `mod.rs` to take `&mut Tui`, drain `app.pending_scrollback` via `tui.insert_history_lines`, then call `tui.draw(viewport_height, |frame| app.render_viewport(frame))`. Drop `EnterAlternateScreen` from startup. Delete the now-unused stubbed scroll methods on `App`. Update guard tests in `mod.rs:1729–2137`: keep all FORBIDDEN-mode checks; add the `Scrollbar`-import guard and the `App`-struct `scroll_offset`-absence guard from the spec. `cargo build && cargo test -p interface --lib --bins` after each file. Commit logically: `feat: render only viewport chrome with scrollback flush`, `feat: emit stable history cells via insert_history_lines`, `feat: switch run_event_loop to inline-viewport pipeline`. Do NOT touch popups (Track D) or the transcript pager (Track D).

### TRACK D — Wrap full-screen surfaces in alt-screen (depends on B)

**Touches**: `crates/interface/src/tui/transcript_pager.rs`, every popup module under `crates/interface/src/tui/app/` matching the audit table, `crates/interface/src/tui/mod.rs::run_event_loop` (only to add `EnterAltScreen` / `LeaveAltScreen` action arms).

**Prerequisite**: Track B merged. `Tui::enter_alt_screen` / `leave_alt_screen` must already work in the inline pipeline.

**Prompt for the agent**:
> Continue from Step 10 of `docs/inline-viewport-port.md`. For each popup in the audit table marked **Alt-screen**, emit a new `AppAction::EnterAltScreen` from the popup's `show()` and `AppAction::LeaveAltScreen` from `dismiss()`. In `run_event_loop`, match those and call `tui.enter_alt_screen()` / `tui.leave_alt_screen()`. For the transcript pager, do the same on `AppState::TranscriptPager` enter/exit. Verify each popup renders correctly: open the TUI, run `/settings`, `/projects`, `/sessions`, etc., confirm full-screen layout. Commit as `feat: wrap large popups in alt-screen`.

### Coordination rules

1. **Each track commits to `main` directly** (per the user's "no PRs, just commits" preference).
2. Track owners pull from `origin/main` before each commit to stay current.
3. If a track touches a file owned by another track (impossible if the rules above are followed), STOP and rebase.
4. Track A and Track C run in parallel. Tracks Docs runs alone (no overlap). Track B starts after A+C land. Track D starts after B lands.
5. Final code-review pass (Step 13) runs after all tracks merge — single session, dispatches the `superpowers:code-reviewer` agent.

### Estimated wall-clock

| Track | Sessions | Approx LOC | Independent? |
|---|---|---:|---|
| A | 1 | +1240 | Yes (start now) |
| C | 1 | +30 / -250 | Yes (start now) |
| Docs | 1 | +20 / -10 | Yes (start now) |
| B | 1 | +280 / -370 | After A + C |
| D | 1 | +150 | After B |
| Code review | 1 | varies | After D |

**Total: 6 sessions, with up to 3 running concurrently** (A + C + Docs).

---

## Files to port from reference

All paths under `reference/codex/codex-rs/tui/src/`:

| Reference file | Lines | Borg destination | Adaptation |
|---|---|---|---|
| `custom_terminal.rs` | 763 | `crates/interface/src/tui/custom_terminal.rs` | ✅ Done. License header preserved (MIT-derived from ratatui). |
| `wrapping.rs` | 1407 | `crates/interface/src/tui/wrapping.rs` (replace existing 47-line file) | Port verbatim. Existing borg callers (3 sites — see grep below) keep using `wrap()` / `contains_url_like()` which remain in the new file. |
| `insert_history.rs` | 843 | `crates/interface/src/tui/insert_history.rs` | Port verbatim. Replace `crate::wrapping::*` imports with `super::wrapping::*`. Replace `crate::custom_terminal::*` with `super::custom_terminal::*`. |
| `tui.rs` (subset only) | ~400 of 948 | `crates/interface/src/tui/tui_lifecycle.rs` (new) | See "Subset of tui.rs" below. Skip event broker, frame requester, suspension, notifications, broadcast streams — borg has its own event loop. |

### Subset of `tui.rs` to port

Pull only these items into `tui_lifecycle.rs`:

- `pub type Terminal = CustomTerminal<CrosstermBackend<Stdout>>;` — type alias.
- `pub fn set_modes() -> Result<()>` (currently borg has equivalent inline at `crates/interface/src/tui/mod.rs:373-376`; replace).
- `fn restore_common(should_disable_raw_mode: bool) -> Result<()>`, `pub fn restore()`, `pub fn restore_keep_raw()`, `pub enum RestoreMode`.
- `struct EnableAlternateScroll` / `struct DisableAlternateScroll` — borg already has these at `tui/mod.rs:85-114`. Move into the new module so `enter_alt_screen` can use them.
- `keyboard_enhancement_disabled()` + helpers (`running_in_wsl`, `running_in_vscode_terminal`, `vscode_terminal_detected`, `parse_bool_env`, `windows_term_program`, `read_windows_term_program`).
- `fn keyboard_enhancement_disabled_for(...)` + tests.
- `pub struct Tui { terminal, alt_screen_enabled, alt_screen_active, alt_saved_viewport, pending_history_lines, is_zellij, ... }` — slimmed: drop event_broker, draw_tx, terminal_focused, suspend_context, frame_requester. Keep just what `draw()` and `enter/leave_alt_screen()` need.
- `Tui::new(...)` — minimal constructor: `terminal`, detect Zellij (`std::env::var("ZELLIJ_SESSION_NAME").is_ok()`), `alt_screen_enabled = !is_zellij`.
- `Tui::insert_history_lines(&mut self, lines: Vec<Line<'static>>)`.
- `Tui::clear_pending_history_lines(&mut self)`.
- `Tui::flush_pending_history_lines(...)` (private).
- `Tui::update_inline_viewport(...)` (private).
- `Tui::scroll_zellij_expanded_viewport(...)` (private).
- `Tui::draw(&mut self, height: u16, draw_fn: impl FnOnce(&mut custom_terminal::Frame))` — primary draw entry.
- `Tui::enter_alt_screen()` / `Tui::leave_alt_screen()` / `Tui::is_alt_screen_active()`.
- `Tui::clear_scrollback_and_visible_screen()` (delegate to terminal).
- `Tui::pending_viewport_area(&self)` if needed.

Do **NOT** port: `event_stream`, `frame_rate_limiter`, `frame_requester`, `job_control`, `notifications`. Borg has its own event loop, frame ticking, signal handling, and notification stack.

---

## File-by-file changes

### `Cargo.toml` (workspace)

✅ Done in `afef448`:
- `ratatui` features: `unstable-rendered-line-info`, `unstable-widget-ref`, `unstable-backend-writer`, `scrolling-regions`.
- Added `pretty_assertions = "1"`.

Add now if missing: nothing else required — `derive_more` was avoided by replacing `IsVariant` with a manual `is_put()` method.

### `crates/interface/Cargo.toml`

✅ Done in `afef448`: `pretty_assertions` dev-dep added.

### `crates/interface/src/tui/mod.rs` (~2138 lines today)

This file owns terminal startup, the panic hook, `run_event_loop`, and 18 guard tests.

**Imports / module list (top of file):**
- `mod custom_terminal;` ✅ added in `afef448`.
- `mod wrapping;` already present (replace contents in step 2).
- Add `mod insert_history;`.
- Add `mod tui_lifecycle;`.

**Terminal startup (currently lines 363-400ish):**

Replace:
```rust
enable_raw_mode()?;
stdout().execute(EnterAlternateScreen)?;
stdout().execute(EnableAlternateScroll)?;
stdout().execute(EnableBracketedPaste)?;
// ...
let backend = CrosstermBackend::new(stdout());
let mut terminal = Terminal::new(backend)?;
```

With:
```rust
tui_lifecycle::set_modes()?;
let backend = CrosstermBackend::new(stdout());
let custom_terminal = custom_terminal::Terminal::with_options(backend)?;
let mut tui = tui_lifecycle::Tui::new(custom_terminal)?;
```

Drop the old `EnableAlternateScroll`/`DisableAlternateScroll` definitions in this file (move to `tui_lifecycle`). Drop the `EnterAlternateScreen` import.

**Panic hook (currently lines 382-389):**

Replace `LeaveAlternateScreen` + `disable_raw_mode` calls with `tui_lifecycle::restore()`.

**`TerminalGuard` impl:** call `tui_lifecycle::restore_keep_raw()` in `Drop` (matches current behavior of just popping bracketed paste + alt scroll).

**`run_event_loop` signature (currently line 605):**

```rust
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App<'_>,
    // ...
)
```

Becomes:

```rust
async fn run_event_loop(
    tui: &mut tui_lifecycle::Tui,
    app: &mut App<'_>,
    // ...
)
```

**Draw call (currently line 618 + 1553/1582/1606/1611/1617):**

Replace every `terminal.draw(|frame| app.render(frame))?;` with:

```rust
let pending = std::mem::take(&mut app.pending_scrollback);
tui.insert_history_lines(pending);
let viewport_height = app.compute_viewport_height(tui.terminal_size()?);
tui.draw(viewport_height, |frame| app.render_viewport(frame))?;
```

Where `app.pending_scrollback: Vec<Line<'static>>` accumulates lines for cells that became stable since the last draw, and `app.compute_viewport_height(size)` returns rows needed for status + composer + footer + active in-flight cell + active small popup.

**Alt-screen call sites (currently lines 1017-1024):**

The existing code does `LeaveAlternateScreen` → `clear` → `EnterAlternateScreen` to bounce around an external editor / similar. Replace with:

```rust
tui.leave_alt_screen()?;
// run the external thing
tui.enter_alt_screen()?;
```

…no, actually — when borg launches an external editor today, it does `LeaveAlternateScreen` so the editor takes the screen, then `EnterAlternateScreen` after. Under inline viewport, the editor takes the screen below the viewport naturally. The `Leave/Enter` pair is no longer needed. Just save and restore raw mode around the external command.

Audit every `EnterAlternateScreen` / `LeaveAlternateScreen` call site (4 of each in mod.rs today) and convert.

**Guard tests (lines 1729-2137):** keep all forbidden-mode checks. Update text-based assertions that scan source for `EnterAlternateScreen` to instead check `tui_lifecycle::set_modes` body. Add new guard:

```rust
#[test]
fn render_rs_does_not_import_scrollbar() {
    let src = include_str!("app/render.rs");
    assert!(!src.contains("Scrollbar"),
        "Scrollbar widget removed in inline-viewport refactor — terminal owns scrolling now");
}
```

Add new guard:

```rust
#[test]
fn app_struct_has_no_scroll_offset() {
    let src = include_str!("app/mod.rs");
    let struct_section = src.split("pub struct App").nth(1).unwrap();
    let struct_body = struct_section.split('}').next().unwrap();
    assert!(!struct_body.contains("scroll_offset"),
        "scroll_offset field removed in inline-viewport refactor");
}
```

### `crates/interface/src/tui/app/mod.rs` (~3382 lines today)

**`App` struct (around line 420):**

Remove fields:
- `pub scroll_offset: usize`
- `pub total_lines: usize`
- `auto_scroll: bool`
- `transcript_area: Rect`

Add fields:
- `pub cells_emitted: usize` — index into `self.cells`; cells `< cells_emitted` are already in scrollback, never re-rendered in viewport.
- `pub pending_scrollback: Vec<Line<'static>>` — drained by `run_event_loop` between `App::render` and `tui.draw`.

**`App::new` (around line 525):**

Drop `scroll_offset: 0`, `total_lines: 0`, `auto_scroll: true`, `transcript_area: Rect::ZERO`. Add `cells_emitted: 0`, `pending_scrollback: Vec::new()`.

**Methods to delete:**
- `scroll_up_by`, `scroll_down_by`, `scroll_to_top`, `scroll_to_bottom`, `max_scroll`, `viewport_height` (recompute as needed inline).

**Methods to add:**
- `pub fn compute_viewport_height(&self, terminal_size: Size) -> u16` — computes viewport rows: `composer_height + status_height + footer_height + queue_preview_height + min(streaming_tail_lines, terminal_size.height/2)`. Cap at `terminal_size.height`.
- `pub fn flush_stable_cells(&mut self, width: u16)` — appends rendered lines for cells `[cells_emitted..stable_index]` to `pending_scrollback`, advances `cells_emitted`. See "Stability predicate" below.

**Stability predicate** — `fn stable_index(&self) -> usize`:

A cell at index `i` is stable iff:
1. `i < cells.len() - 1` (not the tail), AND
2. The cell is not a `Thinking` placeholder that the next thinking-delta might mutate (see `events.rs:155-200` — current logic inserts/replaces Thinking cells at `len-2` or `len-1`), AND
3. The cell is not an open `ToolStart` (a `ToolResult` will mutate `completed: true` later — `events.rs:230-256`), AND
4. The cell is not a `ToolStreaming` whose matching `ToolStart` is still un-completed.

Simplest correct rule: `stable_index = cells_emitted + N` where N is the largest `i ≤ cells.len() - 1` such that all cells `[cells_emitted..i]` are NOT `Thinking`, NOT open `ToolStart`, NOT `ToolStreaming` linked to an open `ToolStart`. **At `TurnComplete` (`events.rs:308-340`), force `stable_index = cells.len() - 1`** (only the tail might still be a streaming Assistant cell).

Implementation note: borg's events sometimes mutate cells deep in history (`events.rs:32-49` for doctor results; `events.rs:343-362` for error attribution). Audit each `cells.iter_mut().rev()` walk. If a mutation can hit a flushed cell, either move it earlier (before flush) or accept the bug (most are "set last error" which only walks back to the most recent unfinalized cell — verify).

### `crates/interface/src/tui/app/render.rs` (~1182 lines)

**`App::render` (line 51):** rename to `App::render_viewport`. Renders only:
- Branded header card (only on **first** frame — flush it to scrollback once via `pending_scrollback`, never again).
- Un-flushed cells `[cells_emitted..]` rendered as a `Paragraph` filling the upper portion of the viewport.
- Status bar.
- Composer.
- Active popup overlay (small popups only — see step 10 for large popups).
- Footer.
- Toasts.

**Delete:**
- `render_transcript` body that walks all cells (lines 116-176).
- `Scrollbar` widget render (lines 158-173).
- `render_autoscroll_hint` (lines 178-210) — irrelevant; auto-scroll is implicit.
- Imports: `Scrollbar`, `ScrollbarOrientation`, `ScrollbarState`.

**Add:** A new helper `flush_branded_header_once(&mut self)` that pushes the borg card + blank line into `pending_scrollback` if `cells_emitted == 0 && !self.banner_emitted`, then sets `self.banner_emitted = true`.

### `crates/interface/src/tui/app/key_handlers.rs` (~975 lines)

**Delete (lines 730-785):**
- `KeyCode::End` → `scroll_to_bottom` arm.
- `KeyCode::Home` → `scroll_to_top` arm.
- `KeyCode::PageUp` → `scroll_up_by` arm.
- `KeyCode::PageDown` → `scroll_down_by` arm.
- Rule A and Rule B `Up`/`Down` arms.

**Replace with:** nothing — fall through directly to `self.composer.handle_key(key)` for Up/Down/PageUp/PageDown/Home/End. The composer's existing history navigation handles Up/Down. PageUp/PageDown can be no-ops at the App level (composer ignores them; terminal owns scrolling).

**Delete tests (lines 1266-1640):**
- All scroll-related: `arrow_up_scrolls_transcript_when_scroll_offset_positive`, `arrow_down_scrolls_transcript_when_scroll_offset_positive`, `arrow_down_at_offset_one_restores_auto_scroll`, `wheel_down_simulation_restores_auto_scroll`, `wheel_up_from_bottom_*`, `pageup_pagedown_*`, `home_end_*`, `streaming_does_not_move_scroll`, `typing_does_not_move_scroll`, plus any using `scroll_offset` as input or assertion.

**Keep (and verify):** all composer-history tests. Add **one** new test: `up_arrow_with_empty_composer_recalls_history` — confirms the user-flagged regression check from this thread.

### `crates/interface/src/tui/app/events.rs` (~589 lines)

**`process_agent_event` (line 71):** at the top of each handler that pushes/mutates cells, after the mutation, call `app.flush_stable_cells(viewport_width)` to push newly-stable cells into scrollback. Width is the last-known viewport width; cache it on App during `render_viewport`.

**`handle_event_turn_complete` (line 308):** force `cells_emitted = self.cells.len() - 1` (or `cells.len()` if the tail is also stable — e.g. last cell is a `ToolResult`).

**Mutations that walk back into history:**
- `events.rs:32-49` (`process_doctor_event`) — replaces a placeholder cell. Verify the placeholder is always at `len-1`; if not, this can't run after the cell flushes. Fix: make placeholder lookup bounded to `>= cells_emitted`.
- `events.rs:230-256` (`handle_event_tool_result`) — walks back to find `ToolStart`. Same bound: `>= cells_emitted`. Otherwise the cell already shipped to scrollback and we'd be mutating gone state. Add an assertion or skip silently.
- `events.rs:319-340`, `events.rs:349-362` — same pattern; bound to `>= cells_emitted`.

### `crates/interface/src/tui/transcript_pager.rs` (~520 lines)

When opened (`AppState::TranscriptPager` set in mod.rs around line 1204): **call `tui.enter_alt_screen()`**. When closed: `tui.leave_alt_screen()`. The pager's render code is unchanged — it draws full-frame and the alt-screen viewport is full terminal size (set by `enter_alt_screen`).

Wire the alt-screen entry/leave through an `AppAction::EnterAltScreen` / `AppAction::LeaveAltScreen` variant or via a callback the App holds onto (mut ref to `Tui` is awkward across the App↔mod.rs boundary; an action enum is cleaner).

### Popups — alt-screen wrap audit

For each popup module, decide:

| Popup | Module | Verdict | Rationale |
|---|---|---|---|
| `command_popup` | `app/command_popup.rs` (slash menu) | Inline | Anchored to composer; small. |
| `file_popup` | `app/file_popup.rs` (`@` mention completion) | Inline | Anchored to composer; small. |
| `model_popup` | `app/model_popup.rs` | Inline | Few rows. |
| `file_search` | `app/file_search.rs` | Inline | Composer-adjacent. |
| `plan_overlay` | `app/plan_overlay.rs` | Inline | Renders in composer area. |
| `settings_popup` | `app/settings_popup.rs` | **Alt-screen** | Multi-section settings UI. |
| `pairing_popup` | `app/pairing_popup.rs` | **Alt-screen** | Full pairing flow. |
| `projects_popup` | `app/projects_popup.rs` | **Alt-screen** | Full project list. |
| `sessions_popup` | `app/sessions_popup.rs` | **Alt-screen** | Full session list. |
| `schedule_popup` | `app/schedule_popup.rs` | **Alt-screen** | Multi-row schedule UI. |
| `migrate_popup` | `app/migrate_popup.rs` | **Alt-screen** | Multi-step wizard. |
| `status_popup` | `app/status_popup.rs` | **Alt-screen** | Detailed vitals. |
| `btw_popup` | `app/btw_popup.rs` | **Alt-screen** | Background-task results, often multi-line. |
| `plugins_popup` | `app/plugins_popup.rs` | **Alt-screen** | Plugin browser. |

For each "Alt-screen" popup: when `popup.show()` is called, emit `AppAction::EnterAltScreen`. When closed, emit `AppAction::LeaveAltScreen`. Match in `run_event_loop`.

### `CLAUDE.md`

In the **Mouse Interaction — Hard Invariant** section:

Keep verbatim:
- The FORBIDDEN list (`?1000h`, `?1002h`, `?1003h`, `?1006h`, `EnableMouseCapture` etc.).
- The hard invariant (click+drag selection works).
- The list of guard tests.

Replace the **Strategy** paragraph with:

> **Strategy:** inline viewport via `custom_terminal::Terminal::with_options` plus `tui_lifecycle::Tui::insert_history_lines`. The terminal's normal scrollback owns prior history; trackpad/wheel scrolls the terminal pane natively. xterm Alternate Scroll Mode (`?1007h`) is enabled on entry to alt-screen surfaces (transcript pager, large popups) so wheel events translate to arrow keys there.

Replace the **Behavior** paragraph with:

> **Behavior:** Click+drag selects text natively. Trackpad/wheel scrolls the terminal's scrollback. Up/Down always navigate composer history (no scroll-offset disambiguation). Ctrl+T opens transcript pager in alt-screen for full-text search. PageUp/PageDown/Home/End are no-ops at the app level — terminal handles them.

### `docs/getting-started.md`

Audit for any mention of "scrollbar" or "scroll up/down with arrows" in the TUI section. Update to "scroll the terminal natively (trackpad/wheel/scrollbar of your terminal)".

---

## Verification (run after every meaningful commit)

```sh
cargo build -p interface
cargo fmt --check
cargo clippy -p interface -- -D warnings
cargo test --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log
grep -E "FAILED|test result: FAILED" /tmp/borg-test.log
```

**Manual TUI verification (must all pass before declaring done):**

1. `cargo run -p interface` → run 5+ turns to fill more than one screen of history.
2. **Trackpad scroll up** → terminal scrolls to reveal earlier turns. ✅ This is the user's reported bug.
3. **Trackpad scroll down** → returns to current viewport position.
4. **Click+drag text in scrolled-up content** → native selection works without modifier keys. ✅ Hard invariant.
5. **Up arrow with empty composer** → recalls previous user message into composer. ✅ User-flagged regression check.
6. **Down arrow** → moves forward in composer history.
7. **Type text** → arrows continue to work in composer (left/right cursor, up/down history).
8. **Ctrl+T** → transcript pager opens in alt-screen. Search with `/`. Esc closes; viewport returns inline.
9. **`/settings`** → opens in alt-screen. Esc closes; inline viewport restored, prior history still visible in scrollback.
10. **`/projects`, `/sessions`, `/plan`, etc.** → each large popup correctly enters alt-screen.
11. **Ctrl+C / `quit`** → terminal is clean; cursor at bottom; prior session output remains in terminal scrollback. (Side benefit of inline mode.)
12. **Resize the terminal mid-conversation** → viewport reflows; history above stays in scrollback.
13. **Cross-terminal smoke**: iTerm2 (primary), Terminal.app, Ghostty, tmux, Zellij. Reference impl has Zellij special-casing — confirm `is_zellij` detection fires correctly.

---

## Code review pass (after step 12)

Run `Agent` with `subagent_type: superpowers:code-reviewer` once all 12 steps land. Brief:

> Review the inline-viewport TUI port across `crates/interface/src/tui/`. Validate: (1) every removed `scroll_offset` usage was actually replaced, no orphans; (2) the stability predicate in `App::flush_stable_cells` matches the mutation patterns in `events.rs` (no flushed cells get mutated); (3) every popup that should enter alt-screen does; (4) guard tests cover the forbidden modes; (5) no Codex references in comments or commit messages; (6) `clippy -D warnings` clean. Report findings with file:line citations.

Fix every finding before merging.

---

## Risk register

| Risk | Mitigation |
|---|---|
| Stability predicate misses a mutation site → flushed cell gets mutated → duplicate / desynced lines in scrollback | Audit every `self.cells.iter_mut()` and `self.cells[idx] =` in events.rs against the predicate. Add debug assertion: `assert!(idx >= self.cells_emitted, "tried to mutate already-flushed cell {idx}")`. |
| Streaming text re-renders flicker in viewport | Cache the most recent rendered tail in `App`; only redraw on `dirty` flag. Borg already does this implicitly via ratatui's diff buffer. |
| `tmux` doesn't honor DECSTBM scroll regions correctly | `is_zellij` detection generalizes — add `is_tmux = std::env::var("TMUX").is_ok()` and use the Zellij newline-fallback path for tmux too. |
| Large popups need a viewport area larger than current terminal height | `enter_alt_screen` saves the inline viewport, sets viewport to full terminal size. On exit, restore. Already in the reference's `enter_alt_screen` — port verbatim. |
| External editor (`crates/interface/src/tui/external_editor.rs`) breaks under inline mode | The external command runs in raw mode, replacing the screen. After it returns, call `tui.terminal.clear()` to force a full repaint. |
| Mouse-text-selection regresses (the hard invariant) | Existing guard tests stay. Add a new one that scans `tui_lifecycle.rs` for the same forbidden modes. |
| Resume-from-suspend (Ctrl+Z / `fg`) broken | Reference handles this in `job_control.rs` (Unix only). Borg can defer — add a known-issue note; not a blocker. |

---

## Estimated diff size

| Step | LOC added | LOC removed |
|---|---:|---:|
| 2. wrapping.rs port | +1407 | -47 |
| 3. insert_history.rs port | +843 | 0 |
| 4. tui_lifecycle.rs | +400 | 0 |
| 5–6. App struct + render | +200 | -250 |
| 7. events.rs mutation guards | +50 | 0 |
| 8. mod.rs run_event_loop rewire | +80 | -120 |
| 9. key_handlers.rs scroll removal | 0 | -200 |
| 10. transcript_pager + popups | +150 | 0 |
| 11. tests | +50 | -400 |
| 12. docs | +20 | -10 |
| **Total** | **~3200** | **~1027** |

Net: ~+2200 LOC, mostly imported reference code that already has tests in the reference codebase.

---

## Continuation prompt for next session

> Pick up from `docs/inline-viewport-port.md`. Last commit on this branch is `afef448`. Continue at "Step 2: wrapping.rs enrichment". Read the spec, port files in the order listed, commit after each numbered step (e.g. `feat: port URL-aware adaptive line wrapping`, `feat: port insert_history scrollback flush`, etc.). Verify `cargo build && cargo test --lib --bins` after each commit. After step 12, dispatch the code-reviewer agent per the "Code review pass" section and fix every finding before declaring done.
