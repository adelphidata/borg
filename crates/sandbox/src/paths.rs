//! Path utilities shared across sandbox and channel code.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to the user's home directory.
///
/// Handles both bare `~` and `~/foo` prefixes. Returns the input unchanged
/// when the home directory cannot be resolved or the path has no tilde prefix.
pub fn expand_tilde(path: &str) -> PathBuf {
    match dirs::home_dir() {
        Some(home) => expand_tilde_with_home(path, &home),
        None => PathBuf::from(path),
    }
}

fn expand_tilde_with_home(path: &str, home: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else if path == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use a literal fake home so the test's oracle is independent of the
    // production resolver. A regression like swapping `strip_prefix("~/")` for
    // `strip_prefix("~")` would now produce `/home/fake/foo` vs `/home/fakefoo`
    // and fail.
    const FAKE_HOME: &str = "/home/fake";

    fn home() -> PathBuf {
        PathBuf::from(FAKE_HOME)
    }

    #[test]
    fn expands_tilde_slash_to_home_joined_with_rest() {
        assert_eq!(
            expand_tilde_with_home("~/foo", &home()),
            PathBuf::from("/home/fake/foo")
        );
        assert_eq!(
            expand_tilde_with_home("~/foo/bar", &home()),
            PathBuf::from("/home/fake/foo/bar")
        );
    }

    #[test]
    fn expands_bare_tilde_to_home() {
        assert_eq!(expand_tilde_with_home("~", &home()), home());
    }

    #[test]
    fn passthrough_for_absolute() {
        assert_eq!(
            expand_tilde_with_home("/etc/passwd", &home()),
            PathBuf::from("/etc/passwd")
        );
        // Also exercise the public entry point so the dirs::home_dir path is
        // covered: non-tilde input must round-trip regardless of resolver.
        assert_eq!(expand_tilde("/etc/passwd"), PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn does_not_expand_mid_path() {
        assert_eq!(
            expand_tilde_with_home("/foo/~/bar", &home()),
            PathBuf::from("/foo/~/bar")
        );
    }

    #[test]
    fn does_not_expand_tilde_without_slash_prefix() {
        // `~foo` is a user-relative path in shell parlance (e.g. `~root`).
        // We don't support that — it must pass through verbatim, NOT get
        // joined onto home as `/home/fake/foo` would imply.
        assert_eq!(
            expand_tilde_with_home("~foo", &home()),
            PathBuf::from("~foo")
        );
    }
}
