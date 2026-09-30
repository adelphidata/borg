//! Git utilities for coding agent capabilities.
//!
//! Provides ghost commits (full repo snapshots for atomic undo), git context
//! collection for system prompt enrichment, and turn diff tracking.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::instrument;

/// Every git call here sits on the agent's turn path, so the ceiling is set by
/// how long a user will wait, not by how slow git can be: a cold `git status`
/// on a huge tree is better abandoned than allowed to stall the turn.
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

/// Blank the credential helper. A helper is a *command* git runs whenever it
/// wants a credential, and `credential.helper` is settable from a repository's
/// own `.git/config` — so a cloned tree could otherwise pick the program that
/// runs the moment any git call touches a remote-ish path. `GIT_TERMINAL_PROMPT=0`
/// only closes the interactive prompt; it says nothing about helpers.
const CREDENTIAL_HELPER_OVERRIDE: &str = "credential.helper=";
/// Blank `core.sshCommand`, which names the binary git execs for transport.
const SSH_COMMAND_OVERRIDE: &str = "core.sshCommand=";
/// Blank the external diff driver. `diff.external` runs a program for every
/// diffed path, and a repository's `.gitattributes` can steer paths at it.
const DIFF_EXTERNAL_OVERRIDE: &str = "diff.external=";
/// Refuse git's `ext::` transport, which treats the remote URL as a shell
/// command to run. Borg runs no `clone`/`fetch` itself today, so nothing here
/// reaches a transport — this is the guard that keeps it that way if one is
/// ever added, paired with the `url.*.insteadOf` rewrites that would otherwise
/// redirect an innocuous-looking URL into `ext::`.
const PROTOCOL_EXT_OVERRIDE: &str = "protocol.ext.allow=never";

/// Config `-c` overrides prepended to every git invocation so a repository's
/// own configuration can't execute code while we operate inside its tree.
/// Returned as a slice, not a fixed-size array: the arity is an implementation
/// detail, and baking it into every caller's type means the next `-c` pair
/// touches signatures and test indices that have nothing to do with it.
pub(crate) fn hardened_prelude() -> &'static [&'static str] {
    &[
        "-c",
        FSMONITOR_OVERRIDE,
        "-c",
        HOOKS_OVERRIDE,
        "-c",
        SAFE_BARE_REPOSITORY,
        "-c",
        CREDENTIAL_HELPER_OVERRIDE,
        "-c",
        SSH_COMMAND_OVERRIDE,
        "-c",
        DIFF_EXTERNAL_OVERRIDE,
        "-c",
        PROTOCOL_EXT_OVERRIDE,
    ]
}

/// Subcommands that take git's diff options, and so can be steered by a
/// repository's `.gitattributes` at a *named* diff driver. `stash` is handled
/// separately because only its `show` verb takes diff options.
const DIFF_OPTION_SUBCOMMANDS: &[&str] = &[
    "diff",
    "show",
    "log",
    "format-patch",
    "range-diff",
    "diff-tree",
    "diff-index",
    "diff-files",
];

/// Per-invocation flags that switch off named diff drivers.
///
/// `diff.external=` in [`hardened_prelude`] blanks the *global* external diff,
/// but a repository can also attach a named driver to any path
/// (`.gitattributes`: `* diff=evil`; `.git/config`: `[diff "evil"] command =
/// …` / `textconv = …`) and `-c` cannot enumerate drivers it has never heard
/// of. `--no-ext-diff` and `--no-textconv` refuse *every* driver's program
/// regardless of name, so they close the hole the prelude can't.
const DIFF_DRIVER_OVERRIDES: &[&str] = &["--no-ext-diff", "--no-textconv"];

/// Insert [`DIFF_DRIVER_OVERRIDES`] after the subcommand when that subcommand
/// takes diff options; leave every other argv untouched. `status`, `rev-parse`
/// and friends reject the flags outright, so a blanket insertion would break
/// them — the subcommand allowlist is what keeps this safe by default.
///
/// The flags go directly after the subcommand (or after `stash show`), which
/// is before any `--` pathspec separator the caller may have added.
pub(crate) fn harden_git_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let insert_at = match args {
        [sub, ..] if DIFF_OPTION_SUBCOMMANDS.contains(sub) => 1,
        ["stash", "show", ..] => 2,
        _ => return args.to_vec(),
    };
    let mut argv = Vec::with_capacity(args.len() + DIFF_DRIVER_OVERRIDES.len());
    argv.extend_from_slice(&args[..insert_at]);
    argv.extend_from_slice(DIFF_DRIVER_OVERRIDES);
    argv.extend_from_slice(&args[insert_at..]);
    argv
}

/// Harden a `git` invocation: scrub its environment and make it
/// non-interactive.
///
/// Every git command we run is local plumbing, so nothing should ever prompt.
/// Two independent channels have to be closed:
///
/// * **stdin** — `spawn` (unlike `.output()`) *inherits* stdin, so git would
///   otherwise get the operator's input stream. [`run_with_budget`] nulls it
///   too; this is the second belt, so a caller that spawns git by some other
///   route still can't hand it a live stdin.
/// * **the controlling terminal** — git opens `/dev/tty` *directly* for
///   credential prompts, so nulling stdin alone does not stop it.
///   `GIT_TERMINAL_PROMPT=0` turns a would-be prompt into an immediate error.
///
/// The environment is scrubbed for the same reason the shell tool, the hook
/// runner, and MCP stdio children scrub theirs: git is a child process that
/// prints what it is told to print, and the daemon's environment holds every
/// provider key the credential pool has loaded. A repository that configures
/// an alias or an external diff driver should not be able to read them, and
/// git needs none of them for local plumbing.
fn harden_git_command(cmd: &mut tokio::process::Command) {
    // Delegates rather than repeating the body: this is a security contract, and
    // the next hardening env var added to one copy would silently miss the other.
    harden_git_command_sync(cmd.as_std_mut());
}

/// [`harden_git_command`] for the blocking `std::process` git runners in
/// the patch applicator (which run under [`run_sync_with_timeout`]). A
/// credential prompt there would still burn the whole budget before erroring.
pub(crate) fn harden_git_command_sync(cmd: &mut std::process::Command) {
    // Filter first: it starts with `env_clear`, so anything set before this
    // call is dropped, and `GIT_TERMINAL_PROMPT` below must come after it —
    // the name is on the allowlist, so an ambient `=1` would otherwise win.
    crate::security::env_allowlist::apply_filtered_env_std(cmd, &HashMap::new());
    cmd.stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");

    // The allowlist also passes through `GIT_DIR`, `GIT_WORK_TREE`,
    // `GIT_CONFIG_GLOBAL`, and friends, which are left alone on purpose. They
    // are read from the *operator's* environment, not from anything a
    // repository controls, so overriding them defends only against an attacker
    // who already chose how the daemon starts — while pinning
    // `GIT_CONFIG_GLOBAL` at a null device would discard the operator's
    // `user.email`, and dropping `GIT_DIR` would break a deliberate setup. The
    // repository-sourced half of that surface is what the `-c` prelude covers,
    // and `-c` outranks every config file those variables could name.
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
    cmd: tokio::process::Command,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    run_with_budget(cmd, GIT_COMMAND_TIMEOUT).await
}

/// [`run_with_timeout`] with an explicit budget so tests can drive the
/// abandon path without burning a full `GIT_COMMAND_TIMEOUT` of wall clock.
async fn run_with_budget(
    mut cmd: tokio::process::Command,
    budget: Duration,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    crate::util::process_tree::spawn_in_own_group(&mut cmd);
    // `Command::output` sets these implicitly; `spawn` inherits instead, so
    // they must be stated or `wait_with_output` returns empty streams and git
    // gets the operator's stdin.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.kill_on_drop(true).spawn() {
        Ok(child) => child,
        Err(e) => return Ok(Err(e)),
    };
    // Read the pid before `wait_with_output` consumes the handle.
    let pid = child.id();
    // Pinned (not moved into `timeout`) so the `Child` is still owned when the
    // budget expires: dropping it first would let `kill_on_drop` reap the pid,
    // and the `killpg` below could then land on a recycled, unrelated group.
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    match tokio::time::timeout(budget, &mut wait).await {
        Ok(result) => Ok(result),
        Err(elapsed) => {
            crate::util::process_tree::kill_process_group(pid);
            Err(elapsed)
        }
    }
}

/// [`run_with_timeout`] for the blocking `std::process` git runners in the
/// patch applicator. Same contract: the budget expiring kills the whole
/// process tree, and the caller gets `ErrorKind::TimedOut` instead of a wedge.
/// Before this existed, those runners called `.output()` bare — a git stalled
/// on a dead NFS mount (or a filter that ignored the hardening prelude) held a
/// runtime worker hostage indefinitely.
pub(crate) fn run_sync_with_timeout(
    cmd: std::process::Command,
) -> std::io::Result<std::process::Output> {
    run_sync_with_budget(cmd, GIT_COMMAND_TIMEOUT)
}

/// [`run_sync_with_timeout`] with an explicit budget for tests.
pub(crate) fn run_sync_with_budget(
    mut cmd: std::process::Command,
    budget: Duration,
) -> std::io::Result<std::process::Output> {
    crate::util::process_tree::spawn_in_own_group_std(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;

    // Drain both pipes on their own threads while waiting, exactly as the
    // async path does via `wait_with_output`. Joining after the group kill is
    // safe: SIGKILL to the group closes every write end.
    fn drain(stream: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut stream = stream;
            // A truncated read is the expected outcome once the group is
            // killed; the caller only needs whatever bytes already arrived.
            let _ = std::io::Read::read_to_end(&mut stream, &mut buf);
            buf
        })
    }
    let stdout = child
        .stdout
        .take()
        .map(drain)
        .ok_or_else(|| std::io::Error::other("stdout was piped above"))?;
    let stderr = child
        .stderr
        .take()
        .map(drain)
        .ok_or_else(|| std::io::Error::other("stderr was piped above"))?;

    let status = crate::util::process_tree::wait_or_kill_group(&mut child, budget)?;
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
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
    harden_git_command(&mut discovery);
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
    /// Nested git repositories in the working tree. The snapshot holds each
    /// one only as a commit pointer, not its files, so a restore cannot bring
    /// their content back and must refuse.
    #[serde(default)]
    pub uncaptured_nested_repos: Vec<PathBuf>,
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
pub(crate) async fn run_git(args: &[&str], cwd: &Path) -> Result<String> {
    run_git_inner(args, cwd, &[]).await
}

/// `git status --porcelain` result, in the `Clone`able shape a shared future
/// needs. `anyhow::Error` is not `Clone`, so the error is flattened to its
/// rendered text — every consumer of this treats a failed status as "unknown"
/// and only ever reports the message.
type StatusResult = std::result::Result<String, String>;

/// In-flight `git status --porcelain` scans, keyed by canonical repository root.
///
/// `workspace_fingerprint` runs at every goal turn boundary and
/// `collect_git_context` at every prompt build, so on a large tree several
/// identical scans were routinely in flight at once. Entries are removed as
/// soon as the scan completes: this deduplicates concurrent callers, it is not
/// a cache, and a later caller always gets a fresh scan.
static STATUS_SCANS_IN_FLIGHT: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, futures::future::Shared<BoxedStatusFuture>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Removes a repository's in-flight entry when the scan task ends, however it
/// ends.
struct InFlightEntry(PathBuf);

impl Drop for InFlightEntry {
    fn drop(&mut self) {
        STATUS_SCANS_IN_FLIGHT
            .lock()
            // A poisoned lock means another scan panicked. The map holds no
            // invariant worth preserving, and skipping the removal here is the
            // one thing that must not happen.
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

type BoxedStatusFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = StatusResult> + Send + 'static>>;

/// Number of `git status` subprocesses actually started, per repository, for
/// the coalescing tests. Deduplication is invisible from the outside — callers
/// get the same answer either way — so counting the leaders is the only way to
/// assert it. Keyed per repository so a test that owns its own temporary repo
/// is unaffected by scans other tests run in parallel.
#[cfg(test)]
static STATUS_SCANS_STARTED: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// How many scans have been started for `repo_root` since the process began.
#[cfg(test)]
fn scans_started_for(repo_root: &Path) -> usize {
    let key = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let map = STATUS_SCANS_STARTED
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.get(&key).copied().unwrap_or(0)
}

/// Run `git status --porcelain` in `repo_root`, sharing one invocation between
/// concurrent callers for the same repository.
///
/// Keyed on the canonicalized root so a sibling directory, a symlink alias, and
/// the root itself all collapse to a single scan.
async fn git_status_porcelain(repo_root: &Path) -> StatusResult {
    let key = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    let shared = {
        // A std mutex, held only across the map lookup and insert — never
        // across the await below.
        let mut in_flight = match STATUS_SCANS_IN_FLIGHT.lock() {
            Ok(guard) => guard,
            // A poisoned lock means some other caller panicked mid-scan. The
            // map holds no invariant worth preserving, so carry on with it.
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(existing) = in_flight.get(&key) {
            existing.clone()
        } else {
            // Spawned rather than run inline so the scan still completes (and
            // still removes its entry) if the caller that started it is
            // cancelled mid-turn. A leader that vanished without clearing the
            // map would wedge every later caller behind a dead future.
            let scan_key = key.clone();
            #[cfg(test)]
            {
                let mut started = STATUS_SCANS_STARTED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *started.entry(key.clone()).or_insert(0) += 1;
            }
            let handle = tokio::spawn(async move {
                // A drop guard, not a trailing statement: if `run_git` panics
                // or the task is dropped, the entry must still go. A stranded
                // entry is permanent — the `Shared` future stays resolved, so
                // every later caller for this repo would be served the same
                // stale answer (or the same error) for the process lifetime,
                // and `workspace_fingerprint` would stop noticing real edits.
                let _entry = InFlightEntry(scan_key.clone());
                run_git(&["status", "--porcelain"], &scan_key)
                    .await
                    .map_err(|e| e.to_string())
            });
            let future: BoxedStatusFuture = Box::pin(async move {
                handle
                    .await
                    .unwrap_or_else(|e| Err(format!("git status task failed: {e}")))
            });
            let shared = futures::FutureExt::shared(future);
            in_flight.insert(key, shared.clone());
            shared
        }
    };

    shared.await
}

/// A cheap "has the tree changed?" token: `HEAD`, `git status --porcelain`,
/// and the size and modification time of each path that status lists.
///
/// Returns `None` outside a git repo, or when either probe fails — callers must
/// treat that as "unknown" rather than as "unchanged".
///
/// This is a progress hint, not a proof. The status text alone is the same
/// before and after a second edit to a file that is already modified or
/// untracked, which is the normal state of a worktree during a goal loop, so
/// the listed paths are also stat-ed. It still cannot see edits to ignored
/// files, inputs outside the repository, or files inside an untracked
/// directory. An equal fingerprint therefore never proves that a check would
/// give the same result, and no caller may skip work because of it.
///
/// Deliberately not a content hash of the worktree: this runs at every goal
/// turn boundary, and two bounded git probes plus a stat per dirty path are
/// affordable where walking the tree is not.
pub async fn workspace_fingerprint(cwd: &Path) -> Option<String> {
    let repo_root = find_repo_root(cwd)?;
    let (head, status) = tokio::join!(
        run_git(&["rev-parse", "HEAD"], &repo_root),
        git_status_porcelain(&repo_root),
    );
    // A repo with no commits yet has no HEAD; its status alone still
    // distinguishes states, so substitute a placeholder rather than bail.
    let head = head.unwrap_or_else(|_| "no-head".to_string());
    let status = status.ok()?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, head.as_bytes());
    sha2::Digest::update(&mut hasher, b"\0");
    sha2::Digest::update(&mut hasher, status.as_bytes());
    for line in status.lines() {
        let Some(path) = porcelain_line_path(line) else {
            continue;
        };
        // A path that cannot be stat-ed (deleted, or quoted in a way this
        // does not decode) adds nothing; its status line is already hashed.
        let Ok(meta) = tokio::fs::symlink_metadata(repo_root.join(path)).await else {
            continue;
        };
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        sha2::Digest::update(&mut hasher, b"\0");
        sha2::Digest::update(&mut hasher, path.as_bytes());
        sha2::Digest::update(&mut hasher, meta.len().to_le_bytes());
        sha2::Digest::update(&mut hasher, modified_nanos.to_le_bytes());
    }
    Some(format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::finalize(hasher)
    ))
}

/// The worktree path a `git status --porcelain` line refers to.
///
/// A line is `XY <path>`, or `XY <old> -> <new>` for a rename, where the new
/// path is the one on disk. Git wraps a path with unusual characters in
/// double quotes; the quotes are removed but escapes are not decoded, so such
/// a path may fail to resolve and the caller skips it.
///
/// The status text is trimmed as a whole, so the first line can have lost the
/// leading space of an ` M` code; then the path starts one byte earlier.
fn porcelain_line_path(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let rest = if bytes.get(2) == Some(&b' ') {
        line.get(3..)?
    } else if bytes.get(1) == Some(&b' ') {
        line.get(2..)?
    } else {
        return None;
    };
    let path = rest.rsplit_once(" -> ").map_or(rest, |(_, new)| new);
    let path = path
        .strip_prefix('"')
        .and_then(|p| p.strip_suffix('"'))
        .unwrap_or(path);
    (!path.is_empty()).then_some(path)
}

/// Shared git runner. Always prepends [`hardened_prelude`] so a repository's
/// own `core.fsmonitor`/hooks can't execute while we work in its tree, adds
/// the per-subcommand diff-driver overrides via [`harden_git_argv`], and
/// applies any `extra_env` (e.g. `GIT_INDEX_FILE`, filter-neutralizing
/// `GIT_CONFIG_*` entries).
#[instrument(name = "run_git", skip_all, fields(git.args = ?args))]
async fn run_git_inner(
    args: &[&str],
    cwd: &Path,
    extra_env: &[(String, String)],
) -> Result<String> {
    let stdout = run_git_bytes(args, cwd, extra_env).await?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

/// [`run_git_inner`] with stdout as raw bytes, for NUL-separated (`-z`)
/// output. A path list must not be trimmed or decoded lossily: a path can
/// start with a space or hold bytes that are not UTF-8.
async fn run_git_bytes(
    args: &[&str],
    cwd: &Path,
    extra_env: &[(String, String)],
) -> Result<Vec<u8>> {
    let mut argv: Vec<&str> = hardened_prelude().to_vec();
    argv.extend(harden_git_argv(args));
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&argv).current_dir(cwd);
    harden_git_command(&mut cmd);
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

    Ok(output.stdout)
}

fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Every untracked, non-ignored path in the working tree. `-z` gives the
/// literal names; the default output quotes and escapes a name with a quote,
/// a newline or a non-ASCII byte, and such a name would match no file.
async fn list_untracked(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let stdout = run_git_bytes(
        &["ls-files", "--others", "--exclude-standard", "-z"],
        repo_root,
        &[],
    )
    .await?;
    Ok(stdout
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
        .map(path_from_git_bytes)
        .collect())
}

/// Paths of the gitlink entries (mode 160000) in `ls-tree -r -z` output.
fn gitlink_paths(ls_tree_stdout: &[u8]) -> Vec<PathBuf> {
    const GITLINK_PREFIX: &[u8] = b"160000 commit ";
    ls_tree_stdout
        .split(|b| *b == 0)
        .filter(|record| record.starts_with(GITLINK_PREFIX))
        .filter_map(|record| {
            let tab = record.iter().position(|b| *b == b'\t')?;
            Some(path_from_git_bytes(&record[tab + 1..]))
        })
        .collect()
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
        git_status_porcelain(work_dir),
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

    // Every untracked file that exists now, unfiltered. This list is the
    // "do not delete" set for `restore_ghost_commit`, so it must be complete:
    // anything omitted here is treated at restore time as agent-created and
    // removed. Filtering it by size or directory name deleted the user's own
    // untracked files (a 12 MB dataset, a non-ignored `build/`) that the agent
    // never touched — unrecoverable, since they were in no commit.
    // A failed listing fails the snapshot for the same reason: an empty list
    // would mark every untracked file for removal.
    let preexisting_untracked = list_untracked(repo_root)
        .await
        .context("list untracked files for ghost snapshot")?;

    // `add`/`read-tree` would otherwise run repo-configured content filters
    // (`filter.<name>.clean`/`.process`) against an untrusted tree.
    let filter_env = filter_neutralizing_env(repo_root).await;

    if has_head {
        run_git_with_index(&tmp_path, &["read-tree", "HEAD"], repo_root, &filter_env).await?;
    }

    run_git_with_index(&tmp_path, &["add", "--all"], repo_root, &filter_env).await?;

    let tree_id = run_git_with_index(&tmp_path, &["write-tree"], repo_root, &filter_env).await?;

    // `add --all` records a nested repository as a gitlink: a commit id with
    // none of the files. Record those paths so restore can refuse.
    let uncaptured_nested_repos = gitlink_paths(
        &run_git_bytes(&["ls-tree", "-r", "-z", &tree_id], repo_root, &[])
            .await
            .context("list nested repositories in ghost snapshot")?,
    );

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
        uncaptured_nested_repos,
    })
}

/// Restore working tree to match a ghost commit.
#[instrument(skip_all)]
pub async fn restore_ghost_commit(repo_root: &Path, commit: &GhostCommit) -> Result<()> {
    if !commit.uncaptured_nested_repos.is_empty() {
        let paths = commit
            .uncaptured_nested_repos
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "ghost undo refused: the snapshot did not capture the content of nested \
             repositories ({paths}); nothing was changed"
        );
    }

    // Restore all tracked files from the ghost commit. Worktree checkout runs
    // `filter.<name>.smudge`/`.process` drivers, so neutralize them too.
    let filter_env = filter_neutralizing_env(repo_root).await;
    // `git restore` writes file by file with no transaction, so a mid-run
    // failure leaves the tree part-restored. Do not early-return on it: the
    // post-snapshot cleanup below is the other half of the undo, and skipping
    // it would additionally leave every agent-created file in place. Run both
    // phases, then report.
    let restored = run_git_inner(
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
    .await;

    // Remove files that were created after the snapshot (not in
    // preexisting_untracked). A failed listing is not an empty listing: skip
    // the removal and report it.
    let current_untracked = list_untracked(repo_root).await;

    for path in current_untracked.iter().flatten() {
        if !commit.preexisting_untracked.contains(path) {
            let full = repo_root.join(path);
            // `symlink_metadata`, so a dangling link counts as present.
            if std::fs::symlink_metadata(&full).is_ok() {
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

    // Surface a part-restored tree explicitly. The caller must not report a
    // clean undo when some paths are at snapshot state and others are not.
    restored.context(
        "ghost undo failed partway: the working tree may hold a mix of \
         snapshot and post-snapshot content — inspect `git status` before continuing",
    )?;
    current_untracked.context(
        "ghost undo incomplete: the untracked files could not be listed, so files \
         created after the snapshot were not removed",
    )?;

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

    /// Build a throwaway repo with a dirty worktree so `git status` has
    /// something to report.
    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("tracked.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn porcelain_line_path_finds_the_path_on_disk() {
        let cases: &[(&str, Option<&str>)] = &[
            (" M src/lib.rs", Some("src/lib.rs")),
            ("?? notes.txt", Some("notes.txt")),
            ("A  new file.rs", Some("new file.rs")),
            // The first line of a trimmed status text.
            ("M src/lib.rs", Some("src/lib.rs")),
            ("D gone.rs", Some("gone.rs")),
            ("R  old.rs -> new.rs", Some("new.rs")),
            ("?? \"odd name.txt\"", Some("odd name.txt")),
            ("?? dir/", Some("dir/")),
            ("", None),
            ("??", None),
        ];
        for (line, expected) in cases {
            assert_eq!(porcelain_line_path(line), *expected, "line {line:?}");
        }
    }

    /// Spec: the fingerprint is how the goal loop sees that the agent did
    /// work. During a goal loop the files are already dirty, so a second
    /// edit leaves the status text unchanged; the fingerprint must still
    /// move. An untouched tree must keep its fingerprint.
    #[tokio::test]
    async fn fingerprint_moves_on_a_second_edit_to_a_dirty_file() {
        let repo = temp_repo();
        let file = repo.path().join("tracked.txt");

        let first = workspace_fingerprint(repo.path()).await.expect("in a repo");
        let again = workspace_fingerprint(repo.path()).await.expect("in a repo");
        assert_eq!(first, again, "an untouched tree keeps its fingerprint");

        let status_before = git_status_porcelain(repo.path()).await.unwrap();
        std::fs::write(&file, "hello, with a longer second edit").unwrap();
        let status_after = git_status_porcelain(repo.path()).await.unwrap();
        assert_eq!(
            status_before, status_after,
            "precondition: the status text alone cannot see this edit"
        );

        let second = workspace_fingerprint(repo.path()).await.expect("in a repo");
        assert_ne!(first, second, "the second edit must move the fingerprint");
    }

    /// Spec: concurrent callers for one repository share a single
    /// `git status` subprocess. `workspace_fingerprint` (every goal turn
    /// boundary) and `collect_git_context` (every prompt build) overlap
    /// routinely, and each was paying for its own scan of the whole tree.
    #[tokio::test]
    async fn concurrent_status_scans_share_one_subprocess() {
        let repo = temp_repo();

        // All eight futures are constructed first, so the first one to be
        // polled registers the in-flight entry that the other seven find.
        // `git status` takes milliseconds; the remaining polls take
        // microseconds, so the leader cannot finish before they arrive.
        let results =
            futures::future::join_all((0..8).map(|_| git_status_porcelain(repo.path()))).await;

        assert_eq!(
            scans_started_for(repo.path()),
            1,
            "eight concurrent callers must share one scan"
        );
        let first = results[0].as_ref().expect("status succeeds");
        for r in &results {
            assert_eq!(
                r.as_ref().expect("every caller gets the result"),
                first,
                "a shared scan must give every caller the same answer"
            );
        }
        assert!(
            first.contains("tracked.txt"),
            "the untracked file must show up: {first:?}"
        );
    }

    /// Spec: deduplication is per repository. Two repos must not collapse into
    /// one scan, or a second workspace would read the first one's status.
    #[tokio::test]
    async fn different_repositories_scan_independently() {
        let a = temp_repo();
        let b = temp_repo();
        std::fs::write(b.path().join("only-in-b.txt"), "x").unwrap();

        let (ra, rb) = tokio::join!(
            git_status_porcelain(a.path()),
            git_status_porcelain(b.path())
        );
        assert_eq!(
            (scans_started_for(a.path()), scans_started_for(b.path())),
            (1, 1),
            "two repositories need one scan each, not a shared one"
        );

        assert!(!ra.unwrap().contains("only-in-b.txt"));
        assert!(rb.unwrap().contains("only-in-b.txt"));
    }

    /// Spec: this deduplicates, it does not cache. A caller arriving after the
    /// previous scan finished must see changes made in between.
    #[tokio::test]
    async fn a_later_caller_gets_a_fresh_scan() {
        let repo = temp_repo();
        let first = git_status_porcelain(repo.path()).await.unwrap();
        assert!(!first.contains("added-later.txt"));

        std::fs::write(repo.path().join("added-later.txt"), "x").unwrap();

        let second = git_status_porcelain(repo.path()).await.unwrap();
        assert_eq!(
            scans_started_for(repo.path()),
            2,
            "the completed scan must have been evicted, not reused"
        );
        assert!(
            second.contains("added-later.txt"),
            "a stale answer would make the goal-loop fingerprint miss real work: {second:?}"
        );
    }

    /// Spec: a scan that ends abnormally must still release its in-flight
    /// entry. A stranded entry is permanent — the resolved `Shared` future
    /// would serve the same answer to every later caller for that repository,
    /// so `workspace_fingerprint` would stop seeing real edits and the goal
    /// quality gates would count real work as no progress.
    #[tokio::test]
    async fn an_abandoned_scan_releases_its_slot() {
        let repo = temp_repo();
        let key = std::fs::canonicalize(repo.path()).expect("canonicalize");

        // Park an entry the way a leader task does, then end the task by
        // unwinding rather than returning.
        let panicked = tokio::spawn({
            let key = key.clone();
            async move {
                let _entry = InFlightEntry(key.clone());
                STATUS_SCANS_IN_FLIGHT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        key,
                        futures::FutureExt::shared(
                            Box::pin(std::future::pending()) as BoxedStatusFuture
                        ),
                    );
                panic!("the scan subprocess blew up");
            }
        })
        .await;
        assert!(panicked.is_err(), "the test's scan task must have panicked");

        assert!(
            STATUS_SCANS_IN_FLIGHT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .is_none(),
            "a panicking scan must still release its slot, or this repository \
             is wedged on a stale answer for the process lifetime"
        );

        // The repository scans normally afterwards rather than resolving to
        // the parked future, which would never complete.
        let status = git_status_porcelain(repo.path())
            .await
            .expect("a released slot must allow a fresh scan");
        assert!(status.contains("tracked.txt"));
    }

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
            uncaptured_nested_repos: vec![],
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
            uncaptured_nested_repos: vec![PathBuf::from("vendor/lib")],
        };
        let json = serde_json::to_string(&gc).unwrap();
        let deserialized: GhostCommit = serde_json::from_str(&json).unwrap();
        assert!(deserialized.parent_id.is_none());
        assert!(deserialized.preexisting_untracked.is_empty());
        assert_eq!(
            deserialized.uncaptured_nested_repos,
            vec![PathBuf::from("vendor/lib")]
        );
    }

    #[test]
    fn ghost_commit_without_the_nested_repo_field_still_loads() {
        let old = r#"{"commit_id":"abc","parent_id":null,"preexisting_untracked":["a.txt"]}"#;
        let gc: GhostCommit = serde_json::from_str(old).unwrap();
        assert_eq!(gc.preexisting_untracked, vec![PathBuf::from("a.txt")]);
        assert!(gc.uncaptured_nested_repos.is_empty());
    }

    #[test]
    fn gitlink_paths_selects_only_mode_160000_records() {
        let out = b"100644 blob aaaa\ta.txt\x00160000 commit bbbb\tvendor/my lib\0\
                    040000 tree cccc\tdir\x00160000 commit dddd\tnested\0";
        assert_eq!(
            gitlink_paths(out),
            vec![PathBuf::from("vendor/my lib"), PathBuf::from("nested")]
        );
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
        // These exact flags are the security invariant: fsmonitor, hooks, and
        // bare-repo discovery are the repo-config code-execution vectors on the
        // plumbing we run. Read as `-c`/value pairs so adding another override
        // doesn't shuffle a pile of hardcoded indices.
        let prelude = hardened_prelude();
        assert_eq!(
            prelude.len() % 2,
            0,
            "prelude must be whole -c/value pairs: {prelude:?}"
        );
        let overrides: Vec<&str> = prelude
            .chunks_exact(2)
            .map(|pair| {
                assert_eq!(pair[0], "-c", "every override must carry its own -c");
                pair[1]
            })
            .collect();

        assert!(overrides.contains(&"core.fsmonitor=false"));
        assert!(overrides.contains(&"safe.bareRepository=explicit"));
        // The remaining repo-config keys that name a program git will execute.
        // Each must be blanked, not merely present: an override that set a
        // value would substitute our own choice of helper for the repo's.
        for key in ["credential.helper", "core.sshCommand", "diff.external"] {
            let entry = overrides
                .iter()
                .find(|o| o.starts_with(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{key} must be overridden; got {overrides:?}"));
            assert_eq!(
                *entry,
                format!("{key}="),
                "{key} must be blanked, not pointed at another program"
            );
        }
        // `ext::` treats the remote URL as a shell command.
        assert!(overrides.contains(&"protocol.ext.allow=never"));
        // Null target must not point back at a usable directory.
        let hooks = overrides
            .iter()
            .find(|o| o.starts_with("core.hooksPath="))
            .expect("hooks must be routed to a null path");
        assert!(*hooks == "core.hooksPath=/dev/null" || *hooks == "core.hooksPath=NUL");
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
            prelude,
            "filter discovery must run under the hardening prelude: {argv:?}"
        );
        assert_eq!(argv[prelude.len()], "config");
    }

    /// Spec: every diff-producing subcommand gets both driver overrides, and
    /// every subcommand that would reject them gets nothing. Table-driven so
    /// the allowlist and the "leave unknown argv alone" rule are one test.
    #[test]
    fn harden_git_argv_adds_driver_overrides_only_where_git_accepts_them() {
        let hardened: &[(&[&str], &[&str])] = &[
            (&["diff"], &["diff", "--no-ext-diff", "--no-textconv"]),
            (
                &["diff", "--", "a.txt"],
                &["diff", "--no-ext-diff", "--no-textconv", "--", "a.txt"],
            ),
            (
                &["show", "HEAD"],
                &["show", "--no-ext-diff", "--no-textconv", "HEAD"],
            ),
            (
                &["log", "--oneline", "-5"],
                &["log", "--no-ext-diff", "--no-textconv", "--oneline", "-5"],
            ),
            (
                &["format-patch", "-1", "--stdout"],
                &[
                    "format-patch",
                    "--no-ext-diff",
                    "--no-textconv",
                    "-1",
                    "--stdout",
                ],
            ),
            (
                &["stash", "show", "-p"],
                &["stash", "show", "--no-ext-diff", "--no-textconv", "-p"],
            ),
        ];
        for (input, expected) in hardened {
            assert_eq!(&harden_git_argv(input), expected, "input {input:?}");
        }

        // Subcommands that reject diff options must pass through verbatim —
        // an inserted flag here is a broken `git status` on every turn.
        let untouched: &[&[&str]] = &[
            &["status", "--porcelain"],
            &["rev-parse", "HEAD"],
            &["stash", "list"],
            &["stash", "push", "-u"],
            &["worktree", "list", "--porcelain"],
            &["ls-files", "--others"],
            &["commit", "-qm", "x"],
            &[],
        ];
        for input in untouched {
            assert_eq!(&harden_git_argv(input), input, "input {input:?}");
        }
    }

    /// Spec: a repository's named diff driver must never run under `run_git`.
    ///
    /// `.gitattributes` can point any path at a driver whose `command` and
    /// `textconv` are programs; `-c diff.external=` does not reach a *named*
    /// driver. Each driver here writes a marker file when it runs. A control
    /// run without the hardening proves the fixture really fires — otherwise
    /// the assertion would pass on a driver git simply ignored.
    #[cfg(unix)]
    #[tokio::test]
    async fn named_diff_drivers_never_execute_under_run_git() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let command_marker = tmp.path().join("command-ran");
        let textconv_marker = tmp.path().join("textconv-ran");

        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(std::process::Stdio::null())
                .status()
                .expect("spawn git")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@borg.test"]);
        git(&["config", "user.name", "borg-test"]);
        // `;` makes git hand the command to `sh -c`, with the diff arguments
        // appended after it, so the marker write is unconditional.
        git(&[
            "config",
            "diff.evilcmd.command",
            &format!("touch {}; :", command_marker.display()),
        ]);
        git(&[
            "config",
            "diff.eviltc.textconv",
            &format!("touch {}; cat", textconv_marker.display()),
        ]);
        std::fs::write(
            repo.join(".gitattributes"),
            "a.txt diff=evilcmd\nb.txt diff=eviltc\n",
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join("b.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.join("b.txt"), "two\n").unwrap();

        // Control: plain git runs both drivers on this fixture.
        git(&["diff"]);
        assert!(
            command_marker.exists() && textconv_marker.exists(),
            "control: the fixture's drivers must fire under an unhardened git"
        );
        std::fs::remove_file(&command_marker).unwrap();
        std::fs::remove_file(&textconv_marker).unwrap();

        for args in [
            &["diff"][..],
            &["diff", "--", "a.txt", "b.txt"],
            &["show", "HEAD"],
            &["log", "-p", "-1"],
        ] {
            run_git(args, &repo)
                .await
                .unwrap_or_else(|e| panic!("git {args:?} must still succeed: {e}"));
            assert!(
                !command_marker.exists(),
                "git {args:?} ran the repository's diff command driver"
            );
            assert!(
                !textconv_marker.exists(),
                "git {args:?} ran the repository's textconv driver"
            );
        }
    }

    /// A command that outlives [`GIT_COMMAND_TIMEOUT`] must not leave orphans
    /// behind. The natural spelling — `timeout(.., cmd.output())` — abandons
    /// the child with no signal at all; here the descendant git would spawn
    /// (a credential helper, fsmonitor) stands in for that leak.
    ///
    /// Exercises `run_with_budget` directly rather than `run_git`: the leak is
    /// in the timeout wrapper, and driving it needs a program that blocks
    /// deterministically without a real repo, network, or `PATH` mutation
    /// (which is process-global and would race sibling tests). The budget is
    /// injected so the test doesn't spend a full `GIT_COMMAND_TIMEOUT` idling.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_with_timeout_kills_the_whole_process_tree() {
        const BUDGET: Duration = Duration::from_millis(500);
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
        let result = run_with_budget(cmd, BUDGET).await;
        assert!(
            result.is_err(),
            "a command that outlives the budget must report the timeout"
        );
        assert!(
            started.elapsed() >= BUDGET,
            "must not have returned before the budget elapsed"
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("shell should have recorded its background job's pid")
            .trim()
            .parse()
            .expect("pid");
        let gone = crate::util::process_tree::wait_for_pid_exit(pid).await;
        assert!(gone, "descendant {pid} outlived the timed-out command");
    }

    /// [`run_with_timeout_kills_the_whole_process_tree`] for the blocking
    /// runner behind the patch applicator. Same spec: the budget expiring must
    /// surface `TimedOut` (not wedge the worker) and must not leave the
    /// command's descendants running.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_sync_with_budget_times_out_and_kills_the_tree() {
        const BUDGET: Duration = Duration::from_millis(500);
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("child.pid");

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "sleep 120 & echo $! > {}; sleep 120",
            pid_file.to_string_lossy()
        ));

        let err = tokio::task::spawn_blocking(move || run_sync_with_budget(cmd, BUDGET))
            .await
            .expect("join")
            .expect_err("a command that outlives the budget must report the timeout");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("shell should have recorded its background job's pid")
            .trim()
            .parse()
            .expect("pid");
        let gone = crate::util::process_tree::wait_for_pid_exit(pid).await;
        assert!(gone, "descendant {pid} outlived the timed-out command");
    }

    /// Output larger than the ~64 KiB pipe buffer must not deadlock the
    /// blocking runner: draining only after `try_wait` reports exit would
    /// block the child in `write` forever and misreport it as a timeout.
    #[cfg(unix)]
    #[test]
    fn run_sync_with_budget_captures_output_larger_than_the_pipe_buffer() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("yes x | head -c 262144");
        let output = run_sync_with_budget(cmd, Duration::from_secs(10)).expect("run");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 262144);
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
        // `git commit` can start a detached `maintenance --auto` that writes
        // under `.git/objects` after the commit returns, racing the tests.
        run(&["config", "maintenance.auto", "false"]);
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

    #[tokio::test]
    async fn ghost_restore_keeps_preexisting_untracked_files_the_agent_never_touched() {
        // Regression: `preexisting_untracked` was filtered by file size and by
        // build-directory name, but the deletion sweep compares the *full*
        // untracked listing against it. Anything the filter dropped therefore
        // looked agent-created at restore time and was deleted — the user's own
        // large or build-dir untracked files, unrecoverable because they had
        // never been committed.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);

        // A big untracked file (past the old 10 MiB cap) and one inside a
        // build-artifact directory that is NOT gitignored here.
        let big = dir.join("dataset.csv");
        std::fs::write(&big, vec![b'x'; 11 * 1024 * 1024]).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let in_build = dir.join("build/keep.txt");
        std::fs::write(&in_build, "user's own artifact\n").unwrap();

        let ghost = create_ghost_commit(dir).await.expect("snapshot");

        // The agent then creates a file of its own, which restore *should* remove.
        std::fs::write(dir.join("agent-made.txt"), "scratch\n").unwrap();

        restore_ghost_commit(dir, &ghost).await.expect("restore");

        assert!(
            big.exists(),
            "a large pre-existing untracked file must survive ghost undo"
        );
        assert!(
            in_build.exists(),
            "a pre-existing untracked file under build/ must survive ghost undo"
        );
        assert!(
            !dir.join("agent-made.txt").exists(),
            "genuinely post-snapshot files must still be removed"
        );
    }

    fn object_file_count(repo: &Path) -> usize {
        fn count(path: &Path) -> usize {
            if !path.is_dir() {
                return 1;
            }
            std::fs::read_dir(path)
                .unwrap()
                .flatten()
                .map(|e| count(&e.path()))
                .sum()
        }
        // `info/` holds metadata such as the commit-graph, not objects.
        std::fs::read_dir(repo.join(".git/objects"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "info")
            .map(|e| count(&e.path()))
            .sum()
    }

    #[tokio::test]
    async fn ghost_snapshot_fails_when_the_untracked_listing_fails() {
        // An incomplete "pre-existing untracked" list makes restore delete the
        // user's files, so a failed listing must fail the snapshot.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);
        std::fs::write(dir.join("notes.txt"), "user's untracked file\n").unwrap();
        // `ls-files` reads the repository index; the snapshot's own index is
        // a temp file, so every other step still works.
        std::fs::write(dir.join(".git/index"), b"not an index").unwrap();
        let objects_before = object_file_count(dir);

        let err = create_ghost_commit(dir)
            .await
            .expect_err("a failed listing must fail the snapshot");

        assert!(
            format!("{err:#}").contains("list untracked files for ghost snapshot"),
            "got: {err:#}"
        );
        assert_eq!(
            object_file_count(dir),
            objects_before,
            "no snapshot object may be written"
        );
    }

    #[tokio::test]
    async fn ghost_restore_refuses_when_a_nested_repository_was_not_captured() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);
        let nested = dir.join("vendor lib");
        std::fs::create_dir_all(&nested).unwrap();
        init_repo_with_commit(&nested);
        std::fs::write(nested.join("a.txt"), "nested at snapshot\n").unwrap();

        let ghost = create_ghost_commit(dir).await.expect("snapshot");
        assert_eq!(
            ghost.uncaptured_nested_repos,
            vec![PathBuf::from("vendor lib")]
        );

        std::fs::write(nested.join("a.txt"), "nested after snapshot\n").unwrap();
        std::fs::write(dir.join("a.txt"), "outer after snapshot\n").unwrap();
        std::fs::write(dir.join("new.txt"), "created later\n").unwrap();

        let err = restore_ghost_commit(dir, &ghost)
            .await
            .expect_err("restore must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vendor lib"),
            "error must name the path: {msg}"
        );
        assert!(msg.contains("nested repositories"), "{msg}");

        // Refused before any write: nothing moved, inside or outside.
        assert_eq!(
            std::fs::read_to_string(nested.join("a.txt")).unwrap(),
            "nested after snapshot\n"
        );
        assert!(nested.join(".git").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "outer after snapshot\n"
        );
        assert!(dir.join("new.txt").exists());
    }

    #[tokio::test]
    async fn ghost_restore_compares_untracked_names_literally() {
        // Without `-z`, git prints such names quoted and escaped. They then
        // match no file on disk and no entry in the pre-existing list.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);
        let kept = [" caf\u{e9} notes.txt", "say \"hi\".txt"];
        for name in kept {
            std::fs::write(dir.join(name), "user's own\n").unwrap();
        }

        let ghost = create_ghost_commit(dir).await.expect("snapshot");
        for name in kept {
            assert!(
                ghost.preexisting_untracked.contains(&PathBuf::from(name)),
                "{name:?} missing from {:?}",
                ghost.preexisting_untracked
            );
        }

        let created = ["na\u{ef}ve new.txt", "plain-new.txt"];
        for name in created {
            std::fs::write(dir.join(name), "agent-made\n").unwrap();
        }
        restore_ghost_commit(dir, &ghost).await.expect("restore");

        for name in kept {
            assert_eq!(
                std::fs::read_to_string(dir.join(name)).unwrap(),
                "user's own\n",
                "{name:?} must survive"
            );
        }
        for name in created {
            assert!(
                !dir.join(name).exists(),
                "post-snapshot file {name:?} must be removed"
            );
        }
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

    #[test]
    fn hardening_replaces_the_environment_rather_than_adding_to_it() {
        let mut cmd = std::process::Command::new("git");
        // Whatever a caller staged before hardening must not survive: the
        // filter is the whole environment, not an overlay on the ambient one.
        cmd.env("OPENROUTER_API_KEY", "sk-should-not-survive");
        // An ambient opt-in to prompting must lose to the explicit override.
        cmd.env("GIT_TERMINAL_PROMPT", "1");

        harden_git_command_sync(&mut cmd);

        let envs: HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();

        assert!(
            !envs.contains_key("OPENROUTER_API_KEY"),
            "a provider key must never reach a git child"
        );
        assert_eq!(
            envs.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0"),
            "the non-interactive override must be applied after the filter"
        );
        assert!(
            envs.contains_key("PATH"),
            "git needs PATH; the filter must re-add it explicitly, since env_clear \
             means inheritance is no longer available"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_git_child_cannot_read_a_secret_from_its_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);

        // A `!`-prefixed alias is the shape that matters: it runs a program of
        // the repository's choosing, which is exactly the reader we are denying.
        let dump = |hardened: bool| -> String {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["-c", "alias.leak=!printenv", "leak"])
                .current_dir(dir)
                .env("OPENROUTER_API_KEY", "sk-should-not-survive");
            if hardened {
                harden_git_command_sync(&mut cmd);
            }
            let out = cmd.output().expect("spawn git");
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        assert!(
            dump(false).contains("sk-should-not-survive"),
            "control: without hardening a git child does read the parent env, \
             so the assertion below is testing something real"
        );
        assert!(
            !dump(true).contains("sk-should-not-survive"),
            "the hardened runner must not hand a provider key to git"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_still_functions_under_the_scrubbed_environment() {
        // The filter is only safe if git keeps working: PATH, HOME and the
        // locale vars have to survive it, or every git call fails to spawn.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);

        let head = run_git_inner(&["rev-parse", "--abbrev-ref", "HEAD"], dir, &[])
            .await
            .expect("git must still run with a filtered environment");
        assert!(!head.is_empty(), "expected a branch name, got {head:?}");
    }
}
