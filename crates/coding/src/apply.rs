//! Filesystem applicator — takes parsed patches and applies them to disk.

use anyhow::{bail, Context, Result};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::parser::{Hunk, Patch, PatchOperation};
use crate::seek_sequence::seek_sequence;

/// Maximum size (in bytes) for any single file created or modified by a patch.
/// Prevents disk exhaustion from LLM-generated oversized patches.
const MAX_PATCH_FILE_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// Categorized list of files affected by a patch application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AffectedPaths {
    /// Files that were newly created.
    pub added: Vec<String>,
    /// Files that were updated in place.
    pub modified: Vec<String>,
    /// Files that were removed.
    pub deleted: Vec<String>,
    /// Files that were moved (from, to).
    pub moved: Vec<(String, String)>,
}

impl AffectedPaths {
    /// Format as a human-readable summary with prefixed lines.
    pub fn format_summary(&self) -> String {
        let mut lines = Vec::new();
        for p in &self.added {
            lines.push(format!("A {p}"));
        }
        for p in &self.modified {
            lines.push(format!("M {p}"));
        }
        for p in &self.deleted {
            lines.push(format!("D {p}"));
        }
        for (from, to) in &self.moved {
            lines.push(format!("R {from} \u{2192} {to}"));
        }
        lines.join("\n")
    }
}

/// Validate that `path` stays within `base_dir` after canonicalization.
///
/// NOTE: This function enforces base_dir containment. Cross-directory support
/// is handled at the tool handler layer by setting base_dir="/" and pre-validating
/// paths against blocked_paths. Do not loosen this check here.
fn validate_patch_path(path: &str, base_dir: &Path) -> Result<()> {
    if path.is_empty() {
        bail!("Empty file path in patch operation");
    }
    let full = base_dir.join(path);
    let canonical_base = std::fs::canonicalize(base_dir).with_context(|| {
        format!(
            "Failed to canonicalize base directory: {}",
            base_dir.display()
        )
    })?;
    // For new files, canonicalize as far as possible then check prefix
    let resolved = if full.exists() {
        std::fs::canonicalize(&full)?
    } else {
        // Walk up to find an existing ancestor, then append the rest
        let mut existing = full;
        let mut tail = Vec::new();
        while !existing.exists() {
            if let Some(file_name) = existing.file_name() {
                tail.push(file_name.to_os_string());
            } else {
                break;
            }
            if let Some(parent) = existing.parent() {
                existing = parent.to_path_buf();
            } else {
                break;
            }
        }
        let mut resolved = if existing.exists() {
            std::fs::canonicalize(&existing)?
        } else {
            existing
        };
        for component in tail.into_iter().rev() {
            resolved = resolved.join(component);
        }
        resolved
    };
    if !resolved.starts_with(&canonical_base) {
        bail!("Path traversal detected: '{path}' escapes base directory");
    }
    Ok(())
}

/// Snapshot of a file's state before patch application, for rollback.
enum FileSnapshot {
    /// File existed with this content.
    Existed(String),
    /// File did not exist (was created by the patch).
    DidNotExist,
}

/// Apply a parsed patch to the filesystem under `base_dir`.
pub fn apply_patch(patch: &Patch, base_dir: &Path) -> Result<AffectedPaths> {
    // Validate all paths up front before making any changes
    for op in &patch.operations {
        match op {
            PatchOperation::AddFile { path, .. } => {
                validate_patch_path(path, base_dir)?;
            }
            PatchOperation::UpdateFile { path, move_to, .. } => {
                validate_patch_path(path, base_dir)?;
                if let Some(dest) = move_to {
                    validate_patch_path(dest, base_dir)?;
                }
            }
            PatchOperation::DeleteFile { path } => {
                validate_patch_path(path, base_dir)?;
            }
        }
    }

    let mut affected = AffectedPaths::default();
    let mut snapshots: Vec<(String, FileSnapshot)> = Vec::new();

    let result = apply_patch_inner(patch, base_dir, &mut affected, &mut snapshots);

    if let Err(e) = result {
        // Rollback: restore all snapshotted files
        info!("Patch failed, rolling back {} files", snapshots.len());
        for (path, snapshot) in snapshots.into_iter().rev() {
            let full_path = base_dir.join(&path);
            match snapshot {
                FileSnapshot::Existed(content) => {
                    if let Err(e) = std::fs::write(&full_path, &content) {
                        warn!("Rollback: failed to restore {}: {e}", full_path.display());
                    }
                }
                FileSnapshot::DidNotExist => {
                    if let Err(e) = std::fs::remove_file(&full_path) {
                        warn!("Rollback: failed to remove {}: {e}", full_path.display());
                    }
                }
            }
        }
        return Err(e);
    }

    Ok(affected)
}

/// Refuse to write through symlinks to prevent TOCTOU attacks.
fn reject_symlink(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("Refusing to write through symlink: {}", path.display());
        }
    }
    Ok(())
}

fn apply_patch_inner(
    patch: &Patch,
    base_dir: &Path,
    affected: &mut AffectedPaths,
    snapshots: &mut Vec<(String, FileSnapshot)>,
) -> Result<()> {
    for op in &patch.operations {
        match op {
            PatchOperation::AddFile { path, content } => {
                if content.len() > MAX_PATCH_FILE_SIZE {
                    bail!(
                        "File '{path}' exceeds maximum size ({} bytes > {} bytes)",
                        content.len(),
                        MAX_PATCH_FILE_SIZE
                    );
                }
                let full_path = base_dir.join(path);
                reject_symlink(&full_path)
                    .with_context(|| format!("Symlink check failed for {path}"))?;
                // Snapshot existing file if it exists (overwrite case)
                if full_path.exists() {
                    let existing = std::fs::read_to_string(&full_path)
                        .with_context(|| format!("Failed to read existing {path}"))?;
                    snapshots.push((path.clone(), FileSnapshot::Existed(existing)));
                } else {
                    snapshots.push((path.clone(), FileSnapshot::DidNotExist));
                }
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create directory for {path}"))?;
                }
                std::fs::write(&full_path, content)
                    .with_context(|| format!("Failed to write {path}"))?;
                info!("Added file: {path}");
                affected.added.push(path.clone());
            }
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            } => {
                let full_path = base_dir.join(path);
                if !full_path.exists() {
                    bail!("Cannot update non-existent file: {path}");
                }

                let original = std::fs::read_to_string(&full_path)
                    .with_context(|| format!("Failed to read {path}"))?;
                snapshots.push((path.clone(), FileSnapshot::Existed(original.clone())));

                let content = apply_hunks(&original, hunks, path)?;

                if content.len() > MAX_PATCH_FILE_SIZE {
                    bail!(
                        "Updated file '{path}' exceeds maximum size ({} bytes > {} bytes)",
                        content.len(),
                        MAX_PATCH_FILE_SIZE
                    );
                }

                if let Some(dest) = move_to {
                    // Move/rename: write to destination, delete original
                    let dest_path = base_dir.join(dest);
                    reject_symlink(&dest_path)
                        .with_context(|| format!("Symlink check failed for {dest}"))?;
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("Failed to create directory for {dest}"))?;
                    }
                    // Snapshot destination if it exists
                    if dest_path.exists() {
                        let existing = std::fs::read_to_string(&dest_path)
                            .with_context(|| format!("Failed to read existing {dest}"))?;
                        snapshots.push((dest.clone(), FileSnapshot::Existed(existing)));
                    } else {
                        snapshots.push((dest.clone(), FileSnapshot::DidNotExist));
                    }
                    std::fs::write(&dest_path, &content)
                        .with_context(|| format!("Failed to write {dest}"))?;
                    std::fs::remove_file(&full_path)
                        .with_context(|| format!("Failed to delete original {path}"))?;
                    info!("Moved file: {path} -> {dest}");
                    affected.moved.push((path.clone(), dest.clone()));
                } else {
                    reject_symlink(&full_path)
                        .with_context(|| format!("Symlink check failed for {path}"))?;
                    std::fs::write(&full_path, &content)
                        .with_context(|| format!("Failed to write {path}"))?;
                    info!("Updated file: {path}");
                    affected.modified.push(path.clone());
                }
            }
            PatchOperation::DeleteFile { path } => {
                let full_path = base_dir.join(path);
                if full_path.exists() {
                    let content = std::fs::read_to_string(&full_path)
                        .with_context(|| format!("Failed to read {path} for snapshot"))?;
                    snapshots.push((path.clone(), FileSnapshot::Existed(content)));
                    std::fs::remove_file(&full_path)
                        .with_context(|| format!("Failed to delete {path}"))?;
                    info!("Deleted file: {path}");
                } else {
                    debug!("File already absent: {path}");
                }
                affected.deleted.push(path.clone());
            }
        }
    }

    Ok(())
}

/// A computed replacement: the matched line range and the new lines to substitute.
struct Replacement {
    start_idx: usize,
    old_line_count: usize,
    new_lines: Vec<String>,
}

/// Apply all hunks to content using two-phase approach:
/// 1. Compute phase: find match positions in original content
/// 2. Apply phase: apply replacements in descending order
fn apply_hunks(content: &str, hunks: &[Hunk], file_path: &str) -> Result<String> {
    if hunks.is_empty() {
        return Ok(content.to_string());
    }

    let content_lines: Vec<String> = content.lines().map(String::from).collect();
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut cursor = 0usize;

    // Compute phase: find each hunk's match position in the original content
    for hunk in hunks {
        if hunk.search.is_empty() {
            // Append mode: place at end of file
            replacements.push(Replacement {
                start_idx: content_lines.len(),
                old_line_count: 0,
                new_lines: hunk.replace.lines().map(String::from).collect(),
            });
            continue;
        }

        let search_lines: Vec<String> = hunk.search.lines().map(String::from).collect();

        // If context_hint is present, try to find the context line first to narrow search
        let search_start = if let Some(ref hint) = hunk.context_hint {
            find_context_hint(&content_lines, hint, cursor).unwrap_or(cursor)
        } else {
            cursor
        };

        let mut found = seek_sequence(
            &content_lines,
            &search_lines,
            search_start,
            hunk.is_end_of_file,
        );

        // Trailing empty line retry: when split('\n') produces a trailing empty
        // string that doesn't exist in content.lines(), retry without it.
        let mut effective_search = search_lines.as_slice();
        let new_lines_vec: Vec<String> = hunk.replace.lines().map(String::from).collect();
        let mut effective_new = new_lines_vec.as_slice();

        if found.is_none() && effective_search.last().is_some_and(String::is_empty) {
            effective_search = &search_lines[..search_lines.len() - 1];
            if effective_new.last().is_some_and(String::is_empty) {
                effective_new = &new_lines_vec[..new_lines_vec.len() - 1];
            }
            found = seek_sequence(
                &content_lines,
                effective_search,
                search_start,
                hunk.is_end_of_file,
            );
        }

        if let Some(idx) = found {
            cursor = idx + effective_search.len();
            replacements.push(Replacement {
                start_idx: idx,
                old_line_count: effective_search.len(),
                new_lines: effective_new.to_vec(),
            });
        } else {
            bail!(
                "Could not find search text in file '{}' (patch line {}):\n---\n{}\n---",
                file_path,
                hunk.source_line,
                hunk.search
            );
        }
    }

    // Apply phase: sort by descending start_idx so earlier replacements don't shift later ones
    replacements.sort_by(|a, b| b.start_idx.cmp(&a.start_idx));

    // Validate no overlapping replacements
    for window in replacements.windows(2) {
        // window[0] has higher start_idx, window[1] has lower
        let earlier = &window[1];
        let later = &window[0];
        if earlier.start_idx + earlier.old_line_count > later.start_idx {
            bail!(
                "Overlapping hunks detected in '{}': lines {}..{} and {}..{}",
                file_path,
                earlier.start_idx,
                earlier.start_idx + earlier.old_line_count,
                later.start_idx,
                later.start_idx + later.old_line_count,
            );
        }
    }

    let mut result_lines = content_lines;
    for rep in replacements {
        let end = rep.start_idx + rep.old_line_count;
        result_lines.splice(rep.start_idx..end, rep.new_lines);
    }

    let mut result = result_lines.join("\n");
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// Find a context hint line in content, starting from cursor.
fn find_context_hint(lines: &[String], hint: &str, cursor: usize) -> Option<usize> {
    let hint_trimmed = hint.trim();
    (cursor..lines.len()).find(|&i| lines[i].trim().contains(hint_trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Hunk, Patch, PatchOperation};
    use tempfile::TempDir;

    fn make_patch(ops: Vec<PatchOperation>) -> Patch {
        Patch { operations: ops }
    }

    fn simple_hunk(search: &str, replace: &str) -> Hunk {
        Hunk {
            context_hint: None,
            search: search.to_string(),
            replace: replace.to_string(),
            is_end_of_file: false,
            source_line: 0,
        }
    }

    #[test]
    fn add_new_file() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::AddFile {
            path: "sub/dir/hello.txt".to_string(),
            content: "hello world".to_string(),
        }]);
        let affected = apply_patch(&patch, dir.path()).unwrap();
        assert_eq!(affected.added, vec!["sub/dir/hello.txt"]);
        let content = std::fs::read_to_string(dir.path().join("sub/dir/hello.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn delete_existing_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("doomed.txt");
        std::fs::write(&file_path, "bye").unwrap();
        assert!(file_path.exists());

        let patch = make_patch(vec![PatchOperation::DeleteFile {
            path: "doomed.txt".to_string(),
        }]);
        let affected = apply_patch(&patch, dir.path()).unwrap();
        assert_eq!(affected.deleted, vec!["doomed.txt"]);
        assert!(!file_path.exists());
    }

    #[test]
    fn delete_nonexistent_file_succeeds() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::DeleteFile {
            path: "ghost.txt".to_string(),
        }]);
        let affected = apply_patch(&patch, dir.path()).unwrap();
        assert_eq!(affected.deleted, vec!["ghost.txt"]);
    }

    #[test]
    fn update_file_exact_match() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {\n    old();\n}\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "main.rs".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("    old();", "    new();")],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("new();"));
        assert!(!content.contains("old();"));
    }

    #[test]
    fn update_file_whitespace_normalized_match() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("ws.rs");
        // File has leading/trailing whitespace differences
        std::fs::write(&file_path, "  fn run() {  \n\t do_thing() ;\t\n}\n").unwrap();

        // Search lines differ only in leading/trailing whitespace — seek_sequence trim handles this
        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "ws.rs".to_string(),
            move_to: None,
            hunks: vec![simple_hunk(
                "fn run() {\ndo_thing() ;",
                "fn run() {\n    do_thing();",
            )],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("    do_thing();"));
    }

    #[test]
    fn update_file_search_not_found_errors() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("nope.rs");
        std::fs::write(&file_path, "fn main() {}").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "nope.rs".to_string(),
            move_to: None,
            hunks: vec![simple_hunk(
                "this text does not exist anywhere",
                "replacement",
            )],
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Could not find search text") || msg.contains("Failed to apply hunk"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn update_nonexistent_file_errors() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "missing.rs".to_string(),
            move_to: None,
            hunks: vec![],
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("non-existent"));
    }

    #[test]
    fn add_file_path_traversal_blocked() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::AddFile {
            path: "../../etc/evil.txt".to_string(),
            content: "malicious".to_string(),
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Path traversal"),
            "expected path traversal error"
        );
    }

    #[test]
    fn update_file_path_traversal_blocked() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "../../../etc/passwd".to_string(),
            move_to: None,
            hunks: vec![],
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Path traversal"),
            "expected path traversal error"
        );
    }

    #[test]
    fn delete_file_path_traversal_blocked() {
        let dir = TempDir::new().unwrap();
        let patch = make_patch(vec![PatchOperation::DeleteFile {
            path: "../../dangerous.txt".to_string(),
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Path traversal"),
            "expected path traversal error"
        );
    }

    /// End-to-end test: parse + apply a patch that creates a file whose content
    /// contains embedded patch DSL markers (e.g., a README documenting the DSL).
    #[test]
    fn end_to_end_add_file_with_embedded_patch_markers() {
        let dir = TempDir::new().unwrap();
        let patch_text = "\
*** Begin Patch
*** Add File: README.md
+# My Project
+
+## Patch DSL
+
+Example:
+
+*** Begin Patch
+*** Add File: tool-name/tool.toml
++content here
+*** End Patch
+
+That's it.
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();

        assert_eq!(affected.added, vec!["README.md"]);
        assert!(dir.path().join("README.md").exists());

        assert!(
            !dir.path().join("tool-name").exists(),
            "Should not create tool-name/ directory from embedded patch example"
        );

        let content = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert!(content.contains("# My Project"));
        assert!(content.contains("*** Begin Patch"));
        assert!(content.contains("*** Add File: tool-name/tool.toml"));
        assert!(content.contains("That's it."));
    }

    /// End-to-end: parse + apply a full patch with add, update, and delete.
    #[test]
    fn end_to_end_full_patch() {
        let dir = TempDir::new().unwrap();

        std::fs::write(dir.path().join("update.txt"), "foo\nbar\nbaz\n").unwrap();
        std::fs::write(dir.path().join("delete.txt"), "gone").unwrap();

        let patch_text = "\
*** Begin Patch
*** Add File: new.txt
+hello world
*** Update File: update.txt
@@
 foo
-bar
+BAR
*** Delete File: delete.txt
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.added.len(), 1);
        assert_eq!(affected.modified.len(), 1);
        assert_eq!(affected.deleted.len(), 1);

        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world");

        let content = std::fs::read_to_string(dir.path().join("update.txt")).unwrap();
        assert!(content.contains("BAR"));
        assert!(!content.contains("\nbar\n"));

        assert!(!dir.path().join("delete.txt").exists());
    }

    // ── New tests for ported features ──

    #[test]
    fn move_file_operation() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.txt"), "hello\nworld\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: new.txt
@@
-hello
+HELLO
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(
            affected.moved,
            vec![("old.txt".to_string(), "new.txt".to_string())]
        );
        assert!(!dir.path().join("old.txt").exists());
        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert!(content.contains("HELLO"));
        assert!(content.contains("world"));
    }

    #[test]
    fn move_only_patch_renames_preserving_content() {
        // Pure rename via the public string-based API: parse + apply with
        // only `*** Move to:` and zero hunks. Catches regressions where
        // empty-hunks paths silently no-op or where content is mangled.
        let dir = TempDir::new().unwrap();
        let original = "line one\nline two\nline three\n";
        std::fs::write(dir.path().join("old.txt"), original).unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: new.txt
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(
            affected.moved,
            vec![("old.txt".to_string(), "new.txt".to_string())]
        );
        assert!(affected.modified.is_empty(), "rename must not also modify");
        assert!(!dir.path().join("old.txt").exists());
        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, original, "content must be preserved byte-for-byte");
    }

    #[test]
    fn move_file_path_traversal_blocked() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("src.txt"), "data").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "src.txt".to_string(),
            move_to: Some("../../etc/evil.txt".to_string()),
            hunks: vec![],
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Path traversal"));
    }

    #[test]
    fn eof_marker_matches_at_end() {
        let dir = TempDir::new().unwrap();
        // Two occurrences of "foo\nbar" — one early, one at the very end
        std::fs::write(
            dir.path().join("f.txt"),
            "header\nfoo\nbar\nmiddle\nfoo\nbar",
        )
        .unwrap();

        // This patch should match the second "foo\nbar" (at end) due to EOF marker
        let patch_text = "\
*** Begin Patch
*** Update File: f.txt
@@
-foo
-bar
+FOO
+BAR
*** End of File
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.modified, vec!["f.txt"]);
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        // The second occurrence (at end) should be replaced
        assert!(content.contains("FOO\nBAR"));
        // First occurrence should still be lowercase
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[1], "foo");
        assert_eq!(lines[2], "bar");
    }

    #[test]
    fn context_hint_disambiguates_hunks() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "fn alpha() {\n    x = 1;\n}\nfn beta() {\n    x = 1;\n}\n",
        )
        .unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: f.txt
@@ fn beta()
     x = 1;
-}
+    y = 2;
+}
*** End Patch";

        crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        // beta should have y = 2 inserted
        assert!(content.contains("fn beta() {\n    x = 1;\n    y = 2;\n}"));
        // alpha should be untouched
        assert!(content.contains("fn alpha() {\n    x = 1;\n}"));
    }

    #[test]
    fn unicode_fuzzy_matching_in_file() {
        let dir = TempDir::new().unwrap();
        // File uses em dash and smart quotes
        std::fs::write(
            dir.path().join("f.txt"),
            "value \u{2014} \u{201C}hello\u{201D}\n",
        )
        .unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![Hunk {
                context_hint: None,
                search: "value - \"hello\"".to_string(),
                replace: "value - \"world\"".to_string(),
                is_end_of_file: false,
                source_line: 1,
            }],
        }]);

        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(content.contains("world"));
    }

    #[test]
    fn two_phase_descending_replacement() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb\nccc\nddd\neee\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("bbb", "BBB\nBBB2"), simple_hunk("ddd", "DDD")],
        }]);

        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(content, "aaa\nBBB\nBBB2\nccc\nDDD\neee\n");
    }

    #[test]
    fn error_message_includes_source_line() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![Hunk {
                context_hint: None,
                search: "nonexistent".to_string(),
                replace: "x".to_string(),
                is_end_of_file: false,
                source_line: 42,
            }],
        }]);

        let err = apply_patch(&patch, dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("patch line 42"),
            "error should contain line number: {err}"
        );
        assert!(
            err.contains("f.txt"),
            "error should contain filename: {err}"
        );
    }

    #[test]
    fn affected_paths_format_summary() {
        let paths = AffectedPaths {
            added: vec!["a.txt".to_string()],
            modified: vec!["b.txt".to_string()],
            deleted: vec!["c.txt".to_string()],
            moved: vec![("d.txt".to_string(), "e.txt".to_string())],
        };
        let summary = paths.format_summary();
        assert!(summary.contains("A a.txt"));
        assert!(summary.contains("M b.txt"));
        assert!(summary.contains("D c.txt"));
        assert!(summary.contains("R d.txt"));
        assert!(summary.contains("e.txt"));
    }

    #[test]
    fn trailing_newline_preserved() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb\nccc\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("bbb", "BBB")],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(
            content.ends_with('\n'),
            "trailing newline should be preserved"
        );
        assert_eq!(content, "aaa\nBBB\nccc\n");
    }

    #[test]
    fn no_trailing_newline_not_added() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("bbb", "BBB")],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(!content.ends_with('\n'), "should not add trailing newline");
        assert_eq!(content, "aaa\nBBB");
    }

    #[test]
    fn rollback_on_multi_op_failure() {
        let dir = TempDir::new().unwrap();
        // First op: add file (will succeed)
        // Second op: update non-matching hunk (will fail)
        std::fs::write(dir.path().join("existing.txt"), "hello\n").unwrap();

        let patch = make_patch(vec![
            PatchOperation::AddFile {
                path: "new.txt".to_string(),
                content: "new content".to_string(),
            },
            PatchOperation::UpdateFile {
                path: "existing.txt".to_string(),
                move_to: None,
                hunks: vec![simple_hunk("nonexistent search text", "replacement")],
            },
        ]);

        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        // The added file should be rolled back
        assert!(
            !dir.path().join("new.txt").exists(),
            "added file should be rolled back on failure"
        );
        // The existing file should be untouched
        let content = std::fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn move_with_failed_hunk_rolls_back() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("src.txt"), "hello\nworld\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: src.txt
*** Move to: dest.txt
@@
-nonexistent
+replacement
*** End Patch";

        let result = crate::apply_patch_to_dir(patch_text, dir.path());
        assert!(result.is_err());
        // Source should still exist, destination should not
        assert!(dir.path().join("src.txt").exists());
        assert!(!dir.path().join("dest.txt").exists());
    }

    #[test]
    fn append_hunk_empty_search() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "existing\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("", "appended line")],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(content.contains("appended line"));
    }

    #[test]
    fn context_hint_with_trailing_at_signs() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "fn alpha() {\n    x = 1;\n}\nfn beta() {\n    x = 1;\n}\n",
        )
        .unwrap();

        // Use @@ fn beta() @@ format (trailing @@)
        let patch_text = "\
*** Begin Patch
*** Update File: f.txt
@@ fn beta() @@
     x = 1;
-}
+    y = 2;
+}
*** End Patch";

        crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(content.contains("fn beta() {\n    x = 1;\n    y = 2;\n}"));
    }

    #[test]
    fn move_to_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.txt"), "content\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: sub/dir/new.txt
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(
            affected.moved,
            vec![("old.txt".to_string(), "sub/dir/new.txt".to_string())]
        );
        assert!(!dir.path().join("old.txt").exists());
        let content = std::fs::read_to_string(dir.path().join("sub/dir/new.txt")).unwrap();
        assert_eq!(content, "content\n");
    }

    #[test]
    fn end_to_end_move_with_hunks() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.rs"), "fn old() {\n    1\n}\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: old.rs
*** Move to: new.rs
@@
-fn old() {
+fn new() {
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(
            affected.moved,
            vec![("old.rs".to_string(), "new.rs".to_string())]
        );
        assert!(!dir.path().join("old.rs").exists());
        let content = std::fs::read_to_string(dir.path().join("new.rs")).unwrap();
        assert!(content.contains("fn new()"));
    }

    /// End-to-end: LLM erroneously prefixes *** End Patch with +.
    /// The marker must NOT leak into the created file.
    #[test]
    fn end_to_end_add_file_plus_end_patch_stripped() {
        let dir = TempDir::new().unwrap();
        let patch_text = "\
*** Begin Patch
*** Add File: snake.py
+import sys
+print('hello')
+*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.added, vec!["snake.py"]);

        let content = std::fs::read_to_string(dir.path().join("snake.py")).unwrap();
        assert!(content.contains("import sys"));
        assert!(content.contains("print('hello')"));
        assert!(
            !content.contains("End Patch"),
            "DSL marker leaked into file: {content}"
        );
    }

    /// End-to-end: +*** End Patch in Update File hunk must not leak to disk.
    #[test]
    fn end_to_end_update_file_plus_end_patch_stripped() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("app.py"), "old_line\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: app.py
@@
-old_line
+new_line
+*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.modified, vec!["app.py"]);

        let content = std::fs::read_to_string(dir.path().join("app.py")).unwrap();
        assert!(content.contains("new_line"));
        assert!(
            !content.contains("End Patch"),
            "DSL marker leaked into file: {content}"
        );
    }

    #[test]
    fn affected_paths_empty_default() {
        let paths = AffectedPaths::default();
        assert!(paths.added.is_empty());
        assert!(paths.modified.is_empty());
        assert!(paths.deleted.is_empty());
        assert!(paths.moved.is_empty());
        assert!(paths.format_summary().is_empty());
    }

    #[test]
    fn test_add_file_exceeds_size_limit() {
        let dir = TempDir::new().unwrap();
        let oversized = "x".repeat(MAX_PATCH_FILE_SIZE + 1);
        let patch = make_patch(vec![PatchOperation::AddFile {
            path: "big.txt".to_string(),
            content: oversized,
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceeds maximum size"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_update_file_exceeds_size_limit() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "small\n").unwrap();
        let big_replacement = "x".repeat(MAX_PATCH_FILE_SIZE + 1);
        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("small", &big_replacement)],
        }]);
        let result = apply_patch(&patch, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceeds maximum size"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_file_within_size_limit() {
        let dir = TempDir::new().unwrap();
        // Just under the limit should succeed
        let content = "x".repeat(MAX_PATCH_FILE_SIZE);
        let patch = make_patch(vec![PatchOperation::AddFile {
            path: "ok.txt".to_string(),
            content: content.clone(),
        }]);
        let affected = apply_patch(&patch, dir.path()).unwrap();
        assert_eq!(affected.added, vec!["ok.txt"]);
        let read = std::fs::read_to_string(dir.path().join("ok.txt")).unwrap();
        assert_eq!(read.len(), MAX_PATCH_FILE_SIZE);
    }

    #[test]
    fn trailing_empty_line_retry() {
        let dir = TempDir::new().unwrap();
        // File ends with newline — content.lines() won't produce trailing empty
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb\n").unwrap();

        // Search pattern that split('\n') would produce: ["bbb", ""]
        // This trailing empty should be retried without.
        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "f.txt".to_string(),
            move_to: None,
            hunks: vec![Hunk {
                context_hint: None,
                search: "bbb\n".to_string(), // produces ["bbb", ""] via lines+split
                replace: "BBB\n".to_string(),
                is_end_of_file: false,
                source_line: 1,
            }],
        }]);
        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(
            content.contains("BBB"),
            "trailing empty line retry should match"
        );
    }

    #[test]
    fn end_to_end_heredoc_patch() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), "x = 1\ny = 2\n").unwrap();

        let patch_text = "<<'EOF'\n\
*** Begin Patch\n\
*** Update File: main.py\n\
@@\n\
-x = 1\n\
+x = 10\n\
*** End Patch\n\
EOF";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.modified, vec!["main.py"]);
        let content = std::fs::read_to_string(dir.path().join("main.py")).unwrap();
        assert!(content.contains("x = 10"));
        assert!(!content.contains("x = 1\n"));
    }

    #[test]
    fn end_to_end_first_chunk_without_at() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.py"), "import os\nx = 1\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: f.py
 import os
-x = 1
+x = 2
*** End Patch";

        let affected = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert_eq!(affected.modified, vec!["f.py"]);
        let content = std::fs::read_to_string(dir.path().join("f.py")).unwrap();
        assert!(content.contains("x = 2"));
        assert!(!content.contains("x = 1"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_target() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();

        // A patch that tries to write through a symlink should fail
        let patch = format!(
            "*** Begin Patch\n*** Update File: link.txt\n@@\n-original\n+hacked\n*** End Patch"
        );
        let result = crate::apply_patch_to_dir(&patch, dir.path());
        assert!(result.is_err(), "should reject write through symlink");
    }
}
