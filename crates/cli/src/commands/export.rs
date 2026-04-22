//! `borg export <session_id> [--format json|csv|txt] [--output PATH]`
//!
//! Thin wrapper over `borg_core::export::export_session`. The same function
//! powers the TUI `/export` command and the `e` key in the `/sessions` popup,
//! so formatting stays consistent across surfaces.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use borg_core::db::Database;
use borg_core::export::{export_session, ExportFormat, ExportOptions};

/// Run `borg export`. With `output=None`, writes to stdout (pipeable).
pub(crate) fn run_export(session_id: &str, format: &str, output: Option<PathBuf>) -> Result<()> {
    let format = ExportFormat::from_str(format)?;
    let db = Database::open().context("opening database")?;
    let (rendered, _suggested) = export_session(&db, session_id, ExportOptions { format })?;

    match output {
        Some(path) => {
            write_file_no_clobber(&path, &rendered)?;
            eprintln!("Exported session {session_id} → {}", path.display());
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(rendered.as_bytes())
                .context("writing export to stdout")?;
            if !rendered.ends_with('\n') {
                lock.write_all(b"\n").ok();
            }
        }
    }
    Ok(())
}

/// Write `contents` to `path`, atomically refusing if the file already exists.
/// `create_new` closes the TOCTOU window that `exists() + write()` opens.
fn write_file_no_clobber(path: &std::path::Path, contents: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "creating export file {} (refusing to overwrite existing)",
                path.display()
            )
        })?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("writing export to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_format_is_rejected_before_db_work() {
        // The validation must fire before Database::open — otherwise a typo
        // on `--format` would produce a misleading "could not open DB" error.
        let err = run_export("any-id", "yaml", None).unwrap_err();
        assert!(err.to_string().contains("unsupported export format"));
    }

    #[test]
    fn write_no_clobber_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_file_no_clobber(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_no_clobber_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, "preexisting").unwrap();
        let err = write_file_no_clobber(&path, "new").unwrap_err();
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "expected overwrite refusal, got: {err}"
        );
        // Original content must be untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preexisting");
    }
}
