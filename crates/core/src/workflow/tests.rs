//! Comprehensive tests for the workflow engine.

use crate::db::Database;
use crate::db::NewWorkflowStep;
use crate::workflow::{status, step_status};

fn test_db() -> Database {
    Database::test_db()
}

fn sample_steps(n: usize) -> Vec<NewWorkflowStep> {
    (0..n)
        .map(|i| NewWorkflowStep {
            title: format!("Step {}", i + 1),
            instructions: format!("Execute step {} instructions", i + 1),
            max_retries: 3,
            timeout_ms: 300000,
        })
        .collect()
}

// ============================================================
// DB Layer Tests
// ============================================================

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

// ============================================================
// Step Claiming Tests
// ============================================================

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

// ============================================================
// Step Completion Tests
// ============================================================

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

// ============================================================
// Step Failure and Retry Tests
// ============================================================

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

// ============================================================
// Cancel Tests
// ============================================================

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

// ============================================================
// Recovery Tests
// ============================================================

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

// ============================================================
// Runnable Workflows Tests
// ============================================================

#[test]
fn test_get_runnable_workflows() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);
}

#[test]
fn test_get_runnable_excludes_cancelled() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    db.cancel_workflow("wf-1").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 1);
    assert_eq!(runnable[0].id, "wf-2");
}

#[test]
fn test_get_runnable_excludes_workflows_with_running_step() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim a step — now wf has a running step, shouldn't be in runnable
    let _s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

// ============================================================
// Completed Steps Query Tests
// ============================================================

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

// ============================================================
// Delete Tests
// ============================================================

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

// ============================================================
// State Machine / Lifecycle Tests
// ============================================================

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

// ============================================================
// workflows_active() Tests
// ============================================================

#[test]
fn test_workflows_active_on_overrides_strong_model() {
    let mut config = crate::config::Config::default();
    config.llm.model = "claude-opus-4".to_string();
    config.workflow.enabled = "on".to_string();
    assert!(crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_off_overrides_weak_model() {
    let mut config = crate::config::Config::default();
    config.llm.model = "llama-3.3-70b".to_string();
    config.workflow.enabled = "off".to_string();
    assert!(!crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_auto_uses_model_heuristic() {
    let mut config = crate::config::Config::default();
    config.workflow.enabled = "auto".to_string();

    // Opus → no workflows
    config.llm.model = "claude-opus-4".to_string();
    assert!(!crate::workflow::workflows_active(&config));

    // Sonnet → workflows
    config.llm.model = "claude-sonnet-4".to_string();
    assert!(crate::workflow::workflows_active(&config));

    // GPT-4o → workflows
    config.llm.model = "gpt-4o".to_string();
    assert!(crate::workflow::workflows_active(&config));

    // Unknown → workflows
    config.llm.model = "custom-model".to_string();
    assert!(crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_default_is_auto() {
    let config = crate::config::Config::default();
    assert_eq!(config.workflow.enabled, "auto");
}

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
// Boundary & Edge Case Tests
// ============================================================

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
fn test_claim_step_nonexistent_workflow() {
    let db = test_db();
    let result = db.claim_next_workflow_step("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_fail_nonexistent_step() {
    let db = test_db();
    let result = db.fail_workflow_step(99999, "err").unwrap();
    assert!(!result);
}

// ============================================================
// Recovery Scenario Tests
// ============================================================

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

// ============================================================
// Runnable Query Edge Case Tests
// ============================================================

#[test]
fn test_get_runnable_excludes_failed_workflows() {
    let db = test_db();
    let mut steps = sample_steps(2);
    steps[0].max_retries = 1;

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Fail step to exhaust retries → workflow becomes FAILED
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "fatal").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

#[test]
fn test_get_runnable_excludes_completed_workflows() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete the only step → workflow becomes COMPLETED
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "done").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

// ============================================================
// Cancellation Edge Case Tests
// ============================================================

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

// ============================================================
// Step Error Tracking Tests
// ============================================================

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

// ============================================================
// Completed Steps Query Tests (Extended)
// ============================================================

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

// ============================================================
// Robustness & Resilience Tests
// ============================================================

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

#[test]
fn test_claim_steps_from_multiple_workflows_same_tick() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF A", "G", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF B", "G", &steps, None, None, None, None)
        .unwrap();

    // Both should be runnable initially
    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);

    // Claim from both (simulating same daemon tick)
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let s2 = db.claim_next_workflow_step("wf-2").unwrap().unwrap();
    assert_eq!(s1.workflow_id, "wf-1");
    assert_eq!(s2.workflow_id, "wf-2");

    // Neither should be runnable now (both have running steps)
    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());

    // Complete both and verify they advance independently
    db.complete_workflow_step(s1.id, "a1").unwrap();
    db.complete_workflow_step(s2.id, "b1").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);
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
