//! Bounded FTS5 segment merge for the daily maintenance sweep.
//!
//! Every insert into `messages` appends a new FTS5 segment to
//! `messages_fts` (via the sync triggers in the schema). Nothing merges
//! them outside SQLite's conservative automerge, so on a long-lived DB the
//! segment count grows without bound: every `MATCH` scans all segments and
//! every insert pays a growing automerge cost that lengthens the write-lock
//! hold time. The daily sweep amortizes that by doing a bounded slice of
//! merge work per run, alongside the WAL checkpoint.

use anyhow::{Context, Result};

use crate::db::Database;

/// Page-write budget for one sweep's worth of FTS5 merge work.
///
/// Passed as a *negative* argument to the FTS5 `merge` command: negative
/// starts a merge of all segments when none is underway (regardless of the
/// `usermerge` threshold) and caps the work at `abs(N)` page writes; a merge
/// already in progress is simply continued under the same cap. Successive
/// daily sweeps therefore converge even a badly fragmented index without any
/// single sweep doing unbounded work. Steady-state cost on a merged index is
/// near zero.
const FTS_MERGE_PAGE_BUDGET: i64 = 500;

/// Run one bounded increment of FTS5 segment merging on `messages_fts`.
///
/// Errors propagate to the caller (the sweep logs and continues — a failed
/// merge must never break maintenance, and it is retried next sweep).
pub(crate) async fn merge_messages_fts_segments(db: &Database) -> Result<()> {
    db.conn()
        .execute(
            "INSERT INTO messages_fts(messages_fts, rank) VALUES('merge', ?1)",
            libsql::params![-FTS_MERGE_PAGE_BUDGET],
        )
        .await
        .context("fts5 'merge' on messages_fts")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_message(db: &Database, id: i64, content: &str) {
        db.conn()
            .execute(
                "INSERT INTO messages (id, session_id, role, content, created_at)
                 VALUES (?1, 's', 'user', ?2, 0)",
                libsql::params![id, content],
            )
            .await
            .expect("insert message");
    }

    async fn fts_match_count(db: &Database, needle: &str) -> i64 {
        let mut rows = db
            .conn()
            .query(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                libsql::params![needle],
            )
            .await
            .expect("fts query");
        rows.next()
            .await
            .expect("row")
            .expect("count row")
            .get::<i64>(0)
            .expect("int")
    }

    /// The merge must succeed on a populated index and leave it fully
    /// queryable: every row indexed before the merge is still found, a
    /// post-merge delete is still honored, and FTS5's own integrity-check
    /// passes (a botched merge of the external-content index fails there).
    #[tokio::test]
    async fn merge_preserves_index_correctness() {
        let db = Database::test_db().await;
        // Row-by-row inserts: each append lands in its own segment, so the
        // merge below has real multi-segment work to do.
        for i in 0..40 {
            insert_message(&db, 700_000 + i, &format!("zebrafish report number {i}")).await;
        }
        insert_message(&db, 700_100, "unrelated aardvark note").await;

        merge_messages_fts_segments(&db)
            .await
            .expect("bounded merge must succeed on a healthy index");

        assert_eq!(
            fts_match_count(&db, "zebrafish").await,
            40,
            "all pre-merge rows must remain matchable after the merge"
        );
        assert_eq!(fts_match_count(&db, "aardvark").await, 1);

        // Deletes routed through the sync trigger must still take effect on
        // the merged index.
        db.conn()
            .execute("DELETE FROM messages WHERE id = 700100", ())
            .await
            .expect("delete message");
        assert_eq!(
            fts_match_count(&db, "aardvark").await,
            0,
            "post-merge delete must drop the row from the index"
        );

        db.conn()
            .execute(
                "INSERT INTO messages_fts(messages_fts) VALUES('integrity-check')",
                (),
            )
            .await
            .expect("fts5 integrity-check must pass after merge");
    }

    /// A second merge on an already-merged (or empty) index is the daily
    /// steady state — it must be a cheap no-op, not an error.
    #[tokio::test]
    async fn merge_is_idempotent_on_quiet_index() {
        let db = Database::test_db().await;
        merge_messages_fts_segments(&db)
            .await
            .expect("merge on empty index");
        insert_message(&db, 700_200, "pangolin sighting").await;
        merge_messages_fts_segments(&db).await.expect("first merge");
        merge_messages_fts_segments(&db)
            .await
            .expect("repeat merge");
        assert_eq!(fts_match_count(&db, "pangolin").await, 1);
    }
}
