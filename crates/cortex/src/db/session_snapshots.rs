//! `session_snapshots`: which file checkpoint was taken before which turn.
//!
//! The checkpoint itself is a commit in the store under the data dir
//! ([`crate::git::checkpoints`]). A row ties that commit to the conversation
//! turn it precedes, so "put the files back to before this turn" has
//! something to look up.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use libsql::params;

use super::Database;
use crate::git::checkpoints::Filed;

/// A checkpoint to record: the state of `snapshot_root` just before the turn
/// that the user message `user_message_seq` started.
#[derive(Debug, Clone)]
pub struct NewSessionSnapshot<'a> {
    /// The root session of the agent tree that took it.
    pub session_id: &'a str,
    /// `messages.seq` of the user message that started the turn, when the
    /// message had been persisted.
    pub user_message_seq: Option<i64>,
    pub snapshot_root: &'a std::path::Path,
    pub commit_id: &'a str,
    /// Paths under the root the checkpoint does not hold.
    pub uncovered: &'a [PathBuf],
    /// True when this is the state a rollback replaced, not the state
    /// before a turn: restoring it undoes that rollback.
    pub before_rollback: bool,
}

/// One row of `session_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshotRow {
    pub id: i64,
    pub session_id: String,
    pub user_message_seq: Option<i64>,
    pub snapshot_root: PathBuf,
    pub commit_id: String,
    pub uncovered: Vec<PathBuf>,
    pub before_rollback: bool,
    pub created_at: i64,
    /// What the person wrote to start the turn, while that message is
    /// still in the log.
    pub prompt: Option<String>,
}

impl Database {
    /// Record a checkpoint; returns its row id.
    pub async fn record_session_snapshot(&self, new: &NewSessionSnapshot<'_>) -> Result<i64> {
        let uncovered: Vec<String> = new
            .uncovered
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let uncovered_json = serde_json::to_string(&uncovered).context("encode uncovered paths")?;
        let mut rows = self
            .conn
            .query(
                "INSERT INTO session_snapshots
                     (session_id, user_message_seq, snapshot_root, commit_id,
                      uncovered_json, before_rollback, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 RETURNING id",
                params!(
                    new.session_id,
                    new.user_message_seq,
                    new.snapshot_root.to_string_lossy().into_owned(),
                    new.commit_id,
                    uncovered_json,
                    i64::from(new.before_rollback),
                    chrono::Utc::now().timestamp()
                ),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .context("insert into session_snapshots returned no row")?;
        Ok(row.get::<i64>(0)?)
    }

    /// A session's checkpoints, newest first.
    pub async fn list_session_snapshots(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<Vec<SessionSnapshotRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT s.id, s.session_id, s.user_message_seq, s.snapshot_root,
                        s.commit_id, s.uncovered_json, s.before_rollback, s.created_at,
                        m.content
                 FROM session_snapshots s
                 LEFT JOIN messages m
                        ON m.id = s.user_message_seq AND m.role = 'user'
                 WHERE s.session_id = ?1
                 ORDER BY s.id DESC
                 LIMIT ?2",
                params!(session_id, i64::from(limit)),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let uncovered: Vec<String> =
                serde_json::from_str(&row.get::<String>(5)?).context("decode uncovered paths")?;
            out.push(SessionSnapshotRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                user_message_seq: row.get(2)?,
                snapshot_root: PathBuf::from(row.get::<String>(3)?),
                commit_id: row.get(4)?,
                uncovered: uncovered.into_iter().map(PathBuf::from).collect(),
                before_rollback: row.get::<i64>(6)? != 0,
                created_at: row.get(7)?,
                prompt: row.get(8)?,
            });
        }
        Ok(out)
    }
}

impl Database {
    /// For each checkpoint commit, when a conversation last filed it as an
    /// undo point. An unchanged workspace files the same commit again and
    /// again, so this — not the commit's own date — is how recently it
    /// mattered to someone.
    pub async fn session_snapshot_last_used(&self) -> Result<HashMap<String, Filed>> {
        let mut rows = self
            .conn
            .query(
                "SELECT commit_id, MAX(created_at), MAX(id)\n                 FROM session_snapshots GROUP BY commit_id",
                (),
            )
            .await?;
        let mut last_used = HashMap::new();
        while let Some(row) = rows.next().await? {
            last_used.insert(
                row.get::<String>(0)?,
                Filed {
                    at: row.get(1)?,
                    seq: row.get(2)?,
                },
            );
        }
        Ok(last_used)
    }

    /// Delete the rows filed before `cutoff`; returns how many.
    pub async fn prune_session_snapshots_before(&self, cutoff: i64) -> Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM session_snapshots WHERE created_at < ?1",
                params!(cutoff),
            )
            .await?;
        Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A recorded checkpoint reads back whole, each session sees only its
    /// own, and the newest comes first — the order "undo the last turn"
    /// depends on.
    #[tokio::test]
    async fn checkpoints_read_back_newest_first_per_session() {
        let db = Database::test_db().await;
        let uncovered = [PathBuf::from("vendor/lib"), PathBuf::from("model.bin")];
        let record = |session: &'static str, seq: Option<i64>, commit: &'static str| {
            let db = &db;
            let uncovered = &uncovered;
            async move {
                db.record_session_snapshot(&NewSessionSnapshot {
                    session_id: session,
                    user_message_seq: seq,
                    snapshot_root: Path::new("/work/project"),
                    commit_id: commit,
                    uncovered,
                    before_rollback: commit == "bbb",
                })
                .await
                .expect("record")
            }
        };
        let first = record("s1", Some(3), "aaa").await;
        record("s2", Some(1), "zzz").await;
        let second = record("s1", None, "bbb").await;
        assert!(second > first);

        let rows = db.list_session_snapshots("s1", 10).await.expect("list");

        assert_eq!(
            rows.iter()
                .map(|r| r.commit_id.as_str())
                .collect::<Vec<_>>(),
            ["bbb", "aaa"]
        );
        assert_eq!(rows[1].id, first);
        assert_eq!(rows[1].user_message_seq, Some(3));
        assert_eq!(rows[0].user_message_seq, None);
        assert_eq!(
            rows.iter().map(|r| r.before_rollback).collect::<Vec<_>>(),
            [true, false]
        );
        assert_eq!(rows[0].snapshot_root, Path::new("/work/project"));
        assert_eq!(rows[0].uncovered, uncovered);
        assert!(rows[0].created_at > 0);

        let capped = db.list_session_snapshots("s1", 1).await.expect("list");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].commit_id, "bbb");
    }

    async fn file(db: &Database, session: &str, seq: Option<i64>, commit: &str, at: i64) {
        let id = db
            .record_session_snapshot(&NewSessionSnapshot {
                session_id: session,
                user_message_seq: seq,
                snapshot_root: Path::new("/work/project"),
                commit_id: commit,
                uncovered: &[],
                before_rollback: false,
            })
            .await
            .expect("record");
        db.conn
            .execute(
                "UPDATE session_snapshots SET created_at = ?1 WHERE id = ?2",
                params!(at, id),
            )
            .await
            .expect("set the time");
    }

    /// An undo point is labelled with what the person wrote to start the
    /// turn. A message that is gone, or a number that names some other
    /// kind of row, gives no label rather than the wrong one.
    #[tokio::test]
    async fn a_checkpoint_carries_the_prompt_of_the_turn_it_precedes() {
        let db = Database::test_db().await;
        let asked = db
            .insert_message("s1", "user", Some("tidy the notes"), None, None, None, None)
            .await
            .expect("user message");
        let replied = db
            .insert_message("s1", "assistant", Some("done"), None, None, None, None)
            .await
            .expect("assistant message");
        file(&db, "s1", Some(asked), "aaa", 100).await;
        file(&db, "s1", Some(replied), "bbb", 200).await;
        file(&db, "s1", Some(asked + 10_000), "ccc", 300).await;
        file(&db, "s1", None, "ddd", 400).await;

        let rows = db.list_session_snapshots("s1", 10).await.expect("list");

        let prompts: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|r| (r.commit_id.as_str(), r.prompt.as_deref()))
            .collect();
        assert_eq!(
            prompts,
            [
                ("ddd", None),
                ("ccc", None),
                ("bbb", None),
                ("aaa", Some("tidy the notes")),
            ]
        );
    }

    /// Pruning needs to know when each checkpoint last mattered to a
    /// conversation, and to drop the rows that are past caring about.
    #[tokio::test]
    async fn last_use_is_the_newest_filing_and_old_rows_can_be_dropped() {
        let db = Database::test_db().await;
        file(&db, "s1", None, "reused", 100).await;
        file(&db, "s2", None, "reused", 900).await;
        file(&db, "s1", None, "once", 500).await;

        let last_used = db.session_snapshot_last_used().await.expect("last used");
        // The time of the newest filing, and — to order two filings in one
        // second — the row it was (rows 1 to 3 here, in the order filed).
        assert_eq!(
            last_used,
            HashMap::from([
                ("reused".to_string(), Filed { at: 900, seq: 2 }),
                ("once".to_string(), Filed { at: 500, seq: 3 }),
            ])
        );

        let deleted = db.prune_session_snapshots_before(500).await.expect("prune");
        assert_eq!(deleted, 1, "only the row filed before the cutoff");
        let left = db.session_snapshot_last_used().await.expect("last used");
        assert_eq!(left.len(), 2);
        assert_eq!(
            db.list_session_snapshots("s1", 10)
                .await
                .expect("list")
                .len(),
            1
        );
    }
}
