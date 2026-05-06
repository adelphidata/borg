//! Path utilities shared across sandbox and channel code.

use std::path::PathBuf;

/// Expand a leading `~` or `~/` to the user's home directory.
///
/// Handles both bare `~` and `~/foo` prefixes. Returns the input unchanged
/// when the home directory cannot be resolved or the path has no tilde prefix.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_prefix() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~/foo"), home.join("foo"));
    }

    #[test]
    fn expands_bare_tilde() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn passthrough_for_absolute() {
        assert_eq!(expand_tilde("/etc/passwd"), PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn does_not_expand_mid_path() {
        assert_eq!(expand_tilde("/foo/~/bar"), PathBuf::from("/foo/~/bar"));
    }
}
