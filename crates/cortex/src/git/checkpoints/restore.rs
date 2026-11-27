//! Put a workspace back to a checkpoint.
//!
//! A restore never asks git to "make the directory look like this commit".
//! It first records the directory as it is now — so the restore itself can be
//! undone — then compares that record with the target and touches exactly the
//! paths that differ. Everything the store does not hold (ignored files,
//! oversize files, nested repositories) is therefore out of reach by
//! construction: a path that is in neither checkpoint is in no list.
//!
//! One case needs more than that. A file the target's ignore rules left out
//! can be in the current record when those rules changed since, and would
//! look like a file made after the target. So the target's ignore files go
//! back first, and whatever they ignore is left where it is.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::instrument;

use super::{
    path_to_git_bytes, spec_file, split_nul, workspace_lock, Checkpoint, CheckpointStore,
    SkipReason, SnapshotOutcome, Workspace,
};

/// Mode git gives a nested repository in a tree.
const GITLINK_MODE: &[u8] = b"160000";

/// Writing files back is the one step a person waits for on purpose, and a
/// timeout halfway leaves the directory between two states. Minutes, not the
/// seconds a snapshot command gets.
const WRITE_BACK_BUDGET: Duration = Duration::from_secs(300);

/// The file that holds a directory's ignore rules.
const IGNORE_FILE: &str = ".gitignore";

/// Result of [`CheckpointStore::restore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The files are back. The report says which.
    Restored(RestoreReport),
    /// The store has no such checkpoint for this workspace: it was pruned,
    /// or the id belongs to another directory. Nothing was touched.
    Gone,
    /// The directory is one the store does not take. Nothing was touched.
    Refused(SkipReason),
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The workspace as it was just before the restore. Restoring this one
    /// undoes the restore.
    pub before: Checkpoint,
    /// Files written back from the checkpoint, relative to the root.
    pub restored: Vec<PathBuf>,
    /// Files removed because the checkpoint does not have them.
    pub removed: Vec<PathBuf>,
    /// Paths that differ from the checkpoint and were left as found.
    pub left: Vec<LeftAlone>,
}

/// A path a restore did not touch, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftAlone {
    pub path: PathBuf,
    pub why: LeftReason,
}

/// Why a restore left a path as it found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftReason {
    /// One of the two checkpoints does not hold this path (a file over the
    /// size cap, a path inside a nested repository, or a file the target's
    /// ignore rules leave out), so there is no trustworthy "before" to put
    /// back.
    NotCovered,
    /// A nested repository.
    NestedRepository,
    /// Something the store does not track sits where the file would go.
    InTheWay,
    /// The file could not be removed or written back.
    Failed(String),
}

/// One difference between the target and the current state.
struct TreeChange {
    path: PathBuf,
    /// `A`: only in the current state. `D`: only in the target. Anything
    /// else: in both, with different content or type.
    status: u8,
    gitlink: bool,
}

impl CheckpointStore {
    /// Put `root` back to the checkpoint `commit_id`.
    ///
    /// `uncovered` is the target's own list of paths it does not hold
    /// ([`Checkpoint::uncovered`]). A file that was too large to capture then
    /// and is small enough now would otherwise look like a file the
    /// checkpoint "does not have", and be deleted.
    #[instrument(skip_all, fields(root = %root.display(), commit = commit_id))]
    pub async fn restore(
        &self,
        root: &Path,
        commit_id: &str,
        uncovered: &[PathBuf],
    ) -> Result<RestoreOutcome> {
        // The id reaches git's argv. An object id cannot be an option.
        if !is_object_id(commit_id) {
            return Ok(RestoreOutcome::Gone);
        }
        let Some(root) = self.project_root(root)? else {
            return Ok(RestoreOutcome::Refused(SkipReason::TooBroad));
        };
        let lock = workspace_lock(&root);
        let _held = lock.lock().await;
        let ws = self.open(root).await?;

        // Only a checkpoint of *this* workspace: any other commit in the
        // store is some other directory's files.
        if !self.checkpoints_of(&ws).await?.contains(commit_id) {
            return Ok(RestoreOutcome::Gone);
        }

        let before = match self.snapshot_locked(&ws).await? {
            SnapshotOutcome::Taken(checkpoint) => checkpoint,
            SnapshotOutcome::Skipped(why) => return Ok(RestoreOutcome::Refused(why)),
        };

        let raw = self
            .git(
                &ws,
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    commit_id,
                    before.commit_id.as_str(),
                ],
                &[],
            )
            .await
            .context("compare the workspace with the checkpoint")?;

        let mut left = Vec::new();
        let mut to_remove = Vec::new();
        let mut to_recreate = Vec::new();
        let mut to_write = Vec::new();
        for change in parse_changes(&raw)? {
            let not_covered = uncovered
                .iter()
                .chain(&before.uncovered)
                .any(|held_out| is_under(&change.path, held_out));
            if not_covered {
                left.push(LeftAlone {
                    path: change.path,
                    why: LeftReason::NotCovered,
                });
            } else if change.gitlink {
                left.push(LeftAlone {
                    path: change.path,
                    why: LeftReason::NestedRepository,
                });
            } else {
                match change.status {
                    b'A' => to_remove.push(change.path),
                    b'D' => to_recreate.push(change.path),
                    _ => to_write.push(change.path),
                }
            }
        }
        let mut removed = Vec::new();
        let mut restored = Vec::new();

        // The target's ignore rules come back before anything is removed.
        // "Only in the current state" means "made since the target" for a
        // file the target could have held, and nothing at all for a file its
        // rules ignored: that one was on disk all along.
        let (added_rules, to_remove): (Vec<_>, Vec<_>) =
            to_remove.into_iter().partition(|rel| is_ignore_file(rel));
        let (mut rules, mut to_write): (Vec<_>, Vec<_>) =
            to_write.into_iter().partition(|rel| is_ignore_file(rel));
        let (absent_rules, mut to_recreate): (Vec<_>, Vec<_>) =
            to_recreate.into_iter().partition(|rel| is_ignore_file(rel));
        // An ignore file whose directory is a file right now goes back with
        // the rest, once the removals have made room. It has nothing to say
        // before then: no file made since can be under a directory that
        // does not exist.
        let (waits_for_room, absent_rules): (Vec<_>, Vec<_>) = absent_rules
            .into_iter()
            .partition(|rel| is_blocked_above(&ws.root, rel));
        to_recreate.extend(waits_for_room);

        let wanted = rules.len() + absent_rules.len();
        rules.extend(free_places(&ws.root, absent_rules, &mut left));
        let mut rules_back = Vec::new();
        self.write_back(&ws, commit_id, rules, &mut rules_back, &mut left)
            .await;
        let rules_in_force = rules_back.len() == wanted;
        restored.append(&mut rules_back);

        // Without the target's rules on disk and readable there is no
        // telling a new file from an ignored one, and a wrong guess deletes
        // a file kept out of every checkpoint for good reason. Then nothing
        // is removed.
        let to_remove = if rules_in_force {
            // Rules made since the target are not the target's.
            let mut gone = Vec::new();
            remove_each(&ws.root, added_rules, &mut gone, &mut left);
            let ignored = self
                .ignored_at_target(&ws, &before.commit_id, &mut gone, &mut left)
                .await;
            removed.append(&mut gone);
            match ignored {
                Ok(ignored) => {
                    let (stay, go): (Vec<_>, Vec<_>) =
                        to_remove.into_iter().partition(|rel| ignored.contains(rel));
                    left.extend(stay.into_iter().map(|path| LeftAlone {
                        path,
                        why: LeftReason::NotCovered,
                    }));
                    go
                }
                Err(e) => {
                    tracing::warn!("rollback removes no files: ignore rules unreadable: {e:#}");
                    let why = format!("the undo point's ignore rules could not be read: {e}");
                    leave_each(to_remove, &why, &mut left);
                    Vec::new()
                }
            }
        } else {
            tracing::warn!("rollback removes no files: an ignore file could not be put back");
            let why = "the undo point's ignore rules could not be put back";
            leave_each(added_rules.into_iter().chain(to_remove), why, &mut left);
            Vec::new()
        };

        // Removals before write-backs: a file the checkpoint has may need
        // the place of a directory that only exists now.
        remove_each(&ws.root, to_remove, &mut removed, &mut left);

        // A path the checkpoint has and the current state does not, yet
        // something is there: the store was not tracking it (an ignored
        // file, most often). It is the user's, not ours to overwrite.
        to_write.extend(free_places(&ws.root, to_recreate, &mut left));
        self.write_back(&ws, commit_id, to_write, &mut restored, &mut left)
            .await;

        restored.sort();
        removed.sort();
        left.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(RestoreOutcome::Restored(RestoreReport {
            before,
            restored,
            removed,
            left,
        }))
    }

    /// Write `paths` back from `commit_id`, each name taken literally. A
    /// failure is reported against the paths, not returned: by this point
    /// the restore has already moved other files, and the caller must still
    /// hear which.
    async fn write_back(
        &self,
        ws: &Workspace,
        commit_id: &str,
        paths: Vec<PathBuf>,
        restored: &mut Vec<PathBuf>,
        left: &mut Vec<LeftAlone>,
    ) {
        if paths.is_empty() {
            return;
        }
        match self.checkout(ws, commit_id, &paths).await {
            Ok(()) => restored.extend(paths),
            Err(e) => {
                tracing::warn!("files not written back from the checkpoint: {e:#}");
                let why = e.root_cause().to_string();
                // git writes what it can and fails for the rest. Only a
                // file that still differs from the checkpoint was not
                // written; when even that cannot be told, none is claimed.
                let unwritten = self.differing_now(ws, commit_id).await;
                if let Err(e) = &unwritten {
                    tracing::warn!("the files could not be compared after that: {e:#}");
                }
                for path in paths {
                    if unwritten.as_ref().is_ok_and(|still| !still.contains(&path)) {
                        restored.push(path);
                    } else {
                        left.push(LeftAlone {
                            path,
                            why: LeftReason::Failed(why.clone()),
                        });
                    }
                }
            }
        }
    }

    /// The paths that differ between `commit_id` and the files as they are.
    /// Only looks, through an index of its own.
    async fn differing_now(&self, ws: &Workspace, commit_id: &str) -> Result<HashSet<PathBuf>> {
        let (view, _scratch) = self.scratch_view(ws)?;
        let (now, _) = self
            .write_tree(&view)
            .await?
            .map_err(|why| anyhow::anyhow!("the workspace cannot be read ({why:?})"))?;
        let names = self
            .git(
                ws,
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    "--name-only",
                    commit_id,
                    now.as_str(),
                ],
                &[],
            )
            .await
            .context("compare the workspace with the checkpoint")?;
        Ok(split_nul(&names)
            .into_iter()
            .map(super::super::path_from_git_bytes)
            .collect())
    }

    async fn checkout(&self, ws: &Workspace, commit_id: &str, paths: &[PathBuf]) -> Result<()> {
        let mut literal = Vec::new();
        for rel in paths {
            literal.extend_from_slice(&path_to_git_bytes(rel));
            literal.push(0);
        }
        let literal = spec_file(&literal)?;
        let from = format!("--pathspec-from-file={}", literal.path().display());
        self.git_within(
            ws,
            &[
                "checkout",
                "--quiet",
                commit_id,
                from.as_str(),
                "--pathspec-file-nul",
            ],
            // Each name is a file, never a pattern.
            &[("GIT_LITERAL_PATHSPECS", "1")],
            WRITE_BACK_BUDGET,
        )
        .await
        .context("write files back from the checkpoint")?;
        Ok(())
    }

    /// The files in the workspace's index that the target's ignore rules
    /// leave out.
    ///
    /// `gone` are the ignore files made since the target, already removed so
    /// that their rules do not count. One of them that the remaining rules
    /// ignore was not made since at all: it was on disk at the target, in no
    /// checkpoint. It comes back from `before` — the files as the restore
    /// found them — and is filed under `left`; its rules count again, so the
    /// question is asked once more.
    async fn ignored_at_target(
        &self,
        ws: &Workspace,
        before: &str,
        gone: &mut Vec<PathBuf>,
        left: &mut Vec<LeftAlone>,
    ) -> Result<HashSet<PathBuf>> {
        loop {
            let ignored = self.ignored(ws).await?;
            let (back, still_gone): (Vec<_>, Vec<_>) = std::mem::take(gone)
                .into_iter()
                .partition(|rel| ignored.contains(rel));
            *gone = still_gone;
            if back.is_empty() {
                return Ok(ignored);
            }
            match self.checkout(ws, before, &back).await {
                Ok(()) => left.extend(back.into_iter().map(|path| LeftAlone {
                    path,
                    why: LeftReason::NotCovered,
                })),
                Err(e) => {
                    let why = format!("could not be put back: {}", e.root_cause());
                    leave_each(back, &why, left);
                    return Err(e.context("put an ignore file back"));
                }
            }
        }
    }

    /// The files in the workspace's index that its ignore rules, as they are
    /// on disk right now, leave out.
    async fn ignored(&self, ws: &Workspace) -> Result<HashSet<PathBuf>> {
        let listed = self
            .git(
                ws,
                &[
                    "ls-files",
                    "--cached",
                    "--ignored",
                    "--exclude-standard",
                    "-z",
                ],
                &[],
            )
            .await
            .context("list the files the ignore rules leave out")?;
        Ok(split_nul(&listed)
            .into_iter()
            .map(super::super::path_from_git_bytes)
            .collect())
    }
}

impl CheckpointStore {
    /// How far each of `commit_ids` is from the files in `root` right now:
    /// for each, in order, how many files differ, or `None` when the store
    /// no longer has that checkpoint. The whole answer is `None` when `root`
    /// is a directory the store does not take.
    ///
    /// Only looks: the files as they are go into the store as a tree to
    /// compare against, but no checkpoint is made of them. Asking what could
    /// be undone must not use up one of the workspace's undo points.
    #[instrument(skip_all, fields(root = %root.display()))]
    pub async fn compare_with_now(
        &self,
        root: &Path,
        commit_ids: &[&str],
    ) -> Result<Option<Vec<Option<usize>>>> {
        let Some(root) = self.project_root(root)? else {
            return Ok(None);
        };
        let lock = workspace_lock(&root);
        let _held = lock.lock().await;
        let ws = self.open(root).await?;
        // A copy of the index: nothing here becomes a checkpoint, and the
        // workspace's own index must only ever name objects one keeps alive.
        let (view, _scratch) = self.scratch_view(&ws)?;
        let Ok((now, _)) = self.write_tree(&view).await? else {
            return Ok(None);
        };
        let known = self.checkpoints_of(&ws).await?;

        let mut differing = Vec::with_capacity(commit_ids.len());
        for commit_id in commit_ids {
            if !known.contains(*commit_id) {
                differing.push(None);
                continue;
            }
            let names = self
                .git(
                    &ws,
                    &[
                        "diff-tree",
                        "-r",
                        "-z",
                        "--no-renames",
                        "--name-only",
                        commit_id,
                        now.as_str(),
                    ],
                    &[],
                )
                .await
                .context("compare the workspace with a checkpoint")?;
            differing.push(Some(split_nul(&names).len()));
        }
        Ok(Some(differing))
    }

    /// The ids of every checkpoint the store holds for this workspace.
    async fn checkpoints_of(&self, ws: &Workspace) -> Result<HashSet<String>> {
        let prefix = format!("refs/borg/snap/{}/", ws.key);
        let listed = self
            .git_text(
                ws,
                &["for-each-ref", "--format=%(objectname)", prefix.as_str()],
                &[],
            )
            .await
            .context("list this workspace's checkpoints")?;
        Ok(listed.lines().map(str::to_string).collect())
    }
}

/// True for a full SHA-1 or SHA-256 object id in git's own spelling.
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse `diff-tree -r -z` output: `:<old mode> <new mode> <old id> <new id>
/// <status>` NUL `<path>` NUL, repeated.
fn parse_changes(raw: &[u8]) -> Result<Vec<TreeChange>> {
    let records = split_nul(raw);
    let mut changes = Vec::with_capacity(records.len() / 2);
    for pair in records.chunks(2) {
        let [meta, path] = pair else {
            anyhow::bail!("git diff-tree output ended in the middle of a record");
        };
        let mut fields = meta
            .strip_prefix(b":")
            .unwrap_or(meta)
            .split(|b| *b == b' ');
        let (Some(old_mode), Some(new_mode), Some(_), Some(_), Some(status)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            anyhow::bail!(
                "unreadable git diff-tree record: {}",
                String::from_utf8_lossy(meta)
            );
        };
        changes.push(TreeChange {
            path: super::super::path_from_git_bytes(path),
            status: status.first().copied().unwrap_or(b'M'),
            gitlink: old_mode == GITLINK_MODE || new_mode == GITLINK_MODE,
        });
    }
    Ok(changes)
}

/// True when `path` is `held_out` or inside it.
///
/// `held_out` may have been through the database, which keeps text: a name
/// that is not UTF-8 comes back with replacement characters. Compared in that
/// form as well, such a path is still recognised. The looser match can only
/// leave one more file alone.
fn is_under(path: &Path, held_out: &Path) -> bool {
    path.starts_with(held_out)
        || Path::new(path.to_string_lossy().as_ref())
            .starts_with(Path::new(held_out.to_string_lossy().as_ref()))
}

fn is_ignore_file(rel: &Path) -> bool {
    rel.file_name().is_some_and(|name| name == IGNORE_FILE)
}

/// Remove each of `paths`, filing it under `removed` or, when it could not
/// be removed, under `left`.
fn remove_each(
    root: &Path,
    paths: Vec<PathBuf>,
    removed: &mut Vec<PathBuf>,
    left: &mut Vec<LeftAlone>,
) {
    for rel in paths {
        match remove_file_and_empty_parents(root, &rel) {
            Ok(()) => removed.push(rel),
            Err(e) => left.push(LeftAlone {
                path: rel,
                why: LeftReason::Failed(e.to_string()),
            }),
        }
    }
}

/// File each of `paths` under `left` as not changed, for the reason `why`.
fn leave_each(paths: impl IntoIterator<Item = PathBuf>, why: &str, left: &mut Vec<LeftAlone>) {
    left.extend(paths.into_iter().map(|path| LeftAlone {
        path,
        why: LeftReason::Failed(why.to_string()),
    }));
}

/// True when nothing is at `root/rel` and a non-directory sits where one of
/// its parents must be.
fn is_blocked_above(root: &Path, rel: &Path) -> bool {
    std::fs::symlink_metadata(root.join(rel)).is_err() && is_occupied(root, rel)
}

/// Of `paths`, the ones nothing on disk is in the way of. The others are
/// filed under `left`.
fn free_places(root: &Path, paths: Vec<PathBuf>, left: &mut Vec<LeftAlone>) -> Vec<PathBuf> {
    let (occupied, free): (Vec<_>, Vec<_>) =
        paths.into_iter().partition(|rel| is_occupied(root, rel));
    left.extend(occupied.into_iter().map(|path| LeftAlone {
        path,
        why: LeftReason::InTheWay,
    }));
    free
}

/// Remove the file at `root/rel`, then each directory the removal left
/// empty, stopping at `root`. A file that is already gone is not an error.
fn remove_file_and_empty_parents(root: &Path, rel: &Path) -> std::io::Result<()> {
    let path = root.join(rel);
    // The current state was recorded a moment ago with every directory above
    // this file a real one. If one is a link now, the path no longer names
    // the file that was recorded, and it may name one outside the workspace.
    let mut above = path.parent();
    while let Some(dir) = above.filter(|dir| *dir != root) {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} is no longer a directory",
                    dir.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        above = dir.parent();
    }

    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    let mut above = path.parent();
    while let Some(dir) = above.filter(|dir| *dir != root) {
        // Fails on the first directory that still has something in it,
        // which is where the walk is meant to stop.
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        above = dir.parent();
    }
    Ok(())
}

/// True when writing `root/rel` would have to replace something: the path
/// itself exists, or a non-directory sits where one of its parents must be.
fn is_occupied(root: &Path, rel: &Path) -> bool {
    let mut at = root.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(part) = parts.next() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            // Nothing here: git creates the rest.
            Err(_) => return false,
            Ok(meta) if parts.peek().is_some() && meta.is_dir() => {}
            Ok(_) => return true,
        }
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use super::super::tests::Fixture;
    use super::*;
    use crate::git::test_support;

    /// What a directory holds: every file and link under it, with content
    /// (or link target) and whether it is executable.
    #[derive(Debug, PartialEq, Eq)]
    enum Entry {
        File { bytes: Vec<u8>, executable: bool },
        Link(PathBuf),
        EmptyDir,
    }

    fn contents(root: &Path) -> BTreeMap<PathBuf, Entry> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let mut any = false;
            for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
                any = true;
                let path = entry.path();
                let rel = path.strip_prefix(root).expect("under root").to_path_buf();
                let meta = std::fs::symlink_metadata(&path).expect("lstat");
                if meta.file_type().is_symlink() {
                    found.insert(
                        rel,
                        Entry::Link(std::fs::read_link(&path).expect("readlink")),
                    );
                } else if meta.is_dir() {
                    pending.push(path);
                } else {
                    found.insert(
                        rel,
                        Entry::File {
                            bytes: std::fs::read(&path).expect("read"),
                            executable: meta.permissions().mode() & 0o111 != 0,
                        },
                    );
                }
            }
            if !any && dir != root {
                let rel = dir.strip_prefix(root).expect("under root").to_path_buf();
                found.insert(rel, Entry::EmptyDir);
            }
        }
        found
    }

    impl Fixture {
        async fn restore(&self, target: &Checkpoint) -> RestoreReport {
            let outcome = self
                .store
                .restore(&self.ws, &target.commit_id, &target.uncovered)
                .await
                .expect("restore");
            match outcome {
                RestoreOutcome::Restored(report) => report,
                other => panic!("not restored: {other:?}"),
            }
        }

        fn remove(&self, rel: &str) {
            std::fs::remove_file(self.ws.join(rel)).expect("rm");
        }

        fn chmod(&self, rel: &str, mode: u32) {
            std::fs::set_permissions(self.ws.join(rel), std::fs::Permissions::from_mode(mode))
                .expect("chmod");
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn left(path: &str, why: LeftReason) -> LeftAlone {
        LeftAlone {
            path: PathBuf::from(path),
            why,
        }
    }

    /// The core promise: after a restore the directory holds what it held at
    /// the checkpoint, byte for byte — edits reverted, deleted files back,
    /// new files gone along with the directories made for them, and the
    /// executable bit as it was.
    #[tokio::test]
    async fn a_restore_puts_edited_deleted_and_new_files_back() {
        let fx = Fixture::new();
        fx.write("notes.txt", "one\r\ntwo\r\n");
        fx.write("src/lib.rs", "fn a() {}\n");
        fx.write("src/deep/gone.bin", b"\xff\xfe\x00raw");
        fx.write("run.sh", "#!/bin/sh\n");
        fx.chmod("run.sh", 0o755);
        let at_checkpoint = contents(&fx.ws);
        let checkpoint = fx.take().await;

        fx.write("notes.txt", "rewritten\n");
        fx.remove("src/deep/gone.bin");
        std::fs::remove_dir(fx.ws.join("src/deep")).expect("rmdir");
        fx.write("generated/a/b/new.txt", "made by the agent\n");
        fx.write("src/extra.rs", "fn b() {}\n");
        fx.write("run.sh", "#!/bin/sh\necho changed\n");
        fx.chmod("run.sh", 0o644);
        let edited = contents(&fx.ws);

        let report = fx.restore(&checkpoint).await;

        assert_eq!(contents(&fx.ws), at_checkpoint);
        assert_eq!(
            report.restored,
            paths(&["notes.txt", "run.sh", "src/deep/gone.bin"])
        );
        assert_eq!(
            report.removed,
            paths(&["generated/a/b/new.txt", "src/extra.rs"])
        );
        assert_eq!(report.left, Vec::new());

        // The state the restore replaced is itself a checkpoint, so the
        // restore can be undone.
        let again = fx.restore(&report.before).await;
        assert_eq!(contents(&fx.ws), edited);
        assert_eq!(
            fx.tree(&again.before.commit_id),
            fx.tree(&checkpoint.commit_id),
            "the state recorded before the second restore is the checkpoint's"
        );
    }

    /// A restore that finds nothing different changes nothing and says so.
    #[tokio::test]
    async fn restoring_the_current_state_is_a_no_op() {
        let fx = Fixture::new();
        fx.write("a.txt", "same\n");
        let checkpoint = fx.take().await;
        let before = contents(&fx.ws);

        let report = fx.restore(&checkpoint).await;

        assert_eq!(contents(&fx.ws), before);
        assert_eq!(report.before.commit_id, checkpoint.commit_id);
        assert_eq!(
            (report.restored, report.removed, report.left),
            (Vec::new(), Vec::new(), Vec::new())
        );
    }

    /// Files the store does not hold are the user's: an ignored file edited
    /// by hand, a dependency directory, a nested repository. A restore
    /// leaves each exactly as it is.
    #[tokio::test]
    async fn files_outside_the_checkpoints_survive_a_restore() {
        let fx = Fixture::new();
        fx.write(".gitignore", "*.log\nsecrets/\n");
        fx.write("app.txt", "v1\n");
        fx.write("debug.log", "old log\n");
        let checkpoint = fx.take().await;

        fx.write("app.txt", "v2\n");
        fx.write("debug.log", "edited by hand\n");
        fx.write("secrets/token", "do not lose\n");
        fx.write("node_modules/pkg/index.js", "module.exports = 1\n");
        fx.write("vendor/README", "nested\n");
        test_support::git(&fx.ws.join("vendor"), &["init", "-q"]);

        let report = fx.restore(&checkpoint).await;

        let now = contents(&fx.ws);
        let text = |rel: &str| match &now[Path::new(rel)] {
            Entry::File { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("{rel} is {other:?}"),
        };
        assert_eq!(text("app.txt"), "v1\n");
        assert_eq!(text("debug.log"), "edited by hand\n");
        assert_eq!(text("secrets/token"), "do not lose\n");
        assert_eq!(text("node_modules/pkg/index.js"), "module.exports = 1\n");
        assert_eq!(text("vendor/README"), "nested\n");
        assert!(fx.ws.join("vendor/.git").is_dir());
        assert_eq!(report.restored, paths(&["app.txt"]));
        assert_eq!(report.removed, Vec::<PathBuf>::new());
    }

    /// A turn that rewrites or deletes the ignore rules makes every file
    /// they kept out look new. Those files were there before the undo point
    /// and are in no checkpoint: a rollback that removed them as "made
    /// since" would delete them for good. The undo point's own rules decide.
    #[tokio::test]
    async fn a_file_the_undo_point_ignored_survives_a_change_of_ignore_rules() {
        for rules_now in [Some("*.tmp\n"), None] {
            let fx = Fixture::new();
            fx.write(".gitignore", ".env\nprivate/\n");
            fx.write(".env", "KEY=only copy\n");
            fx.write("private/token", "only copy\n");
            fx.write("app.txt", "v1\n");
            let checkpoint = fx.take().await;
            assert_eq!(fx.paths(&checkpoint.commit_id), [".gitignore", "app.txt"]);

            match rules_now {
                Some(rules) => fx.write(".gitignore", rules),
                None => fx.remove(".gitignore"),
            }
            fx.write("app.txt", "v2\n");
            fx.write("made.txt", "by the turn\n");

            let report = fx.restore(&checkpoint).await;

            let now = contents(&fx.ws);
            let text = |rel: &str| match now.get(Path::new(rel)) {
                Some(Entry::File { bytes, .. }) => String::from_utf8_lossy(bytes).into_owned(),
                other => panic!("{rel} is {other:?} (rules now: {rules_now:?})"),
            };
            assert_eq!(text(".env"), "KEY=only copy\n");
            assert_eq!(text("private/token"), "only copy\n");
            assert_eq!(text("app.txt"), "v1\n");
            assert_eq!(text(".gitignore"), ".env\nprivate/\n");
            assert!(!now.contains_key(Path::new("made.txt")), "{rules_now:?}");
            assert_eq!(report.restored, paths(&[".gitignore", "app.txt"]));
            assert_eq!(report.removed, paths(&["made.txt"]));
            assert_eq!(
                report.left,
                [
                    left(".env", LeftReason::NotCovered),
                    left("private/token", LeftReason::NotCovered),
                ]
            );
        }
    }

    /// An ignore file that the undo point's rules ignored is one of the
    /// files they kept out. It is not "an ignore file made since": it stays,
    /// with what it says.
    #[tokio::test]
    async fn an_ignore_file_the_undo_point_ignored_survives_too() {
        let fx = Fixture::new();
        fx.write(".gitignore", "build/\n");
        fx.write("build/.gitignore", "*.o\n");
        fx.write("build/keep.txt", "only copy\n");
        fx.write("app.txt", "v1\n");
        let checkpoint = fx.take().await;
        assert_eq!(fx.paths(&checkpoint.commit_id), [".gitignore", "app.txt"]);

        fx.remove(".gitignore");
        fx.write("app.txt", "v2\n");
        fx.write("build/made.o", "ignored by the file that stays\n");
        fx.write("made.txt", "by the turn\n");

        let report = fx.restore(&checkpoint).await;

        let now = contents(&fx.ws);
        let text = |rel: &str| match now.get(Path::new(rel)) {
            Some(Entry::File { bytes, .. }) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("{rel} is {other:?}"),
        };
        assert_eq!(text("build/.gitignore"), "*.o\n");
        assert_eq!(text("build/keep.txt"), "only copy\n");
        assert_eq!(text(".gitignore"), "build/\n");
        assert_eq!(text("app.txt"), "v1\n");
        assert!(!now.contains_key(Path::new("made.txt")));
        assert_eq!(report.restored, paths(&[".gitignore", "app.txt"]));
        assert_eq!(report.removed, paths(&["made.txt"]));
        assert_eq!(
            report.left,
            [
                left("build/.gitignore", LeftReason::NotCovered),
                left("build/keep.txt", LeftReason::NotCovered),
            ]
        );
    }

    /// When one of the undo point's ignore files cannot be put back, its
    /// rules are not there to ask. A rollback that went on would take the
    /// turn's rules for the undo point's and delete what they no longer
    /// hide. It removes nothing, and says so for each file.
    #[tokio::test]
    async fn nothing_is_removed_when_an_ignore_file_cannot_be_put_back() {
        let fx = Fixture::new();
        fx.write("sub/.gitignore", "deep/secret\n");
        fx.write("sub/deep/secret", "only copy\n");
        fx.write("app.txt", "v1\n");
        let checkpoint = fx.take().await;

        fx.write("sub/.gitignore", "");
        fx.write("app.txt", "v2\n");
        fx.write("made.txt", "by the turn\n");
        fx.chmod("sub", 0o555);
        // Permissions do not stop root; nothing to see then.
        if std::fs::write(fx.ws.join("sub/probe"), b"").is_ok() {
            fx.chmod("sub", 0o755);
            return;
        }

        let report = fx.restore(&checkpoint).await;
        fx.chmod("sub", 0o755);

        assert_eq!(
            std::fs::read_to_string(fx.ws.join("sub/deep/secret")).expect("read"),
            "only copy\n"
        );
        assert_eq!(report.removed, Vec::<PathBuf>::new());
        assert_eq!(report.restored, paths(&["app.txt"]));
        let not_changed: Vec<&Path> = report
            .left
            .iter()
            .filter(|left| matches!(left.why, LeftReason::Failed(_)))
            .map(|left| left.path.as_path())
            .collect();
        assert_eq!(
            not_changed,
            ["made.txt", "sub/.gitignore", "sub/deep/secret"].map(Path::new)
        );
        assert_eq!(report.left.len(), 3, "{:?}", report.left);
    }

    /// A directory with an ignore file that the turn replaced by a plain
    /// file: the ignore file cannot go back until the file is out of the
    /// way, and it does go back then.
    #[tokio::test]
    async fn an_ignore_file_goes_back_into_a_directory_that_became_a_file() {
        let fx = Fixture::new();
        fx.write("sub/.gitignore", "*.log\n");
        fx.write("sub/a.txt", "v1\n");
        let checkpoint = fx.take().await;

        std::fs::remove_dir_all(fx.ws.join("sub")).expect("rm -r");
        fx.write("sub", "now a file\n");
        fx.write("made.txt", "by the turn\n");

        let report = fx.restore(&checkpoint).await;

        assert_eq!(
            contents(&fx.ws).keys().collect::<Vec<_>>(),
            [Path::new("sub/.gitignore"), Path::new("sub/a.txt")]
        );
        assert_eq!(report.restored, paths(&["sub/.gitignore", "sub/a.txt"]));
        assert_eq!(report.removed, paths(&["made.txt", "sub"]));
        assert_eq!(report.left, []);
    }

    /// Ignore rules the turn added go away with the turn: a file they hide
    /// that an undo point already holds is still removed. (A file made and
    /// hidden in one turn was never held, and stays like any ignored file.)
    #[tokio::test]
    async fn ignore_rules_made_after_the_undo_point_protect_nothing() {
        let fx = Fixture::new();
        fx.write("app.txt", "v1\n");
        let checkpoint = fx.take().await;

        fx.write("made.txt", "by the turn\n");
        let unhidden = fx.take().await;
        assert!(fx
            .paths(&unhidden.commit_id)
            .contains(&"made.txt".to_string()));
        fx.write(".gitignore", "made.txt\n");

        let report = fx.restore(&checkpoint).await;

        assert_eq!(
            contents(&fx.ws).keys().collect::<Vec<_>>(),
            [Path::new("app.txt")]
        );
        assert_eq!(report.removed, paths(&[".gitignore", "made.txt"]));
        assert_eq!(report.left, []);
    }

    /// A file that cannot be written back does not take the report with it:
    /// the rollback still says what it removed and what it restored, and
    /// names the file it could not change.
    #[tokio::test]
    async fn a_file_that_cannot_be_written_back_is_reported_with_the_rest() {
        let fx = Fixture::new();
        fx.write("app.txt", "v1\n");
        fx.write("locked/file.txt", "v1\n");
        let checkpoint = fx.take().await;

        fx.write("app.txt", "v2\n");
        fx.write("locked/file.txt", "v2\n");
        fx.write("made.txt", "by the turn\n");
        fx.chmod("locked", 0o555);
        // Permissions do not stop root; nothing to see then.
        if std::fs::write(fx.ws.join("locked/probe"), b"").is_ok() {
            fx.chmod("locked", 0o755);
            return;
        }

        let report = fx.restore(&checkpoint).await;
        fx.chmod("locked", 0o755);

        assert_eq!(report.removed, paths(&["made.txt"]));
        assert_eq!(report.restored, paths(&["app.txt"]));
        assert_eq!(report.left.len(), 1, "{:?}", report.left);
        assert_eq!(report.left[0].path, Path::new("locked/file.txt"));
        assert!(
            matches!(&report.left[0].why, LeftReason::Failed(why) if !why.is_empty()),
            "{:?}",
            report.left
        );
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("locked/file.txt")).expect("read"),
            "v2\n"
        );
    }

    /// The checkpoint has a file; since then the file was deleted, became
    /// ignored and was created again. The store is not tracking what is
    /// there now, so the restore must not overwrite it — and must say it
    /// did not.
    #[tokio::test]
    async fn an_untracked_file_in_the_way_is_left_and_reported() {
        let fx = Fixture::new();
        fx.write("build.log", "tracked at the checkpoint\n");
        fx.write("out", "a file at the checkpoint\n");
        fx.write("keep.txt", "v1\n");
        let checkpoint = fx.take().await;

        fx.remove("build.log");
        fx.remove("out");
        fx.take().await;
        fx.write(".gitignore", "*.log\nout/\n");
        fx.write("build.log", "written after it became ignored\n");
        fx.write("out/artifact", "ignored build output\n");
        fx.write("keep.txt", "v2\n");

        let report = fx.restore(&checkpoint).await;

        assert_eq!(
            report.left,
            [
                left("build.log", LeftReason::InTheWay),
                left("out", LeftReason::InTheWay),
            ]
        );
        assert_eq!(report.restored, paths(&["keep.txt"]));
        assert_eq!(report.removed, paths(&[".gitignore"]));
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("build.log")).expect("read"),
            "written after it became ignored\n"
        );
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("out/artifact")).expect("read"),
            "ignored build output\n"
        );
    }

    /// A file too large to capture is in no checkpoint. Whether it was too
    /// large then and is small now, or the other way round, a restore
    /// neither deletes it nor replaces it with an old copy.
    #[tokio::test]
    async fn a_file_over_the_size_cap_at_either_end_is_left_alone() {
        let mut fx = Fixture::new();
        fx.store.max_file_bytes = 16;
        fx.write("was-big.dat", "x".repeat(64));
        fx.write("grows.dat", "small\n");
        fx.write("plain.txt", "v1\n");
        let checkpoint = fx.take().await;
        assert_eq!(checkpoint.uncovered, paths(&["was-big.dat"]));

        fx.write("was-big.dat", "now small\n");
        fx.write("grows.dat", "y".repeat(64));
        fx.write("plain.txt", "v2\n");

        let report = fx.restore(&checkpoint).await;

        assert_eq!(
            report.left,
            [
                left("grows.dat", LeftReason::NotCovered),
                left("was-big.dat", LeftReason::NotCovered),
            ]
        );
        assert_eq!(report.restored, paths(&["plain.txt"]));
        assert_eq!(report.removed, Vec::<PathBuf>::new());
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("was-big.dat")).expect("read"),
            "now small\n"
        );
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("grows.dat")).expect("read"),
            "y".repeat(64)
        );
    }

    /// Names are files, never patterns. A changed file called `*.txt` must
    /// not drag `a.txt` — which the restore has to leave alone — back with
    /// it.
    #[tokio::test]
    async fn a_file_name_with_pattern_characters_restores_only_itself() {
        let mut fx = Fixture::new();
        fx.store.max_file_bytes = 16;
        fx.write("*.txt", "star v1\n");
        fx.write("a.txt", "a v1\n");
        fx.write(":(glob)**", "magic v1\n");
        let checkpoint = fx.take().await;

        fx.write("*.txt", "star v2\n");
        fx.write(":(glob)**", "magic v2\n");
        fx.write("a.txt", "z".repeat(64));

        let report = fx.restore(&checkpoint).await;

        assert_eq!(report.restored, paths(&["*.txt", ":(glob)**"]));
        assert_eq!(report.left, [left("a.txt", LeftReason::NotCovered)]);
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("*.txt")).expect("read"),
            "star v1\n"
        );
        assert_eq!(
            std::fs::read_to_string(fx.ws.join(":(glob)**")).expect("read"),
            "magic v1\n"
        );
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("a.txt")).expect("read"),
            "z".repeat(64)
        );
    }

    /// A file that became a directory, a directory that became a file, and
    /// a link that became a file all go back to what they were. A link the
    /// agent added is removed as a link: what it points at is not followed.
    #[tokio::test]
    async fn files_directories_and_links_that_swapped_places_go_back() {
        let fx = Fixture::new();
        let outside = fx.ws.parent().expect("parent").join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("precious"), "not in the workspace\n").expect("write");

        fx.write("was-file", "a file\n");
        fx.write("was-dir/inner.txt", "inside\n");
        fx.write("target.txt", "pointed at\n");
        std::os::unix::fs::symlink("target.txt", fx.ws.join("was-link")).expect("symlink");
        let at_checkpoint = contents(&fx.ws);
        let checkpoint = fx.take().await;

        fx.remove("was-file");
        fx.write("was-file/now/nested.txt", "a directory now\n");
        std::fs::remove_dir_all(fx.ws.join("was-dir")).expect("rm -r");
        fx.write("was-dir", "a file now\n");
        fx.remove("was-link");
        fx.write("was-link", "a regular file now\n");
        std::os::unix::fs::symlink(&outside, fx.ws.join("escape")).expect("symlink");

        let report = fx.restore(&checkpoint).await;

        assert_eq!(contents(&fx.ws), at_checkpoint);
        assert_eq!(report.left, Vec::new());
        assert_eq!(
            std::fs::read_to_string(outside.join("precious")).expect("read"),
            "not in the workspace\n"
        );
    }

    /// A workspace can route its files through a filter program of its own.
    /// Writing files back is the moment git would run it; the store never
    /// does, and the bytes come back as they were.
    #[tokio::test]
    async fn a_workspace_content_filter_never_runs_on_restore() {
        let fx = Fixture::new();
        fx.write("notes.txt", "one\r\ntwo\r\n");
        test_support::git(&fx.ws, &["init", "-q"]);
        test_support::git(&fx.ws, &["config", "user.email", "t@example.com"]);
        test_support::git(&fx.ws, &["config", "user.name", "t"]);
        test_support::git(&fx.ws, &["add", "notes.txt"]);
        test_support::git(&fx.ws, &["commit", "-qm", "first"]);
        let sentinel_dir = fx.ws.parent().expect("parent").to_path_buf();
        let sentinel = test_support::arm_evil_content_filter(&fx.ws, &sentinel_dir);
        let checkpoint = fx.take().await;

        fx.write("notes.txt", "clobbered\n");
        fx.remove(".gitattributes");
        let report = fx.restore(&checkpoint).await;

        assert_eq!(report.restored, paths(&[".gitattributes", "notes.txt"]));
        assert_eq!(
            std::fs::read(fx.ws.join("notes.txt")).expect("read"),
            b"one\r\ntwo\r\n"
        );
        assert!(!sentinel.exists(), "the workspace's filter ran");
    }

    /// The project's own repository is a bystander to a restore: same HEAD,
    /// same index, same objects.
    #[tokio::test]
    async fn the_workspaces_own_repository_is_not_touched_by_a_restore() {
        let fx = Fixture::new();
        fx.write("tracked.txt", "v1\n");
        test_support::git(&fx.ws, &["init", "-q"]);
        test_support::git(&fx.ws, &["config", "user.email", "t@example.com"]);
        test_support::git(&fx.ws, &["config", "user.name", "t"]);
        test_support::git(&fx.ws, &["add", "."]);
        test_support::git(&fx.ws, &["commit", "-qm", "first"]);
        let checkpoint = fx.take().await;
        fx.write("tracked.txt", "v2\n");
        fx.write("new.txt", "new\n");
        let repo_before = contents(&fx.ws.join(".git"));

        fx.restore(&checkpoint).await;

        assert_eq!(contents(&fx.ws.join(".git")), repo_before);
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("tracked.txt")).expect("read"),
            "v1\n"
        );
        assert!(!fx.ws.join("new.txt").exists());
    }

    /// Only this workspace's own checkpoints can be restored into it.
    /// Another directory's checkpoint, a pruned one, and text that is not an
    /// object id are all "gone", and nothing on disk moves.
    #[tokio::test]
    async fn only_a_checkpoint_of_this_workspace_can_be_restored() {
        let fx = Fixture::new();
        fx.write("mine.txt", "mine\n");
        let own = fx.take().await;
        let other_ws = fx.ws.parent().expect("parent").join("other");
        std::fs::create_dir(&other_ws).expect("mkdir");
        std::fs::write(other_ws.join("theirs.txt"), "theirs\n").expect("write");
        let SnapshotOutcome::Taken(other) = fx.store.snapshot(&other_ws).await.expect("snapshot")
        else {
            panic!("skipped");
        };
        fx.write("mine.txt", "edited\n");
        let before = contents(&fx.ws);

        let pruned = "0".repeat(40);
        for id in [
            other.commit_id.as_str(),
            pruned.as_str(),
            "HEAD",
            "--help",
            "",
            &own.commit_id[..12],
        ] {
            let outcome = fx.store.restore(&fx.ws, id, &[]).await.expect("restore");
            assert_eq!(outcome, RestoreOutcome::Gone, "id {id:?}");
            assert_eq!(contents(&fx.ws), before, "id {id:?}");
        }
    }

    /// The list a person picks an undo point from says, for each one, how
    /// many files a restore would touch — and says nothing for one the store
    /// no longer has, so a dead point is never offered.
    #[tokio::test]
    async fn each_checkpoint_is_compared_with_the_files_as_they_are() {
        let fx = Fixture::new();
        fx.write("a.txt", "1\n");
        fx.write("b.txt", "1\n");
        let first = fx.take().await;
        fx.write("a.txt", "2\n");
        let second = fx.take().await;
        fx.write("b.txt", "2\n");
        fx.write("c.txt", "new\n");
        let pruned = "0".repeat(40);

        let checkpoints_before = fx.store_git(&["for-each-ref", "refs/borg/"]).stdout;
        let index = fx
            .store
            .dir()
            .join("indexes")
            .join(super::super::workspace_key(&fx.ws));
        let index_before = std::fs::read(&index).expect("index");

        let differing = fx
            .store
            .compare_with_now(
                &fx.ws,
                &[&second.commit_id, &first.commit_id, &pruned, "--help"],
            )
            .await
            .expect("compare")
            .expect("a project");

        // b.txt and c.txt since the second; a.txt as well since the first.
        assert_eq!(differing, [Some(2), Some(3), None, None]);
        // Looking is free: no checkpoint was made of the files as they are,
        // so asking does not use up one of the workspace's undo points.
        assert_eq!(
            fx.store_git(&["for-each-ref", "refs/borg/"]).stdout,
            checkpoints_before
        );
        // And leaves no trace in the workspace's index. The new c.txt was
        // stored to be compared; an index that named it would outlive the
        // object, which no checkpoint keeps from the next repack.
        assert!(
            std::fs::read(&index).expect("index") == index_before,
            "a comparison staged into the workspace's own index"
        );

        let home = dirs::home_dir().expect("home dir");
        let refused = fx
            .store
            .compare_with_now(&home, &[])
            .await
            .expect("compare");
        assert_eq!(refused, None);
    }

    /// A directory the store will not snapshot is refused before anything
    /// is compared or written.
    #[tokio::test]
    async fn a_directory_that_is_not_a_project_is_refused() {
        let fx = Fixture::new();
        fx.write("a.txt", "a\n");
        let checkpoint = fx.take().await;
        let home = dirs::home_dir().expect("home dir");

        let outcome = fx
            .store
            .restore(&home, &checkpoint.commit_id, &[])
            .await
            .expect("restore");

        assert_eq!(outcome, RestoreOutcome::Refused(SkipReason::TooBroad));
    }

    /// The walk that tidies up after a removed file stops at the first
    /// directory that still holds something, and never removes the root.
    #[test]
    fn removing_a_file_prunes_only_the_directories_it_emptied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b/c")).expect("mkdir");
        std::fs::write(root.join("a/b/c/gone.txt"), "x").expect("write");
        std::fs::write(root.join("a/stays.txt"), "x").expect("write");
        std::fs::write(root.join("top.txt"), "x").expect("write");

        remove_file_and_empty_parents(root, Path::new("a/b/c/gone.txt")).expect("remove");
        assert!(!root.join("a/b").exists());
        assert!(root.join("a/stays.txt").exists());

        remove_file_and_empty_parents(root, Path::new("a/stays.txt")).expect("remove");
        remove_file_and_empty_parents(root, Path::new("top.txt")).expect("remove");
        assert!(root.is_dir(), "the root itself is never removed");
        assert_eq!(std::fs::read_dir(root).expect("read_dir").count(), 0);

        // Already gone, with or without its directory: not an error.
        remove_file_and_empty_parents(root, Path::new("top.txt")).expect("remove");
        remove_file_and_empty_parents(root, Path::new("no/such/file")).expect("remove");
    }

    /// A directory that was swapped for a link after the state was recorded
    /// must not be walked through: the file it leads to is somewhere else.
    #[test]
    fn a_file_behind_a_link_is_not_removed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("ws");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("victim.txt"), "keep").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("dir")).expect("symlink");

        let result = remove_file_and_empty_parents(&root, Path::new("dir/victim.txt"));

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.txt")).expect("read"),
            "keep"
        );
    }

    #[test]
    fn diff_records_are_read_with_their_status_and_kind() {
        let raw = b":100644 100644 aaaa bbbb M\0changed file.txt\0\
                    :000000 100644 0000 cccc A\0new/\xffname\0\
                    :100644 000000 dddd 0000 D\0gone.txt\0\
                    :160000 000000 eeee 0000 D\0vendor\0\
                    :100644 120000 ffff 1111 T\0now-a-link\0";
        let changes = parse_changes(raw).expect("parse");
        let seen: Vec<(Vec<u8>, char, bool)> = changes
            .iter()
            .map(|c| (path_to_git_bytes(&c.path), c.status as char, c.gitlink))
            .collect();
        assert_eq!(
            seen,
            [
                (b"changed file.txt".to_vec(), 'M', false),
                (b"new/\xffname".to_vec(), 'A', false),
                (b"gone.txt".to_vec(), 'D', false),
                (b"vendor".to_vec(), 'D', true),
                (b"now-a-link".to_vec(), 'T', false),
            ]
        );

        assert!(parse_changes(b":100644 100644 aaaa bbbb M\0").is_err());
        assert!(parse_changes(b"garbage\0path\0").is_err());
    }
}
