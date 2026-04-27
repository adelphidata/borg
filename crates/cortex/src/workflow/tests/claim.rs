use super::*;

#[test]
fn test_claim_next_step_ordering() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // First claim should get step_index 0
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s1.step_index, 0);
    assert_eq!(s1.status, step_status::RUNNING);
    assert!(s1.started_at.is_some());

    // Complete it
    db.complete_workflow_step(s1.id, "done 1").unwrap();

    // Next claim should get step_index 1
    let s2 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s2.step_index, 1);
}

#[test]
fn test_claim_step_skips_completed() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim and complete first two steps
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "done 1").unwrap();
    let s2 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s2.id, "done 2").unwrap();

    // Next claim should get step_index 2
    let s3 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s3.step_index, 2);
}

#[test]
fn test_claim_step_returns_none_when_all_done() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s1.id, "done").unwrap();

    // No more steps
    assert!(db.claim_next_workflow_step("wf-1").unwrap().is_none());
}

#[test]
fn test_claim_step_nonexistent_workflow() {
    let db = test_db();
    let result = db.claim_next_workflow_step("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_double_claim_blocked_by_runnable_query() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim step 0 (now running)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.step_index, 0);

    // get_runnable_workflows should exclude this workflow (it has a running step)
    let runnable = db.get_runnable_workflows().unwrap();
    assert!(
        runnable.is_empty(),
        "Workflow with running step should not be runnable"
    );

    // Complete step 0 first, then workflow becomes runnable again
    db.complete_workflow_step(s.id, "done").unwrap();
    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 1);
}
