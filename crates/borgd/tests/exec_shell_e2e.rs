//! `!cmd` shell mode runs inside the daemon precisely so it inherits the
//! `run_shell` tool's guards instead of re-implementing them. These tests are
//! the proof of that inheritance: they assert on the *effects* of routing
//! through `handle_run_shell_with_context`, which is what would break if
//! someone "simplified" the backend to spawn a bare `Command`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::session::backend::{AgentBackend, SessionBackend};
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;

/// Serializes the whole file: `BORG_HOME` is process-global, so parallel
/// tests here would race each other's data dir — the same failure shape as the
/// known `cortex session::tests` flake.
/// Async-aware: the guard is deliberately held across the backend's `.await`
/// points, so a `std::sync::Mutex` would be wrong here.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// Isolate `BORG_HOME` so the backend's session writes land in a tempdir.
/// Callers must hold [`env_lock`] for the lifetime of the returned dir.
fn isolated_home() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: serialized by `ENV_LOCK`; read during backend construction.
    unsafe { std::env::set_var("BORG_HOME", tmp.path()) };
    tmp
}

async fn backend() -> (
    std::sync::Arc<AgentBackend>,
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let guard = env_lock().await;
    let tmp = isolated_home();
    let backend = AgentBackend::new(
        Config::default(),
        BorgMetrics::default(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("backend builds without an LLM provider");
    (backend, tmp, guard)
}

#[tokio::test]
async fn exec_shell_runs_an_ordinary_command() {
    let (backend, _tmp, _guard) = backend().await;
    let out = backend
        .exec_shell("printf shell-mode-ok", 10_000)
        .await
        .expect("exec_shell succeeds");
    assert!(!out.denied);
    assert!(
        out.output.contains("shell-mode-ok"),
        "expected command output, got: {:?}",
        out.output
    );
}

#[tokio::test]
async fn exec_shell_denies_a_catastrophic_command() {
    // Same ExecutionPolicy `run_shell` enforces. If this ever passes, shell
    // mode has become a bypass of the deny list.
    let (backend, _tmp, _guard) = backend().await;
    let out = backend
        .exec_shell("rm -rf /", 10_000)
        .await
        .expect("exec_shell returns a refusal, not an error");
    assert!(out.denied, "got: {:?}", out.output);
    assert!(out.output.contains("denied"), "got: {:?}", out.output);
}

#[tokio::test]
async fn exec_shell_denies_a_control_plane_write() {
    let (backend, _tmp, _guard) = backend().await;
    let out = backend
        .exec_shell("echo pwned > ~/.borg/hooks.json", 10_000)
        .await
        .expect("exec_shell returns a refusal, not an error");
    assert!(out.denied, "got: {:?}", out.output);
}

#[tokio::test]
async fn exec_shell_does_not_leak_provider_credentials() {
    // THE test for this item: a bare `std::process::Command` inherits the
    // daemon's environment, including provider keys. Only the `run_shell`
    // spawn path does `env_clear` + the credential allowlist, so a leaked key
    // here means the command did not go through it.
    let leak = "BORG_TEST_EXEC_SHELL_FAKE_KEY";
    let value = "must-not-reach-shell-mode-9f2c";
    // SAFETY: a name unique to this test binary.
    unsafe { std::env::set_var(leak, value) };

    let (backend, _tmp, _guard) = backend().await;
    let out = backend
        .exec_shell("env", 10_000)
        .await
        .expect("exec_shell succeeds");
    assert!(
        !out.output.contains(value),
        "shell mode leaked a parent env secret: {:?}",
        out.output
    );
}

#[tokio::test]
async fn exec_shell_is_refused_when_shell_mode_is_disabled() {
    // The RPC must not be a way around the client-side toggle.
    let _guard = env_lock().await;
    let tmp = isolated_home();
    let mut config = Config::default();
    config.ui.shell_mode_enabled = false;
    let backend = AgentBackend::new(config, BorgMetrics::default(), None, None, None, None, None)
        .await
        .expect("backend builds");

    let out = backend
        .exec_shell("printf nope", 10_000)
        .await
        .expect("returns a refusal");
    assert!(out.denied);
    assert!(out.output.contains("disabled"), "got: {:?}", out.output);
    assert!(
        !out.output.contains("nope"),
        "the command must not have run: {:?}",
        out.output
    );
    drop(tmp);
}
