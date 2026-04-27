use super::*;

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
