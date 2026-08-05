//! Git utilities for coding agent capabilities.
//!
//! Provides ghost commits (full repo snapshots for atomic undo), git context
//! collection for system prompt enrichment, and turn diff tracking.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::instrument;

/// Timeout for git subprocess calls.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Disable a repository's filesystem monitor: `core.fsmonitor` can name a
/// program git spawns on index/status operations, so an untrusted tree must
/// not get to pick it while we snapshot.
const FSMONITOR_OVERRIDE: &str = "core.fsmonitor=false";
/// Route hooks (including `post-index-change`, which fires on `add`/`read-tree`)
/// to a null path so a repository can't execute its own hook scripts.
#[cfg(windows)]
const HOOKS_OVERRIDE: &str = "core.hooksPath=NUL";
#[cfg(not(windows))]
const HOOKS_OVERRIDE: &str = "core.hooksPath=/dev/null";

/// Matches the config keys for executable content filter drivers. A repository
/// that configures `filter.<name>.clean`/`.smudge`/`.process` can run arbitrary
/// commands during `git add` / worktree checkout; we discover and neutralize
/// them rather than letting them fire.
pub(crate) const EXECUTABLE_FILTER_CONFIG_PATTERN: &str = r"^filter\..*\.(clean|smudge|process)$";

/// Refuse to treat an *implicitly discovered* directory as a bare repository.
/// A tracked subdirectory can be shaped to look like one, and git's discovery
/// walk would then read that directory's `config` — handing an untrusted tree
/// the very config keys the overrides above exist to deny. Explicit
/// `--git-dir`/`GIT_DIR` use is unaffected.
const SAFE_BARE_REPOSITORY: &str = "safe.bareRepository=explicit";

/// Config `-c` overrides prepended to every git invocation so a repository's
/// own configuration can't execute code while we operate inside its tree.
pub(crate) fn hardened_prelude() -> [&'static str; 6] {
    [
        "-c",
        FSMONITOR_OVERRIDE,
        "-c",
        HOOKS_OVERRIDE,
        "-c",
        SAFE_BARE_REPOSITORY,
    ]
}

/// Make a `git` invocation non-interactive.
///
/// Every git command we run is local plumbing, so nothing should ever prompt.
/// Two independent channels have to be closed:
///
/// * **stdin** — `.output()` nulls it implicitly, but stating it keeps a future
///   `.status()`/`.spawn()` refactor from silently handing git the operator's
///   input stream.
/// * **the controlling terminal** — git opens `/dev/tty` *directly* for
///   credential prompts, so nulling stdin alone does not stop it.
///   `GIT_TERMINAL_PROMPT=0` turns a would-be prompt into an immediate error.
fn make_git_non_interactive(cmd: &mut tokio::process::Command) {
    cmd.stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
}

/// [`make_git_non_interactive`] for the blocking `std::process` git runners in
/// the patch applicator. Those have no timeout at all, so a credential prompt
/// there hangs the tool call outright rather than merely delaying it.
pub(crate) fn make_git_non_interactive_sync(cmd: &mut std::process::Command) {
    cmd.stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
}

/// Run `cmd` under [`GIT_COMMAND_TIMEOUT`], killing the whole process tree if
/// the budget expires.
///
/// The obvious spelling — `timeout(.., cmd.output())` — leaks: dropping that
/// future abandons the child with no signal at all, so a git that hangs on a
/// network remote or a credential helper survives for the life of the machine.
/// Spawning explicitly gives us the pid, so `kill_on_drop` can end git itself
/// and `killpg` can end anything it spawned.
async fn run_with_timeout(
    mut cmd: tokio::process::Command,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    crate::util::process_tree::spawn_in_own_group(&mut cmd);
    // `Command::output` pipes these implicitly; `spawn` inherits instead, so
    // they must be stated or `wait_with_output` returns empty streams.
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.kill_on_drop(true).spawn() {
        Ok(child) => child,
        Err(e) => return Ok(Err(e)),
    };
    // Read the pid before `wait_with_output` consumes the handle.
    let pid = child.id();
    match tokio::time::timeout(GIT_COMMAND_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => Ok(result),
        Err(elapsed) => {
            crate::util::process_tree::kill_process_group(pid);
            Err(elapsed)
        }
    }
}

/// Parse `git config --null --name-only --get-regexp` output (NUL-separated
/// keys) into the set of filter driver prefixes, e.g. `filter.lfs.clean` →
/// `filter.lfs`.
pub(crate) fn parse_filter_drivers(stdout: &str) -> Vec<String> {
    let mut drivers: Vec<String> = stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .filter_map(|key| {
            key.strip_suffix(".clean")
                .or_else(|| key.strip_suffix(".smudge"))
                .or_else(|| key.strip_suffix(".process"))
        })
        .map(str::to_string)
        .collect();
    drivers.sort();
    drivers.dedup();
    drivers
}

/// For each discovered filter driver, build config overrides that blank its
/// executable entries and drop the `required` flag (so blanking can't turn a
/// required filter into a hard error).
pub(crate) fn build_filter_override_env(drivers: &[String]) -> Vec<(String, String)> {
    drivers
        .iter()
        .flat_map(|d| {
            [
                (format!("{d}.clean"), String::new()),
                (format!("{d}.smudge"), String::new()),
                (format!("{d}.process"), String::new()),
                (format!("{d}.required"), "false".to_string()),
            ]
        })
        .collect()
}

/// Encode config key/value overrides as the `GIT_CONFIG_COUNT` /
/// `GIT_CONFIG_KEY_<i>` / `GIT_CONFIG_VALUE_<i>` environment form, which git
/// applies with higher precedence than repository config.
pub(crate) fn git_config_env(overrides: &[(String, String)]) -> Vec<(String, String)> {
    if overrides.is_empty() {
        return Vec::new();
    }
    let mut env = Vec::with_capacity(overrides.len() * 2 + 1);
    env.push(("GIT_CONFIG_COUNT".to_string(), overrides.len().to_string()));
    for (i, (k, v)) in overrides.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), k.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), v.clone()));
    }
    env
}

/// Discover the repository's configured executable filter drivers and return
/// the `GIT_CONFIG_*` env entries that neutralize them. Best-effort: discovery
/// failures log and yield no overrides (the hardening prelude still applies).
async fn filter_neutralizing_env(cwd: &Path) -> Vec<(String, String)> {
    let mut discovery = tokio::process::Command::new("git");
    discovery.args(filter_discovery_argv()).current_dir(cwd);
    make_git_non_interactive(&mut discovery);
    let result = run_with_timeout(discovery).await;
    let stdout = match result {
        // `--get-regexp` exits 1 when nothing matches — that's the common case.
        Ok(Ok(o)) if o.status.success() || o.status.code() == Some(1) => o.stdout,
        Ok(Ok(o)) => {
            tracing::warn!(
                "git filter-driver discovery failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        Ok(Err(e)) => {
            tracing::warn!("git filter-driver discovery failed to spawn: {e}");
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!("git filter-driver discovery timed out");
            return Vec::new();
        }
    };
    neutralizing_env_from_stdout(&String::from_utf8_lossy(&stdout))
}

/// Argv (hardening prelude included) that lists a repository's configured
/// executable filter drivers. Shared by the async and sync neutralization
/// paths so the discovery command lives in exactly one place.
pub(crate) fn filter_discovery_argv() -> Vec<&'static str> {
    let mut argv: Vec<&str> = hardened_prelude().to_vec();
    argv.extend([
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        EXECUTABLE_FILTER_CONFIG_PATTERN,
    ]);
    argv
}

/// Map `git config --get-regexp` discovery stdout to the `GIT_CONFIG_*` env
/// entries that neutralize every discovered filter driver. Shared by the async
/// and sync paths so the parse/encode chain isn't duplicated.
pub(crate) fn neutralizing_env_from_stdout(stdout: &str) -> Vec<(String, String)> {
    git_config_env(&build_filter_override_env(&parse_filter_drivers(stdout)))
}

/// Directories excluded from ghost snapshots (build artifacts, deps, caches).
const IGNORED_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "dist",
    "build",
    ".pytest_cache",
    ".mypy_cache",
    ".cache",
    ".tox",
    "__pycache__",
    "target",
    ".next",
    ".nuxt",
];

/// Max untracked file size to include in ghost snapshot (10 MiB). Larger files
/// are skipped to keep the temp-index snapshot fast and bounded — build
/// artifacts and media should not bloat undo points.
const MAX_UNTRACKED_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A ghost commit that snapshots the entire working tree state.
///
/// Created using a temporary index file and git plumbing commands so that
/// HEAD and the user's index are never touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhostCommit {
    pub commit_id: String,
    pub parent_id: Option<String>,
    /// Untracked files that existed before the snapshot (preserved on restore).
    pub preexisting_untracked: Vec<PathBuf>,
}

/// Summary of git repo state for system prompt enrichment.
#[derive(Debug, Clone, Default)]
pub struct GitContext {
    pub repo_root: Option<PathBuf>,
    /// The main repository root when `repo_root` is a linked `git worktree`
    /// checkout, resolved via the `.git` gitfile pointer. Equal to `repo_root`
    /// for an ordinary checkout. `None` when outside any repository *or* when
    /// `repo_root` is `Some` but its `.git` gitfile is not a worktree pointer
    /// (e.g. a submodule) — so `main_repo_root.is_none()` does not imply
    /// "not in a repository".
    pub main_repo_root: Option<PathBuf>,
    pub branch: Option<String>,
    pub commit_hash: Option<String>,
    pub recent_commits: Vec<String>,
    pub has_uncommitted_changes: bool,
    pub remote_url: Option<String>,
}

/// Find the git repo root by walking up from `cwd`.
pub fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve the *main* repository root for `cwd`, following a linked
/// `git worktree` checkout back to the repository it belongs to.
///
/// [`find_repo_root`] stops at the first ancestor containing a `.git` entry.
/// For a worktree created with `git worktree add`, that `.git` is a *file*
/// holding `gitdir: <main>/.git/worktrees/<name>`, so `find_repo_root` returns
/// the worktree checkout rather than the repository it links to. This function
/// parses that pointer and returns `<main>` instead. For an ordinary checkout
/// (`.git` is a directory) it returns the repo root unchanged, and returns
/// `None` when `cwd` is not inside any git repository or the `.git` file points
/// somewhere that is not a worktree git dir (e.g. a submodule under
/// `.git/modules/...`).
///
/// Pure filesystem inspection — never invokes the `git` binary, so it works
/// even when git is unavailable or the tree is untrusted.
pub fn resolve_main_repo_root(cwd: &Path) -> Option<PathBuf> {
    let repo_root = find_repo_root(cwd)?;
    let dot_git = repo_root.join(".git");

    // Ordinary checkout: `.git` is a directory, so this *is* the main root.
    if dot_git.is_dir() {
        return Some(repo_root);
    }

    // Linked worktree: `.git` is a file pointing at the worktree's git dir,
    // which lives at `<main>/.git/worktrees/<name>`.
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir_rel = contents.trim().strip_prefix("gitdir:")?.trim();
    if git_dir_rel.is_empty() {
        return None;
    }

    // The pointer is normally absolute; a relative pointer is resolved against
    // the directory that holds the `.git` file, matching git's own semantics.
    let git_dir_path = resolve_gitdir_against_base(git_dir_rel, &repo_root);

    // Expect the tail `.../worktrees/<name>`. Anything else is not a linked
    // worktree and gets no rewrite.
    let worktrees_dir = git_dir_path.parent()?; // <main>/.git/worktrees
    if worktrees_dir.file_name() != Some(OsStr::new("worktrees")) {
        return None;
    }
    let common_dir = worktrees_dir.parent()?; // <main>/.git
    if common_dir.file_name() != Some(OsStr::new(".git")) {
        return None;
    }
    common_dir.parent().map(Path::to_path_buf) // <main>
}

/// Resolve a `gitdir:` pointer against the directory holding the `.git` file.
/// Absolute pointers are taken as-is; relative pointers are joined onto `base`.
/// The result is lexically normalized so `.parent()`/`.file_name()` see the
/// clean `.../worktrees/<name>` tail even when the pointer contains `..`.
fn resolve_gitdir_against_base(pointer: &str, base: &Path) -> PathBuf {
    let p = Path::new(pointer);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    normalize_lexically(&joined)
}

/// Collapse `.` and `..` components without touching the filesystem (no symlink
/// resolution, unlike [`std::fs::canonicalize`]). Sufficient for reasoning about
/// the git-managed `.git/worktrees/<name>` path suffix.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Run a git command with timeout, returns stdout on success.
async fn run_git(args: &[&str], cwd: &Path) -> Result<String> {
    run_git_inner(args, cwd, &[]).await
}

/// Shared git runner. Always prepends [`hardened_prelude`] so a repository's
/// own `core.fsmonitor`/hooks can't execute while we work in its tree, and
/// applies any `extra_env` (e.g. `GIT_INDEX_FILE`, filter-neutralizing
/// `GIT_CONFIG_*` entries).
#[instrument(name = "run_git", skip_all, fields(git.args = ?args))]
async fn run_git_inner(
    args: &[&str],
    cwd: &Path,
    extra_env: &[(String, String)],
) -> Result<String> {
    let mut argv: Vec<&str> = hardened_prelude().to_vec();
    argv.extend_from_slice(args);
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&argv).current_dir(cwd);
    make_git_non_interactive(&mut cmd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = run_with_timeout(cmd)
        .await
        .context("git command timed out")?
        .context("failed to execute git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Collect rich git context for system prompt injection.
#[instrument(skip_all)]
pub async fn collect_git_context(cwd: &Path) -> GitContext {
    let repo_root = find_repo_root(cwd);
    let main_repo_root = resolve_main_repo_root(cwd);
    let work_dir = repo_root.as_deref().unwrap_or(cwd);

    // Run all git commands in parallel — they're independent read-only operations.
    let (branch, commit_hash, recent_commits_raw, status, remote_url) = tokio::join!(
        run_git(&["branch", "--show-current"], work_dir),
        run_git(&["rev-parse", "--short", "HEAD"], work_dir),
        run_git(&["log", "--oneline", "-5", "--no-decorate"], work_dir),
        run_git(&["status", "--porcelain"], work_dir),
        run_git(&["remote", "get-url", "origin"], work_dir),
    );

    GitContext {
        repo_root,
        main_repo_root,
        branch: branch.ok().filter(|s| !s.is_empty()),
        commit_hash: commit_hash.ok(),
        recent_commits: recent_commits_raw
            .ok()
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default(),
        has_uncommitted_changes: status.map(|s| !s.is_empty()).unwrap_or(false),
        remote_url: remote_url.ok(),
    }
}

/// Create a ghost commit capturing tracked + untracked files using a temp index.
///
/// This does NOT modify HEAD or the user's index. Uses git plumbing:
/// 1. Create a temp index file
/// 2. `GIT_INDEX_FILE=<tmp> git read-tree HEAD` (copy tracked files)
/// 3. `GIT_INDEX_FILE=<tmp> git add --all` (add untracked, excluding ignored dirs)
/// 4. `GIT_INDEX_FILE=<tmp> git write-tree` (create tree object)
/// 5. `git commit-tree <tree> -p HEAD -m "borg ghost snapshot"` (create commit)
#[instrument(skip_all)]
pub async fn create_ghost_commit(repo_root: &Path) -> Result<GhostCommit> {
    let has_head = run_git(&["rev-parse", "HEAD"], repo_root).await.is_ok();

    let tmp_index = tempfile::NamedTempFile::new().context("create temp index")?;
    let tmp_path = tmp_index.path().to_string_lossy().to_string();

    // Collect untracked files before snapshot for restore filtering
    let untracked_output = run_git(&["ls-files", "--others", "--exclude-standard"], repo_root)
        .await
        .unwrap_or_default();

    let preexisting_untracked: Vec<PathBuf> = untracked_output
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| {
            // Exclude ignored directories
            !IGNORED_DIR_NAMES
                .iter()
                .any(|ignored| line.starts_with(ignored) || line.contains(&format!("/{ignored}/")))
        })
        .filter(|line| {
            // Exclude oversized files
            let full = repo_root.join(line);
            full.metadata()
                .map(|m| m.len() <= MAX_UNTRACKED_FILE_BYTES)
                .unwrap_or(false)
        })
        .map(PathBuf::from)
        .collect();

    // `add`/`read-tree` would otherwise run repo-configured content filters
    // (`filter.<name>.clean`/`.process`) against an untrusted tree.
    let filter_env = filter_neutralizing_env(repo_root).await;

    if has_head {
        run_git_with_index(&tmp_path, &["read-tree", "HEAD"], repo_root, &filter_env).await?;
    }

    run_git_with_index(&tmp_path, &["add", "--all"], repo_root, &filter_env).await?;

    let tree_id = run_git_with_index(&tmp_path, &["write-tree"], repo_root, &filter_env).await?;

    let parent_id = if has_head {
        Some(run_git(&["rev-parse", "HEAD"], repo_root).await?)
    } else {
        None
    };

    let mut commit_args = vec!["commit-tree", &tree_id, "-m", "borg ghost snapshot"];
    if let Some(ref parent) = parent_id {
        commit_args.push("-p");
        commit_args.push(parent);
    }

    let commit_id = run_git(&commit_args, repo_root).await?;

    Ok(GhostCommit {
        commit_id,
        parent_id,
        preexisting_untracked,
    })
}

/// Restore working tree to match a ghost commit.
#[instrument(skip_all)]
pub async fn restore_ghost_commit(repo_root: &Path, commit: &GhostCommit) -> Result<()> {
    // Restore all tracked files from the ghost commit. Worktree checkout runs
    // `filter.<name>.smudge`/`.process` drivers, so neutralize them too.
    let filter_env = filter_neutralizing_env(repo_root).await;
    run_git_inner(
        &[
            "restore",
            "--source",
            &commit.commit_id,
            "--worktree",
            "--",
            ".",
        ],
        repo_root,
        &filter_env,
    )
    .await?;

    // Remove files that were created after the snapshot (not in preexisting_untracked)
    let current_untracked = run_git(&["ls-files", "--others", "--exclude-standard"], repo_root)
        .await
        .unwrap_or_default();

    for line in current_untracked.lines() {
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if !commit.preexisting_untracked.contains(&path) {
            let full = repo_root.join(line);
            if full.exists() {
                tracing::info!(
                    "Ghost restore: removing post-snapshot file: {}",
                    full.display()
                );
                if let Err(e) = std::fs::remove_file(&full) {
                    tracing::warn!("Failed to remove {}: {e}", full.display());
                }
            }
        }
    }

    Ok(())
}

/// Format git context as a string block for system prompt injection.
pub fn format_git_context(ctx: &GitContext) -> String {
    let mut out = String::new();
    // Surface when the working tree is a linked worktree so the agent knows the
    // canonical project it belongs to (its `.git` lives in the main repo).
    if let (Some(main), Some(repo)) = (&ctx.main_repo_root, &ctx.repo_root) {
        if main != repo {
            out.push_str(&format!("Linked worktree of: {}\n", main.display()));
        }
    }
    if let Some(ref branch) = ctx.branch {
        out.push_str(&format!("Git branch: {branch}\n"));
    }
    if let Some(ref hash) = ctx.commit_hash {
        out.push_str(&format!("Git commit: {hash}\n"));
    }
    if ctx.has_uncommitted_changes {
        out.push_str("Uncommitted changes: yes\n");
    }
    if !ctx.recent_commits.is_empty() {
        out.push_str("Recent commits:\n");
        for c in &ctx.recent_commits {
            out.push_str(&format!("  {c}\n"));
        }
    }
    out
}

/// Run a git command against a custom `GIT_INDEX_FILE`, optionally with
/// filter-neutralizing config so an untrusted tree's content filters can't run
/// during index mutation.
async fn run_git_with_index(
    index_path: &str,
    args: &[&str],
    cwd: &Path,
    filter_env: &[(String, String)],
) -> Result<String> {
    let mut env = vec![("GIT_INDEX_FILE".to_string(), index_path.to_string())];
    env.extend_from_slice(filter_env);
    run_git_inner(args, cwd, &env).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_repo_root_in_git_dir() {
        // This test runs inside the borg repo, so CWD should have a .git
        let cwd = std::env::current_dir().unwrap();
        let root = find_repo_root(&cwd);
        assert!(root.is_some());
        assert!(root.unwrap().join(".git").exists());
    }

    #[test]
    fn find_repo_root_from_subdir() {
        // Walking up from a nested src/ should resolve to the same repo root
        // as walking up from cwd — the function climbs until it finds .git.
        let cwd = std::env::current_dir().unwrap();
        let root_from_cwd = find_repo_root(&cwd).expect("cwd is inside the repo");
        let sub = cwd.join("crates").join("cortex").join("src");
        let root_from_sub = find_repo_root(&sub).expect("subdir resolves to same root");
        assert_eq!(root_from_cwd, root_from_sub);
    }

    #[test]
    fn format_git_context_empty() {
        let ctx = GitContext::default();
        let out = format_git_context(&ctx);
        assert!(out.is_empty());
    }

    #[test]
    fn format_git_context_full() {
        let ctx = GitContext {
            repo_root: Some(PathBuf::from("/repo")),
            main_repo_root: Some(PathBuf::from("/repo")),
            branch: Some("main".into()),
            commit_hash: Some("abc1234".into()),
            recent_commits: vec!["abc1234 Fix bug".into(), "def5678 Add feature".into()],
            has_uncommitted_changes: true,
            remote_url: Some("https://github.com/user/repo".into()),
        };
        let out = format_git_context(&ctx);
        assert!(out.contains("Git branch: main"));
        assert!(out.contains("Git commit: abc1234"));
        assert!(out.contains("Uncommitted changes: yes"));
        assert!(out.contains("abc1234 Fix bug"));
    }

    #[test]
    fn format_git_context_shows_linked_worktree_when_roots_differ() {
        let ctx = GitContext {
            repo_root: Some(PathBuf::from("/tmp/wt")),
            main_repo_root: Some(PathBuf::from("/tmp/repo")),
            branch: Some("feature/x".into()),
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(out.contains("Linked worktree of: /tmp/repo"));
        assert!(out.contains("Git branch: feature/x"));
    }

    #[test]
    fn format_git_context_omits_worktree_line_for_ordinary_repo() {
        // Ordinary checkout: main root equals repo root → no worktree line.
        let ctx = GitContext {
            repo_root: Some(PathBuf::from("/tmp/repo")),
            main_repo_root: Some(PathBuf::from("/tmp/repo")),
            branch: Some("main".into()),
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(!out.contains("Linked worktree of"));
    }

    #[test]
    fn format_git_context_branch_only() {
        let ctx = GitContext {
            branch: Some("feature/foo".into()),
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(out.contains("Git branch: feature/foo"));
        assert!(!out.contains("Uncommitted changes"));
        assert!(!out.contains("Recent commits"));
    }

    #[test]
    fn format_git_context_uncommitted_only() {
        let ctx = GitContext {
            has_uncommitted_changes: true,
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(out.contains("Uncommitted changes: yes"));
        assert!(!out.contains("Git branch"));
    }

    #[test]
    fn format_git_context_no_uncommitted_changes() {
        let ctx = GitContext {
            branch: Some("main".into()),
            has_uncommitted_changes: false,
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(!out.contains("Uncommitted changes"));
    }

    #[test]
    fn format_git_context_recent_commits_listed() {
        let ctx = GitContext {
            recent_commits: vec!["abc Fix".into(), "def Add".into(), "ghi Refactor".into()],
            ..Default::default()
        };
        let out = format_git_context(&ctx);
        assert!(out.contains("Recent commits:"));
        assert!(out.contains("abc Fix"));
        assert!(out.contains("def Add"));
        assert!(out.contains("ghi Refactor"));
    }

    #[test]
    fn ghost_commit_serializable() {
        let gc = GhostCommit {
            commit_id: "abc123".to_string(),
            parent_id: Some("def456".to_string()),
            preexisting_untracked: vec![PathBuf::from("file.txt"), PathBuf::from("dir/other.rs")],
        };
        let json = serde_json::to_string(&gc).unwrap();
        let deserialized: GhostCommit = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.commit_id, "abc123");
        assert_eq!(deserialized.parent_id, Some("def456".to_string()));
        assert_eq!(deserialized.preexisting_untracked.len(), 2);
    }

    #[test]
    fn ghost_commit_no_parent() {
        let gc = GhostCommit {
            commit_id: "abc123".to_string(),
            parent_id: None,
            preexisting_untracked: vec![],
        };
        let json = serde_json::to_string(&gc).unwrap();
        let deserialized: GhostCommit = serde_json::from_str(&json).unwrap();
        assert!(deserialized.parent_id.is_none());
        assert!(deserialized.preexisting_untracked.is_empty());
    }

    #[test]
    fn find_repo_root_with_fake_git() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = find_repo_root(tmp.path());
        assert_eq!(root, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn find_repo_root_from_nested_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&sub).unwrap();
        let root = find_repo_root(&sub);
        assert_eq!(root, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn normalize_lexically_collapses_dot_and_dot_dot() {
        assert_eq!(
            normalize_lexically(Path::new("/tmp/wt/../repo/./.git")),
            PathBuf::from("/tmp/repo/.git")
        );
        assert_eq!(
            normalize_lexically(Path::new("/a/b/c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn resolve_gitdir_relative_pointer_resolved_against_base() {
        // A relative `gitdir:` pointer is joined onto the directory holding the
        // `.git` file and normalized, matching git's own semantics.
        let resolved =
            resolve_gitdir_against_base("../repo/.git/worktrees/wt", Path::new("/tmp/wt"));
        assert_eq!(resolved, PathBuf::from("/tmp/repo/.git/worktrees/wt"));
    }

    #[test]
    fn resolve_main_repo_root_outside_repo_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_main_repo_root(tmp.path()), None);
    }

    #[test]
    fn resolve_main_repo_root_regular_repo_returns_repo_root() {
        // `.git` is a directory → this is the main root, unchanged, and a nested
        // subdirectory resolves to the same root.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("sub/dir");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(resolve_main_repo_root(&repo), Some(repo.clone()));
        assert_eq!(resolve_main_repo_root(&nested), Some(repo));
    }

    #[test]
    fn resolve_main_repo_root_detects_worktree_pointer_without_git() {
        // A linked worktree: `.git` is a *file* pointing at
        // `<main>/.git/worktrees/<name>`. Resolution follows the pointer back to
        // `<main>` using only filesystem reads — no `git` binary involved.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let worktree_git_dir = repo_root.join(".git").join("worktrees").join("feature-x");
        let worktree_root = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::create_dir_all(worktree_root.join("nested")).unwrap();
        std::fs::write(
            worktree_root.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();

        assert_eq!(
            resolve_main_repo_root(&worktree_root),
            Some(repo_root.clone())
        );
        // A nested cwd inside the worktree still resolves to the main root.
        assert_eq!(
            resolve_main_repo_root(&worktree_root.join("nested")),
            Some(repo_root)
        );
    }

    #[test]
    fn resolve_main_repo_root_non_worktrees_gitdir_returns_none() {
        // `.git` is a file but the pointer does not live under `.../worktrees/`
        // (e.g. a submodule gitdir). No rewrite — this is not a linked worktree.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("nested")).unwrap();
        std::fs::write(
            proj.join(".git"),
            format!("gitdir: {}\n", tmp.path().join("some/other/loc").display()),
        )
        .unwrap();

        assert_eq!(resolve_main_repo_root(&proj), None);
        assert_eq!(resolve_main_repo_root(&proj.join("nested")), None);
    }

    #[test]
    fn resolve_main_repo_root_worktrees_dir_not_under_dot_git_returns_none() {
        // The pointer tail is `worktrees/<name>` but its grandparent is not a
        // `.git` dir. Real git always nests worktrees under `<main>/.git`, so
        // this is not a linked worktree and must not resolve to `/foo`.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(".git"),
            format!(
                "gitdir: {}\n",
                tmp.path().join("foo/worktrees/bar").display()
            ),
        )
        .unwrap();

        assert_eq!(resolve_main_repo_root(&proj), None);
    }

    #[test]
    fn resolve_main_repo_root_empty_gitdir_pointer_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(".git"), "gitdir:\n").unwrap();
        assert_eq!(resolve_main_repo_root(&proj), None);
    }

    #[tokio::test]
    async fn collect_git_context_in_repo() {
        let cwd = std::env::current_dir().unwrap();
        let ctx = collect_git_context(&cwd).await;
        // We're in a git repo, so should have some context
        assert!(ctx.repo_root.is_some());
        assert!(ctx.branch.is_some() || ctx.commit_hash.is_some());
    }

    #[tokio::test]
    async fn collect_git_context_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = collect_git_context(tmp.path()).await;
        // No git repo, so branch/commit should be None
        assert!(ctx.branch.is_none());
        assert!(ctx.commit_hash.is_none());
    }

    /// Records span names created during a test.
    struct SpanRecorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanRecorder {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(attrs.metadata().name().to_string());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collect_git_context_emits_tracing_span() {
        use tracing_subscriber::layer::SubscriberExt;
        let spans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(SpanRecorder(spans.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let cwd = std::env::current_dir().unwrap();
        let _ = collect_git_context(&cwd).await;

        let recorded = spans.lock().unwrap();
        // The outer `collect_git_context` span may not be captured when a
        // global subscriber is already registered (thread-local `set_default`
        // cannot override `set_global_default`).  The inner `run_git` spans
        // prove the instrumentation pipeline is working.
        assert!(
            recorded
                .iter()
                .any(|s| s == "collect_git_context" || s == "run_git"),
            "expected instrumented spans, got: {recorded:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_git_emits_tracing_span_with_args() {
        use tracing_subscriber::layer::SubscriberExt;
        let spans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(SpanRecorder(spans.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let cwd = std::env::current_dir().unwrap();
        let _ = run_git(&["status", "--short"], &cwd).await;

        let recorded = spans.lock().unwrap();
        assert!(
            recorded.iter().any(|s| s == "run_git"),
            "expected 'run_git' span, got: {recorded:?}"
        );
    }

    #[test]
    fn hardened_prelude_disables_fsmonitor_and_hooks() {
        // These exact flags are the security invariant: fsmonitor and hooks are
        // the two repo-config code-execution vectors on the plumbing we run.
        let prelude = hardened_prelude();
        assert_eq!(prelude[0], "-c");
        assert_eq!(prelude[1], "core.fsmonitor=false");
        assert_eq!(prelude[2], "-c");
        assert!(
            prelude[3].starts_with("core.hooksPath="),
            "hooks must be routed to a null path, got {:?}",
            prelude[3]
        );
        // Null target must not point back at a usable directory.
        assert!(prelude[3] == "core.hooksPath=/dev/null" || prelude[3] == "core.hooksPath=NUL");
        // A tracked subdirectory shaped like a bare repo would otherwise be
        // picked up by git's discovery walk, and its `config` read — which is
        // exactly the file the two overrides above exist to neutralize.
        assert_eq!(prelude[4], "-c");
        assert_eq!(prelude[5], "safe.bareRepository=explicit");
    }

    /// Every git invocation in the crate must carry the full prelude — the
    /// hardening is worthless on the one call site that forgot it. `-c` pairs
    /// are positional, so this also catches a prelude whose arity changed
    /// without its `-c` separators.
    #[test]
    fn discovery_argv_carries_the_whole_prelude() {
        let argv = filter_discovery_argv();
        let prelude = hardened_prelude();
        assert_eq!(
            &argv[..prelude.len()],
            &prelude[..],
            "filter discovery must run under the hardening prelude: {argv:?}"
        );
        assert_eq!(argv[prelude.len()], "config");
    }

    /// A command that outlives [`GIT_COMMAND_TIMEOUT`] must not leave orphans
    /// behind. The natural spelling — `timeout(.., cmd.output())` — abandons
    /// the child with no signal at all; here the descendant git would spawn
    /// (a credential helper, fsmonitor) stands in for that leak.
    ///
    /// Exercises `run_with_timeout` directly rather than `run_git`: the leak is
    /// in the timeout wrapper, and driving it needs a program that blocks
    /// deterministically without a real repo, network, or `PATH` mutation
    /// (which is process-global and would race sibling tests).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_with_timeout_kills_the_whole_process_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("child.pid");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!(
                "sleep 120 & echo $! > {}; sleep 120",
                pid_file.to_string_lossy()
            ))
            .stdout(std::process::Stdio::piped());

        let started = std::time::Instant::now();
        let result = run_with_timeout(cmd).await;
        assert!(
            result.is_err(),
            "a command that outlives the budget must report the timeout"
        );
        assert!(
            started.elapsed() >= GIT_COMMAND_TIMEOUT,
            "must not have returned before the budget elapsed"
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("shell should have recorded its background job's pid")
            .trim()
            .parse()
            .expect("pid");
        // SIGKILL delivery is asynchronous; poll rather than racing it.
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for existence; it never delivers.
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "descendant {pid} outlived the timed-out command");
    }

    #[test]
    fn parse_filter_drivers_extracts_unique_prefixes() {
        let stdout = "filter.lfs.clean\0filter.lfs.process\0filter.evil.smudge\0other.key\0";
        let drivers = parse_filter_drivers(stdout);
        assert_eq!(
            drivers,
            vec!["filter.evil".to_string(), "filter.lfs".to_string()]
        );
    }

    #[test]
    fn parse_filter_drivers_empty_input() {
        assert!(parse_filter_drivers("").is_empty());
        assert!(parse_filter_drivers("\0\0").is_empty());
    }

    #[test]
    fn build_filter_override_env_blanks_executable_entries() {
        let env = build_filter_override_env(&["filter.evil".to_string()]);
        // clean/smudge/process blanked, required forced off.
        assert!(env.contains(&("filter.evil.clean".to_string(), String::new())));
        assert!(env.contains(&("filter.evil.smudge".to_string(), String::new())));
        assert!(env.contains(&("filter.evil.process".to_string(), String::new())));
        assert!(env.contains(&("filter.evil.required".to_string(), "false".to_string())));
    }

    #[test]
    fn git_config_env_encodes_count_and_pairs() {
        let overrides = vec![
            ("filter.x.clean".to_string(), String::new()),
            ("filter.x.required".to_string(), "false".to_string()),
        ];
        let env = git_config_env(&overrides);
        assert_eq!(env[0], ("GIT_CONFIG_COUNT".to_string(), "2".to_string()));
        assert_eq!(
            env[1],
            ("GIT_CONFIG_KEY_0".to_string(), "filter.x.clean".to_string())
        );
        assert_eq!(env[2], ("GIT_CONFIG_VALUE_0".to_string(), String::new()));
        assert_eq!(
            env[3],
            (
                "GIT_CONFIG_KEY_1".to_string(),
                "filter.x.required".to_string()
            )
        );
        assert_eq!(
            env[4],
            ("GIT_CONFIG_VALUE_1".to_string(), "false".to_string())
        );
    }

    #[test]
    fn git_config_env_empty_is_empty() {
        assert!(git_config_env(&[]).is_empty());
    }

    #[cfg(unix)]
    fn init_repo_with_commit(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                // Isolate from the developer's global/system git config so the
                // fixture is deterministic.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("spawn git")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@borg.test"]);
        run(&["config", "user.name", "borg-test"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "init"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_ghost_commit_neutralizes_repo_clean_filter() {
        // A malicious repo configures a content filter that runs a command when
        // `git add` stages a matching file. The ghost-snapshot path must not let
        // that command execute.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);

        let sentinel = dir.join("PWNED");
        let clean_cmd = format!("sh -c 'touch \"{}\"; cat'", sentinel.display());
        let ok = std::process::Command::new("git")
            .args(["config", "filter.evil.clean", &clean_cmd])
            .current_dir(dir)
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "configuring filter failed");
        std::fs::write(dir.join(".gitattributes"), "a.txt filter=evil\n").unwrap();
        // Dirty the tracked file so `add --all` re-stages it (runs the filter).
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();

        let result = create_ghost_commit(dir).await;
        assert!(result.is_ok(), "snapshot should still succeed: {result:?}");
        assert!(
            !sentinel.exists(),
            "repo-configured clean filter must not execute during ghost snapshot"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_ghost_commit_ignores_repo_hooks_path() {
        // A repo points core.hooksPath at a dir whose post-index-change hook
        // writes a sentinel. read-tree/add must not run it.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);

        let hooks_dir = dir.join("evil-hooks");
        std::fs::create_dir(&hooks_dir).unwrap();
        let sentinel = dir.join("HOOK_RAN");
        let hook = hooks_dir.join("post-index-change");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch \"{}\"\n", sentinel.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ok = std::process::Command::new("git")
            .args(["config", "core.hooksPath", &hooks_dir.to_string_lossy()])
            .current_dir(dir)
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "configuring hooksPath failed");
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();

        let result = create_ghost_commit(dir).await;
        assert!(result.is_ok(), "snapshot should still succeed: {result:?}");
        assert!(
            !sentinel.exists(),
            "repo-configured hook must not execute during ghost snapshot"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ghost_snapshot_and_restore_round_trip_restores_working_tree() {
        // The core ghost-commit invariant: a snapshot captures the exact
        // working-tree state, and restore reverts every post-snapshot change
        // back to that captured state — tracked edits, new untracked files, and
        // deletions of files present at snapshot time all undone.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir); // commits a.txt = "hello\n"

        // Snapshot the working tree *as it is now*: dirty a tracked file and add
        // a fresh untracked file so both are captured into the ghost tree.
        std::fs::write(dir.join("a.txt"), "snapshot-state\n").unwrap();
        std::fs::write(dir.join("b.txt"), "untracked-but-captured\n").unwrap();

        let ghost = create_ghost_commit(dir).await.expect("snapshot");
        assert!(
            ghost.parent_id.is_some(),
            "snapshot taken on a repo with HEAD must record the parent commit"
        );

        // Mutate the tree away from the snapshot in three distinct ways.
        std::fs::write(dir.join("a.txt"), "DRIFTED\n").unwrap(); // edit tracked file
        std::fs::remove_file(dir.join("b.txt")).unwrap(); // delete a captured file
        std::fs::write(dir.join("c.txt"), "created-after-snapshot\n").unwrap(); // new file

        restore_ghost_commit(dir, &ghost).await.expect("restore");

        // Tracked edit reverted to the snapshot content.
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "snapshot-state\n",
            "tracked file must be restored to its snapshot content"
        );
        // Captured file that was deleted is recreated with snapshot content.
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "untracked-but-captured\n",
            "file present at snapshot time must be restored after deletion"
        );
        // File created after the snapshot is removed.
        assert!(
            !dir.join("c.txt").exists(),
            "post-snapshot file must be removed on restore"
        );
    }

    /// Create a linked worktree at `wt` off the repo at `repo`, isolated from
    /// the developer's global/system git config for determinism.
    #[cfg(unix)]
    fn add_worktree(repo: &Path, wt: &Path, branch: &str) {
        let ok = std::process::Command::new("git")
            .args(["worktree", "add", wt.to_str().unwrap(), "-b", branch])
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("spawn git worktree add")
            .success();
        assert!(ok, "git worktree add failed");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_main_repo_root_detects_real_worktree() {
        // End-to-end against a real `git worktree add`: the worktree checkout's
        // `.git` file must resolve back to the main repository root, both from
        // the worktree root and from a nested subdirectory within it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo_with_commit(&repo);

        let wt = tmp.path().join("wt");
        add_worktree(&repo, &wt, "feature/x");
        assert!(wt.join(".git").is_file(), "worktree .git should be a file");

        let expected = std::fs::canonicalize(&repo).unwrap();
        let got = resolve_main_repo_root(&wt).expect("worktree resolves to main root");
        assert_eq!(std::fs::canonicalize(&got).unwrap(), expected);

        let nested = wt.join("nested/sub");
        std::fs::create_dir_all(&nested).unwrap();
        let got_nested =
            resolve_main_repo_root(&nested).expect("nested worktree cwd resolves to main root");
        assert_eq!(std::fs::canonicalize(&got_nested).unwrap(), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_git_context_flags_linked_worktree() {
        // The full pipeline: collecting context inside a real worktree records a
        // `main_repo_root` distinct from `repo_root`, and the formatted block a
        // system prompt sees announces the linked worktree.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo_with_commit(&repo);

        let wt = tmp.path().join("wt");
        add_worktree(&repo, &wt, "feature/y");

        let ctx = collect_git_context(&wt).await;
        let repo_root = ctx.repo_root.clone().expect("repo_root");
        let main_root = ctx.main_repo_root.clone().expect("main_repo_root");
        assert_ne!(
            std::fs::canonicalize(&repo_root).unwrap(),
            std::fs::canonicalize(&main_root).unwrap(),
            "worktree repo_root must differ from the resolved main root"
        );
        assert_eq!(
            std::fs::canonicalize(&main_root).unwrap(),
            std::fs::canonicalize(&repo).unwrap(),
        );

        let formatted = format_git_context(&ctx);
        assert!(
            formatted.contains("Linked worktree of:"),
            "system prompt block should announce the linked worktree, got: {formatted:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_git_context_ordinary_repo_has_matching_roots() {
        // Sanity counterpart: an ordinary checkout reports equal roots and emits
        // no worktree line.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo_with_commit(&repo);

        let ctx = collect_git_context(&repo).await;
        assert_eq!(ctx.repo_root, ctx.main_repo_root);
        assert!(!format_git_context(&ctx).contains("Linked worktree of"));
    }
}
