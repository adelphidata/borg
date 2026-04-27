use super::*;

#[test]
fn test_complete_step_advances_current_step() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "result 1").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.current_step, 1);
    assert_eq!(wf.status, status::RUNNING);

    // Verify step has output
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].output.as_deref(), Some("result 1"));
    assert_eq!(all_steps[0].status, step_status::COMPLETED);
}

#[test]
fn test_complete_last_step_completes_workflow() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete both steps
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "r1").unwrap();
    let s2 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s2.id, "r2").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::COMPLETED);
    assert!(wf.completed_at.is_some());
}

#[test]
fn test_get_completed_workflow_steps() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete first two steps
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "output 1").unwrap();
    let s2 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s2.id, "output 2").unwrap();

    let completed = db.get_completed_workflow_steps("wf-1").unwrap();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0].output.as_deref(), Some("output 1"));
    assert_eq!(completed[1].output.as_deref(), Some("output 2"));
}

#[test]
fn test_get_completed_steps_after_cancel() {
    let db = test_db();
    let steps = sample_steps(4);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete first 2 steps
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "output 1").unwrap();
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "output 2").unwrap();

    // Cancel the workflow
    db.cancel_workflow("wf-1").unwrap();

    // Should return exactly the 2 completed steps
    let completed = db.get_completed_workflow_steps("wf-1").unwrap();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0].output.as_deref(), Some("output 1"));
    assert_eq!(completed[1].output.as_deref(), Some("output 2"));
}

#[test]
fn test_get_completed_steps_empty_workflow() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // No steps completed yet
    let completed = db.get_completed_workflow_steps("wf-1").unwrap();
    assert!(completed.is_empty());
}

#[test]
fn test_complete_step_with_large_output() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let large_output = "x".repeat(1_000_000); // 1MB
    db.complete_workflow_step(s.id, &large_output).unwrap();

    // Verify full output is stored
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].output.as_ref().unwrap().len(), 1_000_000);
    assert_eq!(all_steps[0].status, step_status::COMPLETED);
}
