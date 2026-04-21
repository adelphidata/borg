use super::*;
use rusqlite::params;

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

// ── V35: messages_fts ──

/// Query `messages_fts` for matching rowids. Helper shared across FTS tests.
fn fts_match_rowids(db: &Database, query: &str) -> Vec<i64> {
    let mut stmt = db
        .conn
        .prepare("SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 ORDER BY rank")
        .expect("prepare fts query");
    stmt.query_map(params![query], |row| row.get::<_, i64>(0))
        .expect("query fts")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("collect fts rows")
}

#[test]
fn v35_fts_insert_trigger_syncs_new_messages() {
    let db = test_db();
    let id = db
        .insert_message(
            "s1",
            "user",
            Some("deploy the frontend to staging"),
            None,
            None,
            None,
            None,
        )
        .expect("insert");

    let hits = fts_match_rowids(&db, "staging");
    assert_eq!(
        hits,
        vec![id],
        "insert trigger must mirror new row into FTS"
    );
}

#[test]
fn v35_fts_update_trigger_replaces_content() {
    let db = test_db();
    let id = db
        .insert_message(
            "s1",
            "user",
            Some("let's talk about apples"),
            None,
            None,
            None,
            None,
        )
        .expect("insert");

    // Update content — old term must disappear, new term must match.
    db.conn
        .execute(
            "UPDATE messages SET content = ?1 WHERE id = ?2",
            params!["let's talk about bananas", id],
        )
        .expect("update");

    assert!(
        fts_match_rowids(&db, "apples").is_empty(),
        "update trigger must remove stale content"
    );
    assert_eq!(fts_match_rowids(&db, "bananas"), vec![id]);
}

#[test]
fn v35_fts_delete_trigger_removes_row() {
    let db = test_db();
    let id = db
        .insert_message(
            "s1",
            "user",
            Some("ephemeral message"),
            None,
            None,
            None,
            None,
        )
        .expect("insert");
    assert_eq!(fts_match_rowids(&db, "ephemeral"), vec![id]);

    db.conn
        .execute("DELETE FROM messages WHERE id = ?1", params![id])
        .expect("delete");
    assert!(
        fts_match_rowids(&db, "ephemeral").is_empty(),
        "delete trigger must drop row from FTS"
    );
}

#[test]
fn v35_fts_bm25_ranks_higher_term_frequency_first() {
    // Two sessions discuss "kubernetes" but one mentions it repeatedly. BM25
    // should surface the repeated one first. Fails if triggers desync or the
    // FTS table loses its content/content_rowid linkage.
    let db = test_db();
    let id1 = db
        .insert_message(
            "sparse",
            "user",
            Some("once we had kubernetes and moved on"),
            None,
            None,
            None,
            None,
        )
        .expect("insert sparse");
    let id2 = db
        .insert_message(
            "dense",
            "user",
            Some("kubernetes kubernetes kubernetes is our stack"),
            None,
            None,
            None,
            None,
        )
        .expect("insert dense");

    let ranked = fts_match_rowids(&db, "kubernetes");
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0], id2, "denser match must rank first under BM25");
    assert_eq!(ranked[1], id1);
}

#[test]
fn v35_migration_backfills_existing_messages() {
    // Simulate the upgrade path: messages already exist, then V35 is applied.
    // The backfill SELECT is the fragile piece — forgetting it silently
    // excludes every pre-migration row from search.
    let db = test_db();
    let id = db
        .insert_message(
            "s1",
            "user",
            Some("pre-existing legacy content about rustaceans"),
            None,
            None,
            None,
            None,
        )
        .expect("insert");

    // Wipe FTS state as if the table didn't exist before V35. Drop triggers
    // too so they don't auto-repopulate; re-apply the full V35 migration.
    db.conn
        .execute_batch(
            "DROP TRIGGER IF EXISTS messages_ai;
             DROP TRIGGER IF EXISTS messages_ad;
             DROP TRIGGER IF EXISTS messages_au;
             DROP TABLE IF EXISTS messages_fts;",
        )
        .expect("reset fts state");

    db.migrate_v35().expect("re-run V35");

    let hits = fts_match_rowids(&db, "rustaceans");
    assert_eq!(
        hits,
        vec![id],
        "V35 must backfill pre-existing messages into FTS"
    );
}

#[test]
fn messages_fts_search_returns_ranked_hits_with_metadata() {
    let db = test_db();
    db.insert_message(
        "session-alpha",
        "user",
        Some("deploy to production tomorrow morning"),
        None,
        None,
        None,
        None,
    )
    .expect("insert");
    db.insert_message(
        "session-beta",
        "assistant",
        Some("staging deploy verified"),
        None,
        None,
        None,
        None,
    )
    .expect("insert");
    db.insert_message(
        "session-gamma",
        "user",
        Some("completely unrelated cat pictures"),
        None,
        None,
        None,
        None,
    )
    .expect("insert");

    let hits = db
        .messages_fts_search("deploy", 10)
        .expect("fts search succeeds");
    // Two messages contain "deploy" as a token; the third doesn't. FTS5
    // default tokenizer treats "deploy" and "deploying" as distinct tokens
    // (no stemming), which is why the inputs above use the bare verb.
    assert_eq!(hits.len(), 2);
    // Metadata is carried through — caller needs session_id + role to render.
    assert!(hits.iter().any(|h| h.session_id == "session-alpha"));
    assert!(hits.iter().any(|h| h.session_id == "session-beta"));
    assert!(hits.iter().all(|h| !h.content.is_empty()));
    // BM25 scores are positive (negated in SQL so higher-relevance = higher score).
    assert!(hits.iter().all(|h| h.score > 0.0));
}

#[test]
fn messages_fts_search_sanitizes_operator_chars() {
    // Query contains raw FTS5 operators. A naive query would error with
    // "unknown special query term" or similar — sanitizer must strip them.
    let db = test_db();
    db.insert_message(
        "s1",
        "user",
        Some("upgrade kubernetes cluster"),
        None,
        None,
        None,
        None,
    )
    .expect("insert");

    let hits = db
        .messages_fts_search(r#"upgrade "kubernetes": (cluster)*"#, 5)
        .expect("sanitized query must not error");
    assert_eq!(hits.len(), 1);
}

#[test]
fn messages_fts_search_empty_query_returns_empty() {
    let db = test_db();
    db.insert_message("s1", "user", Some("hello"), None, None, None, None)
        .unwrap();
    let hits = db.messages_fts_search("   ", 5).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn v35_fts_skips_null_and_empty_content() {
    // Tool-call-only assistant messages have NULL content — they should not
    // appear in FTS search results (nothing to match, just noise).
    let db = test_db();
    let _tool_id = db
        .insert_message("s1", "assistant", None, Some("[]"), None, None, None)
        .expect("insert tool-call");
    let text_id = db
        .insert_message(
            "s1",
            "user",
            Some("search for kubernetes"),
            None,
            None,
            None,
            None,
        )
        .expect("insert text");

    let hits = fts_match_rowids(&db, "kubernetes");
    assert_eq!(hits, vec![text_id]);
}
