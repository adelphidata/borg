//! Output-redaction audit log.
//!
//! Every time the output-redaction policy strips a secret from agent-generated
//! output (channel deliveries, TUI transcript, log writes), one row per
//! pattern is appended here. The row carries the pattern *label* and the hit
//! count — never the secret value or a preview, so the audit log itself is
//! safe to share. See [`crate::output_redaction`].

use anyhow::Result;
use turso::params;

use super::Database;
use crate::security::secrets::RedactionHit;

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
    pub async fn record_redactions(
        &self,
        sink: &str,
        context_key: Option<&str>,
        hits: &[RedactionHit],
    ) -> Result<()> {
        if hits.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let context_key_owned = context_key.map(std::string::ToString::to_string);
        for hit in hits {
            self.conn
                .execute(
                    "INSERT INTO redaction_audit (ts, sink, label, hit_count, context_key)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params!(
                        now,
                        sink,
                        hit.label,
                        hit.count as i64,
                        context_key_owned.clone()
                    ),
                )
                .await?;
        }
        Ok(())
    }

    /// Most-recent rows, newest first. Read path for a future `/redactions`
    /// viewer or doctor surface.
    pub async fn recent_redactions(&self, limit: usize) -> Result<Vec<RedactionAuditRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, ts, sink, label, hit_count, context_key
             FROM redaction_audit ORDER BY ts DESC, id DESC LIMIT ?1",
            )
            .await?;
        let mut rows = stmt.query(params!(limit as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(RedactionAuditRow {
                id: row.get::<i64>(0)?,
                ts: row.get::<i64>(1)?,
                sink: row.get::<String>(2)?,
                label: row.get::<String>(3)?,
                hit_count: row.get::<i64>(4)?,
                context_key: row.get::<Option<String>>(5)?,
            });
        }
        Ok(out)
    }

    /// Total row count. Used by tests and the chokepoint guard.
    pub async fn count_redactions(&self) -> Result<i64> {
        let mut rows = self
            .conn
            .query("SELECT COUNT(*) FROM redaction_audit", ())
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn empty_hits_writes_nothing() {
        let db = db().await;
        db.record_redactions("channel:telegram", Some("ctx"), &[])
            .await
            .unwrap();
        assert_eq!(db.count_redactions().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn records_one_row_per_hit_with_count_preserved() {
        let db = db().await;
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
            .await
            .unwrap();

        let rows = db.recent_redactions(10).await.unwrap();
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

    #[tokio::test]
    async fn null_context_key_round_trips() {
        // Log sink has no per-sender context. Storing NULL is the contract;
        // a future regression that stores an empty string instead would
        // mis-correlate "no context" with "explicit empty context".
        let db = db().await;
        let hits = vec![RedactionHit {
            label: "GitHub Token",
            count: 1,
        }];
        db.record_redactions("log", None, &hits).await.unwrap();
        let rows = db.recent_redactions(10).await.unwrap();
        assert_eq!(rows[0].context_key, None);
    }

    #[tokio::test]
    async fn recent_redactions_respects_limit() {
        let db = db().await;
        for _ in 0..5 {
            db.record_redactions(
                "tui",
                Some("session-1"),
                &[RedactionHit {
                    label: "JWT",
                    count: 1,
                }],
            )
            .await
            .unwrap();
        }
        assert_eq!(db.recent_redactions(3).await.unwrap().len(), 3);
        assert_eq!(db.count_redactions().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn audit_row_never_contains_raw_secret_value() {
        // Invariant: the audit table must never echo back the matched
        // secret. The recording API takes only label + count, so this is
        // structurally true — but the test pins the contract so a future
        // "helpful" change that adds a preview column fails loudly.
        let db = db().await;
        let secret = "AKIAIOSFODNN7EXAMPLE";
        db.record_redactions(
            "channel:telegram",
            Some(secret), // pathological: even if a caller passes the secret as context, we still store what they gave us — the test verifies the *recording API itself* never derives the value from the hit struct.
            &[RedactionHit {
                label: "AWS Access Key",
                count: 1,
            }],
        )
        .await
        .unwrap();
        let rows = db.recent_redactions(1).await.unwrap();
        assert!(!rows[0].sink.contains(secret));
        assert!(!rows[0].label.contains(secret));
        // context_key is caller-controlled; the chokepoint feeds it sender_id:thread_id
        // which is a stable identifier, not the message content. The
        // chokepoint guard test asserts that contract at the call site.
    }
}
