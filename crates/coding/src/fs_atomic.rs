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

    // Inner closure so a single `?`-chain covers create/write/flush, and we can
    // clean up the temp file on any failure.
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.flush()?;
        // Ensure bytes hit the disk before the rename so a crash right after the
        // rename can't surface an empty file.
        f.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup of the partial temp file; the write error is what matters.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp_path, full_path) {
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
