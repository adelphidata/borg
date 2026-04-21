use super::*;

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
