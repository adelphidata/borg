//! Patch DSL parser and filesystem applicator.
//!
//! Provides a simple text-based patch format for creating, updating, and deleting files.
//! Used by the `apply_patch` tool to modify files in the workspace.
#![warn(missing_docs)]
#![cfg_attr(
    test,
    allow(
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
    )
)]

pub mod apply;
pub mod parser;
mod seek_sequence;

use anyhow::Result;
use std::path::Path;

pub use apply::AffectedPaths;

/// Parse and apply a patch to a directory, returning which files were affected.
pub fn apply_patch_to_dir(patch_text: &str, base_dir: &Path) -> Result<AffectedPaths> {
    let patch = parser::parse_patch(patch_text)?;
    apply::apply_patch(&patch, base_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_patch_to_dir_add_file() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
*** Begin Patch
*** Add File: hello.txt
+Hello, world!
+Line two.
*** End Patch";
        let affected = apply_patch_to_dir(patch, dir.path()).unwrap();
        assert!(affected.added.contains(&"hello.txt".to_string()));
        let content = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
        assert_eq!(content, "Hello, world!\nLine two.");
    }

    #[test]
    fn apply_patch_to_dir_add_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+file a
*** Add File: b.txt
+file b
*** End Patch";
        let affected = apply_patch_to_dir(patch, dir.path()).unwrap();
        assert_eq!(affected.added.len(), 2);
        assert!(dir.path().join("a.txt").exists());
        assert!(dir.path().join("b.txt").exists());
    }

    #[test]
    fn apply_patch_to_dir_delete_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doomed.txt"), "bye").unwrap();
        let patch = "\
*** Begin Patch
*** Delete File: doomed.txt
*** End Patch";
        let affected = apply_patch_to_dir(patch, dir.path()).unwrap();
        assert!(affected.deleted.contains(&"doomed.txt".to_string()));
        assert!(!dir.path().join("doomed.txt").exists());
    }

    #[test]
    fn apply_patch_to_dir_update_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "old line\n").unwrap();
        let patch = "\
*** Begin Patch
*** Update File: file.txt
@@
-old line
+new line
*** End Patch";
        let affected = apply_patch_to_dir(patch, dir.path()).unwrap();
        assert!(affected.modified.contains(&"file.txt".to_string()));
        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert!(content.contains("new line"));
        assert!(!content.contains("old line"));
    }

    #[test]
    fn apply_patch_to_dir_invalid_patch_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "this is not a valid patch";
        assert!(apply_patch_to_dir(patch, dir.path()).is_err());
    }

    #[test]
    fn apply_patch_to_dir_nested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
*** Begin Patch
*** Add File: subdir/nested/file.txt
+nested content
*** End Patch";
        apply_patch_to_dir(patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("subdir/nested/file.txt")).unwrap();
        assert_eq!(content, "nested content");
    }
}
