//! `apply_patch` (cwd / skills / channels), `create_channel`, ghost-commit
//! rollback, ACP text-bridge integration, and patch-DSL path
//! extraction & normalization.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::execution_context::ExecutionContext;

use super::blocked::{is_blocked_path, is_control_plane_write_denied};
use super::default_execution_context;
use crate::tool_handlers::{optional_str_param, require_str_param};

/// Apply a patch to a directory, returning a formatted result message.
fn apply_patch_to(
    args: &serde_json::Value,
    base_dir: &std::path::Path,
    label: &str,
) -> Result<String> {
    let patch = require_str_param(args, "patch")?;
    std::fs::create_dir_all(base_dir)?;
    match coding::apply_patch_to_dir(patch, base_dir) {
        Ok(result) => Ok(format_patch_success(
            &format!("{label} patch applied successfully."),
            &result,
        )),
        Err(e) => Ok(format!("Error applying {label} patch: {e}")),
    }
}

pub fn handle_apply_skill_patch(args: &serde_json::Value, config: &Config) -> Result<String> {
    let patch = require_str_param(args, "patch")?;
    let skills_dir = Config::skills_dir()?;
    if let Some(denial) = deny_skill_patch(patch, &skills_dir, config) {
        return Ok(denial);
    }
    apply_patch_to(args, &skills_dir, "Skill")
}

/// Guard for `apply_patch` with `target: "skills"`. Returns a denial message
/// when the patch must not be applied, `None` when it may proceed.
///
/// Fails closed: every path must be extractable, relative, made of plain
/// components (no `..`, no absolute, no `~`), and must not land inside a
/// bundled skill's directory — the compiled-in skills are Borg's own operating
/// instructions. The loader ignores on-disk SKILL.md shadows of bundled names
/// (the embedded copy wins), but the installed directory's supporting files —
/// notably executable `scripts/` — run from disk, so a patched copy still
/// changes agent behavior. User-authored skills remain freely editable. The
/// blocked-path and control-plane deny lists apply on top, same as the cwd
/// branch.
fn deny_skill_patch(
    patch_text: &str,
    skills_dir: &std::path::Path,
    config: &Config,
) -> Option<String> {
    let paths = guarded_patch_paths(patch_text).unwrap_or_default();
    if paths.is_empty() {
        // No determinable targets: nothing legitimate to write. Refuse rather
        // than hand an unvetted patch body to the applicator.
        return Some(
            "Access denied: could not determine target paths from the skill patch".to_string(),
        );
    }
    for raw_path in &paths {
        let path = std::path::Path::new(raw_path);
        let mut components = path.components();
        let first = match components.next() {
            Some(std::path::Component::Normal(c)) => c.to_string_lossy().into_owned(),
            // Absolute paths, `..`, `.`, or an empty path — anything that
            // could escape the skills dir or defeat the skill-name check.
            _ => {
                return Some(format!(
                    "Access denied: {raw_path} — skill patches must use plain paths \
                     relative to the skills directory (no absolute paths or '..')"
                ))
            }
        };
        if raw_path.starts_with('~')
            || components.any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Some(format!(
                "Access denied: {raw_path} — skill patches must use plain paths \
                 relative to the skills directory (no absolute paths or '..')"
            ));
        }
        if crate::skills::is_builtin_skill_name(&first) {
            return Some(format!(
                "Access denied: '{first}' is a built-in skill and cannot be modified. \
                 Create a new skill under a different name instead."
            ));
        }
        let resolved = skills_dir.join(path);
        if is_blocked_path(
            &resolved,
            &config.security.blocked_paths,
            &config.security.allowed_paths,
        ) {
            return Some(format!("Access denied: {raw_path} is in a blocked path"));
        }
        if is_control_plane_write_denied(&resolved) {
            return Some(format!(
                "Access denied: {raw_path} is a Borg control-plane file"
            ));
        }
    }
    reject_overlong_skill_description(patch_text)
}

/// Refuse a skill patch whose frontmatter `description:` exceeds the catalog
/// cap.
///
/// The catalog renders every skill's description through a truncating budget,
/// so anything past the cap is never seen by the model — it costs authoring
/// effort, and the routing signal that decides whether the skill gets picked
/// silently ends mid-sentence. Rejecting at the write path makes that visible
/// while the author can still fix it.
///
/// Scans added lines only: an unchanged over-length description in surrounding
/// context is somebody else's problem, and rejecting it would make an unrelated
/// edit to an old skill impossible.
///
/// Scoped to `SKILL.md` frontmatter. A `description:` line anywhere else in a
/// skill — a `references/*.md` documenting an API field, a YAML fixture under
/// `scripts/`, prose in the skill body — is not the catalog's routing signal,
/// and rejecting the whole patch over it would block edits that have nothing to
/// do with skill metadata.
fn reject_overlong_skill_description(patch_text: &str) -> Option<String> {
    // Tracks the file each hunk belongs to, plus how many `---` markers we've
    // passed inside it: the frontmatter block sits between the first and the
    // second, so a hunk that hasn't crossed the closing marker is still
    // (possibly) editing metadata. A hunk that touches only the description
    // line carries no markers at all, which is why the default is "inside".
    let mut in_skill_md = false;
    let mut markers_seen = 0usize;
    for line in patch_text.lines() {
        if let Some(path) = line
            .strip_prefix("*** Add File:")
            .or_else(|| line.strip_prefix("*** Update File:"))
            .or_else(|| line.strip_prefix("*** Delete File:"))
            .or_else(|| line.strip_prefix("*** Move to:"))
        {
            in_skill_md = std::path::Path::new(path.trim())
                .file_name()
                .is_some_and(|f| f == "SKILL.md");
            markers_seen = 0;
            continue;
        }
        if !in_skill_md {
            continue;
        }
        // Markers count whether added or carried as context.
        let body = line
            .strip_prefix(['+', '-', ' '])
            .unwrap_or(line)
            .trim_end();
        if body.trim() == "---" {
            markers_seen += 1;
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if markers_seen >= 2 {
            // Past the frontmatter: body prose the catalog never renders.
            continue;
        }
        let Some(value) = added.trim_start().strip_prefix("description:") else {
            continue;
        };
        // YAML scalars may be bare or quoted; the quotes aren't part of the
        // description the catalog renders.
        let value = value.trim().trim_matches(['"', '\''].as_ref());
        let length = value.chars().count();
        if length > crate::skills::MAX_SKILL_DESCRIPTION_CHARS {
            return Some(format!(
                "Rejected: skill description is {length} characters, over the \
                 {} character limit. Descriptions past the limit are truncated \
                 out of the skill catalog, so the routing signal is lost — \
                 shorten it to one line naming what the skill does and when to \
                 use it.",
                crate::skills::MAX_SKILL_DESCRIPTION_CHARS
            ));
        }
    }
    None
}

/// Unified apply_patch handler with `target` parameter.
/// Supports: cwd (default), skills, channels.
pub fn handle_apply_patch_unified(args: &serde_json::Value, config: &Config) -> Result<String> {
    let ctx = default_execution_context()?;
    handle_apply_patch_unified_with_context(args, config, &ctx)
}

pub fn handle_apply_patch_unified_with_context(
    args: &serde_json::Value,
    config: &Config,
    ctx: &ExecutionContext,
) -> Result<String> {
    // Validate patch param exists before dispatching
    let _patch = require_str_param(args, "patch")?;
    let target = optional_str_param(args, "target").unwrap_or("cwd");

    match target {
        "cwd" => handle_apply_patch_with_context(args, config, ctx),
        "skills" => handle_apply_skill_patch(args, config),
        "channels" => handle_create_channel(args),
        other => Ok(format!(
            "Unknown target: {other}. Use: cwd, skills, channels."
        )),
    }
}

// IMPORTANT: apply_patch supports absolute and ~/... paths for cross-directory
// file creation. Security boundary is is_blocked_path(), NOT base_dir containment.
// Do not re-add a CWD-only restriction.
pub fn handle_apply_patch(args: &serde_json::Value, config: &Config) -> Result<String> {
    let ctx = default_execution_context()?;
    handle_apply_patch_with_context(args, config, &ctx)
}

pub fn handle_apply_patch_with_context(
    args: &serde_json::Value,
    config: &Config,
    ctx: &ExecutionContext,
) -> Result<String> {
    let patch_text = require_str_param(args, "patch")?;
    let cwd = ctx.workspace_cwd();

    if let Some(bridge) = ctx.text_file_bridge() {
        if bridge.supports_read_text_file() && bridge.supports_write_text_file() {
            return handle_apply_patch_via_text_bridge(patch_text, config, ctx, bridge.as_ref());
        }
    }

    let paths = guarded_patch_paths(patch_text).unwrap_or_default();
    for raw_path in &paths {
        let resolved = ctx.resolve_path_for_write(raw_path)?;
        if is_blocked_path(
            &resolved,
            &config.security.blocked_paths,
            &config.security.allowed_paths,
        ) {
            return Ok(format!("Access denied: {raw_path} is in a blocked path"));
        }
        if is_control_plane_write_denied(&resolved) {
            return Ok(format!(
                "Access denied: {raw_path} is a Borg control-plane file"
            ));
        }
    }

    let has_non_relative = patch_has_absolute_paths(patch_text);

    let snapshot = ghost_snapshot_for(cwd);
    let outcome: std::result::Result<coding::ApplyPatchResult, anyhow::Error> = if has_non_relative
    {
        // Normalize all paths to absolute so we can use "/" as base_dir
        let normalized = normalize_patch_paths(patch_text, cwd);
        coding::apply_patch_to_dir(&normalized, std::path::Path::new("/"))
    } else {
        // Fast path: all relative paths, use CWD as base_dir.
        coding::apply_patch_to_dir(patch_text, cwd)
    };
    match outcome {
        Ok(result) => {
            let mut msg = format_patch_success("Patch applied successfully.", &result);
            if !has_non_relative {
                // Relative patch paths resolved against the workspace cwd (not
                // the process cwd). Surface the base so a worktree/cwd mismatch
                // — files silently landing in the wrong checkout — is visible
                // rather than hidden behind bare relative names.
                msg.push_str(&format!("\n\nResolved against: {}", cwd.display()));
            }
            Ok(msg)
        }
        Err(e) => {
            // Ghost-commit rollback: if the cwd is inside a git repo
            // and we snapshotted before the failed patch attempt, restore
            // tracked files and clean up post-snapshot creations so a
            // multi-file patch that fails partway doesn't leave the working
            // tree half-applied. Best-effort outside a repo.
            if let Some(snapshot) = snapshot {
                if let Err(restore_err) = restore_ghost_snapshot_sync(&snapshot) {
                    tracing::warn!(
                        repo = %snapshot.repo_root.display(),
                        error = %restore_err,
                        "ghost rollback after apply_patch failure failed"
                    );
                }
            }
            Ok(format!("Error applying patch: {e}"))
        }
    }
}

/// Snapshot of the working tree captured before a disk-backed apply_patch.
/// Used purely to roll back on failure; on success we just drop it (Git's
/// GC reclaims the unreferenced commit).
struct DiskGhostSnapshot {
    repo_root: std::path::PathBuf,
    commit_id: String,
    /// Untracked files that already existed before the snapshot — restore
    /// must not delete these even though they aren't in the snapshot tree.
    preexisting_untracked: Vec<std::path::PathBuf>,
}

/// Best-effort: build a ghost snapshot if `cwd` lives inside a git repo,
/// otherwise return None. Sync wrapper using `std::process::Command` so the
/// existing sync apply_patch dispatch doesn't have to migrate to async.
fn ghost_snapshot_for(cwd: &std::path::Path) -> Option<DiskGhostSnapshot> {
    let repo_root = crate::git::find_repo_root(cwd)?;
    match build_ghost_snapshot_sync(&repo_root) {
        Ok(snapshot) => Some(snapshot),
        Err(e) => {
            tracing::debug!(
                repo = %repo_root.display(),
                error = %e,
                "ghost snapshot skipped; proceeding without rollback safety net"
            );
            None
        }
    }
}

fn build_ghost_snapshot_sync(repo_root: &std::path::Path) -> Result<DiskGhostSnapshot> {
    let has_head = run_git_sync(repo_root, &["rev-parse", "HEAD"], &[]).is_ok();
    let tmp_index = tempfile::NamedTempFile::new()?;
    let tmp_path = tmp_index.path().to_string_lossy().to_string();
    // Index env plus filter-neutralizing config: `add`/`read-tree` would
    // otherwise run repo-configured `filter.<name>.clean`/`.process` drivers
    // against an untrusted tree.
    let mut index_env = vec![("GIT_INDEX_FILE".to_string(), tmp_path)];
    index_env.extend(filter_neutralizing_env_sync(repo_root));
    let untracked = run_git_sync(
        repo_root,
        &["ls-files", "--others", "--exclude-standard"],
        &[],
    )
    .unwrap_or_default();
    let preexisting_untracked: Vec<std::path::PathBuf> = untracked
        .lines()
        .filter(|l| !l.is_empty())
        .map(std::path::PathBuf::from)
        .collect();
    if has_head {
        run_git_sync(repo_root, &["read-tree", "HEAD"], &index_env)?;
    }
    run_git_sync(repo_root, &["add", "--all"], &index_env)?;
    let tree_id = run_git_sync(repo_root, &["write-tree"], &index_env)?;
    let mut commit_args: Vec<&str> =
        vec!["commit-tree", &tree_id, "-m", "borg disk-apply-patch ghost"];
    let head_parent;
    if has_head {
        head_parent = run_git_sync(repo_root, &["rev-parse", "HEAD"], &[])?;
        commit_args.push("-p");
        commit_args.push(head_parent.as_str());
    }
    let commit_id = run_git_sync(repo_root, &commit_args, &[])?;
    Ok(DiskGhostSnapshot {
        repo_root: repo_root.to_path_buf(),
        commit_id,
        preexisting_untracked,
    })
}

fn restore_ghost_snapshot_sync(snapshot: &DiskGhostSnapshot) -> Result<()> {
    // Worktree checkout runs `filter.<name>.smudge`/`.process` drivers.
    let filter_env = filter_neutralizing_env_sync(&snapshot.repo_root);
    // Not an early return: `git restore` is not transactional, and the
    // post-snapshot sweep below is the other half of the rollback. Skipping it
    // on a partial restore would leave every agent-created file behind too.
    let restored = run_git_sync(
        &snapshot.repo_root,
        &[
            "restore",
            "--source",
            &snapshot.commit_id,
            "--worktree",
            "--",
            ".",
        ],
        &filter_env,
    );
    let current = run_git_sync(
        &snapshot.repo_root,
        &["ls-files", "--others", "--exclude-standard"],
        &[],
    )
    .unwrap_or_default();
    for line in current.lines() {
        if line.is_empty() {
            continue;
        }
        let path = std::path::PathBuf::from(line);
        if snapshot.preexisting_untracked.contains(&path) {
            continue;
        }
        let full = snapshot.repo_root.join(&path);
        if full.exists() {
            if let Err(e) = std::fs::remove_file(&full) {
                tracing::warn!(
                    path = %full.display(),
                    error = %e,
                    "ghost rollback: failed to remove post-snapshot file"
                );
            }
        }
    }
    restored.context(
        "ghost rollback failed partway: the working tree may hold a mix of \
         snapshot and post-patch content",
    )?;
    Ok(())
}

/// Discover repo-configured executable content filters and return the
/// `GIT_CONFIG_*` env that neutralizes them. Best-effort: discovery failures
/// yield no overrides (the hardening prelude still applies).
fn filter_neutralizing_env_sync(repo_root: &std::path::Path) -> Vec<(String, String)> {
    let mut discovery = std::process::Command::new("git");
    discovery
        .args(crate::git::filter_discovery_argv())
        .current_dir(repo_root);
    crate::git::make_git_non_interactive_sync(&mut discovery);
    let output = discovery.output();
    match output {
        // `--get-regexp` exits 1 when nothing matches — the common case.
        Ok(o) if o.status.success() || o.status.code() == Some(1) => {
            crate::git::neutralizing_env_from_stdout(&String::from_utf8_lossy(&o.stdout))
        }
        Ok(o) => {
            tracing::warn!(
                "git filter-driver discovery failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!("git filter-driver discovery failed to spawn: {e}");
            Vec::new()
        }
    }
}

fn run_git_sync(cwd: &std::path::Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    // Prepend the hardening prelude so a repository's own `core.fsmonitor`/hooks
    // can't execute while we operate inside its tree.
    let mut argv: Vec<&str> = crate::git::hardened_prelude().to_vec();
    argv.extend_from_slice(args);
    let mut cmd = std::process::Command::new("git");
    cmd.args(&argv).current_dir(cwd);
    // This path blocks with no timeout, so an interactive prompt would wedge
    // the tool call outright rather than merely delaying it.
    crate::git::make_git_non_interactive_sync(&mut cmd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd
        .output()
        .with_context(|| format!("spawn `git {}`", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn handle_apply_patch_via_text_bridge(
    patch_text: &str,
    config: &Config,
    ctx: &ExecutionContext,
    bridge: &dyn crate::execution_context::TextFileBridge,
) -> Result<String> {
    let mut resolved_paths = HashMap::new();
    for raw_path in guarded_patch_paths(patch_text).unwrap_or_default() {
        let resolved = ctx.resolve_path_for_write(&raw_path)?;
        if is_blocked_path(
            &resolved,
            &config.security.blocked_paths,
            &config.security.allowed_paths,
        ) {
            return Ok(format!("Access denied: {raw_path} is in a blocked path"));
        }
        if is_control_plane_write_denied(&resolved) {
            return Ok(format!(
                "Access denied: {raw_path} is a Borg control-plane file"
            ));
        }
        let rel = resolved
            .strip_prefix(ctx.workspace_root())
            .map_err(|_| anyhow::anyhow!("path {} escaped workspace root", resolved.display()))?
            .to_path_buf();
        resolved_paths.insert(raw_path, (resolved, rel));
    }

    let tempdir = tempfile::tempdir()?;
    let mut originals: HashMap<std::path::PathBuf, Option<String>> = HashMap::new();
    let mut staged_paths = HashSet::new();
    for (resolved, rel) in resolved_paths.values() {
        if !staged_paths.insert(rel.clone()) {
            continue;
        }
        match bridge.read_text_file(resolved) {
            Ok(content) => {
                let staged = tempdir.path().join(rel);
                if let Some(parent) = staged.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&staged, content.as_bytes())?;
                originals.insert(resolved.clone(), Some(content));
            }
            Err(err) => {
                if resolved.exists() {
                    return Ok(format!(
                        "ACP fs read_text_file failed for {}: {err}",
                        resolved.display()
                    ));
                }
                originals.insert(resolved.clone(), None);
            }
        }
    }

    let normalized = normalize_patch_paths_to_workspace_root(patch_text, ctx)?;
    let result = match coding::apply_patch_to_dir(&normalized, tempdir.path()) {
        Ok(result) => result,
        Err(e) => return Ok(format!("Error applying patch: {e}")),
    };

    let mut rollback_actions = Vec::new();
    let mut write_targets: Vec<&str> = result
        .added
        .iter()
        .map(String::as_str)
        .chain(result.modified.iter().map(String::as_str))
        .collect();
    for (_, dest) in &result.moved {
        write_targets.push(dest.as_str());
    }

    let mut staged_write_targets = HashSet::new();
    for rel in write_targets {
        if !staged_write_targets.insert(rel) {
            continue;
        }
        let staged = tempdir.path().join(rel);
        let content = match std::fs::read_to_string(&staged) {
            Ok(content) => content,
            Err(e) => {
                rollback_bridge_actions(bridge, &rollback_actions);
                return Ok(format!("Error reading staged patch result for {rel}: {e}"));
            }
        };
        let resolved = ctx.workspace_root().join(rel);
        let original = originals.get(&resolved).cloned().flatten();
        if let Err(e) = bridge.write_text_file(&resolved, &content) {
            rollback_bridge_actions(bridge, &rollback_actions);
            return Ok(format!(
                "ACP fs write_text_file failed for {}: {e}",
                resolved.display()
            ));
        }
        match original {
            Some(original) => rollback_actions.push(BridgeRollbackAction::RestoreText {
                path: resolved,
                content: original,
            }),
            None => rollback_actions.push(BridgeRollbackAction::DeleteFile { path: resolved }),
        }
    }

    for rel in &result.deleted {
        let resolved = ctx.workspace_root().join(rel);
        let original = originals.get(&resolved).cloned().flatten();
        if let Err(e) = remove_bridge_backed_file(&resolved) {
            rollback_bridge_actions(bridge, &rollback_actions);
            return Ok(format!(
                "ACP fs delete failed for {}: {e}",
                resolved.display()
            ));
        }
        if let Some(original) = original {
            rollback_actions.push(BridgeRollbackAction::RestoreText {
                path: resolved,
                content: original,
            });
        }
    }

    let mut moved_sources = HashSet::new();
    for (src, _) in &result.moved {
        if !moved_sources.insert(src.as_str()) {
            continue;
        }
        let resolved = ctx.workspace_root().join(src);
        let original = originals.get(&resolved).cloned().flatten();
        if let Err(e) = remove_bridge_backed_file(&resolved) {
            rollback_bridge_actions(bridge, &rollback_actions);
            return Ok(format!(
                "ACP fs move cleanup failed for {}: {e}",
                resolved.display()
            ));
        }
        if let Some(original) = original {
            rollback_actions.push(BridgeRollbackAction::RestoreText {
                path: resolved,
                content: original,
            });
        }
    }

    let mut output = format_patch_success("Patch applied successfully.", &result);
    if !result.deleted.is_empty() || !result.moved.is_empty() {
        output.push_str(&bridge_unlink_advisory(&result));
    }
    Ok(output)
}

/// ACP defines no `fs/deleteFile` method, so the bridge-backed apply_patch
/// path unlinks deleted/moved-source files directly from the workspace tree
/// while their content was last read/written through the IDE buffer. If the
/// IDE has those files open with unsaved edits, saving the buffer can
/// silently undo the delete. Make the divergence visible to the user so they
/// can close affected buffers — without erroring on legitimate refactors.
fn bridge_unlink_advisory(result: &coding::ApplyPatchResult) -> String {
    let mut paths: Vec<&str> = result
        .deleted
        .iter()
        .map(String::as_str)
        .chain(result.moved.iter().map(|(src, _)| src.as_str()))
        .collect();
    paths.sort();
    paths.dedup();
    format!(
        "\n\nNote: ACP has no fs.deleteFile capability; the following paths were \
unlinked directly from the workspace tree: {}. Close any open editor buffers \
for these files to avoid the on-save handler recreating them.",
        paths.join(", ")
    )
}

enum BridgeRollbackAction {
    RestoreText {
        path: std::path::PathBuf,
        content: String,
    },
    DeleteFile {
        path: std::path::PathBuf,
    },
}

fn rollback_bridge_actions(
    bridge: &dyn crate::execution_context::TextFileBridge,
    actions: &[BridgeRollbackAction],
) {
    for action in actions.iter().rev() {
        match action {
            BridgeRollbackAction::RestoreText { path, content } => {
                if let Err(e) = bridge.write_text_file(path, content) {
                    tracing::warn!(path = %path.display(), error = %e, "ACP fs rollback failed");
                }
            }
            BridgeRollbackAction::DeleteFile { path } => {
                if let Err(e) = remove_bridge_backed_file(path) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ACP fs rollback cleanup failed"
                    );
                }
            }
        }
    }
}

fn remove_bridge_backed_file(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn format_patch_success(prefix: &str, result: &coding::ApplyPatchResult) -> String {
    let mut parts = vec![prefix.to_string()];
    let summary = result.format_summary();
    if !summary.is_empty() {
        parts.push(summary);
    }
    if !result.diff.trim().is_empty() {
        parts.push(result.diff.clone());
    }
    parts.join("\n\n")
}

/// Check whether the patch text contains any absolute or tilde-prefixed file paths.
fn patch_has_absolute_paths(patch_text: &str) -> bool {
    for line in patch_text.lines() {
        for path in extract_paths_from_line(line) {
            if path.starts_with('/') || path.starts_with('~') {
                return true;
            }
        }
    }
    false
}

/// Paths a patch would write, for the pre-write security guard.
///
/// Derived from the parser rather than by scanning the text for DSL markers:
/// the applicator also accepts unified diffs (`parse_patch_with_mode` routes
/// them to `parse_unified_diff`), whose headers carry no DSL marker. A marker
/// scan therefore reports "touches nothing" for such a patch and waves it past
/// `is_blocked_path` / `is_control_plane_write_denied` entirely.
///
/// `None` means the patch does not parse — it writes nothing, so callers hand
/// it to the applicator for the real diagnostic rather than reporting a
/// security denial. Guarding is never skipped for a patch that can write.
fn guarded_patch_paths(patch_text: &str) -> Option<Vec<String>> {
    coding::patch_target_paths(patch_text).ok()
}

/// Patch DSL marker prefixes (each ends with a single trailing space before the path).
///
/// Shared with [`crate::tool::effects`], whose concurrency planner must reserve
/// exactly the paths this handler will write — a marker known here but not
/// there reads as "touches no files" and lets a conflicting write run in
/// parallel.
pub(crate) const PATCH_DSL_MARKERS: &[&str] = &[
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move File: ",
    "*** Move to: ",
    "*** Move To: ",
];

/// Extract a file path from a patch DSL marker line, if present.
fn extract_path_from_line(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("*** Move File: ") {
        let (src, _) = rest.split_once("->")?;
        let path = src.trim();
        if !path.is_empty() {
            return Some(path);
        }
    }
    for prefix in PATCH_DSL_MARKERS {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

fn extract_paths_from_line(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("*** Move File: ") {
        if let Some((src, dst)) = rest.split_once("->") {
            let src = src.trim();
            let dst = dst.trim();
            let mut paths = Vec::new();
            if !src.is_empty() {
                paths.push(src);
            }
            if !dst.is_empty() {
                paths.push(dst);
            }
            return paths;
        }
        return Vec::new();
    }

    extract_path_from_line(line).into_iter().collect()
}

/// Normalize patch file paths so apply_patch_to_dir can use "/" as base_dir.
/// All paths become absolute: tilde-expanded, relative paths prepended with CWD.
/// Rewrites path-bearing patch markers, including standalone move operations.
fn normalize_patch_paths(patch_text: &str, cwd: &std::path::Path) -> String {
    let mut output = String::with_capacity(patch_text.len());
    for (i, line) in patch_text.lines().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        if let Some(normalized_move) = normalize_move_file_line(line, cwd) {
            output.push_str(&normalized_move);
        } else if let Some((prefix, path)) = split_marker_line(line) {
            let expanded = crate::util::unix_ext::expand_tilde(path);
            let absolute = if std::path::Path::new(&expanded).is_absolute() {
                expanded
            } else {
                cwd.join(&expanded).to_string_lossy().to_string()
            };
            output.push_str(prefix);
            output.push_str(&absolute);
        } else {
            output.push_str(line);
        }
    }
    if patch_text.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn normalize_patch_paths_to_workspace_root(
    patch_text: &str,
    ctx: &ExecutionContext,
) -> Result<String> {
    let mut output = String::with_capacity(patch_text.len());
    for (i, line) in patch_text.lines().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        if let Some(normalized_move) = normalize_move_file_line_to_workspace_root(line, ctx)? {
            output.push_str(&normalized_move);
        } else if let Some((prefix, path)) = split_marker_line(line) {
            let resolved = ctx.resolve_path_maybe_missing(path)?;
            let rel = resolved.strip_prefix(ctx.workspace_root()).map_err(|_| {
                anyhow::anyhow!("path {} escaped workspace root", resolved.display())
            })?;
            output.push_str(prefix);
            output.push_str(&rel.to_string_lossy());
        } else {
            output.push_str(line);
        }
    }
    if patch_text.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

fn normalize_move_file_line_to_workspace_root(
    line: &str,
    ctx: &ExecutionContext,
) -> Result<Option<String>> {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix("*** Move File: ") else {
        return Ok(None);
    };
    let (src, dst) = rest
        .split_once("->")
        .ok_or_else(|| anyhow::anyhow!("invalid Move File patch line: {line}"))?;
    let src = normalize_patch_path_fragment_to_workspace_root(src.trim(), ctx)?;
    let dst = normalize_patch_path_fragment_to_workspace_root(dst.trim(), ctx)?;
    Ok(Some(format!("*** Move File: {src} -> {dst}")))
}

fn normalize_patch_path_fragment_to_workspace_root(
    path: &str,
    ctx: &ExecutionContext,
) -> Result<String> {
    let resolved = ctx.resolve_path_maybe_missing(path)?;
    let rel = resolved
        .strip_prefix(ctx.workspace_root())
        .map_err(|_| anyhow::anyhow!("path {} escaped workspace root", resolved.display()))?;
    Ok(rel.to_string_lossy().to_string())
}

/// Split a patch DSL marker line into (prefix_with_space, path), if it's a path line.
fn split_marker_line(line: &str) -> Option<(&str, &str)> {
    for prefix in PATCH_DSL_MARKERS {
        if let Some(idx) = line.find(prefix) {
            let prefix_end = idx + prefix.len();
            let path = line[prefix_end..].trim();
            if !path.is_empty() {
                return Some((&line[..prefix_end], path));
            }
        }
    }
    None
}

fn normalize_move_file_line(line: &str, cwd: &std::path::Path) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("*** Move File: ")?;
    let (src, dst) = rest.split_once("->")?;
    let src = normalize_patch_path_fragment(src.trim(), cwd);
    let dst = normalize_patch_path_fragment(dst.trim(), cwd);
    Some(format!("*** Move File: {src} -> {dst}"))
}

fn normalize_patch_path_fragment(path: &str, cwd: &std::path::Path) -> String {
    let expanded = crate::util::unix_ext::expand_tilde(path);
    if std::path::Path::new(&expanded).is_absolute() {
        expanded
    } else {
        cwd.join(&expanded).to_string_lossy().to_string()
    }
}

pub fn handle_create_channel(args: &serde_json::Value) -> Result<String> {
    apply_patch_to(args, &Config::channels_dir()?, "Channel")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeTextBridge {
        reads: Mutex<HashMap<std::path::PathBuf, String>>,
        writes: Mutex<HashMap<std::path::PathBuf, String>>,
    }

    impl crate::execution_context::TextFileBridge for FakeTextBridge {
        fn supports_read_text_file(&self) -> bool {
            true
        }

        fn supports_write_text_file(&self) -> bool {
            true
        }

        fn read_text_file(&self, path: &std::path::Path) -> Result<String> {
            self.reads
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing fake read buffer"))
        }

        fn write_text_file(&self, path: &std::path::Path, content: &str) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), content.to_string());
            Ok(())
        }
    }

    #[test]
    fn apply_patch_with_bridge_writes_updated_full_buffer() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("demo.txt");
        std::fs::write(&file, "disk\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(file.clone(), "old line\n".to_string());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge.clone()));
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-old line\n+new line\n*** End Patch",
            file.to_string_lossy()
        );
        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();
        assert!(result.contains("Patch applied successfully"));
        let writes = bridge.writes.lock().unwrap();
        assert_eq!(writes.get(&file).map(String::as_str), Some("new line\n"));
    }

    #[test]
    fn apply_patch_with_bridge_supports_delete_file() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("delete-me.txt");
        std::fs::write(&file, "disk\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(file.clone(), "buffered\ntext\n".to_string());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge));
        let patch = format!(
            "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
            file.to_string_lossy()
        );

        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();

        assert!(result.contains("Patch applied successfully"));
        assert!(!file.exists(), "delete should remove the file from disk");
    }

    #[test]
    fn apply_patch_with_bridge_supports_move_to_with_unsaved_buffer_content() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src.txt");
        let dst = root.join("dest.txt");
        std::fs::write(&src, "disk\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(src.clone(), "buffered\ntext\n".to_string());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge.clone()));
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n*** End Patch",
            src.to_string_lossy(),
            dst.to_string_lossy()
        );

        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();

        assert!(result.contains("Patch applied successfully"));
        assert!(!src.exists(), "move should remove the original path");
        let writes = bridge.writes.lock().unwrap();
        assert_eq!(
            writes.get(&dst).map(String::as_str),
            Some("buffered\ntext\n")
        );
    }

    #[test]
    fn apply_patch_with_bridge_supports_move_file_syntax() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src.txt");
        let dst = root.join("nested/dest.txt");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&src, "disk\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(src.clone(), "buffered\ntext\n".to_string());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge.clone()));
        let patch = format!(
            "*** Begin Patch\n*** Move File: {} -> {}\n*** End Patch",
            src.to_string_lossy(),
            dst.to_string_lossy()
        );

        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();

        assert!(result.contains("Patch applied successfully"));
        assert!(!src.exists(), "move should remove the source file");
        let writes = bridge.writes.lock().unwrap();
        assert_eq!(
            writes.get(&dst).map(String::as_str),
            Some("buffered\ntext\n")
        );
    }

    #[test]
    fn apply_patch_with_bridge_emits_advisory_when_unlinking() {
        // Regression: bridge-backed delete/move calls
        // `std::fs::remove_file` directly (ACP has no `fs/deleteFile`), so
        // the IDE's open buffer for the deleted path is left dangling.
        // Make the divergence visible to the user via an advisory in the
        // tool result, so they can close the affected buffers before the
        // on-save handler resurrects the file.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let to_delete = root.join("delete-me.txt");
        let move_src = root.join("src.txt");
        let move_dst = root.join("dest.txt");
        std::fs::write(&to_delete, "x").unwrap();
        std::fs::write(&move_src, "y").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(to_delete.clone(), "buffered-delete\n".into());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(move_src.clone(), "buffered-move\n".into());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge));
        let patch = format!(
            "*** Begin Patch\n*** Delete File: {}\n*** Move File: {} -> {}\n*** End Patch",
            to_delete.to_string_lossy(),
            move_src.to_string_lossy(),
            move_dst.to_string_lossy()
        );

        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();

        assert!(result.contains("Patch applied successfully"));
        assert!(
            result.contains("ACP has no fs.deleteFile capability"),
            "advisory missing from output: {result}"
        );
        assert!(
            result.contains("delete-me.txt"),
            "delete path missing from advisory: {result}"
        );
        assert!(
            result.contains("src.txt"),
            "move source missing from advisory: {result}"
        );
        assert!(!to_delete.exists());
        assert!(!move_src.exists());
    }

    #[test]
    fn apply_patch_with_bridge_omits_advisory_when_no_unlink() {
        // The advisory only fires for delete/move — pure adds/updates must
        // not pollute the result with unrelated noise.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("demo.txt");
        std::fs::write(&file, "old\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(file.clone(), "old\n".into());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge));
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
            file.to_string_lossy()
        );

        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();

        assert!(result.contains("Patch applied successfully"));
        assert!(
            !result.contains("ACP has no fs.deleteFile capability"),
            "advisory must not fire for pure update: {result}"
        );
    }

    #[test]
    fn ghost_snapshot_round_trip_restores_tracked_files_in_git_repo() {
        // Regression: validate that a snapshot taken before a disk-backed
        // apply_patch can faithfully restore the working tree if the apply
        // fails partway. `coding::apply_patch_to_dir` already preflights
        // simple hunks, but I/O-level failures (disk full, EPERM, races)
        // can still leave the tree half-written — that's where the
        // ghost-commit safety net earns its keep. We exercise the
        // build/restore plumbing directly here: snapshot a clean repo,
        // mutate two files plus add a new one, restore from the snapshot,
        // and assert everything is back to pre-snapshot state.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git init");
        for (key, value) in [("user.email", "ci@borg.test"), ("user.name", "ci")] {
            std::process::Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(&repo)
                .output()
                .expect("git config");
        }
        let file_a = repo.join("a.txt");
        let file_b = repo.join("b.txt");
        std::fs::write(&file_a, "v1\n").unwrap();
        std::fs::write(&file_b, "ok\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&repo)
            .output()
            .expect("git commit");

        let snapshot = ghost_snapshot_for(&repo).expect("snapshot inside git repo");

        // Mutate the working tree as if a half-applied patch had run.
        std::fs::write(&file_a, "v2\n").unwrap();
        std::fs::write(&file_b, "WRECKED\n").unwrap();
        std::fs::write(repo.join("orphan.txt"), "leak\n").unwrap();

        restore_ghost_snapshot_sync(&snapshot).expect("rollback");

        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "v1\n");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "ok\n");
        assert!(
            !repo.join("orphan.txt").exists(),
            "post-snapshot creation must be removed by rollback"
        );
    }

    #[test]
    fn ghost_snapshot_returns_none_outside_git_repo() {
        // Outside a git repo, the disk apply_patch path runs without a
        // safety net (acceptable per plan: "fall through to existing
        // direct path"). Confirm the helper does not error or panic.
        let dir = tempfile::tempdir().unwrap();
        assert!(ghost_snapshot_for(dir.path()).is_none());
    }

    #[test]
    fn apply_patch_relative_success_reports_resolved_cwd() {
        // Relative patch paths resolve against the workspace cwd, which can
        // differ from the process cwd in a worktree/remote session. The success
        // message must surface the resolved base so files silently landing in
        // the wrong checkout become visible.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).unwrap();
        let patch = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch";
        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "got: {result}"
        );
        assert!(
            result.contains(root.to_string_lossy().as_ref()),
            "resolved cwd missing from: {result}"
        );
        assert!(
            root.join("created.txt").exists(),
            "relative path should have been written under the workspace cwd"
        );
    }

    #[test]
    fn unified_diff_cannot_write_a_blocked_path() {
        // End-to-end regression for the guard bypass: the handler must deny a
        // unified diff targeting a blocked path, and must not create the file.
        // Before the fix this returned "Patch applied successfully" and wrote
        // `.env` — inside $HOME that reaches `.ssh/`, `.aws/`, and hooks.json.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).unwrap();
        let patch = "\
diff --git a/.env b/.env
--- /dev/null
+++ b/.env
@@ -0,0 +1 @@
+SECRET=leaked
";
        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();
        assert!(
            result.contains("Access denied") && result.contains("blocked path"),
            "unified diff to a blocked path must be denied; got: {result}"
        );
        assert!(
            !root.join(".env").exists(),
            "denied patch must not have written the file"
        );
    }

    #[test]
    fn unified_diff_to_an_ordinary_path_still_applies() {
        // The guard must not turn into a blanket refusal of unified diffs —
        // this is the half that fails if the fix is written too broadly.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).unwrap();
        let patch = "\
diff --git a/notes.txt b/notes.txt
--- /dev/null
+++ b/notes.txt
@@ -0,0 +1 @@
+hello
";
        let result =
            handle_apply_patch_with_context(&json!({"patch": patch}), &config, &ctx).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "ordinary unified diff must still apply; got: {result}"
        );
        assert!(root.join("notes.txt").exists());
    }

    #[test]
    fn apply_patch_unified_unknown_target() {
        let config = Config::default();
        let args = json!({"patch": "*** Begin Patch\n*** End Patch", "target": "invalid"});
        let result = handle_apply_patch_unified(&args, &config).unwrap();
        assert!(result.contains("Unknown target"));
    }

    #[test]
    fn apply_patch_unified_missing_patch() {
        let config = Config::default();
        let args = json!({"target": "cwd"});
        let result = handle_apply_patch_unified(&args, &config);
        assert!(result.is_err());
    }

    #[test]
    fn apply_patch_unified_default_target_is_cwd() {
        let config = Config::default();
        // Omitting `target` must behave exactly like an explicit "cwd" target,
        // and must NOT be treated as an unknown target.
        let defaulted = handle_apply_patch_unified(
            &json!({"patch": "*** Begin Patch\n*** End Patch"}),
            &config,
        )
        .unwrap();
        let explicit_cwd = handle_apply_patch_unified(
            &json!({"patch": "*** Begin Patch\n*** End Patch", "target": "cwd"}),
            &config,
        )
        .unwrap();
        assert_eq!(
            defaulted, explicit_cwd,
            "missing target should default to cwd"
        );
        assert!(
            !defaulted.contains("Unknown target"),
            "default target must be a known target, got: {defaulted}"
        );
    }

    #[test]
    fn apply_skill_patch_missing_patch_param() {
        let args = json!({});
        let result = handle_apply_skill_patch(&args, &Config::default());
        assert!(result.is_err(), "should error on missing patch param");
    }

    // ── Skill-patch guard tests ──

    fn skill_patch_args(path: &str) -> serde_json::Value {
        json!({
            "patch": format!("*** Begin Patch\n*** Add File: {path}\n+content\n*** End Patch")
        })
    }

    #[test]
    fn skill_patch_refuses_builtin_skill() {
        let config = Config::default();
        // `scheduler` is a MANDATORY_SKILLS entry; every bundled name must
        // refuse. `Scheduler` pins case-insensitivity: the default macOS
        // filesystem maps it onto the same on-disk directory.
        for name in ["scheduler", "skill-creator", "Scheduler"] {
            let result =
                handle_apply_skill_patch(&skill_patch_args(&format!("{name}/SKILL.md")), &config)
                    .unwrap();
            assert!(
                result.contains("Access denied") && result.contains("built-in"),
                "bundled skill '{name}' must be refused, got: {result}"
            );
        }
    }

    #[test]
    fn skill_patch_refuses_escape_paths() {
        let config = Config::default();
        for path in [
            "../outside.txt",
            "/etc/passwd",
            "~/other/file.txt",
            "sub/../../escape.txt",
        ] {
            let result = handle_apply_skill_patch(&skill_patch_args(path), &config).unwrap();
            assert!(
                result.contains("Access denied"),
                "path '{path}' must be refused, got: {result}"
            );
        }
    }

    #[test]
    fn skill_patch_refuses_when_no_paths_extractable() {
        let config = Config::default();
        let args = json!({"patch": "*** Begin Patch\n*** End Patch"});
        let result = handle_apply_skill_patch(&args, &config).unwrap();
        assert!(
            result.contains("Access denied"),
            "pathless patch must fail closed, got: {result}"
        );
    }

    #[test]
    fn deny_skill_patch_allows_new_user_skill() {
        // Guard-only check (no filesystem write): a fresh user skill under a
        // non-bundled name passes validation.
        let config = Config::default();
        let patch = "*** Begin Patch\n*** Add File: my-custom-skill/SKILL.md\n+body\n*** End Patch";
        let denial = deny_skill_patch(patch, std::path::Path::new("/tmp/skills"), &config);
        assert!(
            denial.is_none(),
            "new user skill must be allowed, got: {denial:?}"
        );
    }

    #[test]
    fn deny_skill_patch_refuses_blocked_path_component() {
        // `.env` is in the default blocked list; a skill patch must not be a
        // side door for writing into blocked names inside the skills dir.
        let config = Config::default();
        let patch = "*** Begin Patch\n*** Add File: my-skill/.env\n+SECRET=1\n*** End Patch";
        let denial = deny_skill_patch(patch, std::path::Path::new("/tmp/skills"), &config);
        assert!(
            denial.is_some_and(|d| d.contains("blocked path")),
            "blocked-path component must be refused"
        );
    }

    /// A description past the catalog cap is truncated away before any model
    /// reads it, so writing one produces a skill whose routing signal ends
    /// mid-sentence. Refuse it while the author can still fix it.
    #[test]
    fn deny_skill_patch_refuses_description_past_the_catalog_cap() {
        let config = Config::default();
        let overlong = "x".repeat(crate::skills::MAX_SKILL_DESCRIPTION_CHARS + 1);
        let patch = format!(
            "*** Begin Patch\n*** Add File: my-skill/SKILL.md\n+---\n+name: my-skill\n\
             +description: {overlong}\n+---\n+body\n*** End Patch"
        );
        let denial = deny_skill_patch(&patch, std::path::Path::new("/tmp/skills"), &config);
        assert!(
            denial.is_some_and(|d| d.contains("over the")
                && d.contains(&crate::skills::MAX_SKILL_DESCRIPTION_CHARS.to_string())),
            "over-length description must be refused with the limit named"
        );
    }

    /// The guard reads added lines only. Quoted scalars are unwrapped before
    /// measuring (the quotes aren't part of what the catalog renders), and a
    /// description exactly at the cap is legal — an off-by-one here would
    /// reject valid skills.
    #[test]
    fn deny_skill_patch_allows_description_at_the_cap_and_ignores_context() {
        let config = Config::default();
        let exact = "x".repeat(crate::skills::MAX_SKILL_DESCRIPTION_CHARS);
        let at_cap = format!(
            "*** Begin Patch\n*** Add File: my-skill/SKILL.md\n+description: \"{exact}\"\n\
             *** End Patch"
        );
        assert!(
            deny_skill_patch(&at_cap, std::path::Path::new("/tmp/skills"), &config).is_none(),
            "a description exactly at the cap must be allowed"
        );

        // An untouched over-length description in surrounding context belongs
        // to an earlier edit; rejecting it would make unrelated fixes to old
        // skills impossible.
        let overlong = "x".repeat(crate::skills::MAX_SKILL_DESCRIPTION_CHARS + 1);
        let context_only = format!(
            "*** Begin Patch\n*** Update File: my-skill/SKILL.md\n@@\n description: {overlong}\n\
             +new body line\n*** End Patch"
        );
        assert!(
            deny_skill_patch(&context_only, std::path::Path::new("/tmp/skills"), &config).is_none(),
            "an unchanged over-length description must not block an unrelated edit"
        );
    }

    /// The cap governs the catalog's routing signal, which lives in
    /// `SKILL.md` frontmatter and nowhere else. A long `description:` in a
    /// reference doc, a bundled YAML fixture, or the skill's own body prose is
    /// ordinary content — rejecting the patch over it would block edits that
    /// have nothing to do with skill metadata.
    #[test]
    fn deny_skill_patch_only_measures_skill_md_frontmatter() {
        let config = Config::default();
        let overlong = "x".repeat(crate::skills::MAX_SKILL_DESCRIPTION_CHARS + 1);

        let reference_doc = format!(
            "*** Begin Patch\n*** Add File: my-skill/references/api.md\n\
             +# Field reference\n+description: {overlong}\n*** End Patch"
        );
        assert!(
            deny_skill_patch(&reference_doc, std::path::Path::new("/tmp/skills"), &config)
                .is_none(),
            "a description line outside SKILL.md is not the catalog's routing signal"
        );

        let body_prose = format!(
            "*** Begin Patch\n*** Add File: my-skill/SKILL.md\n+---\n+name: my-skill\n\
             +description: short and useful\n+---\n+## Usage\n+description: {overlong}\n\
             *** End Patch"
        );
        assert!(
            deny_skill_patch(&body_prose, std::path::Path::new("/tmp/skills"), &config).is_none(),
            "prose past the frontmatter is never rendered into the catalog"
        );

        // But the common edit — retouching only the description line, with no
        // frontmatter markers in the hunk — must still be caught.
        let metadata_edit = format!(
            "*** Begin Patch\n*** Update File: my-skill/SKILL.md\n@@\n name: my-skill\n\
             -description: short\n+description: {overlong}\n*** End Patch"
        );
        assert!(
            deny_skill_patch(&metadata_edit, std::path::Path::new("/tmp/skills"), &config)
                .is_some(),
            "an over-length description edit must still be refused"
        );
    }

    // ── Cross-directory apply_patch tests ──

    /// Regression test: absolute paths in apply_patch must create files outside CWD.
    #[test]
    fn apply_patch_absolute_path_creates_file_regression() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_patch_abs_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let target = tmp.join("hello.txt");

        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+Hello from absolute path\n*** End Patch",
            target.display()
        );
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "unexpected result: {result}"
        );
        assert!(target.exists(), "file should exist at absolute path");
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "Hello from absolute path");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Regression test: tilde paths in apply_patch must expand correctly.
    #[test]
    fn apply_patch_tilde_path_creates_file_regression() {
        let config = Config::default();
        let home = dirs::home_dir().unwrap();
        let subdir = format!("borg_patch_tilde_{}", std::process::id());
        let target_dir = home.join(&subdir);
        std::fs::create_dir_all(&target_dir).unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Add File: ~/{}/tilde_test.txt\n+Hello from tilde path\n*** End Patch",
            subdir
        );
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "unexpected result: {result}"
        );
        let expected = target_dir.join("tilde_test.txt");
        assert!(
            expected.exists(),
            "file should exist at tilde-expanded path"
        );
        let content = std::fs::read_to_string(&expected).unwrap();
        assert_eq!(content, "Hello from tilde path");

        let _ = std::fs::remove_dir_all(&target_dir);
    }

    /// Regression test: blocked paths must still be denied even with absolute paths.
    #[test]
    fn apply_patch_blocked_path_still_denied_regression() {
        let config = Config::default();
        let home = dirs::home_dir().unwrap();
        let blocked_target = home.join(".ssh/evil_key");

        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+malicious\n*** End Patch",
            blocked_target.display()
        );
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(
            result.contains("Access denied"),
            "expected access denied for blocked path, got: {result}"
        );
        assert!(
            !blocked_target.exists(),
            "file should NOT be created in blocked path"
        );
    }

    /// Regression test: relative paths must still work via CWD as before.
    #[test]
    fn apply_patch_relative_paths_unchanged_regression() {
        let config = Config::default();
        // Use a relative path that creates a temp file in CWD
        let filename = format!("borg_patch_rel_{}.txt", std::process::id());
        let patch =
            format!("*** Begin Patch\n*** Add File: {filename}\n+relative content\n*** End Patch");
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "unexpected result: {result}"
        );
        let cwd = std::env::current_dir().unwrap();
        let created = cwd.join(&filename);
        assert!(created.exists(), "file should exist in CWD");
        let content = std::fs::read_to_string(&created).unwrap();
        assert_eq!(content, "relative content");

        let _ = std::fs::remove_file(&created);
    }

    #[test]
    fn apply_patch_success_includes_diff_output() {
        // Tempdir, not CWD: a panic between `write` and the trailing
        // `remove_file` used to leak `borg_patch_diff_<pid>.txt` into
        // `crates/cortex/` on every failed run. Tempdir cleans up via Drop
        // regardless of how the test exits.
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diff_target.txt");
        std::fs::write(&path, "old\n").unwrap();
        let path_str = path.display().to_string();
        let patch =
            format!("*** Begin Patch\n*** Update File: {path_str}\n@@\n-old\n+new\n*** End Patch");
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(result.contains("Patch applied successfully."));
        assert!(result.contains(&format!("M {path_str}")), "got: {result}");
        assert!(
            result.contains(&format!("--- a/{path_str}")),
            "got: {result}"
        );
        assert!(result.contains("+new"), "got: {result}");
    }

    /// Mixed relative and absolute paths in one patch should both work.
    #[test]
    fn apply_patch_mixed_relative_and_absolute() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_patch_mix_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let abs_target = tmp.join("abs.txt");

        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+absolute file\n*** Add File: mix_rel.txt\n+relative file\n*** End Patch",
            abs_target.display()
        );
        let args = json!({"patch": patch});
        let result = handle_apply_patch(&args, &config).unwrap();
        assert!(
            result.contains("Patch applied successfully"),
            "unexpected result: {result}"
        );
        assert!(abs_target.exists(), "absolute path file should exist");

        // The relative path gets normalized to absolute (CWD-based) when mixed
        // with absolute paths, so it should be created at CWD/mix_rel.txt
        let cwd = std::env::current_dir().unwrap();
        let rel_target = cwd.join("mix_rel.txt");
        assert!(
            rel_target.exists(),
            "relative path file should exist in CWD"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&rel_target);
    }

    #[test]
    fn normalize_patch_paths_handles_standalone_move_file() {
        let cwd = std::env::current_dir().unwrap();
        let patch = "*** Begin Patch\n*** Move File: src.txt -> dest.txt\n*** End Patch";
        let normalized = normalize_patch_paths(patch, &cwd);
        assert!(normalized.contains(&format!(
            "*** Move File: {} -> {}",
            cwd.join("src.txt").display(),
            cwd.join("dest.txt").display()
        )));
    }

    #[test]
    fn patch_has_absolute_paths_sees_move_destination() {
        assert!(patch_has_absolute_paths(
            "*** Move File: src.txt -> /tmp/dest.txt"
        ));
    }

    #[test]
    fn guarded_paths_cover_unified_diff_so_blocked_paths_cannot_be_bypassed() {
        // Regression: the guard used to scan for `*** Update File:` markers,
        // but the applicator also accepts unified diffs. A diff carrying no
        // DSL marker yielded an empty path list, so the `for` loop that calls
        // is_blocked_path / is_control_plane_write_denied never ran and a
        // patch could write `.env`, `.ssh/authorized_keys`, or
        // `~/.borg/hooks.json` (arbitrary shell at every hook point).
        let patch = "\
diff --git a/.env b/.env
--- a/.env
+++ b/.env
@@ -1 +1 @@
-FOO=1
+FOO=2
";
        let paths = guarded_patch_paths(patch).expect("unified diff must parse");
        assert_eq!(
            paths,
            vec![".env".to_string()],
            "the guard must see the path the applicator will write"
        );
    }

    #[test]
    fn guarded_paths_are_none_for_an_unparseable_patch() {
        // Such a patch writes nothing, so it is handed to the applicator for
        // the real parse diagnostic rather than a misleading security denial.
        assert!(guarded_patch_paths("this is not a patch").is_none());
    }

    // ── normalize_patch_paths unit tests ──

    #[test]
    fn normalize_patch_paths_expands_tilde() {
        let cwd = std::path::Path::new("/some/cwd");
        let input = "*** Begin Patch\n*** Add File: ~/foo/bar.txt\n+content\n*** End Patch";
        let result = normalize_patch_paths(input, cwd);
        let home = dirs::home_dir().unwrap();
        let expected_path = home.join("foo/bar.txt");
        assert!(
            result.contains(&expected_path.to_string_lossy().to_string()),
            "tilde should be expanded in: {result}"
        );
    }

    #[test]
    fn normalize_patch_paths_absolutizes_relative() {
        let cwd = std::path::Path::new("/my/project");
        let input = "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {}\n*** End Patch";
        let result = normalize_patch_paths(input, cwd);
        assert!(
            result.contains("*** Add File: /my/project/src/main.rs"),
            "relative path should be absolutized in: {result}"
        );
    }

    #[test]
    fn normalize_patch_paths_preserves_absolute() {
        let cwd = std::path::Path::new("/some/cwd");
        let input = "*** Begin Patch\n*** Add File: /tmp/abs.txt\n+content\n*** End Patch";
        let result = normalize_patch_paths(input, cwd);
        assert!(
            result.contains("*** Add File: /tmp/abs.txt"),
            "absolute path should be preserved in: {result}"
        );
    }

    #[test]
    fn normalize_patch_paths_handles_move_to() {
        let cwd = std::path::Path::new("/my/project");
        let input =
            "*** Begin Patch\n*** Update File: /tmp/old.txt\n*** Move to: ~/new.txt\n*** End Patch";
        let result = normalize_patch_paths(input, cwd);
        let home = dirs::home_dir().unwrap();
        let expected = home.join("new.txt");
        assert!(
            result.contains(&format!("*** Move to: {}", expected.display())),
            "Move to tilde should be expanded in: {result}"
        );
    }

    #[test]
    fn guarded_paths_find_every_dsl_target() {
        let input = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Update File: b.txt\n@@\n-y\n+z\n*** Delete File: c.txt\n*** End Patch";
        let paths = guarded_patch_paths(input).expect("DSL patch must parse");
        assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn patch_has_absolute_paths_detects_absolute() {
        assert!(patch_has_absolute_paths(
            "*** Begin Patch\n*** Add File: /tmp/foo.txt\n+x\n*** End Patch"
        ));
    }

    #[test]
    fn patch_has_absolute_paths_detects_tilde() {
        assert!(patch_has_absolute_paths(
            "*** Begin Patch\n*** Add File: ~/foo.txt\n+x\n*** End Patch"
        ));
    }

    #[test]
    fn patch_has_absolute_paths_false_for_relative() {
        assert!(!patch_has_absolute_paths(
            "*** Begin Patch\n*** Add File: foo.txt\n+x\n*** End Patch"
        ));
    }
}
