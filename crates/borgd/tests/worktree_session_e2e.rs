//! `borg --worktree <name>` is only real if the *agent* ends up in the
//! worktree. The client chdir's its own process, but the agent runs inside the
//! long-lived daemon, so the isolation rests entirely on the workspace context
//! sent at `Session.Open` reaching the tools that touch the filesystem.
//!
//! These tests close that loop: build a session backend with a worktree's
//! execution context and prove that a command run through the daemon lands in
//! the worktree, sees the linked checkout's git state, and leaves the main
//! checkout untouched. If someone drops the context on the way through,
//! the feature silently degrades to "edits the operator's repo", and these
//! fail.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use borgd::session::backend::{AgentBackend, SessionBackend};
use cortex::config::Config;
use cortex::execution_context::ExecutionContext;
use cortex::telemetry::BorgMetrics;

/// Serializes the file: `BORG_DATA_DIR` is process-global, so parallel tests
/// here would race each other's data dir.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// A repo with one commit plus a Borg-managed worktree, mirroring what
/// `borg worktree add` produces.
async fn repo_with_worktree() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    cortex::worktree::Worktree,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("alpha");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@borg.test"]);
    git(&repo, &["config", "user.name", "borg-test"]);
    std::fs::write(repo.join("a.txt"), "main\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "init"]);

    let root = tmp.path().join("worktrees");
    let wt = cortex::worktree::add(
        &repo,
        &root,
        &cortex::worktree::AddOptions {
            name: "fix-login".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("worktree add");
    (tmp, repo, wt)
}

/// Build a backend whose session is scoped to `cwd`, with an isolated data dir.
async fn backend_in(
    cwd: &Path,
) -> (
    std::sync::Arc<AgentBackend>,
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let guard = ENV_LOCK.lock().await;
    let home = tempfile::tempdir().expect("tempdir");
    // SAFETY: serialized by `ENV_LOCK`; read during backend construction.
    unsafe { std::env::set_var("BORG_DATA_DIR", home.path()) };

    let ctx = ExecutionContext::for_workspace(cwd.to_path_buf(), cwd.to_path_buf())
        .expect("workspace context for the worktree");
    let backend = AgentBackend::new(
        Config::default(),
        BorgMetrics::default(),
        None,
        None,
        None,
        Some(ctx),
        None,
    )
    .await
    .expect("backend builds without an LLM provider");
    (backend, home, guard)
}

/// The load-bearing assertion: a command issued through the daemon runs in the
/// worktree, not wherever the daemon process happens to live.
#[tokio::test]
async fn a_worktree_session_runs_commands_inside_the_worktree() {
    let (_tmp, repo, wt) = repo_with_worktree().await;
    let (backend, _home, _guard) = backend_in(&wt.path).await;

    let out = backend
        .exec_shell("pwd", 10_000)
        .await
        .expect("exec_shell succeeds");
    assert!(!out.denied, "got: {:?}", out.output);

    let expect = wt.path.canonicalize().unwrap();
    let expect = expect.to_string_lossy();
    assert!(
        out.output.contains(expect.as_ref()),
        "the session must run in the worktree {expect}, got: {:?}",
        out.output
    );
    assert!(
        !out.output.contains(&format!("{}\n", repo.display())),
        "it must not run in the main checkout, got: {:?}",
        out.output
    );
}

/// The session must see the *linked* checkout's git state, not the main
/// repository's. A linked worktree's `.git` is a file pointing at
/// `.../worktrees/<name>`; the main checkout's is a directory. Reading it from
/// inside the session is therefore proof of which tree the agent is in, and it
/// needs no `git` binary — the sandbox denies executing one.
#[tokio::test]
async fn a_worktree_session_sees_the_linked_checkouts_git_state() {
    let (_tmp, _repo, wt) = repo_with_worktree().await;
    let (backend, _home, _guard) = backend_in(&wt.path).await;

    let out = backend
        .exec_shell("cat .git", 10_000)
        .await
        .expect("exec_shell succeeds");
    assert!(!out.denied, "got: {:?}", out.output);
    assert!(
        out.output.contains("gitdir:") && out.output.contains("worktrees/fix-login"),
        "the session must be in the linked worktree 'fix-login', got: {:?}",
        out.output
    );
}

/// Isolation, stated as the property an operator actually cares about: work
/// done in the session does not appear in the checkout they have open.
#[tokio::test]
async fn writes_in_a_worktree_session_leave_the_main_checkout_clean() {
    let (_tmp, repo, wt) = repo_with_worktree().await;
    let (backend, _home, _guard) = backend_in(&wt.path).await;

    let out = backend
        .exec_shell("printf 'from the agent\\n' > a.txt", 10_000)
        .await
        .expect("exec_shell succeeds");
    assert!(!out.denied, "got: {:?}", out.output);

    assert_eq!(
        std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
        "from the agent\n",
        "the worktree must hold the edit"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "main\n",
        "the main checkout must be untouched — this is the whole feature"
    );
}
