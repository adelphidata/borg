use super::*;

#[test]
fn test_workflow_lifecycle_happy_path() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Verify initial state
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::RUNNING);

    // Execute all steps
    for i in 0..3 {
        let step = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
        assert_eq!(step.step_index, i);
        db.complete_workflow_step(step.id, &format!("result {i}"))
            .unwrap();
    }

    // Verify final state
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::COMPLETED);
    assert!(wf.completed_at.is_some());
    assert_eq!(wf.current_step, 3);
}

#[test]
fn test_workflow_lifecycle_with_retry() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim step 1, fail once
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let exhausted = db.fail_workflow_step(s.id, "timeout").unwrap();
    assert!(!exhausted);

    // Retry should succeed
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.step_index, 0);
    assert_eq!(s.retry_count, 1);
    db.complete_workflow_step(s.id, "ok").unwrap();

    // Step 2
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.step_index, 1);
    db.complete_workflow_step(s.id, "ok").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::COMPLETED);
}

#[test]
fn test_workflow_lifecycle_failure() {
    let db = test_db();
    let mut steps = sample_steps(3);
    steps[1].max_retries = 1;

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete step 1
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "ok").unwrap();

    // Fail step 2 (max_retries = 1)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.step_index, 1);
    let exhausted = db.fail_workflow_step(s.id, "fatal error").unwrap();
    assert!(exhausted);

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::FAILED);
    assert_eq!(wf.error.as_deref(), Some("fatal error"));

    // Step 3 should be skipped
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[2].status, step_status::SKIPPED);
}

#[test]
fn test_workflow_lifecycle_cancel_midway() {
    let db = test_db();
    let steps = sample_steps(5);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete first 2 steps
    for _ in 0..2 {
        let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
        db.complete_workflow_step(s.id, "ok").unwrap();
    }

    db.cancel_workflow("wf-1").unwrap();

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::COMPLETED);
    assert_eq!(all_steps[1].status, step_status::COMPLETED);
    assert_eq!(all_steps[2].status, step_status::SKIPPED);
    assert_eq!(all_steps[3].status, step_status::SKIPPED);
    assert_eq!(all_steps[4].status, step_status::SKIPPED);
}
