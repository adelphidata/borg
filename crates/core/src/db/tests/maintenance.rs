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

    let stalled = db
        .find_stalled_tasks(now, 3600)
        .expect("find_stalled_tasks");
    let ids: Vec<&str> = stalled.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"stalled-1"), "overdue cron must be stalled");
    assert!(!ids.contains(&"oneshot"), "once-tasks must be excluded");
    assert!(!ids.contains(&"future"), "future task must not be stalled");
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
        activity_rows_deleted: 5,
        embeddings_pruned: 0,
        stalled_tasks_healed: 0,
        persistent_warnings: vec!["Gateway:reachable".into()],
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
