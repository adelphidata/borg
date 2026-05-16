use super::*;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn make_app() -> App<'static> {
    let config = Config::default();
    let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
    App::new(config, None, None, None, client, None)
}

// ========================================================================
// build_doctor_warning_lines — pure rendering helper for the startup
// amber warning card. Drives the formatter directly without touching the
// daemon or SQLite.
// ========================================================================

fn warns(s: &[&str]) -> Vec<String> {
    s.iter().map(|w| (*w).to_string()).collect()
}

fn flatten(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn doctor_card_empty_for_no_warnings() {
    assert!(super::super::history::build_doctor_warning_lines(&[], 80).is_empty());
}

#[test]
fn doctor_card_singular_for_one_category() {
    let lines =
        super::super::history::build_doctor_warning_lines(&warns(&["Memory:Index staleness"]), 80);
    let text = flatten(&lines);
    assert!(text.contains("⚠ 1 Warning "), "label: {text}");
    assert!(text.contains("Issue with Memory."), "summary: {text}");
    assert!(text.contains("/doctor"), "footer cta: {text}");
}

#[test]
fn doctor_card_two_categories_uses_and() {
    let lines =
        super::super::history::build_doctor_warning_lines(&warns(&["Memory:a", "Gateway:b"]), 80);
    let text = flatten(&lines);
    assert!(
        text.contains("Issues with Memory and Gateway."),
        "got: {text}"
    );
}

#[test]
fn doctor_card_three_categories_uses_oxford_and() {
    let lines = super::super::history::build_doctor_warning_lines(
        &warns(&["Memory:a", "Gateway:b", "Heartbeat:c"]),
        80,
    );
    let text = flatten(&lines);
    assert!(
        text.contains("Issues with Memory, Gateway, and Heartbeat."),
        "got: {text}"
    );
}

#[test]
fn doctor_card_overflow_collapses_extra_categories() {
    // 8 warnings spanning 5 distinct categories — first 3 listed, the
    // remaining 2 collapse into "and 2 more". Label still shows the raw
    // warning total (8), not the deduped category count.
    let lines = super::super::history::build_doctor_warning_lines(
        &warns(&[
            "Embeddings:provider",
            "Embeddings:cache",
            "Gateway:dir",
            "Heartbeat:age",
            "Memory:idx",
            "Memory:cap",
            "Sandbox:profile",
            "Sandbox:bin",
        ]),
        80,
    );
    let text = flatten(&lines);
    assert!(text.contains("⚠ 8 Warnings "), "label: {text}");
    assert!(
        text.contains("Issues with Embeddings, Gateway, Heartbeat, and 2 more."),
        "summary: {text}"
    );
}

#[test]
fn doctor_card_dedupes_repeated_categories() {
    // Three warnings, all under one category — should render as a single
    // "Issue with Memory" (singular) since only one category remains.
    let lines = super::super::history::build_doctor_warning_lines(
        &warns(&["Memory:a", "Memory:b", "Memory:c"]),
        80,
    );
    let text = flatten(&lines);
    assert!(text.contains("⚠ 3 Warnings "), "label: {text}");
    assert!(text.contains("Issue with Memory."), "summary: {text}");
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// ========================================================================
// /btw routing — popup visibility does not disturb main agent state, late
// results against a dismissed/superseded popup are discarded.
// ========================================================================

#[test]
fn cmd_btw_empty_pushes_usage_does_not_open_popup() {
    // Regression: /btw with no question must not fire the background task,
    // otherwise it would spin up an LLM call (and burn tokens) on typos.
    let mut app = make_app();
    let action = app.handle_submit("/btw").unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(!app.btw_popup.is_visible());
}

#[test]
fn cmd_btw_with_question_opens_loading_and_emits_start_action() {
    let mut app = make_app();
    let action = app.handle_submit("/btw what is X?").unwrap();
    match action {
        AppAction::StartBtw { question, .. } => assert_eq!(question, "what is X?"),
        _ => panic!("expected StartBtw action"),
    }
    assert!(app.btw_popup.is_visible());
}

#[test]
fn cmd_btw_during_streaming_does_not_touch_app_state() {
    // Hard requirement: /btw never cancels or otherwise alters the main
    // turn. If this regresses, typing /btw mid-stream kills the user's
    // in-progress answer.
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    let _ = app.handle_submit("/btw quick thing").unwrap();
    assert!(matches!(app.state, AppState::Streaming { .. }));
    assert!(app.btw_popup.is_visible());
}

#[test]
fn btw_result_for_superseded_question_is_discarded() {
    // A second /btw while one is in flight supersedes the first. The
    // old result must not overwrite the new Loading state.
    let mut app = make_app();
    app.btw_popup.show_loading("new question".to_string());
    app.process_btw_result(BtwResult {
        question: "old question".to_string(),
        outcome: Ok("stale answer".to_string()),
    });
    match app.btw_popup.state() {
        super::super::btw_popup::BtwState::Loading { question } => {
            assert_eq!(question, "new question");
        }
        other => panic!("expected Loading to persist, got {other:?}"),
    }
}

#[test]
fn btw_result_for_dismissed_popup_does_not_resurrect_it() {
    // User hit Esc — don't pop the window back up when the background
    // task finally finishes.
    let mut app = make_app();
    app.btw_popup.show_loading("q".to_string());
    app.btw_popup.dismiss();
    app.process_btw_result(BtwResult {
        question: "q".to_string(),
        outcome: Ok("late answer".to_string()),
    });
    assert!(!app.btw_popup.is_visible());
}

#[test]
fn btw_result_ok_transitions_popup_to_ready() {
    let mut app = make_app();
    app.btw_popup.show_loading("q".to_string());
    app.process_btw_result(BtwResult {
        question: "q".to_string(),
        outcome: Ok("the answer".to_string()),
    });
    match app.btw_popup.state() {
        super::super::btw_popup::BtwState::Ready { answer, .. } => {
            assert_eq!(answer, "the answer");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn btw_result_err_transitions_popup_to_error() {
    let mut app = make_app();
    app.btw_popup.show_loading("q".to_string());
    app.process_btw_result(BtwResult {
        question: "q".to_string(),
        outcome: Err("provider down".to_string()),
    });
    match app.btw_popup.state() {
        super::super::btw_popup::BtwState::Error { error, .. } => {
            assert_eq!(error, "provider down");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ========================================================================
// Transcript pager (Ctrl+T) — opening, routing, dismissal.
// ========================================================================

#[test]
fn ctrl_t_opens_transcript_pager_from_idle() {
    let mut app = make_app();
    assert!(matches!(app.state, AppState::Idle));
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(matches!(app.state, AppState::TranscriptPager));
    assert!(app.transcript_pager.visible);
}

#[test]
fn keys_route_to_pager_while_open_and_skip_composer() {
    let mut app = make_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    // While the pager is open, typing 'q' must dismiss the pager rather
    // than appearing in the composer (which would happen if dispatch leaked).
    app.handle_key(key(KeyCode::Char('q'))).unwrap();
    assert!(matches!(app.state, AppState::Idle));
    assert!(!app.transcript_pager.visible);
    assert!(app.composer.is_empty(), "composer must not receive 'q'");
}

#[test]
fn esc_in_pager_dismisses_back_to_idle() {
    let mut app = make_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(matches!(app.state, AppState::Idle));
    assert!(!app.transcript_pager.visible);
}

// ========================================================================
// Alt-screen wrap (Track D) — emit EnterAltScreen / LeaveAltScreen and
// surface the desired alt-screen state to the event loop.
//
// Regression guard: if any of these break, large popups will paint into
// the inline viewport and clip / overwrite the composer.
// ========================================================================

#[test]
fn ctrl_t_emits_enter_alt_screen() {
    let mut app = make_app();
    let action = app
        .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(
        matches!(action, AppAction::EnterAltScreen),
        "Ctrl+T must emit EnterAltScreen so run_event_loop can call tui.enter_alt_screen()"
    );
    assert!(app.wants_alt_screen());
}

#[test]
fn dismissing_pager_emits_leave_alt_screen() {
    let mut app = make_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    let action = app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(
        matches!(action, AppAction::LeaveAltScreen),
        "Dismissing the pager must emit LeaveAltScreen so the inline viewport restores"
    );
    assert!(!app.wants_alt_screen());
}

#[test]
fn settings_popup_show_drives_wants_alt_screen() {
    let mut app = make_app();
    assert!(!app.wants_alt_screen());
    let client = app.client.clone();
    app.settings_popup.show(&app.config, &client);
    assert!(
        app.wants_alt_screen(),
        "settings popup must request alt-screen surface"
    );
    app.settings_popup.dismiss();
    assert!(!app.wants_alt_screen());
}

#[test]
fn small_popups_do_not_request_alt_screen() {
    // command_popup / model_popup / file_popup / plan_overlay / file_search
    // stay inline. Opening the model popup must not flip wants_alt_screen.
    let mut app = make_app();
    let client = app.client.clone();
    app.model_popup.show(&app.config, &client);
    assert!(app.model_popup.is_visible());
    assert!(
        !app.wants_alt_screen(),
        "model_popup is inline; alt-screen must not be requested"
    );
}

#[test]
fn ctrl_t_in_pager_dismisses_back_to_idle() {
    let mut app = make_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(matches!(app.state, AppState::Idle));
}

#[test]
fn slash_in_pager_enters_search_mode_not_composer() {
    let mut app = make_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    app.handle_key(key(KeyCode::Char('/'))).unwrap();
    assert!(app.transcript_pager.is_searching());
    assert!(app.composer.is_empty(), "composer must not receive '/'");
}

/// Inline-viewport mode: Up arrow with an empty composer falls through
/// to composer history navigation (no app-side scroll model intercepts
/// it). Pre-port this would have been hijacked by Rule B for wheel
/// scrolling — that branch is gone now that the terminal owns scrolling.
#[test]
fn up_arrow_with_empty_composer_recalls_history() {
    let mut app = make_app();
    app.composer.set_text("first");
    app.composer.handle_key(key(KeyCode::Enter));
    app.composer.set_text("second");
    app.composer.handle_key(key(KeyCode::Enter));
    assert!(
        app.composer.is_empty(),
        "composer must be empty after submit"
    );

    app.handle_key(key(KeyCode::Up)).unwrap();

    assert_eq!(
        app.composer.text(),
        "second",
        "Up with empty composer must recall most recent history entry"
    );
    assert!(app.composer.is_browsing_history());
}

// --- Reverse-incremental history search (Ctrl+R) routing ---

fn ctrl(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

#[test]
fn ctrl_r_enters_history_search_mode() {
    let mut app = make_app();
    app.composer.set_text("prev");
    app.composer.handle_key(key(KeyCode::Enter));

    app.handle_key(ctrl('r')).unwrap();
    assert!(app.composer.is_searching());
    // Empty query yields preview of newest entry.
    assert_eq!(app.composer.text(), "prev");
}

#[test]
fn ctrl_p_escape_hatch_preserved_after_cancel() {
    // Regression guard: after canceling search, Ctrl+P must still
    // navigate composer history (existing invariant).
    let mut app = make_app();
    app.composer.set_text("recalled");
    app.composer.handle_key(key(KeyCode::Enter));

    app.handle_key(ctrl('r')).unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(!app.composer.is_searching());

    app.handle_key(ctrl('p')).unwrap();
    assert_eq!(app.composer.text(), "recalled");
    assert!(app.composer.is_browsing_history());
}

#[test]
fn search_enter_accepts_then_next_enter_submits() {
    let mut app = make_app();
    app.composer.set_text("prev cmd");
    app.composer.handle_key(key(KeyCode::Enter));

    app.handle_key(ctrl('r')).unwrap();
    app.handle_key(key(KeyCode::Char('p'))).unwrap();
    assert_eq!(app.composer.text(), "prev cmd");
    // First Enter accepts and exits search, keeps text in composer.
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert!(!app.composer.is_searching());
    assert_eq!(app.composer.text(), "prev cmd");
}

// --- Context-usage % (surfaced in footer) ---
//
// Regression guards for the "ctx 100% but autocompaction never ran" bug.
// The footer percent must reflect the *current* context window
// (last_prompt_tokens) — not cumulative session token counts. If anyone
// reintroduces `+=` accumulation in handle_event_usage these tests fail.

fn usage(prompt_tokens: u64) -> cortex::llm::UsageData {
    cortex::llm::UsageData {
        prompt_tokens,
        ..Default::default()
    }
}

#[test]
fn ctx_pct_zero_by_default() {
    let app = make_app();
    assert_eq!(app.compute_context_pct(), 0);
}

#[test]
fn ctx_pct_reflects_current_context_not_cumulative_session_tokens() {
    // Bug being guarded: `compute_context_pct` summed
    // session_prompt_tokens + session_completion_tokens, both `+=` per
    // turn, so ten turns at 25% each saturated the gauge to 100% even
    // though the live context was still ~25%.
    let mut app = make_app();
    let max = app.config.conversation.max_history_tokens.max(1) as u64;
    let quarter = max / 4;
    for _ in 0..10 {
        app.process_agent_event(AgentEvent::Usage(usage(quarter)));
    }
    // Cumulative would be 250%-clamped-to-100. Correct is ~25%.
    let pct = app.compute_context_pct();
    assert!(
        (24..=26).contains(&pct),
        "expected ~25%, got {pct}% (cumulative regression)"
    );
}

#[test]
fn ctx_pct_drops_after_history_compacted_event() {
    let mut app = make_app();
    let max = app.config.conversation.max_history_tokens.max(1) as u64;
    // Drive the gauge up to 90%.
    app.process_agent_event(AgentEvent::Usage(usage((max * 90) / 100)));
    assert!(app.compute_context_pct() >= 89);
    // Compaction event should snap the gauge to the post-compaction size.
    app.process_agent_event(AgentEvent::HistoryCompacted {
        dropped: 5,
        before_tokens: ((max * 90) / 100) as usize,
        after_tokens: (max / 4) as usize,
        iterative: false,
    });
    let pct = app.compute_context_pct();
    assert!(
        (24..=26).contains(&pct),
        "expected ctx % to drop to ~25% after compaction, got {pct}%"
    );
}

#[test]
fn ctx_pct_clamps_to_100_on_overflow() {
    let mut app = make_app();
    let max = app.config.conversation.max_history_tokens.max(1) as u64;
    app.last_prompt_tokens = max * 5;
    assert_eq!(app.compute_context_pct(), 100);
}

fn qm(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.to_string(),
        images: Vec::new(),
    }
}

// --- Mention expansion wiring (regression guard) ---
//
// If submit stops running `cortex::mentions::expand_mentions`, the TUI
// silently falls back to firing `read_file`/`list_dir` tool calls (slow,
// one extra turn per mention). These tests assert:
//   1. the outgoing `AppAction::SendMessage.input` contains the
//      `<attached_file>` block for an `@path` in the user text,
//   2. a `HistoryCell::MentionCard` is inserted for each resolved
//      mention so the user sees the `Read foo (N lines)` row.
// Both conditions must hold together; either alone is a regression.

#[test]
fn handle_submit_expands_at_file_mention_into_outgoing_message() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("note.md");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let mut app = make_app();
    let input = format!("summarize @{} please", file.display());

    let action = app.handle_submit(&input).unwrap();

    match action {
        AppAction::SendMessage { input: sent, .. } => {
            assert!(
                sent.contains("<attached_file"),
                "outgoing input missing <attached_file>: {sent}"
            );
            assert!(
                sent.contains("alpha\nbeta\ngamma"),
                "outgoing input missing file content"
            );
        }
        _ => panic!("expected SendMessage"),
    }

    let has_card = app.cells.iter().any(|c| {
        matches!(c, HistoryCell::MentionCard { label } if label.starts_with("Read ") && label.contains("(3 lines)"))
    });
    let card_labels: Vec<String> = app
        .cells
        .iter()
        .filter_map(|c| match c {
            HistoryCell::MentionCard { label } => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert!(
        has_card,
        "expected a `Read … (3 lines)` MentionCard in transcript; got cards: {card_labels:?}"
    );
}

#[test]
fn handle_submit_without_mentions_emits_no_cards() {
    // Guard against the expander accidentally injecting a card or
    // block for plain text (e.g. email addresses, words with `@`).
    let mut app = make_app();
    let action = app.handle_submit("ping foo@bar.com how are you").unwrap();
    match action {
        AppAction::SendMessage { input, .. } => {
            assert!(
                !input.contains("<attached_file"),
                "email must not trigger an attachment: {input}"
            );
        }
        _ => panic!("expected SendMessage"),
    }
    assert!(app
        .cells
        .iter()
        .all(|c| !matches!(c, HistoryCell::MentionCard { .. })));
}

#[test]
fn handle_queued_submit_also_expands_mentions() {
    // Queued sends go through a different function; must also expand.
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("q.md");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let mut app = make_app();
    let text = format!("@{} ?", file.display());
    let action = app.handle_queued_submit(qm(&text)).unwrap();
    match action {
        AppAction::SendMessage { input, .. } => {
            assert!(
                input.contains("<attached_file"),
                "queued submit must also expand mentions: {input}"
            );
        }
        _ => panic!("expected SendMessage"),
    }
    assert!(app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::MentionCard { .. })));
}

#[test]
fn drain_pushes_user_cell_to_history() {
    // Queued messages are not echoed to scrollback at queue time —
    // the user cell is pushed when the queued message actually drains.
    let mut app = make_app();
    let before = app.cells.len();
    app.handle_queued_submit(qm("hello queued")).unwrap();
    let user_cell = app
        .cells
        .iter()
        .find_map(|c| match c {
            HistoryCell::User { text } if text == "hello queued" => Some(text.clone()),
            _ => None,
        })
        .expect("drain must push the queued message as a User cell");
    assert_eq!(user_cell, "hello queued");
    // User cell + Assistant streaming cell, at minimum.
    assert!(app.cells.len() > before);
}

#[test]
fn queueing_during_streaming_does_not_push_history_cell() {
    // While streaming, pressing Enter on text should add to the queue
    // only — no echo cells in scrollback. Bug regression: dual-render
    // path produced only a thin highlight sliver in the transcript.
    use crossterm::event::{KeyEvent, KeyModifiers};
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    // No steer_tx → fallback queue path.
    app.steer_tx = None;
    app.composer.set_text("queued while busy");
    let cells_before = app.cells.len();

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();

    assert_eq!(
        app.cells.len(),
        cells_before,
        "queueing must not push any cells to scrollback"
    );
    assert_eq!(app.queued_messages.len(), 1);
    assert_eq!(app.queued_messages[0].text, "queued while busy");
    assert!(app.composer.text().is_empty());
}

#[test]
fn test_tab_queues_multiple() {
    let mut app = make_app();
    app.queued_messages.push_back(qm("first"));
    app.queued_messages.push_back(qm("second"));
    app.queued_messages.push_back(qm("third"));

    assert_eq!(app.queued_messages.len(), 3);
    assert_eq!(app.pop_next_queued().unwrap().text, "first");
    assert_eq!(app.pop_next_queued().unwrap().text, "second");
    assert_eq!(app.pop_next_queued().unwrap().text, "third");
    assert!(app.pop_next_queued().is_none());
}

#[test]
fn test_esc_clears_queue() {
    let mut app = make_app();
    app.queued_messages.push_back(qm("a"));
    app.queued_messages.push_back(qm("b"));

    app.queued_messages.clear();

    assert!(app.queued_messages.is_empty());
}

#[test]
fn test_alt_up_pops_last() {
    let mut app = make_app();
    app.queued_messages.push_back(qm("a"));
    app.queued_messages.push_back(qm("b"));
    app.queued_messages.push_back(qm("c"));

    let last = app.queued_messages.pop_back().unwrap();
    assert_eq!(last.text, "c");
    assert_eq!(app.queued_messages.len(), 2);
    assert_eq!(app.queued_messages[0].text, "a");
    assert_eq!(app.queued_messages[1].text, "b");
}

#[test]
fn test_drain_pops_front() {
    let mut app = make_app();
    app.queued_messages.push_back(qm("first"));
    app.queued_messages.push_back(qm("second"));

    let popped = app.pop_next_queued().unwrap();
    assert_eq!(popped.text, "first");
    assert_eq!(app.queued_messages.len(), 1);
    assert_eq!(app.queued_messages[0].text, "second");
}

#[test]
fn test_queue_preview_height() {
    let mut app = make_app();
    let w = 80u16;

    // Empty queue
    assert_eq!(app.compute_queue_preview_height(w), 0);

    // 1 short message: header(1) + body(1) + hint(1) = 3
    app.queued_messages.push_back(qm("a"));
    assert_eq!(app.compute_queue_preview_height(w), 3);

    // 3 short messages: header(1) + body(3) + hint(1) = 5
    app.queued_messages.push_back(qm("b"));
    app.queued_messages.push_back(qm("c"));
    assert_eq!(app.compute_queue_preview_height(w), 5);

    // 5 messages: only first 3 shown (no overflow line), height unchanged
    app.queued_messages.push_back(qm("d"));
    app.queued_messages.push_back(qm("e"));
    assert_eq!(app.compute_queue_preview_height(w), 5);
}

#[test]
fn test_queue_preview_height_wraps_long_message() {
    let mut app = make_app();
    // inner width = 20 - 4 = 16; message of 50 chars wraps to ~4 lines,
    // capped at 3 + 1 overflow line.
    let long = "a".repeat(50);
    app.queued_messages.push_back(qm(&long));
    let h = app.compute_queue_preview_height(20);
    // header(1) + 3 wrapped + 1 overflow + hint(1) = 6
    assert_eq!(h, 6);
}

// --- Interrupt preserves queued messages ---

#[test]
fn interrupt_restores_queued_to_composer() {
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    app.cancel_token = Some(CancellationToken::new());
    app.queued_messages.push_back(qm("msg1"));
    app.queued_messages.push_back(qm("msg2"));
    app.queued_messages.push_back(qm("msg3"));

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(app.queued_messages.is_empty());
    assert_eq!(app.composer.text(), "msg1\nmsg2\nmsg3");
    let sys = last_system_text(&app).unwrap();
    assert!(sys.contains("restored to composer"));
}

#[test]
fn interrupt_empty_queue_shows_interrupted() {
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    app.cancel_token = Some(CancellationToken::new());

    app.handle_key(key(KeyCode::Esc)).unwrap();

    let sys = last_system_text(&app).unwrap();
    assert_eq!(sys, "[interrupted]");
}

// --- /cancel slash command ---

#[test]
fn cmd_cancel_with_in_flight_token_cancels_and_clears() {
    let mut app = make_app();
    let token = CancellationToken::new();
    app.cancel_token = Some(token.clone());

    let action = app.cmd_cancel().unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(token.is_cancelled());
    assert!(app.cancel_token.is_none());
    let sys = last_system_text(&app).unwrap();
    assert_eq!(sys, "[cancelled]");
}

#[test]
fn cmd_cancel_without_in_flight_token_reports_nothing() {
    let mut app = make_app();
    app.cancel_token = None;

    let action = app.cmd_cancel().unwrap();
    assert!(matches!(action, AppAction::Continue));
    let sys = last_system_text(&app).unwrap();
    assert_eq!(sys, "Nothing to cancel.");
}

#[test]
fn cancel_typed_mid_stream_interrupts_instead_of_steering() {
    // /cancel typed into the composer during streaming must not be sent
    // as a mid-turn steer — it should behave the same as pressing Esc.
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    let token = CancellationToken::new();
    app.cancel_token = Some(token.clone());
    app.composer.set_text("/cancel");

    app.handle_key(key(KeyCode::Enter)).unwrap();

    assert!(token.is_cancelled());
    assert!(app.cancel_token.is_none());
    assert!(matches!(app.state, AppState::Idle));
    let sys = last_system_text(&app).unwrap();
    assert_eq!(sys, "[cancelled]");
}

#[test]
fn interrupt_with_existing_composer_text() {
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    app.cancel_token = Some(CancellationToken::new());
    app.composer.set_text("draft");
    app.queued_messages.push_back(qm("queued"));

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert_eq!(app.composer.text(), "draft\nqueued");
}

#[test]
fn esc_during_streaming_cancels_even_with_event_rx_backlog() {
    // Regression: a hot LLM stream keeps `event_rx` perpetually ready.
    // The fix lives in `run_event_loop`'s biased select! (terminal-input
    // arm now polled before the agent-event arm) — this test pins the
    // App-level contract the fix relies on: handle_key(Esc) cancels the
    // turn synchronously without depending on `event_rx` being drained.
    // If a future change makes Esc handling consume from event_rx or
    // otherwise gate on it, this test will fail.
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    let token = CancellationToken::new();
    app.cancel_token = Some(token.clone());

    // Backlog of agent events sitting in the channel — simulating a
    // mid-stream burst of TextDelta tokens that biased select would
    // otherwise prefer.
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    for i in 0..100 {
        tx.try_send(AgentEvent::TextDelta(format!("tok{i}")))
            .expect("backlog send");
    }
    app.event_rx = Some(rx);

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(token.is_cancelled(), "Esc must cancel the turn token");
    assert!(app.cancel_token.is_none(), "cancel_token must be taken");
    assert!(
        matches!(app.state, AppState::Idle),
        "Esc must drop streaming state to Idle"
    );
    assert!(app.event_rx.is_none(), "Esc must drop event_rx");
    assert_eq!(last_system_text(&app).unwrap(), "[interrupted]");

    // Dropping event_rx without draining must close the channel — i.e.
    // Esc handling did not pull events off the queue. If a future change
    // routes Esc through the agent-event path, the receiver would still
    // be alive and the sender would not see the close.
    assert!(
        tx.is_closed(),
        "event_rx must be dropped, closing the channel"
    );
}

// --- Channel close robustness ---

#[test]
fn channel_close_with_queue_pauses() {
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };
    app.queued_messages.push_back(qm("pending"));

    app.handle_agent_channel_closed();

    assert!(app.last_turn_errored);
    assert_eq!(app.queued_messages.len(), 1);
    let sys = last_system_text(&app).unwrap();
    assert!(sys.contains("queue paused"));
}

#[test]
fn channel_close_empty_queue_no_error() {
    let mut app = make_app();
    app.state = AppState::Streaming {
        start: Instant::now(),
    };

    app.handle_agent_channel_closed();

    assert!(!app.last_turn_errored);
}

// --- Backtrack mode ---

#[test]
fn backtrack_enters_on_esc_empty_composer() {
    let mut app = make_app();
    app.cells.push(HistoryCell::User {
        text: "hello".to_string(),
    });
    app.cells
        .push(HistoryCell::assistant("hi".to_string(), false));
    app.cells.push(HistoryCell::User {
        text: "world".to_string(),
    });

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(matches!(
        app.backtrack,
        BacktrackPhase::Selecting { cursor: 0, .. }
    ));
}

#[test]
fn backtrack_no_user_messages_noop() {
    let mut app = make_app();
    app.cells.push(HistoryCell::System {
        text: "welcome".to_string(),
    });

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
}

#[test]
fn backtrack_esc_cancels() {
    let mut app = make_app();
    app.cells.push(HistoryCell::User {
        text: "hello".to_string(),
    });

    app.handle_key(key(KeyCode::Esc)).unwrap(); // enter backtrack
    assert!(matches!(app.backtrack, BacktrackPhase::Selecting { .. }));

    app.handle_key(key(KeyCode::Esc)).unwrap(); // cancel
    assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
}

#[test]
fn backtrack_navigate_and_select() {
    let mut app = make_app();
    app.cells.push(HistoryCell::User {
        text: "first".to_string(),
    });
    app.cells
        .push(HistoryCell::assistant("resp1".to_string(), false));
    app.cells.push(HistoryCell::User {
        text: "second".to_string(),
    });
    app.cells
        .push(HistoryCell::assistant("resp2".to_string(), false));

    // Enter backtrack (cursor starts at 0 = most recent)
    app.handle_key(key(KeyCode::Esc)).unwrap();

    // Navigate up to select first (older) message
    app.handle_key(key(KeyCode::Up)).unwrap();

    // Select it
    let action = app.handle_key(key(KeyCode::Enter)).unwrap();

    assert!(matches!(
        action,
        AppAction::RewindTo {
            nth_user_message: 0
        }
    ));
    assert_eq!(app.composer.text(), "first");
    // Cells should be truncated to before the first user message
    assert_eq!(app.cells.len(), 0);
    assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
}

#[test]
fn backtrack_not_triggered_with_text_in_composer() {
    let mut app = make_app();
    app.cells.push(HistoryCell::User {
        text: "hello".to_string(),
    });
    app.composer.set_text("draft");

    // Esc with text in composer should NOT enter backtrack — it goes to composer's Esc handler
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
}

// --- /tools command ---

fn last_system_text(app: &App) -> Option<String> {
    app.cells.iter().rev().find_map(|c| match c {
        HistoryCell::System { text } => Some(text.clone()),
        _ => None,
    })
}

#[test]
fn preparing_event_adds_thinking_cell() {
    let mut app = make_app();
    app.process_agent_event(AgentEvent::Preparing);
    assert!(matches!(
        app.cells.last(),
        Some(HistoryCell::Thinking { text }) if text.is_empty()
    ));
}

#[test]
fn text_delta_removes_preparing_placeholder() {
    let mut app = make_app();
    app.process_agent_event(AgentEvent::Preparing);
    app.process_agent_event(AgentEvent::TextDelta("Hello".into()));
    // The empty Thinking cell should be gone, replaced with Assistant
    assert!(!matches!(
        app.cells.first(),
        Some(HistoryCell::Thinking { text }) if text.is_empty()
    ));
    assert!(matches!(
        app.cells.last(),
        Some(HistoryCell::Assistant { text, .. }) if text == "Hello"
    ));
}

#[test]
fn text_delta_preserves_nonempty_thinking() {
    let mut app = make_app();
    app.cells.push(HistoryCell::Thinking {
        text: "some thought".into(),
    });
    app.process_agent_event(AgentEvent::TextDelta("Hello".into()));
    // Thinking cell should still be there (non-empty, not a placeholder)
    assert!(matches!(
        &app.cells[0],
        HistoryCell::Thinking { text } if text == "some thought"
    ));
    assert!(matches!(
        app.cells.last(),
        Some(HistoryCell::Assistant { text, .. }) if text == "Hello"
    ));
}

#[test]
fn preparing_then_thinking_delta_appends() {
    let mut app = make_app();
    app.process_agent_event(AgentEvent::Preparing);
    app.process_agent_event(AgentEvent::ThinkingDelta("reasoning...".into()));
    assert_eq!(app.cells.len(), 1);
    assert!(matches!(
        app.cells.last(),
        Some(HistoryCell::Thinking { text }) if text == "reasoning..."
    ));
}

// --- Slash command handling ---

#[test]
fn unknown_slash_command_rejected() {
    let mut app = make_app();
    let result = app.handle_submit("/foobar").unwrap();
    assert!(matches!(result, AppAction::Continue));
    let has_unknown_msg = app.cells.iter().any(|c| {
        matches!(
            c, HistoryCell::System { text } if text.contains("Unknown command")
        )
    });
    assert!(has_unknown_msg, "should show unknown command message");
}

#[test]
fn unknown_slash_command_not_sent_to_agent() {
    let mut app = make_app();
    app.handle_submit("/notacommand").unwrap();
    // Should NOT create a User cell (which would mean it was sent to the agent)
    let has_user_cell = app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::User { .. }));
    assert!(
        !has_user_cell,
        "unknown slash command should not be sent to agent"
    );
}

#[test]
fn help_command_works() {
    let mut app = make_app();
    let result = app.handle_submit("/help").unwrap();
    assert!(matches!(result, AppAction::Continue));
    let has_help = app.cells.iter().any(|c| {
        matches!(
            c, HistoryCell::System { text } if text.contains("/help")
        )
    });
    assert!(has_help, "should show help text");
}

#[test]
fn mode_prefix_command_not_rejected() {
    let mut app = make_app();
    let result = app.handle_submit("/mode execute").unwrap();
    assert!(matches!(result, AppAction::Continue));
    // Should show mode change message, not "Unknown command"
    let has_unknown = app.cells.iter().any(|c| {
        matches!(
            c, HistoryCell::System { text } if text.contains("Unknown command")
        )
    });
    assert!(
        !has_unknown,
        "/mode execute should not be treated as unknown"
    );
}

#[test]
fn settings_prefix_not_rejected() {
    let mut app = make_app();
    let _ = app.handle_submit("/settings temperature 0.5").unwrap();
    // Should either update or error, not "Unknown command"
    let has_unknown = app.cells.iter().any(|c| {
        matches!(
            c, HistoryCell::System { text } if text.contains("Unknown command")
        )
    });
    assert!(
        !has_unknown,
        "/settings key value should not be treated as unknown"
    );
}

#[test]
fn plan_prefix_not_rejected() {
    let mut app = make_app();
    let _result = app.handle_submit("/plan hello").unwrap();
    let has_unknown = app.cells.iter().any(|c| {
        matches!(
            c, HistoryCell::System { text } if text.contains("Unknown command")
        )
    });
    assert!(!has_unknown, "/plan <msg> should not be treated as unknown");
}

#[test]
fn plan_toggle_sets_collaboration_mode_to_plan() {
    let mut app = make_app();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
    let _ = app.handle_submit("/plan").unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Plan,
        "/plan should switch collaboration mode to Plan"
    );
    assert_eq!(
        app.previous_collab_mode,
        Some(CollaborationMode::Execute),
        "/plan should stash the previous mode for restore"
    );
}

#[test]
fn plan_toggle_twice_restores_previous_mode() {
    let mut app = make_app();
    app.config.conversation.collaboration_mode = CollaborationMode::Execute;
    let _ = app.handle_submit("/plan").unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Plan
    );
    let _ = app.handle_submit("/plan").unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Execute,
        "toggling /plan a second time should restore the prior mode"
    );
    assert_eq!(app.previous_collab_mode, None);
}

#[test]
fn mode_plan_stashes_previous_mode() {
    let mut app = make_app();
    app.config.conversation.collaboration_mode = CollaborationMode::Execute;
    let _ = app.handle_submit("/mode plan").unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Plan
    );
    assert_eq!(app.previous_collab_mode, Some(CollaborationMode::Execute));
}

#[test]
fn mode_switch_away_from_plan_clears_previous() {
    let mut app = make_app();
    let _ = app.handle_submit("/plan").unwrap();
    assert_eq!(app.previous_collab_mode, Some(CollaborationMode::Execute));
    let _ = app.handle_submit("/mode execute").unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
    assert_eq!(
        app.previous_collab_mode, None,
        "leaving Plan via /mode should clear the stashed prior mode"
    );
}

#[test]
fn shift_tab_cycles_modes_and_manages_previous_collab_mode() {
    // Shift+Tab (BackTab) is the only keyboard path through
    // `cycle_collaboration_mode`. Cycle is Execute ↔ Plan only — there is
    // no Default mode. Invariant: stash the prior mode when *entering*
    // Plan so the post-turn review overlay can restore it; clear when
    // leaving Plan.
    let mut app = make_app();
    let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

    // Execute → Plan. Entering Plan stashes Execute.
    app.handle_key(back_tab).unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Plan
    );
    assert_eq!(
        app.previous_collab_mode,
        Some(CollaborationMode::Execute),
        "entering Plan must stash the prior mode for restore"
    );

    // Plan → Execute. Leaving Plan must clear the stash.
    app.handle_key(back_tab).unwrap();
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
    assert_eq!(
        app.previous_collab_mode, None,
        "leaving Plan via cycle must clear the stashed mode"
    );
}

#[test]
fn turn_complete_in_plan_mode_opens_review_overlay() {
    let mut app = make_app();
    app.config.conversation.collaboration_mode = CollaborationMode::Plan;
    app.process_agent_event(AgentEvent::TurnComplete);
    assert!(
        matches!(app.state, AppState::PlanReview),
        "TurnComplete while in Plan mode should enter PlanReview"
    );
    assert_eq!(
        app.config.conversation.collaboration_mode,
        CollaborationMode::Plan,
        "mode should stay Plan until the user picks Proceed on the overlay"
    );
}

#[test]
fn turn_complete_in_default_mode_returns_to_idle() {
    let mut app = make_app();
    app.process_agent_event(AgentEvent::TurnComplete);
    assert!(matches!(app.state, AppState::Idle));
}

#[test]
fn turn_complete_moves_final_assistant_tail_into_pending_scrollback() {
    let mut app = make_app();
    app.last_viewport_width = 80;
    app.cells
        .push(HistoryCell::assistant("done".to_string(), true));

    app.process_agent_event(AgentEvent::TurnComplete);

    assert_eq!(app.cells_emitted, app.cells.len());
    let pending_text = app
        .pending_scrollback
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        pending_text.contains("done"),
        "finalized assistant tail must move into pending scrollback, got: {pending_text:?}"
    );
}

// --- Update command ---

fn assert_self_update(input: &str, want_pre: bool, want_check: bool) {
    let mut app = make_app();
    let result = app.handle_submit(input).unwrap();
    match result {
        AppAction::SelfUpdate { prerelease, check } => {
            assert_eq!(prerelease, want_pre, "input {input:?}: prerelease");
            assert_eq!(check, want_check, "input {input:?}: check");
        }
        _ => panic!("input {input:?}: expected SelfUpdate"),
    }
}

#[test]
fn update_command_full_flag_matrix() {
    let cases: &[(&str, bool, bool)] = &[
        ("/update", false, false),
        ("/update --prerelease", true, false),
        ("/update --pre", true, false),
        ("/update --dev", true, false),
        ("/update -p", true, false),
        ("/update --check", false, true),
        ("/update -c", false, true),
        ("/update -pc", true, true),
        ("/update --prerelease --check", true, true),
        ("/update --check --dev", true, true),
    ];
    for (input, pre, check) in cases {
        assert_self_update(input, *pre, *check);
    }
}

#[test]
fn update_command_unknown_flag_does_not_invoke_update() {
    let mut app = make_app();
    let result = app.handle_submit("/update --bogus").unwrap();
    assert!(
        matches!(result, AppAction::Continue),
        "unknown flag must not trigger SelfUpdate"
    );
    assert!(
        app.cells.iter().any(|c| matches!(
            c,
            HistoryCell::System { text } if text.starts_with("/update:")
        )),
        "expected a /update error message in transcript"
    );
}

#[test]
fn update_command_does_not_swallow_similar_prefix() {
    // `/updateXYZ` (no space) must NOT be treated as `/update`. Otherwise the
    // prefix handler would silently eat unrelated user input.
    let mut app = make_app();
    let result = app.handle_submit("/updateNOPE").unwrap();
    assert!(
        !matches!(result, AppAction::SelfUpdate { .. }),
        "/updateNOPE must not match /update"
    );
}

// --- Status popup integration ---

#[test]
fn status_popup_opens_on_slash_status() {
    let mut app = make_app();
    assert!(!app.status_popup.is_visible());
    let _result = app.handle_submit("/status").unwrap();
    assert!(app.status_popup.is_visible());
}

#[test]
fn status_popup_esc_closes() {
    let mut app = make_app();
    let _ = app.handle_submit("/status").unwrap();
    assert!(app.status_popup.is_visible());
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(!app.status_popup.is_visible());
}

#[test]
fn status_popup_consumes_keys() {
    let mut app = make_app();
    let _ = app.handle_submit("/status").unwrap();
    assert!(app.status_popup.is_visible());
    // A random key should be consumed by the popup, not the composer.
    let initial_text = app.composer.text().to_string();
    app.handle_key(key(KeyCode::Char('a'))).unwrap();
    assert_eq!(app.composer.text(), initial_text);
    assert!(app.status_popup.is_visible());
}

// --- /uninstall ---

#[test]
fn uninstall_shows_warning_and_enters_confirming_state() {
    let mut app = make_app();
    let action = app.handle_submit("/uninstall").unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(matches!(app.state, AppState::ConfirmingUninstall));
    assert!(app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("WARNING"))));
}

#[test]
fn uninstall_confirm_y_returns_uninstall_action() {
    let mut app = make_app();
    app.state = AppState::ConfirmingUninstall;
    let action = app
        .handle_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('y'),
        ))
        .unwrap();
    assert!(matches!(action, AppAction::Uninstall));
}

#[test]
fn uninstall_confirm_n_returns_to_idle() {
    let mut app = make_app();
    app.state = AppState::ConfirmingUninstall;
    let action = app
        .handle_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('n'),
        ))
        .unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(matches!(app.state, AppState::Idle));
    assert!(app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("cancelled"))));
}

#[test]
fn uninstall_enter_defaults_to_cancel() {
    let mut app = make_app();
    app.state = AppState::ConfirmingUninstall;
    let action = app
        .handle_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ))
        .unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(matches!(app.state, AppState::Idle));
}

#[test]
fn uninstall_release_event_is_ignored() {
    // REPORT_EVENT_TYPES delivers both Press and Release. The Enter that
    // submits `/uninstall` must not re-fire as "cancel" via its Release
    // event inside ConfirmingUninstall. Regression guard for the
    // auto-cancel bug.
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    let mut app = make_app();
    app.state = AppState::ConfirmingUninstall;
    let release = KeyEvent::new_with_kind_and_state(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Release,
        KeyEventState::NONE,
    );
    let action = app.handle_key(release).unwrap();
    assert!(matches!(action, AppAction::Continue));
    assert!(matches!(app.state, AppState::ConfirmingUninstall));
    assert!(!app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("cancelled"))));
}

#[test]
fn uninstall_not_in_autocomplete() {
    let popup = crate::tui::command_popup::CommandPopup::new();
    let all = popup.filtered();
    assert!(!all.iter().any(|c| c.name == "/uninstall"));
}

#[test]
fn uninstall_partial_not_in_autocomplete() {
    let mut popup = crate::tui::command_popup::CommandPopup::new();
    popup.update_filter("/unins");
    let matches = popup.filtered();
    assert!(matches.is_empty());
}

// --- /poke command ---

#[test]
fn poke_with_channel_sends_and_continues() {
    let config = Config::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
    let mut app = App::new(config, None, None, Some(tx), client, None);
    let result = app.try_handle_command("/poke");
    assert!(result.is_some());
    let action = result.unwrap().unwrap();
    assert!(matches!(action, AppAction::Continue));
    let sys = last_system_text(&app).unwrap();
    assert!(sys.contains("heartbeat triggered"));
}

#[test]
fn poke_without_channel_returns_poke_action() {
    let mut app = make_app(); // poke_tx is None
    let result = app.try_handle_command("/poke");
    assert!(result.is_some());
    let action = result.unwrap().unwrap();
    assert!(matches!(action, AppAction::Poke));
    // No pre-send chatter — the confirmation is emitted by the
    // AppAction::Poke handler in tui::mod.rs after the request fires,
    // not by the command dispatcher.
}

#[test]
fn poke_full_channel_shows_pending_message() {
    let config = Config::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    // Fill the channel
    tx.try_send(()).unwrap();
    let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
    let mut app = App::new(config, None, None, Some(tx), client, None);
    let result = app.try_handle_command("/poke");
    assert!(result.is_some());
    let action = result.unwrap().unwrap();
    assert!(matches!(action, AppAction::Continue));
    let sys = last_system_text(&app).unwrap();
    assert!(sys.contains("already pending"));
}

// --- Popup vs Streaming: paste/key must never leak to composer ---

#[test]
fn any_popup_visible_reflects_plugins_popup() {
    let mut app = make_app();
    assert!(!app.any_popup_visible());
    let data_dir = std::env::temp_dir().join("borg-test-any-popup");
    app.plugins_popup.show(&app.config, &data_dir);
    assert!(app.any_popup_visible());
}

/// Mirror of the per-popup `wants_text_input` tests at the App level:
/// no popup open, no popup wants text input.
#[test]
fn any_popup_wants_text_input_false_when_idle() {
    let app = make_app();
    assert!(!app.any_popup_wants_text_input());
}

/// Critical gating test: when a popup is in a text-input mode (cron `*/5`,
/// API key, path setting, etc.) the global `/` interceptor MUST NOT fire.
/// Otherwise `/` mid-input dismisses the popup and silently dumps a single
/// `/` into the composer — the original bug this fix could have introduced.
#[test]
fn slash_does_not_dismiss_popup_in_text_input_mode() {
    let mut app = make_app();
    let client = app.client.clone();
    app.settings_popup.show(&app.config, &client);
    app.settings_popup
        .force_editing_for_test("cron */5".to_string());
    assert!(app.any_popup_wants_text_input());

    app.handle_key(key(KeyCode::Char('/'))).unwrap();

    assert!(
        app.settings_popup.is_visible(),
        "popup must stay open while editing — `/` is a valid value char"
    );
    assert!(
        app.composer.text().is_empty(),
        "composer must NOT receive `/` — it belongs in the value buffer"
    );
}

/// Regression: typing `/` while another popup is open must dismiss that
/// popup and reach the composer, so slash commands like `/stats` still
/// work without first hitting Esc. Previously the popup's modal capture
/// silently ate every keystroke.
#[test]
fn slash_dismisses_open_popup_and_falls_through_to_composer() {
    let mut app = make_app();
    let client = app.client.clone();
    app.settings_popup.show(&app.config, &client);
    assert!(app.settings_popup.is_visible());

    app.handle_key(key(KeyCode::Char('/'))).unwrap();

    assert!(
        !app.settings_popup.is_visible(),
        "settings popup should be dismissed by global slash"
    );
    assert_eq!(
        app.composer.text(),
        "/",
        "slash should reach composer, not be swallowed by the popup"
    );

    // Confirm the slash command itself still routes to the Status popup
    // (independent of typing mechanics — `try_handle_command` is the same
    // dispatch path the composer reaches after Enter).
    app.try_handle_command("/stats")
        .expect("/stats handled")
        .unwrap();
    assert!(
        app.status_popup.is_visible(),
        "/stats should open the status popup"
    );
}

#[test]
fn paste_goes_to_popup_not_composer_during_streaming() {
    let mut app = make_app();
    // Open plugins popup
    let data_dir = std::env::temp_dir().join("borg-test-paste-stream");
    app.plugins_popup.show(&app.config, &data_dir);
    // Force Streaming state (simulates drain_queued_if_idle starting a turn)
    app.state = AppState::Streaming {
        start: std::time::Instant::now(),
    };
    // Paste should be swallowed by the popup, not queued as a steer
    let action = app.handle_paste("secret-token".to_string());
    assert!(matches!(action, AppAction::Continue));
    // Composer must remain empty
    assert!(app.composer.text().is_empty());
    // pending_steers must remain empty (paste was NOT queued)
    assert!(app.pending_steers.is_empty());
}

#[test]
fn key_chars_go_to_popup_not_composer_during_streaming() {
    let mut app = make_app();
    let data_dir = std::env::temp_dir().join("borg-test-key-stream");
    app.plugins_popup.show(&app.config, &data_dir);

    // Select Telegram and enter credential input phase
    let tg_idx = app
        .plugins_popup
        .items_for_test()
        .iter()
        .position(|i| i.0 == "messaging/telegram")
        .unwrap();
    app.plugins_popup.set_cursor_for_test(tg_idx);
    // Force uninstalled so toggle + Enter enters CredentialInput
    app.plugins_popup.force_uninstalled_for_test(tg_idx);
    app.handle_key(key(KeyCode::Char(' '))).unwrap(); // toggle
    app.handle_key(key(KeyCode::Enter)).unwrap(); // enter cred input

    // Now force Streaming
    app.state = AppState::Streaming {
        start: std::time::Instant::now(),
    };

    // Type chars — they should go to the popup, not the composer
    app.handle_key(key(KeyCode::Char('a'))).unwrap();
    app.handle_key(key(KeyCode::Char('b'))).unwrap();
    app.handle_key(key(KeyCode::Char('c'))).unwrap();

    assert!(app.composer.text().is_empty());
}

#[test]
fn model_exact_opens_popup() {
    let mut app = make_app();
    assert!(!app.model_popup.is_visible());
    let _ = app.handle_submit("/model").unwrap();
    assert!(app.model_popup.is_visible(), "/model should open the popup");
    let has_unknown = app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Unknown command")));
    assert!(!has_unknown, "/model should not be treated as unknown");
}

#[test]
fn model_prefix_sets_provider_and_model() {
    let mut app = make_app();
    let _ = app
        .handle_submit("/model openrouter/anthropic/claude-sonnet-4")
        .unwrap();
    assert_eq!(app.config.llm.provider.as_deref(), Some("openrouter"));
    assert_eq!(app.config.llm.model, "anthropic/claude-sonnet-4");
    let has_unknown = app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Unknown command")));
    assert!(!has_unknown);
}

#[test]
fn model_prefix_rejects_malformed() {
    let mut app = make_app();
    let original_provider = app.config.llm.provider.clone();
    let original_model = app.config.llm.model.clone();
    let _ = app.handle_submit("/model no-slash-here").unwrap();
    assert_eq!(app.config.llm.provider, original_provider);
    assert_eq!(app.config.llm.model, original_model);
    let has_usage = app
        .cells
        .iter()
        .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Usage: /model")));
    assert!(has_usage, "malformed /model should show usage hint");
}

// ========================================================================
// request_user_input — multi-question carousel key handling.
// Guards the new batched contract: ↑/↓ move cursor, ←/→ navigate between
// questions preserving per-question state, Tab toggles Notes editor, Space
// toggles multi-select rows, Enter on the last question submits the whole
// batch as a HashMap<id, Answer>, Esc cancels with an interrupted marker
// per question.
// ========================================================================

use cortex::tool_handlers::user_input::{Answer, Question, QuestionOption, INTERRUPTED_MARKER};
use std::collections::HashMap;

fn make_question(id: &str, header: &str, labels: &[&str], multi_select: bool) -> Question {
    Question {
        id: id.to_string(),
        header: header.to_string(),
        question: format!("Pick from {header}"),
        multi_select,
        options: labels
            .iter()
            .map(|l| QuestionOption {
                label: (*l).to_string(),
                description: format!("desc for {l}"),
            })
            .collect(),
    }
}

fn seed_awaiting_with_questions(
    app: &mut App<'_>,
    questions: Vec<Question>,
) -> oneshot::Receiver<HashMap<String, Answer>> {
    let (tx, rx) = oneshot::channel::<HashMap<String, Answer>>();
    let per_question_state = (0..questions.len())
        .map(|_| QuestionState::default())
        .collect();
    app.state = AppState::AwaitingInput {
        questions,
        current_idx: 0,
        per_question_state,
        respond: Some(tx),
    };
    rx
}

#[test]
fn enter_on_last_question_submits_full_answer_map() {
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![
            make_question("color", "Color", &["red", "blue"], false),
            make_question("size", "Size", &["small", "large"], false),
        ],
    );

    // Q1: pick "blue" (move cursor down to index 1) then advance.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap(); // advances to Q2
                                                  // Q2: leave on default (cursor 0 = "small") then submit.
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let map = rx.try_recv().expect("response sent");
    assert_eq!(map.get("color").unwrap().answers, vec!["blue".to_string()]);
    assert_eq!(map.get("size").unwrap().answers, vec!["small".to_string()]);
    assert!(matches!(app.state, AppState::Streaming { .. }));
}

#[test]
fn arrow_keys_navigate_between_questions_preserving_per_question_state() {
    let mut app = make_app();
    let _rx = seed_awaiting_with_questions(
        &mut app,
        vec![
            make_question("a", "A", &["x", "y", "z"], false),
            make_question("b", "B", &["p", "q"], false),
        ],
    );

    // Move Q1 cursor to "z" (index 2).
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    // Right arrow: advance to Q2 — Q1 cursor must be retained.
    app.handle_key(key(KeyCode::Right)).unwrap();
    // Move Q2 cursor to index 1.
    app.handle_key(key(KeyCode::Down)).unwrap();
    // Left arrow: back to Q1 — Q1 cursor should still be 2 (not reset).
    app.handle_key(key(KeyCode::Left)).unwrap();

    match &app.state {
        AppState::AwaitingInput {
            current_idx,
            per_question_state,
            ..
        } => {
            assert_eq!(*current_idx, 0);
            assert_eq!(per_question_state[0].cursor, 2);
            assert_eq!(per_question_state[1].cursor, 1);
        }
        _ => panic!("expected AwaitingInput"),
    }
}

#[test]
fn tab_toggles_notes_field_on_current_question() {
    let mut app = make_app();
    let _rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question("only", "Only", &["a", "b"], false)],
    );

    // Tab opens notes; subsequent character keys land in notes, not options.
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Char('h'))).unwrap();
    app.handle_key(key(KeyCode::Char('i'))).unwrap();
    match &app.state {
        AppState::AwaitingInput {
            per_question_state, ..
        } => {
            assert!(per_question_state[0].notes_open);
            assert_eq!(per_question_state[0].notes, "hi");
        }
        _ => panic!("expected AwaitingInput"),
    }
    // Tab again closes notes.
    app.handle_key(key(KeyCode::Tab)).unwrap();
    match &app.state {
        AppState::AwaitingInput {
            per_question_state, ..
        } => assert!(!per_question_state[0].notes_open),
        _ => panic!("expected AwaitingInput"),
    }
}

#[test]
fn multi_select_question_accumulates_selections_with_space() {
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question(
            "channels",
            "Channels",
            &["telegram", "slack", "discord"],
            true,
        )],
    );

    // Cursor on "telegram" (idx 0): toggle on.
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    // Move to "discord" (idx 2): toggle on.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    // Submit.
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let map = rx.try_recv().expect("response sent");
    // Multi-select preserves option order, not toggle order.
    assert_eq!(
        map.get("channels").unwrap().answers,
        vec!["telegram".to_string(), "discord".to_string()]
    );
}

#[test]
fn esc_cancels_and_returns_interrupted_marker_per_question() {
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![
            make_question("a", "A", &["x", "y"], false),
            make_question("b", "B", &["p", "q"], false),
        ],
    );

    app.handle_key(key(KeyCode::Esc)).unwrap();

    let map = rx.try_recv().expect("response sent");
    assert_eq!(
        map.get("a").unwrap().answers,
        vec![INTERRUPTED_MARKER.to_string()]
    );
    assert_eq!(
        map.get("b").unwrap().answers,
        vec![INTERRUPTED_MARKER.to_string()]
    );
    assert!(matches!(app.state, AppState::Streaming { .. }));
}

#[test]
fn notes_appended_with_user_note_prefix_on_submit() {
    // Regression guard: free-form notes must round-trip into the answer
    // payload prefixed with `user_note: ` so the LLM can distinguish them
    // from option labels.
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question("color", "Color", &["red", "blue"], false)],
    );

    // Open notes, type "I love crimson", close, submit.
    app.handle_key(key(KeyCode::Tab)).unwrap();
    for c in "crimson".chars() {
        app.handle_key(key(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let map = rx.try_recv().expect("response sent");
    let entry = map.get("color").unwrap();
    assert_eq!(entry.answers[0], "red");
    assert_eq!(entry.answers[1], "user_note: crimson");
}

#[test]
fn esc_does_not_close_notes_it_cancels_the_batch() {
    // Regression guard: previously Esc only closed the notes editor when
    // it was open, leaving users with no way to cancel from inside notes.
    // Now Esc always cancels the batch, regardless of notes state.
    let mut app = make_app();
    let mut rx =
        seed_awaiting_with_questions(&mut app, vec![make_question("a", "A", &["x", "y"], false)]);

    // Open notes, type something, then Esc.
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Char('z'))).unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();

    let map = rx.try_recv().expect("response sent");
    assert_eq!(
        map.get("a").unwrap().answers,
        vec![INTERRUPTED_MARKER.to_string()]
    );
}

#[test]
fn ctrl_c_cancel_fills_interrupted_markers_per_question() {
    // Regression guard: Ctrl+C used to send an empty HashMap, which
    // serialize_answers would fill with empty answer lists — a different
    // shape from Esc cancellation. Now both produce identical [interrupted]
    // markers so the LLM sees one cancel shape.
    use crossterm::event::KeyModifiers;
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![
            make_question("a", "A", &["x", "y"], false),
            make_question("b", "B", &["p", "q"], false),
        ],
    );

    let evt = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_key(evt).unwrap();

    let map = rx.try_recv().expect("response sent");
    assert_eq!(
        map.get("a").unwrap().answers,
        vec![INTERRUPTED_MARKER.to_string()]
    );
    assert_eq!(
        map.get("b").unwrap().answers,
        vec![INTERRUPTED_MARKER.to_string()]
    );
}

#[test]
fn moving_cursor_to_other_auto_opens_notes_in_single_select() {
    // The bare "Other" label tells the LLM nothing without context. So when
    // the cursor lands on the Other row, the notes editor pops open
    // automatically, prompting the user to type their actual answer.
    let mut app = make_app();
    let _rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question("color", "Color", &["red", "blue"], false)],
    );
    // 2 options + 1 Other → two Downs lands on the Other row.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    match &app.state {
        AppState::AwaitingInput {
            per_question_state, ..
        } => {
            assert!(
                per_question_state[0].notes_open,
                "expected notes to auto-open on Other"
            );
        }
        _ => panic!("expected AwaitingInput"),
    }
}

#[test]
fn toggling_other_in_multi_select_auto_opens_notes() {
    let mut app = make_app();
    let _rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question(
            "channels",
            "Channels",
            &["telegram", "slack"],
            true,
        )],
    );
    // Move cursor to the Other row (idx 2), then toggle with Space.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    match &app.state {
        AppState::AwaitingInput {
            per_question_state, ..
        } => {
            assert!(
                per_question_state[0].other_selected,
                "Other should be toggled on"
            );
            assert!(
                per_question_state[0].notes_open,
                "toggling Other on should auto-open notes"
            );
        }
        _ => panic!("expected AwaitingInput"),
    }
}

#[test]
fn multi_select_with_other_and_notes_round_trips_in_option_order() {
    // Combined-path coverage: multi-select picks two options + Other +
    // free-form note. The serialized answer must contain options in their
    // declared order, then "Other", then the user_note line.
    let mut app = make_app();
    let mut rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question(
            "channels",
            "Channels",
            &["telegram", "slack", "discord"],
            true,
        )],
    );

    // Toggle telegram (idx 0).
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    // Move to slack (idx 1) and toggle.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    // Move past discord (idx 2) to Other (idx 3) and toggle.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    // Other auto-opened notes; type the note and Tab back out.
    for c in "via webhook".chars() {
        app.handle_key(key(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let map = rx.try_recv().expect("response sent");
    let answers = &map.get("channels").unwrap().answers;
    assert_eq!(
        answers,
        &vec![
            "telegram".to_string(),
            "slack".to_string(),
            "Other".to_string(),
            "user_note: via webhook".to_string(),
        ]
    );
}

#[test]
fn other_row_is_appended_below_options() {
    // The "Other" row is auto-added at index `options.len()` so total rows
    // is options.len() + 1. Cursor wrap-around must include it.
    let mut app = make_app();
    let _rx = seed_awaiting_with_questions(
        &mut app,
        vec![make_question("color", "Color", &["red", "blue"], false)],
    );
    // 2 options + 1 Other = 3 rows. Two Downs lands us on "Other".
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    match &app.state {
        AppState::AwaitingInput {
            per_question_state,
            questions,
            ..
        } => {
            assert_eq!(per_question_state[0].cursor, questions[0].options.len());
        }
        _ => panic!("expected AwaitingInput"),
    }
}
