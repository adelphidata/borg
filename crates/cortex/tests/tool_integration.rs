//! Tool handler integration tests.
//!
//! Tests tool handlers with real filesystem (temp dirs) and in-memory DB.
//! Verifies the tool execution pipeline without any LLM calls.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use std::fs;

use cortex::config::Config;
use cortex::tool_handlers;
use cortex::types::ToolOutput;

mod common;

// ── Test: apply_patch creates and modifies files ──

#[test]
fn apply_patch_creates_and_modifies() {
    let tmp = common::test_tempdir();
    let dir = tmp.path();

    // Create a file via patch using the unified handler with cwd target
    // We need to set CWD for this handler; use apply_patch_to_dir directly instead
    let patch_create =
        "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n+Line two\n*** End Patch";
    let affected = coding::apply_patch_to_dir(patch_create, dir).expect("create patch");
    assert!(affected.format_summary().contains("hello.txt"));

    // Verify file exists
    let content = fs::read_to_string(dir.join("hello.txt")).expect("read created file");
    assert!(content.contains("Hello world") && content.contains("Line two"));

    // Modify the file via patch
    let patch_update = "*** Begin Patch\n*** Update File: hello.txt\n@@\n Hello world\n-Line two\n+Line two updated\n*** End Patch";
    coding::apply_patch_to_dir(patch_update, dir).expect("update patch");

    let content = fs::read_to_string(dir.join("hello.txt")).expect("read modified file");
    assert!(content.contains("Line two updated"));
}

// ── Test: apply_patch with delete ──

#[test]
fn apply_patch_delete_file() {
    let tmp = common::test_tempdir();
    let dir = tmp.path();

    // Create a file first
    fs::write(dir.join("to_delete.txt"), "temporary\n").expect("write file");
    assert!(dir.join("to_delete.txt").exists());

    // Delete via patch
    let patch = "*** Begin Patch\n*** Delete File: to_delete.txt\n*** End Patch";
    coding::apply_patch_to_dir(patch, dir).expect("delete patch");
    assert!(
        !dir.join("to_delete.txt").exists(),
        "File should be deleted"
    );
}

// ── Test: apply_patch moves a file via `*** Move to:` ──

#[test]
fn apply_patch_moves_file() {
    let tmp = common::test_tempdir();
    let dir = tmp.path();
    fs::write(dir.join("old.rs"), "old\n").expect("seed");

    let patch = "*** Begin Patch\n*** Update File: old.rs\n*** Move to: new.rs\n@@\n-old\n+new\n*** End Patch";
    let affected = coding::apply_patch_to_dir(patch, dir).expect("move patch");

    assert!(!dir.join("old.rs").exists(), "source should be removed");
    let dst = fs::read_to_string(dir.join("new.rs")).expect("read destination");
    assert!(dst.contains("new"));
    assert!(
        affected
            .moved
            .iter()
            .any(|(from, to)| from == "old.rs" && to == "new.rs"),
        "moved vec missing entry: {:?}",
        affected.moved
    );
}

// ── Test: apply_patch rejects malformed input without touching disk ──

#[test]
fn apply_patch_rejects_malformed() {
    let tmp = common::test_tempdir();
    let dir = tmp.path();
    fs::write(dir.join("keep.txt"), "keep\n").expect("seed");

    let cases: &[(&str, &str)] = &[
        ("garbage", "this is not a valid patch"),
        (
            "empty Add path",
            "*** Begin Patch\n*** Add File: \n+x\n*** End Patch",
        ),
        (
            "update nonexistent file",
            "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
        ),
        (
            // Common-mistake case from CLAUDE.md: an UpdateFile body whose
            // lines all lack the ' '/'+'/'-' prefix and which is missing the
            // `@@` hunk header. The parser sees no hunks and rejects.
            "missing hunk prefix",
            "*** Begin Patch\n*** Update File: keep.txt\nkeep\n*** End Patch",
        ),
    ];

    let snapshot_before = fs::read_to_string(dir.join("keep.txt")).unwrap();
    for (name, patch) in cases {
        let result = coding::apply_patch_to_dir(patch, dir);
        assert!(result.is_err(), "case '{name}' should fail, got {result:?}");
    }
    let snapshot_after = fs::read_to_string(dir.join("keep.txt")).unwrap();
    assert_eq!(
        snapshot_before, snapshot_after,
        "malformed patches must not mutate the workspace"
    );
}

// ── Test: apply_patch refuses path traversal ──

#[test]
fn apply_patch_rejects_path_traversal() {
    let tmp = common::test_tempdir();
    let dir = tmp.path();

    let patch = "*** Begin Patch\n*** Add File: ../escape.txt\n+pwned\n*** End Patch";
    let result = coding::apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "path traversal must be rejected");

    // The escape file must not exist outside the tempdir.
    let escaped = dir.parent().expect("parent").join("escape.txt");
    assert!(!escaped.exists(), "patch escaped tempdir to {escaped:?}");
}

// ── Test: blocked-path predicate covers the full default set + decoys ──

#[test]
fn blocked_path_rejects_full_default_set() {
    // Each tuple: (blocked component as it appears in the default list,
    //              a leaf filename to put under it,
    //              a sibling decoy that must NOT be blocked).
    let cases: &[(&str, &str, &str)] = &[
        (".ssh", "id_rsa", ".ssh_backup"),
        (".aws", "credentials", ".aws_archive"),
        (".gnupg", "secring.gpg", ".gnupg_old"),
        (".config/gh", "hosts.yml", ".config/gh_legacy"),
        (".env", "values", ".env_example"),
        ("credentials", "token", "credentials_template"),
        ("private_key", "rsa", "private_key_template"),
    ];

    let blocked = Config::default().security.blocked_paths;
    for (entry, leaf, decoy) in cases {
        let blocked_path = std::path::Path::new("/tmp").join(entry).join(leaf);
        assert!(
            tool_handlers::is_blocked_path(&blocked_path, &blocked, &[]),
            "{entry}: expected {blocked_path:?} to be blocked",
        );

        let decoy_path = std::path::Path::new("/tmp").join(decoy).join("file");
        assert!(
            !tool_handlers::is_blocked_path(&decoy_path, &blocked, &[]),
            "{entry}: decoy {decoy_path:?} must not match blocked component",
        );
    }
}

// ── Test: handler layer (read_file + list_dir) refuses blocked paths ──

#[test]
fn read_file_and_list_dir_refuse_blocked_paths() {
    let tmp = common::test_tempdir();
    let root = tmp.path();

    // Materialize each blocked-component layout under the tempdir so the
    // handlers' canonicalize step succeeds and the blocked-path check is
    // the thing that fires (not a "File not found").
    let layouts: &[(&str, &str)] = &[
        (".ssh", "id_rsa"),
        (".aws", "credentials"),
        (".gnupg", "secring.gpg"),
        (".config/gh", "hosts.yml"),
        (".env", "values"),
        ("credentials", "token"),
        ("private_key", "rsa"),
    ];

    let config = Config::default();
    for (entry, leaf) in layouts {
        let dir = root.join(entry);
        fs::create_dir_all(&dir).expect("mkdir blocked layout");
        let file = dir.join(leaf);
        fs::write(&file, "secret").expect("write blocked file");

        // read_file: must report Access denied (string-matched), never the
        // file's contents.
        let read_args = serde_json::json!({"path": file.to_str().unwrap()});
        let read_out = tool_handlers::handle_read_file(&read_args, &config)
            .expect("handle_read_file must not panic");
        let text = match &read_out {
            ToolOutput::Text(t) => t.clone(),
            ToolOutput::Multimodal { text, .. } => text.clone(),
        };
        assert!(
            text.contains("Access denied"),
            "{entry}: read_file should deny, got: {text}"
        );
        assert!(
            !text.contains("secret"),
            "{entry}: read_file leaked file contents: {text}"
        );

        // list_dir: pointing at the blocked dir itself must also deny.
        let list_args = serde_json::json!({"path": dir.to_str().unwrap()});
        let list_out = tool_handlers::handle_list_dir(&list_args, &config)
            .expect("handle_list_dir must not panic");
        assert!(
            list_out.contains("Access denied"),
            "{entry}: list_dir should deny, got: {list_out}"
        );
    }
}

// ── Test: read_file returns raw content (no line-number gutter) ──

#[test]
fn read_file_returns_raw_content() {
    let tmp = common::test_tempdir();
    let file = tmp.path().join("test.txt");
    fs::write(&file, "line one\nline two\nline three\n").expect("write file");

    let config = Config::default();
    let args = serde_json::json!({
        "path": file.to_str().unwrap()
    });
    let output = tool_handlers::handle_read_file(&args, &config).expect("read_file");
    let text = match &output {
        ToolOutput::Text(t) => t.as_str(),
        ToolOutput::Multimodal { text, .. } => text.as_str(),
    };

    // Raw, byte-for-byte: no line-number gutter, no tab separator. Dropping the
    // gutter is the input-token saving; edits anchor on content, not line nums.
    assert_eq!(text, "line one\nline two\nline three\n", "got: {text}");
    assert!(!text.contains('\t'), "must not emit a tab gutter: {text}");
}

// ── Test: list_dir not a directory ──

#[test]
fn list_dir_not_a_directory() {
    let tmp = common::test_tempdir();
    let file = tmp.path().join("just_a_file.txt");
    fs::write(&file, "content").expect("write file");

    let config = Config::default();
    let args = serde_json::json!({
        "path": file.to_str().unwrap()
    });
    let result = tool_handlers::handle_list_dir(&args, &config).expect("list_dir on file");
    assert!(
        result.contains("Not a directory"),
        "Expected error, got: {result}"
    );
}
