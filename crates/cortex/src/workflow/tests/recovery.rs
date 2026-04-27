use super::*;

#[test]
fn test_recover_stale_steps() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim a step (sets it to running with started_at = now)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();

    // Backdate started_at to simulate a crash (>5 min ago)
    let stale_time = chrono::Utc::now().timestamp() - 600;
    db.conn()
        .execute(
            "UPDATE workflow_steps SET started_at = ?1 WHERE id = ?2",
            rusqlite::params![stale_time, s.id],
        )
        .unwrap();

    // Recover stale steps
    let count = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(count, 1);

    // Step should be back to pending
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::PENDING);
    assert!(all_steps[0].started_at.is_none());
}

#[test]
fn test_recover_does_not_touch_recent_running_steps() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim a step — started_at is now (within 5-min grace)
    let _s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();

    // Recovery should NOT touch it
    let count = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(count, 0);

    // Step should still be running
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::RUNNING);
}

#[test]
fn test_recover_no_stale_steps() {
    let db = test_db();
    let count = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_recover_multiple_stale_steps_across_workflows() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    // Claim one step from each workflow
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let s2 = db.claim_next_workflow_step("wf-2").unwrap().unwrap();

    // Backdate both to simulate crash
    let stale_time = chrono::Utc::now().timestamp() - 600;
    db.conn()
        .execute(
            "UPDATE workflow_steps SET started_at = ?1 WHERE id IN (?2, ?3)",
            rusqlite::params![stale_time, s1.id, s2.id],
        )
        .unwrap();

    let count = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(count, 2);

    // Both should be back to pending
    let steps1 = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(steps1[0].status, step_status::PENDING);
    let steps2 = db.get_workflow_steps("wf-2").unwrap();
    assert_eq!(steps2[0].status, step_status::PENDING);
}

#[test]
fn test_recover_preserves_retry_count() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim, fail (retry_count becomes 1), claim again, fail (retry_count becomes 2)
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "err1").unwrap();
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "err2").unwrap();

    // Now claim again — step has retry_count=2
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s.retry_count, 2);

    // Backdate to make it stale
    let stale_time = chrono::Utc::now().timestamp() - 600;
    db.conn()
        .execute(
            "UPDATE workflow_steps SET started_at = ?1 WHERE id = ?2",
            rusqlite::params![stale_time, s.id],
        )
        .unwrap();

    db.recover_stale_workflow_steps().unwrap();

    // Retry count should be preserved
    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::PENDING);
    assert_eq!(all_steps[0].retry_count, 2);
}

#[test]
fn test_recover_mixed_state_workflow() {
    let db = test_db();
    let steps = sample_steps(3);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete step 0
    let s0 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s0.id, "done").unwrap();

    // Claim step 1, then backdate to make it stale
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let stale_time = chrono::Utc::now().timestamp() - 600;
    db.conn()
        .execute(
            "UPDATE workflow_steps SET started_at = ?1 WHERE id = ?2",
            rusqlite::params![stale_time, s1.id],
        )
        .unwrap();

    // Step 2 is still pending (untouched)

    let count = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(count, 1); // Only step 1 recovered

    let all_steps = db.get_workflow_steps("wf-1").unwrap();
    assert_eq!(all_steps[0].status, step_status::COMPLETED); // unchanged
    assert_eq!(all_steps[1].status, step_status::PENDING); // recovered
    assert!(all_steps[1].started_at.is_none());
    assert_eq!(all_steps[2].status, step_status::PENDING); // unchanged
}

#[test]
fn test_recover_stale_step_then_complete_workflow() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim step 0 and simulate it going stale (>5 min ago)
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let stale_time = chrono::Utc::now().timestamp() - 600;
    db.conn()
        .execute(
            "UPDATE workflow_steps SET started_at = ?1 WHERE id = ?2",
            rusqlite::params![stale_time, s1.id],
        )
        .unwrap();

    // Recovery should reset it
    let recovered = db.recover_stale_workflow_steps().unwrap();
    assert_eq!(recovered, 1);

    // Step should be claimable again — complete both steps
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s1.step_index, 0);
    db.complete_workflow_step(s1.id, "recovered").unwrap();

    let s2 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    assert_eq!(s2.step_index, 1);
    db.complete_workflow_step(s2.id, "done").unwrap();

    let wf = db.get_workflow("wf-1").unwrap().unwrap();
    assert_eq!(wf.status, status::COMPLETED);
    assert!(wf.completed_at.is_some());
}
