//! Security blocklist enforcement: path-component matching with allow-list
//! override and symlink resolution. Shared by `patch`, `listing`, and `read`.
//!
//! `is_control_plane_write_denied` is a separate, non-configurable hard
//! invariant that blocks writes into Borg's own data directory (DB file, KEK
//! material, sockets). User config cannot weaken or override it — a
//! prompt-injected agent rewriting `~/.borg/borg.db` or `~/.borg/secrets/`
//! would silently subvert every other security boundary.

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
        let expanded = crate::util::unix_ext::expand_tilde(raw_entry);
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

/// Canonicalize a path that may not exist yet (the `apply_patch` add-file
/// case). `Path::canonicalize` fails for missing leaves; the workaround is to
/// canonicalize the *parent* and re-attach the file name. If even the parent
/// is absent (an attacker patching `~/.borg/new-dir/borg.db` where `new-dir`
/// doesn't exist), fall back to expanding tilde + lexical normalization so a
/// raw `~/.borg/borg.db` or `./../.borg/borg.db` still resolves close enough
/// for the control-plane comparison to fire.
fn canonicalize_write_target(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(real_parent) = parent.canonicalize() {
            return real_parent.join(name);
        }
    }
    // No part of the path exists yet. Tilde-expand + lexically normalize
    // components so `~/.borg/borg.db` and `~/foo/../.borg/borg.db` still
    // collapse to the literal `$HOME/.borg/borg.db` for comparison.
    let expanded = crate::util::unix_ext::expand_tilde(&path.to_string_lossy());
    let mut out = std::path::PathBuf::new();
    for comp in std::path::Path::new(&expanded).components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Hard-coded write deny for Borg control-plane files inside `$BORG_HOME`.
///
/// Returns `true` if `path` resolves (after `canonicalize`) to:
/// - the SQLite DB file or its WAL/SHM siblings,
/// - any path under `$BORG_HOME/secrets/`,
/// - the gRPC sockets, pairing socket, daemon PID file, or scheduler lock.
///
/// Resolution uses `canonicalize` on both sides so `..` traversal and symlink
/// chains cannot bypass the check. Falls back to the literal path if
/// canonicalize fails (path doesn't exist yet) — the agent shouldn't be
/// creating new files at those locations either.
///
/// Operator escape hatch is intentional absence: there is none. Use the
/// daemon's gRPC API or shell out manually if you need to touch control-plane
/// files from outside the agent loop.
pub fn is_control_plane_write_denied(path: &std::path::Path) -> bool {
    let Ok(home) = crate::paths::resolve_borg_home_readonly(None) else {
        return false;
    };
    let home = home.canonicalize().unwrap_or(home);
    let target = canonicalize_write_target(path);

    // SQLite DB plus its WAL/SHM siblings — `borg.db`, `borg.db-wal`, `borg.db-shm`.
    for sibling in ["borg.db", "borg.db-wal", "borg.db-shm"] {
        if target == home.join(sibling) {
            return true;
        }
    }

    // Whole-tree denies for secret material.
    if target.starts_with(home.join("secrets")) {
        return true;
    }

    // Sockets / pidfile / lockfile at the home root.
    for name in [
        "borgd.sock",
        "borgd-pair.sock",
        "borgd.pid",
        "scheduler.tick.lock",
    ] {
        if target == home.join(name) {
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
    fn control_plane_denies_db_and_secrets_and_resolves_traversal() {
        // Point $BORG_HOME at a tempdir so the canonical comparison resolves
        // to a real path, then assert: (a) DB + WAL/SHM denied, (b) secrets/
        // subtree denied, (c) `..` traversal cannot escape the deny, (d) a
        // symlink into a denied region is also denied via canonicalize, and
        // (e) ordinary user paths under home are unaffected.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("BORG_HOME", &home);
        std::fs::create_dir_all(home.join("secrets")).expect("mkdir secrets");
        std::fs::write(home.join("borg.db"), b"db").expect("seed db");

        assert!(is_control_plane_write_denied(&home.join("borg.db")));
        assert!(is_control_plane_write_denied(&home.join("borg.db-wal")));
        assert!(is_control_plane_write_denied(&home.join("borg.db-shm")));
        assert!(is_control_plane_write_denied(&home.join("secrets/kek.enc")));
        assert!(is_control_plane_write_denied(
            &home.join("secrets/sub/nested.bin")
        ));
        assert!(is_control_plane_write_denied(&home.join("borgd.sock")));

        // `..`-traversal that still lands on the DB resolves to the DB path.
        let traversal = home
            .join("../")
            .join(home.file_name().unwrap())
            .join("borg.db");
        assert!(is_control_plane_write_denied(&traversal));

        // Symlink whose target is the real DB file must be denied.
        use std::os::unix::fs::symlink;
        let outside = tmp.path().join("dangling_link");
        symlink(home.join("borg.db"), &outside).expect("symlink");
        assert!(is_control_plane_write_denied(&outside));

        // Ordinary file under home — not a control-plane file — is allowed.
        assert!(!is_control_plane_write_denied(&home.join("notes.md")));

        std::env::remove_var("BORG_HOME");
    }

    #[test]
    #[cfg(unix)]
    fn control_plane_blocks_writes_to_files_that_do_not_exist_yet() {
        // Regression: `canonicalize` fails for missing leaves, which is
        // exactly the `apply_patch` add-file scenario. The previous fallback
        // returned the literal raw path and skipped every comparison, so a
        // prompt-injected agent could create `~/.borg/borg.db` (or a
        // not-yet-existing subdir under `secrets/`) and bypass the deny.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("BORG_HOME", &home);
        std::fs::create_dir_all(home.join("secrets")).expect("mkdir secrets");

        // Files that don't exist but live under canonical control-plane paths.
        assert!(is_control_plane_write_denied(
            &home.join("borg.db") // no file yet
        ));
        assert!(is_control_plane_write_denied(
            &home.join("secrets").join("new-kek.enc")
        ));
        // Even when the leaf's parent doesn't exist yet.
        assert!(is_control_plane_write_denied(
            &home.join("secrets").join("not-yet-a-dir").join("kek.enc")
        ));

        // Lexical traversal through a non-existent prefix still resolves.
        let traversal = home.join("ghost").join("..").join("borg.db");
        assert!(is_control_plane_write_denied(&traversal));

        std::env::remove_var("BORG_HOME");
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
