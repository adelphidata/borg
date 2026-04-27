use super::*;

#[test]
fn test_create_workflow_with_steps() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow(
        "wf-1",
        "Test WF",
        "Do the thing",
        &steps,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.title, "Test WF");
    assert_eq!(wf.goal, "Do the thing");
    assert_eq!(wf.status, status::RUNNING);
    assert_eq!(wf.current_step, 0);

    let db_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(db_steps.len(), 3);
    assert_eq!(db_steps[0].title, "Step 1");
    assert_eq!(db_steps[0].step_index, 0);
    assert_eq!(db_steps[1].step_index, 1);
    assert_eq!(db_steps[2].step_index, 2);
    for s in &db_steps {
        assert_eq!(s.status, step_status::PENDING);
    }
}

#[test]
fn test_create_workflow_empty_steps_rejected() {
    let db = test_db();
    let result = db.create_workflow("wf-1", "Empty", "Nothing", &[], None, None, None, None);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one step"));
}

#[test]
fn test_create_workflow_exceeds_max_steps() {
    let db = test_db();
    let steps = sample_steps(51);
    let result = db.create_workflow("wf-1", "Too many", "Goal", &steps, None, None, None, None);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("maximum"));
}

#[test]
fn test_create_workflow_with_delivery() {
    let db = test_db();
    let steps = sample_steps(1);

    // Create session for FK
    db.conn()
        .execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES ('session-123', 1000, 1000)",
            [],
        )
        .unwrap();

    db.create_workflow(
        "wf-d",
        "Deliver WF",
        "Goal",
        &steps,
        Some("session-123"),
        None,
        Some("telegram"),
        Some("12345"),
    )
    .unwrap();

    let wf = db.get_workflow("wf-d").unwrap().unwrap();
    assert_eq!(wf.session_id.as_deref(), Some("session-123"));
    assert_eq!(wf.delivery_channel.as_deref(), Some("telegram"));
    assert_eq!(wf.delivery_target.as_deref(), Some("12345"));
}

#[test]
fn test_get_workflow_not_found() {
    let db = test_db();
    assert!(db.get_workflow("nonexistent").unwrap().is_none());
}

#[test]
fn test_list_workflows_no_filter() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    let all = db.list_workflows(None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn test_list_workflows_with_status_filter() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    // Cancel one
    db.cancel_workflow("wf-1").unwrap();

    let running = db.list_workflows(Some(status::RUNNING)).unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].id, "wf-2");

    let cancelled = db.list_workflows(Some(status::CANCELLED)).unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].id, "wf-1");
}

#[test]
fn test_workflow_cascade_delete() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    assert!(db.delete_workflow("wf-1").unwrap());
    assert!(db.get_workflow("wf-1").unwrap().is_none());
    assert!(db.get_workflow_steps("wf-1").unwrap().is_empty());
}

#[test]
fn test_delete_nonexistent_workflow() {
    let db = test_db();
    assert!(!db.delete_workflow("nonexistent").unwrap());
}

#[test]
fn test_create_workflow_exactly_max_steps() {
    let db = test_db();
    let steps = sample_steps(50);

    db.create_workflow(
        "wf-max",
        "Max Steps",
        "Goal",
        &steps,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let db_steps = db.get_workflow_steps("wf-max").unwrap();
    assert_eq!(db_steps.len(), 50);
    assert_eq!(db_steps[49].step_index, 49);
}
