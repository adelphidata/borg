//! Filesystem log pruning and capping.
//!
//! Two responsibilities:
//! - Delete dated `*.jsonl` files beyond the configured retention window.
//! - Head-truncate unbounded append-only logs (`daemon.log`, `daemon.err`,
//!   `tui.log`) when they grow past a hard cap. These have no runtime
//!   rotation; a single noisy warn loop once blew them past 40 MB.

use anyhow::Result;
use std::fs;
use std::path::Path;

/// Size threshold above which `daemon.log`, `daemon.err`, and `tui.log` are
/// head-truncated during the daily sweep. Nothing rotates these files
/// otherwise — a single noisy warn loop once blew them up to >40 MB.
pub(super) const LOG_FILE_SIZE_CAP_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
/// When a log file exceeds [`LOG_FILE_SIZE_CAP_BYTES`], keep this many
/// trailing bytes (the most recent lines) and discard the rest.
pub(super) const LOG_FILE_KEEP_BYTES: u64 = 1024 * 1024; // 1 MB

/// Log files the maintenance sweep is allowed to head-truncate. These are
/// appended-to freely by the daemon and TUI with no rotation; every other
/// file under `~/.borg/logs/` (e.g. dated `*.jsonl`) is handled separately.
pub(super) const CAPPED_LOG_FILES: &[&str] = &["daemon.log", "daemon.err", "tui.log"];

/// Head-truncate append-only log files under `dir` whose size exceeds
/// `cap_bytes`, keeping the last `keep_bytes` of each. Returns the total
/// number of bytes freed across all affected files.
///
/// These logs (`daemon.log`, `tui.log`, `daemon.err`) have no runtime
/// rotation. A single misbehaving warn! loop blew `daemon.log` past 20 MB
/// in under two weeks; this cap prevents recurrence.
pub(super) fn truncate_oversized_logs(
    dir: &Path,
    names: &[&str],
    cap_bytes: u64,
    keep_bytes: u64,
) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut freed: u64 = 0;
    for name in names {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let size = meta.len();
        if size <= cap_bytes {
            continue;
        }
        match truncate_file_keeping_tail(&path, keep_bytes) {
            Ok(new_size) => {
                let delta = size.saturating_sub(new_size);
                freed = freed.saturating_add(delta);
                tracing::info!(
                    "maintenance: truncated {} ({} MB → {} MB)",
                    name,
                    size / (1024 * 1024),
                    new_size / (1024 * 1024),
                );
            }
            Err(e) => tracing::warn!("maintenance: could not truncate {}: {e}", path.display()),
        }
    }
    Ok(freed)
}

/// Rewrite `path` to contain only its last `keep_bytes`. Best-effort
/// attempts to start at a newline so the new head of the file is a
/// complete log line rather than a mid-line fragment.
fn truncate_file_keeping_tail(path: &Path, keep_bytes: u64) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let total = f.metadata()?.len();
    let start = total.saturating_sub(keep_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(keep_bytes as usize);
    f.read_to_end(&mut tail)?;
    // Advance to the first newline so we don't keep a half-truncated head line.
    let offset = tail
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let trimmed = &tail[offset..];
    f.seek(SeekFrom::Start(0))?;
    f.write_all(trimmed)?;
    f.set_len(trimmed.len() as u64)?;
    f.sync_all()?;
    Ok(trimmed.len() as u64)
}

/// Delete `*.jsonl` log files under `dir` whose mtime is older than
/// `cutoff` (unix seconds). Returns deletion count.
pub(super) fn prune_log_files(dir: &Path, cutoff: i64) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut deleted = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if (since_epoch.as_secs() as i64) < cutoff {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!("maintenance: could not remove log {path:?}: {e}");
                continue;
            }
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::SECS_PER_DAY;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn prune_log_files_deletes_only_jsonl_older_than_cutoff() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();

        let jsonl = dir.join("old.jsonl");
        let other = dir.join("keep.txt");
        fs::write(&jsonl, b"old").unwrap();
        fs::write(&other, b"other").unwrap();

        // Cutoff far in the future: every file looks "old", but only the
        // .jsonl should be deleted.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let far_future_cutoff = now + 365 * SECS_PER_DAY;
        let deleted = prune_log_files(dir, far_future_cutoff).unwrap();
        assert_eq!(deleted, 1, "only the .jsonl should be pruned");
        assert!(!jsonl.exists(), "jsonl should be gone");
        assert!(other.exists(), "non-jsonl file should remain");
    }

    #[test]
    fn prune_log_files_keeps_files_newer_than_cutoff() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();
        let jsonl = dir.join("fresh.jsonl");
        fs::write(&jsonl, b"fresh").unwrap();

        // Cutoff in the past: everything is newer than cutoff, nothing is pruned.
        let deleted = prune_log_files(dir, 0).unwrap();
        assert_eq!(deleted, 0);
        assert!(jsonl.exists());
    }

    #[test]
    fn prune_log_files_handles_missing_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(prune_log_files(&missing, 0).unwrap(), 0);
    }

    /// Writes a file with predictable line content and returns `(path, size)`.
    /// Each line is `"line N\n"` — predictable length makes assertions easy.
    fn write_sample_log(dir: &Path, name: &str, approx_bytes: usize) -> (std::path::PathBuf, u64) {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        let mut written = 0usize;
        let mut n = 0u64;
        while written < approx_bytes {
            let line = format!("line {n}\n");
            f.write_all(line.as_bytes()).unwrap();
            written += line.len();
            n += 1;
        }
        f.sync_all().unwrap();
        let size = fs::metadata(&path).unwrap().len();
        (path, size)
    }

    #[test]
    fn truncate_oversized_logs_head_truncates_and_starts_at_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // 8 MB file, cap 5 MB, keep 1 MB.
        let (path, original) = write_sample_log(dir, "daemon.log", 8 * 1024 * 1024);
        assert!(original > 5 * 1024 * 1024);

        let freed =
            truncate_oversized_logs(dir, &["daemon.log"], 5 * 1024 * 1024, 1024 * 1024).unwrap();

        let new_size = fs::metadata(&path).unwrap().len();
        // Must be close to keep_bytes and strictly smaller than original.
        assert!(new_size < original, "file should shrink");
        assert!(
            new_size <= 1024 * 1024,
            "new size {new_size} must not exceed keep_bytes"
        );
        // Freed count must reflect bytes actually removed.
        assert_eq!(freed, original - new_size);

        // The retained head must be a complete line — "line " prefix, not
        // a mid-line fragment. This is the invariant that guarantees future
        // log parsers don't choke on a half-truncated first row.
        let head = fs::read_to_string(&path).unwrap();
        assert!(
            head.starts_with("line "),
            "retained head must start at a line boundary, got: {:?}",
            &head[..head.len().min(40)]
        );
    }

    #[test]
    fn truncate_oversized_logs_noop_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (path, original) = write_sample_log(dir, "tui.log", 2 * 1024 * 1024);

        let freed =
            truncate_oversized_logs(dir, &["tui.log"], 5 * 1024 * 1024, 1024 * 1024).unwrap();

        assert_eq!(freed, 0);
        let new_size = fs::metadata(&path).unwrap().len();
        assert_eq!(new_size, original, "file under cap must not change");
    }

    #[test]
    fn truncate_oversized_logs_preserves_live_append_handles() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("daemon.log");
        let mut writer = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for n in 0..200_000 {
            writeln!(writer, "line {n}").unwrap();
        }
        writer.flush().unwrap();

        let freed = truncate_oversized_logs(dir, &["daemon.log"], 1024 * 1024, 256 * 1024).unwrap();
        assert!(freed > 0, "oversized log should have been truncated");

        writeln!(writer, "tail after truncation").unwrap();
        writer.flush().unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("tail after truncation"),
            "writes through the original append handle must still land in the visible log file"
        );
    }

    #[test]
    fn truncate_oversized_logs_tolerates_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Nothing written; list references files that don't exist.
        let freed = truncate_oversized_logs(
            dir,
            &["daemon.log", "tui.log", "daemon.err"],
            5 * 1024 * 1024,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(freed, 0);
    }
}
