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

use rusqlite::Connection;

use borg_core::db::{Database, NewTask};

mod common;
use common::test_db;

// ── Test: session lifecycle ──

#[test]
fn session_lifecycle() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();

    // Create session
    db.upsert_session("sess-1", now, now, 0, "gpt-4", "Test session")
        .expect("upsert session");

    // Insert messages (session_id, role, content, tool_calls_json, tool_call_id, timestamp, content_parts_json)
    db.insert_message("sess-1", "user", Some("Hello"), None, None, None, None)
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
    .expect("insert assistant msg");

    // Update session with token count
    db.upsert_session("sess-1", now, now + 1, 150, "gpt-4", "Test session")
        .expect("update session");

    // List sessions
    let sessions = db.list_sessions(10).expect("list sessions");
    assert!(!sessions.is_empty());
    let sess = sessions.iter().find(|s| s.id == "sess-1").unwrap();
    assert_eq!(sess.total_tokens, 150);
    assert_eq!(sess.model, "gpt-4");

    // Query messages
    let msgs = db.load_session_messages("sess-1").expect("get messages");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
}

// ── Test: task lifecycle with runs ──

#[test]
fn task_lifecycle_with_runs() {
    let db = test_db();
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
    };

    db.create_task(&task).expect("create task");

    // Task should be due
    let due = db.get_due_tasks(now + 1).expect("get due tasks");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "task-1");

    // Record a successful run
    db.record_task_run("task-1", now, 1500, Some("All good"), None)
        .expect("record success run");

    // Record a failed run
    db.record_task_run("task-1", now + 100, 500, None, Some("timeout"))
        .expect("record failed run");

    // Check run history (most recent first)
    let runs = db.task_run_history("task-1", 10).expect("task run history");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].started_at, now + 100); // most recent first
    assert!(runs[0].error.is_some());
    assert_eq!(runs[1].started_at, now);
    assert!(runs[1].error.is_none());

    // Update next_run
    db.update_task_next_run("task-1", Some(now + 3600))
        .expect("update next_run");

    // Should not be due anymore
    let due = db
        .get_due_tasks(now + 1)
        .expect("get due tasks after update");
    assert_eq!(due.len(), 0);
}

// ── Test: token usage aggregation ──

#[test]
fn token_usage_aggregation() {
    let db = test_db();

    // Log multiple usage entries (prompt, completion, total, provider, model, cost_usd)
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .expect("log usage 1");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4", None)
        .expect("log usage 2");
    db.log_token_usage(50, 25, 75, "anthropic", "claude-3", None)
        .expect("log usage 3");

    let total = db.monthly_token_total().expect("monthly total");
    assert_eq!(total, 525); // 150 + 300 + 75
}

// ── Test: concurrent sessions isolated ──

#[test]
fn concurrent_sessions_isolated() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();

    // Create two sessions
    db.upsert_session("sess-a", now, now, 0, "gpt-4", "Session A")
        .expect("create session A");
    db.upsert_session("sess-b", now, now, 0, "claude-3", "Session B")
        .expect("create session B");

    // Interleave messages
    db.insert_message("sess-a", "user", Some("Hello A"), None, None, None, None)
        .expect("msg A1");
    db.insert_message("sess-b", "user", Some("Hello B"), None, None, None, None)
        .expect("msg B1");
    db.insert_message("sess-a", "assistant", Some("Hi A!"), None, None, None, None)
        .expect("msg A2");
    db.insert_message("sess-b", "assistant", Some("Hi B!"), None, None, None, None)
        .expect("msg B2");
    db.insert_message("sess-b", "user", Some("More B"), None, None, None, None)
        .expect("msg B3");

    // Verify isolation
    let msgs_a = db.load_session_messages("sess-a").expect("get messages A");
    let msgs_b = db.load_session_messages("sess-b").expect("get messages B");

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

#[test]
fn migration_idempotency() {
    // Use a file-backed temp DB so we can close and reopen the same database
    let tmp = tempfile::NamedTempFile::new().expect("create temp db file");
    let path = tmp.path().to_str().unwrap().to_string();

    // First open — runs all migrations
    {
        let conn = Connection::open(&path).expect("open db first time");
        let db = Database::from_connection(conn).expect("init db first time");
        db.upsert_session("sess-x", 1000, 1000, 0, "gpt-4", "Test")
            .expect("insert data");
    }

    // Second open — migrations must be idempotent (run again on same schema)
    {
        let conn = Connection::open(&path).expect("open db second time");
        let db = Database::from_connection(conn).expect("init db second time (migrations rerun)");

        // Data from first open should survive
        let sessions = db.list_sessions(10).expect("list");
        assert!(
            sessions.iter().any(|s| s.id == "sess-x"),
            "Data should persist across reopens"
        );

        // Should be able to insert new data
        db.upsert_session("sess-y", 2000, 2000, 0, "claude-3", "Test 2")
            .expect("insert after reopen");
        let sessions = db.list_sessions(10).expect("list after insert");
        assert_eq!(sessions.len(), 2);
    }
}
