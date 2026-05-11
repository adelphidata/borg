//! Output-redaction audit log.
//!
//! Every time the output-redaction policy strips a secret from agent-generated
//! output (channel deliveries, TUI transcript, log writes), one row per
//! pattern is appended here. The row carries the pattern *label* and the hit
//! count — never the secret value or a preview, so the audit log itself is
//! safe to share. See [`crate::output_redaction`].

use anyhow::Result;
use rusqlite::params;

use super::Database;
use crate::secrets::RedactionHit;

/// One row from the `redaction_audit` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionAuditRow {
    pub id: i64,
    pub ts: i64,
    pub sink: String,
    pub label: String,
    pub hit_count: i64,
    pub context_key: Option<String>,
}

impl Database {
    /// Append one row per [`RedactionHit`] to `redaction_audit`. Empty `hits`
    /// is a no-op and writes nothing — callers don't need to gate on it.
    pub fn record_redactions(
        &self,
        sink: &str,
        context_key: Option<&str>,
        hits: &[RedactionHit],
    ) -> Result<()> {
        if hits.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let mut stmt = self.conn.prepare(
            "INSERT INTO redaction_audit (ts, sink, label, hit_count, context_key)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for hit in hits {
            stmt.execute(params![now, sink, hit.label, hit.count as i64, context_key,])?;
        }
        Ok(())
    }

    /// Most-recent rows, newest first. Read path for a future `/redactions`
    /// viewer or doctor surface.
    pub fn recent_redactions(&self, limit: usize) -> Result<Vec<RedactionAuditRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, sink, label, hit_count, context_key
             FROM redaction_audit ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(RedactionAuditRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    sink: row.get(2)?,
                    label: row.get(3)?,
                    hit_count: row.get(4)?,
                    context_key: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total row count. Used by tests and the chokepoint guard.
    pub fn count_redactions(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM redaction_audit", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::test_db()
    }

    #[test]
    fn empty_hits_writes_nothing() {
        let db = db();
        db.record_redactions("channel:telegram", Some("ctx"), &[])
            .unwrap();
        assert_eq!(db.count_redactions().unwrap(), 0);
    }

    #[test]
    fn records_one_row_per_hit_with_count_preserved() {
        let db = db();
        let hits = vec![
            RedactionHit {
                label: "AWS Access Key",
                count: 2,
            },
            RedactionHit {
                label: "JWT",
                count: 1,
            },
        ];
        db.record_redactions("channel:slack", Some("user-42:thread-9"), &hits)
            .unwrap();

        let rows = db.recent_redactions(10).unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].label, "JWT");
        assert_eq!(rows[0].hit_count, 1);
        assert_eq!(rows[1].label, "AWS Access Key");
        assert_eq!(rows[1].hit_count, 2, "hit_count must round-trip");
        for r in &rows {
            assert_eq!(r.sink, "channel:slack");
            assert_eq!(r.context_key.as_deref(), Some("user-42:thread-9"));
        }
    }

    #[test]
    fn null_context_key_round_trips() {
        // Log sink has no per-sender context. Storing NULL is the contract;
        // a future regression that stores an empty string instead would
        // mis-correlate "no context" with "explicit empty context".
        let db = db();
        let hits = vec![RedactionHit {
            label: "GitHub Token",
            count: 1,
        }];
        db.record_redactions("log", None, &hits).unwrap();
        let rows = db.recent_redactions(10).unwrap();
        assert_eq!(rows[0].context_key, None);
    }

    #[test]
    fn recent_redactions_respects_limit() {
        let db = db();
        for _ in 0..5 {
            db.record_redactions(
                "tui",
                Some("session-1"),
                &[RedactionHit {
                    label: "JWT",
                    count: 1,
                }],
            )
            .unwrap();
        }
        assert_eq!(db.recent_redactions(3).unwrap().len(), 3);
        assert_eq!(db.count_redactions().unwrap(), 5);
    }

    #[test]
    fn audit_row_never_contains_raw_secret_value() {
        // Invariant: the audit table must never echo back the matched
        // secret. The recording API takes only label + count, so this is
        // structurally true — but the test pins the contract so a future
        // "helpful" change that adds a preview column fails loudly.
        let db = db();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        db.record_redactions(
            "channel:telegram",
            Some(secret), // pathological: even if a caller passes the secret as context, we still store what they gave us — the test verifies the *recording API itself* never derives the value from the hit struct.
            &[RedactionHit {
                label: "AWS Access Key",
                count: 1,
            }],
        )
        .unwrap();
        let rows = db.recent_redactions(1).unwrap();
        assert!(!rows[0].sink.contains(secret));
        assert!(!rows[0].label.contains(secret));
        // context_key is caller-controlled; the chokepoint feeds it sender_id:thread_id
        // which is a stable identifier, not the message content. The
        // chokepoint guard test asserts that contract at the call site.
    }
}
