//! Tests for self-healing maintenance: stalled-task detection,
//! `doctor_runs` persistence, and the seeded maintenance task.

use super::{simple_task, test_db};
use crate::db::NewTask;
use crate::maintenance::{MaintenanceReport, MAINTENANCE_TASK_ID};
use crate::tasks::{
    calculate_next_run, heal_stalled_tasks, recover_wedged_runs, RUN_STATUS_FAILED,
    RUN_STATUS_MISSED, RUN_STATUS_RUNNING, RUN_STATUS_SUCCESS,
};
use rusqlite::params;

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

// ── Wedged-run recovery (in-daemon sweep) ──

/// Directly insert a `task_runs` row with a controlled `started_at` and
/// status. The normal `start_task_run` helper stamps `now`, which makes
/// it impossible to simulate an old wedged row without a second UPDATE.
fn insert_run_at(db: &crate::db::Database, task_id: &str, started_at: i64, status: &str) -> i64 {
    db.conn()
        .execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
             VALUES (?1, ?2, 0, ?3)",
            params![task_id, started_at, status],
        )
        .unwrap();
    db.conn().last_insert_rowid()
}

fn task_with_timeout<'a>(
    id: &'a str,
    schedule_expr: &'a str,
    timeout_ms: Option<i64>,
) -> NewTask<'a> {
    NewTask {
        id,
        name: "t",
        prompt: "p",
        schedule_type: "cron",
        schedule_expr,
        timezone: "local",
        next_run: None,
        max_retries: None,
        timeout_ms,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
    }
}

fn run_status(db: &crate::db::Database, run_id: i64) -> (String, Option<String>) {
    db.conn()
        .query_row(
            "SELECT status, error FROM task_runs WHERE id = ?1",
            params![run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
}

#[test]
fn recover_wedged_runs_fails_rows_past_task_timeout() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    // 1-minute task timeout — anything older than 60s is wedged.
    db.create_task(&task_with_timeout("tight", "0 0 * * * *", Some(60_000)))
        .unwrap();
    let run_id = insert_run_at(&db, "tight", now - 3600, RUN_STATUS_RUNNING);

    let n = recover_wedged_runs(&db, now, 3600).expect("recover");
    assert_eq!(n, 1);
    let (status, error) = run_status(&db, run_id);
    assert_eq!(status, RUN_STATUS_FAILED);
    assert!(
        error.as_deref().unwrap_or("").contains("wedged"),
        "error must mention wedged, got: {error:?}"
    );
}

#[test]
fn recover_wedged_runs_leaves_young_rows_alone() {
    // Generous 2-hour timeout — a 30-min-old running row must not be
    // reaped even though the default grace (1h) is smaller. Verifies
    // the per-task timeout beats the default in the COALESCE.
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    db.create_task(&task_with_timeout(
        "roomy",
        "0 0 * * * *",
        Some(2 * 60 * 60 * 1000),
    ))
    .unwrap();
    let young = insert_run_at(&db, "roomy", now - 1800, RUN_STATUS_RUNNING);

    let n = recover_wedged_runs(&db, now, 60).expect("recover");
    assert_eq!(n, 0, "30-min-old row with 2h timeout must not be reaped");
    assert_eq!(run_status(&db, young).0, RUN_STATUS_RUNNING);
}

#[test]
fn recover_wedged_runs_ignores_non_running_rows() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    db.create_task(&task_with_timeout("done", "0 0 * * * *", Some(60_000)))
        .unwrap();
    let success = insert_run_at(&db, "done", now - 3600, RUN_STATUS_SUCCESS);
    let missed = insert_run_at(&db, "done", now - 3600, RUN_STATUS_MISSED);

    let n = recover_wedged_runs(&db, now, 3600).expect("recover");
    assert_eq!(n, 0);
    assert_eq!(run_status(&db, success).0, RUN_STATUS_SUCCESS);
    assert_eq!(run_status(&db, missed).0, RUN_STATUS_MISSED);
}

// ── Clock-jump storm aggregation ──

#[test]
fn heal_stalled_tasks_caps_audit_rows_on_clock_jump() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let long_ago = now - 24 * 3600;
    // 10 > CLOCK_JUMP_AGGREGATE_THRESHOLD (5), so we expect aggregation.
    for i in 0..10 {
        let id = format!("storm-{i}");
        db.create_task(&simple_task(
            &id,
            "s",
            "p",
            "cron",
            "0 0 * * * *",
            Some(long_ago),
        ))
        .unwrap();
    }

    let report = heal_stalled_tasks(&db, now, 3600).expect("heal");
    assert_eq!(report.detected, 10);
    assert_eq!(report.reset, 10);
    assert!(report.aggregated, "storm above threshold must aggregate");

    // Count missed audit rows across all storm-N tasks.
    let total_missed: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND task_id LIKE 'storm-%'",
            params![RUN_STATUS_MISSED],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        total_missed, 1,
        "aggregated sweep must write exactly one audit row, got {total_missed}"
    );

    // Every task's next_run should have been advanced past `now`.
    for i in 0..10 {
        let id = format!("storm-{i}");
        let task = db.get_task_by_id(&id).unwrap().unwrap();
        assert!(
            task.next_run.unwrap() > now,
            "{id} next_run must be advanced"
        );
    }

    // Aggregate row mentions the fleet size and the clock-jump marker.
    let (_status, error) = db
        .conn()
        .query_row(
            "SELECT status, error FROM task_runs WHERE status = ?1 LIMIT 1",
            params![RUN_STATUS_MISSED],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert!(error.contains("10"), "error must name fleet size: {error}");
    assert!(
        error.contains("clock-jump"),
        "error must flag clock-jump: {error}"
    );
}

#[test]
fn heal_stalled_tasks_records_per_task_below_threshold() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let long_ago = now - 24 * 3600;
    // 3 tasks (<= threshold) — expect one row per task.
    for i in 0..3 {
        let id = format!("few-{i}");
        db.create_task(&simple_task(
            &id,
            "s",
            "p",
            "cron",
            "0 0 * * * *",
            Some(long_ago),
        ))
        .unwrap();
    }
    let report = heal_stalled_tasks(&db, now, 3600).expect("heal");
    assert_eq!(report.detected, 3);
    assert!(!report.aggregated, "below-threshold must not aggregate");

    let total_missed: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND task_id LIKE 'few-%'",
            params![RUN_STATUS_MISSED],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total_missed, 3, "one missed row per task below threshold");
}

// ── Timezone-aware cron evaluation ──

#[test]
fn calculate_next_run_honors_iana_timezone() {
    use chrono::{TimeZone, Timelike};
    use chrono_tz::America::New_York;

    // "02:00 every day" in New York. Regardless of current UTC time, the
    // next firing converted back to America/New_York must land at 02:00.
    let ts = calculate_next_run("cron", "0 0 2 * * *", "America/New_York")
        .unwrap()
        .expect("cron produces a next firing");
    let ny_time = New_York.timestamp_opt(ts, 0).unwrap();
    assert_eq!(
        (ny_time.hour(), ny_time.minute(), ny_time.second()),
        (2, 0, 0),
        "next firing must be 02:00 New York local, got {ny_time}"
    );
}

#[test]
fn calculate_next_run_falls_back_to_utc_on_bad_tz() {
    // Unparseable zone must not error and must match the UTC result
    // for the same cron expression. A scheduled task with a corrupted
    // timezone field must keep firing, not go dark.
    let bad = calculate_next_run("cron", "0 0 9 * * *", "Not/A/Zone")
        .unwrap()
        .unwrap();
    let utc = calculate_next_run("cron", "0 0 9 * * *", "UTC")
        .unwrap()
        .unwrap();
    assert_eq!(bad, utc);
}

#[test]
fn calculate_next_run_treats_local_and_empty_as_utc() {
    // Regression guard: `"local"`, `""`, and `"UTC"` must all resolve
    // identically so existing seeded rows (stored as "local") keep the
    // pre-timezone-aware UTC semantics.
    let expr = "0 0 3 * * *";
    let local = calculate_next_run("cron", expr, "local").unwrap().unwrap();
    let empty = calculate_next_run("cron", expr, "").unwrap().unwrap();
    let utc = calculate_next_run("cron", expr, "UTC").unwrap().unwrap();
    assert_eq!(local, empty);
    assert_eq!(local, utc);
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
