//! Tests for self-healing maintenance: stalled-task detection,
//! `doctor_runs` persistence, and the seeded maintenance task.

use super::{simple_task, test_db};
use crate::maintenance::{MaintenanceReport, MAINTENANCE_TASK_ID};
use crate::tasks::{heal_stalled_tasks, RUN_STATUS_MISSED};

#[test]
fn maintenance_task_is_seeded_by_v37() {
    let db = test_db();
    let task = db
        .get_task_by_id(MAINTENANCE_TASK_ID)
        .expect("query")
        .expect("seeded maintenance task");
    assert_eq!(task.task_type, "maintenance");
    assert_eq!(task.schedule_type, "cron");
    assert_eq!(task.status, "active");
    assert!(
        task.next_run.is_some(),
        "seeded task must have next_run computed"
    );
}

#[test]
fn find_stalled_tasks_returns_overdue_recurring_only() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let long_ago = now - 24 * 3600;

    // Overdue cron task — should show up.
    db.create_task(&simple_task(
        "stalled-1",
        "stalled",
        "prompt",
        "cron",
        "0 0 * * * *",
        Some(long_ago),
    ))
    .unwrap();

    // One-shot task also overdue — must be excluded (one-shots are
    // allowed to sit until manually fired).
    db.create_task(&simple_task(
        "oneshot",
        "later",
        "prompt",
        "once",
        "",
        Some(long_ago),
    ))
    .unwrap();

    // Future task — not stalled.
    db.create_task(&simple_task(
        "future",
        "ok",
        "prompt",
        "cron",
        "0 0 * * * *",
        Some(now + 3600),
    ))
    .unwrap();

    // Overdue recurring task that's already in retry backoff — excluded
    // so we don't double-handle what the retry loop is already managing.
    db.create_task(&simple_task(
        "in-retry",
        "retrying",
        "prompt",
        "cron",
        "0 0 * * * *",
        Some(long_ago),
    ))
    .unwrap();
    db.set_task_retry("in-retry", 1, "simulated retry", now + 300)
        .unwrap();

    let stalled = db
        .find_stalled_tasks(now, 3600)
        .expect("find_stalled_tasks");
    let ids: Vec<&str> = stalled.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"stalled-1"), "overdue cron must be stalled");
    assert!(!ids.contains(&"oneshot"), "once-tasks must be excluded");
    assert!(!ids.contains(&"future"), "future task must not be stalled");
    assert!(
        !ids.contains(&"in-retry"),
        "tasks with retry_after set must be excluded"
    );
}

#[test]
fn heal_stalled_tasks_records_miss_and_resets_next_run() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let long_ago = now - 24 * 3600;
    db.create_task(&simple_task(
        "heal-me",
        "overdue",
        "prompt",
        "cron",
        "0 0 * * * *",
        Some(long_ago),
    ))
    .unwrap();

    let report = heal_stalled_tasks(&db, now, 3600).expect("heal");
    assert!(report.detected >= 1);
    assert!(report.reset >= 1);

    let task = db.get_task_by_id("heal-me").unwrap().expect("task exists");
    assert!(
        task.next_run.unwrap() > now,
        "next_run should be advanced into the future"
    );

    let runs = db.task_run_history("heal-me", 10).unwrap();
    assert!(
        runs.iter().any(|r| r.status == RUN_STATUS_MISSED),
        "a missed-run audit row should be recorded"
    );
}

#[test]
fn record_and_latest_doctor_run_roundtrip() {
    let db = test_db();
    let report = MaintenanceReport {
        ran_at: 42,
        pass_count: 10,
        warn_count: 2,
        fail_count: 0,
        log_files_deleted: 1,
        log_bytes_truncated: 0,
        workflows_pruned: 0,
        activity_rows_deleted: 5,
        embeddings_pruned: 0,
        stalled_tasks_healed: 0,
        persistent_warnings: vec!["Gateway:reachable".into()],
        current_issues: vec!["Gateway:reachable".into()],
        check_summary: vec!["  ⚠ Gateway:reachable — down".into()],
    };
    db.record_doctor_run(&report).expect("record");

    let latest = db.latest_doctor_run().unwrap().expect("latest");
    assert_eq!(latest.ran_at, 42);
    assert_eq!(latest.pass_count, 10);
    assert_eq!(latest.warn_count, 2);
    assert_eq!(latest.persistent_warnings, vec!["Gateway:reachable"]);
}

#[test]
fn prune_completed_workflows_only_deletes_terminal_older_than_cutoff() {
    use crate::db::models::NewWorkflowStep;
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let old = now - 30 * 86_400; // 30 days ago
    let recent = now - 1 * 86_400; // 1 day ago
    let cutoff = now - 7 * 86_400; // default workflow_retention_days

    let step = || NewWorkflowStep {
        title: "s".into(),
        instructions: "i".into(),
        max_retries: 0,
        timeout_ms: 1000,
    };

    // Three workflows: old-completed, recent-completed, old-running.
    db.create_workflow("old-done", "t", "g", &[step()], None, None, None, None)
        .unwrap();
    db.create_workflow("recent-done", "t", "g", &[step()], None, None, None, None)
        .unwrap();
    db.create_workflow("old-running", "t", "g", &[step()], None, None, None, None)
        .unwrap();

    // Force status + completed_at directly so the test controls the timeline
    // (create_workflow always stamps `now`). This mirrors what the workflow
    // executor does on completion, without running a real agent loop.
    db.conn()
        .execute(
            "UPDATE workflows SET status = 'completed', completed_at = ?1 WHERE id = 'old-done'",
            [old],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE workflows SET status = 'completed', completed_at = ?1 WHERE id = 'recent-done'",
            [recent],
        )
        .unwrap();
    // old-running: keep status=running, completed_at=NULL — must NEVER be pruned.

    let deleted = db.prune_completed_workflows(cutoff).unwrap();
    assert_eq!(deleted, 1, "only old-done should be pruned");

    assert!(
        db.get_workflow("old-done").unwrap().is_none(),
        "old terminal workflow should be gone"
    );
    assert!(
        db.get_workflow("recent-done").unwrap().is_some(),
        "recent terminal workflow must be retained"
    );
    assert!(
        db.get_workflow("old-running").unwrap().is_some(),
        "active workflow must never be pruned even if old"
    );

    // Steps for the pruned workflow must also be gone (no orphan rows).
    let orphan_steps = db.get_workflow_steps("old-done").unwrap();
    assert!(
        orphan_steps.is_empty(),
        "steps of pruned workflow must be deleted too"
    );
}

#[test]
fn prune_doctor_runs_keeps_n_newest() {
    let db = test_db();
    for ts in [1, 2, 3, 4, 5] {
        let r = MaintenanceReport {
            ran_at: ts,
            ..Default::default()
        };
        db.record_doctor_run(&r).unwrap();
    }
    let deleted = db.prune_doctor_runs(2).unwrap();
    assert_eq!(deleted, 3);
    let latest = db.latest_doctor_run().unwrap().unwrap();
    assert_eq!(latest.ran_at, 5);
}
