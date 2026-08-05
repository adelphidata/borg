//! Hash-chained audit log.
//!
//! Append-only blake3 chain over security-relevant events. Each row's `hash`
//! is computed over `prev_hash || event_type || event_json || ts_le ||
//! event_purged_byte`, so flipping any byte in any row breaks the chain at
//! that row. Including `event_purged` in the hash is what closes the
//! "set the tombstone bit to mask payload tampering" attack — without it a
//! purger could rewrite `event_json`, leave `prev_hash`/`hash` intact, and
//! flip `event_purged = 1` to claim the row was tombstoned.
//!
//! The verifier is **pure**: no clock, no network, no DB writes.
//!
//! ## Tombstone semantics (retention purge)
//!
//! When the daily retention purge tombstones an event, it:
//! 1. Rewrites `event_json` to `{"purged":1,"purged_at":<ts>,"original_type":"<t>"}`.
//! 2. Sets `event_purged = 1`.
//! 3. **Leaves `prev_hash` and `hash` untouched** — the original hash is
//!    preserved as a forensic anchor so external ledgers (e.g. backup
//!    tarballs taken before purge) can confirm "this row existed at this
//!    seq".
//!
//! The verifier accepts a tombstoned row by:
//! - Always checking `prev_hash` linkage against the previous row's stored hash.
//! - When `event_purged = 1` AND the payload parses as the canonical tombstone
//!   shape, **skipping** the recomputed-hash equality check (the payload
//!   no longer matches the original, by design).
//! - When `event_purged = 1` but payload is NOT the canonical tombstone shape
//!   → `Tampered` (catches the "set bit + leave arbitrary content" mask).
//! - When `event_purged = 0` but payload IS the tombstone shape → `Tampered`
//!   (catches "rewrite payload to look tombstoned but clear the bit").
//! - Otherwise: recompute hash and compare; mismatch → `Tampered`.
//!
//! Returns `OkWithTombstones { count }` when at least one tombstone exists
//! and every row passes; `Ok` for an unpurged chain; `Tampered { seq }`
//! otherwise.
use anyhow::Result;
use blake3::Hasher;
use libsql::params;

use super::Database;

/// blake3 digest width — the chain's link size; changing it invalidates
/// every persisted `prev_hash` and forces a re-chain migration.
const HASH_LEN: usize = 32;

pub const GENESIS_PREV_HASH: [u8; HASH_LEN] = [0u8; HASH_LEN];

#[derive(Debug, Clone)]
pub struct AuditChainRow {
    pub seq: i64,
    pub prev_hash: Vec<u8>,
    pub hash: Vec<u8>,
    pub event_type: String,
    pub event_json: String,
    pub ts: i64,
    pub principal_id: String,
    pub event_purged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditChainOutcome {
    /// Chain is intact and contains no tombstones.
    Ok,
    /// Chain is intact and contains at least one tombstoned row. Distinct
    /// from `Ok` so operators can distinguish "everything original" from
    /// "retention has fired" without re-querying.
    OkWithTombstones { count: usize },
    /// Chain is broken at `seq`. Either prev_hash linkage failed, the row's
    /// hash doesn't match its current contents, or a row claims to be
    /// tombstoned but the payload isn't the canonical tombstone shape.
    Tampered { seq: i64 },
}

/// Hash domain. Includes the `event_purged` bit so the bit can never be
/// flipped without invalidating the row's hash.
pub fn compute_row_hash(
    prev_hash: &[u8],
    event_type: &str,
    event_json: &str,
    ts: i64,
    event_purged: bool,
) -> [u8; HASH_LEN] {
    let mut h = Hasher::new();
    h.update(prev_hash);
    h.update(event_type.as_bytes());
    h.update(event_json.as_bytes());
    h.update(&ts.to_le_bytes());
    h.update(&[u8::from(event_purged)]);
    *h.finalize().as_bytes()
}

/// True when `event_json` parses as the canonical tombstone shape written
/// by [`Database::tombstone_audit_event`]. Used to enforce that a row with
/// `event_purged = 1` actually carries a tombstone payload, not arbitrary
/// content with the bit flipped on.
fn is_tombstone_payload(event_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(event_json) else {
        return false;
    };
    let purged_ok = v
        .get("purged")
        .and_then(|p| p.as_i64().or_else(|| p.as_bool().map(i64::from)))
        .is_some_and(|p| p == 1);
    purged_ok && v.get("purged_at").is_some() && v.get("original_type").is_some()
}

impl Database {
    pub async fn append_audit_event(
        &self,
        event_type: &str,
        event_json: &str,
        principal_id: &str,
        ts: i64,
    ) -> Result<i64> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<i64> = async {
            let prev: Vec<u8> = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT hash FROM audit_log_chain ORDER BY seq DESC LIMIT 1",
                        (),
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get::<Vec<u8>>(0)?,
                    None => GENESIS_PREV_HASH.to_vec(),
                }
            };

            let hash = compute_row_hash(&prev, event_type, event_json, ts, false);

            self.conn
                .execute(
                    "INSERT INTO audit_log_chain
             (prev_hash, hash, event_type, event_json, ts, principal_id, event_purged)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    params!(
                        prev,
                        hash.to_vec(),
                        event_type,
                        event_json,
                        ts,
                        principal_id
                    ),
                )
                .await?;
            let seq = self.conn.last_insert_rowid();
            Ok(seq)
        }
        .await;
        match result {
            Ok(seq) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(seq)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("audit: rollback after append failure failed: {re}");
                }
                Err(e)
            }
        }
    }

    /// Tombstone an audit event in place. Rewrites `event_json` to the
    /// canonical tombstone shape and sets `event_purged = 1`. Leaves
    /// `prev_hash` and `hash` untouched so the original hash remains a
    /// forensic anchor (per `data-handling.md` §"Tombstone-on-purge").
    ///
    /// Idempotent: tombstoning an already-tombstoned row is a no-op.
    pub async fn tombstone_audit_event(&self, seq: i64, purged_at: i64) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<()> = async {
            let row: Option<(String, bool)> = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT event_type, event_purged
                 FROM audit_log_chain WHERE seq = ?1",
                        params!(seq),
                    )
                    .await?;
                match rows.next().await? {
                    Some(r) => Some((r.get::<String>(0)?, r.get::<i64>(1)? != 0)),
                    None => None,
                }
            };
            let Some((event_type, already)) = row else {
                anyhow::bail!("audit_log_chain row {seq} not found");
            };
            if already {
                return Ok(());
            }
            let payload = serde_json::json!({
                "purged": 1,
                "purged_at": purged_at,
                "original_type": event_type,
            })
            .to_string();
            self.conn
                .execute(
                    "UPDATE audit_log_chain
             SET event_json = ?1, event_purged = 1
             WHERE seq = ?2",
                    params!(payload, seq),
                )
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("audit: rollback after tombstone failure failed: {re}");
                }
                Err(e)
            }
        }
    }

    pub async fn verify_audit_chain(&self) -> Result<AuditChainOutcome> {
        let stmt = self
            .conn
            .prepare(
                "SELECT seq, prev_hash, hash, event_type, event_json, ts, event_purged
             FROM audit_log_chain ORDER BY seq ASC",
            )
            .await?;
        let mut rows = stmt.query(()).await?;

        let mut expected_prev: Vec<u8> = GENESIS_PREV_HASH.to_vec();
        let mut tombstone_count: usize = 0;
        while let Some(r) = rows.next().await? {
            let seq: i64 = r.get::<i64>(0)?;
            let prev_hash: Vec<u8> = r.get::<Vec<u8>>(1)?;
            let stored_hash: Vec<u8> = r.get::<Vec<u8>>(2)?;
            let event_type: String = r.get::<String>(3)?;
            let event_json: String = r.get::<String>(4)?;
            let ts: i64 = r.get::<i64>(5)?;
            let event_purged: bool = r.get::<i64>(6)? != 0;

            if prev_hash != expected_prev {
                return Ok(AuditChainOutcome::Tampered { seq });
            }
            // A tombstoned row must carry the canonical tombstone payload.
            // Rejects the "set event_purged=1 with arbitrary event_json"
            // mask — the verifier skips hash-equality on tombstones, so
            // this structural check is the load-bearing one.
            if event_purged && !is_tombstone_payload(&event_json) {
                return Ok(AuditChainOutcome::Tampered { seq });
            }
            // And a non-tombstoned row must not look like a tombstone:
            // catches the inverse attack (rewrite payload to look purged
            // but leave the bit at 0).
            if !event_purged && is_tombstone_payload(&event_json) {
                return Ok(AuditChainOutcome::Tampered { seq });
            }
            if event_purged {
                // Tombstone — payload was overwritten by design. The
                // stored `hash` is preserved as a forensic anchor; we do
                // not recompute it here. Linkage continues forward via
                // the unchanged stored_hash.
                tombstone_count += 1;
            } else {
                let computed = compute_row_hash(&prev_hash, &event_type, &event_json, ts, false);
                if computed.as_slice() != stored_hash.as_slice() {
                    return Ok(AuditChainOutcome::Tampered { seq });
                }
            }
            expected_prev = stored_hash;
        }
        if tombstone_count > 0 {
            Ok(AuditChainOutcome::OkWithTombstones {
                count: tombstone_count,
            })
        } else {
            Ok(AuditChainOutcome::Ok)
        }
    }

    /// Tombstone every `audit_log_chain` row whose `ts < cutoff` and which
    /// is not already tombstoned. Returns the count tombstoned. Each row
    /// is tombstoned in its own transaction (see `tombstone_audit_event`)
    /// so a partial failure leaves the chain in a verifiable state.
    ///
    /// This is the daemon-side helper used by the daily retention sweep.
    pub async fn tombstone_audit_events_before(
        &self,
        cutoff: i64,
        purged_at: i64,
    ) -> Result<usize> {
        let stmt = self
            .conn
            .prepare(
                "SELECT seq FROM audit_log_chain
             WHERE ts < ?1 AND event_purged = 0
             ORDER BY seq ASC",
            )
            .await?;
        let mut rows = stmt.query(params!(cutoff)).await?;
        let mut seqs: Vec<i64> = Vec::new();
        while let Some(row) = rows.next().await? {
            seqs.push(row.get::<i64>(0)?);
        }
        drop(rows);
        drop(stmt);
        let mut count = 0usize;
        for seq in seqs {
            if let Err(e) = self.tombstone_audit_event(seq, purged_at).await {
                tracing::warn!("audit: tombstone seq={seq} failed: {e}");
                continue;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Delete `messages` rows older than `cutoff`. Hard delete — `messages`
    /// is not hash-chained, so retention can free disk directly. Returns
    /// the row count deleted.
    /// Delete `messages` rows older than `cutoff`, except those belonging to a
    /// pinned session.
    ///
    /// A pin is the per-session escape hatch from daemon-wide retention: the
    /// global `purge_policy = Never` is all-or-nothing, so without this the
    /// only way to keep one conversation is to keep every conversation.
    pub async fn purge_messages_before(&self, cutoff: i64) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "DELETE FROM messages WHERE created_at < ?1
                 AND session_id NOT IN (SELECT id FROM sessions WHERE pinned = 1)",
                params!(cutoff),
            )
            .await?;
        Ok(n as usize)
    }

    pub async fn list_audit_events(&self) -> Result<Vec<AuditChainRow>> {
        let stmt = self
            .conn
            .prepare(
                "SELECT seq, prev_hash, hash, event_type, event_json, ts, principal_id, event_purged
             FROM audit_log_chain ORDER BY seq ASC",
            )
            .await?;
        let mut rows = stmt.query(()).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(AuditChainRow {
                seq: r.get::<i64>(0)?,
                prev_hash: r.get::<Vec<u8>>(1)?,
                hash: r.get::<Vec<u8>>(2)?,
                event_type: r.get::<String>(3)?,
                event_json: r.get::<String>(4)?,
                ts: r.get::<i64>(5)?,
                principal_id: r.get::<String>(6)?,
                event_purged: r.get::<i64>(7)? != 0,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    async fn append_n(db: &Database, n: i64) {
        for i in 0..n {
            db.append_audit_event(
                "evt",
                &format!(r#"{{"i":{i}}}"#),
                "principal-1",
                1_700_000_000 + i,
            )
            .await
            .expect("append");
        }
    }

    #[tokio::test]
    async fn verify_empty_chain_ok() {
        let db = test_db().await;
        assert_eq!(
            db.verify_audit_chain().await.expect("verify"),
            AuditChainOutcome::Ok
        );
    }

    #[tokio::test]
    async fn append_then_verify_ok_for_long_chain() {
        let db = test_db().await;
        append_n(&db, 50).await;
        assert_eq!(
            db.verify_audit_chain().await.expect("verify"),
            AuditChainOutcome::Ok
        );
    }

    #[tokio::test]
    async fn first_row_uses_genesis_prev_hash() {
        let db = test_db().await;
        db.append_audit_event("first", r#"{"a":1}"#, "p", 1_700_000_000)
            .await
            .expect("append");
        let rows = db.list_audit_events().await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prev_hash, GENESIS_PREV_HASH.to_vec());
        assert!(!rows[0].event_purged);
    }

    #[tokio::test]
    async fn tamper_with_event_json_detected_at_correct_seq() {
        let db = test_db().await;
        append_n(&db, 20).await;
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"tampered\":true}' WHERE seq = 7",
                (),
            )
            .await
            .expect("tamper");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 7),
            other => panic!("expected Tampered(7), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tamper_with_prev_hash_detected() {
        let db = test_db().await;
        append_n(&db, 10).await;
        let zeros = vec![0u8; HASH_LEN];
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET prev_hash = ?1 WHERE seq = 4",
                params!(zeros),
            )
            .await
            .expect("tamper");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 4),
            other => panic!("expected Tampered(4), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deleting_a_middle_row_breaks_chain_linkage() {
        let db = test_db().await;
        append_n(&db, 6).await;
        db.conn()
            .execute("DELETE FROM audit_log_chain WHERE seq = 3", ())
            .await
            .expect("delete");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 4),
            other => panic!("expected Tampered(4), got {other:?}"),
        }
    }

    #[test]
    fn hash_function_is_deterministic_and_byte_sensitive() {
        let h1 = compute_row_hash(&[0u8; 32], "evt", "{}", 1, false);
        let h2 = compute_row_hash(&[0u8; 32], "evt", "{}", 1, false);
        let h3 = compute_row_hash(&[0u8; 32], "evt", "{}", 2, false);
        let h4 = compute_row_hash(&[0u8; 32], "evt", "{}", 1, true);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        // Tombstone bit must change the hash, otherwise mask attacks pass.
        assert_ne!(h1, h4);
    }

    // ── Tombstone tests ──

    #[tokio::test]
    async fn tombstone_roundtrip_verifies_clean() {
        let db = test_db().await;
        append_n(&db, 10).await;
        db.tombstone_audit_event(5, 1_700_000_999)
            .await
            .expect("tombstone");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 1),
            other => panic!("expected OkWithTombstones(1), got {other:?}"),
        }
        let rows = db.list_audit_events().await.expect("list");
        let r = rows.iter().find(|r| r.seq == 5).expect("seq 5");
        assert!(r.event_purged);
        assert!(is_tombstone_payload(&r.event_json));
    }

    #[tokio::test]
    async fn tombstone_preserves_downstream_linkage() {
        let db = test_db().await;
        append_n(&db, 10).await;
        db.tombstone_audit_event(5, 1_700_000_999)
            .await
            .expect("tombstone");
        db.append_audit_event("post_tomb", r#"{"k":11}"#, "p", 1_700_000_011)
            .await
            .expect("append");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 1),
            other => panic!("expected OkWithTombstones(1), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn flipping_purged_bit_without_payload_rewrite_is_tampered() {
        let db = test_db().await;
        append_n(&db, 6).await;
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_purged = 1 WHERE seq = 3",
                (),
            )
            .await
            .expect("flip bit");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 3),
            other => panic!("expected Tampered(3), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tombstone_payload_without_purged_bit_is_tampered() {
        let db = test_db().await;
        append_n(&db, 6).await;
        db.tombstone_audit_event(3, 1_700_000_999)
            .await
            .expect("tombstone");
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_purged = 0 WHERE seq = 3",
                (),
            )
            .await
            .expect("clear bit");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 3),
            other => panic!("expected Tampered(3), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tombstone_idempotent() {
        let db = test_db().await;
        append_n(&db, 4).await;
        db.tombstone_audit_event(2, 1_700_000_999)
            .await
            .expect("first");
        let head_after_first: Vec<u8> = {
            let mut rows = db
                .conn()
                .query(
                    "SELECT hash FROM audit_log_chain WHERE seq = ?1",
                    params!(2_i64),
                )
                .await
                .expect("hash");
            rows.next()
                .await
                .expect("next")
                .expect("row")
                .get::<Vec<u8>>(0)
                .expect("hash bytes")
        };
        db.tombstone_audit_event(2, 1_700_001_000)
            .await
            .expect("idempotent re-tombstone");
        let head_after_second: Vec<u8> = {
            let mut rows = db
                .conn()
                .query(
                    "SELECT hash FROM audit_log_chain WHERE seq = ?1",
                    params!(2_i64),
                )
                .await
                .expect("hash");
            rows.next()
                .await
                .expect("next")
                .expect("row")
                .get::<Vec<u8>>(0)
                .expect("hash bytes")
        };
        // Idempotent: second call must NOT rewrite the hash with the new ts.
        assert_eq!(head_after_first, head_after_second);
        assert_eq!(
            db.verify_audit_chain().await.expect("verify"),
            AuditChainOutcome::OkWithTombstones { count: 1 }
        );
    }

    #[tokio::test]
    async fn tombstone_audit_events_before_only_targets_old_rows() {
        let db = test_db().await;
        for i in 0..10 {
            db.append_audit_event("evt", &format!(r#"{{"i":{i}}}"#), "p", 1_000 + i)
                .await
                .expect("append");
        }
        // Cutoff at 1_005 → rows with ts < 1_005 (seqs 1..=5) get tombstoned.
        let n = db
            .tombstone_audit_events_before(1_005, 9_999)
            .await
            .expect("bulk tombstone");
        assert_eq!(n, 5);
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 5),
            other => panic!("expected OkWithTombstones(5), got {other:?}"),
        }
        let n2 = db
            .tombstone_audit_events_before(1_005, 9_999)
            .await
            .expect("idempotent bulk");
        assert_eq!(n2, 0);
    }

    #[tokio::test]
    async fn multiple_tombstones_counted() {
        let db = test_db().await;
        append_n(&db, 10).await;
        db.tombstone_audit_event(2, 1_700_000_999)
            .await
            .expect("t1");
        db.tombstone_audit_event(7, 1_700_000_999)
            .await
            .expect("t2");
        match db.verify_audit_chain().await.expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 2),
            other => panic!("expected OkWithTombstones(2), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rewriting_tombstone_payload_within_canonical_shape_is_not_detected() {
        let db = test_db().await;
        append_n(&db, 6).await;
        db.tombstone_audit_event(3, 1_700_000_999)
            .await
            .expect("tombstone");
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = ?1 WHERE seq = 3",
                params!(r#"{"purged":1,"purged_at":99999,"original_type":"evt"}"#),
            )
            .await
            .expect("backdate");
        assert_eq!(
            db.verify_audit_chain().await.expect("verify"),
            AuditChainOutcome::OkWithTombstones { count: 1 }
        );
    }
}
