//! Overflow storage for tool output too large to put in the conversation.
//!
//! A truncated result loses its middle permanently: the model cannot get the
//! omitted bytes back except by re-running the tool, which costs another call
//! and, for a non-idempotent tool, may not even be safe. Spilling writes the
//! full text to a file under `<data_dir>/cache/` and lets the elision marker
//! name the path, so the model can read or grep the file instead.
//!
//! Two callers today: `run_shell`, which spills per output stream, and the
//! conversation-level result cap in `agent::session_manager`, which covers
//! every other tool — MCP results above all, since a server that returns an
//! un-paginated payload otherwise loses everything past the cap.
//!
//! **Callers redact before handing content here**, so the file gets the same
//! hygiene as the text the model sees. Note that both callers redact through
//! an operator-toggleable path: with redaction switched off, raw output lands
//! on disk. The file is written owner-only (`0600` on unix) for exactly that
//! reason — the permission bits are the floor, not the redaction.
//!
//! These files live under `~/.borg/cache/`, which the sandbox's deny-read list
//! does *not* cover. That is deliberate: the whole point is that the agent can
//! read the file back.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Spill files older than this are deleted (best-effort) whenever a new one is
/// written. They are re-derivable scratch, not records.
pub(crate) const SPILL_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Upper bound on the bytes any one spill file may hold.
///
/// A spill is a recovery aid, not an archive, and it survives for
/// [`SPILL_MAX_AGE`]. Without a ceiling a single pathological result — an MCP
/// server returning a multi-hundred-megabyte payload — parks that much in
/// `~/.borg/cache/` for a day. `run_shell` has always bounded its in-memory
/// copy at this size; applying the same bound to every writer keeps one
/// oversized result from filling the disk.
pub(crate) const SPILL_KEEP_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Resolve `<data_dir>/cache/<leaf>/`. `None` (with a warn) when the data dir
/// cannot be resolved — callers fall back to plain truncation rather than
/// failing the tool call.
pub(crate) fn spill_dir(leaf: &str, context: &str) -> Option<PathBuf> {
    match crate::config::Config::data_dir() {
        Ok(dir) => Some(crate::paths::subdir(&dir, crate::paths::BorgSubdir::Cache).join(leaf)),
        Err(e) => {
            tracing::warn!(error = %e, "{context}: cannot resolve data dir for output spill");
            None
        }
    }
}

/// Clamp `content` to [`SPILL_KEEP_MAX_BYTES`], keeping the head and naming
/// what was dropped so the reader is never silently shown a partial file.
///
/// Splits on a char boundary — the content is UTF-8 and slicing mid-codepoint
/// would panic.
fn bound_spill_content(content: &str) -> std::borrow::Cow<'_, str> {
    if content.len() <= SPILL_KEEP_MAX_BYTES {
        return std::borrow::Cow::Borrowed(content);
    }
    let mut end = SPILL_KEEP_MAX_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = content.len() - end;
    std::borrow::Cow::Owned(format!(
        "{}\n\n[spill truncated: {dropped} of {} bytes dropped at the {SPILL_KEEP_MAX_BYTES}-byte \
         spill ceiling]\n",
        &content[..end],
        content.len()
    ))
}

/// Best-effort reap of spill files under `dir` older than `max_age` whose name
/// starts with `prefix`. Failures are logged and skipped — cleanup must never
/// block writing the new spill.
fn cleanup_old_spill_files(dir: &Path, prefix: &str, max_age: Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), "spill cleanup: cannot read dir");
            return;
        }
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > max_age);
        if expired {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::warn!(error = %e, path = %entry.path().display(), "spill cleanup failed");
            }
        }
    }
}

/// Write `content` (already redacted) to a fresh uniquely-named spill file,
/// reaping expired siblings first.
///
/// `prefix` names the writer (`"shell-spill"`, `"tool-spill"`) and doubles as
/// the cleanup filter, so one writer's reaper never deletes another's files.
/// `label` distinguishes files from the same writer within a process.
///
/// Owner-only permissions on unix: redaction is pattern-based and can miss, so
/// the file gets the same posture as the rest of `~/.borg`.
///
/// Content past [`SPILL_KEEP_MAX_BYTES`] is dropped with a trailing note, so a
/// pathological result cannot park hundreds of megabytes in the cache for a
/// day. The file stays a best-effort recovery aid past that point.
pub(crate) fn write_spill_file(
    dir: &Path,
    prefix: &str,
    label: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    cleanup_old_spill_files(dir, prefix, SPILL_MAX_AGE);
    let content = bound_spill_content(content);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "{prefix}-{nanos}-{}-{label}.txt",
        std::process::id()
    ));
    // Owner-only from the moment the file exists: a create-then-chmod would
    // leave the full output readable to other local users for the duration of
    // the write, and redaction is pattern-based enough to miss.
    //
    // `create_new` is O_CREAT|O_EXCL, which also refuses to follow a symlink
    // planted at this path — the unique name makes that unlikely, and the
    // exclusive open makes it harmless.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    std::io::Write::write_all(&mut file, content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reaper must delete expired files belonging to the writer that
    /// invoked it, and nothing else. Both callers share one cache root, so a
    /// prefix-blind sweep would let a `run_shell` spill delete a tool spill the
    /// model is still being pointed at; a name-blind sweep would eat unrelated
    /// neighbors outright.
    #[test]
    fn cleanup_only_reaps_expired_files_matching_the_callers_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = std::time::SystemTime::now() - Duration::from_secs(48 * 60 * 60);

        let mine = dir.path().join("tool-spill-1-2-a.txt");
        let other_writer = dir.path().join("shell-spill-1-2-a.txt");
        let unrelated = dir.path().join("keepme.txt");
        for path in [&mine, &other_writer, &unrelated] {
            std::fs::write(path, "x").expect("seed");
            let f = std::fs::File::options()
                .write(true)
                .open(path)
                .expect("reopen");
            f.set_modified(stale).expect("backdate");
        }

        cleanup_old_spill_files(dir.path(), "tool-spill", SPILL_MAX_AGE);

        assert!(
            !mine.exists(),
            "an expired file with my prefix must be reaped"
        );
        assert!(
            other_writer.exists(),
            "another writer's expired file must be left alone"
        );
        assert!(unrelated.exists(), "non-spill files must be untouched");
    }

    /// A fresh file must survive the reaper, or the spill a caller just wrote
    /// could be deleted by the next caller moments later.
    #[test]
    fn cleanup_leaves_files_younger_than_the_max_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_spill_file(dir.path(), "tool-spill", "probe", "body").expect("write");

        cleanup_old_spill_files(dir.path(), "tool-spill", SPILL_MAX_AGE);

        assert!(path.exists(), "a just-written spill must not be reaped");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "body");
    }

    /// A spill lives for 24h, so one pathological result must not be able to
    /// park unbounded bytes in the cache dir. The file is capped and says what
    /// it dropped — a silently partial recovery file is worse than none.
    #[test]
    fn an_oversized_spill_is_capped_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let oversized = "z".repeat(SPILL_KEEP_MAX_BYTES + 4096);

        let path = write_spill_file(dir.path(), "tool-spill", "probe", &oversized).expect("write");
        let written = std::fs::read_to_string(&path).expect("read");

        assert!(
            written.len() < oversized.len(),
            "content past the ceiling must not reach disk"
        );
        assert!(
            written.contains("spill truncated"),
            "a capped spill must disclose the truncation: {}",
            &written[written.len().saturating_sub(200)..]
        );
        assert!(
            written.starts_with("zzz"),
            "the head of the content must be preserved"
        );
    }

    /// Multi-byte content sitting across the ceiling must not panic the
    /// writer: slicing a `str` mid-codepoint is a hard panic, and this path
    /// runs on arbitrary tool output.
    #[test]
    fn capping_splits_on_a_char_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        // '→' is 3 bytes and does not divide the ceiling evenly, so a naive
        // byte slice at SPILL_KEEP_MAX_BYTES lands inside a codepoint.
        let oversized = "→".repeat(SPILL_KEEP_MAX_BYTES);

        let path = write_spill_file(dir.path(), "tool-spill", "probe", &oversized).expect("write");

        let written = std::fs::read_to_string(&path).expect("read must be valid UTF-8");
        assert!(written.contains("spill truncated"));
    }

    /// Owner-only from creation: the spill holds output that redaction may
    /// have missed, so it must never be world-readable even briefly.
    #[cfg(unix)]
    #[test]
    fn spill_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_spill_file(dir.path(), "tool-spill", "probe", "secret").expect("write");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "spill must be owner read/write only");
    }
}
