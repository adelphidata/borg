//! Borg-owned snapshot store: undo points for the files in a workspace.
//!
//! A checkpoint is a commit in one bare repository under the data dir
//! (`<borg home>/checkpoints/store`), made by pointing git at the workspace
//! with `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`. Consequences that the
//! rest of this module leans on:
//!
//! * **The workspace's own `.git` is never read or written.** No loose
//!   objects pile up in the user's repository, and its `config` — the place a
//!   cloned tree can name hooks, filters and helpers — is not consulted.
//! * **A plain directory works as well as a repository.**
//! * **The agent's own shell cannot destroy an undo point**: the sandbox
//!   denies writes to the whole data dir.
//!
//! Each workspace gets its own index file (so a second snapshot only rehashes
//! what changed) and its own refs: `refs/borg/tip/<key>` is the latest
//! checkpoint, `refs/borg/snap/<key>/<commit>` keeps each one alive until it
//! is pruned. Commits are parentless, so dropping a ref drops the checkpoint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tracing::instrument;

/// Per-command ceiling. Higher than the turn-path budget in the parent module
/// because the first snapshot of a workspace hashes every file in it; the
/// caller bounds the snapshot as a whole.
const GIT_BUDGET: Duration = Duration::from_secs(30);

/// A file larger than this is left out of checkpoints (and reported as
/// uncovered): hashing a dataset or a build artifact on the turn path costs
/// more than the undo point is worth.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// More new or changed files than this and the workspace is not snapshotted:
/// it is a tree nobody meant to hand to an agent as one project.
const MAX_CHANGED_FILES: usize = 50_000;

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// Config for the store's own git calls, on top of the parent module's
/// hardening prelude. Content must round-trip byte for byte, and a snapshot
/// must never stop to repack.
const STORE_CONFIG: &[&str] = &["-c", "core.autocrlf=false", "-c", "gc.auto=0"];

/// `info/attributes` outranks every `.gitattributes` in the work tree. A
/// workspace must not be able to route its files through a filter, an
/// encoding or a line-ending conversion on the way into (or out of) the store.
const STORE_ATTRIBUTES: &str = "* -filter -text -eol -ident -working-tree-encoding\n";

/// Directories no checkpoint needs, for workspaces with no `.gitignore` of
/// their own. Deliberately short: a name on this list is silently not covered.
const STORE_EXCLUDES: &str = "node_modules/\n.venv/\n__pycache__/\n.DS_Store\n";

/// The identity on checkpoint commits. They are never pushed or shown, and
/// the operator's own identity must not be required to exist.
const IDENTITY_ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "borg"),
    ("GIT_AUTHOR_EMAIL", "borg@localhost"),
    ("GIT_COMMITTER_NAME", "borg"),
    ("GIT_COMMITTER_EMAIL", "borg@localhost"),
];

/// One async lock per workspace, process-wide: two snapshots of the same
/// tree would fight over its index file. Keyed by the workspace's name in the
/// store, because the prune knows a workspace by nothing else and must not
/// take its index away in the middle of a snapshot.
static WORKSPACE_LOCKS: LazyLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Serializes first-use creation of a store directory.
static INIT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

mod prune;
mod restore;

pub use prune::{Filed, PrunePolicy, PruneReport};
pub use restore::{LeftAlone, LeftReason, RestoreOutcome, RestoreReport};

/// The snapshot store under one Borg data dir.
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    dir: PathBuf,
    max_file_bytes: u64,
    max_changed_files: usize,
}

/// A point the workspace can be put back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// The commit in the store.
    pub commit_id: String,
    /// The directory it is a snapshot of (canonical).
    pub root: PathBuf,
    /// False when the workspace was byte-identical to its previous
    /// checkpoint: `commit_id` is then that earlier commit.
    pub is_new: bool,
    /// Paths under `root`, relative to it, that this checkpoint does not
    /// hold: nested repositories and files over the size cap. A restore must
    /// leave them exactly as it finds them.
    pub uncovered: Vec<PathBuf>,
}

/// Why a workspace was not snapshotted. None of these is a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The root is `/`, the home directory or one of its ancestors, or a
    /// directory that contains the store itself.
    TooBroad,
    /// More new or changed files than the store takes in one snapshot.
    TooManyFiles { files: usize },
}

/// Result of [`CheckpointStore::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Taken(Checkpoint),
    Skipped(SkipReason),
}

/// One workspace as the store sees it.
struct Workspace {
    root: PathBuf,
    key: String,
    index: PathBuf,
}

/// In the error of a git command that ran and exited non-zero, as opposed to
/// one that could not start or ran out of time.
const GIT_FAILED: &str = " failed";

/// What one attempt to write the workspace as a tree came to.
enum Staged {
    Tree {
        id: String,
        uncovered: Vec<PathBuf>,
    },
    Skipped(SkipReason),
    /// Staging worked and `write-tree` did not.
    IndexBroken(anyhow::Error),
}

impl CheckpointStore {
    /// The store for the data dir `borg_home`. Nothing is created until the
    /// first snapshot.
    pub fn new(borg_home: &Path) -> Self {
        Self {
            dir: borg_home.join("checkpoints").join("store"),
            max_file_bytes: MAX_FILE_BYTES,
            max_changed_files: MAX_CHANGED_FILES,
        }
    }

    /// Where the store lives; the directory to protect and to leave out of
    /// backups is its parent.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record the current state of `root` as a checkpoint.
    ///
    /// `Ok(Skipped(_))` means the directory is one the store will not take;
    /// `Err` means git failed or timed out. Neither may stop the caller's
    /// work: a missing undo point is a warning, not a refusal.
    #[instrument(skip_all, fields(root = %root.display()))]
    pub async fn snapshot(&self, root: &Path) -> Result<SnapshotOutcome> {
        let Some(root) = self.project_root(root)? else {
            return Ok(SnapshotOutcome::Skipped(SkipReason::TooBroad));
        };
        let lock = workspace_lock(&root);
        let _held = lock.lock().await;
        let ws = self.open(root).await?;
        self.snapshot_locked(&ws).await
    }

    /// `root` in canonical form, or `None` when it is a directory the store
    /// does not take.
    fn project_root(&self, root: &Path) -> Result<Option<PathBuf>> {
        self.project_root_with_home(root, dirs::home_dir().as_deref())
    }

    /// [`Self::project_root`] with the home directory named by the caller.
    fn project_root_with_home(&self, root: &Path, home: Option<&Path>) -> Result<Option<PathBuf>> {
        let root = root
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", root.display()))?;
        // `root` has its links resolved, so the two directories it is
        // measured against must have theirs resolved too: a home reached
        // through a link (`/home` -> `/data/home`) would otherwise pass as
        // an ordinary project, credentials and data dir included.
        let home = home.map(resolved);
        let too_broad = is_too_broad(&root, home.as_deref(), &resolved(&self.dir));
        Ok((!too_broad).then_some(root))
    }

    /// The store's view of `root`, creating the store on first use. The
    /// caller holds the workspace lock.
    async fn open(&self, root: PathBuf) -> Result<Workspace> {
        self.ensure_store().await?;
        self.workspace(root)
    }

    /// [`Self::snapshot`] for a caller that already holds the workspace lock.
    async fn snapshot_locked(&self, ws: &Workspace) -> Result<SnapshotOutcome> {
        let (tree, uncovered) = match self.write_tree(ws).await? {
            Ok(written) => written,
            Err(why) => return Ok(SnapshotOutcome::Skipped(why)),
        };

        let tip_ref = format!("refs/borg/tip/{}", ws.key);
        let tip = self
            .git_text(
                ws,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(tree)",
                    tip_ref.as_str(),
                ],
                &[],
            )
            .await?;
        if let Some((commit, tip_tree)) = tip.split_once(' ') {
            if tip_tree == tree {
                // The tip outlives its checkpoint when a prune drops that
                // one by count. Handed out again, it must be restorable.
                let snap_ref = format!("refs/borg/snap/{}/{commit}", ws.key);
                self.git(ws, &["update-ref", snap_ref.as_str(), commit], &[])
                    .await?;
                self.note_handed_out(ws, commit);
                return Ok(SnapshotOutcome::Taken(Checkpoint {
                    commit_id: commit.to_string(),
                    root: ws.root.clone(),
                    is_new: false,
                    uncovered,
                }));
            }
        }

        let commit_id = self
            .git_text(
                ws,
                &["commit-tree", tree.as_str(), "-m", "borg checkpoint"],
                &[],
            )
            .await?;
        let snap_ref = format!("refs/borg/snap/{}/{commit_id}", ws.key);
        for name in [snap_ref.as_str(), tip_ref.as_str()] {
            self.git(ws, &["update-ref", name, commit_id.as_str()], &[])
                .await?;
        }
        self.note_handed_out(ws, &commit_id);
        Ok(SnapshotOutcome::Taken(Checkpoint {
            commit_id,
            root: ws.root.clone(),
            is_new: true,
            uncovered,
        }))
    }

    /// Where the store keeps which checkpoints of the workspace it knows as
    /// `key` it handed out lately, and when.
    fn handed_out_file(&self, key: &str) -> PathBuf {
        self.dir.join("handed-out").join(key)
    }

    /// Note that the checkpoint `commit_id` of `ws` was handed out just now.
    ///
    /// A caller files its undo point in the database a moment after the
    /// snapshot returns, and a prune that read the database before that would
    /// take an unchanged folder's reused commit for one nobody has used in
    /// weeks. This note is written under the workspace lock the prune also
    /// takes, so the prune sees it whatever the database said. It names the
    /// commit: the tip may have moved on by the time the prune looks.
    ///
    /// Failing to write it loses only that protection; the checkpoint is
    /// good, so it is logged and the snapshot stands.
    fn note_handed_out(&self, ws: &Workspace, commit_id: &str) {
        let path = self.handed_out_file(&ws.key);
        let now = unix_now();
        let mut notes = self.handed_out(&ws.key);
        notes.retain(|(noted, at)| noted != commit_id && *at >= now - HAND_OUT_NOTE_SECS);
        notes.push((commit_id.to_string(), now));
        let body: String = notes
            .iter()
            .map(|(noted, at)| format!("{at} {noted}\n"))
            .collect();
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, body));
        if let Err(e) = written {
            tracing::warn!(path = %path.display(), "checkpoint use not noted: {e}");
        }
    }

    /// The checkpoints of the workspace `key` handed out lately, each with
    /// the unix second of it. Empty when none was noted. A note that cannot
    /// be read protects nothing, so that is logged.
    fn handed_out(&self, key: &str) -> Vec<(String, i64)> {
        let path = self.handed_out_file(key);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), "checkpoint use note not read: {e}");
                return Vec::new();
            }
        };
        text.lines()
            .filter_map(|line| {
                let note = line
                    .split_once(' ')
                    .and_then(|(at, commit)| Some((commit.to_string(), at.parse().ok()?)));
                if note.is_none() {
                    tracing::warn!(path = %path.display(), "checkpoint use note has a line that is not `<time> <commit>`: {line:?}");
                }
                note
            })
            .collect()
    }

    /// Write the workspace as it is into the store as a tree, and return the
    /// tree's id with the paths it leaves out. No commit and no ref: on its
    /// own this is the directory's content, not a checkpoint of it. The
    /// caller holds the workspace lock.
    async fn write_tree(
        &self,
        ws: &Workspace,
    ) -> Result<std::result::Result<(String, Vec<PathBuf>), SkipReason>> {
        match self.stage_and_write(ws).await? {
            Staged::Tree { id, uncovered } => return Ok(Ok((id, uncovered))),
            Staged::Skipped(why) => return Ok(Err(why)),
            // The index names an object the store no longer has: a staging
            // that never became a checkpoint, repacked away since. git does
            // not rehash a file it believes is staged, so every later
            // snapshot would fail the same way. Start the index again from
            // the files; the cost is one full rehash.
            Staged::IndexBroken(e) => {
                tracing::warn!(
                    index = %ws.index.display(),
                    "checkpoint index rebuilt from the files: {e:#}"
                );
                match std::fs::remove_file(&ws.index) {
                    Ok(()) => {}
                    Err(io) if io.kind() == std::io::ErrorKind::NotFound => {}
                    Err(io) => {
                        return Err(io).with_context(|| format!("remove {}", ws.index.display()))
                    }
                }
            }
        }
        match self.stage_and_write(ws).await? {
            Staged::Tree { id, uncovered } => Ok(Ok((id, uncovered))),
            Staged::Skipped(why) => Ok(Err(why)),
            Staged::IndexBroken(e) => Err(e.context("write the workspace as a tree")),
        }
    }

    /// One attempt at [`Self::write_tree`].
    async fn stage_and_write(&self, ws: &Workspace) -> Result<Staged> {
        let listing = self
            .git(
                ws,
                &[
                    "ls-files",
                    "--others",
                    "--modified",
                    "--exclude-standard",
                    "-z",
                ],
                &[],
            )
            .await
            .context("list changed files")?;
        let changed = split_nul(&listing);
        if changed.len() > self.max_changed_files {
            return Ok(Staged::Skipped(SkipReason::TooManyFiles {
                files: changed.len(),
            }));
        }
        let uncovered = self.uncovered(&ws.root, &changed);

        self.stage(ws, &uncovered).await?;
        match self.git_text(ws, &["write-tree"], &[]).await {
            Ok(id) => Ok(Staged::Tree { id, uncovered }),
            // git refused the index. A command that ran out of time says
            // nothing about the index, and a rebuild would only cost more.
            Err(e) if format!("{e:#}").contains(GIT_FAILED) => Ok(Staged::IndexBroken(e)),
            Err(e) => Err(e),
        }
    }

    /// `ws` with an index of its own: a copy of the workspace's, in a
    /// directory that goes away with the returned guard.
    ///
    /// For looking without recording. Staging into the real index and then
    /// making no checkpoint would leave it naming objects nothing keeps
    /// alive; the next repack deletes them and the index is broken.
    fn scratch_view(&self, ws: &Workspace) -> Result<(Workspace, tempfile::TempDir)> {
        let scratch = tempfile::tempdir().context("create a scratch index directory")?;
        let index = scratch.path().join("index");
        match std::fs::copy(&ws.index, &index) {
            Ok(_) => {}
            // No snapshot yet: git starts from an empty index.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("copy {}", ws.index.display())),
        }
        let view = Workspace {
            root: ws.root.clone(),
            key: ws.key.clone(),
            index,
        };
        Ok((view, scratch))
    }

    /// Create the bare repository on first use and (re)write the two `info/`
    /// files that make it safe to point at an arbitrary work tree.
    async fn ensure_store(&self) -> Result<()> {
        let _held = INIT_LOCK.lock().await;
        // All three: a `git init` killed at its budget can leave `HEAD`
        // behind with nothing else, and running it again completes it.
        let made = self.dir.join("HEAD").exists()
            && self.dir.join("objects").is_dir()
            && self.dir.join("refs").is_dir();
        if !made {
            std::fs::create_dir_all(&self.dir)
                .with_context(|| format!("create {}", self.dir.display()))?;
            let dir = self.dir.to_string_lossy().into_owned();
            run_git(&self.dir, &["init", "--bare", "--quiet", dir.as_str()], &[])
                .await
                .context("create the checkpoint store")?;
        }
        for (name, content) in [
            ("attributes", STORE_ATTRIBUTES),
            ("exclude", STORE_EXCLUDES),
        ] {
            let path = self.dir.join("info").join(name);
            if std::fs::read_to_string(&path).is_ok_and(|have| have == content) {
                continue;
            }
            std::fs::create_dir_all(self.dir.join("info"))?;
            std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
        }
        Ok(())
    }

    fn workspace(&self, root: PathBuf) -> Result<Workspace> {
        let key = workspace_key(&root);
        let indexes = self.dir.join("indexes");
        std::fs::create_dir_all(&indexes)
            .with_context(|| format!("create {}", indexes.display()))?;
        let index = indexes.join(&key);
        // A git killed at the budget leaves its lock behind, and every later
        // snapshot of this workspace would fail on it: the index's, or the
        // one on the workspace's tip ref. The workspace lock is held and one
        // daemon owns a data dir (the daemon lock), so nothing of ours can
        // still own either.
        let tip_lock = self
            .dir
            .join("refs")
            .join("borg")
            .join("tip")
            .join(format!("{key}.lock"));
        for lock in [indexes.join(format!("{key}.lock")), tip_lock] {
            match std::fs::remove_file(&lock) {
                Ok(()) => {
                    tracing::warn!(lock = %lock.display(), "removed a stale checkpoint lock")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("remove {}", lock.display())),
            }
        }
        Ok(Workspace { root, key, index })
    }

    /// Of the changed paths git listed, the ones a checkpoint must not hold.
    /// git lists a nested repository as one entry with a trailing slash and
    /// does not descend into it.
    fn uncovered(&self, root: &Path, changed: &[&[u8]]) -> Vec<PathBuf> {
        changed
            .iter()
            .filter_map(|record| match record.strip_suffix(b"/") {
                Some(nested_repo) => Some(super::path_from_git_bytes(nested_repo)),
                None => {
                    let rel = super::path_from_git_bytes(record);
                    let oversize = std::fs::symlink_metadata(root.join(&rel))
                        .is_ok_and(|meta| meta.is_file() && meta.len() > self.max_file_bytes);
                    oversize.then_some(rel)
                }
            })
            .collect()
    }

    /// Bring the workspace's index in line with the directory, minus the
    /// uncovered paths.
    async fn stage(&self, ws: &Workspace, uncovered: &[PathBuf]) -> Result<()> {
        if uncovered.is_empty() {
            self.git(ws, &["add", "--all", "--", "."], &[])
                .await
                .context("stage the workspace")?;
            return Ok(());
        }

        // A path that was captured before and is uncovered now (a file that
        // grew past the cap) must leave the index, or the checkpoint would
        // carry its old content as if it were current.
        let mut literal = Vec::new();
        // Everything, except each uncovered path taken literally.
        let mut pathspec = b".\0".to_vec();
        for rel in uncovered {
            let bytes = path_to_git_bytes(rel);
            literal.extend_from_slice(&bytes);
            literal.push(0);
            pathspec.extend_from_slice(b":(exclude,literal)");
            pathspec.extend_from_slice(&bytes);
            pathspec.push(0);
        }
        let literal = spec_file(&literal)?;
        let pathspec = spec_file(&pathspec)?;
        let from = |file: &tempfile::NamedTempFile| {
            format!("--pathspec-from-file={}", file.path().display())
        };

        self.git(
            ws,
            &[
                "rm",
                "--cached",
                // Index-only, so nothing on disk is at stake; without it git
                // refuses a path whose file changed since it was staged.
                "--force",
                "-r",
                "--quiet",
                "--ignore-unmatch",
                from(&literal).as_str(),
                "--pathspec-file-nul",
            ],
            &[("GIT_LITERAL_PATHSPECS", "1")],
        )
        .await
        .context("drop uncovered paths from the checkpoint index")?;
        self.git(
            ws,
            &[
                "add",
                "--all",
                from(&pathspec).as_str(),
                "--pathspec-file-nul",
            ],
            &[],
        )
        .await
        .context("stage the workspace")?;
        Ok(())
    }

    async fn git(&self, ws: &Workspace, args: &[&str], env: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.git_within(ws, args, env, GIT_BUDGET).await
    }

    /// [`Self::git`] with a budget of the caller's choosing.
    async fn git_within(
        &self,
        ws: &Workspace,
        args: &[&str],
        env: &[(&str, &str)],
        budget: Duration,
    ) -> Result<Vec<u8>> {
        let dir = self.dir.to_string_lossy();
        let root = ws.root.to_string_lossy();
        let index = ws.index.to_string_lossy();
        let mut all_env: Vec<(&str, &str)> = vec![
            ("GIT_DIR", dir.as_ref()),
            ("GIT_WORK_TREE", root.as_ref()),
            ("GIT_INDEX_FILE", index.as_ref()),
        ];
        all_env.extend_from_slice(env);
        run_git_within(&ws.root, args, &all_env, budget).await
    }

    async fn git_text(
        &self,
        ws: &Workspace,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String> {
        let stdout = self.git(ws, args, env).await?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

/// Run one git command for the store: the parent module's hardening, no
/// global or system config, the store's own config and identity.
async fn run_git(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Vec<u8>> {
    run_git_within(cwd, args, env, GIT_BUDGET).await
}

/// [`run_git`] with a budget of the caller's choosing.
async fn run_git_within(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    budget: Duration,
) -> Result<Vec<u8>> {
    let mut argv: Vec<&str> = super::hardened_prelude().to_vec();
    argv.extend_from_slice(STORE_CONFIG);
    argv.extend(super::harden_git_argv(args));
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&argv).current_dir(cwd);
    super::harden_git_command(&mut cmd);
    cmd.env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for (key, value) in IDENTITY_ENV.iter().chain(env) {
        cmd.env(key, value);
    }
    // The subcommand, past any `-c name=value` the caller put before it.
    let verb = args
        .iter()
        .copied()
        .scan(false, |skip, arg| {
            let is_option = *skip || arg == "-c";
            *skip = arg == "-c";
            Some((is_option, arg))
        })
        .find_map(|(is_option, arg)| (!is_option).then_some(arg))
        .unwrap_or("git");
    let output = super::run_with_budget(cmd, budget)
        .await
        .map_err(|_| anyhow::anyhow!("git {verb} timed out after {budget:?}"))?
        .with_context(|| format!("run git {verb}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {verb}{GIT_FAILED}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// True when `root` is not a project but a place projects live in: the
/// filesystem root, the home directory, anything above it, or a directory
/// that holds the store. Snapshotting any of those would hash the user's
/// whole disk, or the store into itself.
fn is_too_broad(root: &Path, home: Option<&Path>, store_dir: &Path) -> bool {
    root.parent().is_none()
        || home.is_some_and(|home| home.starts_with(root))
        || store_dir.starts_with(root)
}

/// Stable name for a workspace inside the store: the first 16 hex digits of
/// the SHA-256 of its canonical path.
fn workspace_key(root: &Path) -> String {
    let digest = Sha256::digest(path_to_git_bytes(root));
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn workspace_lock(root: &Path) -> Arc<tokio::sync::Mutex<()>> {
    workspace_lock_by_key(&workspace_key(root))
}

/// How long a hand-out note is kept. A caller files its undo point within a
/// moment of the hand-out; an hour is that with room to spare.
const HAND_OUT_NOTE_SECS: i64 = 3600;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The lock of the workspace the store knows as `key`.
fn workspace_lock_by_key(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = WORKSPACE_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(key.to_string()).or_default())
}

/// `path` with its links resolved, as far as it exists: the part that does
/// not exist yet (a store before its first snapshot) is kept as written.
fn resolved(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = at.canonicalize() {
            return missing
                .iter()
                .rev()
                .fold(real, |so_far, name| so_far.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn split_nul(stdout: &[u8]) -> Vec<&[u8]> {
    stdout
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
        .collect()
}

fn path_to_git_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

fn spec_file(content: &[u8]) -> Result<tempfile::NamedTempFile> {
    use std::io::Write as _;
    let mut file = tempfile::NamedTempFile::new().context("create a pathspec file")?;
    file.write_all(content).context("write a pathspec file")?;
    file.flush().context("write a pathspec file")?;
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git::test_support;

    /// A store and a workspace in one tempdir, outside `$HOME`'s ancestry.
    pub(super) struct Fixture {
        _tmp: tempfile::TempDir,
        pub(super) store: CheckpointStore,
        pub(super) ws: PathBuf,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let base = tmp.path().canonicalize().expect("canonicalize");
            let ws = base.join("project");
            std::fs::create_dir(&ws).expect("mkdir workspace");
            Self {
                store: CheckpointStore::new(&base.join("borg-home")),
                ws,
                _tmp: tmp,
            }
        }

        pub(super) fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
            let path = self.ws.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, content).expect("write");
        }

        pub(super) async fn take(&self) -> Checkpoint {
            match self.store.snapshot(&self.ws).await.expect("snapshot") {
                SnapshotOutcome::Taken(checkpoint) => checkpoint,
                SnapshotOutcome::Skipped(why) => panic!("skipped: {why:?}"),
            }
        }

        pub(super) fn store_git(&self, args: &[&str]) -> std::process::Output {
            std::process::Command::new("git")
                .args(args)
                .env("GIT_DIR", self.store.dir())
                .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("spawn git")
        }

        /// Every path in a checkpoint's tree, with its mode.
        pub(super) fn tree(&self, commit: &str) -> Vec<(String, String)> {
            let out = self.store_git(&["ls-tree", "-r", commit]);
            assert!(out.status.success(), "ls-tree failed");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|line| {
                    let (meta, path) = line.split_once('\t').expect("ls-tree record");
                    let mode = meta.split(' ').next().expect("mode");
                    (path.to_string(), mode.to_string())
                })
                .collect()
        }

        pub(super) fn paths(&self, commit: &str) -> Vec<String> {
            self.tree(commit)
                .into_iter()
                .map(|(path, _)| path)
                .collect()
        }

        pub(super) fn blob(&self, commit: &str, rel: &str) -> Vec<u8> {
            let out = self.store_git(&["cat-file", "-p", &format!("{commit}:{rel}")]);
            assert!(out.status.success(), "{rel} is not in {commit}");
            out.stdout
        }
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// A plain directory — no repository anywhere — gets a checkpoint that
    /// holds its files byte for byte, including bytes git would otherwise
    /// "normalize" (CRLF, invalid UTF-8) and the executable bit.
    #[tokio::test]
    async fn a_plain_directory_is_captured_byte_for_byte() {
        let fx = Fixture::new();
        fx.write("notes.txt", "one\r\ntwo\r\n");
        fx.write("data/blob.bin", b"\xff\xfe\x00raw");
        fx.write("run.sh", "#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(fx.ws.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let checkpoint = fx.take().await;

        assert!(checkpoint.is_new);
        assert_eq!(checkpoint.root, fx.ws);
        assert_eq!(checkpoint.uncovered, Vec::<PathBuf>::new());
        assert_eq!(
            fx.blob(&checkpoint.commit_id, "notes.txt"),
            b"one\r\ntwo\r\n"
        );
        assert_eq!(
            fx.blob(&checkpoint.commit_id, "data/blob.bin"),
            b"\xff\xfe\x00raw"
        );
        #[cfg(unix)]
        assert!(fx
            .tree(&checkpoint.commit_id)
            .contains(&("run.sh".to_string(), "100755".to_string())));
    }

    /// An unchanged workspace yields the checkpoint it already has; an edit,
    /// a new file and a deletion each show up in the next one.
    #[tokio::test]
    async fn a_new_checkpoint_is_made_only_when_the_workspace_changed() {
        let fx = Fixture::new();
        fx.write("keep.txt", "v1\n");
        fx.write("drop.txt", "bye\n");

        let first = fx.take().await;
        let again = fx.take().await;
        assert!(!again.is_new);
        assert_eq!(again.commit_id, first.commit_id);

        fx.write("keep.txt", "v2\n");
        fx.write("added.txt", "hi\n");
        std::fs::remove_file(fx.ws.join("drop.txt")).expect("rm");
        let second = fx.take().await;

        assert!(second.is_new);
        assert_ne!(second.commit_id, first.commit_id);
        assert_eq!(fx.paths(&second.commit_id), ["added.txt", "keep.txt"]);
        assert_eq!(fx.blob(&second.commit_id, "keep.txt"), b"v2\n");
        // The earlier checkpoint is still there, unchanged.
        assert_eq!(fx.blob(&first.commit_id, "keep.txt"), b"v1\n");
        assert_eq!(fx.paths(&first.commit_id), ["drop.txt", "keep.txt"]);
    }

    /// Snapshotting a repository must leave that repository exactly as it
    /// was: no objects, no refs, no index change, nothing in its work tree.
    #[tokio::test]
    async fn the_workspaces_own_repository_is_not_touched() {
        let fx = Fixture::new();
        test_support::git(&fx.ws, &["init", "-q"]);
        test_support::git(&fx.ws, &["config", "user.email", "t@borg.test"]);
        test_support::git(&fx.ws, &["config", "user.name", "t"]);
        fx.write("tracked.txt", "v1\n");
        test_support::git(&fx.ws, &["add", "."]);
        test_support::git(&fx.ws, &["commit", "-qm", "init"]);
        fx.write("tracked.txt", "dirty\n");
        fx.write("untracked.txt", "new\n");
        let before = files_under(&fx.ws);
        let index_before = std::fs::read(fx.ws.join(".git/index")).expect("index");

        let checkpoint = fx.take().await;

        assert_eq!(files_under(&fx.ws), before, "files appeared or vanished");
        assert_eq!(
            std::fs::read(fx.ws.join(".git/index")).expect("index"),
            index_before
        );
        assert_eq!(
            fx.paths(&checkpoint.commit_id),
            ["tracked.txt", "untracked.txt"],
            "the checkpoint holds the working files, never `.git`"
        );
        assert_eq!(fx.blob(&checkpoint.commit_id, "tracked.txt"), b"dirty\n");
    }

    /// A workspace can name a content filter in `.gitattributes` and define
    /// it in its own `.git/config`. The store never runs it and stores the
    /// file as it is on disk.
    #[tokio::test]
    async fn a_workspace_content_filter_never_runs() {
        let fx = Fixture::new();
        test_support::git(&fx.ws, &["init", "-q"]);
        test_support::git(&fx.ws, &["config", "user.email", "t@borg.test"]);
        test_support::git(&fx.ws, &["config", "user.name", "t"]);
        fx.write("seed.txt", "seed\n");
        test_support::git(&fx.ws, &["add", "."]);
        test_support::git(&fx.ws, &["commit", "-qm", "init"]);
        let sentinel_dir = fx.ws.parent().expect("parent").join("sentinel");
        std::fs::create_dir(&sentinel_dir).expect("mkdir");
        let sentinel = test_support::arm_evil_content_filter(&fx.ws, &sentinel_dir);
        fx.write("secret.txt", "plain\n");

        let checkpoint = fx.take().await;

        assert!(!sentinel.exists(), "the workspace's filter ran");
        assert_eq!(fx.blob(&checkpoint.commit_id, "secret.txt"), b"plain\n");
    }

    /// Ignored files and dependency directories stay out; so does the
    /// built-in exclude list in a directory with no `.gitignore` at all.
    #[tokio::test]
    async fn ignored_files_and_dependency_directories_are_left_out() {
        let fx = Fixture::new();
        fx.write(".gitignore", "*.log\nbuild/\n");
        fx.write("app.py", "print()\n");
        fx.write("debug.log", "noise\n");
        fx.write("build/out.o", "obj\n");
        fx.write("node_modules/dep/index.js", "x\n");
        fx.write(".venv/bin/python", "x\n");

        let checkpoint = fx.take().await;

        assert_eq!(fx.paths(&checkpoint.commit_id), [".gitignore", "app.py"]);
        assert_eq!(checkpoint.uncovered, Vec::<PathBuf>::new());
    }

    /// A nested repository — with commits or freshly `git init`ed — is not
    /// captured and does not break the snapshot. It is reported as uncovered
    /// so a restore knows to leave it alone.
    #[tokio::test]
    async fn nested_repositories_are_reported_and_skipped() {
        let fx = Fixture::new();
        fx.write("top.txt", "x\n");
        for nested in ["vendor/lib", "scratch"] {
            std::fs::create_dir_all(fx.ws.join(nested)).expect("mkdir");
            test_support::git(&fx.ws.join(nested), &["init", "-q"]);
        }
        fx.write("vendor/lib/code.c", "int x;\n");
        test_support::git(&fx.ws.join("vendor/lib"), &["add", "."]);
        test_support::git(
            &fx.ws.join("vendor/lib"),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@borg.test",
                "commit",
                "-qm",
                "init",
            ],
        );

        let checkpoint = fx.take().await;

        let mut uncovered = checkpoint.uncovered.clone();
        uncovered.sort();
        assert_eq!(
            uncovered,
            [PathBuf::from("scratch"), PathBuf::from("vendor/lib")]
        );
        assert_eq!(
            fx.tree(&checkpoint.commit_id),
            [("top.txt".to_string(), "100644".to_string())],
            "no gitlink and no nested file may enter the checkpoint"
        );
        // Still reported on the next snapshot, which has nothing new to add.
        let again = fx.take().await;
        assert!(!again.is_new);
        assert_eq!(again.uncovered.len(), 2);
    }

    /// A file over the size cap is uncovered, not captured. One that grows
    /// past the cap after it was captured leaves the next checkpoint rather
    /// than lingering there with its old content; one that shrinks back is
    /// captured again.
    #[tokio::test]
    async fn oversize_files_are_uncovered_never_stale() {
        let mut fx = Fixture::new();
        fx.store.max_file_bytes = 16;
        fx.write("small.txt", "tiny\n");
        fx.write("model.bin", vec![b'x'; 64]);

        let first = fx.take().await;
        assert_eq!(first.uncovered, [PathBuf::from("model.bin")]);
        assert_eq!(fx.paths(&first.commit_id), ["small.txt"]);

        fx.write("small.txt", vec![b'y'; 64]);
        let second = fx.take().await;
        let mut uncovered = second.uncovered.clone();
        uncovered.sort();
        assert_eq!(
            uncovered,
            [PathBuf::from("model.bin"), PathBuf::from("small.txt")]
        );
        assert_eq!(fx.paths(&second.commit_id), Vec::<String>::new());

        fx.write("small.txt", "tiny again\n");
        let third = fx.take().await;
        assert_eq!(third.uncovered, [PathBuf::from("model.bin")]);
        assert_eq!(fx.blob(&third.commit_id, "small.txt"), b"tiny again\n");
    }

    /// A path with pathspec magic characters in its name is excluded as that
    /// literal path, not interpreted.
    #[tokio::test]
    async fn an_uncovered_path_is_matched_literally() {
        let mut fx = Fixture::new();
        fx.store.max_file_bytes = 16;
        fx.write("*.txt", vec![b'x'; 64]);
        fx.write("real.txt", "keep\n");
        fx.write(":(top)odd", vec![b'x'; 64]);

        let checkpoint = fx.take().await;

        assert_eq!(fx.paths(&checkpoint.commit_id), ["real.txt"]);
        assert_eq!(checkpoint.uncovered.len(), 2);
    }

    /// A tree with more changed files than the store takes is skipped whole,
    /// and no checkpoint of it exists afterwards.
    #[tokio::test]
    async fn a_tree_with_too_many_changed_files_is_skipped() {
        let mut fx = Fixture::new();
        fx.store.max_changed_files = 3;
        for n in 0..5 {
            fx.write(&format!("f{n}.txt"), "x\n");
        }

        let outcome = fx.store.snapshot(&fx.ws).await.expect("snapshot");

        assert_eq!(
            outcome,
            SnapshotOutcome::Skipped(SkipReason::TooManyFiles { files: 5 })
        );
        let refs = fx.store_git(&["for-each-ref", "refs/borg"]);
        assert!(refs.stdout.is_empty(), "a skipped tree left a checkpoint");
    }

    /// Two workspaces share one store without seeing each other's files.
    #[tokio::test]
    async fn each_workspace_has_its_own_checkpoints() {
        let fx = Fixture::new();
        let other = fx.ws.parent().expect("parent").join("other");
        std::fs::create_dir(&other).expect("mkdir");
        fx.write("mine.txt", "a\n");
        std::fs::write(other.join("theirs.txt"), "b\n").expect("write");

        let mine = fx.take().await;
        let SnapshotOutcome::Taken(theirs) = fx.store.snapshot(&other).await.expect("snapshot")
        else {
            panic!("skipped");
        };
        let mine_again = fx.take().await;

        assert_eq!(fx.paths(&mine.commit_id), ["mine.txt"]);
        assert_eq!(fx.paths(&theirs.commit_id), ["theirs.txt"]);
        assert!(
            !mine_again.is_new,
            "another workspace's snapshot must not disturb this one's tip"
        );
    }

    /// A git killed mid-snapshot leaves a lock behind: on the index while it
    /// stages, on the workspace's tip ref while it records. The next
    /// snapshot of that workspace must still work after either.
    #[tokio::test]
    async fn a_stale_lock_does_not_block_the_next_snapshot() {
        // Where each lock sits, under the store.
        for held_in in ["indexes", "refs/borg/tip"] {
            let fx = Fixture::new();
            fx.write("a.txt", "v1\n");
            let first = fx.take().await;
            let lock = fx
                .store
                .dir()
                .join(held_in)
                .join(format!("{}.lock", workspace_key(&fx.ws)));
            let what = held_in;
            assert!(
                lock.with_extension("").exists(),
                "{what}: nothing of this workspace is kept beside {}",
                lock.display()
            );
            std::fs::write(&lock, b"").expect("plant lock");

            fx.write("a.txt", "v2\n");
            let second = fx.take().await;

            assert_ne!(second.commit_id, first.commit_id, "{what}");
            assert_eq!(fx.blob(&second.commit_id, "a.txt"), b"v2\n", "{what}");
            assert!(!lock.exists(), "{what}");
        }
    }

    /// The index remembers each file by the id of an object in the store,
    /// and git does not read a file again while its size and times match.
    /// When that object is gone (staged once, never part of a checkpoint,
    /// repacked away) every snapshot would fail on it. The store starts the
    /// index again from the files instead.
    #[tokio::test]
    async fn a_snapshot_recovers_when_the_index_names_an_object_that_is_gone() {
        let fx = Fixture::new();
        fx.write("a.txt", "v1\n");
        fx.write("b.txt", "kept as it is\n");
        // Long settled: a file written in the same second as the index is
        // one git reads again anyway, which would hide the fault.
        let settled = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(fx.ws.join("b.txt"))
            .and_then(|file| file.set_modified(settled))
            .expect("set mtime");
        let first = fx.take().await;
        let lost = fx.store_git(&["rev-parse", &format!("{}:b.txt", first.commit_id)]);
        let lost = String::from_utf8_lossy(&lost.stdout).trim().to_string();
        let (fan, rest) = lost.split_at(2);
        std::fs::remove_file(fx.store.dir().join("objects").join(fan).join(rest))
            .expect("the object is loose");

        fx.write("a.txt", "v2\n");
        let second = fx.take().await;

        assert_eq!(fx.blob(&second.commit_id, "a.txt"), b"v2\n");
        assert_eq!(fx.blob(&second.commit_id, "b.txt"), b"kept as it is\n");
    }

    /// A prune keeps a set number of checkpoints per workspace, and the tip
    /// can be one it dropped. An unchanged folder is then given the tip's
    /// commit again — which has to be a checkpoint again, or the undo point
    /// just handed out could not be restored.
    #[tokio::test]
    async fn an_unchanged_folder_gets_a_checkpoint_that_can_be_restored() {
        let fx = Fixture::new();
        fx.write("a.txt", "v1\n");
        let first = fx.take().await;
        let snap = format!(
            "refs/borg/snap/{}/{}",
            workspace_key(&fx.ws),
            first.commit_id
        );
        assert!(fx.store_git(&["update-ref", "-d", &snap]).status.success());

        let again = fx.take().await;

        assert_eq!(again.commit_id, first.commit_id);
        assert!(!again.is_new);
        let outcome = fx
            .store
            .restore(&fx.ws, &again.commit_id, &again.uncovered)
            .await
            .expect("restore");
        assert!(
            matches!(outcome, RestoreOutcome::Restored(_)),
            "{outcome:?}"
        );
    }

    /// A store whose creation was cut short (`HEAD` written, nothing else)
    /// is finished on the next use instead of failing every snapshot.
    #[tokio::test]
    async fn a_half_made_store_is_completed() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.store.dir()).expect("mkdir");
        std::fs::write(fx.store.dir().join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        fx.write("a.txt", "v1\n");

        let checkpoint = fx.take().await;

        assert_eq!(fx.blob(&checkpoint.commit_id, "a.txt"), b"v1\n");
    }

    /// The workspace is measured with its links resolved, so the home
    /// directory and the store must be too. A home reached through a link is
    /// still the home; a data dir reached through a link still holds the
    /// store.
    #[cfg(unix)]
    #[test]
    fn a_home_or_store_behind_a_link_is_still_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let real_home = base.join("data/home/someone");
        std::fs::create_dir_all(real_home.join("code/project")).expect("mkdir");
        std::fs::create_dir_all(real_home.join(".borg")).expect("mkdir");
        std::os::unix::fs::symlink(base.join("data/home"), base.join("home")).expect("symlink");
        let home = base.join("home/someone");
        // The store does not exist yet: only its data dir does.
        let store = CheckpointStore::new(&home.join(".borg"));

        let taken = |root: &Path| {
            store
                .project_root_with_home(root, Some(&home))
                .expect("resolve")
        };

        for refused in [
            home.clone(),
            real_home.clone(),
            base.join("data"),
            home.join(".borg"),
            real_home.join(".borg"),
        ] {
            assert_eq!(taken(&refused), None, "{}", refused.display());
        }
        assert_eq!(
            taken(&home.join("code/project")),
            Some(real_home.join("code/project"))
        );
    }

    /// The directories that are places, not projects.
    #[test]
    fn roots_that_are_not_projects_are_too_broad() {
        let home = Path::new("/Users/someone");
        let store = Path::new("/Users/someone/.borg/checkpoints/store");
        let cases: &[(&str, bool)] = &[
            ("/", true),
            ("/Users", true),
            ("/Users/someone", true),
            ("/Users/someone/.borg", true),
            ("/Users/someone/.borg/checkpoints", true),
            ("/Users/someone/code/project", false),
            ("/Users/someone-else/project", false),
            ("/srv/app", false),
        ];
        for (root, expected) in cases {
            assert_eq!(
                is_too_broad(Path::new(root), Some(home), store),
                *expected,
                "{root}"
            );
        }
        // No resolvable home: only the filesystem root and the store's own
        // ancestry are refused.
        assert!(is_too_broad(Path::new("/"), None, store));
        assert!(!is_too_broad(Path::new("/srv/app"), None, store));
    }

    /// The home directory itself is refused end to end, before git runs.
    #[tokio::test]
    async fn the_home_directory_is_never_snapshotted() {
        let Some(home) = dirs::home_dir() else { return };
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = CheckpointStore::new(tmp.path());

        let outcome = store.snapshot(&home).await.expect("snapshot");

        assert_eq!(outcome, SnapshotOutcome::Skipped(SkipReason::TooBroad));
        assert!(!store.dir().exists(), "no store may be created for a skip");
    }
}
