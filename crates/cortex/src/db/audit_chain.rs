//! Hash-chained audit log.
//!
//! Append-only blake3 chain over security-relevant events. Each row's `hash`
//! is computed over `prev_hash || event_type || event_json || ts_le`, so
//! flipping any byte in any row breaks the chain at that row.
//!
//! The verifier is **pure**: no clock, no network, no DB writes.
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditChainOutcome {
    Ok,
    Tampered { seq: i64 },
}

pub fn compute_row_hash(
    prev_hash: &[u8],
    event_type: &str,
    event_json: &str,
    ts: i64,
) -> [u8; HASH_LEN] {
    let mut h = Hasher::new();
    h.update(prev_hash);
    h.update(event_type.as_bytes());
    h.update(event_json.as_bytes());
    h.update(&ts.to_le_bytes());
    *h.finalize().as_bytes()
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

        let hash = compute_row_hash(&prev, event_type, event_json, ts);

        tx.execute(
            "INSERT INTO audit_log_chain
             (prev_hash, hash, event_type, event_json, ts, principal_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                prev,
                hash.to_vec(),
                event_type,
                event_json,
                ts,
                principal_id
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;
        Ok(seq)
    }

    pub fn verify_audit_chain(&self) -> Result<AuditChainOutcome> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, prev_hash, hash, event_type, event_json, ts
             FROM audit_log_chain ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query([])?;

        let mut expected_prev: Vec<u8> = GENESIS_PREV_HASH.to_vec();
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let prev_hash: Vec<u8> = r.get(1)?;
            let stored_hash: Vec<u8> = r.get(2)?;
            let event_type: String = r.get(3)?;
            let event_json: String = r.get(4)?;
            let ts: i64 = r.get(5)?;

            if prev_hash != expected_prev {
                return Ok(AuditChainOutcome::Tampered { seq });
            }
            let computed = compute_row_hash(&prev_hash, &event_type, &event_json, ts);
            if computed.as_slice() != stored_hash.as_slice() {
                return Ok(AuditChainOutcome::Tampered { seq });
            }
            expected_prev = stored_hash;
        }
        Ok(AuditChainOutcome::Ok)
    }

    pub fn list_audit_events(&self) -> Result<Vec<AuditChainRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, prev_hash, hash, event_type, event_json, ts, principal_id
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

    #[test]
    fn verify_empty_chain_ok() {
        let db = test_db();
        assert_eq!(
            db.verify_audit_chain().expect("verify"),
            AuditChainOutcome::Ok
        );
    }

    #[test]
    fn append_then_verify_ok_for_long_chain() {
        let db = test_db();
        for i in 0..50 {
            let payload = format!(r#"{{"i":{}}}"#, i);
            db.append_audit_event("test_event", &payload, "principal-1", 1_700_000_000 + i)
                .expect("append");
        }
        assert_eq!(
            db.verify_audit_chain().expect("verify"),
            AuditChainOutcome::Ok
        );
    }

    #[test]
    fn first_row_uses_genesis_prev_hash() {
        let db = test_db();
        db.append_audit_event("first", r#"{"a":1}"#, "p", 1_700_000_000)
            .expect("append");
        let rows = db.list_audit_events().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prev_hash, GENESIS_PREV_HASH.to_vec());
    }

    #[test]
    fn tamper_with_event_json_detected_at_correct_seq() {
        let db = test_db();
        for i in 0..20 {
            db.append_audit_event("evt", &format!(r#"{{"i":{}}}"#, i), "p", 1_700_000_000 + i)
                .expect("append");
        }
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"tampered\":true}' WHERE seq = 7",
                [],
            )
            .expect("tamper");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 7),
            other => panic!("expected Tampered(7), got {:?}", other),
        }
    }

    #[test]
    fn tamper_with_prev_hash_detected() {
        let db = test_db();
        for i in 0..10 {
            db.append_audit_event("evt", &format!(r#"{{"i":{}}}"#, i), "p", 1_700_000_000 + i)
                .expect("append");
        }
        let zeros = vec![0u8; HASH_LEN];
        db.conn()
            .execute(
                "UPDATE audit_log_chain SET prev_hash = ?1 WHERE seq = 4",
                params![zeros],
            )
            .expect("tamper");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 4),
            other => panic!("expected Tampered(4), got {:?}", other),
        }
    }

    #[test]
    fn deleting_a_middle_row_breaks_chain_linkage() {
        let db = test_db();
        for i in 0..6 {
            db.append_audit_event("evt", &format!(r#"{{"i":{}}}"#, i), "p", 1_700_000_000 + i)
                .expect("append");
        }
        db.conn()
            .execute("DELETE FROM audit_log_chain WHERE seq = 3", [])
            .expect("delete");
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::Tampered { seq } => assert_eq!(seq, 4),
            other => panic!("expected Tampered(4), got {:?}", other),
        }
    }

    #[test]
    fn hash_function_is_deterministic_and_byte_sensitive() {
        let h1 = compute_row_hash(&[0u8; 32], "evt", "{}", 1);
        let h2 = compute_row_hash(&[0u8; 32], "evt", "{}", 1);
        let h3 = compute_row_hash(&[0u8; 32], "evt", "{}", 2);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
