//! Prefix completion over the session workspace, for a client offering a file
//! picker as someone types.
//!
//! This is a read-only listing with a menu on the other end of it, which makes
//! it a disclosure surface rather than a convenience: whatever it names, it
//! names to anyone looking at the screen. So it answers to the same two rules
//! the read tools do — containment inside the workspace, and
//! [`super::blocked::is_blocked_path`] — and it applies them with *no* way to
//! opt out.
//!
//! Containment here is deliberately stricter than [`ExecutionContext`]'s own
//! `ensure_within_workspace`, which honours an `enforce_workspace_root` flag a
//! session may leave off. A completion menu that walked out of the workspace
//! because a flag was unset would be a leak nobody asked for, so the check runs
//! unconditionally.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::blocked::is_blocked_path;
use crate::execution_context::ExecutionContext;

/// One path a client can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSuggestion {
    /// Written the way the user would type it: relative to the session's
    /// working directory, with a trailing `/` on a directory.
    pub path: String,
    /// True for a directory, so a client can keep the menu open on it.
    pub is_dir: bool,
}

/// The most rows this will ever return, however large a cap is asked for.
///
/// A menu is read, not scrolled: past a screenful the extra rows cost a walk
/// and teach nothing.
pub const MAX_PATH_SUGGESTIONS: usize = 50;

/// Paths under the session workspace whose name starts with `prefix`.
///
/// `prefix` is what the user typed after the trigger, relative to the session's
/// working directory — `src/ma` offers `src/main.rs`. A trailing `/` lists that
/// directory. Anything that does not resolve inside the workspace, and anything
/// the blocklist covers, is simply absent; there is no error for it, because a
/// menu that explains why a file is missing has just disclosed the file.
pub fn complete_paths(
    prefix: &str,
    ctx: &ExecutionContext,
    blocked: &[String],
    allowed: &[String],
    limit: usize,
) -> Result<Vec<PathSuggestion>> {
    let root = ctx.workspace_root().canonicalize().with_context(|| {
        format!(
            "workspace root {} is unreadable",
            ctx.workspace_root().display()
        )
    })?;

    // An absolute path, or one through `~`, is not a workspace reference at
    // all. Refusing the whole shape is simpler to be sure of than resolving it
    // and hoping the containment check catches every way home can be spelled.
    if prefix.starts_with('/') || prefix.starts_with('~') {
        return Ok(Vec::new());
    }

    // Everything up to the last `/` names the directory to read; the rest is
    // the partial name to match. `src/ma` reads `src` and matches `ma`.
    let (dir_part, leaf) = match prefix.rfind('/') {
        Some(at) => (&prefix[..at + 1], &prefix[at + 1..]),
        None => ("", prefix),
    };

    let Some(base) = resolve_within(&ctx.workspace_cwd().join(dir_part), &root) else {
        return Ok(Vec::new());
    };

    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(dir = %base.display(), error = %e, "path completion could not read dir");
            return Ok(Vec::new());
        }
    };

    let leaf_lower = leaf.to_lowercase();
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::debug!(dir = %base.display(), error = %e, "skipping unreadable dir entry");
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();

        if !name.to_lowercase().starts_with(&leaf_lower) {
            continue;
        }
        // Dotfiles stay out of the resting list and appear once someone types
        // the dot — the same rule the palette applies to raw settings keys.
        // `.git` and friends are noise in front of every completion otherwise.
        if !leaf.starts_with('.') && name.starts_with('.') {
            continue;
        }

        // Resolved, not joined: a symlink pointing out of the workspace is the
        // one way a name inside it can still name a file outside.
        let Some(resolved) = resolve_within(&base.join(&name), &root) else {
            continue;
        };
        if is_blocked_path(&resolved, blocked, allowed) {
            continue;
        }

        let is_dir = resolved.is_dir();
        out.push(PathSuggestion {
            path: format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }),
            is_dir,
        });
    }

    // Directories first: picking one is how you get deeper, so it is the move
    // most likely wanted from a half-typed prefix.
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
    });
    out.truncate(limit.min(MAX_PATH_SUGGESTIONS));
    Ok(out)
}

/// The canonical form of `candidate`, or None when it does not exist or lands
/// outside `root`.
fn resolve_within(candidate: &Path, root: &Path) -> Option<PathBuf> {
    let resolved = candidate.canonicalize().ok()?;
    (resolved == root || resolved.starts_with(root)).then_some(resolved)
}

#[cfg(test)]
mod tests;
