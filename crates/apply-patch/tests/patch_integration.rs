//! Apply-patch integration tests.
//!
//! Tests complex multi-file patches, path traversal prevention, nested
//! directory creation, and edge cases in the patch DSL.

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

use borg_apply_patch::apply_patch_to_dir;

// ── Test: multi-file add in single patch ──

#[test]
fn multi_file_add_single_patch() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: src/main.rs
+fn main() {
+    println!(\"hello\");
+}
*** Add File: src/lib.rs
+pub fn add(a: i32, b: i32) -> i32 {
+    a + b
+}
*** Add File: README.md
+# My Project
+A simple project.
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("multi-file add");
    assert_eq!(affected.added.len(), 3);
    assert!(dir.join("src/main.rs").exists());
    assert!(dir.join("src/lib.rs").exists());
    assert!(dir.join("README.md").exists());

    let main = fs::read_to_string(dir.join("src/main.rs")).expect("read main");
    assert!(main.contains("println!"));
}

// ── Test: nested directory creation ──

#[test]
fn nested_directory_creation() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: a/b/c/d/deep.txt
+deeply nested content
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("nested add");
    let content = fs::read_to_string(dir.join("a/b/c/d/deep.txt")).expect("read");
    assert!(content.contains("deeply nested"));
}

// ── Test: path traversal rejected ──

#[test]
fn path_traversal_rejected() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: ../../etc/evil.txt
+evil content
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Path traversal should be rejected");
}

// ── Test: absolute path rejected ──

#[test]
fn absolute_path_rejected() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: /etc/passwd
+evil
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Absolute paths should be rejected");
}

// ── Test: update nonexistent file errors ──

#[test]
fn update_nonexistent_file_errors() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Update File: does_not_exist.txt
@@
 old line
-old line
+new line
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Updating nonexistent file should error");
}

// ── Test: add then update in sequence ──

#[test]
fn add_then_update_sequence() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // First add
    let add_patch = "\
*** Begin Patch
*** Add File: config.yaml
+key: value
+debug: false
*** End Patch";
    apply_patch_to_dir(add_patch, dir).expect("add");

    // Then update
    let update_patch = "\
*** Begin Patch
*** Update File: config.yaml
@@
 key: value
-debug: false
+debug: true
*** End Patch";
    let affected = apply_patch_to_dir(update_patch, dir).expect("update");
    assert!(affected.modified.contains(&"config.yaml".to_string()));

    let content = fs::read_to_string(dir.join("config.yaml")).expect("read");
    assert!(content.contains("debug: true"));
    assert!(!content.contains("debug: false"));
}

// ── Test: delete nonexistent file succeeds ──

#[test]
fn delete_nonexistent_succeeds() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Delete File: ghost.txt
*** End Patch";

    // Delete of nonexistent should succeed (idempotent)
    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_ok(), "Delete nonexistent should be idempotent");
}

// ── Test: affected paths summary ──

#[test]
fn affected_paths_summary() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // Create a file to modify and one to delete
    fs::write(dir.join("modify.txt"), "original\n").expect("write");
    fs::write(dir.join("remove.txt"), "temporary\n").expect("write");

    let patch = "\
*** Begin Patch
*** Add File: new.txt
+new file
*** Update File: modify.txt
@@
-original
+modified
*** Delete File: remove.txt
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("mixed operations");
    let summary = affected.format_summary();

    assert!(
        summary.contains("new.txt"),
        "Summary should list added file"
    );
    assert!(
        summary.contains("modify.txt"),
        "Summary should list modified file"
    );
    assert!(
        summary.contains("remove.txt"),
        "Summary should list deleted file"
    );
}

// ── Test: empty patch returns error ──

#[test]
fn empty_patch_returns_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(
        result.is_err(),
        "Empty patch with no operations should error"
    );
}

// ── Test: file with spaces in name ──

#[test]
fn spaces_in_filename() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: my file (1).txt
+content with spaces in name
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("spaces in filename should work");
    assert!(dir.join("my file (1).txt").exists());
    assert!(affected.added.contains(&"my file (1).txt".to_string()));
}

// ── Test: multi-hunk update ──

#[test]
fn multi_hunk_update() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(
        dir.join("multi.txt"),
        "line 1\nline 2\nline 3\nline 4\nline 5\n",
    )
    .expect("write");

    let patch = "\
*** Begin Patch
*** Update File: multi.txt
@@
-line 1
+LINE ONE
@@
-line 5
+LINE FIVE
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("multi-hunk");
    let content = fs::read_to_string(dir.join("multi.txt")).expect("read");
    assert!(content.contains("LINE ONE"));
    assert!(content.contains("LINE FIVE"));
    assert!(content.contains("line 3")); // untouched middle
}
