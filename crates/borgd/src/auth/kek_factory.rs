//! KEK backend selection at daemon startup.
//!
//! `borgd` cannot launch without a working `KekStore` — the root CA private
//! key has to live somewhere. On desktop the OS keychain is the right
//! answer; on a headless Linux VPS without dbus + secret-service the
//! `KeychainStore` round-trip fails immediately and the daemon used to
//! refuse to start. This factory makes both paths bootable:
//!
//! 1. `KekBackend::OsKeychain` — explicit; bare `KeychainStore`.
//! 2. `KekBackend::File`       — explicit; passphrase-encrypted file at
//!    `~/.borg/secrets/kek.enc`. Passphrase comes from
//!    `BORG_KEK_PASSPHRASE` (set by systemd `EnvironmentFile=`).
//! 3. `KekBackend::Auto`       — probe `KeychainStore` with a sentinel
//!    round-trip; on failure fall through to `File`. The resolved choice
//!    is written back to `security.kek_backend` so subsequent starts skip
//!    the probe and the posture system gets a stable signal.

use anyhow::{anyhow, Context, Result};
use cortex::config::security::KekBackend;
use cortex::config::Config as CoreConfig;
use cortex::db::Database;
use identity::{FileKekStore, KekStore, KeychainStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Account name used to probe the OS keychain. Distinct from the real
/// CA / server-cert accounts so a half-broken keychain that read-only
/// rejects some accounts but accepts others doesn't get a false positive.
const PROBE_ACCOUNT: &str = "borg-probe";
const PROBE_VALUE: &[u8] = b"v1";

/// Env var read for `KekBackend::File`. Setting it via systemd
/// `EnvironmentFile=` keeps the passphrase out of the unit file and
/// readable only by root (0600 on the env file).
pub const PASSPHRASE_ENV: &str = "BORG_KEK_PASSPHRASE";

/// File path under `$BORG_HOME` for the encrypted KEK blob.
pub fn kek_file_path(borg_home: &Path) -> PathBuf {
    borg_home.join("secrets").join("kek.enc")
}

/// Build the KEK store for this daemon process. Resolves `Auto` and
/// persists the chosen backend so the posture snapshot reads a concrete
/// value on the very next call.
pub fn build_kek_store(
    borg_home: &Path,
    cfg: &CoreConfig,
    settings_db: &Database,
) -> Result<Arc<dyn KekStore>> {
    match cfg.security.kek_backend {
        KekBackend::OsKeychain => {
            tracing::info!("KEK backend: os_keychain (explicit)");
            Ok(Arc::new(KeychainStore::new()))
        }
        KekBackend::File => {
            tracing::info!("KEK backend: file (explicit)");
            Ok(open_file_store(borg_home)?)
        }
        KekBackend::Auto => {
            if probe_keychain() {
                tracing::info!("KEK backend: os_keychain (auto-detected)");
                persist_resolved_backend(settings_db, KekBackend::OsKeychain);
                Ok(Arc::new(KeychainStore::new()))
            } else {
                tracing::warn!(
                    "KEK backend: file (OS keychain unavailable — \
                    posture will be Reduced; install gnome-keyring + dbus or \
                    run `borg service install --secure` to upgrade)"
                );
                let store = open_file_store(borg_home)?;
                persist_resolved_backend(settings_db, KekBackend::File);
                Ok(store)
            }
        }
    }
}

/// Probe the OS keychain by writing-and-reading a sentinel. Any error
/// (unavailable backend, missing dbus session, locked keyring, …) returns
/// `false` so the caller falls back to `File`.
fn probe_keychain() -> bool {
    let store = KeychainStore::new();
    if store.put(PROBE_ACCOUNT, PROBE_VALUE).is_err() {
        return false;
    }
    let got = store.get(PROBE_ACCOUNT);
    // Always attempt cleanup so we don't pollute the keychain even if
    // get/compare diverged.
    let _ = store.delete(PROBE_ACCOUNT);
    matches!(got, Ok(v) if v == PROBE_VALUE)
}

fn open_file_store(borg_home: &Path) -> Result<Arc<dyn KekStore>> {
    let passphrase = std::env::var(PASSPHRASE_ENV).map_err(|_| {
        anyhow!(
            "{PASSPHRASE_ENV} is required for the file KEK backend. \
            Run `borg service install` to template a systemd EnvironmentFile, \
            or export the variable before launching borgd."
        )
    })?;
    if passphrase.is_empty() {
        return Err(anyhow!(
            "{PASSPHRASE_ENV} is empty; refusing to open file KEK store"
        ));
    }
    let path = kek_file_path(borg_home);
    let store = FileKekStore::open(&path, passphrase.into_bytes())
        .with_context(|| format!("open file KEK store at {}", path.display()))?;
    Ok(Arc::new(store))
}

/// Best-effort persistence of the resolved backend.
///
/// Race semantics: two daemons starting simultaneously (e.g. systemd
/// retry loop overlapping a manual `borg service install`) will both
/// probe and both write — last-writer-wins is fine because both
/// converge on the same answer for a given host. A failure here is
/// non-fatal: the daemon is already up with a working KEK; the *next*
/// daemon start that calls `Config::load_from_db` after this commit
/// observes the resolved value. If the writer crashes between probe
/// and commit, the next start re-probes — same outcome, slightly
/// slower bootstrap. We log so a chronic write failure surfaces in
/// `borg doctor`.
fn persist_resolved_backend(db: &Database, backend: KekBackend) {
    let value = backend.to_string();
    if let Err(e) = db.set_setting("security.kek_backend", &value) {
        tracing::warn!(
            "could not persist resolved kek_backend={value}: {e} \
            (probe will repeat on next start)"
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::redundant_closure_for_method_calls
)]
mod tests {
    use super::*;
    use cortex::config::Config;
    use rusqlite::Connection;
    use std::sync::Mutex;

    /// Process-wide guard: tests in this module mutate the global
    /// `BORG_KEK_PASSPHRASE` env var, and Cargo runs them on a shared
    /// thread pool. Without serialization a "remove_var" in one test races
    /// with a "set_var" in another and produces flakes that look like
    /// production bugs.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_db_and_home() -> (tempfile::TempDir, Database) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Database::from_connection(Connection::open_in_memory().expect("open in-mem db"))
            .expect("init test db");
        (tmp, db)
    }

    #[test]
    fn explicit_file_backend_uses_file_store_and_round_trips() {
        // Reflects the `--secure` non-Debian fallback path: the installer
        // wrote `kek_backend=file`, supplied a passphrase via env, and the
        // daemon must boot a working FileKekStore even when the OS keychain
        // is unavailable in CI.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (home, db) = fresh_db_and_home();
        std::env::set_var(PASSPHRASE_ENV, "test-passphrase");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let store =
            build_kek_store(home.path(), &cfg, &db).expect("build file store should succeed");
        store.put("ca-key", b"PEM").expect("put");
        assert_eq!(store.get("ca-key").expect("get"), b"PEM".to_vec());
        std::env::remove_var(PASSPHRASE_ENV);
    }

    #[test]
    fn explicit_file_backend_errors_when_passphrase_missing() {
        // Real failure mode: systemd unit lost the EnvironmentFile, daemon
        // restarts, and we must hard-fail with a clear message rather than
        // silently boot with a default-passphrase empty store (which would
        // mint a fresh CA and invalidate every paired device).
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (home, db) = fresh_db_and_home();
        std::env::remove_var(PASSPHRASE_ENV);
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let result = build_kek_store(home.path(), &cfg, &db);
        match result {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains(PASSPHRASE_ENV),
                    "error must name the missing env var; got: {msg}"
                );
            }
            Ok(_) => panic!("must fail without {PASSPHRASE_ENV}"),
        }
    }

    #[test]
    fn explicit_file_backend_errors_when_passphrase_is_empty() {
        // Defense-in-depth: empty-string env is a common mistake (e.g. a
        // missing `=value` in EnvironmentFile). We don't want to encrypt
        // with an empty passphrase silently.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (home, db) = fresh_db_and_home();
        std::env::set_var(PASSPHRASE_ENV, "");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let result = build_kek_store(home.path(), &cfg, &db);
        std::env::remove_var(PASSPHRASE_ENV);
        assert!(result.is_err(), "must reject empty passphrase");
    }

    #[test]
    fn kek_file_path_is_under_borg_home_secrets() {
        // Posture math depends on the file living at `<BORG_HOME>/secrets/kek.enc`
        // — `borg service install --secure` writes the passphrase env file
        // alongside it. A drift here would hide the file from `borg doctor`
        // and break the regression integration test.
        let p = kek_file_path(Path::new("/var/lib/borg"));
        assert!(p.ends_with("secrets/kek.enc"), "got {p:?}");
    }

    /// Headless-boot integration: build a file-backed KEK, write the
    /// CA private key under it, drop the store, rebuild against the
    /// same on-disk file + passphrase, and assert the same private key
    /// comes back. Catches "fresh CA on every boot" regressions that
    /// would invalidate every paired device.
    #[test]
    fn file_backend_persists_kek_across_restart() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        std::env::set_var(PASSPHRASE_ENV, "integration-passphrase");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;

        // Round 1: build store, write a sentinel CA private key.
        let db1 =
            Database::from_connection(Connection::open_in_memory().expect("db")).expect("init db1");
        let store1 = build_kek_store(home, &cfg, &db1).expect("kek round 1");
        let sentinel_key = b"-----BEGIN PRIVATE KEY-----\nFAKE_CA_PEM\n-----END PRIVATE KEY-----";
        store1.put("root-ca-key", sentinel_key).expect("put");
        drop(store1);

        // KEK landed at the documented path with restrictive perms.
        let kek_path = kek_file_path(home);
        assert!(kek_path.exists(), "kek.enc must be on disk after round 1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&kek_path)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "kek.enc must be 0600 on disk");
        }

        // Round 2: rebuild from disk + same passphrase. The CA private
        // key must round-trip — that's the property a paired device
        // depends on across daemon restarts.
        let db2 =
            Database::from_connection(Connection::open_in_memory().expect("db")).expect("init db2");
        let store2 = build_kek_store(home, &cfg, &db2).expect("kek round 2");
        let recovered = store2
            .get("root-ca-key")
            .expect("round-2 store must return the sentinel KEK entry");
        assert_eq!(
            recovered,
            sentinel_key.to_vec(),
            "CA private key must survive a daemon restart on the file backend"
        );
        std::env::remove_var(PASSPHRASE_ENV);
    }
}
