//! End-to-end coverage for the subprocess process registry.
//!
//! These tests drive the public `run_shell` tool handler against real
//! `sh -c` children and assert the registry tracks them faithfully —
//! every spawn goes in, every exit / timeout comes out, no phantom
//! Running entries linger.
//!
//! No mocks: real PIDs, real signals, real lifecycle.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use std::path::PathBuf;
use std::time::Duration;

use cortex::config::Config;
use cortex::execution_context::ExecutionContext;
use cortex::policy::ExecutionPolicy;
use cortex::process_registry::{ProcessRegistry, ProcessStatus};
use cortex::tool_handlers::handle_run_shell_with_context;
use tokio::sync::mpsc;

fn workspace_cwd() -> PathBuf {
    std::env::current_dir().expect("cwd")
}

fn ctx_with_registry(reg: &ProcessRegistry) -> ExecutionContext {
    ExecutionContext::unrestricted(workspace_cwd())
        .with_process_registry(Some(reg.clone()))
        .with_session_id(Some("test-session".to_string()))
}

/// Spec proof: invoking `run_shell` with a short command MUST produce
/// exactly one registry entry that transitions Running → Exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_round_trips_registry() {
    let reg = ProcessRegistry::new();
    let ctx = ctx_with_registry(&reg);
    let (tx, _rx) = mpsc::channel(64);

    let args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 5000,
    });
    let output = handle_run_shell_with_context(
        &args,
        &Config::default(),
        &ExecutionPolicy::default(),
        &tx,
        "call-1",
        None,
        &ctx,
    )
    .await
    .expect("run_shell ok");

    assert!(output.contains("hello"), "stdout captured: {output}");
    assert!(
        reg.running().is_empty(),
        "registry leaked Running entries: {:?}",
        reg.running()
    );
    let snap = reg.snapshot();
    let entry = snap
        .finished
        .iter()
        .find(|e| e.command == "echo hello")
        .expect("finished entry for echo hello");
    assert_eq!(entry.status, ProcessStatus::Exited);
    assert_eq!(entry.exit_code, Some(0));
    assert_eq!(entry.session_id.as_deref(), Some("test-session"));
}

/// A command that exceeds its `timeout_ms` MUST mark the entry
/// `TimedOut`, distinct from `Killed` — the doctor surface formats
/// these differently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_timeout_marks_timed_out_not_killed() {
    let reg = ProcessRegistry::new();
    let ctx = ctx_with_registry(&reg);
    let (tx, _rx) = mpsc::channel(64);

    let args = serde_json::json!({
        "command": "sleep 5",
        "timeout_ms": 200,
    });
    // Timeout produces an Ok("Error: ...") result, not an Err — the
    // handler surfaces the message to the model rather than failing
    // the tool call.
    let output = handle_run_shell_with_context(
        &args,
        &Config::default(),
        &ExecutionPolicy::default(),
        &tx,
        "call-2",
        None,
        &ctx,
    )
    .await
    .expect("handler returns Ok with timeout message");
    assert!(output.contains("timed out"), "output: {output}");

    // The registry should have observed the timeout. Give kill_on_drop
    // a beat to finalize.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snap = reg.snapshot();
    let entry = snap
        .finished
        .iter()
        .find(|e| e.command == "sleep 5")
        .expect("finished entry for sleep 5");
    assert_eq!(
        entry.status,
        ProcessStatus::TimedOut,
        "must mark TimedOut, got {:?}",
        entry.status
    );
}

/// Running two shells concurrently — the registry MUST track both
/// independently, then drain to two finished entries with no
/// cross-contamination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_shells_are_tracked_independently() {
    let reg = ProcessRegistry::new();
    let ctx = ctx_with_registry(&reg);
    let (tx, _rx) = mpsc::channel(64);

    let cfg = Config::default();
    let policy = ExecutionPolicy::default();
    let args1 = serde_json::json!({"command": "echo one", "timeout_ms": 5000});
    let args2 = serde_json::json!({"command": "echo two", "timeout_ms": 5000});
    let f1 = handle_run_shell_with_context(&args1, &cfg, &policy, &tx, "call-a", None, &ctx);
    let f2 = handle_run_shell_with_context(&args2, &cfg, &policy, &tx, "call-b", None, &ctx);
    let (o1, o2) = tokio::join!(f1, f2);
    o1.expect("shell one ok");
    o2.expect("shell two ok");

    assert!(reg.running().is_empty(), "no leaked running entries");
    let snap = reg.snapshot();
    let count = snap
        .finished
        .iter()
        .filter(|e| e.command == "echo one" || e.command == "echo two")
        .count();
    assert_eq!(count, 2, "both shells finalized");
}

/// Registry-less context (the default `from_process` path) MUST still
/// execute commands successfully — the registry is purely additive
/// observability and must never gate functionality.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_works_without_registry() {
    let ctx = ExecutionContext::unrestricted(workspace_cwd());
    let (tx, _rx) = mpsc::channel(64);

    let args = serde_json::json!({
        "command": "echo no-registry",
        "timeout_ms": 5000,
    });
    let output = handle_run_shell_with_context(
        &args,
        &Config::default(),
        &ExecutionPolicy::default(),
        &tx,
        "call-3",
        None,
        &ctx,
    )
    .await
    .expect("registry-less shell still runs");
    assert!(output.contains("no-registry"));
}
