//! Filesystem applicator — takes parsed patches and applies them to disk.

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::parser::{Hunk, Patch, PatchOperation};
use crate::seek_sequence::{seek_sequence, MatchOutcome};

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

/// Successful patch application details.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyPatchResult {
    /// Files affected by the patch.
    pub affected: AffectedPaths,
    /// Unified-diff style rendering of applied changes.
    pub diff: String,
}

impl ApplyPatchResult {
    /// Format the affected paths summary.
    pub fn format_summary(&self) -> String {
        self.affected.format_summary()
    }
}

impl Deref for ApplyPatchResult {
    type Target = AffectedPaths;

    fn deref(&self) -> &Self::Target {
        &self.affected
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
    let resolved = if full.exists() {
        std::fs::canonicalize(&full)?
    } else {
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

enum CommitAction {
    Write { path: String, content: String },
    Delete { path: String },
}

struct PreparedPatch {
    result: ApplyPatchResult,
    actions: Vec<CommitAction>,
}

/// Apply a parsed patch to the filesystem under `base_dir`.
pub fn apply_patch(patch: &Patch, base_dir: &Path) -> Result<ApplyPatchResult> {
    let prepared = prepare_patch(patch, base_dir)?;
    commit_prepared_patch(prepared, base_dir)
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

fn prepare_patch(patch: &Patch, base_dir: &Path) -> Result<PreparedPatch> {
    let mut errors = Vec::new();
    let mut overlay: HashMap<String, Option<String>> = HashMap::new();
    let mut actions = Vec::new();
    let mut affected = AffectedPaths::default();
    let mut diffs = Vec::new();

    for op in &patch.operations {
        match op {
            PatchOperation::AddFile { path, content } => {
                if let Err(err) = validate_patch_path(path, base_dir) {
                    errors.push(err.to_string());
                    continue;
                }
                if content.len() > MAX_PATCH_FILE_SIZE {
                    errors.push(format!(
                        "File '{path}' exceeds maximum size ({} bytes > {} bytes)",
                        content.len(),
                        MAX_PATCH_FILE_SIZE
                    ));
                    continue;
                }

                match virtual_file_state(path, base_dir, &overlay) {
                    Ok(previous) => {
                        overlay.insert(path.clone(), Some(content.clone()));
                        actions.push(CommitAction::Write {
                            path: path.clone(),
                            content: content.clone(),
                        });
                        affected.added.push(path.clone());
                        let from = if previous.is_some() {
                            format!("a/{path}")
                        } else {
                            "/dev/null".to_string()
                        };
                        diffs.push(render_unified_diff(
                            previous.as_deref(),
                            Some(content),
                            &from,
                            &format!("b/{path}"),
                        ));
                    }
                    Err(err) => errors.push(err.to_string()),
                }
            }
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            } => {
                if let Err(err) = validate_patch_path(path, base_dir) {
                    errors.push(err.to_string());
                    continue;
                }
                if let Some(dest) = move_to {
                    if let Err(err) = validate_patch_path(dest, base_dir) {
                        errors.push(err.to_string());
                        continue;
                    }
                }

                let original = match virtual_file_state(path, base_dir, &overlay) {
                    Ok(Some(content)) => content,
                    Ok(None) => {
                        errors.push(format!("Cannot update non-existent file: {path}"));
                        continue;
                    }
                    Err(err) => {
                        errors.push(err.to_string());
                        continue;
                    }
                };

                let updated = match apply_hunks(&original, hunks, path) {
                    Ok(content) => content,
                    Err(err) => {
                        errors.push(format!("{err:#}"));
                        continue;
                    }
                };

                if updated.len() > MAX_PATCH_FILE_SIZE {
                    errors.push(format!(
                        "Updated file '{path}' exceeds maximum size ({} bytes > {} bytes)",
                        updated.len(),
                        MAX_PATCH_FILE_SIZE
                    ));
                    continue;
                }

                if let Some(dest) = move_to {
                    if dest != path {
                        match virtual_file_state(dest, base_dir, &overlay) {
                            Ok(Some(_)) => {
                                errors.push(format!(
                                    "Move destination already exists: {dest} (refusing to overwrite)"
                                ));
                                continue;
                            }
                            Ok(None) => {}
                            Err(err) => {
                                errors.push(err.to_string());
                                continue;
                            }
                        }
                    }

                    overlay.insert(path.clone(), None);
                    overlay.insert(dest.clone(), Some(updated.clone()));
                    actions.push(CommitAction::Write {
                        path: dest.clone(),
                        content: updated.clone(),
                    });
                    if dest != path {
                        actions.push(CommitAction::Delete { path: path.clone() });
                        affected.moved.push((path.clone(), dest.clone()));
                    } else {
                        affected.modified.push(path.clone());
                    }

                    let mut diff = String::new();
                    if dest != path {
                        diff.push_str(&format!("# Moved: {path} -> {dest}\n"));
                    }
                    let rendered = render_unified_diff(
                        Some(&original),
                        Some(&updated),
                        &format!("a/{path}"),
                        &format!("b/{dest}"),
                    );
                    if !rendered.is_empty() {
                        diff.push_str(&rendered);
                    }
                    if !diff.trim().is_empty() {
                        diffs.push(diff.trim_end().to_string());
                    }
                } else {
                    overlay.insert(path.clone(), Some(updated.clone()));
                    actions.push(CommitAction::Write {
                        path: path.clone(),
                        content: updated.clone(),
                    });
                    affected.modified.push(path.clone());
                    diffs.push(render_unified_diff(
                        Some(&original),
                        Some(&updated),
                        &format!("a/{path}"),
                        &format!("b/{path}"),
                    ));
                }
            }
            PatchOperation::DeleteFile { path } => {
                if let Err(err) = validate_patch_path(path, base_dir) {
                    errors.push(err.to_string());
                    continue;
                }

                match virtual_file_state(path, base_dir, &overlay) {
                    Ok(Some(previous)) => {
                        overlay.insert(path.clone(), None);
                        actions.push(CommitAction::Delete { path: path.clone() });
                        affected.deleted.push(path.clone());
                        diffs.push(render_unified_diff(
                            Some(&previous),
                            None,
                            &format!("a/{path}"),
                            "/dev/null",
                        ));
                    }
                    Ok(None) => {
                        debug!("File already absent during preflight: {path}");
                        affected.deleted.push(path.clone());
                    }
                    Err(err) => errors.push(err.to_string()),
                }
            }
        }
    }

    if !errors.is_empty() {
        bail!(
            "Patch validation failed (no files were modified):\n{}",
            errors
                .into_iter()
                .map(|err| format!("  • {err}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    Ok(PreparedPatch {
        result: ApplyPatchResult {
            affected,
            diff: diffs
                .into_iter()
                .filter(|diff| !diff.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
        },
        actions,
    })
}

fn commit_prepared_patch(prepared: PreparedPatch, base_dir: &Path) -> Result<ApplyPatchResult> {
    let mut snapshots: Vec<(String, FileSnapshot)> = Vec::new();
    let mut snapshotted = HashSet::new();

    for action in &prepared.actions {
        let path = match action {
            CommitAction::Write { path, .. } | CommitAction::Delete { path } => path,
        };
        if snapshotted.insert(path.clone()) {
            snapshots.push((path.clone(), snapshot_file_state(path, base_dir)?));
        }
    }

    let result = (|| -> Result<()> {
        for action in &prepared.actions {
            match action {
                CommitAction::Write { path, content } => {
                    let full_path = base_dir.join(path);
                    reject_symlink(&full_path)
                        .with_context(|| format!("Symlink check failed for {path}"))?;
                    if let Some(parent) = full_path.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("Failed to create directory for {path}"))?;
                    }
                    std::fs::write(&full_path, content)
                        .with_context(|| format!("Failed to write {path}"))?;
                    info!("Wrote file: {path}");
                }
                CommitAction::Delete { path } => {
                    let full_path = base_dir.join(path);
                    if full_path.exists() {
                        std::fs::remove_file(&full_path)
                            .with_context(|| format!("Failed to delete {path}"))?;
                        info!("Deleted file: {path}");
                    }
                }
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        info!(
            "Patch failed during commit, rolling back {} files",
            snapshots.len()
        );
        for (path, snapshot) in snapshots.into_iter().rev() {
            let full_path = base_dir.join(&path);
            match snapshot {
                FileSnapshot::Existed(content) => {
                    if let Some(parent) = full_path.parent() {
                        if let Err(err) = std::fs::create_dir_all(parent) {
                            warn!(
                                "Rollback: failed to recreate parent for {}: {err}",
                                full_path.display()
                            );
                            continue;
                        }
                    }
                    if let Err(err) = std::fs::write(&full_path, &content) {
                        warn!("Rollback: failed to restore {}: {err}", full_path.display());
                    }
                }
                FileSnapshot::DidNotExist => {
                    if let Err(err) = std::fs::remove_file(&full_path) {
                        if err.kind() != std::io::ErrorKind::NotFound {
                            warn!("Rollback: failed to remove {}: {err}", full_path.display());
                        }
                    }
                }
            }
        }
        return Err(e);
    }

    Ok(prepared.result)
}

fn virtual_file_state(
    path: &str,
    base_dir: &Path,
    overlay: &HashMap<String, Option<String>>,
) -> Result<Option<String>> {
    if let Some(value) = overlay.get(path) {
        return Ok(value.clone());
    }
    let full_path = base_dir.join(path);
    if full_path.exists() {
        return Ok(Some(
            std::fs::read_to_string(&full_path)
                .with_context(|| format!("Failed to read {path}"))?,
        ));
    }
    Ok(None)
}

fn snapshot_file_state(path: &str, base_dir: &Path) -> Result<FileSnapshot> {
    let full_path = base_dir.join(path);
    if full_path.exists() {
        Ok(FileSnapshot::Existed(
            std::fs::read_to_string(&full_path)
                .with_context(|| format!("Failed to read existing {path}"))?,
        ))
    } else {
        Ok(FileSnapshot::DidNotExist)
    }
}

struct Replacement {
    start_idx: usize,
    old_line_count: usize,
    new_lines: Vec<String>,
}

fn apply_hunks(content: &str, hunks: &[Hunk], file_path: &str) -> Result<String> {
    if hunks.is_empty() {
        return Ok(content.to_string());
    }

    let content_lines: Vec<String> = content.lines().map(String::from).collect();
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut cursor = 0usize;

    for hunk in hunks {
        if hunk.search.is_empty() {
            let insertion_idx = if let Some(ref hint) = hunk.context_hint {
                find_context_anchor(&content_lines, hint, cursor).with_context(|| {
                    format!("In file '{file_path}' (patch line {})", hunk.source_line)
                })? + 1
            } else {
                content_lines.len()
            };

            let new_lines: Vec<String> = hunk.replace.lines().map(String::from).collect();
            cursor = insertion_idx;
            replacements.push(Replacement {
                start_idx: insertion_idx,
                old_line_count: 0,
                new_lines,
            });
            continue;
        }

        let search_lines: Vec<String> = hunk.search.lines().map(String::from).collect();
        let new_lines_vec: Vec<String> = hunk.replace.lines().map(String::from).collect();
        let search_start = if let Some(ref hint) = hunk.context_hint {
            find_optional_context_anchor(&content_lines, hint, cursor)
                .with_context(|| {
                    format!(
                        "In file '{file_path}' (patch line {}) while resolving context hint",
                        hunk.source_line
                    )
                })?
                .unwrap_or(cursor)
        } else {
            cursor
        };

        let (matched_idx, matched_search, matched_replace) = match locate_hunk_match(
            &content_lines,
            &search_lines,
            &new_lines_vec,
            search_start,
            hunk,
        ) {
            Ok(found) => found,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("In file '{file_path}' (patch line {})", hunk.source_line)
                });
            }
        };

        cursor = matched_idx + matched_search.len();
        replacements.push(Replacement {
            start_idx: matched_idx,
            old_line_count: matched_search.len(),
            new_lines: matched_replace.to_vec(),
        });
    }

    replacements.sort_by(|a, b| b.start_idx.cmp(&a.start_idx));
    for window in replacements.windows(2) {
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
    for replacement in replacements {
        let end = replacement.start_idx + replacement.old_line_count;
        result_lines.splice(replacement.start_idx..end, replacement.new_lines);
    }

    let mut result = result_lines.join("\n");
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn locate_hunk_match<'a>(
    content_lines: &[String],
    search_lines: &'a [String],
    replacement_lines: &'a [String],
    search_start: usize,
    hunk: &Hunk,
) -> Result<(usize, &'a [String], &'a [String])> {
    let mut outcome = seek_sequence(
        content_lines,
        search_lines,
        search_start,
        hunk.is_end_of_file,
    );
    let mut effective_search = search_lines;
    let mut effective_replace = replacement_lines;

    if matches!(outcome, MatchOutcome::NotFound)
        && effective_search.last().is_some_and(String::is_empty)
    {
        effective_search = &search_lines[..search_lines.len() - 1];
        if effective_replace.last().is_some_and(String::is_empty) {
            effective_replace = &replacement_lines[..replacement_lines.len() - 1];
        }
        outcome = seek_sequence(
            content_lines,
            effective_search,
            search_start,
            hunk.is_end_of_file,
        );
    }

    match outcome {
        MatchOutcome::Unique(idx) => Ok((idx, effective_search, effective_replace)),
        MatchOutcome::Ambiguous { count, strategy } => bail!(
            "Search text matched {count} locations via {strategy}; provide more surrounding context:\n---\n{}\n---",
            hunk.search
        ),
        MatchOutcome::NotFound => bail!("Could not find search text:\n---\n{}\n---", hunk.search),
    }
}

fn find_context_anchor(lines: &[String], hint: &str, cursor: usize) -> Result<usize> {
    let hint_trimmed = hint.trim();
    let mut positions = Vec::new();
    for (idx, line) in lines.iter().enumerate().skip(cursor) {
        if line.trim().contains(hint_trimmed) {
            positions.push(idx);
        }
    }

    match positions.as_slice() {
        [idx] => Ok(*idx),
        [] => bail!("addition-only hunk context hint '{hint_trimmed}' not found"),
        many => bail!(
            "addition-only hunk context hint '{hint_trimmed}' is ambiguous ({} occurrences); provide a more unique hint",
            many.len()
        ),
    }
}

fn find_optional_context_anchor(
    lines: &[String],
    hint: &str,
    cursor: usize,
) -> Result<Option<usize>> {
    let hint_trimmed = hint.trim();
    let mut positions = Vec::new();
    for (idx, line) in lines.iter().enumerate().skip(cursor) {
        if line.trim().contains(hint_trimmed) {
            positions.push(idx);
        }
    }

    match positions.as_slice() {
        [] => Ok(None),
        [idx] => Ok(Some(*idx)),
        many => {
            debug!(
                "Context hint '{}' matched {} locations; falling back to full-file search",
                hint_trimmed,
                many.len()
            );
            Ok(None)
        }
    }
}

fn render_unified_diff(
    old: Option<&str>,
    new: Option<&str>,
    from_file: &str,
    to_file: &str,
) -> String {
    let old_text = old.unwrap_or("");
    let new_text = new.unwrap_or("");
    if old_text == new_text {
        return String::new();
    }

    let old_lines: Vec<&str> = if old_text.is_empty() {
        Vec::new()
    } else {
        old_text.lines().collect()
    };
    let new_lines: Vec<&str> = if new_text.is_empty() {
        Vec::new()
    } else {
        new_text.lines().collect()
    };

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_changed_end = old_lines.len().saturating_sub(suffix);
    let new_changed_end = new_lines.len().saturating_sub(suffix);
    let old_start = prefix + 1;
    let new_start = prefix + 1;

    let mut diff = String::new();
    diff.push_str(&format!("--- {from_file}\n"));
    diff.push_str(&format!("+++ {to_file}\n"));
    diff.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_start,
        old_changed_end.saturating_sub(prefix),
        new_start,
        new_changed_end.saturating_sub(prefix)
    ));

    for line in &old_lines[..prefix] {
        diff.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[prefix..old_changed_end] {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_changed_end] {
        diff.push_str(&format!("+{line}\n"));
    }
    for line in &old_lines[old_changed_end..] {
        diff.push_str(&format!(" {line}\n"));
    }
    diff.trim_end().to_string()
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
+*** End Patch
*** End Patch";

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
+*** End Patch
*** End Patch";

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

        // Heredoc-wrapped input requires Lenient mode: the public API defaults
        // to Strict (sentinels at the outermost layer), so callers that need
        // shell-heredoc tolerance must opt in.
        let affected =
            crate::apply_patch_to_dir_with_mode(patch_text, dir.path(), crate::ParseMode::Lenient)
                .unwrap();
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

    #[test]
    fn ambiguous_search_text_is_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("dup.txt"),
            "alpha\nsame\nomega\nbeta\nsame\nomega\n",
        )
        .unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: dup.txt
@@
 same
-omega
+OMEGA
*** End Patch";

        let err = crate::apply_patch_to_dir(patch_text, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched 2 locations"), "got: {err}");
        assert!(
            err.contains("provide more surrounding context"),
            "got: {err}"
        );
        let content = std::fs::read_to_string(dir.path().join("dup.txt")).unwrap();
        assert!(content.contains("same\nomega"));
        assert!(!content.contains("OMEGA"));
    }

    #[test]
    fn multi_hunk_matching_uses_original_file_content() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("chain.txt"), "a\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "chain.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("a", "b"), simple_hunk("b", "c")],
        }]);

        let err = apply_patch(&patch, dir.path()).unwrap_err().to_string();
        assert!(err.contains("Could not find search text"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("chain.txt")).unwrap(),
            "a\n"
        );
    }

    #[test]
    fn addition_only_hunk_with_unique_hint_inserts_at_anchor() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("app.py"), "def main():\n    pass\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: app.py
@@ def main @@
+def helper():
+    return 42
*** End Patch";

        let result = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("app.py")).unwrap();
        assert!(content.contains("def main():\ndef helper():\n    return 42\n    pass"));
        assert!(result.diff.contains("+++ b/app.py"));
    }

    #[test]
    fn addition_only_hunk_with_ambiguous_hint_fails() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            "def main():\n    pass\n\ndef main():\n    pass\n",
        )
        .unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: app.py
@@ def main @@
+def helper():
+    return 42
*** End Patch";

        let err = crate::apply_patch_to_dir(patch_text, dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("addition-only hunk context hint 'def main' is ambiguous"),
            "got: {err}"
        );
    }

    #[test]
    fn duplicated_context_hint_falls_back_to_full_file_search() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("main.py"),
            "def main():\n    alpha()\n\ndef main_helper():\n    beta()\n",
        )
        .unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "main.py".to_string(),
            move_to: None,
            hunks: vec![Hunk {
                context_hint: Some("def main".to_string()),
                search: "def main_helper():\n    beta()".to_string(),
                replace: "def main_helper():\n    gamma()".to_string(),
                is_end_of_file: false,
                source_line: 9,
            }],
        }]);

        apply_patch(&patch, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("main.py")).unwrap();
        assert!(content.contains("def main_helper():\n    gamma()"));
        assert!(content.contains("def main():\n    alpha()"));
    }

    #[test]
    fn move_destination_overwrite_is_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("src.txt"), "source\n").unwrap();
        std::fs::write(dir.path().join("dest.txt"), "dest\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: src.txt
*** Move to: dest.txt
@@
-source
+SOURCE
*** End Patch";

        let err = crate::apply_patch_to_dir(patch_text, dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Move destination already exists"),
            "got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest.txt")).unwrap(),
            "dest\n"
        );
    }

    #[test]
    fn preflight_aggregates_errors_without_writing_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "same\nblock\nsame\nblock\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Add File: new.txt
+new file
*** Update File: dup.txt
@@
 same
-block
+BLOCK
*** Update File: missing.txt
@@
-old
+new
*** End Patch";

        let err = crate::apply_patch_to_dir(patch_text, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched 2 locations"), "got: {err}");
        assert!(
            err.contains("Cannot update non-existent file: missing.txt"),
            "got: {err}"
        );
        assert!(
            !dir.path().join("new.txt").exists(),
            "preflight should prevent partial writes"
        );
    }

    #[test]
    fn apply_result_includes_diff() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    old();\n}\n").unwrap();

        let patch_text = "\
*** Begin Patch
*** Update File: main.rs
@@
-    old();
+    new();
*** End Patch";

        let result = crate::apply_patch_to_dir(patch_text, dir.path()).unwrap();
        assert!(
            result.diff.contains("--- a/main.rs"),
            "got: {}",
            result.diff
        );
        assert!(
            result.diff.contains("+++ b/main.rs"),
            "got: {}",
            result.diff
        );
        assert!(result.diff.contains("-    old();"), "got: {}", result.diff);
        assert!(result.diff.contains("+    new();"), "got: {}", result.diff);
    }

    #[test]
    fn apply_result_diff_uses_one_based_hunk_headers() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("diff-lines.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let patch = make_patch(vec![PatchOperation::UpdateFile {
            path: "diff-lines.txt".to_string(),
            move_to: None,
            hunks: vec![simple_hunk("three", "THREE")],
        }]);

        let result = apply_patch(&patch, dir.path()).unwrap();
        assert!(
            result.diff.contains("@@ -3,1 +3,1 @@"),
            "got diff: {}",
            result.diff
        );
    }
}
