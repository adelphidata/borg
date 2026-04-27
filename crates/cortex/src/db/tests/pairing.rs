use super::*;

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
