//! Crash-safe file writes and UTF-8 BOM handling for the patch applicator.
//!
//! Two concerns live here because the patch applicator needs both at the same
//! seam (the commit step that writes content to disk):
//!
//! * [`atomic_write`] replaces a direct `std::fs::write` so a crash, signal, or
//!   filesystem error mid-write can never leave a half-written (truncated)
//!   target file. We write the full content to a sibling temp file, flush it,
//!   then `rename` it over the target — `rename` is atomic on a single
//!   filesystem, so a reader sees either the old file or the complete new one.
//! * [`split_bom`] detects a leading UTF-8 byte-order mark (U+FEFF). The
//!   applicator strips it before hunk matching (so a patch's first context line
//!   matches a BOM-prefixed file) and re-prepends it on write (so a file
//!   authored on Windows keeps its BOM).

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The UTF-8 byte-order mark as a `char` (U+FEFF). Encodes to `EF BB BF`.
pub const UTF8_BOM: char = '\u{FEFF}';

/// Monotonic counter giving each temp file a unique name within this process,
/// without relying on wall-clock time or randomness (both intentionally avoided
/// in this workspace).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Split a leading UTF-8 BOM off `s`.
///
/// Returns `(had_bom, body)` where `body` is `s` with a single leading U+FEFF
/// removed (if present). Only the first BOM is stripped; a BOM later in the
/// string is left untouched.
#[must_use]
pub fn split_bom(s: &str) -> (bool, &str) {
    match s.strip_prefix(UTF8_BOM) {
        Some(rest) => (true, rest),
        None => (false, s),
    }
}

/// Atomically write `bytes` to `full_path`.
///
/// Writes to a temp file in the same directory, flushes it to the OS, then
/// renames it over the target. Same-directory placement keeps the `rename` on
/// one filesystem so it stays atomic. On any error the temp file is removed on a
/// best-effort basis (temp cleanup is the documented exception to the
/// no-silent-swallowing rule).
///
/// The caller is responsible for any parent-directory creation and symlink
/// checks on `full_path` before calling this.
///
/// Permissions (Unix): the `rename` installs the temp file's mode, so the temp
/// must end up with the right one. An existing target keeps its exact mode
/// (executable bits, `0600` hardening) — the temp is created `0600` so the
/// content is never world-readable during the write window, then chmod'd to the
/// target's mode before the rename. A brand-new target gets ordinary
/// umask-masked `0666` (the same default a plain create would produce).
///
/// Ownership (Unix): an in-place rewrite keeps the inode and therefore its
/// owner for free; this function swaps in a new inode owned by the *writing*
/// user. Rewriting a user-owned file as root — `sudo borg`, a container or NAS
/// volume — would silently transfer it to root and lock the real owner out, so
/// an existing target's uid/gid are restored onto the temp before the rename.
/// Restoring is best-effort: an unprivileged process cannot chown, which is
/// also the case where the owner has not changed, so a failure there is
/// expected and must not fail the write.
pub fn atomic_write(full_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = full_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = full_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tick = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{file_name}.borg-tmp-{}-{tick}",
        std::process::id()
    ));

    // NotFound is the only error that means "new file"; anything else
    // (EACCES, EIO, ELOOP) must propagate — treating it as new would let the
    // rename below rewrite a 0600-hardened target with umask-default perms.
    #[cfg(unix)]
    let existing_meta = match std::fs::metadata(full_path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    let existing_perms = existing_meta.as_ref().map(std::fs::Metadata::permissions);

    // Inner closure so a single `?`-chain covers create/write/flush, and we can
    // clean up the temp file on any failure.
    let write_result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt as _;
            // 0600 while an existing target's content is in flight; 0666 (the
            // kernel applies the umask) when the target is new.
            let mode = if existing_perms.is_some() {
                0o600
            } else {
                0o666
            };
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp_path)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.flush()?;
        // Ensure bytes hit the disk before the rename so a crash right after the
        // rename can't surface an empty file.
        f.sync_all()?;
        #[cfg(unix)]
        if let Some(perms) = existing_perms.clone() {
            std::fs::set_permissions(&tmp_path, perms)?;
        }
        // Ownership last, and only when it would actually change: `fchown`
        // requires privilege, so an ordinary user rewriting their own file
        // would otherwise take an EPERM on every write. Deliberately not `?` —
        // see the ownership note in the doc comment.
        #[cfg(unix)]
        if let Some(meta) = existing_meta.as_ref() {
            use std::os::unix::fs::MetadataExt as _;
            let (uid, gid) = (meta.uid(), meta.gid());
            // SAFETY: plain libc getters, no arguments, always succeed.
            let (self_uid, self_gid) = unsafe { (libc::geteuid(), libc::getegid()) };
            if (uid, gid) != (self_uid, self_gid) {
                if let Err(e) = std::os::unix::fs::fchown(&f, Some(uid), Some(gid)) {
                    tracing::warn!(
                        path = %full_path.display(),
                        uid,
                        gid,
                        error = %e,
                        "could not restore file ownership across atomic write; the file is now owned by the writing user"
                    );
                }
            }
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup of the partial temp file; the write error is what matters.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp_path, full_path) {
        // Rename failed, so the temp file is orphaned; the rename error is what matters.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_bom_strips_single_leading_mark() {
        let (had, body) = split_bom("\u{FEFF}hello");
        assert!(had);
        assert_eq!(body, "hello");
    }

    #[test]
    fn split_bom_absent_returns_input_untouched() {
        let (had, body) = split_bom("hello");
        assert!(!had);
        assert_eq!(body, "hello");
    }

    #[test]
    fn split_bom_only_removes_first_mark() {
        // A BOM in the middle is real content (U+FEFF zero-width no-break space)
        // and must survive.
        let (had, body) = split_bom("\u{FEFF}a\u{FEFF}b");
        assert!(had);
        assert_eq!(body, "a\u{FEFF}b");
    }

    #[test]
    fn atomic_write_creates_exact_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        atomic_write(&target, b"exact bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"exact bytes");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        std::fs::write(&target, "old content that is longer").unwrap();
        atomic_write(&target, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        // An executable script and a 0600-hardened secret file must keep
        // their modes across a patch rewrite — resetting them to umask
        // defaults would silently strip the exec bit / widen a private file.
        for mode in [0o755u32, 0o600] {
            let target = dir.path().join(format!("target-{mode:o}"));
            std::fs::write(&target, "old").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
            atomic_write(&target, b"new").unwrap();
            let got = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "mode {mode:o} not preserved, got {got:o}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_new_file_gets_umask_default_not_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fresh.txt");
        atomic_write(&target, b"content").unwrap();
        let got = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        // The exact value depends on the process umask; the contract is that a
        // new file is a normal readable file (owner r/w set, not locked to
        // 0600 by the private write window, never world-writable under any
        // sane umask).
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "content").unwrap();
        let want = std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            got, want,
            "new-file mode {got:o} != plain-create mode {want:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_owner_when_it_already_matches() {
        use std::os::unix::fs::MetadataExt as _;
        // The common case: an ordinary user rewriting their own file. The
        // ownership branch must be a no-op here rather than attempting a
        // privileged fchown that would fail the write with EPERM.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("owned.txt");
        std::fs::write(&target, "old").unwrap();
        let before = std::fs::metadata(&target).unwrap();
        let (uid, gid) = (before.uid(), before.gid());

        atomic_write(&target, b"new").expect("write must not fail on an unprivileged chown path");

        let after = std::fs::metadata(&target).unwrap();
        assert_eq!((after.uid(), after.gid()), (uid, gid));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    /// A metadata error other than NotFound must fail the write instead of
    /// being treated as "new file" — here a self-referential symlink, whose
    /// resolution errors while the path itself very much exists. Treating it
    /// as new would let the rename clobber the link with a plain file.
    #[cfg(unix)]
    #[test]
    fn atomic_write_propagates_non_notfound_metadata_errors() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("loop");
        std::os::unix::fs::symlink(&target, &target).unwrap();

        assert!(
            atomic_write(&target, b"content").is_err(),
            "a symlink loop must fail the write, not count as a new file"
        );
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "the unresolvable link must survive the failed write"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["loop".to_string()],
            "no temp residue after the early failure: {entries:?}"
        );
    }

    /// Ownership restoration must be skipped when the writer already owns the
    /// file: an unprivileged fchown — even to the file's current owner —
    /// makes the kernel strip setuid/setgid, so a gratuitous one would break
    /// the "existing target keeps its exact mode" contract.
    #[cfg(unix)]
    #[test]
    fn atomic_write_does_not_chown_a_file_the_writer_already_owns() {
        use std::os::unix::fs::PermissionsExt as _;
        // SAFETY: plain libc getter, no arguments, always succeeds.
        if unsafe { libc::geteuid() } == 0 {
            // root chown never clears setuid, so the observable vanishes.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("suid.sh");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o4755)).unwrap();

        atomic_write(&target, b"new").unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o4755,
            "the setuid bit must survive a same-owner rewrite"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn atomic_write_leaves_no_temp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        atomic_write(&target, b"content").unwrap();
        // The directory must contain only the target — no stray `.borg-tmp-*`.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["out.txt".to_string()],
            "leftover temp files: {entries:?}"
        );
    }
}
