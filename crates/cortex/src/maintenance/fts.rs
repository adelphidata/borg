//! Bounded FTS5 segment merge for the daily maintenance sweep.
//!
//! Every insert into `messages` appends a new FTS5 segment to
//! `messages_fts` (via the sync triggers in the schema). Nothing merges
//! them outside SQLite's conservative automerge, so on a long-lived DB the
//! segment count grows without bound: every `MATCH` scans all segments and
//! every insert pays a growing automerge cost that lengthens the write-lock
//! hold time. The daily sweep amortizes that by doing a bounded slice of
//! merge work per run, alongside the WAL checkpoint.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::db::Database;

/// Every FTS5 index the schema maintains. Each is an external-content table
/// whose shadow `_data` segments can rot independently of the source rows.
pub(crate) const FTS_TABLES: [&str; 2] = ["messages_fts", "memory_chunks_fts"];

/// Remediation key the doctor attaches to a failed index check; the repair
/// driver dispatches it to [`rebuild_all`].
pub(crate) const FTS_REMEDIATION_KEY: &str = "fts.rebuild";

/// Next-action hint every corruption message carries.
pub(crate) const FTS_REPAIR_HINT: &str = "run `borg doctor --repair` to rebuild the search index";

/// Set once the first corruption error has been logged this process, so a
/// corrupt index that fails every search does not flood the log. Later hits
/// still leave a `debug!` trail.
static CORRUPTION_LOGGED: AtomicBool = AtomicBool::new(false);

/// If `err` is SQLite corruption, log it at `error!` with the repair hint —
/// once per process — and return `true`. Any other error returns `false`
/// untouched so the caller applies its usual handling.
pub(crate) fn log_corruption_once(context: &str, err: &anyhow::Error) -> bool {
    if !crate::db::is_corruption(err) {
        return false;
    }
    if CORRUPTION_LOGGED.swap(true, Ordering::SeqCst) {
        tracing::debug!("{context}: search index is corrupt ({err:#}); already reported");
    } else {
        tracing::error!("{context}: search index is corrupt ({err:#}); {FTS_REPAIR_HINT}");
    }
    true
}

/// Run FTS5's own `integrity-check` on `table`: verifies the shadow segments
/// against the external content table. Errors are the corruption report.
pub(crate) async fn integrity_check(db: &Database, table: &str) -> Result<()> {
    db.conn()
        .execute(
            &format!("INSERT INTO {table}({table}) VALUES('integrity-check')"),
            (),
        )
        .await
        .with_context(|| format!("fts5 'integrity-check' on {table}"))?;
    Ok(())
}

/// Integrity-check every index in [`FTS_TABLES`], returning each outcome.
pub(crate) async fn check_all(db: &Database) -> Vec<(&'static str, Result<()>)> {
    let mut out = Vec::with_capacity(FTS_TABLES.len());
    for table in FTS_TABLES {
        out.push((table, integrity_check(db, table).await));
    }
    out
}

/// Outcome of [`rebuild_all`]: which indexes were rebuilt and which failed,
/// so a caller reports every error rather than the first.
#[derive(Debug, Default)]
pub struct FtsRebuildReport {
    /// Indexes rebuilt from their content table.
    pub rebuilt: Vec<&'static str>,
    /// `(table, error)` for indexes whose rebuild failed.
    pub failed: Vec<(&'static str, String)>,
}

/// Rebuild one index from its content table inside its own transaction, so
/// a failure leaves the previous segments in place rather than half-written.
async fn rebuild_one(db: &Database, table: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute_batch("BEGIN IMMEDIATE")
        .await
        .with_context(|| format!("begin transaction for {table} rebuild"))?;
    let rebuilt = conn
        .execute(
            &format!("INSERT INTO {table}({table}) VALUES('rebuild')"),
            (),
        )
        .await
        .map(|_| ())
        .with_context(|| format!("fts5 'rebuild' on {table}"));
    finish_rebuild_tx(conn, table, rebuilt).await
}

/// Close the transaction [`rebuild_one`] opened: `COMMIT` when the body
/// succeeded, `ROLLBACK` when the body *or the commit* failed. SQLite keeps
/// the transaction open after a failed `COMMIT`, and this is the daemon's
/// shared long-lived connection — without the rollback it would hold the
/// write lock for the rest of the process.
async fn finish_rebuild_tx(conn: &libsql::Connection, table: &str, body: Result<()>) -> Result<()> {
    let outcome = match body {
        Ok(()) => conn
            .execute_batch("COMMIT")
            .await
            .map(|_| ())
            .with_context(|| format!("commit {table} rebuild")),
        Err(e) => Err(e),
    };
    if let Err(e) = &outcome {
        if let Err(rb) = conn.execute_batch("ROLLBACK").await {
            tracing::warn!(
                "rollback after failed {table} rebuild also failed: {rb} (original: {e:#})"
            );
        }
    }
    outcome
}

/// Rebuild every index in [`FTS_TABLES`], one transaction each. A failed
/// index never stops the next one; all errors are collected in the report.
pub(crate) async fn rebuild_all(db: &Database) -> FtsRebuildReport {
    let mut report = FtsRebuildReport::default();
    for table in FTS_TABLES {
        match rebuild_one(db, table).await {
            Ok(()) => report.rebuilt.push(table),
            Err(e) => report.failed.push((table, format!("{e:#}"))),
        }
    }
    report
}

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
pub(crate) mod tests {
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

    /// Overwrite every segment block of `table`'s shadow `_data` table so the
    /// index no longer parses — the on-disk shape of a torn write.
    pub(crate) async fn garble_fts_data(db: &Database, table: &str) {
        db.conn()
            .execute(&format!("UPDATE {table}_data SET block = zeroblob(1)"), ())
            .await
            .expect("garble shadow table");
    }

    #[tokio::test]
    async fn garbled_index_fails_integrity_check_and_rebuild_restores_matching() {
        let db = Database::test_db().await;
        insert_message(&db, 700_300, "quokka census").await;
        integrity_check(&db, "messages_fts")
            .await
            .expect("precondition: a fresh index passes");
        garble_fts_data(&db, "messages_fts").await;

        let err = integrity_check(&db, "messages_fts")
            .await
            .expect_err("a garbled shadow table must fail fts5 integrity-check");
        assert!(
            crate::db::is_corruption(&err),
            "the failure must classify as corruption so callers hint the repair: {err:#}"
        );

        let report = rebuild_all(&db).await;
        assert!(report.failed.is_empty(), "failed: {:?}", report.failed);
        assert_eq!(report.rebuilt, FTS_TABLES);
        integrity_check(&db, "messages_fts")
            .await
            .expect("integrity-check passes after rebuild");
        assert_eq!(
            fts_match_count(&db, "quokka").await,
            1,
            "the row indexed before corruption must be findable again"
        );
    }

    #[tokio::test]
    async fn rebuild_continues_past_a_failing_index_and_reports_it() {
        let db = Database::test_db().await;
        db.conn()
            .execute(
                "INSERT INTO memory_chunks (scope, filename, chunk_index, start_line, end_line, content, content_hash, embedding, created_at)
                 VALUES ('global', 'f.md', 0, 1, 1, 'numbat burrow', 'h', X'00', 0)",
                (),
            )
            .await
            .expect("insert chunk");
        garble_fts_data(&db, "memory_chunks_fts").await;
        // Drop the first index's content table out from under it: its rebuild
        // cannot succeed, which must not stop the second index's repair.
        db.conn()
            .execute("DROP TRIGGER messages_ai", ())
            .await
            .expect("drop trigger");
        db.conn()
            .execute("DROP TRIGGER messages_ad", ())
            .await
            .expect("drop trigger");
        db.conn()
            .execute("DROP TRIGGER messages_au", ())
            .await
            .expect("drop trigger");
        db.conn()
            .execute("ALTER TABLE messages RENAME TO messages_gone", ())
            .await
            .expect("rename content table");

        let report = rebuild_all(&db).await;
        assert_eq!(report.rebuilt, vec!["memory_chunks_fts"]);
        assert_eq!(report.failed.len(), 1, "failed: {:?}", report.failed);
        assert_eq!(report.failed[0].0, "messages_fts");
        assert!(
            report.failed[0].1.contains("messages_fts"),
            "the error must name the index: {}",
            report.failed[0].1
        );
        let mut rows = db
            .conn()
            .query(
                "SELECT COUNT(*) FROM memory_chunks_fts WHERE memory_chunks_fts MATCH 'numbat'",
                (),
            )
            .await
            .expect("match on rebuilt index");
        let n = rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap();
        assert_eq!(
            n, 1,
            "memory_chunks_fts must be rebuilt even though messages_fts failed"
        );
    }

    /// A `COMMIT` can fail after the rebuild body succeeded (a deferred
    /// constraint stands in for I/O trouble at commit time). SQLite keeps the
    /// transaction open on a failed commit, so the helper must roll back on
    /// that arm too, or the shared connection holds the write lock for the
    /// life of the daemon. Proven by opening a fresh `BEGIN IMMEDIATE`
    /// afterwards, which SQLite refuses inside an open transaction.
    #[tokio::test]
    async fn a_failed_commit_is_rolled_back_so_the_connection_is_free_again() {
        let db = Database::test_db().await;
        let conn = db.conn();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE fk_parent (id INTEGER PRIMARY KEY);
             CREATE TABLE fk_child (
                 parent INTEGER REFERENCES fk_parent(id) DEFERRABLE INITIALLY DEFERRED
             );",
        )
        .await
        .expect("fixture schema");

        // Body failed: rolled back.
        conn.execute_batch("BEGIN IMMEDIATE").await.expect("begin");
        let err = finish_rebuild_tx(conn, "messages_fts", Err(anyhow::anyhow!("boom")))
            .await
            .expect_err("the body error must propagate");
        assert_eq!(err.to_string(), "boom");
        conn.execute_batch("BEGIN IMMEDIATE")
            .await
            .expect("the body failure must have rolled the transaction back");
        conn.execute_batch("ROLLBACK").await.expect("rollback");

        // Body succeeded, COMMIT failed: rolled back as well.
        conn.execute_batch("BEGIN IMMEDIATE").await.expect("begin");
        conn.execute("INSERT INTO fk_child (parent) VALUES (42)", ())
            .await
            .expect("a deferred violation is accepted until commit");
        let err = finish_rebuild_tx(conn, "messages_fts", Ok(()))
            .await
            .expect_err("COMMIT must fail on the deferred foreign key");
        assert!(
            format!("{err:#}").contains("commit messages_fts rebuild"),
            "the commit error must name the step: {err:#}"
        );
        conn.execute_batch("BEGIN IMMEDIATE")
            .await
            .expect("a failed COMMIT must not leave the connection inside a transaction");
        conn.execute_batch("ROLLBACK").await.expect("rollback");
    }

    #[test]
    fn log_corruption_once_only_classifies_corruption() {
        let plain = anyhow::anyhow!("disk full");
        assert!(!log_corruption_once("test", &plain));
        let corrupt = anyhow::Error::new(libsql::Error::SqliteFailure(
            267,
            "database disk image is malformed".into(),
        ));
        assert!(log_corruption_once("test", &corrupt));
        assert!(
            CORRUPTION_LOGGED.load(Ordering::SeqCst),
            "the once-flag must latch after the first corruption report"
        );
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
