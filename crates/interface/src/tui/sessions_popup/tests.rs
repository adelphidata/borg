use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn make_session(id: &str, title: &str) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        title: title.to_string(),
        updated_unix_ts: 1_735_689_600, // 2025-01-01T00:00:00Z
        message_count: 5,
    }
}

fn make_popup_with_sessions(count: usize) -> SessionsPopup {
    let mut popup = SessionsPopup::new();
    popup.visible = true;
    for i in 0..count {
        popup.sessions.push(make_session(
            &format!("session-{i}"),
            &format!("Session {i}"),
        ));
    }
    popup
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn esc_dismisses() {
    let mut popup = make_popup_with_sessions(2);
    popup.handle_key(key(KeyCode::Esc));
    assert!(!popup.is_visible());
}

/// Enter on an empty session list must NOT return a Load action — that
/// would attempt to load `session-<panic>`. Enter on a populated list
/// loads the highlighted entry (cursor=0 → "session-0").
#[test]
fn enter_on_empty_returns_none_and_on_populated_loads_cursor() {
    let mut empty = SessionsPopup::new();
    empty.visible = true;
    assert!(empty.handle_key(key(KeyCode::Enter)).is_none());

    let mut popup = make_popup_with_sessions(3);
    match popup.handle_key(key(KeyCode::Enter)) {
        Some(SessionAction::Load { id }) => assert_eq!(id, "session-0"),
        other => panic!("expected Load, got {other:?}"),
    }
}

/// Arrow keys wrap at both ends so the user can't strand the cursor
/// past the list. Down past the last entry wraps to 0; Up from 0 wraps
/// to the last entry.
#[test]
fn cursor_wraps_at_both_ends() {
    let mut popup = make_popup_with_sessions(2);
    assert_eq!(popup.cursor, 0);
    popup.handle_key(key(KeyCode::Down));
    assert_eq!(popup.cursor, 1);
    popup.handle_key(key(KeyCode::Down));
    assert_eq!(popup.cursor, 0, "Down past last must wrap to 0");

    let mut popup = make_popup_with_sessions(2);
    popup.handle_key(key(KeyCode::Up));
    assert_eq!(popup.cursor, 1, "Up from 0 must wrap to last");
}

#[test]
fn enter_after_navigation_loads_correct_session() {
    let mut popup = make_popup_with_sessions(3);
    popup.handle_key(key(KeyCode::Down)); // cursor -> 1
    let result = popup.handle_key(key(KeyCode::Enter));
    match result {
        Some(SessionAction::Load { id }) => assert_eq!(id, "session-1"),
        other => panic!("expected Load, got {other:?}"),
    }
}

#[test]
fn e_key_exports_highlighted_session_and_dismisses() {
    let mut popup = make_popup_with_sessions(3);
    popup.handle_key(key(KeyCode::Down)); // cursor -> 1
    let result = popup.handle_key(key(KeyCode::Char('e')));
    match result {
        Some(SessionAction::Export { id }) => assert_eq!(id, "session-1"),
        other => panic!("expected Export, got {other:?}"),
    }
    assert!(!popup.is_visible(), "pressing 'e' should dismiss the popup");
}

#[test]
fn e_key_on_empty_returns_none() {
    let mut popup = SessionsPopup::new();
    popup.visible = true;
    let result = popup.handle_key(key(KeyCode::Char('e')));
    assert!(result.is_none());
}

#[test]
fn hidden_popup_ignores_keys() {
    let mut popup = make_popup_with_sessions(2);
    popup.visible = false;
    let result = popup.handle_key(key(KeyCode::Enter));
    assert!(result.is_none());
}
