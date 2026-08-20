//! Database workflow integration tests.
//!
//! Tests multi-step operations across DB modules using in-memory SQLite.
//! These catch constraint violations and state corruption that single-operation
//! unit tests miss.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use cortex::db::{Database, NewTask};

mod common;
use common::test_db;

// ── Test: session lifecycle ──

#[tokio::test]
async fn session_lifecycle() {
    let db = test_db().await;
    let now = chrono::Utc::now().timestamp();

    // Create session
    db.upsert_session("sess-1", now, now, 0, "gpt-4", "Test session")
        .await
        .expect("upsert session");

    // Insert messages (session_id, role, content, tool_calls_json, tool_call_id, timestamp, content_parts_json)
    db.insert_message("sess-1", "user", Some("Hello"), None, None, None, None)
        .await
        .expect("insert user msg");
    db.insert_message(
        "sess-1",
        "assistant",
        Some("Hi there!"),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("insert assistant msg");

    // Update session with token count
    db.upsert_session("sess-1", now, now + 1, 150, "gpt-4", "Test session")
        .await
        .expect("update session");

    // List sessions
    let sessions = db.list_sessions(10).await.expect("list sessions");
    assert!(!sessions.is_empty());
    let sess = sessions.iter().find(|s| s.id == "sess-1").unwrap();
    assert_eq!(sess.total_tokens, 150);
    assert_eq!(sess.model, "gpt-4");

    // Query messages
    let msgs = db
        .load_session_messages("sess-1")
        .await
        .expect("get messages");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
}

// ── Test: task lifecycle with runs ──

#[tokio::test]
async fn task_lifecycle_with_runs() {
    let db = test_db().await;
    let now = chrono::Utc::now().timestamp();

    let task = NewTask {
        id: "task-1",
        name: "Daily check",
        prompt: "Check email",
        schedule_type: "cron",
        schedule_expr: "0 9 * * *",
        timezone: "UTC",
        next_run: Some(now),
        max_retries: Some(3),
        timeout_ms: Some(60_000),
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
        continuity: false,
    };

    db.create_task(&task).await.expect("create task");

    // Task should be due
    let due = db.get_due_tasks(now + 1).await.expect("get due tasks");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "task-1");

    // Record a successful run
    db.record_task_run("task-1", now, 1500, Some("All good"), None)
        .await
        .expect("record success run");

    // Record a failed run
    db.record_task_run("task-1", now + 100, 500, None, Some("timeout"))
        .await
        .expect("record failed run");

    // Check run history (most recent first)
    let runs = db
        .task_run_history("task-1", 10)
        .await
        .expect("task run history");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].started_at, now + 100); // most recent first
    assert!(runs[0].error.is_some());
    assert_eq!(runs[1].started_at, now);
    assert!(runs[1].error.is_none());

    // Update next_run
    db.update_task_next_run("task-1", Some(now + 3600))
        .await
        .expect("update next_run");

    // Should not be due anymore
    let due = db
        .get_due_tasks(now + 1)
        .await
        .expect("get due tasks after update");
    assert_eq!(due.len(), 0);
}

// ── Test: token usage aggregation ──

#[tokio::test]
async fn token_usage_aggregation() {
    let db = test_db().await;

    // Log multiple usage entries (prompt, completion, total, provider, model, cost_usd)
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .await
        .expect("log usage 1");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4", None)
        .await
        .expect("log usage 2");
    db.log_token_usage(50, 25, 75, "anthropic", "claude-3", None)
        .await
        .expect("log usage 3");

    let total = db.monthly_token_total().await.expect("monthly total");
    assert_eq!(total, 525); // 150 + 300 + 75
}

// ── Test: concurrent sessions isolated ──

#[tokio::test]
async fn concurrent_sessions_isolated() {
    let db = test_db().await;
    let now = chrono::Utc::now().timestamp();

    // Create two sessions
    db.upsert_session("sess-a", now, now, 0, "gpt-4", "Session A")
        .await
        .expect("create session A");
    db.upsert_session("sess-b", now, now, 0, "claude-3", "Session B")
        .await
        .expect("create session B");

    // Interleave messages
    db.insert_message("sess-a", "user", Some("Hello A"), None, None, None, None)
        .await
        .expect("msg A1");
    db.insert_message("sess-b", "user", Some("Hello B"), None, None, None, None)
        .await
        .expect("msg B1");
    db.insert_message("sess-a", "assistant", Some("Hi A!"), None, None, None, None)
        .await
        .expect("msg A2");
    db.insert_message("sess-b", "assistant", Some("Hi B!"), None, None, None, None)
        .await
        .expect("msg B2");
    db.insert_message("sess-b", "user", Some("More B"), None, None, None, None)
        .await
        .expect("msg B3");

    // Verify isolation
    let msgs_a = db
        .load_session_messages("sess-a")
        .await
        .expect("get messages A");
    let msgs_b = db
        .load_session_messages("sess-b")
        .await
        .expect("get messages B");

    assert_eq!(msgs_a.len(), 2);
    assert_eq!(msgs_b.len(), 3);

    // Verify no cross-contamination
    assert!(msgs_a
        .iter()
        .all(|m| m.content.as_deref().map_or(true, |c| !c.contains("B"))));
    assert!(msgs_b
        .iter()
        .all(|m| m.content.as_deref().map_or(true, |c| !c.contains("A"))));
}

// ── Test: migration idempotency ──

#[tokio::test]
async fn migration_idempotency() {
    // Use a file-backed temp DB so we can close and reopen the same database
    let tmp = tempfile::NamedTempFile::new().expect("create temp db file");
    let path = tmp.path().to_path_buf();

    // First open — runs all migrations
    {
        let db = Database::open_at(&path).await.expect("open db first time");
        db.upsert_session("sess-x", 1000, 1000, 0, "gpt-4", "Test")
            .await
            .expect("insert data");
    }

    // Second open — migrations must be idempotent (run again on same schema)
    {
        let db = Database::open_at(&path)
            .await
            .expect("init db second time (migrations rerun)");

        // Data from first open should survive
        let sessions = db.list_sessions(10).await.expect("list");
        assert!(
            sessions.iter().any(|s| s.id == "sess-x"),
            "Data should persist across reopens"
        );

        // Should be able to insert new data
        db.upsert_session("sess-y", 2000, 2000, 0, "claude-3", "Test 2")
            .await
            .expect("insert after reopen");
        let sessions = db.list_sessions(10).await.expect("list after insert");
        assert_eq!(sessions.len(), 2);
    }
}

/// A missed fire must reach the surfaces that report task health.
///
/// A `task_runs` row alone is history nobody reads unless they go looking.
/// `last_error` is what the system prompt's `<scheduled_tasks>` snapshot, the
/// heartbeat `task_health` augmenter, and `borg doctor` all read — so without
/// stamping it, a job that quietly stopped firing stays invisible until the
/// user notices its output has been missing for days.
#[tokio::test]
async fn a_missed_run_reaches_the_surfaces_that_report_task_health() {
    let db = Database::test_db().await;
    let now = chrono::Utc::now().timestamp();
    let task = NewTask {
        id: "nightly",
        name: "Nightly summary",
        prompt: "Summarize",
        schedule_type: "interval",
        schedule_expr: "24h",
        timezone: "UTC",
        next_run: Some(now),
        max_retries: Some(0),
        timeout_ms: Some(60_000),
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
        continuity: false,
    };
    db.create_task(&task).await.expect("create task");
    assert!(
        db.tasks_with_last_error()
            .await
            .expect("query")
            .iter()
            .all(|t| t.id != "nightly"),
        "precondition: the task starts healthy"
    );

    db.record_missed_run("nightly", now, "daemon was down")
        .await
        .expect("record missed run");

    let failing = db.tasks_with_last_error().await.expect("query");
    let row = failing
        .iter()
        .find(|t| t.id == "nightly")
        .expect("a missed run must surface on the task row");
    let err = row.last_error.as_deref().unwrap_or_default();
    assert!(
        err.contains("missed"),
        "the reason must say the run was missed, got: {err}"
    );
    assert!(
        err.contains("daemon was down"),
        "the caller's explanation must survive, got: {err}"
    );

    let runs = db.task_run_history("nightly", 5).await.expect("history");
    assert_eq!(
        runs.first().map(|r| r.status.as_str()),
        Some("missed"),
        "the run-history row must still be written too"
    );
}

/// Continuity round-trips through the schema and is opt-in.
///
/// Off by default matters: carrying prior output costs context on every fire,
/// and most scheduled tasks are stateless. A default that silently grew every
/// task's prompt would be a cost regression nobody asked for.
#[tokio::test]
async fn task_continuity_defaults_off_and_round_trips() {
    let db = Database::test_db().await;
    let now = chrono::Utc::now().timestamp();
    let base = NewTask {
        id: "plain",
        name: "Plain",
        prompt: "Summarize",
        schedule_type: "interval",
        schedule_expr: "24h",
        timezone: "UTC",
        next_run: Some(now),
        max_retries: Some(0),
        timeout_ms: Some(60_000),
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
        continuity: false,
    };
    db.create_task(&base).await.expect("create plain");
    db.create_task(&NewTask {
        id: "tracking",
        name: "Tracking",
        continuity: true,
        ..base
    })
    .await
    .expect("create tracking");

    let plain = db
        .get_task_by_id("plain")
        .await
        .expect("read")
        .expect("exists");
    assert!(!plain.continuity, "continuity must be opt-in");
    let tracking = db
        .get_task_by_id("tracking")
        .await
        .expect("read")
        .expect("exists");
    assert!(tracking.continuity, "the flag must survive the round trip");

    db.set_task_continuity("plain", true)
        .await
        .expect("enable continuity");
    assert!(
        db.get_task_by_id("plain")
            .await
            .expect("read")
            .expect("exists")
            .continuity,
        "update must be able to turn it on"
    );

    db.set_task_continuity("tracking", false)
        .await
        .expect("disable continuity");
    assert!(
        !db.get_task_by_id("tracking")
            .await
            .expect("read")
            .expect("exists")
            .continuity,
        "update must be able to turn it off again"
    );
}

/// A database stamped before `continuity` existed must gain the column.
///
/// This is the whole reason `migrate_v2` exists, and it is the one path no
/// other test covers: every other test builds a fresh database from
/// `SCHEMA_SQL`, where the column is already present, so the `ALTER` branch
/// never runs. Without the migration, `continuity` would exist on new
/// installs and be missing on every database created before it — and every
/// query naming the column would fail there while passing here.
#[tokio::test]
async fn an_older_database_gains_the_continuity_column_on_open() {
    let tmp = tempfile::NamedTempFile::new().expect("create temp db file");
    let path = tmp.path().to_path_buf();

    // Build the pre-`continuity` shape: open normally, then drop the column
    // back off and rewind the stamp so the next open looks like an install
    // that predates the change.
    {
        let db = Database::open_at(&path).await.expect("open db");
        db.conn()
            .execute("ALTER TABLE scheduled_tasks DROP COLUMN continuity", ())
            .await
            .expect("drop the column to simulate an older database");
        db.set_meta("schema_version", "1")
            .await
            .expect("rewind the stamp");
    }

    let db = Database::open_at(&path).await.expect("reopen db");

    // The column is back, and a task created through the normal path reads
    // its documented default rather than erroring on a missing column.
    let now = chrono::Utc::now().timestamp();
    db.create_task(&NewTask {
        id: "legacy",
        name: "Legacy",
        prompt: "Summarize",
        schedule_type: "interval",
        schedule_expr: "24h",
        timezone: "UTC",
        next_run: Some(now),
        max_retries: Some(0),
        timeout_ms: Some(60_000),
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
        continuity: false,
    })
    .await
    .expect("an upgraded database must accept a task insert");

    let task = db
        .get_task_by_id("legacy")
        .await
        .expect("read")
        .expect("exists");
    assert!(!task.continuity, "the recovered column must default to off");

    // And the upgrade must be idempotent: opening again must not fail trying
    // to add a column that is already there.
    drop(db);
    let db = Database::open_at(&path).await.expect("third open");
    assert!(
        db.get_task_by_id("legacy").await.expect("read").is_some(),
        "a second upgrade pass must leave the data alone"
    );
}
