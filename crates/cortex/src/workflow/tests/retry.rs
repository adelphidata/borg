use super::*;

#[test]
fn test_fail_step_increments_retry_count() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let exhausted = db.fail_workflow_step(s1.id, "timeout").unwrap();
    assert!(!exhausted);

    // Step should be back to pending with incremented retry
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::PENDING);
    assert_eq!(all_steps[0].retry_count, 1);
    assert_eq!(all_steps[0].error.as_deref(), Some("timeout"));

    // Workflow should still be running
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::RUNNING);
}

#[test]
fn test_fail_step_exceeds_max_retries() {
    let db = test_db();
    let steps = vec![NewWorkflowStep {
        title: "Flaky step".to_string(),
        instructions: "Try it".to_string(),
        max_retries: 2,
        timeout_ms: 300000,
    }];

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Fail twice to exhaust retries (max_retries = 2)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "err1").unwrap();

    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let exhausted = db.fail_workflow_step(s.id, "err2").unwrap();
    assert!(exhausted);

    // Step should be failed
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::FAILED);
    assert_eq!(all_steps[0].retry_count, 2);

    // Workflow should be failed
    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::FAILED);
    assert_eq!(wf.error.as_deref(), Some("err2"));
}

#[test]
fn test_fail_step_skips_remaining_pending_steps() {
    let db = test_db();
    let mut steps = sample_steps(3);
    steps[0].max_retries = 1; // Will exhaust after 1 failure

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "fatal").unwrap();

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::FAILED);
    assert_eq!(all_steps[1].status, step_status::SKIPPED);
    assert_eq!(all_steps[2].status, step_status::SKIPPED);
}

#[test]
fn test_workflow_with_zero_max_retries() {
    let db = test_db();
    let steps = vec![NewWorkflowStep {
        title: "No retries".to_string(),
        instructions: "Fail immediately".to_string(),
        max_retries: 0,
        timeout_ms: 300000,
    }];

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let exhausted = db.fail_workflow_step(s.id, "fatal").unwrap();
    assert!(exhausted);

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::FAILED);

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::FAILED);
    assert_eq!(all_steps[0].retry_count, 1);
}

#[test]
fn test_fail_nonexistent_step() {
    let db = test_db();
    let result = db.fail_workflow_step(99999, "err").unwrap();
    assert!(!result);
}

#[test]
fn test_step_error_updated_on_each_retry() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Fail with first error
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "timeout error").unwrap();

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].error.as_deref(), Some("timeout error"));

    // Fail with second error
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "connection refused").unwrap();

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].error.as_deref(), Some("connection refused"));
    assert_eq!(all_steps[0].retry_count, 2);
}

#[test]
fn test_fail_retry_then_succeed_completes_workflow() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim and fail (retry 1 of 3)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let exhausted = db.fail_workflow_step(s.id, "transient error").unwrap();
    assert!(!exhausted);

    // Step should be reset to pending with retry_count = 1
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::PENDING);
    assert_eq!(all_steps[0].retry_count, 1);

    // Claim again and succeed this time
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.retry_count, 1);
    db.complete_workflow_step(s.id, "success on retry").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::COMPLETED);
}
