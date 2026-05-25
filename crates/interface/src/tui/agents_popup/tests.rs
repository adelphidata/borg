use super::*;
use protocols::admin::AgentMetrics as PbAgentMetrics;

fn entry(id: &str, parent_session: &str, session: &str, depth: u32, status: &str) -> AgentEntry {
    AgentEntry {
        id: id.to_string(),
        nickname: format!("nick-{id}"),
        role: "test".to_string(),
        model: "test-model".to_string(),
        parent_session_id: parent_session.to_string(),
        session_id: session.to_string(),
        depth,
        status: status.to_string(),
        created_at: 1000,
        ended_at: if is_terminal(status) { 2000 } else { 0 },
        error: String::new(),
        result: String::new(),
        metrics: Some(PbAgentMetrics::default()),
    }
}

fn snap(agents: Vec<AgentEntry>) -> AgentList {
    AgentList {
        agents,
        history: Vec::new(),
    }
}

/// Filter=Running must include `running` AND `paused` AND `pending_init`
/// (all "live" states), and EXCLUDE every terminal status. Both
/// inclusion and exclusion are asserted — a regression that only
/// dropped Paused would be missed by a positive-only test.
#[test]
fn filter_running_includes_live_excludes_terminal() {
    let mut popup = AgentsPopup::new();
    popup.set_snapshot_for_test(snap(vec![
        entry("a", "root", "sess-a", 0, "running"),
        entry("b", "root", "sess-b", 0, "paused"),
        entry("c", "root", "sess-c", 0, "completed"),
        entry("d", "root", "sess-d", 0, "errored"),
        entry("e", "root", "sess-e", 0, "shutdown"),
        entry("f", "root", "sess-f", 0, "pending_init"),
    ]));
    popup.filter = AgentFilter::Running;
    popup.recompute_visible();

    let ids: Vec<&str> = popup
        .visible_indices
        .iter()
        .map(|&i| popup.snapshot.agents[i].id.as_str())
        .collect();
    assert!(ids.contains(&"a"), "running must be included");
    assert!(
        ids.contains(&"b"),
        "paused must be included in Running filter"
    );
    assert!(ids.contains(&"f"), "pending_init must be included");
    assert!(!ids.contains(&"c"), "completed must be excluded");
    assert!(!ids.contains(&"d"), "errored must be excluded");
    assert!(!ids.contains(&"e"), "shutdown must be excluded");
}

/// `x` on a Running agent returns Kill(id); `x` on a Completed agent
/// returns None — the popup must not fire a guaranteed-no-op RPC
/// (the daemon would return FailedPrecondition; better to never call).
#[test]
fn x_kills_only_non_terminal_agents() {
    let mut popup = AgentsPopup::new();
    popup.set_snapshot_for_test(snap(vec![
        entry("live", "root", "sess-live", 0, "running"),
        entry("done", "root", "sess-done", 0, "completed"),
    ]));

    // Cursor at 0 — running agent.
    assert_eq!(popup.cursor_agent_id_for_test(), Some("live"));
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        matches!(action, AgentsAction::Kill { ref id } if id == "live"),
        "x on Running must emit Kill, got {action:?}"
    );

    // Move cursor to terminal agent.
    popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(popup.cursor_agent_id_for_test(), Some("done"));
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        action,
        AgentsAction::None,
        "x on Completed must be a no-op (terminal agents aren't killable)"
    );
}

/// Shift+X emits KillSubtree, distinct from plain x. Both keys must
/// produce the right action at the input layer where it's cheap to
/// verify.
#[test]
fn shift_x_emits_kill_subtree() {
    let mut popup = AgentsPopup::new();
    popup.set_snapshot_for_test(snap(vec![entry(
        "root",
        "outer",
        "sess-root",
        0,
        "running",
    )]));
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('X'),
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert!(
        matches!(action, AgentsAction::KillSubtree { ref id } if id == "root"),
        "Shift+X must emit KillSubtree, got {action:?}"
    );
}

/// `p` flips Pause/Resume based on current status. The pair-vs-single
/// distinction is the popup's responsibility — daemon validates again
/// but the UI shouldn't surface Resume on a Running agent or vice versa.
#[test]
fn p_toggles_pause_and_resume_by_status() {
    let mut popup = AgentsPopup::new();
    popup.set_snapshot_for_test(snap(vec![
        entry("r", "root", "sess-r", 0, "running"),
        entry("p", "root", "sess-p", 0, "paused"),
        entry("d", "root", "sess-d", 0, "completed"),
    ]));

    // Running → Pause.
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('p'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(action, AgentsAction::Pause { ref id } if id == "r"));

    // Move to paused → Resume.
    popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    ));
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('p'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(action, AgentsAction::Resume { ref id } if id == "p"));

    // Move to terminal → None.
    popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    ));
    let action = popup.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('p'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(action, AgentsAction::None, "p on terminal must be no-op");
}

/// Collapsing a parent hides every descendant (not just immediate
/// children). Verifies the parent-chain walk in `recompute_visible`.
/// Negative side: D is a sibling subtree and must remain visible.
#[test]
fn collapse_hides_full_subtree_not_just_immediate_children() {
    let mut popup = AgentsPopup::new();
    // Tree:  A → B → C ;  X (sibling root)
    popup.set_snapshot_for_test(snap(vec![
        entry("A", "outer", "sess-A", 0, "running"),
        entry("B", "sess-A", "sess-B", 1, "running"),
        entry("C", "sess-B", "sess-C", 2, "running"),
        entry("X", "outer", "sess-X", 0, "running"),
    ]));
    // Collapse A.
    popup.expanded.insert("sess-A".to_string());
    popup.recompute_visible();

    let ids: Vec<&str> = popup
        .visible_indices
        .iter()
        .map(|&i| popup.snapshot.agents[i].id.as_str())
        .collect();
    assert!(ids.contains(&"A"), "collapsed parent itself is still shown");
    assert!(
        !ids.contains(&"B"),
        "B (immediate child of A) must be hidden"
    );
    assert!(!ids.contains(&"C"), "C (grandchild of A) must be hidden");
    assert!(
        ids.contains(&"X"),
        "X (sibling subtree) must remain visible"
    );
}

/// Every status variant must map to a unique glyph. Catches "added a
/// new status, forgot the glyph" via a HashSet uniqueness check.
#[test]
fn status_glyph_is_unique_per_known_variant() {
    use std::collections::HashSet;
    let variants = [
        "pending_init",
        "running",
        "paused",
        "completed",
        "errored",
        "shutdown",
    ];
    let glyphs: HashSet<&str> = variants.iter().map(|v| status_glyph(v)).collect();
    assert_eq!(
        glyphs.len(),
        variants.len(),
        "each status must have a unique glyph"
    );
}
