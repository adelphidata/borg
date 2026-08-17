//! Tiny filesystem helpers shared across crates (some Unix-specific).

#[cfg(unix)]
use anyhow::{Context, Result};
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

/// Create `path` for writing so that neither a planted symlink nor the umask
/// can reach the contents.
///
/// Two failure modes this closes, both of which `std::fs::write` has:
///
/// 1. **Symlink redirection.** `fs::write` follows a link at the target path,
///    so anyone who can create a file in the parent directory can aim the
///    write at a file of their choosing. `O_EXCL` refuses an existing entry
///    and `O_NOFOLLOW` refuses a link-shaped one, so the write fails instead
///    of landing somewhere else.
/// 2. **The create-then-`chmod` window.** Writing first and tightening
///    permissions afterwards publishes the bytes at the umask default —
///    typically world-readable — for the length of the write. Passing the mode
///    to `open(2)` means the file is never readable by anyone else, not even
///    briefly.
///
/// This is for *new* files. Callers replacing an existing file should go
/// through [`replace_private`], which checks the old entry is not a link
/// before removing it.
#[cfg(unix)]
pub fn create_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Windows has no `O_NOFOLLOW` and no mode bits; `create_new` still gives the
/// exclusive-create half, and file ACLs inherit from the parent directory.
#[cfg(not(unix))]
pub fn create_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Write `contents` to `path` at owner-only permissions, replacing whatever is
/// there — but refusing to write *through* a symlink.
///
/// The existing entry is inspected with `symlink_metadata` (which does not
/// traverse) and unlinked only when it is a real file, then the replacement is
/// created exclusively via [`create_private_new`]. A link at the path is an
/// error, not a redirect.
///
/// Use this for secrets and other files whose contents must not be readable by
/// other local users at any point.
///
/// Returns the open handle so a caller that needs durability can `sync_all()`
/// the descriptor it actually wrote to. Reopening the path to fsync would both
/// hand back a read-only descriptor (where `fsync(2)` is not portable) and risk
/// syncing a different inode after a concurrent replacement. Dropping the
/// return value simply closes the file.
pub fn replace_private(path: &Path, contents: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::Write as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to write through a symlink at {} — remove it and retry",
                    path.display()
                ),
            ));
        }
        // A real file (or directory entry) may be replaced; `remove_file`
        // unlinks the name rather than following it. A parallel process may
        // have unlinked the same fixed-name path between the stat and here —
        // that is the state we wanted, not a failure.
        Ok(_) => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = create_private_new(path)?;
    file.write_all(contents)?;
    file.flush()?;
    Ok(file)
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

    /// The attack this exists to stop: a symlink planted at the write target
    /// must make the write *fail*, not redirect it onto the linked file. A
    /// plain `fs::write` here silently overwrites the victim.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_refused_not_followed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, b"original").expect("seed victim");
        let target = tmp.path().join("secret.key");
        std::os::unix::fs::symlink(&victim, &target).expect("plant link");

        assert!(
            replace_private(&target, b"attacker-visible").is_err(),
            "a link at the target must refuse the write"
        );
        assert!(
            create_private_new(&target).is_err(),
            "exclusive create must refuse the link too"
        );
        assert_eq!(
            std::fs::read(&victim).expect("victim still readable"),
            b"original",
            "the linked-to file must be untouched"
        );
    }

    /// Secrets must never be readable by other local users, not even for the
    /// length of the write — which is exactly what create-then-`chmod` leaves
    /// open. The mode has to come from `open(2)` itself.
    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only_from_creation() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");

        let fresh = tmp.path().join("fresh.key");
        drop(create_private_new(&fresh).expect("create fresh"));
        let mode = std::fs::metadata(&fresh)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600 at creation, got {mode:o}");

        // A second create on the same path is exclusive, so it must fail
        // rather than truncate — that is what keeps the link check honest.
        assert!(
            create_private_new(&fresh).is_err(),
            "create_private_new must not clobber an existing file"
        );

        // Replacing a world-readable predecessor must not inherit its mode.
        let replaced = tmp.path().join("replaced.key");
        std::fs::write(&replaced, b"old").expect("seed");
        set_unix_mode(&replaced, 0o644).expect("widen");
        replace_private(&replaced, b"new-secret").expect("replace");
        let mode = std::fs::metadata(&replaced)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "replacement must be 0o600, got {mode:o}");
        assert_eq!(
            std::fs::read(&replaced).expect("read back"),
            b"new-secret",
            "contents must be the new ones"
        );
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
