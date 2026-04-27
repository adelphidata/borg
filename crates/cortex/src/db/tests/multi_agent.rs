use super::*;
use crate::multi_agent::SubAgentStatus;

// ── Role CRUD Tests ──

#[test]
fn insert_and_get_role_round_trip() {
    let db = test_db();
    // Use a unique name to avoid conflict with seeded builtin roles
    db.insert_role(
        "custom-analyst",
        "Custom analyst role",
        Some("gpt-4"),
        Some("openai"),
        Some(0.5),
        Some("You are an analyst."),
        Some("read_file,run_shell"),
        Some(10),
        false,
    )
    .unwrap();

    let role = db.get_role("custom-analyst").unwrap().unwrap();
    assert_eq!(role.name, "custom-analyst");
    assert_eq!(role.description, "Custom analyst role");
    assert_eq!(role.model.as_deref(), Some("gpt-4"));
    assert_eq!(role.provider.as_deref(), Some("openai"));
    assert!((role.temperature.unwrap() - 0.5).abs() < f32::EPSILON);
    assert_eq!(
        role.system_instructions.as_deref(),
        Some("You are an analyst.")
    );
    assert_eq!(role.tools_allowed.as_deref(), Some("read_file,run_shell"));
    assert_eq!(role.max_iterations, Some(10));
    assert!(!role.is_builtin);
}

#[test]
fn get_role_returns_none_for_unknown() {
    let db = test_db();
    assert!(db.get_role("nonexistent").unwrap().is_none());
}

#[test]
fn list_roles_ordered_by_name() {
    let db = test_db();
    // 3 builtin roles (coder, researcher, writer) are seeded by migrations
    let baseline = db.list_roles().unwrap().len();

    db.insert_role(
        "zeta-custom",
        "Z",
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    db.insert_role(
        "alpha-custom",
        "A",
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();

    let roles = db.list_roles().unwrap();
    assert_eq!(roles.len(), baseline + 2);
    // Verify ordering: alpha-custom should come before zeta-custom
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    let alpha_pos = names.iter().position(|n| *n == "alpha-custom").unwrap();
    let zeta_pos = names.iter().position(|n| *n == "zeta-custom").unwrap();
    assert!(alpha_pos < zeta_pos);
}

#[test]
fn update_role_partial_coalesce() {
    let db = test_db();
    db.insert_role(
        "r1",
        "original",
        Some("gpt-4"),
        None,
        Some(0.7),
        None,
        None,
        None,
        false,
    )
    .unwrap();

    // Update only description and temperature, other fields should remain
    db.update_role(
        "r1",
        Some("updated desc"),
        None,
        None,
        Some(0.3),
        None,
        None,
        None,
    )
    .unwrap();

    let role = db.get_role("r1").unwrap().unwrap();
    assert_eq!(role.description, "updated desc");
    assert_eq!(role.model.as_deref(), Some("gpt-4")); // unchanged
    assert!((role.temperature.unwrap() - 0.3).abs() < f32::EPSILON);
}

#[test]
fn delete_role_returns_true_false() {
    let db = test_db();
    db.insert_role("r1", "test", None, None, None, None, None, None, false)
        .unwrap();
    assert!(db.delete_role("r1").unwrap());
    assert!(!db.delete_role("r1").unwrap()); // already deleted
    assert!(db.get_role("r1").unwrap().is_none());
}

// ── Sub-Agent Run Tests ──

#[test]
fn insert_and_get_sub_agent_run() {
    let db = test_db();
    db.insert_sub_agent_run("run1", "nick", "researcher", "parent-s1", "child-s1", 1)
        .unwrap();

    let run = db.get_sub_agent_run("run1").unwrap().unwrap();
    assert_eq!(run.id, "run1");
    assert_eq!(run.nickname, "nick");
    assert_eq!(run.role, "researcher");
    assert_eq!(run.parent_session_id, "parent-s1");
    assert_eq!(run.session_id, "child-s1");
    assert_eq!(run.depth, 1);
    assert_eq!(run.status, "pending_init");
    assert!(run.result_text.is_none());
    assert!(run.error_text.is_none());
    assert!(run.completed_at.is_none());
}

#[test]
fn get_sub_agent_run_returns_none_for_unknown() {
    let db = test_db();
    assert!(db.get_sub_agent_run("nonexistent").unwrap().is_none());
}

#[test]
fn list_sub_agent_runs_filters_by_parent() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "a", "role", "parent1", "s1", 1)
        .unwrap();
    db.insert_sub_agent_run("r2", "b", "role", "parent1", "s2", 1)
        .unwrap();
    db.insert_sub_agent_run("r3", "c", "role", "parent2", "s3", 1)
        .unwrap();

    let runs = db.list_sub_agent_runs("parent1").unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.parent_session_id == "parent1"));

    let runs2 = db.list_sub_agent_runs("parent2").unwrap();
    assert_eq!(runs2.len(), 1);
}

#[test]
fn update_sub_agent_status_sets_completed_at_on_terminal() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();

    // Non-terminal status should not set completed_at
    db.update_sub_agent_status("r1", &SubAgentStatus::Running)
        .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "running");
    assert!(run.completed_at.is_none());

    // Terminal status should set completed_at
    db.update_sub_agent_status(
        "r1",
        &SubAgentStatus::Completed {
            result: "result text".to_string(),
        },
    )
    .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "completed");
    assert!(run.completed_at.is_some());
    assert_eq!(run.result_text.as_deref(), Some("result text"));
}

#[test]
fn update_sub_agent_status_errored_sets_error_text() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();
    db.update_sub_agent_status(
        "r1",
        &SubAgentStatus::Errored {
            error: "something failed".to_string(),
        },
    )
    .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "errored");
    assert!(run.completed_at.is_some());
    assert_eq!(run.error_text.as_deref(), Some("something failed"));
}

#[test]
fn update_sub_agent_status_shutdown_is_terminal() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();
    db.update_sub_agent_status("r1", &SubAgentStatus::Shutdown)
        .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "shutdown");
    assert!(run.completed_at.is_some());
}

#[test]
fn list_sub_agent_runs_empty_for_unknown_parent() {
    let db = test_db();
    let runs = db.list_sub_agent_runs("no-such-parent").unwrap();
    assert!(runs.is_empty());
}

#[test]
fn update_role_preserves_none_fields() {
    let db = test_db();
    db.insert_role("r2", "desc", None, None, None, None, None, None, false)
        .unwrap();
    db.update_role("r2", Some("new desc"), None, None, None, None, None, None)
        .unwrap();
    let role = db.get_role("r2").unwrap().unwrap();
    assert_eq!(role.description, "new desc");
    assert!(role.model.is_none());
    assert!(role.provider.is_none());
    assert!(role.temperature.is_none());
    assert!(role.system_instructions.is_none());
    assert!(role.tools_allowed.is_none());
    assert!(role.max_iterations.is_none());
}
