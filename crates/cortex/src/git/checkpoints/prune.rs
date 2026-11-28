//! Dropping checkpoints nobody will go back to.
//!
//! Every checkpoint has a ref of its own and no parent, so dropping one is
//! deleting that ref; the objects only it used go at the next repack. Three
//! limits, in the order they are applied: an age, a count per workspace, and
//! a size for the whole store.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::instrument;

use super::{run_git_within, CheckpointStore};

/// Repacking a store at its size limit is minutes, not the seconds one
/// snapshot command gets.
const REPACK_BUDGET: Duration = Duration::from_secs(300);

/// How long an object no ref reaches is kept before a repack deletes it.
///
/// Never "now": a snapshot writes its objects first and its ref last, and a
/// repack that ran in between would delete a checkpoint that is about to
/// exist. A day is far beyond any snapshot.
const UNREACHABLE_GRACE: &str = "gc.pruneExpire=1.day.ago";

/// Which checkpoints to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Checkpoints last used longer ago than this are dropped.
    pub max_age_secs: i64,
    /// At most this many per workspace, newest first.
    pub keep_per_workspace: usize,
    /// When the store is larger than this after the two rules above, each
    /// workspace is cut to a quarter of `keep_per_workspace`. The space comes
    /// back at the next repack after [`UNREACHABLE_GRACE`]; the limit is
    /// pressure, not a hard ceiling.
    pub max_store_bytes: u64,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            max_age_secs: 7 * 24 * 60 * 60,
            keep_per_workspace: 20,
            max_store_bytes: 500 * 1024 * 1024,
        }
    }
}

/// When a conversation last filed a checkpoint as an undo point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Filed {
    /// Unix seconds.
    pub at: i64,
    /// A number that grows with every filing. Two turns can file within
    /// one second, and "the newest" must still mean one thing.
    pub seq: i64,
}

/// What a prune did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneReport {
    /// Checkpoints dropped.
    pub dropped: usize,
    /// Size of the store after the repack.
    pub store_bytes: u64,
}

/// One checkpoint ref.
struct Stored {
    workspace: String,
    commit_id: String,
    /// Later of the commit's own date and its last filing.
    last_used: Filed,
}

impl CheckpointStore {
    /// Drop the checkpoints `policy` does not keep, then repack.
    ///
    /// `last_used` says, for a commit id, when a conversation last filed it
    /// as an undo point. An unchanged workspace reuses its
    /// previous checkpoint, so a commit made last week can be this morning's
    /// undo point; its age is counted from the later of the two. A checkpoint
    /// no conversation filed is as old as its commit.
    #[instrument(skip_all)]
    pub async fn prune(
        &self,
        policy: &PrunePolicy,
        now: i64,
        last_used: &HashMap<String, Filed>,
    ) -> Result<PruneReport> {
        if !self.dir.join("HEAD").exists() {
            return Ok(PruneReport::default());
        }

        let mut by_workspace = self.stored(last_used).await?;
        let mut dropped = 0;
        let cutoff = now.saturating_sub(policy.max_age_secs);
        dropped += self
            .keep_only(&mut by_workspace, policy.keep_per_workspace, cutoff)
            .await?;
        self.repack().await;

        if dir_size(&self.dir) > policy.max_store_bytes {
            let tight = (policy.keep_per_workspace / 4).max(1);
            let more = self.keep_only(&mut by_workspace, tight, cutoff).await?;
            if more > 0 {
                tracing::info!(
                    dropped = more,
                    "checkpoint store over its size limit: kept {tight} per workspace"
                );
                self.repack().await;
            }
            dropped += more;
        }

        Ok(PruneReport {
            dropped,
            store_bytes: dir_size(&self.dir),
        })
    }

    /// Every checkpoint in the store, per workspace, newest first.
    async fn stored(
        &self,
        last_used: &HashMap<String, Filed>,
    ) -> Result<HashMap<String, Vec<Stored>>> {
        let listing = self
            .store_git(
                &[
                    "for-each-ref",
                    "--format=%(refname) %(committerdate:unix)",
                    "refs/borg/snap/",
                ],
                super::GIT_BUDGET,
            )
            .await
            .context("list checkpoints")?;
        let mut by_workspace: HashMap<String, Vec<Stored>> = HashMap::new();
        for line in String::from_utf8_lossy(&listing).lines() {
            let Some((name, committed)) = line.split_once(' ') else {
                continue;
            };
            let mut parts = name
                .strip_prefix("refs/borg/snap/")
                .unwrap_or("")
                .split('/');
            let (Some(workspace), Some(commit_id), None) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            // Never filed: as old as its commit, and behind any filing made
            // in that same second.
            let committed = Filed {
                at: committed.trim().parse::<i64>().unwrap_or(0),
                seq: 0,
            };
            let filed = last_used.get(commit_id).copied().unwrap_or(committed);
            by_workspace
                .entry(workspace.to_string())
                .or_default()
                .push(Stored {
                    workspace: workspace.to_string(),
                    commit_id: commit_id.to_string(),
                    last_used: committed.max(filed),
                });
        }
        for stored in by_workspace.values_mut() {
            stored.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.last_used));
        }
        Ok(by_workspace)
    }

    /// Drop every checkpoint past position `keep` in its workspace or last
    /// used before `cutoff`. A workspace left with none loses its tip ref and
    /// its index too. Returns how many were dropped.
    async fn keep_only(
        &self,
        by_workspace: &mut HashMap<String, Vec<Stored>>,
        keep: usize,
        cutoff: i64,
    ) -> Result<usize> {
        let mut dropped = 0;
        for (workspace, stored) in by_workspace.iter_mut() {
            let kept = stored
                .iter()
                .take(keep)
                .take_while(|checkpoint| checkpoint.last_used.at >= cutoff)
                .count();
            if kept == stored.len() {
                continue;
            }
            // A snapshot of this workspace stages into its index and then
            // writes that index as a tree. Taking the index away in between
            // would give it an empty tree: a checkpoint that says the folder
            // had no files, and a rollback to it removes them all.
            let lock = super::workspace_lock_by_key(workspace);
            let _held = lock.lock().await;
            // The caller's record of use is older than this lock. A turn may
            // have been given a checkpoint since, and not have filed it yet.
            let kept = if self.count_hand_outs_as_used(workspace, stored) {
                stored
                    .iter()
                    .take(keep)
                    .take_while(|checkpoint| checkpoint.last_used.at >= cutoff)
                    .count()
            } else {
                kept
            };
            for gone in stored.drain(kept..) {
                let name = format!("refs/borg/snap/{}/{}", gone.workspace, gone.commit_id);
                self.store_git(&["update-ref", "-d", name.as_str()], super::GIT_BUDGET)
                    .await
                    .with_context(|| format!("drop checkpoint {}", gone.commit_id))?;
                dropped += 1;
            }
            if kept == 0 && self.has_no_checkpoints(workspace).await? {
                // The tip pointed at the newest checkpoint, now gone, and
                // would keep its files alive on its own.
                let tip = format!("refs/borg/tip/{workspace}");
                self.store_git(&["update-ref", "-d", tip.as_str()], super::GIT_BUDGET)
                    .await
                    .with_context(|| format!("drop the tip of workspace {workspace}"))?;
                for file in [
                    self.dir.join("indexes").join(workspace),
                    self.handed_out_file(workspace),
                ] {
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(e).with_context(|| format!("remove {}", file.display()))
                        }
                    }
                }
            }
        }
        by_workspace.retain(|_, stored| !stored.is_empty());
        Ok(dropped)
    }

    /// Count each checkpoint of `workspace` the store noted handing out as
    /// used at that time, and put `stored` back in order. True when anything
    /// changed. The caller holds the workspace lock.
    fn count_hand_outs_as_used(&self, workspace: &str, stored: &mut [Stored]) -> bool {
        let mut changed = false;
        for (commit_id, at) in self.handed_out(workspace) {
            // Ahead of any filing made in that same second: it is the newer use.
            let handed_out = Filed { at, seq: i64::MAX };
            if let Some(checkpoint) = stored
                .iter_mut()
                .find(|checkpoint| checkpoint.commit_id == commit_id)
                .filter(|checkpoint| checkpoint.last_used < handed_out)
            {
                checkpoint.last_used = handed_out;
                changed = true;
            }
        }
        if changed {
            stored.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.last_used));
        }
        changed
    }

    /// True when the store holds no checkpoint of `workspace` right now. The
    /// listing a prune works from is older than its own deletions: a snapshot
    /// taken since has a ref the prune never saw, and its tip and index must
    /// stay.
    async fn has_no_checkpoints(&self, workspace: &str) -> Result<bool> {
        let prefix = format!("refs/borg/snap/{workspace}/");
        let listed = self
            .store_git(
                &["for-each-ref", "--format=%(refname)", prefix.as_str()],
                super::GIT_BUDGET,
            )
            .await
            .with_context(|| format!("list the checkpoints of workspace {workspace}"))?;
        Ok(listed.iter().all(u8::is_ascii_whitespace))
    }

    /// Pack loose objects and delete the ones no checkpoint has needed for
    /// [`UNREACHABLE_GRACE`]. Snapshots never repack on their own (they run
    /// on the turn path), so this is the only place it happens.
    ///
    /// A repack that fails or runs out of time is logged and left for the
    /// next sweep. The checkpoints are already dropped by then, and the
    /// rest of the prune (the size limit, the caller's own bookkeeping)
    /// must still run.
    async fn repack(&self) {
        let packed = self
            .store_git(&["-c", UNREACHABLE_GRACE, "gc", "--quiet"], REPACK_BUDGET)
            .await;
        if let Err(e) = packed {
            tracing::warn!("checkpoint store not repacked this time: {e:#}");
        }
    }

    /// A git command on the store itself, with no work tree.
    async fn store_git(&self, args: &[&str], budget: Duration) -> Result<Vec<u8>> {
        let dir = self.dir.to_string_lossy();
        run_git_within(&self.dir, args, &[("GIT_DIR", dir.as_ref())], budget).await
    }
}

/// Bytes under `dir`. Unreadable entries count as nothing: this feeds a
/// size limit, and a limit that errors is a limit that never applies.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => pending.push(entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use crate::git::checkpoints::{RestoreOutcome, SnapshotOutcome};

    const DAY: i64 = 24 * 60 * 60;

    fn policy(keep_per_workspace: usize) -> PrunePolicy {
        PrunePolicy {
            keep_per_workspace,
            ..PrunePolicy::default()
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Each of `commits` filed at the same second `at`, in the order given
    /// — the worst case for telling newest from oldest.
    fn filed_in_order(commits: &[String], at: i64) -> HashMap<String, Filed> {
        commits
            .iter()
            .enumerate()
            .map(|(n, commit)| {
                let seq = i64::try_from(n).expect("small") + 1;
                (commit.clone(), Filed { at, seq })
            })
            .collect()
    }

    impl Fixture {
        /// A new checkpoint with `content` in `file.txt`.
        async fn checkpoint_of(&self, content: &str) -> String {
            self.write("file.txt", content);
            self.take().await.commit_id
        }

        fn refs(&self, prefix: &str) -> Vec<String> {
            let out = self.store_git(&["for-each-ref", "--format=%(refname)", prefix]);
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        }

        async fn can_restore(&self, commit_id: &str) -> bool {
            let outcome = self
                .store
                .restore(&self.ws, commit_id, &[])
                .await
                .expect("restore");
            matches!(outcome, RestoreOutcome::Restored(_))
        }

        /// The store's name for the fixture's one workspace.
        fn only_workspace(&self) -> String {
            self.only_index()
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        }

        /// The index file of the fixture's one workspace.
        fn only_index(&self) -> std::path::PathBuf {
            let mut indexes: Vec<_> = std::fs::read_dir(self.store.dir().join("indexes"))
                .expect("indexes")
                .map(|entry| entry.expect("entry").path())
                .collect();
            assert_eq!(indexes.len(), 1, "{indexes:?}");
            indexes.remove(0)
        }
    }

    /// A turn in a folder nobody touched for weeks is given the folder's old
    /// checkpoint again, and files it a moment later. A prune that read "who
    /// used what" before that moment must not drop it: the store's own note
    /// of the hand-out counts, whatever the caller's record says.
    #[tokio::test]
    async fn a_checkpoint_handed_out_since_the_callers_record_is_kept() {
        let fx = Fixture::new();
        let older = fx.checkpoint_of("first\n").await;
        let tip = fx.checkpoint_of("unchanged since\n").await;
        // The record the prune is given: the older one was filed last, the
        // tip never. Room for one.
        let last_used = HashMap::from([(
            older.clone(),
            Filed {
                at: now() + 60,
                seq: 9,
            },
        )]);
        // Handed out again, as it is to a turn in the unchanged folder; the
        // note says "after that record".
        assert_eq!(fx.checkpoint_of("unchanged since\n").await, tip);
        std::fs::write(
            fx.store.handed_out_file(&fx.only_workspace()),
            format!("{} {tip}\n", now() + 120),
        )
        .expect("note");

        let report = fx
            .store
            .prune(&policy(1), now() + 180, &last_used)
            .await
            .expect("prune");

        assert_eq!(report.dropped, 1);
        assert!(!fx.can_restore(&older).await);
        assert!(fx.can_restore(&tip).await);
    }

    /// The note names each commit. A turn is given the old tip, then the
    /// folder changes and the next turn's snapshot moves the tip: both
    /// hand-outs count, not only the one the tip points at now.
    #[tokio::test]
    async fn a_hand_out_counts_after_the_tip_has_moved_on() {
        let fx = Fixture::new();
        let oldest = fx.checkpoint_of("first\n").await;
        let handed_out = fx.checkpoint_of("second\n").await;
        let tip = fx.checkpoint_of("third\n").await;
        // The record the prune is given knows only the oldest. Room for two.
        let last_used = HashMap::from([(
            oldest.clone(),
            Filed {
                at: now() + 60,
                seq: 9,
            },
        )]);
        std::fs::write(
            fx.store.handed_out_file(&fx.only_workspace()),
            format!("{} {handed_out}\n{} {tip}\n", now() + 120, now() + 130),
        )
        .expect("note");

        let report = fx
            .store
            .prune(&policy(2), now() + 180, &last_used)
            .await
            .expect("prune");

        assert_eq!(report.dropped, 1);
        assert!(!fx.can_restore(&oldest).await);
        assert!(fx.can_restore(&handed_out).await);
        assert!(fx.can_restore(&tip).await);
    }

    /// The note is read line by line: one that is not `<time> <commit>` is
    /// passed over and the others still count. The next snapshot drops the
    /// lines older than an hour — a caller files its undo point within a
    /// moment, so nothing waits on them.
    #[tokio::test]
    async fn the_note_survives_a_bad_line_and_sheds_old_ones() {
        let fx = Fixture::new();
        let first = fx.checkpoint_of("v1\n").await;
        let key = fx.only_workspace();
        let long_ago = now() - 2 * 3600;
        std::fs::write(
            fx.store.handed_out_file(&key),
            format!("{long_ago} aaaa\nnot a note\n{} {first}\n", now()),
        )
        .expect("note");

        let commits = |notes: Vec<(String, i64)>| -> Vec<String> {
            notes.into_iter().map(|(commit, _)| commit).collect()
        };
        assert_eq!(
            commits(fx.store.handed_out(&key)),
            ["aaaa".to_string(), first.clone()]
        );

        let second = fx.checkpoint_of("v2\n").await;
        assert_eq!(commits(fx.store.handed_out(&key)), [first, second]);
    }

    /// A snapshot notes the hand-out itself, and a forgotten workspace takes
    /// the note with it.
    #[tokio::test]
    async fn the_store_notes_each_hand_out_and_forgets_it_with_the_workspace() {
        let fx = Fixture::new();
        let before = now();
        let taken = fx.checkpoint_of("v1\n").await;
        let key = fx.only_workspace();

        let noted = fx.store.handed_out(&key);
        let [(commit, at)] = noted.as_slice() else {
            panic!("one note, got {noted:?}");
        };
        assert_eq!(commit, &taken);
        assert!((before..=now()).contains(at), "{at}");

        // The next checkpoint is noted beside the first, and one handed out
        // twice is noted once.
        let next = fx.checkpoint_of("v2\n").await;
        assert_eq!(fx.checkpoint_of("v2\n").await, next);
        let commits: Vec<String> = fx
            .store
            .handed_out(&key)
            .into_iter()
            .map(|(commit, _)| commit)
            .collect();
        assert_eq!(commits, [taken, next]);

        fx.store
            .prune(&PrunePolicy::default(), now() + 30 * DAY, &HashMap::new())
            .await
            .expect("prune");

        assert_eq!(fx.store.handed_out(&key), Vec::new());
        assert!(!fx.store.handed_out_file(&key).exists());
    }

    /// A workspace's index goes when its last checkpoint does. A snapshot in
    /// progress is using that index, so the prune waits for it — and a
    /// checkpoint that snapshot made keeps the workspace known.
    #[tokio::test]
    async fn a_prune_waits_for_a_snapshot_of_the_workspace_it_would_forget() {
        let fx = Fixture::new();
        fx.checkpoint_of("old\n").await;
        let index = fx.only_index();
        let lock = super::super::workspace_lock(&fx.ws);
        let snapshot_in_progress = lock.lock().await;

        let store = fx.store.clone();
        let later = now() + 30 * DAY;
        let pruning = tokio::spawn(async move {
            store
                .prune(&PrunePolicy::default(), later, &HashMap::new())
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!pruning.is_finished(), "the prune ran under a snapshot");
        assert!(index.exists(), "the index went while a snapshot held it");

        // The snapshot finishes with a checkpoint the prune never listed.
        fx.write("file.txt", "new\n");
        let fresh = fx
            .store
            .snapshot_locked(&fx.store.workspace(fx.ws.clone()).expect("workspace"))
            .await
            .expect("snapshot");
        drop(snapshot_in_progress);
        let report = pruning.await.expect("join").expect("prune");

        assert_eq!(report.dropped, 1, "only the aged-out checkpoint");
        let SnapshotOutcome::Taken(fresh) = fresh else {
            panic!("skipped");
        };
        assert!(fx.can_restore(&fresh.commit_id).await);
        assert!(
            index.exists(),
            "a workspace with a checkpoint keeps its index"
        );
        assert_eq!(fx.refs("refs/borg/tip/").len(), 1);
    }

    /// Past the count, the oldest checkpoints of a workspace go and the
    /// newest stay restorable. Dropping one must not take another's files
    /// with it, even after the repack.
    #[tokio::test]
    async fn the_newest_checkpoints_of_a_workspace_are_kept() {
        let fx = Fixture::new();
        let mut made = Vec::new();
        for n in 0..5 {
            made.push(fx.checkpoint_of(&format!("version {n}\n")).await);
        }
        let last_used = filed_in_order(&made, now());

        let report = fx
            .store
            .prune(&policy(2), now(), &last_used)
            .await
            .expect("prune");

        assert_eq!(report.dropped, 3);
        assert!(report.store_bytes > 0);
        assert_eq!(fx.refs("refs/borg/snap/").len(), 2);
        for gone in &made[..3] {
            assert!(!fx.can_restore(gone).await, "{gone} should be gone");
        }
        assert!(fx.can_restore(&made[3]).await);
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("file.txt")).expect("read"),
            "version 3\n"
        );
        assert!(fx.can_restore(&made[4]).await);
    }

    /// Age is counted from when a conversation last filed the checkpoint,
    /// not from when its commit was made: an untouched project reuses last
    /// week's commit as this morning's undo point, and pruning it tonight
    /// would leave this morning's turn with nothing to go back to.
    #[tokio::test]
    async fn a_checkpoint_filed_recently_is_kept_however_old_its_commit() {
        let fx = Fixture::new();
        let reused = fx.checkpoint_of("unchanged for weeks\n").await;
        let stale = fx.checkpoint_of("nobody came back to this\n").await;
        // Seen from ten days on, both commits are past the week…
        let later = now() + 10 * DAY;
        // …but a turn filed the first one again an hour before the prune.
        let last_used = HashMap::from([(
            reused.clone(),
            Filed {
                at: later - 3_600,
                seq: 1,
            },
        )]);

        let report = fx
            .store
            .prune(&policy(20), later, &last_used)
            .await
            .expect("prune");

        assert_eq!(report.dropped, 1);
        // Asked first: a restore records the files as it finds them, and
        // they are still as the dropped checkpoint had them.
        assert!(!fx.can_restore(&stale).await);
        assert!(fx.can_restore(&reused).await);
    }

    /// When every checkpoint of a workspace has aged out, nothing of that
    /// workspace is left to keep its files alive: no tip ref, no index. The
    /// next snapshot of it starts clean and still works.
    #[tokio::test]
    async fn a_workspace_with_no_checkpoints_left_is_forgotten_and_can_start_again() {
        let fx = Fixture::new();
        fx.checkpoint_of("one\n").await;
        fx.checkpoint_of("two\n").await;
        let indexes = fx.store.dir().join("indexes");
        assert_eq!(std::fs::read_dir(&indexes).expect("read_dir").count(), 1);

        let report = fx
            .store
            .prune(&policy(20), now() + 30 * DAY, &HashMap::new())
            .await
            .expect("prune");

        assert_eq!(report.dropped, 2);
        assert_eq!(fx.refs("refs/borg/"), Vec::<String>::new());
        assert_eq!(std::fs::read_dir(&indexes).expect("read_dir").count(), 0);

        let fresh = fx.checkpoint_of("three\n").await;
        assert!(fx.can_restore(&fresh).await);
    }

    /// One workspace's count does not eat another's.
    #[tokio::test]
    async fn the_count_is_per_workspace() {
        let fx = Fixture::new();
        fx.checkpoint_of("a1\n").await;
        fx.checkpoint_of("a2\n").await;
        let other = fx.ws.parent().expect("parent").join("other");
        std::fs::create_dir(&other).expect("mkdir");
        std::fs::write(other.join("b.txt"), "b1\n").expect("write");
        fx.store.snapshot(&other).await.expect("snapshot");

        let report = fx
            .store
            .prune(&policy(1), now(), &HashMap::new())
            .await
            .expect("prune");

        assert_eq!(report.dropped, 1);
        assert_eq!(fx.refs("refs/borg/snap/").len(), 2);
        assert_eq!(fx.refs("refs/borg/tip/").len(), 2);
    }

    /// A store over its size limit is cut to a quarter of the usual count
    /// per workspace — never to nothing: the latest undo point survives.
    #[tokio::test]
    async fn a_store_over_its_size_limit_keeps_fewer_per_workspace() {
        let fx = Fixture::new();
        let mut made = Vec::new();
        for n in 0..9 {
            made.push(fx.checkpoint_of(&format!("version {n}\n")).await);
        }
        let last_used = filed_in_order(&made, now());
        let tiny = PrunePolicy {
            keep_per_workspace: 8,
            max_store_bytes: 1,
            ..PrunePolicy::default()
        };

        let report = fx
            .store
            .prune(&tiny, now(), &last_used)
            .await
            .expect("prune");

        // 9 → 8 by count, then 8 → 2 under size pressure.
        assert_eq!(report.dropped, 7);
        assert!(fx.can_restore(&made[8]).await);
        assert!(fx.can_restore(&made[7]).await);
        assert!(!fx.can_restore(&made[6]).await);

        let one = PrunePolicy {
            keep_per_workspace: 1,
            max_store_bytes: 1,
            ..PrunePolicy::default()
        };
        fx.store
            .prune(&one, now(), &last_used)
            .await
            .expect("prune");
        assert_eq!(fx.refs("refs/borg/snap/").len(), 1);
    }

    /// Before the first snapshot there is no store, and a prune neither
    /// fails nor creates one.
    #[tokio::test]
    async fn pruning_a_store_that_was_never_used_does_nothing() {
        let fx = Fixture::new();

        let report = fx
            .store
            .prune(&PrunePolicy::default(), now(), &HashMap::new())
            .await
            .expect("prune");

        assert_eq!(report, PruneReport::default());
        assert!(!fx.store.dir().exists());
    }

    #[test]
    fn dir_size_adds_up_every_file_below_a_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("a/b")).expect("mkdir");
        std::fs::write(tmp.path().join("top"), [0u8; 10]).expect("write");
        std::fs::write(tmp.path().join("a/b/deep"), [0u8; 32]).expect("write");

        assert_eq!(dir_size(tmp.path()), 42);
        assert_eq!(dir_size(&tmp.path().join("missing")), 0);
    }
}
