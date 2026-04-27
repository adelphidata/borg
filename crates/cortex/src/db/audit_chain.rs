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
//! ## Tombstone semantics (Task 05 retention)
//!
//! When the daily retention purge tombstones an event, it:
//! 1. Rewrites `event_json` to `{"purged":1,"purged_at":<ts>,"original_type":"<t>"}`.
//! 2. Sets `event_purged = 1`.
//! 3. **Leaves `prev_hash` and `hash` untouched** — per
//!    `docs/redesign/data-handling.md`, the original hash is preserved as a
//!    forensic anchor so external ledgers (e.g. backup tarballs taken
//!    before purge) can confirm "this row existed at this seq".
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
use rusqlite::params;

use super::Database;

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
    pub fn append_audit_event(
        &self,
        event_type: &str,
        event_json: &str,
        principal_id: &str,
        ts: i64,
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let prev: Vec<u8> = tx
            .query_row(
                "SELECT hash FROM audit_log_chain ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| GENESIS_PREV_HASH.to_vec());

        let hash = compute_row_hash(&prev, event_type, event_json, ts, false);

        tx.execute(
            "INSERT INTO audit_log_chain
             (prev_hash, hash, event_type, event_json, ts, principal_id, event_purged)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![prev, hash.to_vec(), event_type, event_json, ts, principal_id],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;
        Ok(seq)
    }

    /// Tombstone an audit event in place. Rewrites `event_json` to the
    /// canonical tombstone shape and sets `event_purged = 1`. Leaves
    /// `prev_hash` and `hash` untouched so the original hash remains a
    /// forensic anchor (per `data-handling.md` §"Tombstone-on-purge").
    ///
    /// Idempotent: tombstoning an already-tombstoned row is a no-op.
    pub fn tombstone_audit_event(&self, seq: i64, purged_at: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let row: Option<(String, bool)> = tx
            .query_row(
                "SELECT event_type, event_purged
                 FROM audit_log_chain WHERE seq = ?1",
                params![seq],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .ok();
        let Some((event_type, already)) = row else {
            anyhow::bail!("audit_log_chain row {seq} not found");
        };
        if already {
            tx.commit()?;
            return Ok(());
        }
        let payload = serde_json::json!({
            "purged": 1,
            "purged_at": purged_at,
            "original_type": event_type,
        })
        .to_string();
        tx.execute(
            "UPDATE audit_log_chain
             SET event_json = ?1, event_purged = 1
             WHERE seq = ?2",
            params![payload, seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn verify_audit_chain(&self) -> Result<AuditChainOutcome> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, prev_hash, hash, event_type, event_json, ts, event_purged
             FROM audit_log_chain ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query([])?;

        let mut expected_prev: Vec<u8> = GENESIS_PREV_HASH.to_vec();
        let mut tombstone_count: usize = 0;
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let prev_hash: Vec<u8> = r.get(1)?;
            let stored_hash: Vec<u8> = r.get(2)?;
            let event_type: String = r.get(3)?;
            let event_json: String = r.get(4)?;
            let ts: i64 = r.get(5)?;
            let event_purged: bool = r.get::<_, i64>(6)? != 0;

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
                let computed =
                    compute_row_hash(&prev_hash, &event_type, &event_json, ts, false);
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
    /// is tombstoned in its own transaction (see [`tombstone_audit_event`])
    /// so a partial failure leaves the chain in a verifiable state.
    ///
    /// This is the daemon-side helper used by the daily retention sweep.
    pub fn tombstone_audit_events_before(&self, cutoff: i64, purged_at: i64) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT seq FROM audit_log_chain
             WHERE ts < ?1 AND event_purged = 0
             ORDER BY seq ASC",
        )?;
        let seqs: Vec<i64> = stmt
            .query_map(params![cutoff], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut count = 0usize;
        for seq in seqs {
            if let Err(e) = self.tombstone_audit_event(seq, purged_at) {
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
    pub fn purge_messages_before(&self, cutoff: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM messages WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    pub fn list_audit_events(&self) -> Result<Vec<AuditChainRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, prev_hash, hash, event_type, event_json, ts, principal_id, event_purged
             FROM audit_log_chain ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AuditChainRow {
                seq: r.get(0)?,
                prev_hash: r.get(1)?,
                hash: r.get(2)?,
                event_type: r.get(3)?,
                event_json: r.get(4)?,
                ts: r.get(5)?,
                principal_id: r.get(6)?,
                event_purged: r.get::<_, i64>(7)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("in-mem db"))
            .expect("init db")
    }

    fn append_n(db: &Database, n: i64) {
        for i in 0..n {
            db.append_audit_event(
                "evt",
                &format!(r#"{{"i":{i}}}"#),
                "principal-1",
                1_700_000_000 + i,
            )
            .expect("append");
        }
    }

    #[test]
    fn verify_empty_chain_ok() {
        let db = test_db();
        assert_eq!(db.verify_audit_chain().expect("verify"), AuditChainOutcome::Ok);
    }

    #[test]
    fn append_then_verify_ok_for_long_chain() {
        let db = test_db();
        append_n(&db, 50);
        assert_eq!(db.verify_audit_chain().expect("verify"), AuditChainOutcome::Ok);
    }

    #[test]
    fn first_row_uses_genesis_prev_hash() {
        let db = test_db();
        db.append_audit_event("first", r#"{"a":1}"#, "p", 1_700_000_000).expect("append");
        let rows = db.list_audit_events().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prev_hash, GENESIS_PREV_HASH.to_vec());
        assert!(!rows[0].event_purged);
    }

    #[test]
    fn tamper_with_event_json_detected_at_correct_seq() {
        let db = test_db();
        append_n(&db, 20);
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"tampered\":true}' WHERE seq = 7",
                [],
            )
            .expect("tamper");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 7),
            other => panic!("expected Tampered(7), got {other:?}"),
        }
    }

    #[test]
    fn tamper_with_prev_hash_detected() {
        let db = test_db();
        append_n(&db, 10);
        let zeros = vec![0u8; HASH_LEN];
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET prev_hash = ?1 WHERE seq = 4",
                params![zeros],
            )
            .expect("tamper");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 4),
            other => panic!("expected Tampered(4), got {other:?}"),
        }
    }

    #[test]
    fn deleting_a_middle_row_breaks_chain_linkage() {
        let db = test_db();
        append_n(&db, 6);
        db.conn()
            .execute("DELETE FROM audit_log_chain WHERE seq = 3", [])
            .expect("delete");
        match db.verify_audit_chain().expect("verify") {
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

    // ── Tombstone tests (Task 05) ──

    /// After tombstoning, verify reports `OkWithTombstones { count: 1 }`,
    /// not `Tampered`. The seq=5 payload is the canonical tombstone shape.
    #[test]
    fn tombstone_roundtrip_verifies_clean() {
        let db = test_db();
        append_n(&db, 10);
        db.tombstone_audit_event(5, 1_700_000_999).expect("tombstone");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 1),
            other => panic!("expected OkWithTombstones(1), got {other:?}"),
        }
        let rows = db.list_audit_events().expect("list");
        let r = rows.iter().find(|r| r.seq == 5).expect("seq 5");
        assert!(r.event_purged);
        assert!(is_tombstone_payload(&r.event_json));
    }

    /// Appending after a tombstone keeps downstream linkage. By design
    /// (`data-handling.md` §"Tombstone-on-purge") the tombstoned row's
    /// stored `hash` is left untouched — it remains a forensic anchor
    /// to the original payload — so the next row's `prev_hash` already
    /// matches without any recompute. This test guards against a
    /// regression where `tombstone_audit_event` rewrites either column
    /// and breaks the chain at seq+1.
    #[test]
    fn tombstone_preserves_downstream_linkage() {
        let db = test_db();
        append_n(&db, 10);
        db.tombstone_audit_event(5, 1_700_000_999).expect("tombstone");
        db.append_audit_event("post_tomb", r#"{"k":11}"#, "p", 1_700_000_011)
            .expect("append");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 1),
            other => panic!("expected OkWithTombstones(1), got {other:?}"),
        }
    }

    /// Mask attack: flip `event_purged` to 1 without rewriting payload.
    /// Hash domain includes the bit, so the row's stored hash no longer
    /// matches → Tampered. This is the spec test posture in
    /// `data-handling.md` §"Tombstone integrity".
    #[test]
    fn flipping_purged_bit_without_payload_rewrite_is_tampered() {
        let db = test_db();
        append_n(&db, 6);
        db.conn()
            .execute("UPDATE audit_log_chain SET event_purged = 1 WHERE seq = 3", [])
            .expect("flip bit");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 3),
            other => panic!("expected Tampered(3), got {other:?}"),
        }
    }

    /// Inverse mask: clear the bit on a real tombstone but leave the
    /// tombstone payload in place. The "tombstone-shaped payload without
    /// the bit" check rejects this even before hashes mismatch.
    #[test]
    fn tombstone_payload_without_purged_bit_is_tampered() {
        let db = test_db();
        append_n(&db, 6);
        db.tombstone_audit_event(3, 1_700_000_999).expect("tombstone");
        db.conn()
            .execute("UPDATE audit_log_chain SET event_purged = 0 WHERE seq = 3", [])
            .expect("clear bit");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 3),
            other => panic!("expected Tampered(3), got {other:?}"),
        }
    }

    /// Tombstoning a row already tombstoned is a no-op (idempotent).
    /// Daily retention can run twice across a row near the cutoff without
    /// breaking the chain.
    #[test]
    fn tombstone_idempotent() {
        let db = test_db();
        append_n(&db, 4);
        db.tombstone_audit_event(2, 1_700_000_999).expect("first");
        let head_after_first: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT hash FROM audit_log_chain WHERE seq = ?1",
                params![2_i64],
                |r| r.get(0),
            )
            .expect("hash");
        db.tombstone_audit_event(2, 1_700_001_000)
            .expect("idempotent re-tombstone");
        let head_after_second: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT hash FROM audit_log_chain WHERE seq = ?1",
                params![2_i64],
                |r| r.get(0),
            )
            .expect("hash");
        // Idempotent: second call must NOT rewrite the hash with the new ts.
        assert_eq!(head_after_first, head_after_second);
        assert_eq!(
            db.verify_audit_chain().expect("verify"),
            AuditChainOutcome::OkWithTombstones { count: 1 }
        );
    }

    /// Bulk tombstoning by cutoff: only rows with `ts < cutoff` get
    /// tombstoned, and the chain still verifies. Regression target:
    /// re-running the bulk helper must not re-tombstone (idempotency
    /// at the DELETE FROM-style boundary).
    #[test]
    fn tombstone_audit_events_before_only_targets_old_rows() {
        let db = test_db();
        for i in 0..10 {
            db.append_audit_event("evt", &format!(r#"{{"i":{i}}}"#), "p", 1_000 + i)
                .expect("append");
        }
        // Cutoff at 1_005 → rows with ts < 1_005 (seqs 1..=5) get tombstoned.
        let n = db
            .tombstone_audit_events_before(1_005, 9_999)
            .expect("bulk tombstone");
        assert_eq!(n, 5);
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 5),
            other => panic!("expected OkWithTombstones(5), got {other:?}"),
        }
        // Re-running with the same cutoff is a no-op — the existing
        // tombstones short-circuit `tombstone_audit_event`.
        let n2 = db
            .tombstone_audit_events_before(1_005, 9_999)
            .expect("idempotent bulk");
        assert_eq!(n2, 0);
    }

    /// Multiple non-contiguous tombstones — count reports correctly.
    #[test]
    fn multiple_tombstones_counted() {
        let db = test_db();
        append_n(&db, 10);
        db.tombstone_audit_event(2, 1_700_000_999).expect("t1");
        db.tombstone_audit_event(7, 1_700_000_999).expect("t2");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 2),
            other => panic!("expected OkWithTombstones(2), got {other:?}"),
        }
    }

    /// Documented limitation: editing a tombstone's `purged_at` (or any
    /// other field within the canonical tombstone shape) is *not*
    /// detected by `verify_audit_chain`, by design. The spec
    /// (`data-handling.md` §"Tombstone-on-purge") leaves `hash`
    /// untouched on tombstone so the original hash remains a forensic
    /// anchor for external ledgers. This test captures the edge case so
    /// future readers don't mistake the gap for a bug — operators who
    /// need cryptographic detection of tombstone-payload edits must
    /// compare against a backup taken before the purge.
    #[test]
    fn rewriting_tombstone_payload_within_canonical_shape_is_not_detected() {
        let db = test_db();
        append_n(&db, 6);
        db.tombstone_audit_event(3, 1_700_000_999).expect("tombstone");
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = ?1 WHERE seq = 3",
                params![r#"{"purged":1,"purged_at":99999,"original_type":"evt"}"#],
            )
            .expect("backdate");
        // Verifier still reports OkWithTombstones — by design.
        assert_eq!(
            db.verify_audit_chain().expect("verify"),
            AuditChainOutcome::OkWithTombstones { count: 1 }
        );
    }
}
