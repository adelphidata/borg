//! Append-only triggers regression test.
//!
//! `vitals_events`, `bond_events`, and `evolution_events` each carry a
//! `RAISE(ABORT, ...)` trigger that fires on `UPDATE` and `DELETE`. The
//! invariant is non-negotiable: these tables are the per-domain HMAC chains
//! that the evolution / vitals / bond replays verify against. A successful
//! `UPDATE` or `DELETE` would silently break tamper detection.
//!
//! libsql fires `RAISE(ABORT, ...)` on `CREATE TRIGGER … BEFORE UPDATE …
//! SELECT RAISE(ABORT, …)` without any flag — this test pins that behavior.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use cortex::db::Database;
use libsql::params;

/// Each row is (table_name, message-substring-the-trigger-must-mention).
/// Table-driven so adding a new append-only table is a one-line change.
const APPEND_ONLY_TABLES: &[(&str, &str)] = &[
    ("vitals_events", "vitals_events is append-only"),
    ("bond_events", "bond_events is append-only"),
    ("evolution_events", "evolution_events is append-only"),
];

/// Insert one minimal row into `table`. Bypasses the high-level DB API
/// because that path enforces HMAC chaining + rate limiting — irrelevant
/// for triggering an UPDATE/DELETE test.
async fn seed_one(db: &Database, table: &str) {
    let now = chrono::Utc::now().timestamp();
    let sql = match table {
        "vitals_events" => {
            "INSERT INTO vitals_events
             (category, source, stability_delta, focus_delta, sync_delta,
              growth_delta, happiness_delta, metadata_json, created_at, hmac, prev_hmac)
             VALUES ('telemetry', 'test', 0, 0, 0, 0, 0, NULL, ?1, 'h0', '0')"
        }
        "bond_events" => {
            "INSERT INTO bond_events
             (event_type, score_delta, reason, hmac, prev_hmac, created_at)
             VALUES ('test', 0, '', 'h0', '0', ?1)"
        }
        "evolution_events" => {
            "INSERT INTO evolution_events
             (event_type, xp_delta, archetype, source, metadata_json,
              created_at, hmac, prev_hmac, session_id, pubkey_id)
             VALUES ('test', 0, NULL, 'test', NULL, ?1, 'h0', '0', NULL, NULL)"
        }
        _ => unreachable!("unknown append-only table: {table}"),
    };
    db.conn()
        .execute(sql, params!(now))
        .await
        .unwrap_or_else(|e| panic!("seed insert into {table}: {e}"));
}

/// Assert the error message carries the trigger's `RAISE(ABORT, …)`
/// string or at least the word `ABORT` — the spike showed libsql surfaces
/// `RAISE` payloads verbatim, but we accept the bare word as a fallback in
/// case a future libsql release reformats them.
fn assert_trigger_fired(table: &str, expected_msg: &str, err: libsql::Error) {
    let msg = err.to_string();
    assert!(
        msg.contains(expected_msg) || msg.contains("ABORT"),
        "{table}: expected '{expected_msg}' or 'ABORT' in error, got: {msg}"
    );
}

#[tokio::test]
async fn append_only_tables_reject_updates_and_deletes() {
    for (table, expected_msg) in APPEND_ONLY_TABLES {
        let db = Database::test_db().await;
        seed_one(&db, table).await;

        // Row exists.
        let mut rows = db
            .conn()
            .query(&format!("SELECT COUNT(*) FROM {table}"), ())
            .await
            .unwrap();
        let count_before: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count_before, 1, "{table}: seed insert must succeed");

        // UPDATE must fail.
        let update_err = db
            .conn()
            .execute(
                &format!("UPDATE {table} SET created_at = created_at + 1"),
                (),
            )
            .await
            .expect_err("UPDATE on append-only table must fail");
        assert_trigger_fired(table, expected_msg, update_err);

        // DELETE must fail.
        let delete_err = db
            .conn()
            .execute(&format!("DELETE FROM {table}"), ())
            .await
            .expect_err("DELETE on append-only table must fail");
        assert_trigger_fired(table, expected_msg, delete_err);

        // Row still there: the trigger ABORTed before the write took effect.
        let mut rows = db
            .conn()
            .query(&format!("SELECT COUNT(*) FROM {table}"), ())
            .await
            .unwrap();
        let count_after: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            count_after, 1,
            "{table}: row count must be unchanged after rejected UPDATE/DELETE"
        );
    }
}
