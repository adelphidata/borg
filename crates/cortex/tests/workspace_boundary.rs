#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Workspace-boundary enforcement for ACP-scoped tool execution.
//!
//! These tests pin the contract that — when an `ExecutionContext` is
//! constructed via `for_workspace`, every workspace-aware tool handler
//! refuses to read, list, patch, or shell-execute outside the workspace
//! root. The plan (`ACP_IMPLEMENTATION_PLAN.md` §"Workspace Boundary
//! Enforcement") names these four surfaces explicitly.

use std::path::PathBuf;

use cortex::config::Config;
use cortex::execution_context::ExecutionContext;
use cortex::policy::ExecutionPolicy;
use cortex::tool_handlers::filesystem::{
    handle_apply_patch_with_context, handle_list_dir_with_context, handle_read_file_with_context,
};
use cortex::tool_handlers::handle_run_shell_with_context;
use cortex::types::ToolOutput;
use tokio::sync::mpsc;

/// Workspace fixture. `outside` is canonicalized so files written there can
/// be referenced both by absolute path (in patches/args) and by the same
/// canonical form the handlers compare against. The workspace root lives on
/// `ctx.workspace_root()` to avoid divergence on platforms where the tempdir
/// lives under a symlinked tree (`/var → /private/var` on macOS).
struct Workspace {
    _tmp: tempfile::TempDir,
    outside: PathBuf,
    ctx: ExecutionContext,
}

fn make_workspace() -> Workspace {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw_root = tmp.path().join("ws");
    let raw_outside = tmp.path().join("outside");
    std::fs::create_dir_all(&raw_root).expect("mkdir ws");
    std::fs::create_dir_all(&raw_outside).expect("mkdir outside");
    let ctx =
        ExecutionContext::for_workspace(raw_root.clone(), raw_root).expect("workspace context");
    Workspace {
        _tmp: tmp,
        outside: raw_outside.canonicalize().expect("canonicalize outside"),
        ctx,
    }
}

fn extract_text(output: ToolOutput) -> String {
    match output {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    }
}

#[test]
fn read_file_rejects_absolute_outside_root() {
    let ws = make_workspace();
    let target = ws.outside.join("secret.txt");
    std::fs::write(&target, "ssh-rsa AAA").expect("write secret");

    let args = serde_json::json!({ "path": target.to_string_lossy() });
    let config = Config::default();
    let result = handle_read_file_with_context(&args, &config, &ws.ctx).expect("handler");
    let text = extract_text(result);
    // Containment failure surfaces as "File not found" via the handler's
    // existing not-found short-circuit (see filesystem::handle_read_file_with_context):
    // a Result error from `resolve_existing_path` is converted into the
    // not-found message. Assert the surface fired AND the secret didn't leak.
    assert!(
        text.contains("File not found"),
        "expected containment-failure surface, got: {text}"
    );
    assert!(
        !text.contains("ssh-rsa"),
        "out-of-workspace read leaked content: {text}"
    );
}

#[test]
fn read_file_rejects_relative_traversal() {
    let ws = make_workspace();
    let target = ws.outside.join("escape.txt");
    std::fs::write(&target, "private").expect("write");

    let args = serde_json::json!({ "path": "../outside/escape.txt" });
    let config = Config::default();
    let text =
        extract_text(handle_read_file_with_context(&args, &config, &ws.ctx).expect("handler"));
    assert!(
        text.contains("File not found"),
        "expected containment-failure surface, got: {text}"
    );
    assert!(
        !text.contains("private"),
        "relative-traversal read leaked content: {text}"
    );
}

#[test]
fn list_dir_rejects_outside_root() {
    let ws = make_workspace();
    std::fs::write(ws.outside.join("a.txt"), "x").expect("write");

    let args = serde_json::json!({ "path": ws.outside.to_string_lossy() });
    let config = Config::default();
    let result = handle_list_dir_with_context(&args, &config, &ws.ctx).expect("list_dir handler");
    // Today the handler maps containment failure to "Not a directory: …".
    // Allow either that surface or an "outside the workspace" surface so a
    // future UX-clarifying refactor doesn't regress this test for the wrong
    // reason. The non-leak invariant is what we actually care about.
    assert!(
        !result.contains("a.txt"),
        "list_dir leaked outside-workspace entries: {result}"
    );
    assert!(
        result.contains("Not a directory") || result.contains("outside the workspace"),
        "expected containment-failure surface, got: {result}"
    );
}

#[test]
fn apply_patch_rejects_relative_traversal_in_patch_text() {
    // Regression guard: containment must fire BEFORE coding::apply_patch_to_dir
    // sees the patch. The check lives in handle_apply_patch_with_context
    // (filesystem.rs) where extract_patch_paths feeds resolve_path_for_write.
    // If a future refactor moves containment downstream of apply_patch_to_dir,
    // this test would weaken into a "didn't write the file" smoke check.
    let ws = make_workspace();
    let patch = "*** Begin Patch\n*** Add File: ../outside/created.txt\n+pwn\n*** End Patch\n";
    let args = serde_json::json!({ "patch": patch });
    let config = Config::default();
    let err = handle_apply_patch_with_context(&args, &config, &ws.ctx)
        .expect_err("traversal in patch must error before any write");
    let msg = err.to_string();
    assert!(
        msg.contains("outside the workspace root"),
        "expected workspace error, got: {msg}"
    );
    assert!(
        !ws.outside.join("created.txt").exists(),
        "patch applied outside workspace despite rejection"
    );
}

#[test]
fn apply_patch_rejects_absolute_outside_root() {
    let ws = make_workspace();
    let abs = ws.outside.join("created.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+pwn\n*** End Patch\n",
        abs.display()
    );
    let args = serde_json::json!({ "patch": patch });
    let config = Config::default();
    let err = handle_apply_patch_with_context(&args, &config, &ws.ctx)
        .expect_err("absolute outside-root in patch must be rejected");
    assert!(
        err.to_string().contains("outside the workspace root"),
        "got: {err}"
    );
    assert!(
        !abs.exists(),
        "patch applied outside workspace despite rejection"
    );
}

#[test]
fn apply_patch_allows_writes_to_new_nested_dirs_inside_root() {
    // Regression: previously parent.canonicalize() failed for non-existent
    // ancestors, blocking legitimate in-workspace writes.
    let ws = make_workspace();
    let patch = "*** Begin Patch\n*** Add File: a/b/c/new.txt\n+hello\n*** End Patch\n";
    let args = serde_json::json!({ "patch": patch });
    let config = Config::default();
    let result =
        handle_apply_patch_with_context(&args, &config, &ws.ctx).expect("in-workspace add");
    assert!(
        result.contains("Patch applied successfully"),
        "got: {result}"
    );
    let written = std::fs::read_to_string(ws.ctx.workspace_root().join("a/b/c/new.txt"))
        .expect("read written");
    assert_eq!(written.trim_end(), "hello");
}

#[tokio::test]
async fn run_shell_rejects_workdir_outside_root() {
    let ws = make_workspace();
    let policy = ExecutionPolicy::default();
    let config = Config::default();
    let (event_tx, _event_rx) = mpsc::channel(8);

    let args = serde_json::json!({
        "command": "echo escaped",
        "workdir": ws.outside.to_string_lossy(),
    });
    let err = handle_run_shell_with_context(
        &args,
        &config,
        &policy,
        &event_tx,
        "call-ws-1",
        None,
        &ws.ctx,
    )
    .await
    .expect_err("out-of-workspace workdir must be rejected before exec");
    let msg = err.to_string();
    assert!(msg.contains("outside the workspace root"), "got: {msg}");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn run_shell_sandbox_blocks_writes_outside_workspace() {
    // M2 regression: an ACP-scoped run_shell must be wrapped in a
    // workspace-bounded sandbox profile (Seatbelt on macOS, bwrap on
    // Linux). Rust-level workdir validation is not enough — the kernel
    // boundary is what stops a shell that already passed validation
    // (cwd inside workspace) from writing to /tmp.
    let ws = make_workspace();
    let policy = ExecutionPolicy::default();
    let config = Config::default();
    let (event_tx, _event_rx) = mpsc::channel(8);

    // Pick a path that is unambiguously outside the workspace root.
    let escape_path = ws.outside.join("escape.txt");
    let escape_path_str = escape_path.to_string_lossy().into_owned();
    let args = serde_json::json!({
        "command": format!("echo pwned > {escape_path_str}"),
    });
    let result = handle_run_shell_with_context(
        &args,
        &config,
        &policy,
        &event_tx,
        "call-ws-sandbox",
        None,
        &ws.ctx,
    )
    .await
    .expect("shell should run; sandbox should reject the write internally");

    assert!(
        !escape_path.exists(),
        "sandbox failed to block out-of-workspace write at {}",
        escape_path.display()
    );
    // We don't pin the exit code or specific error text — different
    // sandbox runtimes (sandbox-exec vs bwrap) surface denials
    // differently — but a successful "Exit code: 0" with no file is the
    // failure mode we explicitly reject.
    if result.contains("Exit code: 0") {
        assert!(
            !escape_path.exists(),
            "shell reported success AND wrote outside the workspace: {result}"
        );
    }
}

#[tokio::test]
async fn run_shell_defaults_cwd_to_workspace() {
    // Without workdir, the shell process must launch under workspace_cwd —
    // not the daemon's process cwd. This is the runtime guarantee the
    // workspace boundary depends on.
    let ws = make_workspace();
    let policy = ExecutionPolicy::default();
    let config = Config::default();
    let (event_tx, _event_rx) = mpsc::channel(8);

    let marker = ws.ctx.workspace_root().join("hello-cwd-marker.txt");
    std::fs::write(&marker, "x").expect("write marker");

    let args = serde_json::json!({ "command": "ls -1 hello-cwd-marker.txt" });
    let result = handle_run_shell_with_context(
        &args,
        &config,
        &policy,
        &event_tx,
        "call-ws-2",
        None,
        &ws.ctx,
    )
    .await
    .expect("shell ran");
    assert!(
        result.contains("hello-cwd-marker.txt") && result.contains("Exit code: 0"),
        "shell did not default to workspace cwd; got: {result}"
    );
}
