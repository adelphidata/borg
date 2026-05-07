//! Workspace boundary enforcement at the ACP transport boundary.
//!
//! These tests pin the contract for *what crosses the wire*: cwd validation,
//! canonicalisation, and the [`ExecutionContext`] the daemon receives from
//! `session/new`. Deeper tool-level enforcement (e.g. `read_file` rejecting a
//! path outside workspace_root, `apply_patch` rejecting `..` traversal) is
//! exercised by cortex-level tests against `cortex::tool_handlers::*`; they
//! don't depend on the ACP transport at all and would only duplicate that
//! coverage if we re-ran them through the subprocess.

#![allow(dead_code)]

use super::harness::{initialize_params_with_fs, simple_text_turn, AcpClient, BorgdFixture};
use borgd::session::scripted::ScriptedBackend;
use serde_json::json;

async fn boot_initialized() -> (BorgdFixture, AcpClient) {
    let backend = ScriptedBackend::fixed("workspace-test", simple_text_turn("ok"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn borg acp");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    (fixture, client)
}

#[tokio::test]
async fn session_new_canonicalizes_parent_traversal_in_cwd() {
    // Real failure mode: passing a literal `..` segment through to the
    // daemon would let an IDE escape its declared workspace post-validation
    // (e.g. via `/abs/path/sub/../..`). `validate_workspace_cwd` calls
    // `Path::canonicalize`, which collapses traversal segments before the
    // path ever crosses into the daemon's `ExecutionContext`.
    let (fixture, mut client) = boot_initialized().await;
    let real = fixture
        .workspace_path()
        .canonicalize()
        .expect("canonicalize workspace");
    std::fs::create_dir_all(real.join("sub")).expect("create sub dir");
    let traversed = format!("{}/sub/..", real.display());

    let resp = client
        .request("session/new", json!({"cwd": traversed}))
        .await
        .expect("session/new");
    assert!(
        resp.get("result").is_some(),
        "session/new must succeed for resolvable traversal: {resp}"
    );

    let captured = fixture.factory().captured_contexts();
    let ctx = captured
        .iter()
        .rev()
        .find_map(|c| c.as_ref())
        .expect("at least one ExecutionContext captured");
    assert_eq!(
        ctx.workspace_cwd(),
        real,
        "workspace_cwd must be the canonicalized path, not the literal input"
    );
    assert_eq!(
        ctx.workspace_root(),
        real,
        "workspace_root must equal workspace_cwd at session/new time"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_new_resolves_symlink_cwd_to_real_target() {
    // Real failure mode: an IDE could pass a symlink whose target lives
    // outside the user-visible project root, and downstream workspace
    // boundary checks would compare against the symlink path rather than
    // the real target. Canonicalisation in `validate_workspace_cwd`
    // resolves symlinks; the daemon must see the real target.
    let (fixture, mut client) = boot_initialized().await;
    let real = fixture
        .workspace_path()
        .canonicalize()
        .expect("canonicalize workspace");
    let target_dir = real.join("real_target");
    std::fs::create_dir_all(&target_dir).expect("create symlink target");
    let link_path = real.join("link");
    std::os::unix::fs::symlink(&target_dir, &link_path).expect("create symlink");

    let resp = client
        .request(
            "session/new",
            json!({"cwd": link_path.display().to_string()}),
        )
        .await
        .expect("session/new");
    assert!(resp.get("result").is_some(), "session/new must succeed");

    let captured = fixture.factory().captured_contexts();
    let ctx = captured
        .iter()
        .rev()
        .find_map(|c| c.as_ref())
        .expect("captured ExecutionContext");
    assert_eq!(
        ctx.workspace_cwd(),
        target_dir.canonicalize().expect("canonicalize real target"),
        "workspace_cwd must resolve through the symlink"
    );
}

#[tokio::test]
async fn session_new_plumbs_cwd_as_both_workspace_cwd_and_root() {
    // Real failure mode: a regression that forgot to mirror cwd → root would
    // mean tools see `workspace_root = "/"` (or empty) and skip every
    // workspace-bound check.
    let (fixture, mut client) = boot_initialized().await;
    let real = fixture
        .workspace_path()
        .canonicalize()
        .expect("canonicalize workspace");

    let _ = client
        .request("session/new", json!({"cwd": real.display().to_string()}))
        .await
        .expect("session/new");

    let captured = fixture.factory().captured_contexts();
    let ctx = captured
        .iter()
        .rev()
        .find_map(|c| c.as_ref())
        .expect("captured ExecutionContext");
    assert_eq!(ctx.workspace_cwd(), ctx.workspace_root());
    assert_eq!(ctx.workspace_cwd(), real);
}

#[tokio::test]
async fn session_new_rejects_cwd_with_unresolvable_traversal() {
    // Real failure mode: `Path::canonicalize` errors on traversal that
    // descends through a non-existent intermediate, e.g.
    // `/abs/does_not_exist/..`. The transport must surface that as
    // `invalid_params`, not panic and not silently strip the segments.
    let (_fix, mut client) = boot_initialized().await;
    let real = _fix
        .workspace_path()
        .canonicalize()
        .expect("canonicalize workspace");
    let bad = format!("{}/does_not_exist/..", real.display());
    let resp = client
        .request("session/new", json!({"cwd": bad}))
        .await
        .expect("session/new");
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("expected invalid_params; got: {resp}"));
    assert_eq!(err["code"], json!(-32602));
}

#[tokio::test]
async fn session_new_with_invalid_cwd_does_not_open_factory() {
    // A regression that opened a session BEFORE validating cwd would leak
    // a phantom SessionHost into the registry — subsequent calls might
    // resume it, or it would just sit there holding resources.
    let (fixture, mut client) = boot_initialized().await;
    let before = fixture.factory().captured_contexts().len();

    let resp = client
        .request("session/new", json!({"cwd": "relative/path"}))
        .await
        .expect("session/new");
    assert!(resp.get("error").is_some(), "expected error: {resp}");

    let after = fixture.factory().captured_contexts().len();
    assert_eq!(
        before, after,
        "factory.open() must NOT be invoked for invalid cwd"
    );
}

#[tokio::test]
async fn workspace_dir_remains_independent_of_data_dir() {
    // The fixture deliberately keeps BORG_DATA_DIR (where the daemon stores
    // sessions/settings) separate from the per-test workspace tempdir.
    // A regression that conflated the two — e.g. defaulting workspace_root
    // to the data dir when cwd is missing — would let workspace-bound tools
    // touch internal daemon files. This pins the independence.
    let (fixture, mut client) = boot_initialized().await;
    let real = fixture
        .workspace_path()
        .canonicalize()
        .expect("canonicalize workspace");

    let _ = client
        .request("session/new", json!({"cwd": real.display().to_string()}))
        .await
        .expect("session/new");

    let captured = fixture.factory().captured_contexts();
    let ctx = captured
        .iter()
        .rev()
        .find_map(|c| c.as_ref())
        .expect("captured ExecutionContext");
    assert_eq!(ctx.workspace_cwd(), real);

    let data_dir = std::env::var("BORG_DATA_DIR").expect("BORG_DATA_DIR set by harness");
    let data_dir_canon = std::path::Path::new(&data_dir)
        .canonicalize()
        .expect("canonicalize BORG_DATA_DIR");
    assert_ne!(
        ctx.workspace_cwd(),
        data_dir_canon,
        "workspace_cwd must not coincide with BORG_DATA_DIR"
    );
}
