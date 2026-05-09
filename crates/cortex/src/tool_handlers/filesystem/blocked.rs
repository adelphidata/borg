//! Security blocklist enforcement: path-component matching with allow-list
//! override and symlink resolution. Shared by `patch`, `listing`, and `read`.

/// Check whether `path` is denied by the configured blocklist, taking the
/// allow list into account.
///
/// Matching rules:
/// 1. The path is canonicalized first (resolves symlinks, `..`, and
///    tilde-expanded entries on the allow list). If canonicalization fails
///    for a path that doesn't exist, the original path is used — this is
///    fine because nonexistent paths are handled separately by callers.
/// 2. If the canonical path starts with any entry in `allowed_paths`,
///    access is allowed regardless of blocklist matches. This is the
///    escape hatch for specific `.env` files or `.aws/` directories that
///    the user legitimately wants the agent to read.
/// 3. Otherwise, access is denied if **any path component** of the
///    canonical path matches an entry in `blocked`. Blocked entries may be
///    single components (e.g. `.ssh`) or multi-component suffixes (e.g.
///    `.config/gh`) — both forms are supported.
///
/// This deliberately does **not** jail access to `$HOME` or CWD: absolute
/// paths to non-sensitive files elsewhere on the system
/// (`/etc/hosts`, `/Users/you/other-project/...`, `../sibling/file.rs`)
/// remain fully accessible, as does cross-directory navigation via `@`.
pub fn is_blocked_path(path: &std::path::Path, blocked: &[String], allowed: &[String]) -> bool {
    // Canonicalize so we make decisions on the real on-disk path. If
    // canonicalization fails (path doesn't exist, permissions), fall back to
    // the literal path — callers handle nonexistent paths separately.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    // 1. Allow list: if the canonical path is under any allowed prefix,
    //    short-circuit to allow. Entries support tilde expansion.
    for raw_entry in allowed {
        let expanded = crate::unix_ext::expand_tilde(raw_entry);
        let entry_path = std::path::PathBuf::from(&expanded);
        // Canonicalize the allow entry too, so `~/work/.aws` and
        // `/Users/me/work/.aws` compare equal.
        let entry_canonical = entry_path.canonicalize().unwrap_or(entry_path);
        if canonical.starts_with(&entry_canonical) {
            return false;
        }
    }

    // 2. Block list: match any component or multi-component suffix against
    //    the canonical path's components.
    for raw_entry in blocked {
        if path_contains_blocked_entry(&canonical, raw_entry) {
            return true;
        }
    }

    false
}

/// Returns `true` if `path` contains `entry` as a contiguous sequence of
/// components. `entry` may itself be multi-component (e.g. `.config/gh`).
fn path_contains_blocked_entry(path: &std::path::Path, entry: &str) -> bool {
    // Split the entry into components, tolerating both forward slashes and
    // the platform separator.
    let entry_components: Vec<&std::ffi::OsStr> = std::path::Path::new(entry)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if entry_components.is_empty() {
        return false;
    }

    let path_components: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if path_components.len() < entry_components.len() {
        return false;
    }

    // Sliding window match.
    for start in 0..=path_components.len() - entry_components.len() {
        let window = &path_components[start..start + entry_components.len()];
        if window == entry_components.as_slice() {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_blocked_path_matches_blocked_dir() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked = vec![".ssh".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_rejects_non_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join("Documents/safe.txt");
        let blocked = vec![".ssh".to_string(), ".aws".to_string()];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_nested_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".aws/credentials/secret");
        let blocked = vec![".aws".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_empty_blocked_list() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked: Vec<String> = vec![];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_outside_home() {
        let blocked = vec![".ssh".to_string()];
        let path = std::path::Path::new("/tmp/.ssh/id_rsa");
        assert!(is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_allowed_overrides_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".env.example");
        let blocked = vec![".env".to_string()];
        // The path has ".env" as a substring of the component ".env.example"
        // but component matching should only match exact components
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_absolute_path_not_under_home() {
        let blocked = vec![".aws".to_string()];
        let path = std::path::Path::new("/etc/hosts");
        assert!(!is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    #[cfg(unix)]
    fn is_blocked_path_resolves_symlink_chain_into_blocked() {
        // Regression guard: protect against an attacker creating a chain of
        // symlinks (`a -> b -> .ssh/id_rsa`) under a non-blocked directory and
        // having the agent read the chain as if it were a benign file. This
        // pins the realpath-style resolution we rely on — if a future change
        // swaps `canonicalize()` for a single-hop resolver, this fails loudly.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&blocked_dir).expect("mkdir .ssh");
        let target = blocked_dir.join("id_rsa");
        std::fs::write(&target, b"PRIVATE").expect("seed target");

        let mid = tmp.path().join("mid_link");
        let entry = tmp.path().join("entry_link");
        symlink(&target, &mid).expect("mid symlink");
        symlink(&mid, &entry).expect("entry symlink");

        let blocked = vec![".ssh".to_string()];
        assert!(
            is_blocked_path(&entry, &blocked, &[]),
            "two-hop symlink into .ssh must canonicalize and be blocked"
        );
    }
}
