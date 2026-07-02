//! Integration tests for the `[security] blocked_paths` invariant.
//!
//! Spec (CLAUDE.md → "Security — Blocked Paths"):
//!   `[security] blocked_paths` defaults: `.ssh`, `.aws`, `.gnupg`,
//!   `.config/gh`, `.env`, `credentials`, `private_key`. Filtered from
//!   `fs_read`/`fs_write` and checked in `list_dir`.
//!
//! These tests exercise the production handlers (`handle_read_file_with_context`,
//! `handle_apply_patch_with_context`, `handle_list_dir_with_context`) against
//! real on-disk files containing default-blocked names and assert the spec
//! holds: contents are not returned, writes are not performed, and entries
//! appear as `[blocked]` in directory listings.
//!
//! ── Verification (mutation check) ──
//!
//! To confirm these tests are not vacuously passing, temporarily comment
//! out the `is_blocked_path` call in
//! `crates/cortex/src/tool_handlers/filesystem.rs` (e.g. lines ~89, ~267,
//! ~714, ~937). Each of the assertions below will fail:
//!
//!   - `read_file` denial → returns the secret content.
//!   - `apply_patch` denial → secret file appears on disk.
//!   - `list_dir` `[blocked]` marker → entry shows the file leaf instead.
//!
//! Restore the production code and re-run; tests pass again.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use cortex::config::Config;
use cortex::execution_context::ExecutionContext;
use cortex::tool_handlers::filesystem::{
    handle_apply_patch_with_context, handle_list_dir_with_context, handle_read_file_with_context,
};
use cortex::types::ToolOutput;

fn workspace_ctx(root: &Path) -> ExecutionContext {
    ExecutionContext::for_workspace(root.to_path_buf(), root.to_path_buf())
        .expect("build workspace ctx for tempdir")
}

fn read_file_text(args: serde_json::Value, cfg: &Config, ctx: &ExecutionContext) -> String {
    match handle_read_file_with_context(&args, cfg, ctx).expect("read_file handler returned Err") {
        ToolOutput::Text(s) => s,
        other => panic!("expected text output, got {other:?}"),
    }
}

fn assert_default_blocked_set(cfg: &Config) {
    // Anchor the test to the documented spec: if the defaults shift, this
    // assertion forces a deliberate update rather than silently invalidating
    // the rest of the suite.
    let expected = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/gh",
        ".env",
        "credentials",
        "private_key",
    ];
    for entry in expected {
        assert!(
            cfg.security.blocked_paths.iter().any(|p| p == entry),
            "default blocked_paths missing `{entry}`; suite presumes CLAUDE.md spec defaults"
        );
    }
}

// ── fs_read: default-blocked names refuse to return contents ──

#[test]
fn fs_read_denies_dot_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    assert_default_blocked_set(&cfg);
    let secret = "BORG_API_KEY=should-not-leak\n";
    fs::write(tmp.path().join(".env"), secret).expect("seed .env");

    let ctx = workspace_ctx(tmp.path());
    let out = read_file_text(serde_json::json!({ "path": ".env" }), &cfg, &ctx);

    assert!(
        out.contains("Access denied"),
        "expected denial message, got: {out}"
    );
    assert!(
        !out.contains("should-not-leak"),
        "secret leaked into read_file output: {out}"
    );
}

#[test]
fn fs_read_denies_ssh_private_key() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let secret = "-----BEGIN OPENSSH PRIVATE KEY-----\nshould-not-leak\n";
    fs::create_dir(tmp.path().join(".ssh")).expect("mkdir .ssh");
    fs::write(tmp.path().join(".ssh/id_rsa"), secret).expect("seed id_rsa");

    let ctx = workspace_ctx(tmp.path());
    let out = read_file_text(serde_json::json!({ "path": ".ssh/id_rsa" }), &cfg, &ctx);

    assert!(out.contains("Access denied"), "expected denial: {out}");
    assert!(!out.contains("should-not-leak"), "secret leaked: {out}");
}

#[test]
fn fs_read_denies_credentials_file_under_aws() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let secret = "[default]\naws_secret_access_key = should-not-leak\n";
    fs::create_dir(tmp.path().join(".aws")).expect("mkdir .aws");
    fs::write(tmp.path().join(".aws/credentials"), secret).expect("seed creds");

    let ctx = workspace_ctx(tmp.path());
    let out = read_file_text(
        serde_json::json!({ "path": ".aws/credentials" }),
        &cfg,
        &ctx,
    );

    // Two blocked components in the path (`.aws` and `credentials`); either
    // one being honored is sufficient for spec compliance.
    assert!(out.contains("Access denied"), "expected denial: {out}");
    assert!(!out.contains("should-not-leak"), "secret leaked: {out}");
}

// ── fs_read: control case proves the deny isn't blanket ──

#[test]
fn fs_read_allows_unblocked_neighbor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    fs::write(tmp.path().join(".env"), "secret\n").expect("seed");
    fs::write(tmp.path().join("notes.txt"), "this is fine\n").expect("seed notes");

    let ctx = workspace_ctx(tmp.path());
    let out = read_file_text(serde_json::json!({ "path": "notes.txt" }), &cfg, &ctx);

    assert!(
        out.contains("this is fine"),
        "unblocked neighbor must be readable; got: {out}"
    );
    assert!(
        !out.contains("Access denied"),
        "unblocked neighbor wrongly denied: {out}"
    );
}

// ── fs_write (apply_patch): blocked path rejects write and leaves disk untouched ──

#[test]
fn apply_patch_refuses_write_under_aws() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let ctx = workspace_ctx(tmp.path());

    // Patch attempts to add `.aws/credentials` containing a fresh secret.
    let patch = "*** Begin Patch\n\
                 *** Add File: .aws/credentials\n\
                 +[default]\n\
                 +aws_secret_access_key = injected-by-test\n\
                 *** End Patch";
    let result =
        handle_apply_patch_with_context(&serde_json::json!({ "patch": patch }), &cfg, &ctx)
            .expect("handler must return Ok for blocked rejection");

    assert!(
        result.contains("Access denied"),
        "expected blocked-path denial, got: {result}"
    );

    // Spec: write must not have happened. The file must not exist on disk.
    let target = tmp.path().join(".aws/credentials");
    assert!(
        !target.exists(),
        "blocked apply_patch must not create the file; found at {}",
        target.display()
    );
}

#[test]
fn apply_patch_refuses_write_to_dot_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let ctx = workspace_ctx(tmp.path());

    let patch = "*** Begin Patch\n\
                 *** Add File: .env\n\
                 +SECRET=injected-by-test\n\
                 *** End Patch";
    let result =
        handle_apply_patch_with_context(&serde_json::json!({ "patch": patch }), &cfg, &ctx)
            .expect("handler must return Ok for blocked rejection");

    assert!(
        result.contains("Access denied"),
        "expected blocked-path denial, got: {result}"
    );
    assert!(
        !tmp.path().join(".env").exists(),
        ".env must not be created when the patch is denied"
    );
}

#[test]
fn apply_patch_refuses_write_to_private_key_named_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let ctx = workspace_ctx(tmp.path());

    // Default blocked entry `private_key` matches a path component literally
    // equal to `private_key`, per is_blocked_path's component-window match.
    let patch = "*** Begin Patch\n\
                 *** Add File: private_key\n\
                 +-----BEGIN PRIVATE KEY-----\n\
                 +injected-by-test\n\
                 *** End Patch";
    let result =
        handle_apply_patch_with_context(&serde_json::json!({ "patch": patch }), &cfg, &ctx)
            .expect("handler must return Ok for blocked rejection");

    assert!(
        result.contains("Access denied"),
        "expected blocked-path denial, got: {result}"
    );
    assert!(
        !tmp.path().join("private_key").exists(),
        "private_key must not be created when the patch is denied"
    );
}

#[test]
#[cfg(unix)]
fn apply_patch_refuses_write_through_symlinked_parent_into_blocked_dir() {
    // Regression (security): an add-file whose parent directory is a symlink
    // into a blocked directory OUTSIDE the workspace must be refused. Uses the
    // unrestricted context (production `from_process` behavior — no workspace
    // jail) so the blocked-path guard, not workspace containment, is the only
    // thing standing between the write and `.ssh`. The leaf does not exist yet,
    // which is exactly where the old `canonicalize`-then-literal fallback left
    // the symlinked parent unresolved and let the write through to
    // `.ssh/authorized_keys`.
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    assert_default_blocked_set(&cfg);

    // A `.ssh` dir standing in for the user's real home key store, sitting
    // OUTSIDE the project workspace.
    let ssh_dir = tmp.path().join(".ssh");
    fs::create_dir(&ssh_dir).expect("mkdir .ssh");

    // The project the agent is working in, with an innocuous-looking symlink
    // that actually points at the key store.
    let project = tmp.path().join("project");
    fs::create_dir(&project).expect("mkdir project");
    symlink(&ssh_dir, project.join("docs")).expect("symlink project/docs -> .ssh");

    let ctx = ExecutionContext::unrestricted(project.clone());
    let patch = "*** Begin Patch\n\
                 *** Add File: docs/authorized_keys\n\
                 +ssh-ed25519 AAAAinjected-by-test attacker\n\
                 *** End Patch";
    let result =
        handle_apply_patch_with_context(&serde_json::json!({ "patch": patch }), &cfg, &ctx)
            .expect("handler must return Ok for blocked rejection");

    assert!(
        result.contains("Access denied"),
        "expected blocked-path denial for symlinked-parent write, got: {result}"
    );
    assert!(
        !ssh_dir.join("authorized_keys").exists(),
        "write must not land in the blocked .ssh dir via a symlinked parent"
    );
}

// ── list_dir: blocked entries surface as `[blocked]` markers, not raw leaves ──

#[test]
fn list_dir_marks_blocked_named_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    fs::write(tmp.path().join("private_key"), "should-not-list\n").expect("seed pk");
    fs::write(tmp.path().join("credentials"), "should-not-list\n").expect("seed cred");
    fs::write(tmp.path().join("notes.txt"), "ok\n").expect("seed notes");

    let ctx = workspace_ctx(tmp.path());
    let out = handle_list_dir_with_context(&serde_json::json!({ "path": "." }), &cfg, &ctx)
        .expect("list_dir Ok");

    assert!(
        out.contains("[blocked] private_key"),
        "missing blocked marker for private_key in:\n{out}"
    );
    assert!(
        out.contains("[blocked] credentials"),
        "missing blocked marker for credentials in:\n{out}"
    );
    assert!(
        out.contains("notes.txt"),
        "neighbor file must still be listed:\n{out}"
    );
    // The blocked marker is the only legitimate way these names should
    // appear — the raw `[file]` line would be a leak.
    assert!(
        !out.contains("[file] private_key"),
        "private_key listed as plain file (not blocked):\n{out}"
    );
    assert!(
        !out.contains("[file] credentials"),
        "credentials listed as plain file (not blocked):\n{out}"
    );
}

#[test]
fn list_dir_marks_blocked_dot_env_when_hidden_visible() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    fs::write(tmp.path().join(".env"), "SECRET=x\n").expect("seed");
    fs::write(tmp.path().join("visible.txt"), "ok\n").expect("seed");

    let ctx = workspace_ctx(tmp.path());
    let out = handle_list_dir_with_context(
        // include_hidden=true forces the walker past the dotfile-skip path so
        // the blocked-path branch is the only thing standing between `.env`
        // and the listing. With include_hidden=false, dot-prefixed entries
        // are filtered as hidden — that path doesn't prove the blocked-path
        // branch fires.
        &serde_json::json!({ "path": ".", "include_hidden": true }),
        &cfg,
        &ctx,
    )
    .expect("list_dir Ok");

    assert!(
        out.contains("[blocked] .env"),
        "missing [blocked] .env in:\n{out}"
    );
    assert!(out.contains("visible.txt"), "neighbor missing:\n{out}");
    assert!(
        !out.contains("[file] .env"),
        ".env listed as plain file (not blocked):\n{out}"
    );
}

#[test]
fn list_dir_blocks_descent_into_ssh_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    fs::create_dir(tmp.path().join(".ssh")).expect("mkdir");
    fs::write(tmp.path().join(".ssh/id_rsa"), "should-not-list\n").expect("seed");
    fs::write(tmp.path().join("readme.md"), "ok\n").expect("seed");

    let ctx = workspace_ctx(tmp.path());
    let out = handle_list_dir_with_context(
        &serde_json::json!({ "path": ".", "include_hidden": true, "depth": 3 }),
        &cfg,
        &ctx,
    )
    .expect("list_dir Ok");

    assert!(
        out.contains("[blocked] .ssh"),
        "missing [blocked] .ssh in:\n{out}"
    );
    // id_rsa under the blocked dir must never surface — recursion stops at
    // the [blocked] marker.
    assert!(
        !out.contains("id_rsa"),
        "id_rsa leaked into recursive listing:\n{out}"
    );
}
