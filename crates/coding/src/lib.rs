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
pub mod fs_atomic;
pub mod invocation;
pub mod parser;
mod seek_sequence;
pub mod streaming_parser;

pub use invocation::maybe_extract_apply_patch_from_shell;

use anyhow::Result;
use std::path::Path;

pub use apply::{AffectedPaths, ApplyPatchResult};
pub use parser::ParseMode;
pub use streaming_parser::{StreamEvent, StreamingPatchParser};

/// Parse and apply a patch to a directory, returning which files were affected.
///
/// Defaults to `ParseMode::Lenient`: the `*** Begin Patch` / `*** End Patch`
/// sentinels are still required, but a surrounding shell-style EOF heredoc
/// (`<<EOF`, `<<'EOF'`, `<<"EOF"`) is tolerated and stripped before parsing.
/// Some LLM tool-call shapes emit patches wrapped this way; rejecting them
/// just costs the agent an extra retry. Use `apply_patch_to_dir_with_mode`
/// with `ParseMode::Strict` if you need to reject heredoc wrappers.
pub fn apply_patch_to_dir(patch_text: &str, base_dir: &Path) -> Result<ApplyPatchResult> {
    apply_patch_to_dir_with_mode(patch_text, base_dir, ParseMode::Lenient)
}

/// Parse and apply a patch with an explicit `ParseMode`.
pub fn apply_patch_to_dir_with_mode(
    patch_text: &str,
    base_dir: &Path,
    mode: ParseMode,
) -> Result<ApplyPatchResult> {
    let patch = parser::parse_patch_with_mode(patch_text, mode)?;
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
    fn apply_patch_to_dir_default_is_lenient_unwraps_eof_heredoc() {
        // LLMs that target a `local_shell`-shaped tool sometimes wrap the patch
        // in a `<<EOF`-family heredoc. The default entry point must tolerate this.
        let dir = tempfile::tempdir().unwrap();
        let inner = "*** Begin Patch\n*** Add File: hi.txt\n+hi\n*** End Patch";
        for opener in ["<<EOF", "<<'EOF'", "<<\"EOF\""] {
            let wrapped = format!("{opener}\n{inner}\nEOF\n");
            let affected = apply_patch_to_dir(&wrapped, dir.path())
                .unwrap_or_else(|e| panic!("opener {opener} should be accepted: {e}"));
            assert!(
                affected.added.contains(&"hi.txt".to_string())
                    || affected.modified.contains(&"hi.txt".to_string())
            );
            // Reset for next iteration.
            let _ = std::fs::remove_file(dir.path().join("hi.txt"));
        }
    }

    #[test]
    fn apply_patch_to_dir_strict_mode_still_rejects_heredoc() {
        // The mode-explicit entry point still honors Strict for callers that
        // want to reject the heredoc shape (e.g. trusted internal callers).
        let dir = tempfile::tempdir().unwrap();
        let wrapped = "<<EOF\n*** Begin Patch\n*** Add File: hi.txt\n+hi\n*** End Patch\nEOF\n";
        let err = apply_patch_to_dir_with_mode(wrapped, dir.path(), ParseMode::Strict)
            .expect_err("strict mode must reject heredoc-wrapped input");
        assert!(
            err.to_string().contains("*** Begin Patch"),
            "error should mention the missing sentinel: {err}"
        );
    }

    #[test]
    fn apply_patch_to_dir_empty_patch_is_rejected() {
        // An empty patch (sentinels only, zero hunks) is a no-op that wastes a
        // turn and signals an LLM error. Reject at parse time so the agent gets
        // a clear message and retries.
        let dir = tempfile::tempdir().unwrap();
        let empty = "*** Begin Patch\n*** End Patch\n";
        let err = apply_patch_to_dir(empty, dir.path()).expect_err("empty patch must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("No operations") || msg.contains("empty"),
            "error should explain emptiness, got: {msg}"
        );
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
