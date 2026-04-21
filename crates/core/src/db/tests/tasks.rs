use super::*;
use rusqlite::params;

#[test]
fn create_and_list_tasks() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "morning summary",
        "summarize",
        "cron",
        "0 9 * * *",
        Some(100),
    ))
    .expect("create task");
    db.create_task(&simple_task(
        "t2",
        "stock check",
        "check stocks",
        "interval",
        "1h",
        Some(200),
    ))
    .expect("create task 2");

    let tasks = db.list_tasks().expect("list");
    // +5 for seeded tasks: Monthly Security Audit, Daily Summary, Nightly
    // Consolidation, Weekly Maintenance, Daily Self-Healing Maintenance.
    assert_eq!(tasks.len(), 7);
}

#[test]
fn get_due_tasks_filters_correctly() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "due",
        "prompt",
        "cron",
        "expr",
        Some(50),
    ))
    .expect("create");
    db.create_task(&simple_task(
        "t2",
        "not due",
        "prompt",
        "cron",
        "expr",
        Some(200),
    ))
    .expect("create");
    db.create_task(&simple_task(
        "t3",
        "paused",
        "prompt",
        "cron",
        "expr",
        Some(50),
    ))
    .expect("create");
    db.update_task_status("t3", "paused").expect("pause");

    let due = db.get_due_tasks(100).expect("due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "t1");
}

#[test]
fn update_task_status_and_next_run() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "cron",
        "expr",
        Some(100),
    ))
    .expect("create");

    assert!(db.update_task_status("t1", "paused").expect("update"));
    let task = db.get_task_by_id("t1").expect("get").expect("found");
    assert_eq!(task.status, "paused");

    db.update_task_next_run("t1", Some(999))
        .expect("update next_run");
    let task = db.get_task_by_id("t1").expect("get").expect("found");
    assert_eq!(task.next_run, Some(999));
}

#[test]
fn record_and_query_task_runs() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    db.record_task_run("t1", 1000, 500, Some("done"), None)
        .expect("record");
    db.record_task_run("t1", 2000, 300, None, Some("failed"))
        .expect("record");

    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].started_at, 2000); // most recent first
}

#[test]
fn update_nonexistent_task_returns_false() {
    let db = test_db();
    assert!(!db
        .update_task_status("nonexistent", "paused")
        .expect("update"));
}

#[test]
fn get_task_by_id_found() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.name, "test");
    assert_eq!(task.schedule_expr, "30m");
}

#[test]
fn get_task_by_id_not_found() {
    let db = test_db();
    assert!(db.get_task_by_id("nope").expect("get").is_none());
}

#[test]
fn delete_task_removes_task_and_runs() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    db.record_task_run("t1", 1000, 500, Some("done"), None)
        .expect("record");

    assert!(db.delete_task("t1").expect("delete"));
    assert!(db.get_task_by_id("t1").expect("get").is_none());
    assert!(db.task_run_history("t1", 10).expect("history").is_empty());
}

#[test]
fn delete_nonexistent_task_returns_false() {
    let db = test_db();
    assert!(!db.delete_task("nope").expect("delete"));
}

#[test]
fn update_task_fields() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "old name",
        "old prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");

    let update = UpdateTask {
        name: Some("new name"),
        prompt: None,
        schedule_type: None,
        schedule_expr: Some("1h"),
        timezone: None,
    };
    assert!(db.update_task("t1", &update).expect("update"));

    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.name, "new name");
    assert_eq!(task.prompt, "old prompt");
    assert_eq!(task.schedule_expr, "1h");
}

#[test]
fn update_task_not_found() {
    let db = test_db();
    let update = UpdateTask {
        name: Some("x"),
        prompt: None,
        schedule_type: None,
        schedule_expr: None,
        timezone: None,
    };
    assert!(!db.update_task("nope", &update).expect("update"));
}

#[test]
fn last_task_run_returns_most_recent() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    db.record_task_run("t1", 1000, 500, Some("first"), None)
        .expect("record");
    db.record_task_run("t1", 2000, 300, Some("second"), None)
        .expect("record");

    let run = db.last_task_run("t1").expect("last").expect("some");
    assert_eq!(run.started_at, 2000);
    assert_eq!(run.result.as_deref(), Some("second"));
}

#[test]
fn last_task_run_none_when_no_runs() {
    let db = test_db();
    assert!(db.last_task_run("t1").expect("last").is_none());
}

// ── V14 scheduled task retry/delivery tests ──

#[test]
fn migrate_v14_adds_task_columns() {
    let db = test_db();
    let version = db.get_meta("schema_version").unwrap().unwrap();
    assert_eq!(version, Database::CURRENT_VERSION.to_string());

    // Create a task and verify new columns have defaults
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.max_retries, 3);
    assert_eq!(task.retry_count, 0);
    assert!(task.retry_after.is_none());
    assert!(task.last_error.is_none());
    assert_eq!(task.timeout_ms, 300_000);
    assert!(task.delivery_channel.is_none());
    assert!(task.delivery_target.is_none());
}

#[test]
fn create_task_with_delivery_config() {
    let db = test_db();
    db.create_task(&NewTask {
        id: "t1",
        name: "notify task",
        prompt: "do stuff",
        schedule_type: "interval",
        schedule_expr: "1h",
        timezone: "local",
        next_run: Some(100),
        max_retries: Some(5),
        timeout_ms: Some(60_000),
        delivery_channel: Some("telegram"),
        delivery_target: Some("12345"),
        allowed_tools: None,
        task_type: "prompt",
    })
    .expect("create");
    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.max_retries, 5);
    assert_eq!(task.timeout_ms, 60_000);
    assert_eq!(task.delivery_channel.as_deref(), Some("telegram"));
    assert_eq!(task.delivery_target.as_deref(), Some("12345"));
}

#[test]
fn create_task_with_allowed_tools() {
    let db = test_db();
    db.create_task(&NewTask {
        id: "t-tools",
        name: "restricted task",
        prompt: "check weather",
        schedule_type: "interval",
        schedule_expr: "1h",
        timezone: "local",
        next_run: Some(100),
        max_retries: None,
        timeout_ms: None,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: Some("run_shell,read_file"),
        task_type: "prompt",
    })
    .expect("create");
    let task = db.get_task_by_id("t-tools").expect("get").expect("some");
    assert_eq!(task.allowed_tools.as_deref(), Some("run_shell,read_file"));
}

#[test]
fn create_task_without_allowed_tools() {
    let db = test_db();
    db.create_task(&simple_task(
        "t-no-tools",
        "open task",
        "do anything",
        "interval",
        "1h",
        Some(100),
    ))
    .expect("create");
    let task = db.get_task_by_id("t-no-tools").expect("get").expect("some");
    assert!(task.allowed_tools.is_none());
}

#[test]
fn allowed_tools_survives_list_tasks() {
    let db = test_db();
    db.create_task(&NewTask {
        id: "t-list",
        name: "listed task",
        prompt: "check stuff",
        schedule_type: "interval",
        schedule_expr: "30m",
        timezone: "local",
        next_run: Some(100),
        max_retries: None,
        timeout_ms: None,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: Some("read_memory,write_memory"),
        task_type: "prompt",
    })
    .expect("create");
    let tasks = db.list_tasks().expect("list");
    let task = tasks.iter().find(|t| t.id == "t-list").expect("find");
    assert_eq!(
        task.allowed_tools.as_deref(),
        Some("read_memory,write_memory")
    );
}

#[test]
fn set_and_clear_task_retry() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "test",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");

    db.set_task_retry("t1", 2, "connection timeout", 9999)
        .expect("set retry");
    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.retry_count, 2);
    assert_eq!(task.retry_after, Some(9999));
    assert_eq!(task.last_error.as_deref(), Some("connection timeout"));

    db.clear_task_retry("t1").expect("clear");
    let task = db.get_task_by_id("t1").expect("get").expect("some");
    assert_eq!(task.retry_count, 0);
    assert!(task.retry_after.is_none());
    assert!(task.last_error.is_none());
}

#[test]
fn get_tasks_pending_retry() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "retry-me",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");
    db.create_task(&simple_task(
        "t2",
        "not-retry",
        "prompt",
        "interval",
        "30m",
        Some(100),
    ))
    .expect("create");

    db.set_task_retry("t1", 1, "timeout", 50).expect("set");

    // t1 has retry_after=50, query with now=60 should find it
    let pending = db.get_tasks_pending_retry(60).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "t1");

    // query with now=40 should find nothing (not yet due)
    let pending = db.get_tasks_pending_retry(40).expect("pending");
    assert!(pending.is_empty());
}

#[test]
fn get_due_tasks_excludes_retry_pending() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "normal",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");
    db.create_task(&simple_task(
        "t2",
        "retrying",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    // t2 is pending retry — should not appear in get_due_tasks
    db.set_task_retry("t2", 1, "error", 9999).expect("set");

    let due = db.get_due_tasks(100).expect("due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "t1");
}

#[test]
fn seed_default_tasks_creates_security_audit() {
    let db = test_db();
    // seed_default_tasks is called during migrate_v15 which runs in test_db(),
    // so the task should already exist
    let task = db
        .get_task_by_id("00000000-0000-4000-8000-5ec041700001")
        .expect("get")
        .expect("task should exist");
    assert_eq!(task.name, "Monthly Security Audit");
    assert_eq!(task.schedule_type, "cron");
    assert_eq!(task.schedule_expr, "0 0 9 1 * *");
    assert_eq!(task.timezone, "local");
    assert_eq!(task.status, "active");
    assert_eq!(task.max_retries, 3);
    assert_eq!(task.timeout_ms, 300_000);
    assert!(task.next_run.is_some());
    assert!(task.prompt.contains("security audit"));
}

#[test]
fn seed_default_tasks_is_idempotent() {
    let db = test_db();
    // Already seeded by migration; call again explicitly
    db.seed_default_tasks().expect("second seed should succeed");
    let tasks = db.list_tasks().expect("list");
    let audit_count = tasks
        .iter()
        .filter(|t| t.id == "00000000-0000-4000-8000-5ec041700001")
        .count();
    assert_eq!(
        audit_count, 1,
        "should have exactly one security audit task"
    );
    let daily_count = tasks
        .iter()
        .filter(|t| t.id == crate::daily_summary::DAILY_SUMMARY_TASK_ID)
        .count();
    assert_eq!(daily_count, 1, "should have exactly one daily summary task");
}

#[test]
fn seed_default_tasks_creates_daily_summary() {
    let db = test_db();
    let task = db
        .get_task_by_id(crate::daily_summary::DAILY_SUMMARY_TASK_ID)
        .expect("get")
        .expect("task should exist");
    assert_eq!(task.name, "Daily Summary");
    assert_eq!(task.schedule_type, "cron");
    assert_eq!(task.schedule_expr, "0 0 9 * * 1-5");
    assert_eq!(task.timezone, "local");
    assert_eq!(task.status, "active");
    assert_eq!(task.max_retries, 3);
    assert_eq!(task.timeout_ms, 300_000);
    assert!(task.next_run.is_some());
    assert!(task.prompt.contains("daily standup"));
}

// ── V18: Atomic claim, status tracking, daemon lock tests ──

#[test]
fn claim_due_tasks_returns_claimed_with_run_id() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    let claimed = db.claim_due_tasks(100).expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].task.id, "t1");
    assert!(claimed[0].run_id > 0);

    // Verify a 'running' task_run row was created
    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "running");
    assert_eq!(runs[0].id, claimed[0].run_id);
}

#[test]
fn claim_due_tasks_is_idempotent() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    let first = db.claim_due_tasks(100).expect("first claim");
    assert_eq!(first.len(), 1);

    // Second claim with same time should return empty (next_run was advanced)
    let second = db.claim_due_tasks(100).expect("second claim");
    assert_eq!(second.len(), 0);
}

#[test]
fn claim_due_tasks_once_marks_completed() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "once-task",
        "prompt",
        "once",
        "",
        Some(50),
    ))
    .expect("create");

    let claimed = db.claim_due_tasks(100).expect("claim");
    assert_eq!(claimed.len(), 1);

    // Task should be marked completed with no next_run
    let task = db.get_task_by_id("t1").expect("get").expect("exists");
    assert_eq!(task.status, "completed");
    assert!(task.next_run.is_none());
}

#[test]
fn complete_task_run_success() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    let run_id = db.start_task_run("t1", 1000).expect("start");
    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs[0].status, "running");

    let updated = db
        .complete_task_run(run_id, 500, Some("result text"), None)
        .expect("complete");
    assert!(updated, "should have updated the run row");

    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs[0].status, "success");
    assert_eq!(runs[0].duration_ms, 500);
    assert_eq!(runs[0].result.as_deref(), Some("result text"));
    assert!(runs[0].error.is_none());
}

#[test]
fn complete_task_run_failure() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    let run_id = db.start_task_run("t1", 1000).expect("start");
    let updated = db
        .complete_task_run(run_id, 200, None, Some("timeout error"))
        .expect("complete");
    assert!(updated);

    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs[0].status, "failed");
    assert_eq!(runs[0].error.as_deref(), Some("timeout error"));
    assert!(runs[0].result.is_none());
}

#[test]
fn recover_stale_runs_marks_failed() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    db.start_task_run("t1", 1000).expect("start");
    db.start_task_run("t1", 2000).expect("start");

    let count = db.recover_stale_runs("Daemon crashed").expect("recover");
    assert_eq!(count, 2);

    let runs = db.task_run_history("t1", 10).expect("history");
    for run in &runs {
        assert_eq!(run.status, "failed");
        assert_eq!(run.error.as_deref(), Some("Daemon crashed"));
    }
}

#[test]
fn recover_stale_runs_ignores_completed() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    // Insert completed runs (not running)
    db.record_task_run("t1", 1000, 500, Some("ok"), None)
        .expect("record");
    db.record_task_run("t1", 2000, 300, None, Some("err"))
        .expect("record");

    let count = db.recover_stale_runs("Daemon crashed").expect("recover");
    assert_eq!(count, 0);
}

#[test]
fn daemon_lock_acquire_release() {
    let db = test_db();
    let now = 1000;

    assert!(db.acquire_daemon_lock(100, now).expect("acquire"));
    db.release_daemon_lock(100).expect("release");

    // After release, different PID can acquire
    assert!(db
        .acquire_daemon_lock(200, now)
        .expect("acquire after release"));
}

#[test]
fn daemon_lock_prevents_duplicate() {
    let db = test_db();
    let now = 1000;

    assert!(db.acquire_daemon_lock(100, now).expect("first acquire"));

    // Different PID with recent heartbeat should fail
    assert!(!db
        .acquire_daemon_lock(200, now + 10)
        .expect("second acquire"));
}

#[test]
fn daemon_lock_stale_takeover() {
    let db = test_db();

    assert!(db.acquire_daemon_lock(100, 1000).expect("first acquire"));

    // 400s later (> 300s staleness threshold), different PID should succeed
    assert!(db.acquire_daemon_lock(200, 1400).expect("stale takeover"));
}

#[test]
fn start_task_run_creates_running_row() {
    let db = test_db();
    db.create_task(&simple_task(
        "t1",
        "task1",
        "prompt",
        "interval",
        "30m",
        Some(50),
    ))
    .expect("create");

    let run_id = db.start_task_run("t1", 5000).expect("start");
    assert!(run_id > 0);

    let runs = db.task_run_history("t1", 10).expect("history");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "running");
    assert_eq!(runs[0].started_at, 5000);
    assert_eq!(runs[0].duration_ms, 0);
}

#[test]
fn migrate_v18_adds_status_and_daemon_lock() {
    let db = test_db();
    let version = db.schema_version().expect("get version");
    assert_eq!(version, Database::CURRENT_VERSION);

    // Verify status column exists on task_runs
    let _run_id = {
        db.create_task(&simple_task(
            "t1",
            "task1",
            "prompt",
            "interval",
            "30m",
            Some(50),
        ))
        .expect("create");
        db.start_task_run("t1", 1000).expect("start")
    };
    let runs = db.task_run_history("t1", 1).expect("history");
    assert_eq!(runs[0].status, "running");

    // Verify daemon_lock table exists
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM daemon_lock", [], |r| r.get(0))
        .expect("daemon_lock table should exist");
    assert_eq!(count, 0);
}

#[test]
fn daemon_lock_held_when_fresh() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    assert!(db.acquire_daemon_lock(1234, now).unwrap());
    assert!(db.is_daemon_lock_held());
}

#[test]
fn daemon_lock_not_held_when_empty() {
    let db = test_db();
    assert!(!db.is_daemon_lock_held());
}

#[test]
fn daemon_lock_not_held_when_stale() {
    let db = test_db();
    let stale_time = chrono::Utc::now().timestamp() - 400; // 400s ago > 300s threshold
    assert!(db.acquire_daemon_lock(1234, stale_time).unwrap());
    assert!(!db.is_daemon_lock_held());
}

#[test]
fn daemon_lock_not_held_after_release() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    assert!(db.acquire_daemon_lock(1234, now).unwrap());
    db.release_daemon_lock(1234).unwrap();
    assert!(!db.is_daemon_lock_held());
}

// ── Daemon lock refresh resilience tests ──

#[test]
fn refresh_daemon_lock_updates_heartbeat() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    assert!(db.acquire_daemon_lock(100, now).unwrap());

    // Refresh should succeed and update heartbeat
    db.refresh_daemon_lock(100, now + 60).unwrap();

    // Verify heartbeat was updated (lock should still be held)
    assert!(db.is_daemon_lock_held());
}

#[test]
fn refresh_daemon_lock_stolen_returns_error() {
    let db = test_db();
    let now = 1000;
    assert!(db.acquire_daemon_lock(100, now).unwrap());

    // Simulate lock theft: manually update PID to a different value
    db.conn
        .execute("UPDATE daemon_lock SET pid = 999 WHERE id = 1", [])
        .unwrap();

    // Refresh with original PID should fail (0 rows matched)
    let result = db.refresh_daemon_lock(100, now + 60);
    assert!(result.is_err(), "refresh should fail when lock is stolen");
    assert!(
        result.unwrap_err().to_string().contains("lock lost"),
        "error should mention lock lost"
    );
}

#[test]
fn refresh_daemon_lock_after_release_returns_error() {
    let db = test_db();
    let now = 1000;
    assert!(db.acquire_daemon_lock(100, now).unwrap());
    db.release_daemon_lock(100).unwrap();

    // Refresh after release should fail (no row to update)
    let result = db.refresh_daemon_lock(100, now + 60);
    assert!(result.is_err(), "refresh should fail after release");
}

// ── Tamper-proof validation tests ──

#[test]
fn v34_seeds_memory_consolidation_tasks() {
    // T7 — running migrations on a fresh DB must seed the nightly and weekly
    // memory consolidation tasks with their fixed UUIDs and correct cron
    // expressions. Without this regression guard, a migration reorder could
    // silently disable nightly consolidation.
    let db = test_db();
    let nightly_id = crate::consolidation::NIGHTLY_CONSOLIDATION_TASK_ID;
    let weekly_id = crate::consolidation::WEEKLY_CONSOLIDATION_TASK_ID;

    let (name, schedule_type, schedule_expr, allowed_tools): (String, String, String, String) = db
        .conn()
        .query_row(
            "SELECT name, schedule_type, schedule_expr, allowed_tools
             FROM scheduled_tasks WHERE id = ?1",
            params![nightly_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("nightly consolidation task must be seeded by V34");
    assert_eq!(schedule_type, "cron");
    assert_eq!(schedule_expr, "0 0 3 * * *");
    assert!(
        name.to_lowercase().contains("nightly"),
        "unexpected nightly task name: {name}"
    );
    assert!(
        allowed_tools.contains("write_memory")
            && allowed_tools.contains("read_memory")
            && allowed_tools.contains("memory_search"),
        "consolidation task must allow memory tools: got {allowed_tools}"
    );

    let (weekly_name, weekly_type, weekly_expr): (String, String, String) = db
        .conn()
        .query_row(
            "SELECT name, schedule_type, schedule_expr
             FROM scheduled_tasks WHERE id = ?1",
            params![weekly_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("weekly consolidation task must be seeded by V34");
    assert_eq!(weekly_type, "cron");
    assert_eq!(weekly_expr, "0 0 4 * * 7");
    assert!(
        weekly_name.to_lowercase().contains("weekly"),
        "unexpected weekly task name: {weekly_name}"
    );
}
