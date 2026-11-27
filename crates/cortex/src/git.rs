//! Git utilities for coding agent capabilities.
//!
//! Provides the hardened git runners, git context collection for system
//! prompt enrichment, workspace fingerprints, and (in [`checkpoints`]) the
//! undo points taken before a turn changes files.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::instrument;

pub mod checkpoints;

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

/// [`harden_git_command`] on a `std::process::Command`: the one body both
/// the async runners and the blocking probes in tests share.
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
/// the `GIT_CONFIG_*` env entries that neutralize them.
///
/// Fails closed: when discovery cannot run, or the repository carries a
/// conditional include (see [`neutralizing_env_from_probes`]), the caller gets
/// an error and must not run the git command it was guarding. Running it
/// without overrides is exactly the case the overrides exist to prevent.
async fn filter_neutralizing_env(cwd: &Path) -> Result<Vec<(String, String)>> {
    let (includes, filters) = tokio::join!(repo_conditional_include_probe(cwd), async {
        config_probe_stdout(
            "filter-driver",
            run_config_probe(cwd, filter_discovery_argv()).await,
        )
    });
    neutralizing_env_from_probes(includes, filters)
}

/// Run one `git config` probe under the timeout. A timeout comes back as an
/// `ErrorKind::TimedOut` error, the same shape the blocking runner reports.
async fn run_config_probe(
    cwd: &Path,
    argv: Vec<&'static str>,
) -> std::io::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(argv).current_dir(cwd);
    harden_git_command(&mut cmd);
    run_with_timeout(cmd).await.unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ))
    })
}

/// The repository's conditional includes, as `<scope>\0<key>\0` records (see
/// [`repo_conditional_includes`]). Uses `--show-scope` (git 2.26+), and on an
/// older git that rejects the option falls back to per-scope probes.
async fn repo_conditional_include_probe(cwd: &Path) -> Result<String> {
    let scoped = run_config_probe(cwd, conditional_include_probe_argv()).await;
    if let Some(stdout) = scoped_include_probe_stdout(scoped)? {
        return Ok(stdout);
    }
    let local = run_config_probe(cwd, legacy_include_probe_argv("--local")).await;
    let worktree = run_config_probe(cwd, legacy_include_probe_argv("--worktree")).await;
    legacy_include_probe_stdout(local, worktree)
}

/// Outcome of the `--show-scope` conditional-include probe: its stdout, or
/// `None` when this git predates the option (2.26) and the per-scope fallback
/// must run instead. Any other failure is an error.
///
/// An unknown option is a usage error: git exits 129 and says "unknown
/// option". Either signal counts, so a git that words it differently still
/// takes the fallback rather than refusing every guarded command.
fn scoped_include_probe_stdout(
    output: std::io::Result<std::process::Output>,
) -> Result<Option<String>> {
    if let Ok(o) = &output {
        let failed = !o.status.success() && o.status.code() != Some(1);
        let unsupported = failed
            && (o.status.code() == Some(129)
                || String::from_utf8_lossy(&o.stderr).contains("unknown option"));
        if unsupported {
            tracing::debug!("git config lacks --show-scope; probing includes per scope");
            return Ok(None);
        }
    }
    config_probe_stdout("conditional-include", output).map(Some)
}

/// Argv (hardening prelude included) for the pre-2.26 fallback: the
/// conditional-include keys of one scope (`--local` or `--worktree`).
/// `--includes` is stated because a scope flag turns include-following off by
/// default, and an `includeIf` nested in a file the repository includes
/// unconditionally must still show up.
pub(crate) fn legacy_include_probe_argv(scope: &'static str) -> Vec<&'static str> {
    let mut argv: Vec<&str> = hardened_prelude().to_vec();
    argv.extend([
        "config",
        scope,
        "--includes",
        "--null",
        "--name-only",
        "--get-regexp",
        CONDITIONAL_INCLUDE_PATTERN,
    ]);
    argv
}

/// Combine the pre-2.26 `--local` and `--worktree` probes into the
/// `<scope>\0<key>\0` records the `--show-scope` probe produces.
///
/// The local probe must succeed: its failure is fail-closed, and the error
/// asks for a newer git, the one change that removes this path. The worktree
/// probe may fail and is skipped — git rejects `--worktree` when it predates
/// 2.20 or when a repository with several worktrees has not enabled
/// `extensions.worktreeConfig`, and in both cases git reads no worktree
/// config file at all, so there is no include there to miss.
fn legacy_include_probe_stdout(
    local: std::io::Result<std::process::Output>,
    worktree: std::io::Result<std::process::Output>,
) -> Result<String> {
    let local = config_probe_stdout("conditional-include", local).context(
        "cannot check this repository's config for conditional includes; upgrade git to 2.26+",
    )?;
    let worktree = match config_probe_stdout("worktree conditional-include", worktree) {
        Ok(stdout) => stdout,
        Err(e) => {
            tracing::debug!("skipping worktree-scope include probe: {e:#}");
            String::new()
        }
    };
    // Without `extensions.worktreeConfig`, `--worktree` reads the local file
    // again, so a key both probes report is listed once.
    let mut seen = std::collections::HashSet::new();
    let mut records = String::new();
    for (scope, stdout) in [("local", &local), ("worktree", &worktree)] {
        for key in stdout
            .split('\0')
            .filter(|k| !k.is_empty() && seen.insert(*k))
        {
            records.push_str(scope);
            records.push('\0');
            records.push_str(key);
            records.push('\0');
        }
    }
    Ok(records)
}

/// Stdout of a `git config --get-regexp` probe, or an error when the probe did
/// not run to completion. `--get-regexp` exits 1 when nothing matches — the
/// common case — so only other exit codes are failures.
fn config_probe_stdout(
    probe: &str,
    output: std::io::Result<std::process::Output>,
) -> Result<String> {
    let output = output.with_context(|| format!("git {probe} discovery failed to run"))?;
    if output.status.success() || output.status.code() == Some(1) {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    anyhow::bail!(
        "git {probe} discovery failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Combine the two discovery probes into the neutralizing env, refusing a
/// repository that carries its own conditional includes.
///
/// Discovery reads the configuration as it applies *now*, in this directory,
/// on this branch. An `includeIf` in the repository's own config can load a
/// different file under conditions the guarded command itself brings about —
/// `onbranch:` once `worktree add` checks out a new branch, `gitdir:` in the
/// new checkout's directory — and a filter defined there was never seen, so
/// it cannot be blanked. The operator's own (global/system) includes are not
/// repository-controlled and are left alone.
pub(crate) fn neutralizing_env_from_probes(
    includes: Result<String>,
    filters: Result<String>,
) -> Result<Vec<(String, String)>> {
    let includes = repo_conditional_includes(&includes?);
    if !includes.is_empty() {
        anyhow::bail!(
            "refusing to run git here: this repository's config has conditional \
             includes ({}) that can load content filters Borg cannot neutralize. \
             Remove the includeIf entries from the repository's .git/config, then retry",
            includes.join(", ")
        );
    }
    Ok(neutralizing_env_from_stdout(&filters?))
}

/// Pattern for `includeIf.<condition>.path` keys (git lowercases the section).
const CONDITIONAL_INCLUDE_PATTERN: &str = r"^includeif\.";

/// Argv (hardening prelude included) that lists every conditional include
/// with the scope it came from. Includes are followed, so an `includeIf`
/// nested in a file the repository includes unconditionally still shows up —
/// attributed to the scope of the file that included it.
pub(crate) fn conditional_include_probe_argv() -> Vec<&'static str> {
    let mut argv: Vec<&str> = hardened_prelude().to_vec();
    argv.extend([
        "config",
        "--show-scope",
        "--null",
        "--name-only",
        "--get-regexp",
        CONDITIONAL_INCLUDE_PATTERN,
    ]);
    argv
}

/// The conditional-include keys in `--show-scope --null --name-only` output
/// (`<scope>\0<key>\0` records) that the repository itself controls: the
/// `local` and `worktree` scopes.
pub(crate) fn repo_conditional_includes(stdout: &str) -> Vec<String> {
    let fields: Vec<&str> = stdout.split('\0').collect();
    fields
        .chunks(2)
        .filter_map(|record| match record {
            [scope, key] if matches!(*scope, "local" | "worktree") => Some((*key).to_string()),
            _ => None,
        })
        .collect()
}

/// Run a git command that can execute content filters — anything that checks
/// files out (`worktree add`, `restore`) or compares the worktree with the
/// index (`status`, `worktree remove`) — with every discovered filter driver
/// neutralized. Refuses, with an error, when discovery fails.
pub(crate) async fn run_git_neutralized(args: &[&str], cwd: &Path) -> Result<String> {
    run_git_neutralized_with_env(args, cwd, &[]).await
}

/// [`run_git_neutralized`] with extra environment entries, e.g. `LC_ALL=C`
/// for a caller that matches git's error text.
pub(crate) async fn run_git_neutralized_with_env(
    args: &[&str],
    cwd: &Path,
    extra_env: &[(String, String)],
) -> Result<String> {
    let mut env = filter_neutralizing_env(cwd)
        .await
        .with_context(|| format!("git {} not run", args.join(" ")))?;
    env.extend_from_slice(extra_env);
    run_git_inner(args, cwd, &env).await
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
                // `status` runs a repository's clean filter on any file whose
                // stat data changed, so it is a filter-executing command.
                // It must also stay out of submodule checkouts: each reads its
                // own config, which filter discovery here never sees, so a
                // scan there would run any filter a submodule names. `dirty`
                // still reports a submodule that moved to another commit.
                //
                // This scan is a background read of the user's own index.
                // Without `GIT_OPTIONAL_LOCKS=0`, `status` writes its stat
                // refresh back to `.git/index` — computed here with the clean
                // filters neutralized, so a file that differs from HEAD only
                // through its filter would then look unmodified to the user's
                // own `git status`. It also keeps this scan from holding
                // `index.lock` while the user or the agent runs git.
                run_git_neutralized_with_env(
                    &["status", "--porcelain", "--ignore-submodules=dirty"],
                    &scan_key,
                    &[("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string())],
                )
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
/// files, inputs outside the repository, files inside an untracked
/// directory, or uncommitted edits inside a submodule. An equal fingerprint
/// therefore never proves that a check would give the same result, and no
/// caller may skip work because of it.
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
    Some(crate::util::hex::hex_encode(
        &<sha2::Sha256 as sha2::Digest>::finalize(hasher),
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

/// Collect rich git context for system prompt injection.
#[instrument(skip_all)]
pub async fn collect_git_context(cwd: &Path) -> GitContext {
    let repo_root = find_repo_root(cwd);
    let main_repo_root = resolve_main_repo_root(cwd);
    let work_dir = repo_root.as_deref().unwrap_or(cwd);

    // Run all git commands in parallel — they're independent read-only operations.
    let (branch, commit_hash, recent_commits_raw, status) = tokio::join!(
        run_git(&["branch", "--show-current"], work_dir),
        run_git(&["rev-parse", "--short", "HEAD"], work_dir),
        run_git(&["log", "--oneline", "-5", "--no-decorate"], work_dir),
        git_status_porcelain(work_dir),
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
    }
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

/// Fixtures shared by the git and worktree tests.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Run a fixture `git` command in `dir`, isolated from the developer's
    /// global and system config, and assert it succeeds.
    ///
    /// Background maintenance is off: a `commit` otherwise leaves a detached
    /// `git maintenance` behind that takes and drops `objects/maintenance.lock`
    /// a moment later, and a test that compares a repository before and
    /// after would blame the code under test for it.
    pub fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// Arm the repository at `dir` (which must have a commit) with a hostile
    /// content filter on every path: its smudge and clean commands create the
    /// returned sentinel, and `required` turns a missing driver into an error.
    /// The sentinel sits outside the repository so a filter run from any
    /// checkout of it is caught.
    pub fn arm_evil_content_filter(dir: &Path, sentinel_dir: &Path) -> PathBuf {
        // Committed before the filter exists, so arming it runs nothing.
        std::fs::write(dir.join(".gitattributes"), "* filter=evil\n").unwrap();
        git(dir, &["add", ".gitattributes"]);
        git(dir, &["commit", "-qm", "attributes"]);
        configure_evil_filter(dir, sentinel_dir)
    }

    /// [`arm_evil_content_filter`] through the repository's own
    /// `info/attributes` instead of a commit, so the checkout stays clean and
    /// it works in a submodule clone that has no identity. Armed in a
    /// submodule, it lives in config the superproject never reads.
    pub fn arm_evil_local_filter(dir: &Path, sentinel_dir: &Path) -> PathBuf {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "{dir:?} is not a git checkout");
        let git_dir = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info/attributes"), "* filter=evil\n").unwrap();
        configure_evil_filter(dir, sentinel_dir)
    }

    fn configure_evil_filter(dir: &Path, sentinel_dir: &Path) -> PathBuf {
        let sentinel = sentinel_dir.join("PWNED");
        let cmd = format!("touch '{}'; cat", sentinel.display());
        git(dir, &["config", "filter.evil.smudge", &cmd]);
        git(dir, &["config", "filter.evil.clean", &cmd]);
        git(dir, &["config", "filter.evil.required", "true"]);
        sentinel
    }

    /// Move `path`'s mtime forward so git can no longer trust its stat data
    /// and has to run the clean filter to compare content.
    pub fn touch_forward(path: &Path) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
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

    /// Spec: the background scan is read-only. A plain `git status` rewrites
    /// `.git/index` when a tracked file's stat data went stale; this scan must
    /// leave the user's index byte-for-byte alone and still report the change.
    #[tokio::test]
    async fn status_scan_does_not_rewrite_the_users_index() {
        use test_support::git;
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        let file = repo.path().join("tracked.txt");
        std::fs::write(&file, "hello").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        // Same size, different content and an mtime far from the indexed one:
        // the cached stat entry is stale, which is what `status` refreshes.
        std::fs::write(&file, "HELLO").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let index = repo.path().join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let status = git_status_porcelain(repo.path()).await.unwrap();
        assert!(status.contains("tracked.txt"), "got: {status:?}");
        assert_eq!(
            std::fs::read(&index).unwrap(),
            before,
            "the status scan wrote to .git/index"
        );
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

    /// Only the repository's own scopes are refused: an operator's global
    /// `includeIf "gitdir:~/work/"` (the common work-email setup) must not
    /// block every git call.
    #[test]
    fn only_repository_scoped_conditional_includes_are_reported() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            (
                "local\0includeif.onbranch:x.path\0",
                &["includeif.onbranch:x.path"],
            ),
            (
                "worktree\0includeif.gitdir:/w/.path\0",
                &["includeif.gitdir:/w/.path"],
            ),
            ("global\0includeif.gitdir:~/work/.path\0", &[]),
            ("system\0includeif.a.path\0command\0includeif.b.path\0", &[]),
            (
                "global\0includeif.g.path\0local\0includeif.l.path\0",
                &["includeif.l.path"],
            ),
        ];
        for (stdout, want) in cases {
            assert_eq!(repo_conditional_includes(stdout), *want, "for {stdout:?}");
        }
    }

    /// Discovery fails closed: a probe that did not complete, or a repository
    /// with its own conditional include, is an error — never an empty set of
    /// overrides that lets the guarded command run unprotected.
    #[cfg(unix)]
    #[test]
    fn filter_discovery_fails_closed() {
        use std::os::unix::process::ExitStatusExt as _;
        let exited = |code: i32| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: b"fatal: bad config line 3".to_vec(),
            })
        };
        assert_eq!(config_probe_stdout("p", exited(1)).unwrap(), "", "no match");
        assert!(config_probe_stdout("p", exited(128)).is_err());
        assert!(config_probe_stdout("p", Err(std::io::Error::other("no git"))).is_err());

        let ok = |s: &str| Ok(s.to_string());
        let failed = || Err(anyhow::anyhow!("probe failed"));
        assert!(neutralizing_env_from_probes(ok(""), failed()).is_err());
        assert!(neutralizing_env_from_probes(failed(), ok("")).is_err());
        let refused =
            neutralizing_env_from_probes(ok("local\0includeif.onbranch:x.path\0"), ok(""))
                .unwrap_err();
        assert!(
            refused.to_string().contains("includeif.onbranch:x.path"),
            "the refusal must name the include: {refused}"
        );
        let env = neutralizing_env_from_probes(ok(""), ok("filter.evil.clean\0")).unwrap();
        assert!(
            env.contains(&(
                "GIT_CONFIG_KEY_0".to_string(),
                "filter.evil.clean".to_string()
            )),
            "a clean repository still gets its filters neutralized: {env:?}"
        );
    }

    /// A git older than 2.26 rejects `--show-scope`; that must take the
    /// per-scope fallback, not fail closed for every guarded command. Only a
    /// failure of the fallback's local probe refuses, and it names the fix.
    #[cfg(unix)]
    #[test]
    fn include_probe_falls_back_when_show_scope_is_unknown() {
        use std::os::unix::process::ExitStatusExt as _;
        let out = |code: i32, stdout: &str, stderr: &str| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })
        };
        let unknown = "error: unknown option `show-scope'\nusage: git config [<options>]";
        // Expected: Some(Some(stdout)) / Some(None) = fall back / None = error.
        type Want = Option<Option<&'static str>>;
        // (case, probe output, expected)
        let cases: Vec<(&str, std::io::Result<std::process::Output>, Want)> = vec![
            (
                "match",
                out(0, "local\0includeif.a.path\0", ""),
                Some(Some("local\0includeif.a.path\0")),
            ),
            ("no match", out(1, "", ""), Some(Some(""))),
            ("usage exit", out(129, "", unknown), Some(None)),
            (
                "unknown option, other code",
                out(128, "", unknown),
                Some(None),
            ),
            ("bad config", out(128, "", "fatal: bad config line 3"), None),
            ("spawn failure", Err(std::io::Error::other("no git")), None),
        ];
        for (case, output, want) in cases {
            let got = scoped_include_probe_stdout(output);
            match want {
                Some(want) => assert_eq!(got.unwrap().as_deref(), want, "{case}"),
                None => assert!(got.is_err(), "{case}: must fail closed"),
            }
        }

        let both = legacy_include_probe_stdout(
            out(0, "includeif.onbranch:x.path\0", ""),
            out(0, "includeif.gitdir:/w/.path\0", ""),
        )
        .unwrap();
        assert_eq!(
            repo_conditional_includes(&both),
            vec!["includeif.onbranch:x.path", "includeif.gitdir:/w/.path"]
        );
        let worktree_rejected = legacy_include_probe_stdout(
            out(1, "", ""),
            out(
                128,
                "",
                "fatal: --worktree cannot be used with multiple working trees",
            ),
        )
        .unwrap();
        assert_eq!(worktree_rejected, "", "a rejected --worktree is skipped");
        let local_failed =
            legacy_include_probe_stdout(out(128, "", "fatal: bad config line 3"), out(1, "", ""))
                .unwrap_err();
        assert!(
            format!("{local_failed:#}").contains("upgrade git to 2.26+"),
            "{local_failed:#}"
        );
    }

    /// The fallback argv, run against a real git, finds a repository's own
    /// conditional include — including one nested in a file the repository
    /// includes unconditionally, which a scoped probe skips by default.
    #[cfg(unix)]
    #[test]
    fn legacy_include_probe_finds_local_and_nested_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);
        std::fs::write(
            dir.join(".git/nested.cfg"),
            "[includeIf \"onbranch:evil\"]\n\tpath = evil.cfg\n",
        )
        .unwrap();
        test_support::git(dir, &["config", "include.path", "nested.cfg"]);
        let run = |argv: Vec<&'static str>| {
            let mut cmd = std::process::Command::new("git");
            cmd.args(argv).current_dir(dir);
            harden_git_command_sync(&mut cmd);
            cmd.output()
        };
        let records = legacy_include_probe_stdout(
            run(legacy_include_probe_argv("--local")),
            run(legacy_include_probe_argv("--worktree")),
        )
        .unwrap();
        assert!(
            repo_conditional_includes(&records)
                .contains(&"includeif.onbranch:evil.path".to_string()),
            "{records:?}"
        );
    }

    /// `git status` runs the clean filter on a file whose stat data changed,
    /// so the shared status scan must neutralize it like any other
    /// filter-executing command.
    #[cfg(unix)]
    #[tokio::test]
    async fn status_scan_never_runs_a_repo_clean_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo_with_commit(&repo);
        let sentinel = test_support::arm_evil_content_filter(&repo, tmp.path());
        test_support::touch_forward(&repo.join("a.txt"));

        let status = git_status_porcelain(&repo).await;
        assert_eq!(status, Ok(String::new()), "content is unchanged");
        assert!(
            !sentinel.exists(),
            "a repo-configured clean filter ran during git status"
        );
    }

    /// A submodule reads its own config, which the superproject's filter
    /// discovery never sees. The scan must not descend into the submodule's
    /// checkout and run a filter armed there, yet must still notice the
    /// submodule moving to a commit the superproject does not record.
    #[cfg(unix)]
    #[tokio::test]
    async fn status_scan_never_runs_a_submodule_clean_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, sub) = (tmp.path().join("repo"), tmp.path().join("sub"));
        for dir in [&repo, &sub] {
            std::fs::create_dir(dir).unwrap();
            init_repo_with_commit(dir);
        }
        let file_ok = ["-c", "protocol.file.allow=always"];
        let sub_url = sub.to_string_lossy().to_string();
        test_support::git(
            &repo,
            &[&file_ok[..], &["submodule", "add", "-q", &sub_url, "sm"]].concat(),
        );
        test_support::git(&repo, &["commit", "-qm", "add submodule"]);
        let sm = repo.join("sm");
        let sentinel = test_support::arm_evil_local_filter(&sm, tmp.path());
        test_support::touch_forward(&sm.join("a.txt"));

        let status = git_status_porcelain(&repo).await;
        assert_eq!(status, Ok(String::new()), "content is unchanged");
        assert!(
            !sentinel.exists(),
            "a submodule-configured clean filter ran during git status"
        );

        let who = ["-c", "user.email=t@borg.test", "-c", "user.name=borg-test"];
        test_support::git(
            &sm,
            &[&who[..], &["commit", "-q", "--allow-empty", "-m", "moved"]].concat(),
        );
        let status = git_status_porcelain(&repo).await;
        assert_eq!(status, Ok("M sm".to_string()), "the moved submodule shows");
    }

    /// A repository's own `includeIf` can load filters discovery never saw,
    /// so every filter-guarded operation refuses it — and says why.
    #[cfg(unix)]
    #[tokio::test]
    async fn repo_conditional_include_refuses_filter_guarded_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commit(dir);
        test_support::git(dir, &["config", "includeIf.onbranch:evil.path", "evil.cfg"]);

        let status = run_git_neutralized(&["status", "--porcelain"], dir)
            .await
            .unwrap_err();
        assert!(
            format!("{status:#}").contains("conditional includes"),
            "{status:#}"
        );
    }
}
