//! Sweep step: drop undo points nobody will go back to.
//!
//! The checkpoint store grows by one commit per turn that changes files, and
//! the turn path never tidies up after itself. This is the only place old
//! checkpoints, and the `session_snapshots` rows that name them, are removed.

use super::budget::BudgetTracker;
use super::MaintenanceReport;
use crate::db::Database;
use crate::git::checkpoints::{CheckpointStore, PrunePolicy};

/// Prune the store under the data dir and the rows that point into it.
/// Budgeted like every other mutating step; a failure logs and the sweep
/// goes on.
pub(super) async fn prune_undo_points(
    db: &Database,
    now: i64,
    report: &mut MaintenanceReport,
    budget: &mut BudgetTracker,
) {
    match crate::paths::resolve_borg_home_readonly(None) {
        Ok(home) => {
            let store = CheckpointStore::new(&home);
            prune_undo_points_in(db, &store, &PrunePolicy::default(), now, report, budget).await;
        }
        Err(e) => {
            tracing::warn!("maintenance: cannot resolve the data dir, undo points not pruned: {e}");
        }
    }
}

async fn prune_undo_points_in(
    db: &Database,
    store: &CheckpointStore,
    policy: &PrunePolicy,
    now: i64,
    report: &mut MaintenanceReport,
    budget: &mut BudgetTracker,
) {
    if budget.is_blown() {
        return;
    }
    // Without this the store would age checkpoints by their commit date
    // alone, and drop one a conversation filed this morning.
    let last_used = match db.session_snapshot_last_used().await {
        Ok(last_used) => last_used,
        Err(e) => {
            tracing::warn!("maintenance: undo points not pruned, cannot read their use: {e:#}");
            return;
        }
    };
    match store.prune(policy, now, &last_used).await {
        Ok(pruned) => {
            report.undo_points_pruned = pruned.dropped;
            budget.try_spend(pruned.dropped);
        }
        Err(e) => {
            tracing::warn!("maintenance: undo point pruning failed: {e:#}");
            // The rows stay too: they may still name checkpoints that exist.
            return;
        }
    }
    // A row older than the age limit names a checkpoint that is gone, or one
    // a newer row keeps alive under its own name.
    match db
        .prune_session_snapshots_before(now.saturating_sub(policy.max_age_secs))
        .await
    {
        Ok(rows) => {
            budget.try_spend(rows);
        }
        Err(e) => tracing::warn!("maintenance: undo point row pruning failed: {e:#}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::constants::SECS_PER_DAY;
    use crate::db::NewSessionSnapshot;
    use crate::git::checkpoints::SnapshotOutcome;
    use std::path::PathBuf;

    struct Fixture {
        _tmp: tempfile::TempDir,
        db: Database,
        store: CheckpointStore,
        ws: PathBuf,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let base = tmp.path().canonicalize().expect("canonicalize");
            let ws = base.join("project");
            std::fs::create_dir(&ws).expect("mkdir");
            Self {
                db: Database::test_db().await,
                store: CheckpointStore::new(&base.join("borg-home")),
                ws,
                _tmp: tmp,
            }
        }

        /// A turn's undo point: a checkpoint of the workspace with `content`
        /// in it, filed against a conversation. Returns its commit id.
        async fn undo_point(&self, content: &str) -> String {
            std::fs::write(self.ws.join("file.txt"), content).expect("write");
            let SnapshotOutcome::Taken(checkpoint) =
                self.store.snapshot(&self.ws).await.expect("snapshot")
            else {
                panic!("skipped");
            };
            self.db
                .record_session_snapshot(&NewSessionSnapshot {
                    session_id: "s1",
                    user_message_seq: None,
                    snapshot_root: &checkpoint.root,
                    commit_id: &checkpoint.commit_id,
                    uncovered: &[],
                    before_rollback: false,
                })
                .await
                .expect("record");
            checkpoint.commit_id
        }

        async fn sweep(&self, policy: &PrunePolicy, now: i64, budget: &mut BudgetTracker) -> usize {
            let mut report = MaintenanceReport::default();
            prune_undo_points_in(&self.db, &self.store, policy, now, &mut report, budget).await;
            report.undo_points_pruned
        }

        async fn rows(&self) -> Vec<String> {
            self.db
                .list_session_snapshots("s1", 50)
                .await
                .expect("list")
                .into_iter()
                .map(|row| row.commit_id)
                .collect()
        }

        /// Whether the store still has this undo point. Only looks: a
        /// restore would record the files first, and so add a checkpoint of
        /// its own to the counts under test.
        async fn can_restore(&self, commit_id: &str) -> bool {
            let differing = self
                .store
                .compare_with_now(&self.ws, &[commit_id])
                .await
                .expect("compare")
                .expect("a project");
            differing[0].is_some()
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// The nightly sweep keeps what a person may still go back to and drops
    /// the rest from both places: the store and the conversation's list.
    #[tokio::test]
    async fn the_sweep_drops_undo_points_past_their_count_and_age() {
        let fx = Fixture::new().await;
        let first = fx.undo_point("one\n").await;
        let second = fx.undo_point("two\n").await;
        let third = fx.undo_point("three\n").await;
        let keep_two = PrunePolicy {
            keep_per_workspace: 2,
            ..PrunePolicy::default()
        };
        let mut budget = BudgetTracker::new(1_000);

        // Past the count: the oldest goes from the store. Its row is not a
        // week old yet, so it stays — and is simply no longer offered.
        let dropped = fx.sweep(&keep_two, now(), &mut budget).await;
        assert_eq!(dropped, 1);
        assert_eq!(budget.spent, 1);
        assert!(!fx.can_restore(&first).await);
        assert!(fx.can_restore(&second).await);
        assert!(fx.can_restore(&third).await);
        assert_eq!(fx.rows().await.len(), 3);

        // A month on, everything has aged out, rows included.
        let dropped = fx
            .sweep(&keep_two, now() + 30 * SECS_PER_DAY, &mut budget)
            .await;
        assert_eq!(dropped, 2);
        assert!(!fx.can_restore(&third).await);
        assert_eq!(fx.rows().await, Vec::<String>::new());
        // Two checkpoints and three rows on top of the first sweep's one.
        assert_eq!(budget.spent, 6);
    }

    /// An untouched project's undo point from this morning is last week's
    /// commit. The sweep reads when it was last filed, and keeps it.
    #[tokio::test]
    async fn an_undo_point_filed_today_survives_however_old_its_commit() {
        let fx = Fixture::new().await;
        let commit = fx.undo_point("unchanged\n").await;
        // Ten days on the commit is past the week, but a turn files it again.
        let later = now() + 10 * SECS_PER_DAY;
        fx.db
            .conn()
            .execute(
                "UPDATE session_snapshots SET created_at = ?1",
                libsql::params!(later - 60),
            )
            .await
            .expect("age the row forward");
        let mut budget = BudgetTracker::new(1_000);

        let dropped = fx.sweep(&PrunePolicy::default(), later, &mut budget).await;

        assert_eq!(dropped, 0);
        assert!(fx.can_restore(&commit).await);
        assert_eq!(fx.rows().await, [commit]);
    }

    /// A sweep that has already spent its action budget leaves undo points
    /// for the next one.
    #[tokio::test]
    async fn an_exhausted_budget_skips_the_step() {
        let fx = Fixture::new().await;
        let commit = fx.undo_point("one\n").await;
        let mut budget = BudgetTracker::new(1);
        budget.try_spend(5);
        assert!(budget.is_blown());

        let dropped = fx
            .sweep(
                &PrunePolicy::default(),
                now() + 30 * SECS_PER_DAY,
                &mut budget,
            )
            .await;

        assert_eq!(dropped, 0);
        assert!(fx.can_restore(&commit).await);
        assert_eq!(fx.rows().await.len(), 1);
    }
}
