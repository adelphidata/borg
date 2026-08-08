use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn make_session(id: &str, title: &str) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        title: title.to_string(),
        updated_unix_ts: 1_735_689_600, // 2025-01-01T00:00:00Z
        message_count: 5,
        pinned: false,
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

/// The `*` pin marker is the only TUI signal distinguishing a pinned session;
/// it must render for pinned rows and stay absent (aligned padding) for
/// unpinned ones.
#[test]
fn row_label_marks_pinned_sessions() {
    let mut pinned = make_session("s1", "Pinned one");
    pinned.pinned = true;
    let unpinned = make_session("s2", "Plain one");

    let pinned_label = SessionsPopup::row_label(&pinned);
    let unpinned_label = SessionsPopup::row_label(&unpinned);

    assert!(
        pinned_label.starts_with(" * Pinned one"),
        "pinned rows must lead with the * marker: {pinned_label:?}"
    );
    assert!(
        unpinned_label.starts_with("   Plain one"),
        "unpinned rows keep the column alignment with a blank marker: {unpinned_label:?}"
    );
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
        Some(SessionAction::Export { id, format }) => {
            assert_eq!(id, "session-1");
            assert_eq!(
                format,
                cortex::export::ExportFormat::Json,
                "default export format must be JSON"
            );
        }
        other => panic!("expected Export, got {other:?}"),
    }
    assert!(!popup.is_visible(), "pressing 'e' should dismiss the popup");
}

/// `f` cycles through every offered format and wraps back to JSON; the
/// selected format is carried into the Export action.
#[test]
fn f_key_cycles_export_formats_and_export_uses_selection() {
    use cortex::export::ExportFormat;
    let mut popup = make_popup_with_sessions(1);
    // json -> csv -> txt -> md
    for expected in [ExportFormat::Csv, ExportFormat::Txt, ExportFormat::Markdown] {
        assert!(popup.handle_key(key(KeyCode::Char('f'))).is_none());
        assert_eq!(popup.selected_format(), expected);
    }
    // Export in the currently selected (Markdown) format.
    match popup.handle_key(key(KeyCode::Char('e'))) {
        Some(SessionAction::Export { format, .. }) => {
            assert_eq!(format, ExportFormat::Markdown);
        }
        other => panic!("expected Export, got {other:?}"),
    }
    // Wrap-around: one more 'f' from md returns to json.
    let mut popup = make_popup_with_sessions(1);
    for _ in 0..EXPORT_FORMATS.len() {
        popup.handle_key(key(KeyCode::Char('f')));
    }
    assert_eq!(popup.selected_format(), ExportFormat::Json);
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
