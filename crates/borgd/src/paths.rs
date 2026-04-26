//! Filesystem path helpers for borgd.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Resolve `$BORG_HOME` from CLI override or default `~/.borg`.
/// Creates the directory if it does not exist.
pub fn resolve_borg_home(override_path: Option<&Path>) -> Result<PathBuf> {
    let home = match override_path {
        Some(p) => p.to_path_buf(),
        None => {
            let h = dirs::home_dir().context("could not determine home directory")?;
            h.join(".borg")
        }
    };
    if !home.exists() {
        std::fs::create_dir_all(&home)
            .with_context(|| format!("creating BORG_HOME at {}", home.display()))?;
    }
    Ok(home)
}

/// Path to the UDS socket inside `$BORG_HOME`.
pub fn socket_path(borg_home: &Path) -> PathBuf {
    borg_home.join("borgd.sock")
}

/// Path to the PID/lock file inside `$BORG_HOME`.
pub fn pid_path(borg_home: &Path) -> PathBuf {
    borg_home.join("borgd.pid")
}
