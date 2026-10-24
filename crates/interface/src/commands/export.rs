//! `borg export <session_id> [--format json|csv|txt|md] [--output PATH] [--no-redact]`
//!
//! Thin wrapper over `Admin.ExportSession` (daemon-side) which in turn calls
//! `cortex::export::export_session`. Routing through the daemon keeps the
//! cli a single-writer client and matches the same code path used by the TUI
//! `/export` command and the `e` key in the `/sessions` popup.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use cortex::export::ExportFormat;

use crate::daemon_client::connect_or_spawn;

/// Run `borg export`. With `output=None`, writes to stdout (pipeable).
/// `no_redact` opts out of the default fail-closed secret redaction.
pub(crate) fn run_export(
    session_id: &str,
    format: &str,
    output: Option<PathBuf>,
    no_redact: bool,
) -> Result<()> {
    // Validate locally before the round-trip. The daemon validates again, but
    // pre-validation keeps a typo on `--format` from showing up as an opaque
    // tonic error and lets the format-rejection unit test stay synchronous.
    let _ = ExportFormat::from_str(format)?;

    let resp = crate::async_bridge::block_on(async {
        let mut client = connect_or_spawn().await?;
        client.export_session(session_id, format, no_redact).await
    })?;

    match output {
        Some(path) => {
            write_file_no_clobber(&path, &resp.content)?;
            eprintln!("Exported session {session_id} → {}", path.display());
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&resp.content)
                .context("writing export to stdout")?;
            if !resp.content.ends_with(b"\n") {
                // Best-effort newline so terminal prompts don't wedge on the
                // last line; ignoring the error matches the prior behavior.
                let _ = lock.write_all(b"\n");
            }
        }
    }
    Ok(())
}

/// Write `contents` to `path`, atomically refusing if the file already exists.
/// `create_new` closes the TOCTOU window that `exists() + write()` opens.
fn write_file_no_clobber(path: &std::path::Path, contents: &[u8]) -> Result<()> {
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
    f.write_all(contents)
        .with_context(|| format!("writing export to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_format_is_rejected_before_daemon_round_trip() {
        // The validation must fire BEFORE we try to spawn / connect to the
        // daemon — otherwise a typo on `--format` would surface as a UDS
        // connection error instead of a useful "unsupported format" message.
        // Run on a single-threaded runtime so the assertion fires whether or
        // not the harness already has one ambient.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let err = rt
            .block_on(async {
                tokio::task::spawn_blocking(|| run_export("any-id", "yaml", None, false)).await
            })
            .expect("spawn_blocking")
            .unwrap_err();
        assert!(
            err.to_string().contains("unsupported export format"),
            "expected format rejection, got: {err}"
        );
    }

    #[test]
    fn write_no_clobber_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_file_no_clobber(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_no_clobber_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, "preexisting").unwrap();
        let err = write_file_no_clobber(&path, b"new").unwrap_err();
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "expected overwrite refusal, got: {err}"
        );
        // Original content must be untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preexisting");
    }
}
