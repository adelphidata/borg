//! `borg export <session_id> [--format json|csv|txt] [--output PATH]`
//!
//! Thin wrapper over `borg_core::export::export_session`. The same function
//! powers the TUI `/export` command and the `e` key in the `/sessions` popup,
//! so formatting stays consistent across surfaces.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use borg_core::db::Database;
use borg_core::export::{export_session, ExportFormat, ExportOptions};

/// Run `borg export`. With `output=None`, writes to stdout (pipeable).
pub(crate) fn run_export(session_id: &str, format: &str, output: Option<PathBuf>) -> Result<()> {
    let format = ExportFormat::from_str(format)?;
    let db = Database::open().context("opening database")?;
    let (rendered, suggested) = export_session(&db, session_id, ExportOptions { format })?;

    match output {
        Some(path) => {
            std::fs::write(&path, &rendered)
                .with_context(|| format!("writing export to {}", path.display()))?;
            eprintln!("Exported session {session_id} → {}", path.display());
        }
        None => {
            // stdout stays pipeable; suggested filename only mentioned on stderr.
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(rendered.as_bytes())
                .context("writing export to stdout")?;
            if !rendered.ends_with('\n') {
                lock.write_all(b"\n").ok();
            }
            let _ = suggested; // suggestion unused when piping
        }
    }
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
}
