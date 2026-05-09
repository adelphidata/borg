//! Tiny filesystem helpers shared across crates (some Unix-specific).

#[cfg(unix)]
use anyhow::{Context, Result};
#[cfg(unix)]
use std::path::Path;

/// `chmod` a path to the given mode, with a contextual error on failure.
///
/// Wrapper around `std::fs::set_permissions(Permissions::from_mode(mode))`
/// that produces a uniform `chmod <path> <octal>` error message.
#[cfg(unix)]
pub fn set_unix_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod {} {:o}", path.display(), mode))
}

/// Expand a leading `~` to the user's home directory.
///
/// Thin owned-string wrapper around [`shellexpand::tilde`]. Returns the
/// input unchanged when no expansion applies (no leading `~`, or the
/// home directory cannot be determined). Use this instead of inlining
/// `shellexpand::tilde(x).to_string()` so the cortex codebase has a
/// single canonical home for the operation.
pub fn expand_tilde(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
}
