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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_passes_through_absolute_paths_unchanged() {
        // Spec: only `~`-prefixed inputs are rewritten. Absolute paths
        // (and bare relative names) must round-trip byte-identically so
        // callers can use this unconditionally.
        assert_eq!(expand_tilde("/etc/passwd"), "/etc/passwd");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn expand_tilde_rewrites_tilde_slash() {
        // Real-world failure: `expand_tilde("~/x")` returning literally
        // `~/x` would silently create files in `cwd` instead of `$HOME`.
        // Without mocking $HOME we can only assert the prefix changed.
        let out = expand_tilde("~/some-borg-test-dir");
        assert!(
            !out.starts_with('~'),
            "tilde must have been rewritten: {out}"
        );
        assert!(
            out.ends_with("/some-borg-test-dir"),
            "tail must be preserved: {out}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn set_unix_mode_round_trips_through_metadata() {
        // Spec: chmod must actually be applied to disk, not silently
        // dropped (a swap to `set_readonly()` would still typecheck but
        // wouldn't honor the requested octal — caught here).
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("chmod_target");
        std::fs::write(&path, b"x").expect("seed file");

        set_unix_mode(&path, 0o600).expect("chmod 600");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");

        set_unix_mode(&path, 0o755).expect("chmod 755");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "expected 0o755, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn set_unix_mode_errors_on_missing_path() {
        // Negative: chmod on a non-existent path must surface an error so
        // callers can `?` it; silently succeeding would hide bugs.
        let err = set_unix_mode(
            std::path::Path::new("/nonexistent/borg-test-chmod-target"),
            0o600,
        )
        .expect_err("must error on missing path");
        let msg = err.to_string();
        assert!(
            msg.contains("chmod") && msg.contains("600"),
            "error must include the chmod context and octal mode: {msg}"
        );
    }
}
