//! Pruning of quarantined database copies.
//!
//! When `borg.db` cannot be opened, recovery renames it (and its `-wal` /
//! `-shm` sidecars) to `borg.db.corrupt-<unix_ts>` and creates a fresh
//! database in its place. That preserves the operator's data for a manual
//! salvage, and nothing in the recovery path ever removes those copies —
//! deliberately, because recovery must stay bounded and side-effect-light.
//!
//! The cost of that lands here: a daemon restart-looping against a
//! persistently unopenable file writes one full-size copy per restart, onto
//! the same disk the live database needs. The daily sweep keeps the newest
//! few and deletes the rest.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// How many quarantined copies survive a sweep.
///
/// More than one because a salvage attempt may want to compare copies from
/// different failures; few, because each is a full-size database.
pub const CORRUPT_DB_COPIES_KEPT: usize = 3;

/// Marks a quarantined file. Recovery builds these names as
/// `<file>.corrupt-<unix_ts>` plus the original extension for sidecars.
const QUARANTINE_MARKER: &str = ".corrupt-";

/// Delete all but the newest `keep` quarantined database copies in `dir`.
///
/// Returns how many files were deleted. Ordering is by the timestamp in the
/// name rather than mtime: the name is what recovery assigned, and a restore
/// or a backup tool rewriting mtimes must not reshuffle which copy is
/// "newest". Sidecars are grouped with their database so a kept copy keeps
/// its `-wal`, without which it may not be salvageable at all.
pub fn prune_corrupt_db_copies(dir: &Path, keep: usize) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing data dir is the ordinary "nothing to prune" case; an
        // unreadable one is a real problem that would otherwise let copies
        // accumulate silently until the disk fills.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                "maintenance: cannot scan for quarantined database copies: {e}",
            );
            return Ok(0);
        }
    };

    // stamp -> every file quarantined under it (the DB plus its sidecars).
    let mut groups: std::collections::BTreeMap<u64, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stamp) = quarantine_stamp(&name) else {
            continue;
        };
        groups.entry(stamp).or_default().push(path);
    }

    if groups.len() <= keep {
        return Ok(0);
    }

    let doomed = groups.len() - keep;
    let mut deleted = 0;
    for (stamp, paths) in groups.into_iter().take(doomed) {
        for path in paths {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    deleted += 1;
                    tracing::info!(
                        path = %path.display(),
                        stamp,
                        "maintenance: deleted quarantined database copy",
                    );
                }
                // Best-effort: a copy the operator is mid-salvage on may be
                // locked or already gone. Say so rather than failing the
                // whole sweep.
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    "maintenance: could not delete quarantined copy: {e}",
                ),
            }
        }
    }
    Ok(deleted)
}

/// The unix timestamp recovery stamped into a quarantined file's name, or
/// `None` if this is not one of ours.
///
/// Requires a fully numeric stamp, so an operator's own
/// `borg.db.corrupt-before-upgrade` is left alone.
fn quarantine_stamp(name: &str) -> Option<u64> {
    let rest = name.split_once(QUARANTINE_MARKER)?.1;
    // Sidecars carry the original extension after the stamp
    // (`borg.db.corrupt-123-wal`).
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() != rest.len() && !rest[digits.len()..].starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn the_newest_copies_survive_and_the_rest_are_deleted() {
        // The failure this prevents: a restart loop writes one full-size
        // database copy per restart until the disk the live database needs
        // is full.
        let dir = tempfile::TempDir::new().unwrap();
        for stamp in [100, 200, 300, 400, 500] {
            touch(dir.path(), &format!("borg.db.corrupt-{stamp}"));
        }

        let deleted = prune_corrupt_db_copies(dir.path(), 3).unwrap();

        assert_eq!(deleted, 2);
        assert_eq!(
            names(dir.path()),
            vec![
                "borg.db.corrupt-300".to_string(),
                "borg.db.corrupt-400".to_string(),
                "borg.db.corrupt-500".to_string(),
            ],
            "the newest copies are the ones worth salvaging"
        );
    }

    #[test]
    fn a_kept_copy_keeps_its_sidecars() {
        // A database separated from its `-wal` may not be salvageable, so
        // sidecars are grouped with their copy rather than counted as
        // copies of their own.
        let dir = tempfile::TempDir::new().unwrap();
        for stamp in [100, 200] {
            touch(dir.path(), &format!("borg.db.corrupt-{stamp}"));
            touch(dir.path(), &format!("borg.db.corrupt-{stamp}-wal"));
            touch(dir.path(), &format!("borg.db.corrupt-{stamp}-shm"));
        }

        let deleted = prune_corrupt_db_copies(dir.path(), 1).unwrap();

        assert_eq!(deleted, 3, "the whole older group goes, not one file of it");
        assert_eq!(
            names(dir.path()),
            vec![
                "borg.db.corrupt-200".to_string(),
                "borg.db.corrupt-200-shm".to_string(),
                "borg.db.corrupt-200-wal".to_string(),
            ],
        );
    }

    #[test]
    fn the_live_database_and_operator_files_are_never_touched() {
        let dir = tempfile::TempDir::new().unwrap();
        touch(dir.path(), "borg.db");
        touch(dir.path(), "borg.db-wal");
        touch(dir.path(), "borg.db.corrupt-before-upgrade");
        touch(dir.path(), "notes.txt");
        for stamp in [1, 2, 3, 4] {
            touch(dir.path(), &format!("borg.db.corrupt-{stamp}"));
        }

        prune_corrupt_db_copies(dir.path(), 1).unwrap();

        let remaining = names(dir.path());
        assert!(remaining.contains(&"borg.db".to_string()));
        assert!(remaining.contains(&"borg.db-wal".to_string()));
        assert!(
            remaining.contains(&"borg.db.corrupt-before-upgrade".to_string()),
            "a hand-named backup is not ours to delete"
        );
        assert!(remaining.contains(&"notes.txt".to_string()));
    }

    #[test]
    fn nothing_is_deleted_while_under_the_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        touch(dir.path(), "borg.db.corrupt-1");
        touch(dir.path(), "borg.db.corrupt-2");

        assert_eq!(prune_corrupt_db_copies(dir.path(), 3).unwrap(), 0);
        assert_eq!(names(dir.path()).len(), 2);
    }
}
