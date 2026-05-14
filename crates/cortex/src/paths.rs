//! Canonical filesystem paths under `$BORG_HOME` (default `~/.borg`).
//!
//! Single source of truth for every path inside the borg data directory.
//! Crates that need a path under the data dir call `resolve_borg_home()` once
//! (or accept a `&Path` from a caller that did) and then use the helpers below
//! — never compose `home.join("memory")` by hand.
//!
//! # Two env vars, one resolution order
//!
//! Historically the codebase used `BORG_DATA_DIR` for cortex (test isolation
//! sets it heavily) and `BORG_HOME` for the daemon CLI flag. Both are honored
//! to preserve existing test fixtures and operator muscle memory:
//!
//! 1. Explicit `override_path` argument (CLI flag).
//! 2. `$BORG_HOME` env var.
//! 3. `$BORG_DATA_DIR` env var (legacy; kept for cortex test fixtures).
//! 4. `~/.borg`.
//!
//! `BORG_HOME` is the preferred name for new docs and tooling. Once test
//! fixtures migrate, `BORG_DATA_DIR` can be retired without breaking ops.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::constants::{DB_FILE, HEARTBEAT_FILE, IDENTITY_FILE, MEMORY_INDEX_FILE};

/// Env var read by the daemon CLI flag and operator docs.
pub const BORG_HOME_ENV: &str = "BORG_HOME";

/// Legacy env var still honored for cortex test fixtures.
//
// TODO: retire `BORG_DATA_DIR` once the ~50 test sites that set it migrate
// to `BORG_HOME`. The resolver above places it second so `BORG_HOME` wins
// when both are set — the migration can be staged.
pub const BORG_DATA_DIR_ENV: &str = "BORG_DATA_DIR";

/// Typed enumeration of every well-known subdirectory under `$BORG_HOME`.
///
/// Use [`subdir`] to materialize a `PathBuf`. Adding a new subdir means adding
/// a variant here — typos at call sites become compile errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorgSubdir {
    /// `memory/` — short-term scratch and embedding artifacts.
    Memory,
    /// `skills/` — user-installed skills (overrides built-ins).
    Skills,
    /// `scripts/` — user-defined scripts surfaced to the agent.
    Scripts,
    /// `channels/` — script-based channel integrations.
    Channels,
    /// `tools/` — user-defined runtime tools.
    Tools,
    /// `logs/` — TUI, daemon, and date-stamped JSONL logs.
    Logs,
    /// `sessions/` — per-session transcript artifacts.
    Sessions,
    /// `secrets/` — KEK material and bootstrap secrets (0700 dir).
    Secrets,
    /// `backups/` — `borg backup` tarballs.
    Backups,
    /// `cache/` — re-derivable caches (embedding, compiled WASM modules).
    Cache,
}

impl BorgSubdir {
    /// Stable on-disk name. Never rename — operators have these in scripts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Skills => "skills",
            Self::Scripts => "scripts",
            Self::Channels => "channels",
            Self::Tools => "tools",
            Self::Logs => "logs",
            Self::Sessions => "sessions",
            Self::Secrets => "secrets",
            Self::Backups => "backups",
            Self::Cache => "cache",
        }
    }
}

/// Resolve `$BORG_HOME` from CLI override, env vars, or default.
///
/// Creates the directory (and parents) if it does not exist. See module docs
/// for the resolution order. Returns the canonical `PathBuf`.
pub fn resolve_borg_home(override_path: Option<&Path>) -> Result<PathBuf> {
    let home = if let Some(p) = override_path {
        p.to_path_buf()
    } else if let Some(p) = std::env::var_os(BORG_HOME_ENV) {
        PathBuf::from(p)
    } else if let Some(p) = std::env::var_os(BORG_DATA_DIR_ENV) {
        PathBuf::from(p)
    } else {
        dirs::home_dir()
            .context("could not determine home directory")?
            .join(".borg")
    };
    if !home.exists() {
        std::fs::create_dir_all(&home)
            .with_context(|| format!("creating BORG_HOME at {}", home.display()))?;
    }
    Ok(home)
}

/// Read-only variant of [`resolve_borg_home`]: same resolution order, but
/// does NOT create the directory. Use this from cortex contexts (config
/// load, doctor checks) that must work before the daemon has bootstrapped.
pub fn resolve_borg_home_readonly(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os(BORG_HOME_ENV) {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os(BORG_DATA_DIR_ENV) {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::home_dir()
        .context("could not determine home directory")?
        .join(".borg"))
}

/// Compose a subdirectory path under `home`.
pub fn subdir(home: &Path, kind: BorgSubdir) -> PathBuf {
    home.join(kind.as_str())
}

// ── Well-known files ──────────────────────────────────────────────────────

/// `$BORG_HOME/borg.db` — main SQLite database.
pub fn db_path(home: &Path) -> PathBuf {
    home.join(DB_FILE)
}

/// `$BORG_HOME/IDENTITY.md` — agent persona scaffold.
pub fn identity_path(home: &Path) -> PathBuf {
    home.join(IDENTITY_FILE)
}

/// `$BORG_HOME/MEMORY.md` — long-term memory index.
pub fn memory_index_path(home: &Path) -> PathBuf {
    home.join(MEMORY_INDEX_FILE)
}

/// `$BORG_HOME/HEARTBEAT.md` — proactive check-in checklist.
pub fn heartbeat_path(home: &Path) -> PathBuf {
    home.join(HEARTBEAT_FILE)
}

/// `$BORG_HOME/borgd.sock` — main gRPC UDS.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("borgd.sock")
}

/// `$BORG_HOME/borgd.pid` — daemon PID/lockfile.
pub fn pid_path(home: &Path) -> PathBuf {
    home.join("borgd.pid")
}

/// `$BORG_HOME/scheduler.tick.lock` — per-tick advisory lock for the
/// scheduler dispatcher.
pub fn tick_lock_path(home: &Path) -> PathBuf {
    home.join("scheduler.tick.lock")
}

/// `$BORG_HOME/borgd-pair.sock` — plaintext SPAKE2 bootstrap socket.
/// Distinct from the main UDS so a leaked FD on the pairing surface
/// cannot serve the rest of the gRPC API.
pub fn pairing_socket_path(home: &Path) -> PathBuf {
    home.join("borgd-pair.sock")
}

/// `$BORG_HOME/secrets/kek.enc` — passphrase-encrypted file KEK store
/// (used when `security.kek_backend = file`).
pub fn kek_file_path(home: &Path) -> PathBuf {
    subdir(home, BorgSubdir::Secrets).join("kek.enc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdir_names_are_lowercase_kebab_or_single_word() {
        for kind in [
            BorgSubdir::Memory,
            BorgSubdir::Skills,
            BorgSubdir::Scripts,
            BorgSubdir::Channels,
            BorgSubdir::Tools,
            BorgSubdir::Logs,
            BorgSubdir::Sessions,
            BorgSubdir::Secrets,
            BorgSubdir::Backups,
            BorgSubdir::Cache,
        ] {
            let s = kind.as_str();
            assert!(!s.is_empty(), "subdir name must not be empty");
            assert!(
                s.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "subdir {s:?} must be lowercase ASCII (no slashes, no spaces) — \
                 operator scripts depend on stable names"
            );
        }
    }

    #[test]
    fn subdir_names_are_unique() {
        let all = [
            BorgSubdir::Memory,
            BorgSubdir::Skills,
            BorgSubdir::Scripts,
            BorgSubdir::Channels,
            BorgSubdir::Tools,
            BorgSubdir::Logs,
            BorgSubdir::Sessions,
            BorgSubdir::Secrets,
            BorgSubdir::Backups,
            BorgSubdir::Cache,
        ];
        let mut seen = std::collections::HashSet::new();
        for kind in all {
            assert!(
                seen.insert(kind.as_str()),
                "duplicate subdir name {:?}",
                kind.as_str()
            );
        }
    }

    /// Serializes tests that mutate `BORG_HOME` / `BORG_DATA_DIR` so the
    /// cargo-test thread pool doesn't race on process-wide env state.
    fn paths_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_data: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(home: Option<&Path>, data: Option<&Path>) -> Self {
            let lock = paths_env_lock();
            let prev_home = std::env::var_os(BORG_HOME_ENV);
            let prev_data = std::env::var_os(BORG_DATA_DIR_ENV);
            unsafe {
                match home {
                    Some(p) => std::env::set_var(BORG_HOME_ENV, p),
                    None => std::env::remove_var(BORG_HOME_ENV),
                }
                match data {
                    Some(p) => std::env::set_var(BORG_DATA_DIR_ENV, p),
                    None => std::env::remove_var(BORG_DATA_DIR_ENV),
                }
            }
            Self {
                _lock: lock,
                prev_home,
                prev_data,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_home.take() {
                    Some(v) => std::env::set_var(BORG_HOME_ENV, v),
                    None => std::env::remove_var(BORG_HOME_ENV),
                }
                match self.prev_data.take() {
                    Some(v) => std::env::set_var(BORG_DATA_DIR_ENV, v),
                    None => std::env::remove_var(BORG_DATA_DIR_ENV),
                }
            }
        }
    }

    #[test]
    fn explicit_override_wins_over_both_env_vars() {
        // With BOTH env vars set to different paths, an explicit override
        // still wins. Locks out env race with the rest of the test suite.
        let home_env = tempfile::tempdir().unwrap();
        let data_env = tempfile::tempdir().unwrap();
        let _g = EnvGuard::set(Some(home_env.path()), Some(data_env.path()));
        let override_path = std::path::PathBuf::from("/tmp/explicit-override-12345");
        let home = resolve_borg_home_readonly(Some(&override_path)).unwrap();
        assert_eq!(home, override_path);
    }

    #[test]
    fn borg_home_takes_precedence_over_borg_data_dir() {
        // Regression guard for the user-observable behavior change introduced
        // when `Config::data_dir()` was unified onto `resolve_borg_home_*`:
        // previously only `BORG_DATA_DIR` was honored. New rule: `BORG_HOME`
        // wins. If this ordering reverts, processes started by `borgd --listen`
        // (which sets `BORG_HOME` via clap-env) would silently fall through to
        // `BORG_DATA_DIR` from a parent shell, splitting state across two dirs.
        let home_env = tempfile::tempdir().unwrap();
        let data_env = tempfile::tempdir().unwrap();
        assert_ne!(home_env.path(), data_env.path());
        let _g = EnvGuard::set(Some(home_env.path()), Some(data_env.path()));
        let resolved = resolve_borg_home_readonly(None).unwrap();
        assert_eq!(resolved, home_env.path());
    }

    #[test]
    fn borg_data_dir_used_when_borg_home_unset() {
        // The legacy env var still resolves when BORG_HOME is absent — this
        // keeps the ~50 cortex test fixtures that set BORG_DATA_DIR working
        // without changes.
        let data_env = tempfile::tempdir().unwrap();
        let _g = EnvGuard::set(None, Some(data_env.path()));
        let resolved = resolve_borg_home_readonly(None).unwrap();
        assert_eq!(resolved, data_env.path());
    }

    #[test]
    fn well_known_files_live_directly_under_home() {
        // Regression guard: db_path / identity_path / etc. must NOT live
        // inside a subdir — operators expect them at the top of $BORG_HOME.
        let home = PathBuf::from("/some/borg");
        assert_eq!(db_path(&home).parent().unwrap(), home);
        assert_eq!(identity_path(&home).parent().unwrap(), home);
        assert_eq!(memory_index_path(&home).parent().unwrap(), home);
        assert_eq!(heartbeat_path(&home).parent().unwrap(), home);
        assert_eq!(socket_path(&home).parent().unwrap(), home);
        assert_eq!(pid_path(&home).parent().unwrap(), home);
        assert_eq!(pairing_socket_path(&home).parent().unwrap(), home);
        assert_eq!(tick_lock_path(&home).parent().unwrap(), home);
    }

    #[test]
    fn kek_file_lives_under_secrets_subdir() {
        // The KEK file is the one well-known path that intentionally lives
        // under a subdir (`secrets/`). Pinning this keeps the FileKekStore
        // contract aligned with operator docs.
        let home = PathBuf::from("/some/borg");
        let kek = kek_file_path(&home);
        assert_eq!(kek.parent().unwrap(), home.join("secrets"));
        assert_eq!(kek.file_name().unwrap(), "kek.enc");
    }
}
