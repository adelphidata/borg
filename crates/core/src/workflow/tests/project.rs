use super::*;

// ============================================================
// Project DB Tests
// ============================================================

#[test]
fn test_create_project() {
    let db = test_db();
    db.create_project("proj-1", "My Project", "A test project")
        .unwrap();

    let proj = db.get_project("proj-1").unwrap().unwrap();
    assert_eq!(proj.name, "My Project");
    assert_eq!(proj.description, "A test project");
    assert_eq!(proj.status, "active");
}

#[test]
fn test_get_project_not_found() {
    let db = test_db();
    assert!(db.get_project("nonexistent").unwrap().is_none());
}

#[test]
fn test_list_projects_with_filter() {
    let db = test_db();
    db.create_project("p1", "Active Project", "").unwrap();
    db.create_project("p2", "Another Active", "").unwrap();
    db.create_project("p3", "Will Archive", "").unwrap();
    db.archive_project("p3").unwrap();

    let active = db.list_projects(Some("active")).unwrap();
    assert_eq!(active.len(), 2);

    let archived = db.list_projects(Some("archived")).unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, "p3");

    let all = db.list_projects(None).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_archive_project() {
    let db = test_db();
    db.create_project("p1", "Project", "").unwrap();

    assert!(db.archive_project("p1").unwrap());

    let proj = db.get_project("p1").unwrap().unwrap();
    assert_eq!(proj.status, "archived");

    // Archiving again is a no-op
    assert!(!db.archive_project("p1").unwrap());
}

#[test]
fn test_delete_project() {
    let db = test_db();
    db.create_project("p1", "Project", "").unwrap();

    assert!(db.delete_project("p1").unwrap());
    assert!(db.get_project("p1").unwrap().is_none());

    // Deleting nonexistent
    assert!(!db.delete_project("nonexistent").unwrap());
}

#[test]
fn test_update_project() {
    let db = test_db();
    db.create_project("p1", "Original", "Desc").unwrap();

    // Update name only
    assert!(db
        .update_project("p1", Some("Renamed"), None, None)
        .unwrap());
    let p = db.get_project("p1").unwrap().unwrap();
    assert_eq!(p.name, "Renamed");
    assert_eq!(p.description, "Desc"); // unchanged

    // Update description only
    assert!(db
        .update_project("p1", None, Some("New desc"), None)
        .unwrap());
    let p = db.get_project("p1").unwrap().unwrap();
    assert_eq!(p.name, "Renamed"); // unchanged
    assert_eq!(p.description, "New desc");

    // Update status
    assert!(db
        .update_project("p1", None, None, Some("archived"))
        .unwrap());
    let p = db.get_project("p1").unwrap().unwrap();
    assert_eq!(p.status, "archived");

    // Update all at once
    assert!(db
        .update_project("p1", Some("Final"), Some("Final desc"), Some("active"))
        .unwrap());
    let p = db.get_project("p1").unwrap().unwrap();
    assert_eq!(p.name, "Final");
    assert_eq!(p.description, "Final desc");
    assert_eq!(p.status, "active");

    // Nonexistent
    assert!(!db
        .update_project("nonexistent", Some("X"), None, None)
        .unwrap());
}

// ============================================================
// Workflow-by-Session Query Tests
// ============================================================

#[test]
fn test_list_workflows_by_session_id() {
    let db = test_db();
    let steps = sample_steps(1);

    // Create a session first (FK target)
    db.conn()
        .execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES ('sess-a', 1000, 1000)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES ('sess-b', 1000, 1000)",
            [],
        )
        .unwrap();

    db.create_workflow(
        "wf-1",
        "WF 1",
        "G1",
        &steps,
        Some("sess-a"),
        None,
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-2",
        "WF 2",
        "G2",
        &steps,
        Some("sess-a"),
        None,
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-3",
        "WF 3",
        "G3",
        &steps,
        Some("sess-b"),
        None,
        None,
        None,
    )
    .unwrap();

    let by_a = db.list_workflows_by_session("sess-a").unwrap();
    assert_eq!(by_a.len(), 2);
    assert!(by_a
        .iter()
        .all(|w| w.session_id.as_deref() == Some("sess-a")));

    let by_b = db.list_workflows_by_session("sess-b").unwrap();
    assert_eq!(by_b.len(), 1);
}

#[test]
fn test_list_workflows_by_session_id_none() {
    let db = test_db();
    let steps = sample_steps(1);

    // Workflow without session_id
    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let results = db.list_workflows_by_session("any-session").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_list_workflows_by_nonexistent_session() {
    let db = test_db();
    let results = db.list_workflows_by_session("no-such-session").unwrap();
    assert!(results.is_empty());
}

// ============================================================
// Workflow-by-Project Query Tests
// ============================================================

#[test]
fn test_list_workflows_by_project_id() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_project("proj-1", "Project Alpha", "").unwrap();

    db.create_workflow(
        "wf-1",
        "WF 1",
        "G1",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-2",
        "WF 2",
        "G2",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();
    db.create_workflow("wf-3", "WF 3", "G3", &steps, None, None, None, None)
        .unwrap();

    let by_proj = db.list_workflows_by_project("proj-1").unwrap();
    assert_eq!(by_proj.len(), 2);
    assert!(by_proj
        .iter()
        .all(|w| w.project_id.as_deref() == Some("proj-1")));
}

#[test]
fn test_list_workflows_by_project_id_none() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let results = db.list_workflows_by_project("any-project").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_list_workflows_by_nonexistent_project() {
    let db = test_db();
    let results = db.list_workflows_by_project("no-such-project").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_project_with_multiple_workflows_lifecycle() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_project("proj-1", "Release v2", "Ship the v2 release")
        .unwrap();

    db.create_workflow(
        "wf-1",
        "Build",
        "Build release",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-2",
        "Test",
        "Run tests",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-3",
        "Deploy",
        "Deploy to prod",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();

    // Complete wf-1
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "built").unwrap();
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "done").unwrap();

    // Fail wf-2
    let s = db.claim_next_workflow_step("wf-2").unwrap().unwrap();
    db.fail_workflow_step(s.id, "test failure").unwrap();

    // Query all workflows for the project — should return all 3 regardless of status
    let project_wfs = db.list_workflows_by_project("proj-1").unwrap();
    assert_eq!(project_wfs.len(), 3);

    let statuses: Vec<&str> = project_wfs.iter().map(|w| w.status.as_str()).collect();
    assert!(statuses.contains(&"completed"));
    assert!(statuses.contains(&"running")); // wf-2 still running (retry not exhausted), wf-3 running
}

// ============================================================
// FK Nullable Tests
// ============================================================

#[test]
fn test_workflow_nullable_session_id() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert!(wf.session_id.is_none());
}

#[test]
fn test_workflow_nullable_project_id() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert!(wf.project_id.is_none());
}

// ============================================================
// Project-Workflow Relationship Tests
// ============================================================

#[test]
fn test_delete_project_nullifies_workflow_project_id() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_project("proj-1", "Project", "Desc").unwrap();
    db.create_workflow(
        "wf-1",
        "WF 1",
        "G1",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();
    db.create_workflow(
        "wf-2",
        "WF 2",
        "G2",
        &steps,
        None,
        Some("proj-1"),
        None,
        None,
    )
    .unwrap();

    // Verify workflows and steps exist
    assert_eq!(db.list_workflows_by_project("proj-1").unwrap().len(), 2);
    assert_eq!(db.get_workflow_steps("wf-1").unwrap().len(), 2);

    // Delete the project — should nullify project_id, not cascade-delete workflows
    assert!(db.delete_project("proj-1").unwrap());

    // Project is gone
    assert!(db.get_project("proj-1").unwrap().is_none());

    // Workflows still exist but project_id is NULL
    let wf1 = db.get_workflow("wf-1").unwrap().unwrap();
    assert!(wf1.project_id.is_none());
    let wf2 = db.get_workflow("wf-2").unwrap().unwrap();
    assert!(wf2.project_id.is_none());

    // Steps still intact
    assert_eq!(db.get_workflow_steps("wf-1").unwrap().len(), 2);
    assert_eq!(db.get_workflow_steps("wf-2").unwrap().len(), 2);
}

#[test]
fn test_archive_project_preserves_workflow_links() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_project("proj-1", "Project", "Desc").unwrap();
    db.create_workflow("wf-1", "WF", "G", &steps, None, Some("proj-1"), None, None)
        .unwrap();

    db.archive_project("proj-1").unwrap();

    // Workflow should still be linked
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.project_id.as_deref(), Some("proj-1"));

    // Project query should still work
    let by_proj = db.list_workflows_by_project("proj-1").unwrap();
    assert_eq!(by_proj.len(), 1);
}
