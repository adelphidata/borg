use super::*;
use crate::multi_agent::SubAgentStatus;
use rusqlite::params;

fn test_db() -> Database {
    Database::test_db()
}

fn simple_task<'a>(
    id: &'a str,
    name: &'a str,
    prompt: &'a str,
    schedule_type: &'a str,
    schedule_expr: &'a str,
    next_run: Option<i64>,
) -> NewTask<'a> {
    NewTask {
        id,
        name,
        prompt,
        schedule_type,
        schedule_expr,
        timezone: "local",
        next_run,
        max_retries: None,
        timeout_ms: None,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
    }
}

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
    // +4 for seeded tasks (Monthly Security Audit + Daily Summary + Nightly Consolidation + Weekly Maintenance)
    assert_eq!(tasks.len(), 6);
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
fn upsert_session_metadata() {
    let db = test_db();
    db.upsert_session("s1", 100, 100, 500, "gpt-4", "Hello chat")
        .expect("upsert");
    db.upsert_session("s1", 100, 200, 1000, "gpt-4", "Hello chat updated")
        .expect("upsert again");

    let sessions = db.list_sessions(10).expect("list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].total_tokens, 1000);
    assert_eq!(sessions[0].title, "Hello chat updated");
}

#[test]
fn meta_get_set() {
    let db = test_db();
    assert!(db.get_meta("version").expect("get").is_none());

    db.set_meta("version", "1").expect("set");
    assert_eq!(db.get_meta("version").expect("get").as_deref(), Some("1"));

    db.set_meta("version", "2").expect("set again");
    assert_eq!(db.get_meta("version").expect("get").as_deref(), Some("2"));
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

#[test]
fn log_and_query_token_usage() {
    let db = test_db();
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .expect("log usage");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4", None)
        .expect("log usage 2");
    let total = db.monthly_token_total().expect("query");
    assert_eq!(total, 450);
}

#[test]
fn log_token_usage_with_cache_persists_cache_columns() {
    let db = test_db();
    db.log_token_usage_with_cache(
        1000,
        200,
        1200,
        800,
        150,
        "anthropic",
        "claude-sonnet-4",
        None,
    )
    .expect("log with cache");
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 1000);
    assert_eq!(cached, 800);
    assert_eq!(created, 150);
}

#[test]
fn log_token_usage_defaults_cache_columns_to_zero() {
    let db = test_db();
    db.log_token_usage(500, 100, 600, "openai", "gpt-4", None)
        .expect("log plain");
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 500);
    assert_eq!(cached, 0);
    assert_eq!(created, 0);
}

#[test]
fn cache_token_summary_since_empty_returns_zeros() {
    let db = test_db();
    let (prompt, cached, created) = db
        .cache_token_summary_since(0)
        .expect("cache token summary");
    assert_eq!(prompt, 0);
    assert_eq!(cached, 0);
    assert_eq!(created, 0);
}

#[test]
fn migration_v30_adds_cache_columns() {
    // A fresh test_db() has already run all migrations. Verify the new
    // columns exist on token_usage and default to 0 for plain inserts.
    let db = test_db();
    let cached_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('token_usage') WHERE name = 'cached_input_tokens'",
            [],
            |r| r.get(0),
        )
        .expect("pragma");
    let created_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('token_usage') WHERE name = 'cache_creation_tokens'",
            [],
            |r| r.get(0),
        )
        .expect("pragma");
    assert_eq!(cached_exists, 1, "cached_input_tokens column must exist");
    assert_eq!(created_exists, 1, "cache_creation_tokens column must exist");
}

#[test]
fn monthly_token_total_empty_returns_zero() {
    let db = test_db();
    let total = db.monthly_token_total().expect("query");
    assert_eq!(total, 0);
}

#[test]
fn monthly_token_total_excludes_old_entries() {
    let db = test_db();
    // Insert a row with a very old timestamp (year 2020)
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens)
             VALUES (?1, 500, 500, 1000)",
            params![1577836800_i64], // 2020-01-01
        )
        .expect("insert old");
    // Insert a current row
    db.log_token_usage(100, 50, 150, "openai", "gpt-4", None)
        .expect("log current");
    let total = db.monthly_token_total().expect("query");
    // Old entry should be excluded, only current entry counts
    assert_eq!(total, 150);
}

#[test]
fn schema_version_tracking() {
    let db = test_db();
    let version = db.schema_version().expect("get version");
    assert_eq!(version, Database::CURRENT_VERSION);
}

#[test]
fn insert_and_load_messages() {
    let db = test_db();
    db.insert_message(
        "s1",
        "user",
        Some("Hello"),
        None,
        None,
        Some("2026-01-01T00:00:00Z"),
        None,
    )
    .expect("insert user msg");
    db.insert_message(
        "s1",
        "assistant",
        Some("Hi there"),
        None,
        None,
        Some("2026-01-01T00:00:01Z"),
        None,
    )
    .expect("insert assistant msg");
    db.insert_message("s2", "user", Some("Other session"), None, None, None, None)
        .expect("insert other session msg");

    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content.as_deref(), Some("Hello"));
    assert_eq!(msgs[1].role, "assistant");
}

#[test]
fn delete_session_messages() {
    let db = test_db();
    db.insert_message("s1", "user", Some("msg1"), None, None, None, None)
        .expect("insert");
    db.insert_message("s1", "user", Some("msg2"), None, None, None, None)
        .expect("insert");
    let deleted = db.delete_session_messages("s1").expect("delete");
    assert_eq!(deleted, 2);
    let msgs = db.load_session_messages("s1").expect("load");
    assert!(msgs.is_empty());
}

#[test]
fn compact_session_messages_keeps_recent() {
    let db = test_db();
    for i in 0..5 {
        db.insert_message(
            "s1",
            "user",
            Some(&format!("msg{i}")),
            None,
            None,
            None,
            None,
        )
        .expect("insert");
    }
    let deleted = db.compact_session_messages("s1", 2).expect("compact");
    assert_eq!(deleted, 3);
    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content.as_deref(), Some("msg3"));
    assert_eq!(msgs[1].content.as_deref(), Some("msg4"));
}

#[test]
fn compact_session_messages_noop_when_under_threshold() {
    let db = test_db();
    db.insert_message("s1", "user", Some("only one"), None, None, None, None)
        .expect("insert");
    let deleted = db.compact_session_messages("s1", 10).expect("compact");
    assert_eq!(deleted, 0);
    assert_eq!(db.count_session_messages("s1").expect("count"), 1);
}

#[test]
fn delete_last_assistant_turn_removes_tool_chain() {
    let db = test_db();
    // user -> assistant (with tool call) -> tool -> assistant (final)
    db.insert_message("s1", "user", Some("hi"), None, None, None, None)
        .expect("insert");
    db.insert_message(
        "s1",
        "assistant",
        None,
        Some(r#"[{"id":"c1"}]"#),
        None,
        None,
        None,
    )
    .expect("insert");
    db.insert_message("s1", "tool", Some("result"), None, Some("c1"), None, None)
        .expect("insert");
    db.insert_message("s1", "assistant", Some("done"), None, None, None, None)
        .expect("insert");
    let deleted = db.delete_last_assistant_turn("s1").expect("undo");
    assert_eq!(deleted, 3);
    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "user");
}

#[test]
fn delete_last_assistant_turn_noop_when_empty() {
    let db = test_db();
    let deleted = db.delete_last_assistant_turn("s1").expect("undo");
    assert_eq!(deleted, 0);
}

#[test]
fn delete_last_assistant_turn_stops_at_user() {
    let db = test_db();
    // user -> assistant -> user -> assistant
    db.insert_message("s1", "user", Some("q1"), None, None, None, None)
        .expect("insert");
    db.insert_message("s1", "assistant", Some("a1"), None, None, None, None)
        .expect("insert");
    db.insert_message("s1", "user", Some("q2"), None, None, None, None)
        .expect("insert");
    db.insert_message("s1", "assistant", Some("a2"), None, None, None, None)
        .expect("insert");
    let deleted = db.delete_last_assistant_turn("s1").expect("undo");
    assert_eq!(deleted, 1); // only last assistant
    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2].role, "user");
}

#[test]
fn messages_with_tool_calls() {
    let db = test_db();
    let tc_json = r#"[{"id":"c1","type":"function","function":{"name":"test","arguments":"{}"}}]"#;
    db.insert_message("s1", "assistant", None, Some(tc_json), None, None, None)
        .expect("insert");
    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].tool_calls_json.as_deref(), Some(tc_json));
}

#[test]
fn resolve_channel_session_creates_new() {
    let db = test_db();
    let session_id = db
        .resolve_channel_session("slack", "user1")
        .expect("resolve");
    assert!(!session_id.is_empty());
    // UUID v4 format
    assert_eq!(session_id.len(), 36);
}

#[test]
fn resolve_channel_session_returns_existing() {
    let db = test_db();
    let first = db.resolve_channel_session("slack", "user1").expect("first");
    let second = db
        .resolve_channel_session("slack", "user1")
        .expect("second");
    assert_eq!(first, second);
}

#[test]
fn resolve_channel_session_different_senders() {
    let db = test_db();
    let s1 = db.resolve_channel_session("slack", "alice").expect("alice");
    let s2 = db.resolve_channel_session("slack", "bob").expect("bob");
    assert_ne!(s1, s2);
}

/// Two threads from the same sender must resolve to distinct session IDs.
///
/// This mirrors the composite session key built by
/// `crates/gateway/src/handler.rs` (`{sender_id}:{thread_id}`), and guards
/// against regressions where the Slack/Teams/Discord parsers stop populating
/// `InboundMessage.thread_id` — a past bug that collapsed every thread from
/// the same sender into one conversation history.
#[test]
fn resolve_channel_session_isolates_threads_from_same_sender() {
    let db = test_db();
    let root = db.resolve_channel_session("slack", "alice").expect("root");
    let thread_a = db
        .resolve_channel_session("slack", "alice:thread-111")
        .expect("thread_a");
    let thread_b = db
        .resolve_channel_session("slack", "alice:thread-222")
        .expect("thread_b");
    assert_ne!(root, thread_a);
    assert_ne!(root, thread_b);
    assert_ne!(thread_a, thread_b);

    // Re-resolving the same thread key returns the same session (stable).
    let thread_a_again = db
        .resolve_channel_session("slack", "alice:thread-111")
        .expect("thread_a_again");
    assert_eq!(thread_a, thread_a_again);
}

#[test]
fn update_channel_session_id_works() {
    let db = test_db();
    let old_id = db.resolve_channel_session("tg", "u1").expect("resolve");
    let updated = db
        .update_channel_session_id("tg", "u1", "new-session-id")
        .expect("update");
    assert!(updated);
    let current = db.resolve_channel_session("tg", "u1").expect("resolve2");
    assert_eq!(current, "new-session-id");
    assert_ne!(current, old_id);
}

#[test]
fn update_channel_session_id_no_row() {
    let db = test_db();
    let updated = db
        .update_channel_session_id("tg", "nobody", "new-id")
        .expect("update");
    assert!(!updated);
}

#[test]
fn count_session_messages_works() {
    let db = test_db();
    assert_eq!(db.count_session_messages("s1").expect("count"), 0);
    db.insert_message("s1", "user", Some("hi"), None, None, None, None)
        .expect("insert");
    db.insert_message("s1", "assistant", Some("hello"), None, None, None, None)
        .expect("insert");
    assert_eq!(db.count_session_messages("s1").expect("count"), 2);
    assert_eq!(db.count_session_messages("s2").expect("count"), 0);
}

#[test]
fn log_channel_message_and_count() {
    let db = test_db();
    let id1 = db
        .log_channel_message("slack", "user1", "inbound", Some("hello"), None, None)
        .expect("log 1");
    let id2 = db
        .log_channel_message("slack", "user1", "outbound", Some("hi back"), None, None)
        .expect("log 2");
    assert!(id1 > 0);
    assert!(id2 > id1);
}

#[test]
fn insert_message_with_tool_call_id() {
    let db = test_db();
    db.insert_message(
        "s1",
        "tool",
        Some("result data"),
        None,
        Some("call_abc123"),
        None,
        None,
    )
    .expect("insert");
    let msgs = db.load_session_messages("s1").expect("load");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_abc123"));
    assert_eq!(msgs[0].content.as_deref(), Some("result data"));
}

#[test]
fn delete_messages_nonexistent_session() {
    let db = test_db();
    let deleted = db
        .delete_session_messages("no-such-session")
        .expect("delete");
    assert_eq!(deleted, 0);
}

#[test]
fn insert_and_list_plugins() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert");
    let list = db.list_plugins().expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "email/gmail"); // ordered by category, name
    assert_eq!(list[1].id, "messaging/telegram");
}

#[test]
fn delete_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    assert!(db.delete_plugin("messaging/telegram").expect("delete"));
    assert!(!db.delete_plugin("nonexistent").expect("delete missing"));
    let list = db.list_plugins().expect("list");
    assert!(list.is_empty());
}

#[test]
fn set_plugin_verified() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    let list = db.list_plugins().expect("list");
    assert!(list[0].verified_at.is_none());

    db.set_plugin_verified("messaging/telegram")
        .expect("verify");
    let list = db.list_plugins().expect("list");
    assert!(list[0].verified_at.is_some());
}

#[test]
fn insert_and_delete_credentials() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_credential(
        "messaging/telegram",
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");
    let deleted = db
        .delete_credentials_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 1);
}

#[test]
fn credential_cascade_on_plugin_delete() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_credential(
        "messaging/telegram",
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");

    db.delete_plugin("messaging/telegram").expect("delete");
    // Credential should be cascade-deleted
    let deleted = db
        .delete_credentials_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 0);
}

#[test]
fn insert_plugin_replaces_existing() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_plugin("messaging/telegram", "Telegram v2", "channel", "messaging")
        .expect("replace");
    let list = db.list_plugins().expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "Telegram v2");
}

#[test]
fn migrate_v5_creates_file_hashes_table() {
    let db = test_db();
    let mut stmt = db
        .conn
        .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='file_hashes'")
        .expect("prepare");
    let count: i64 = stmt.query_row([], |row| row.get(0)).expect("query");
    assert_eq!(count, 1);
}

#[test]
fn insert_and_get_file_hashes() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc123")
        .expect("insert hash 1");
    db.insert_file_hash("messaging/telegram", "telegram/parse_inbound.py", "def456")
        .expect("insert hash 2");
    db.insert_file_hash("messaging/telegram", "telegram/send_outbound.py", "ghi789")
        .expect("insert hash 3");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert_eq!(hashes.len(), 3);
}

#[test]
fn file_hashes_cascade_delete() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc123")
        .expect("insert hash");
    db.delete_plugin("messaging/telegram").expect("delete cust");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert!(hashes.is_empty());
}

#[test]
fn delete_file_hashes_by_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust 1");
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust 2");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc")
        .expect("insert hash 1");
    db.insert_file_hash("email/gmail", "gmail/tool.toml", "def")
        .expect("insert hash 2");
    db.delete_file_hashes("messaging/telegram").expect("delete");
    let t_hashes = db.get_file_hashes("messaging/telegram").expect("get");
    let g_hashes = db.get_file_hashes("email/gmail").expect("get");
    assert!(t_hashes.is_empty());
    assert_eq!(g_hashes.len(), 1);
}

#[test]
fn insert_installed_tool_and_get_plugin_id() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust");
    db.insert_installed_tool("gmail", "Gmail integration", "python", "email/gmail")
        .expect("insert tool");
    let cust_id = db.get_tool_plugin_id("gmail").expect("get");
    assert_eq!(cust_id.as_deref(), Some("email/gmail"));
}

#[test]
fn get_tool_plugin_id_returns_none_for_unknown() {
    let db = test_db();
    let cust_id = db.get_tool_plugin_id("nonexistent").expect("get");
    assert!(cust_id.is_none());
}

#[test]
fn insert_installed_channel_and_get_plugin_id() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "python",
        "messaging/telegram",
        "/webhook/telegram",
    )
    .expect("insert channel");
    let cust_id = db.get_channel_plugin_id("telegram").expect("get");
    assert_eq!(cust_id.as_deref(), Some("messaging/telegram"));
}

#[test]
fn get_channel_plugin_id_returns_none_for_unknown() {
    let db = test_db();
    let cust_id = db.get_channel_plugin_id("nonexistent").expect("get");
    assert!(cust_id.is_none());
}

#[test]
fn file_hash_upsert_on_reinstall() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "old_hash")
        .expect("insert hash");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "new_hash")
        .expect("upsert hash");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].1, "new_hash");
}

#[test]
fn delete_installed_channels_for_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert plugin");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "native",
        "messaging/telegram",
        "/webhook/telegram",
    )
    .expect("insert channel");
    assert_eq!(
        db.get_channel_plugin_id("telegram").unwrap().as_deref(),
        Some("messaging/telegram")
    );
    let deleted = db
        .delete_installed_channels_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 1);
    assert!(db.get_channel_plugin_id("telegram").unwrap().is_none());
}

#[test]
fn delete_installed_tools_for_plugin() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert plugin");
    db.insert_installed_tool("gmail", "Gmail integration", "python", "email/gmail")
        .expect("insert tool");
    assert!(db.get_tool_plugin_id("gmail").unwrap().is_some());
    let deleted = db
        .delete_installed_tools_for("email/gmail")
        .expect("delete");
    assert_eq!(deleted, 1);
    assert!(db.get_tool_plugin_id("gmail").unwrap().is_none());
}

#[test]
fn full_uninstall_leaves_no_artifacts() {
    let db = test_db();
    let plugin_id = "messaging/telegram";

    // Simulate full install
    db.insert_plugin(plugin_id, "Telegram", "channel", "messaging")
        .expect("insert plugin");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "native",
        plugin_id,
        "/webhook/telegram",
    )
    .expect("insert channel");
    db.insert_credential(
        plugin_id,
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");
    db.insert_file_hash(plugin_id, "telegram/channel.toml", "abc123")
        .expect("insert hash");

    // Simulate full uninstall (same operations as TUI handler)
    db.delete_plugin(plugin_id).expect("delete plugin");
    db.delete_installed_channels_for(plugin_id)
        .expect("delete channels");
    db.delete_credentials_for(plugin_id).expect("delete creds");
    db.delete_file_hashes(plugin_id).expect("delete hashes");

    // Verify nothing remains
    assert!(db.list_plugins().unwrap().is_empty());
    assert!(db.get_channel_plugin_id("telegram").unwrap().is_none());
    assert!(db.get_file_hashes(plugin_id).unwrap().is_empty());
    assert_eq!(db.delete_credentials_for(plugin_id).unwrap(), 0);
}

#[test]
fn migrate_v6_creates_delivery_queue() {
    let db = test_db();
    let version: String = db.get_meta("schema_version").unwrap().unwrap_or_default();
    assert_eq!(version, Database::CURRENT_VERSION.to_string());

    // Table should exist
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='delivery_queue'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

fn new_delivery<'a>(
    id: &'a str,
    channel_name: &'a str,
    sender_id: &'a str,
    channel_id: Option<&'a str>,
    payload: &'a str,
    max_retries: i32,
) -> NewDelivery<'a> {
    NewDelivery {
        id,
        channel_name,
        sender_id,
        channel_id,
        session_id: None,
        payload_json: payload,
        max_retries,
    }
}

#[test]
fn delivery_queue_enqueue_and_claim() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery(
        "d1",
        "slack",
        "user1",
        Some("C123"),
        r#"{"text":"hi"}"#,
        3,
    ))
    .unwrap();
    db.enqueue_delivery(&new_delivery(
        "d2",
        "slack",
        "user2",
        None,
        r#"{"text":"bye"}"#,
        3,
    ))
    .unwrap();

    let claimed = db.claim_pending_deliveries(10).unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].id, "d1");
    assert_eq!(claimed[0].channel_name, "slack");
}

#[test]
fn delivery_queue_mark_delivered() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery("d1", "slack", "user1", None, "{}", 3))
        .unwrap();
    let claimed = db.claim_pending_deliveries(10).unwrap();
    assert_eq!(claimed.len(), 1);

    db.mark_delivered("d1").unwrap();

    // Should not be claimable again
    let claimed2 = db.claim_pending_deliveries(10).unwrap();
    assert!(claimed2.is_empty());
}

#[test]
fn delivery_queue_mark_failed_with_retry() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery("d1", "slack", "user1", None, "{}", 3))
        .unwrap();
    let _ = db.claim_pending_deliveries(10).unwrap();

    let future = chrono::Utc::now().timestamp() + 60;
    db.mark_failed("d1", "timeout", Some(future)).unwrap();

    // Should not be claimable yet (next_retry_at is in the future)
    let claimed = db.claim_pending_deliveries(10).unwrap();
    assert!(claimed.is_empty());
}

#[test]
fn mark_failed_no_next_retry_immediately_reclaimable() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery("d1", "slack", "user1", None, "{}", 3))
        .unwrap();
    let _ = db.claim_pending_deliveries(10).unwrap();

    // Mark failed with no next_retry_at (None) -> immediately reclaimable
    db.mark_failed("d1", "transient error", None).unwrap();

    let claimed = db.claim_pending_deliveries(10).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, "d1");
}

#[test]
fn claim_pending_deliveries_respects_limit() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery("d1", "slack", "u1", None, "{}", 3))
        .unwrap();
    db.enqueue_delivery(&new_delivery("d2", "slack", "u2", None, "{}", 3))
        .unwrap();
    db.enqueue_delivery(&new_delivery("d3", "slack", "u3", None, "{}", 3))
        .unwrap();

    let claimed = db.claim_pending_deliveries(2).unwrap();
    assert_eq!(claimed.len(), 2);
}

#[test]
fn load_session_messages_unknown_session_empty() {
    let db = test_db();
    let msgs = db
        .load_session_messages("nonexistent-session-id")
        .expect("load");
    assert!(msgs.is_empty());
}

#[test]
fn list_sessions_ordered_by_most_recent() {
    let db = test_db();
    db.upsert_session("s1", 100, 100, 500, "gpt-4", "First")
        .expect("upsert");
    db.upsert_session("s2", 200, 300, 1000, "gpt-4", "Second")
        .expect("upsert");
    db.upsert_session("s3", 150, 200, 750, "gpt-4", "Third")
        .expect("upsert");

    let sessions = db.list_sessions(10).expect("list");
    assert_eq!(sessions.len(), 3);
    // Most recently updated first
    assert_eq!(sessions[0].id, "s2"); // updated_at = 300
    assert_eq!(sessions[1].id, "s3"); // updated_at = 200
    assert_eq!(sessions[2].id, "s1"); // updated_at = 100
}

#[test]
fn insert_credential_round_trip() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust");
    db.insert_credential(
        "email/gmail",
        "GMAIL_TOKEN",
        "env",
        None,
        Some("GMAIL_TOKEN"),
    )
    .expect("insert cred 1");
    db.insert_credential(
        "email/gmail",
        "GMAIL_SECRET",
        "env",
        None,
        Some("GMAIL_SECRET"),
    )
    .expect("insert cred 2");
    let deleted = db.delete_credentials_for("email/gmail").expect("delete");
    assert_eq!(deleted, 2);
}

#[test]
fn delivery_queue_replay_unfinished() {
    let mut db = test_db();
    db.enqueue_delivery(&new_delivery("d1", "slack", "user1", None, "{}", 3))
        .unwrap();
    let _ = db.claim_pending_deliveries(10).unwrap();

    // d1 is now in_progress
    let reset = db.replay_unfinished().unwrap();
    assert_eq!(reset, 1);

    // Should be claimable again
    let claimed = db.claim_pending_deliveries(10).unwrap();
    assert_eq!(claimed.len(), 1);
}

#[test]
fn v10_migration_creates_embeddings_table() {
    let db = test_db();
    let version = db.get_meta("schema_version").unwrap().unwrap();
    assert_eq!(version, Database::CURRENT_VERSION.to_string());
    // Table should exist
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn upsert_and_get_embedding() {
    let db = test_db();
    let embedding = vec![1.0f32, 2.0, 3.0];
    let bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

    db.upsert_embedding(
        "global",
        "notes.md",
        "hash123",
        &bytes,
        3,
        "text-embedding-3-small",
    )
    .unwrap();

    let row = db.get_embedding("global", "notes.md").unwrap().unwrap();
    assert_eq!(row.filename, "notes.md");
    assert_eq!(row.scope, "global");
    assert_eq!(row.content_hash, "hash123");
    assert_eq!(row.dimension, 3);
    assert_eq!(row.model, "text-embedding-3-small");
    assert_eq!(row.embedding, bytes);
}

#[test]
fn upsert_embedding_updates_on_conflict() {
    let db = test_db();
    let bytes1 = vec![0u8; 12];
    let bytes2 = vec![1u8; 12];

    db.upsert_embedding("global", "notes.md", "hash1", &bytes1, 3, "model-a")
        .unwrap();
    db.upsert_embedding("global", "notes.md", "hash2", &bytes2, 3, "model-b")
        .unwrap();

    let row = db.get_embedding("global", "notes.md").unwrap().unwrap();
    assert_eq!(row.content_hash, "hash2");
    assert_eq!(row.embedding, bytes2);
    assert_eq!(row.model, "model-b");

    // Should still be only one row
    assert_eq!(db.count_embeddings("global").unwrap(), 1);
}

#[test]
fn get_all_embeddings_filters_by_scope() {
    let db = test_db();
    let bytes = vec![0u8; 12];

    db.upsert_embedding("global", "a.md", "h1", &bytes, 3, "m")
        .unwrap();
    db.upsert_embedding("global", "b.md", "h2", &bytes, 3, "m")
        .unwrap();
    db.upsert_embedding("local", "c.md", "h3", &bytes, 3, "m")
        .unwrap();

    let global = db.get_all_embeddings("global").unwrap();
    assert_eq!(global.len(), 2);

    let local = db.get_all_embeddings("local").unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].filename, "c.md");
}

#[test]
fn delete_embedding_works() {
    let db = test_db();
    let bytes = vec![0u8; 12];

    db.upsert_embedding("global", "notes.md", "h1", &bytes, 3, "m")
        .unwrap();
    assert_eq!(db.count_embeddings("global").unwrap(), 1);

    let deleted = db.delete_embedding("global", "notes.md").unwrap();
    assert!(deleted);
    assert_eq!(db.count_embeddings("global").unwrap(), 0);

    // Deleting again returns false
    let deleted = db.delete_embedding("global", "notes.md").unwrap();
    assert!(!deleted);
}

#[test]
fn get_embedding_returns_none_for_missing() {
    let db = test_db();
    let result = db.get_embedding("global", "nonexistent.md").unwrap();
    assert!(result.is_none());
}

#[test]
fn count_embeddings_empty() {
    let db = test_db();
    assert_eq!(db.count_embeddings("global").unwrap(), 0);
}

#[test]
fn monthly_usage_by_model_groups_correctly() {
    let db = test_db();
    db.log_token_usage(
        100,
        50,
        150,
        "openrouter",
        "anthropic/claude-sonnet-4",
        Some(0.00105),
    )
    .expect("log");
    db.log_token_usage(
        200,
        100,
        300,
        "openrouter",
        "anthropic/claude-sonnet-4",
        Some(0.0021),
    )
    .expect("log");
    db.log_token_usage(500, 200, 700, "openai", "gpt-4o", Some(0.00325))
        .expect("log");

    let rows = db.monthly_usage_by_model().expect("query");
    assert_eq!(rows.len(), 2);
    // Ordered by total_tokens DESC
    assert_eq!(rows[0].model, "gpt-4o");
    assert_eq!(rows[0].total_tokens, 700);
    assert_eq!(rows[1].model, "anthropic/claude-sonnet-4");
    assert_eq!(rows[1].total_tokens, 450);
    assert_eq!(rows[1].prompt_tokens, 300);
    assert_eq!(rows[1].completion_tokens, 150);
}

#[test]
fn monthly_total_cost_sums_correctly() {
    let db = test_db();
    db.log_token_usage(100, 50, 150, "openai", "gpt-4o", Some(0.001))
        .expect("log");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4o", Some(0.002))
        .expect("log");

    let cost = db.monthly_total_cost().expect("query");
    assert!((cost.unwrap() - 0.003).abs() < 1e-9);
}

#[test]
fn old_rows_without_provider_handled() {
    let db = test_db();
    // Simulate pre-V11 row with no provider/model/cost
    db.conn
        .execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens)
             VALUES (?1, 100, 50, 150)",
            params![chrono::Utc::now().timestamp()],
        )
        .expect("insert old-style");
    db.log_token_usage(200, 100, 300, "openai", "gpt-4o", Some(0.002))
        .expect("log new");

    let rows = db.monthly_usage_by_model().expect("query");
    assert_eq!(rows.len(), 2);
    // One row with empty provider/model (old), one with real values
    let old_row = rows.iter().find(|r| r.model.is_empty());
    assert!(old_row.is_some());
    assert_eq!(old_row.unwrap().total_tokens, 150);
}

#[test]
fn migrate_v12_creates_memory_chunks() {
    let db = test_db();
    let version = db.schema_version().expect("get version");
    assert_eq!(version, Database::CURRENT_VERSION);
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM memory_chunks", [], |r| r.get(0))
        .expect("memory_chunks table should exist");
    assert_eq!(count, 0);
}

#[test]
fn migrate_v12_creates_fts_table() {
    let db = test_db();
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM memory_chunks_fts", [], |r| r.get(0))
        .expect("FTS table should exist");
    assert_eq!(count, 0);
}

#[test]
fn upsert_and_get_chunks() {
    let db = test_db();
    let chunks = vec![
        ChunkData {
            chunk_index: 0,
            content: "First chunk about Rust programming".into(),
            content_hash: "hash0".into(),
            embedding: Some(vec![0u8; 12]),
            dimension: Some(3),
            model: Some("test-model".into()),
            start_line: Some(1),
            end_line: Some(10),
        },
        ChunkData {
            chunk_index: 1,
            content: "Second chunk about memory systems".into(),
            content_hash: "hash1".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(11),
            end_line: Some(20),
        },
    ];
    db.upsert_chunks("global", "notes.md", &chunks)
        .expect("upsert");
    let loaded = db.get_all_chunks("global", None).expect("get all");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].filename, "notes.md");
    assert_eq!(loaded[0].chunk_index, 0);
    assert_eq!(loaded[1].chunk_index, 1);
}

#[test]
fn upsert_chunks_replaces_existing() {
    let db = test_db();
    let chunks_v1 = vec![ChunkData {
        chunk_index: 0,
        content: "Old content".into(),
        content_hash: "old_hash".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(5),
    }];
    db.upsert_chunks("global", "notes.md", &chunks_v1)
        .expect("v1");

    let chunks_v2 = vec![ChunkData {
        chunk_index: 0,
        content: "New content".into(),
        content_hash: "new_hash".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(8),
    }];
    db.upsert_chunks("global", "notes.md", &chunks_v2)
        .expect("v2");

    let loaded = db.get_all_chunks("global", None).expect("get");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].content, "New content");
}

#[test]
fn fts_search_returns_matching_chunks() {
    let db = test_db();
    let chunks = vec![
        ChunkData {
            chunk_index: 0,
            content: "The quick brown fox jumps over the lazy dog".into(),
            content_hash: "h0".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(1),
            end_line: Some(1),
        },
        ChunkData {
            chunk_index: 1,
            content: "Rust programming language is fast and safe".into(),
            content_hash: "h1".into(),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some(2),
            end_line: Some(2),
        },
    ];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");

    let results = db.fts_search("global", "fox", 10).expect("fts search");
    assert_eq!(results.len(), 1);
    assert!(results[0].0.content.contains("fox"));

    let results2 = db
        .fts_search("global", "Rust programming", 10)
        .expect("fts");
    assert_eq!(results2.len(), 1);
    assert!(results2[0].0.content.contains("Rust"));
}

#[test]
fn fts_search_no_results() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "Hello world".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");
    let results = db.fts_search("global", "nonexistent", 10).expect("fts");
    assert!(results.is_empty());
}

#[test]
fn delete_chunks_for_file_works() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "content".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "a.md", &chunks)
        .expect("upsert a");
    db.upsert_chunks("global", "b.md", &chunks)
        .expect("upsert b");
    assert_eq!(db.get_all_chunks("global", None).unwrap().len(), 2);

    db.delete_chunks_for_file("global", "a.md").expect("delete");
    let remaining = db.get_all_chunks("global", None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].filename, "b.md");
}

#[test]
fn chunks_scoped_isolation() {
    let db = test_db();
    let chunk = vec![ChunkData {
        chunk_index: 0,
        content: "scoped content".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "g.md", &chunk).expect("global");
    db.upsert_chunks("local", "l.md", &chunk).expect("local");

    assert_eq!(db.get_all_chunks("global", None).unwrap().len(), 1);
    assert_eq!(db.get_all_chunks("local", None).unwrap().len(), 1);
}

#[test]
fn fts_triggers_stay_in_sync_after_upsert() {
    let db = test_db();
    let v1 = vec![ChunkData {
        chunk_index: 0,
        content: "alpha beta gamma".into(),
        content_hash: "h1".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &v1).expect("v1");
    assert_eq!(db.fts_search("global", "alpha", 10).unwrap().len(), 1);

    let v2 = vec![ChunkData {
        chunk_index: 0,
        content: "delta epsilon zeta".into(),
        content_hash: "h2".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &v2).expect("v2");

    assert!(db.fts_search("global", "alpha", 10).unwrap().is_empty());
    assert_eq!(db.fts_search("global", "delta", 10).unwrap().len(), 1);
}

// ── Pairing tests ──

#[test]
fn migrate_v13_creates_pairing_tables() {
    let db = test_db();
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM pairing_requests", [], |r| r.get(0))
        .expect("pairing_requests table should exist");
    assert_eq!(count, 0);
    let count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM approved_senders", [], |r| r.get(0))
        .expect("approved_senders table should exist");
    assert_eq!(count, 0);
}

#[test]
fn create_and_find_pairing_request() {
    let db = test_db();
    let id = db
        .create_pairing_request("telegram", "user123", "ABCD1234", None, 3600)
        .expect("create");
    assert!(!id.is_empty());

    let found = db
        .find_pending_pairing("telegram", "ABCD1234")
        .expect("find")
        .expect("should exist");
    assert_eq!(found.channel_name, "telegram");
    assert_eq!(found.sender_id, "user123");
    assert_eq!(found.code, "ABCD1234");
    assert_eq!(found.status, "pending");

    // Not found for wrong channel
    assert!(db
        .find_pending_pairing("slack", "ABCD1234")
        .expect("find")
        .is_none());
}

#[test]
fn find_pending_for_sender_reuses_code() {
    let db = test_db();
    db.create_pairing_request("telegram", "user123", "CODE1111", None, 3600)
        .expect("create");

    let found = db
        .find_pending_for_sender("telegram", "user123")
        .expect("find")
        .expect("should exist");
    assert_eq!(found.code, "CODE1111");
}

#[test]
fn approve_pairing() {
    let db = test_db();
    db.create_pairing_request("telegram", "user456", "WXYZ9876", None, 3600)
        .expect("create");

    let approved = db.approve_pairing("telegram", "WXYZ9876").expect("approve");
    assert_eq!(approved.sender_id, "user456");

    // Sender should now be approved
    assert!(db.is_sender_approved("telegram", "user456").expect("check"));

    // Pending request should be gone
    assert!(db
        .find_pending_pairing("telegram", "WXYZ9876")
        .expect("find")
        .is_none());
}

#[test]
fn approve_nonexistent_code_errors() {
    let db = test_db();
    let result = db.approve_pairing("telegram", "NOCODE");
    assert!(result.is_err());
}

#[test]
fn is_sender_approved_false_by_default() {
    let db = test_db();
    assert!(!db.is_sender_approved("telegram", "nobody").expect("check"));
}

#[test]
fn revoke_sender() {
    let db = test_db();
    db.create_pairing_request("telegram", "user789", "REVO1234", None, 3600)
        .expect("create");
    db.approve_pairing("telegram", "REVO1234").expect("approve");
    assert!(db.is_sender_approved("telegram", "user789").expect("check"));

    assert!(db.revoke_sender("telegram", "user789").expect("revoke"));
    assert!(!db.is_sender_approved("telegram", "user789").expect("check"));

    // Revoking again returns false
    assert!(!db.revoke_sender("telegram", "user789").expect("revoke"));
}

#[test]
fn list_pairings_filters_by_channel() {
    let db = test_db();
    db.create_pairing_request("telegram", "u1", "CODE0001", None, 3600)
        .expect("create");
    db.create_pairing_request("slack", "u2", "CODE0002", None, 3600)
        .expect("create");

    let all = db.list_pairings(None).expect("list");
    assert_eq!(all.len(), 2);

    let tg = db.list_pairings(Some("telegram")).expect("list");
    assert_eq!(tg.len(), 1);
    assert_eq!(tg[0].channel_name, "telegram");

    let sl = db.list_pairings(Some("slack")).expect("list");
    assert_eq!(sl.len(), 1);
    assert_eq!(sl[0].channel_name, "slack");
}

#[test]
fn list_approved_senders_filters_by_channel() {
    let db = test_db();
    db.create_pairing_request("telegram", "u1", "APPR0001", None, 3600)
        .expect("create");
    db.create_pairing_request("slack", "u2", "APPR0002", None, 3600)
        .expect("create");
    db.approve_pairing("telegram", "APPR0001").expect("approve");
    db.approve_pairing("slack", "APPR0002").expect("approve");

    let all = db.list_approved_senders(None).expect("list");
    assert_eq!(all.len(), 2);

    let tg = db.list_approved_senders(Some("telegram")).expect("list");
    assert_eq!(tg.len(), 1);
    assert_eq!(tg[0].sender_id, "u1");
}

#[test]
fn expired_pairing_not_found() {
    let db = test_db();
    // Create with TTL of 0 — immediately expired
    db.create_pairing_request("telegram", "user_exp", "EXPR1234", None, 0)
        .expect("create");

    // Should not be findable
    assert!(db
        .find_pending_pairing("telegram", "EXPR1234")
        .expect("find")
        .is_none());
    assert!(db
        .find_pending_for_sender("telegram", "user_exp")
        .expect("find")
        .is_none());

    // Cannot approve expired code
    assert!(db.approve_pairing("telegram", "EXPR1234").is_err());
}

#[test]
fn approve_pairing_case_insensitive() {
    let db = test_db();
    db.create_pairing_request("telegram", "user_ci", "ABCD5678", None, 3600)
        .expect("create");

    // Approve with lowercase — should still work
    let approved = db.approve_pairing("telegram", "abcd5678").expect("approve");
    assert_eq!(approved.sender_id, "user_ci");
    assert_eq!(approved.status, "approved");
    assert!(approved.approved_at.is_some());
}

#[test]
fn approve_pairing_returns_updated_status() {
    let db = test_db();
    db.create_pairing_request("telegram", "user_st", "STAT1234", None, 3600)
        .expect("create");

    let approved = db.approve_pairing("telegram", "STAT1234").expect("approve");
    assert_eq!(approved.status, "approved");
    assert!(approved.approved_at.is_some());
}

#[test]
fn cleanup_expired_pairings() {
    let db = test_db();
    // Create one expired (TTL=0) and one valid
    db.create_pairing_request("telegram", "u_exp", "EXP00001", None, 0)
        .expect("create");
    db.create_pairing_request("telegram", "u_valid", "VAL00001", None, 3600)
        .expect("create");

    let cleaned = db.cleanup_expired_pairings().expect("cleanup");
    assert_eq!(cleaned, 1);

    // Valid one should still be findable
    assert!(db
        .find_pending_for_sender("telegram", "u_valid")
        .expect("find")
        .is_some());
}

#[test]
fn duplicate_sender_approval_is_idempotent() {
    let db = test_db();
    db.create_pairing_request("telegram", "u_dup", "DUP00001", None, 3600)
        .expect("create first");
    db.approve_pairing("telegram", "DUP00001")
        .expect("approve first");
    assert!(db.is_sender_approved("telegram", "u_dup").expect("check"));

    // Create a second request and approve it — should update, not duplicate
    db.create_pairing_request("telegram", "u_dup", "DUP00002", None, 3600)
        .expect("create second");
    db.approve_pairing("telegram", "DUP00002")
        .expect("approve second");

    // Still only one approved sender row
    let senders = db.list_approved_senders(Some("telegram")).expect("list");
    let matching: Vec<_> = senders.iter().filter(|s| s.sender_id == "u_dup").collect();
    assert_eq!(matching.len(), 1);
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

#[test]
fn sessions_since_filters_by_time() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    // Recent session
    db.upsert_session("recent", now - 100, now - 50, 100, "m", "Recent")
        .unwrap();
    // Old session
    db.upsert_session("old", now - 200_000, now - 200_000, 100, "m", "Old")
        .unwrap();

    let since = now - 86400;
    let sessions = db.sessions_since(since).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "recent");
}

#[test]
fn get_all_chunks_with_limit() {
    let db = test_db();
    let chunks: Vec<ChunkData> = (0..20)
        .map(|i| ChunkData {
            chunk_index: i,
            content: format!("Chunk number {i}"),
            content_hash: format!("hash_{i}"),
            embedding: None,
            dimension: None,
            model: None,
            start_line: Some((i as i64) * 10 + 1),
            end_line: Some((i as i64 + 1) * 10),
        })
        .collect();
    db.upsert_chunks("global", "big.md", &chunks)
        .expect("upsert");

    // Without limit
    let all = db.get_all_chunks("global", None).expect("get all");
    assert_eq!(all.len(), 20);

    // With limit
    let limited = db.get_all_chunks("global", Some(5)).expect("get limited");
    assert_eq!(limited.len(), 5);

    // Limit larger than actual count
    let over = db.get_all_chunks("global", Some(100)).expect("get over");
    assert_eq!(over.len(), 20);
}

#[test]
fn fts_search_empty_query() {
    let db = test_db();
    let chunks = vec![ChunkData {
        chunk_index: 0,
        content: "Hello world of programming".into(),
        content_hash: "h".into(),
        embedding: None,
        dimension: None,
        model: None,
        start_line: Some(1),
        end_line: Some(1),
    }];
    db.upsert_chunks("global", "test.md", &chunks)
        .expect("upsert");
    // Empty query after sanitization should return empty
    let results = db.fts_search("global", "", 10).expect("fts");
    assert!(results.is_empty());
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

// ── Scripts CRUD tests ──

#[test]
fn scripts_crud() {
    let db = test_db();
    let s = NewScript {
        id: "s1",
        name: "test-script",
        description: "A test script",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "abc123",
        created_at: 1000,
        updated_at: 1000,
    };
    db.create_script(&s).unwrap();

    // get by name
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.name, "test-script");
    assert_eq!(row.description, "A test script");
    assert_eq!(row.runtime, "python");
    assert_eq!(row.hmac, "abc123");
    assert_eq!(row.run_count, 0);
    assert!(!row.network_access);
    assert!(!row.ephemeral);

    // list
    let scripts = db.list_scripts().unwrap();
    assert_eq!(scripts.len(), 1);

    // update hmac
    db.update_script_hmac("s1", "def456", 2000).unwrap();
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.hmac, "def456");
    assert_eq!(row.updated_at, 2000);

    // record run
    db.record_script_run("s1").unwrap();
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.run_count, 1);
    assert!(row.last_run_at.is_some());

    // delete
    db.delete_script("s1").unwrap();
    assert!(db.get_script_by_name("test-script").unwrap().is_none());
}

#[test]
fn scripts_name_uniqueness() {
    let db = test_db();
    let s = NewScript {
        id: "s1",
        name: "dup",
        description: "",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h1",
        created_at: 1000,
        updated_at: 1000,
    };
    db.create_script(&s).unwrap();

    let s2 = NewScript {
        id: "s2",
        name: "dup",
        description: "",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h2",
        created_at: 1000,
        updated_at: 1000,
    };
    assert!(db.create_script(&s2).is_err());
}

#[test]
fn delete_ephemeral_scripts() {
    let db = test_db();
    // Old ephemeral
    db.create_script(&NewScript {
        id: "e1",
        name: "old-ephemeral",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: true,
        hmac: "h",
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();
    // Recent ephemeral
    db.create_script(&NewScript {
        id: "e2",
        name: "new-ephemeral",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: true,
        hmac: "h",
        created_at: 9999,
        updated_at: 9999,
    })
    .unwrap();
    // Non-ephemeral
    db.create_script(&NewScript {
        id: "p1",
        name: "persistent",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h",
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();

    let deleted = db.delete_ephemeral_scripts_older_than(5000).unwrap();
    assert_eq!(deleted, 1);
    let remaining = db.list_scripts().unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|s| s.name == "new-ephemeral"));
    assert!(remaining.iter().any(|s| s.name == "persistent"));
}

#[test]
fn migrate_v19_creates_scripts_table() {
    let db = test_db();
    let version: u32 = db
        .get_meta("schema_version")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(version, Database::CURRENT_VERSION);

    // Verify the scripts table exists
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='scripts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "scripts table should exist after migration");
}

#[test]
fn migrate_v21_creates_indexes() {
    let db = test_db();
    let index_exists = |name: &str| -> bool {
        db.conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    };
    assert!(index_exists("idx_task_runs_task"));
    assert!(index_exists("idx_tasks_due"));
    assert!(index_exists("idx_task_runs_status"));
    assert!(index_exists("idx_scripts_ephemeral"));
}

#[test]
fn auto_vacuum_is_set() {
    let db = test_db();
    let mode: i64 = db
        .conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .expect("auto_vacuum pragma");
    // 2 = INCREMENTAL
    assert_eq!(mode, 2, "auto_vacuum should be INCREMENTAL (2)");
}

// ── Embedding Cache Tests ──

#[test]
fn cache_embedding_round_trip() {
    let db = test_db();
    let data = vec![1u8, 2, 3, 4];
    db.cache_embedding("openai", "text-embedding-3-small", "hash1", &data, 4)
        .unwrap();
    let result = db
        .get_cached_embedding("openai", "text-embedding-3-small", "hash1")
        .unwrap();
    assert!(result.is_some());
    let (embedding, dimension) = result.unwrap();
    assert_eq!(embedding, data);
    assert_eq!(dimension, 4);
}

#[test]
fn get_cached_embedding_returns_none_for_missing() {
    let db = test_db();
    let result = db
        .get_cached_embedding("openai", "text-embedding-3-small", "nonexistent")
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn cache_embedding_upsert_overwrites() {
    let db = test_db();
    db.cache_embedding("openai", "model", "hash1", &[1, 2], 2)
        .unwrap();
    db.cache_embedding("openai", "model", "hash1", &[3, 4, 5], 3)
        .unwrap();
    let (embedding, dimension) = db
        .get_cached_embedding("openai", "model", "hash1")
        .unwrap()
        .unwrap();
    assert_eq!(embedding, vec![3, 4, 5]);
    assert_eq!(dimension, 3);
}

#[test]
fn clear_embedding_cache_deletes_all() {
    let db = test_db();
    db.cache_embedding("p1", "m1", "h1", &[1], 1).unwrap();
    db.cache_embedding("p2", "m2", "h2", &[2], 1).unwrap();
    let deleted = db.clear_embedding_cache().unwrap();
    assert_eq!(deleted, 2);
    assert!(db.get_cached_embedding("p1", "m1", "h1").unwrap().is_none());
}

// ── Session Index Status Tests ──

#[test]
fn session_indexed_round_trip() {
    let db = test_db();
    assert!(!db.is_session_indexed("s1").unwrap());
    db.mark_session_indexed("s1", 10).unwrap();
    assert!(db.is_session_indexed("s1").unwrap());
}

#[test]
fn is_session_indexed_false_for_unknown() {
    let db = test_db();
    assert!(!db.is_session_indexed("nonexistent").unwrap());
}

#[test]
fn get_unindexed_sessions_filters_indexed() {
    let db = test_db();
    db.upsert_session("s1", 100, 100, 0, "m", "t1").unwrap();
    db.upsert_session("s2", 200, 200, 0, "m", "t2").unwrap();
    db.upsert_session("s3", 300, 300, 0, "m", "t3").unwrap();
    db.mark_session_indexed("s2", 5).unwrap();

    let unindexed = db.get_unindexed_sessions(10).unwrap();
    assert_eq!(unindexed.len(), 2);
    assert!(unindexed.contains(&"s1".to_string()));
    assert!(unindexed.contains(&"s3".to_string()));
    assert!(!unindexed.contains(&"s2".to_string()));
}

#[test]
fn get_unindexed_sessions_respects_limit() {
    let db = test_db();
    db.upsert_session("s1", 100, 100, 0, "m", "t1").unwrap();
    db.upsert_session("s2", 200, 200, 0, "m", "t2").unwrap();
    db.upsert_session("s3", 300, 300, 0, "m", "t3").unwrap();

    let unindexed = db.get_unindexed_sessions(2).unwrap();
    assert_eq!(unindexed.len(), 2);
}

// ── Role CRUD Tests ──

#[test]
fn insert_and_get_role_round_trip() {
    let db = test_db();
    // Use a unique name to avoid conflict with seeded builtin roles
    db.insert_role(
        "custom-analyst",
        "Custom analyst role",
        Some("gpt-4"),
        Some("openai"),
        Some(0.5),
        Some("You are an analyst."),
        Some("read_file,run_shell"),
        Some(10),
        false,
    )
    .unwrap();

    let role = db.get_role("custom-analyst").unwrap().unwrap();
    assert_eq!(role.name, "custom-analyst");
    assert_eq!(role.description, "Custom analyst role");
    assert_eq!(role.model.as_deref(), Some("gpt-4"));
    assert_eq!(role.provider.as_deref(), Some("openai"));
    assert!((role.temperature.unwrap() - 0.5).abs() < f32::EPSILON);
    assert_eq!(
        role.system_instructions.as_deref(),
        Some("You are an analyst.")
    );
    assert_eq!(role.tools_allowed.as_deref(), Some("read_file,run_shell"));
    assert_eq!(role.max_iterations, Some(10));
    assert!(!role.is_builtin);
}

#[test]
fn get_role_returns_none_for_unknown() {
    let db = test_db();
    assert!(db.get_role("nonexistent").unwrap().is_none());
}

#[test]
fn list_roles_ordered_by_name() {
    let db = test_db();
    // 3 builtin roles (coder, researcher, writer) are seeded by migrations
    let baseline = db.list_roles().unwrap().len();

    db.insert_role(
        "zeta-custom",
        "Z",
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    db.insert_role(
        "alpha-custom",
        "A",
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();

    let roles = db.list_roles().unwrap();
    assert_eq!(roles.len(), baseline + 2);
    // Verify ordering: alpha-custom should come before zeta-custom
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    let alpha_pos = names.iter().position(|n| *n == "alpha-custom").unwrap();
    let zeta_pos = names.iter().position(|n| *n == "zeta-custom").unwrap();
    assert!(alpha_pos < zeta_pos);
}

#[test]
fn update_role_partial_coalesce() {
    let db = test_db();
    db.insert_role(
        "r1",
        "original",
        Some("gpt-4"),
        None,
        Some(0.7),
        None,
        None,
        None,
        false,
    )
    .unwrap();

    // Update only description and temperature, other fields should remain
    db.update_role(
        "r1",
        Some("updated desc"),
        None,
        None,
        Some(0.3),
        None,
        None,
        None,
    )
    .unwrap();

    let role = db.get_role("r1").unwrap().unwrap();
    assert_eq!(role.description, "updated desc");
    assert_eq!(role.model.as_deref(), Some("gpt-4")); // unchanged
    assert!((role.temperature.unwrap() - 0.3).abs() < f32::EPSILON);
}

#[test]
fn delete_role_returns_true_false() {
    let db = test_db();
    db.insert_role("r1", "test", None, None, None, None, None, None, false)
        .unwrap();
    assert!(db.delete_role("r1").unwrap());
    assert!(!db.delete_role("r1").unwrap()); // already deleted
    assert!(db.get_role("r1").unwrap().is_none());
}

// ── Sub-Agent Run Tests ──

#[test]
fn insert_and_get_sub_agent_run() {
    let db = test_db();
    db.insert_sub_agent_run("run1", "nick", "researcher", "parent-s1", "child-s1", 1)
        .unwrap();

    let run = db.get_sub_agent_run("run1").unwrap().unwrap();
    assert_eq!(run.id, "run1");
    assert_eq!(run.nickname, "nick");
    assert_eq!(run.role, "researcher");
    assert_eq!(run.parent_session_id, "parent-s1");
    assert_eq!(run.session_id, "child-s1");
    assert_eq!(run.depth, 1);
    assert_eq!(run.status, "pending_init");
    assert!(run.result_text.is_none());
    assert!(run.error_text.is_none());
    assert!(run.completed_at.is_none());
}

#[test]
fn get_sub_agent_run_returns_none_for_unknown() {
    let db = test_db();
    assert!(db.get_sub_agent_run("nonexistent").unwrap().is_none());
}

#[test]
fn list_sub_agent_runs_filters_by_parent() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "a", "role", "parent1", "s1", 1)
        .unwrap();
    db.insert_sub_agent_run("r2", "b", "role", "parent1", "s2", 1)
        .unwrap();
    db.insert_sub_agent_run("r3", "c", "role", "parent2", "s3", 1)
        .unwrap();

    let runs = db.list_sub_agent_runs("parent1").unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.parent_session_id == "parent1"));

    let runs2 = db.list_sub_agent_runs("parent2").unwrap();
    assert_eq!(runs2.len(), 1);
}

#[test]
fn update_sub_agent_status_sets_completed_at_on_terminal() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();

    // Non-terminal status should not set completed_at
    db.update_sub_agent_status("r1", &SubAgentStatus::Running)
        .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "running");
    assert!(run.completed_at.is_none());

    // Terminal status should set completed_at
    db.update_sub_agent_status(
        "r1",
        &SubAgentStatus::Completed {
            result: "result text".to_string(),
        },
    )
    .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "completed");
    assert!(run.completed_at.is_some());
    assert_eq!(run.result_text.as_deref(), Some("result text"));
}

#[test]
fn update_sub_agent_status_errored_sets_error_text() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();
    db.update_sub_agent_status(
        "r1",
        &SubAgentStatus::Errored {
            error: "something failed".to_string(),
        },
    )
    .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "errored");
    assert!(run.completed_at.is_some());
    assert_eq!(run.error_text.as_deref(), Some("something failed"));
}

#[test]
fn update_sub_agent_status_shutdown_is_terminal() {
    let db = test_db();
    db.insert_sub_agent_run("r1", "nick", "role", "p1", "s1", 1)
        .unwrap();
    db.update_sub_agent_status("r1", &SubAgentStatus::Shutdown)
        .unwrap();
    let run = db.get_sub_agent_run("r1").unwrap().unwrap();
    assert_eq!(run.status, "shutdown");
    assert!(run.completed_at.is_some());
}

#[test]
fn list_sub_agent_runs_empty_for_unknown_parent() {
    let db = test_db();
    let runs = db.list_sub_agent_runs("no-such-parent").unwrap();
    assert!(runs.is_empty());
}

#[test]
fn update_role_preserves_none_fields() {
    let db = test_db();
    db.insert_role("r2", "desc", None, None, None, None, None, None, false)
        .unwrap();
    db.update_role("r2", Some("new desc"), None, None, None, None, None, None)
        .unwrap();
    let role = db.get_role("r2").unwrap().unwrap();
    assert_eq!(role.description, "new desc");
    assert!(role.model.is_none());
    assert!(role.provider.is_none());
    assert!(role.temperature.is_none());
    assert!(role.system_instructions.is_none());
    assert!(role.tools_allowed.is_none());
    assert!(role.max_iterations.is_none());
}

#[test]
fn open_with_custom_timeout() {
    // Verify that a custom busy timeout is accepted
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let db = Database::init_connection(conn, Database::GATEWAY_BUSY_TIMEOUT_MS)
        .expect("init with gateway timeout");
    // Verify the timeout was applied by reading it back
    let timeout: i64 = db
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("read busy_timeout pragma");
    assert_eq!(timeout, Database::GATEWAY_BUSY_TIMEOUT_MS as i64);
}

#[test]
fn default_open_uses_5s_timeout() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let db = Database::init_connection(conn, 5000).expect("init with default timeout");
    let timeout: i64 = db
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("read busy_timeout pragma");
    assert_eq!(timeout, 5000);
}

// ── Vitals DB tests (event-sourced) ──

#[test]
fn vitals_state_baseline_no_events() {
    let db = test_db();
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 40);
    assert_eq!(state.focus, 40);
    assert_eq!(state.sync, 40);
    assert_eq!(state.growth, 40);
    assert_eq!(state.happiness, 40);
}

#[test]
fn record_and_replay_vitals_event() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Creation);
    db.record_vitals_event("creation", "create_tool", &deltas, None)
        .unwrap();
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 41); // 40 + 1
    assert_eq!(state.focus, 40); // 40 + 0
    assert_eq!(state.sync, 40); // 40 + 0
    assert_eq!(state.growth, 41); // 40 + 1
    assert_eq!(state.happiness, 41); // 40 + 1
}

#[test]
fn vitals_events_since_returns_events() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "session_start", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "user_message", &deltas, None)
        .unwrap();
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].category, "interaction");
    assert_eq!(events[0].source, "user_message"); // DESC order
    assert_eq!(events[1].source, "session_start");
}

#[test]
fn vitals_event_ledger_appends() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    for _ in 0..5 {
        db.record_vitals_event("success", "run_shell", &deltas, None)
            .unwrap();
    }
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 5);
    // State replayed from events with source decay (all same source "run_shell"):
    // counts 1-2: full (+1 each), count 3+: floor(1*0.5)=0, so only 2 count
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.stability, 42);
}

#[test]
fn vitals_hmac_chain_integrity() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "a", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "b", &deltas, None)
        .unwrap();
    // Events should have valid HMAC chain
    let events = db.vitals_events_since(0).unwrap();
    assert!(!events[0].hmac.is_empty());
    assert!(!events[1].hmac.is_empty());
    // State should be valid (both events applied)
    let state = db.get_vitals_state().unwrap();
    assert_eq!(state.sync, 42); // 40 + 1 + 1
}

// ── Bond DB Tests ──

#[test]
fn bond_migration_creates_table() {
    let db = test_db();
    // Table should exist after migration
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bond_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn bond_no_events_returns_empty() {
    let db = test_db();
    let events = db.get_all_bond_events().unwrap();
    assert!(events.is_empty());
}

#[test]
fn bond_record_and_read_event() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let hmac = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        "0",
        "tool_success",
        1,
        "run_shell",
        now,
    );
    db.record_bond_event("tool_success", 1, "run_shell", &hmac, "0", now)
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "tool_success");
    assert_eq!(events[0].score_delta, 1);
    assert_eq!(events[0].reason, "run_shell");
    assert_eq!(events[0].hmac, hmac);
    assert_eq!(events[0].prev_hmac, "0");
}

#[test]
fn bond_get_last_hmac() {
    let db = test_db();
    // No events — should return "0"
    let hmac = db.get_last_bond_event_hmac().unwrap();
    assert_eq!(hmac, "0");

    // Add an event
    let now = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "test", now);
    db.record_bond_event("tool_success", 1, "test", &h1, "0", now)
        .unwrap();
    assert_eq!(db.get_last_bond_event_hmac().unwrap(), h1);

    // Add another
    let h2 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h1,
        "creation",
        2,
        "write_memory",
        now + 1,
    );
    db.record_bond_event("creation", 2, "write_memory", &h2, &h1, now + 1)
        .unwrap();
    assert_eq!(db.get_last_bond_event_hmac().unwrap(), h2);
}

#[test]
fn bond_events_since_filters() {
    let db = test_db();
    let now = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "a", now);
    db.record_bond_event("tool_success", 1, "a", &h1, "0", now)
        .unwrap();

    // Events are timestamped at now(), so "since 0" should include everything
    let events = db.bond_events_since(0).unwrap();
    assert_eq!(events.len(), 1);

    // Far future should return nothing
    let events = db
        .bond_events_since(chrono::Utc::now().timestamp() + 9999)
        .unwrap();
    assert!(events.is_empty());
}

#[test]
fn bond_events_recent_limits() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    for i in 0..5 {
        let prev = if i == 0 {
            "0".to_string()
        } else {
            db.get_last_bond_event_hmac().unwrap()
        };
        let ts = base + i;
        let h = crate::bond::compute_event_hmac(
            b"borg-bond-chain-v1",
            &prev,
            "tool_success",
            1,
            "t",
            ts,
        );
        db.record_bond_event("tool_success", 1, "t", &h, &prev, ts)
            .unwrap();
    }

    let events = db.bond_events_recent(3).unwrap();
    assert_eq!(events.len(), 3);
}

#[test]
fn bond_count_events_since() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    let h1 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", "0", "tool_success", 1, "a", base);
    db.record_bond_event("tool_success", 1, "a", &h1, "0", base)
        .unwrap();
    let h2 =
        crate::bond::compute_event_hmac(b"borg-bond-chain-v1", &h1, "creation", 2, "b", base + 1);
    db.record_bond_event("creation", 2, "b", &h2, &h1, base + 1)
        .unwrap();
    let h3 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h2,
        "tool_success",
        1,
        "c",
        base + 2,
    );
    db.record_bond_event("tool_success", 1, "c", &h3, &h2, base + 2)
        .unwrap();

    // All events (empty type = all)
    let total = db.count_bond_events_since(0, "").unwrap();
    assert_eq!(total, 3);

    // Filter by type
    let ts = db.count_bond_events_since(0, "tool_success").unwrap();
    assert_eq!(ts, 2);

    let cr = db.count_bond_events_since(0, "creation").unwrap();
    assert_eq!(cr, 1);
}

#[test]
fn bond_replay_with_db() {
    let db = test_db();
    let base = chrono::Utc::now().timestamp();
    // Record a chain of events
    let h1 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        "0",
        "tool_success",
        1,
        "read_file",
        base,
    );
    db.record_bond_event("tool_success", 1, "read_file", &h1, "0", base)
        .unwrap();
    let h2 = crate::bond::compute_event_hmac(
        b"borg-bond-chain-v1",
        &h1,
        "creation",
        1,
        "write_memory",
        base + 1,
    );
    db.record_bond_event("creation", 1, "write_memory", &h2, &h1, base + 1)
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    let state = crate::bond::replay_events(&events);
    assert!(state.chain_valid);
    // 25 + 1 + 1 = 27
    assert_eq!(state.score, 27);
    assert_eq!(state.level, crate::bond::BondLevel::Emerging);
}

#[test]
fn bond_record_chained_produces_valid_chain() {
    let db = test_db();
    db.record_bond_event_chained("tool_success", 1, "read_file")
        .unwrap();
    db.record_bond_event_chained("creation", 1, "write_memory")
        .unwrap();
    db.record_bond_event_chained("tool_failure", -1, "run_shell")
        .unwrap();

    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 3);

    // Replay should verify the chain is valid (use derived key matching record_bond_event_chained)
    let key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let state = crate::bond::replay_events_with_key(&key, &events);
    assert!(state.chain_valid);
    // 25 + 1 + 1 - 1 = 26
    assert_eq!(state.score, 26);

    // Verify chain linking
    assert_eq!(events[0].prev_hmac, "0");
    assert_eq!(events[1].prev_hmac, events[0].hmac);
    assert_eq!(events[2].prev_hmac, events[1].hmac);
}

#[test]
fn bond_record_rejects_invalid_event_type() {
    let db = test_db();
    let result = db.record_bond_event_chained("custom_exploit", 1, "test");
    assert!(result.is_err());
}

#[test]
fn bond_record_rejects_wrong_delta() {
    let db = test_db();
    let result = db.record_bond_event_chained("tool_success", 99, "test");
    assert!(result.is_err());
    let result = db.record_bond_event_chained("tool_success", 1, "test");
    assert!(result.is_ok());
}

#[test]
fn bond_record_total_hourly_cap() {
    let db = test_db();
    for i in 0..15 {
        let event_type = match i % 6 {
            0 => "tool_success",
            1 => "tool_failure",
            2 => "creation",
            3 => "correction",
            4 => "suggestion_accepted",
            _ => "suggestion_rejected",
        };
        let delta = match event_type {
            "tool_success" | "suggestion_accepted" => 1,
            "tool_failure" | "suggestion_rejected" => -1,
            "creation" => 1,
            "correction" => -2,
            _ => unreachable!(),
        };
        db.record_bond_event_chained(event_type, delta, "test")
            .unwrap();
    }
    // 16th event should be silently dropped (total cap = 15)
    db.record_bond_event_chained("tool_failure", -1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 15);
}

#[test]
fn bond_record_positive_delta_hourly_cap() {
    let db = test_db();
    for _ in 0..8 {
        db.record_bond_event_chained("tool_success", 1, "test")
            .unwrap();
    }
    // 9th positive event should be dropped
    db.record_bond_event_chained("suggestion_accepted", 1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 8);
    // Negative event should still work
    db.record_bond_event_chained("tool_failure", -1, "test")
        .unwrap();
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(events.len(), 9);
}

#[test]
fn bond_count_vitals_events_by_category() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "session_start", &deltas, None)
        .unwrap();
    db.record_vitals_event("interaction", "user_message", &deltas, None)
        .unwrap();
    let corr_deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Correction);
    db.record_vitals_event("correction", "user_message", &corr_deltas, None)
        .unwrap();

    let (corrections, total) = db
        .count_vitals_events_by_category_since(0, "correction")
        .unwrap();
    assert_eq!(corrections, 1);
    assert_eq!(total, 3);

    let (interactions, _) = db
        .count_vitals_events_by_category_since(0, "interaction")
        .unwrap();
    assert_eq!(interactions, 2);
}

// ── Tamper-Proof Hardening Tests ──

#[test]
fn vitals_record_time_rate_limiting() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Correction);
    // Correction cap is 3/hour
    for _ in 0..10 {
        db.record_vitals_event("correction", "test", &deltas, None)
            .unwrap();
    }
    // Only 3 should actually be recorded
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(
        events.len(),
        3,
        "record-time rate limiting should cap at 3 correction events/hour"
    );
}

#[test]
fn bond_record_time_rate_limiting() {
    let db = test_db();
    // creation cap is 3/hour
    for _ in 0..10 {
        db.record_bond_event_chained("creation", 1, "test").unwrap();
    }
    let events = db.get_all_bond_events().unwrap();
    assert_eq!(
        events.len(),
        3,
        "record-time rate limiting should cap at 3 creation events/hour"
    );
}

#[test]
fn evolution_record_time_rate_limiting() {
    let db = test_db();
    // Per-source cap is 5/hour, per-type cap is 15/hour, total cap is 20/hour.
    // With the same source, per-source (5) kicks in first.
    for _ in 0..35 {
        db.record_evolution_event("xp_gain", 3, Some("builder"), "test", None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(
        events.len(),
        5,
        "record-time per-source rate limiting should cap at 5 events/hour from same source"
    );
}

#[test]
fn append_only_triggers_block_update() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .unwrap();

    // UPDATE should be blocked by trigger
    let result = db.conn.execute(
        "UPDATE vitals_events SET category = 'hacked' WHERE id = 1",
        [],
    );
    assert!(result.is_err(), "append-only trigger should prevent UPDATE");
    assert!(
        result.unwrap_err().to_string().contains("append-only"),
        "error message should mention append-only"
    );
}

#[test]
fn append_only_triggers_block_delete() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Interaction);
    db.record_vitals_event("interaction", "test", &deltas, None)
        .unwrap();

    // DELETE should be blocked by trigger
    let result = db
        .conn
        .execute("DELETE FROM vitals_events WHERE id = 1", []);
    assert!(result.is_err(), "append-only trigger should prevent DELETE");
}

#[test]
fn bond_append_only_triggers() {
    let db = test_db();
    db.record_bond_event_chained("tool_success", 1, "test")
        .unwrap();

    let update = db
        .conn
        .execute("UPDATE bond_events SET score_delta = 100 WHERE id = 1", []);
    assert!(update.is_err(), "bond trigger should prevent UPDATE");

    let delete = db.conn.execute("DELETE FROM bond_events WHERE id = 1", []);
    assert!(delete.is_err(), "bond trigger should prevent DELETE");
}

#[test]
fn evolution_append_only_triggers() {
    let db = test_db();
    db.record_evolution_event("xp_gain", 3, Some("builder"), "test", None)
        .unwrap();

    let update = db.conn.execute(
        "UPDATE evolution_events SET xp_delta = 99999 WHERE id = 1",
        [],
    );
    assert!(update.is_err(), "evolution trigger should prevent UPDATE");

    let delete = db
        .conn
        .execute("DELETE FROM evolution_events WHERE id = 1", []);
    assert!(delete.is_err(), "evolution trigger should prevent DELETE");
}

#[test]
fn chain_integrity_verification_healthy() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    db.record_vitals_event("success", "test", &deltas, None)
        .unwrap();
    db.record_bond_event_chained("tool_success", 1, "test")
        .unwrap();
    db.record_evolution_event("xp_gain", 3, Some("ops"), "test", None)
        .unwrap();

    let health = db.verify_event_chains();
    assert!(health.vitals_valid, "vitals chain should be valid");
    assert!(health.bond_valid, "bond chain should be valid");
    assert!(health.evolution_valid, "evolution chain should be valid");
    assert_eq!(health.vitals_count, 1);
    assert_eq!(health.bond_count, 1);
    assert_eq!(health.evolution_count, 1);
}

#[test]
fn chain_integrity_empty_db() {
    let db = test_db();
    let health = db.verify_event_chains();
    assert!(health.vitals_valid, "empty chain should be valid");
    assert!(health.bond_valid, "empty chain should be valid");
    assert!(health.evolution_valid, "empty chain should be valid");
    assert_eq!(health.vitals_count, 0);
}

#[test]
fn per_install_hmac_salt_persists() {
    let db = test_db();
    let salt1 = db.get_meta("hmac_salt").unwrap().unwrap();
    // Create another DB from same connection — salt should be the same
    let salt2 = db.get_meta("hmac_salt").unwrap().unwrap();
    assert_eq!(salt1, salt2, "HMAC salt should persist across reads");
    assert_eq!(salt1.len(), 64, "salt should be 64 hex chars (32 bytes)");
}

#[test]
fn derived_keys_differ_by_domain() {
    let db = test_db();
    let vitals_key = db.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
    let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let evo_key = db.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
    assert_ne!(vitals_key, bond_key, "vitals and bond keys should differ");
    assert_ne!(bond_key, evo_key, "bond and evolution keys should differ");
    assert_ne!(
        vitals_key, evo_key,
        "vitals and evolution keys should differ"
    );
}

#[test]
fn vitals_transaction_atomicity() {
    let db = test_db();
    let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Success);
    // Record two events and verify HMAC chain is valid (proves atomic transactions)
    db.record_vitals_event("success", "a", &deltas, None)
        .unwrap();
    db.record_vitals_event("success", "b", &deltas, None)
        .unwrap();

    let state = db.get_vitals_state().unwrap();
    assert!(
        state.chain_valid,
        "HMAC chain should be valid with transactional writes"
    );
}

#[test]
fn hmac_checkpoint_write_and_read() {
    let db = test_db();
    // No checkpoint initially
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap();
    assert!(cp.is_none());

    // Save a checkpoint
    db.save_hmac_checkpoint("vitals", 42, "prev_abc", "state_hash_123")
        .unwrap();
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap().unwrap();
    assert_eq!(cp.domain, "vitals");
    assert_eq!(cp.event_id, 42);
    assert_eq!(cp.prev_hmac, "prev_abc");
    assert_eq!(cp.state_hash, "state_hash_123");

    // Save another checkpoint — latest should win
    db.save_hmac_checkpoint("vitals", 100, "prev_xyz", "state_hash_456")
        .unwrap();
    let cp = db.load_latest_hmac_checkpoint("vitals").unwrap().unwrap();
    assert_eq!(cp.event_id, 100);

    // Different domain should have its own checkpoints
    let cp_bond = db.load_latest_hmac_checkpoint("bond").unwrap();
    assert!(cp_bond.is_none());
}

#[test]
fn bond_to_evolution_gate_integration() {
    let db = test_db();
    // Record bond events to build score
    for _ in 0..20 {
        db.record_bond_event_chained("tool_success", 1, "test")
            .unwrap();
    }
    // Verify bond state replays correctly
    let bond_events = db.get_all_bond_events().unwrap();
    let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);
    assert!(bond_state.chain_valid);
    // Baseline 40 + 15 (capped per hour) = 55
    assert!(
        bond_state.score >= 30,
        "bond score {} should be >= 30 for stage1 gate",
        bond_state.score
    );

    // Verify evolution state can replay correctly after bond + evolution events
    let evo_state = db.get_evolution_state().unwrap();
    assert!(evo_state.chain_valid);
    assert_eq!(evo_state.stage, crate::evolution::Stage::Base);
}

// ── Activity Log Tests ──

#[test]
fn test_log_and_query_activity() {
    let db = test_db();
    db.log_activity("info", "session", "New session started", None)
        .unwrap();
    db.log_activity("warn", "task", "Task slow", Some("details"))
        .unwrap();
    db.log_activity("error", "agent", "Agent error", None)
        .unwrap();

    let entries = db.query_activity(10, Some("debug"), None).unwrap();
    assert_eq!(entries.len(), 3);
    // All inserted in same second, so ordered by id DESC (most recent first)
    // Verify all entries are present
    let levels: Vec<&str> = entries.iter().map(|e| e.level.as_str()).collect();
    assert!(levels.contains(&"info"));
    assert!(levels.contains(&"warn"));
    assert!(levels.contains(&"error"));
    // Verify detail is preserved
    let warn_entry = entries.iter().find(|e| e.level == "warn").unwrap();
    assert_eq!(warn_entry.detail.as_deref(), Some("details"));
}

#[test]
fn test_activity_level_filtering() {
    let db = test_db();
    db.log_activity("error", "agent", "err", None).unwrap();
    db.log_activity("warn", "task", "wrn", None).unwrap();
    db.log_activity("info", "session", "inf", None).unwrap();
    db.log_activity("debug", "heartbeat", "dbg", None).unwrap();

    // error only
    let entries = db.query_activity(10, Some("error"), None).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].level, "error");

    // warn+ (error, warn)
    let entries = db.query_activity(10, Some("warn"), None).unwrap();
    assert_eq!(entries.len(), 2);

    // info+ (error, warn, info) — default
    let entries = db.query_activity(10, None, None).unwrap();
    assert_eq!(entries.len(), 3);

    // debug+ (all)
    let entries = db.query_activity(10, Some("debug"), None).unwrap();
    assert_eq!(entries.len(), 4);
}

#[test]
fn test_activity_category_filtering() {
    let db = test_db();
    db.log_activity("info", "session", "s1", None).unwrap();
    db.log_activity("info", "task", "t1", None).unwrap();
    db.log_activity("info", "task", "t2", None).unwrap();
    db.log_activity("info", "heartbeat", "h1", None).unwrap();

    let entries = db.query_activity(10, Some("debug"), Some("task")).unwrap();
    assert_eq!(entries.len(), 2);
    for e in &entries {
        assert_eq!(e.category, "task");
    }
}

#[test]
fn test_activity_prune_before() {
    let db = test_db();
    // Insert entries, then manually update created_at for one to be old
    db.log_activity("info", "session", "old entry", None)
        .unwrap();
    db.log_activity("info", "session", "new entry", None)
        .unwrap();

    // Make the first entry very old
    db.conn()
        .execute(
            "UPDATE activity_log SET created_at = 1000 WHERE message = 'old entry'",
            [],
        )
        .unwrap();

    let cutoff = chrono::Utc::now().timestamp() - 86400;
    let pruned = db.prune_activity_before(cutoff).unwrap();
    assert_eq!(pruned, 1);

    let remaining = db.query_activity(10, Some("debug"), None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].message, "new entry");
}

#[test]
fn test_activity_empty_db() {
    let db = test_db();
    let entries = db.query_activity(10, None, None).unwrap();
    assert!(entries.is_empty());
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
fn evolution_rejects_inflated_xp_delta() {
    let db = test_db();
    let result = db.record_evolution_event("xp_gain", 1000, Some("ops"), "test", None);
    assert!(result.is_err(), "should reject xp_delta > MAX_XP_DELTA");
    assert!(
        result.unwrap_err().to_string().contains("invalid xp_delta"),
        "error should mention invalid xp_delta"
    );
}

#[test]
fn evolution_rejects_negative_xp_delta() {
    let db = test_db();
    let result = db.record_evolution_event("xp_gain", -1, Some("ops"), "test", None);
    assert!(result.is_err(), "should reject negative xp_delta");
}

#[test]
fn evolution_rejects_unknown_event_type() {
    let db = test_db();
    let result = db.record_evolution_event("fake_type", 0, None, "test", None);
    assert!(result.is_err(), "should reject unknown event_type");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid evolution event_type"),
        "error should mention invalid event_type"
    );
}

#[test]
fn evolution_rejects_nonzero_delta_for_non_xp_types() {
    let db = test_db();
    // evolution events must have xp_delta = 0
    let result = db.record_evolution_event("evolution", 5, None, "test", None);
    assert!(
        result.is_err(),
        "evolution event should reject nonzero xp_delta"
    );
    // classification events must have xp_delta = 0
    let result = db.record_evolution_event("classification", 1, None, "test", None);
    assert!(
        result.is_err(),
        "classification event should reject nonzero xp_delta"
    );
}

#[test]
fn evolution_accepts_valid_xp_deltas() {
    let db = test_db();
    // xp_gain with 1, 2, 3 should all succeed (up to source rate limit)
    for delta in 1..=3 {
        db.record_evolution_event("xp_gain", delta, Some("ops"), &format!("src{delta}"), None)
            .unwrap();
    }
    // evolution with 0 should succeed
    db.record_evolution_event("evolution", 0, None, "gate_check", None)
        .unwrap();
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(events.len(), 4);
}

#[test]
fn evolution_source_rate_limiting_at_write_time() {
    let db = test_db();
    // Per-source cap is 5/hour
    for _ in 0..10 {
        db.record_evolution_event("xp_gain", 1, Some("ops"), "same_source", None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    assert_eq!(
        events.len(),
        5,
        "per-source write-time cap should limit to 5 events"
    );
}

#[test]
fn evolution_total_rate_limiting_at_write_time() {
    let db = test_db();
    // Total cap is 20/hour. Use unique sources to avoid per-source cap (5).
    for i in 0..30 {
        db.record_evolution_event("xp_gain", 1, Some("ops"), &format!("src{i}"), None)
            .unwrap();
    }
    let events = db.load_all_evolution_events().unwrap();
    // Per-type cap is 15 for xp_gain, and total cap is 20. Per-type (15) kicks in first.
    assert_eq!(
        events.len(),
        15,
        "per-type cap (15) should kick in before total cap (20)"
    );
}

#[test]
fn vitals_rejects_inflated_deltas() {
    let db = test_db();
    let bad_deltas = crate::vitals::StatDeltas {
        stability: 100,
        focus: 0,
        sync: 0,
        growth: 0,
        happiness: 0,
    };
    let result = db.record_vitals_event("interaction", "test", &bad_deltas, None);
    assert!(result.is_err(), "should reject inflated deltas");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("delta validation failed"),
        "error should mention delta validation"
    );
}

#[test]
fn vitals_rejects_unknown_category() {
    let db = test_db();
    let deltas = crate::vitals::StatDeltas::default();
    let result = db.record_vitals_event("hacked", "test", &deltas, None);
    assert!(result.is_err(), "should reject unknown category");
}

#[test]
fn vitals_accepts_correct_deltas_for_each_category() {
    let db = test_db();
    let categories = [
        "interaction",
        "success",
        "failure",
        "correction",
        "creation",
    ];
    for cat in &categories {
        let deltas = crate::vitals::deltas_for(match *cat {
            "interaction" => crate::vitals::EventCategory::Interaction,
            "success" => crate::vitals::EventCategory::Success,
            "failure" => crate::vitals::EventCategory::Failure,
            "correction" => crate::vitals::EventCategory::Correction,
            "creation" => crate::vitals::EventCategory::Creation,
            _ => unreachable!(),
        });
        db.record_vitals_event(cat, "test", &deltas, None).unwrap();
    }
    let events = db.vitals_events_since(0).unwrap();
    assert_eq!(events.len(), 5, "all 5 valid categories should persist");
}

// ── Pending Celebrations ──

#[test]
fn pending_celebrations_round_trip() {
    let db = test_db();
    let payload = r#"{"from_stage":"base","to_stage":"evolved","evolution_name":"Test Borg"}"#;
    db.insert_pending_celebration("evolution", payload).unwrap();

    let pending = db.get_pending_celebrations().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].celebration_type, "evolution");
    assert_eq!(pending[0].payload_json, payload);

    db.mark_celebration_delivered(pending[0].id).unwrap();

    let after = db.get_pending_celebrations().unwrap();
    assert!(after.is_empty(), "should be empty after marking delivered");
}

#[test]
fn no_pending_celebrations_initially() {
    let db = test_db();
    let pending = db.get_pending_celebrations().unwrap();
    assert!(pending.is_empty());
}

#[test]
fn multiple_pending_celebrations_ordered() {
    let db = test_db();
    db.insert_pending_celebration("evolution", r#"{"id":1}"#)
        .unwrap();
    db.insert_pending_celebration("evolution", r#"{"id":2}"#)
        .unwrap();

    let pending = db.get_pending_celebrations().unwrap();
    assert_eq!(pending.len(), 2);
    // Should be ordered by created_at ASC
    assert!(pending[0].created_at <= pending[1].created_at);

    // Mark first delivered, second should remain
    db.mark_celebration_delivered(pending[0].id).unwrap();
    let remaining = db.get_pending_celebrations().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].payload_json, r#"{"id":2}"#);
}
