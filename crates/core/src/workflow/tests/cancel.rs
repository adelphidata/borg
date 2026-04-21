use super::*;

#[test]
fn test_cancel_workflow() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete first step, then cancel
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "done").unwrap();

    let cancelled = db.cancel_workflow("wf-1").unwrap();
    assert!(cancelled);

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::CANCELLED);

    // Pending steps should be skipped, completed stays completed
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::COMPLETED);
    assert_eq!(all_steps[1].status, step_status::SKIPPED);
    assert_eq!(all_steps[2].status, step_status::SKIPPED);
}

#[test]
fn test_complete_step_after_cancel_is_noop() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim step 1 (sets it to running)
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();

    // Cancel the workflow (sets running step to skipped)
    db.cancel_workflow("wf-1").unwrap();

    // Try to complete the step — should be a no-op since it's now skipped
    db.complete_workflow_step(s1.id, "late result").unwrap();

    // Step should still be skipped, not completed
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::SKIPPED);

    // Workflow should still be cancelled
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::CANCELLED);
}

#[test]
fn test_fail_step_after_cancel_is_noop() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.cancel_workflow("wf-1").unwrap();

    // Try to fail the step — should return false (no-op)
    let exhausted = db.fail_workflow_step(s1.id, "too late").unwrap();
    assert!(!exhausted);

    // Step should still be skipped
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::SKIPPED);
}

#[test]
fn test_cancel_already_completed() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "done").unwrap();

    let cancelled = db.cancel_workflow("wf-1").unwrap();
    assert!(!cancelled);
}

#[test]
fn test_cancel_nonexistent() {
    let db = test_db();
    let cancelled = db.cancel_workflow("nonexistent").unwrap();
    assert!(!cancelled);
}

#[test]
fn test_no_step_claimed_after_workflow_cancelled() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    db.cancel_workflow("wf-1").unwrap();

    // All steps are skipped, so nothing to claim
    assert!(db.claim_next_workflow_step("wf-1").unwrap().is_none());
}

#[test]
fn test_cancel_failed_workflow() {
    let db = test_db();
    let mut steps = sample_steps(1);
    steps[0].max_retries = 1;

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Fail to make workflow FAILED
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "fatal").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::FAILED);

    // Cancelling a failed workflow should return false
    let cancelled = db.cancel_workflow("wf-1").unwrap();
    assert!(!cancelled);
}
