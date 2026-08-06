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
use cortex::paths::{self, BorgSubdir};
#[cfg(unix)]
use cortex::util::unix_ext::set_unix_mode;
use identity::{FileKekStore, KekStore, KeychainStore};
use std::path::Path;
use std::sync::Arc;

/// Filename of the passphrase env file written under `<BORG_HOME>/secrets/`.
/// Same path the systemd installer templates as `EnvironmentFile=`, so a
/// passphrase auto-provisioned by `Auto` here is picked up by future
/// systemd-managed restarts without a separate `borg service install` run.
const KEK_ENV_FILE: &str = "kek.env";

/// Account name used to probe the OS keychain. Distinct from the real
/// CA / server-cert accounts so a half-broken keychain that read-only
/// rejects some accounts but accepts others doesn't get a false positive.
const PROBE_ACCOUNT: &str = "borg-probe";
const PROBE_VALUE: &[u8] = b"v1";

/// Env var read for `KekBackend::File`. Setting it via systemd
/// `EnvironmentFile=` keeps the passphrase out of the unit file and
/// readable only by root (0600 on the env file).
pub const PASSPHRASE_ENV: &str = "BORG_KEK_PASSPHRASE";

pub use cortex::paths::kek_file_path;

/// Build the KEK store for this daemon process. Resolves `Auto` and
/// persists the chosen backend so the posture snapshot reads a concrete
/// value on the very next call.
pub async fn build_kek_store(
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
                // Deliberately NOT persisted. Latching `auto` to
                // `os_keychain` removed the fallback from every later boot:
                // the explicit arm returns a bare `KeychainStore` with no
                // probe, so a host whose keyring is merely *transiently*
                // unavailable — a systemd user unit starting before the login
                // keyring is unlocked — hard-fails `ensure_root_ca` and
                // restart-loops the daemon. Staying `auto` costs one cheap
                // probe per start and preserves the always-boots guarantee.
                // The `File` resolution below IS persisted: that probe is the
                // expensive one, and it is the headless case worth caching.
                Ok(Arc::new(KeychainStore::new()))
            } else {
                tracing::warn!(
                    "KEK backend: file (OS keychain unavailable — \
                    posture will be Reduced; install gnome-keyring + dbus or \
                    run `borg service install --secure` to upgrade)"
                );
                // Auto mode is the "always boots" guarantee. If the
                // passphrase is missing we provision one rather than
                // hard-fail — explicit `File` still hard-fails because
                // a missing passphrase there means EnvironmentFile got
                // lost, and silently rewriting it would invalidate the
                // existing encrypted blob.
                let store = open_file_store_auto(borg_home)?;
                persist_resolved_backend(settings_db, KekBackend::File).await;
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

/// File-store opener for the `Auto` fallback path. Differs from
/// `open_file_store` in that a missing passphrase causes a fresh one to
/// be generated and written to `<BORG_HOME>/secrets/kek.env` (0600)
/// rather than hard-failing — this is what makes the "always boots"
/// guarantee real on a host where neither systemd nor a prior installer
/// has set up the env var. Reads from an existing kek.env if present.
fn open_file_store_auto(borg_home: &Path) -> Result<Arc<dyn KekStore>> {
    let passphrase = match std::env::var(PASSPHRASE_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => load_or_provision_passphrase(borg_home)?,
    };
    let path = kek_file_path(borg_home);
    let store = FileKekStore::open(&path, passphrase.into_bytes())
        .with_context(|| format!("open file KEK store at {}", path.display()))?;
    Ok(Arc::new(store))
}

/// Read the passphrase from `<BORG_HOME>/secrets/kek.env` (parsing the
/// systemd-style `BORG_KEK_PASSPHRASE=...` line) or generate a new one
/// and write it. The env file is created at 0600 under a 0700 parent.
fn load_or_provision_passphrase(borg_home: &Path) -> Result<String> {
    let secrets_dir = paths::subdir(borg_home, BorgSubdir::Secrets);
    let env_path = secrets_dir.join(KEK_ENV_FILE);
    if let Ok(body) = std::fs::read_to_string(&env_path) {
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix(&format!("{PASSPHRASE_ENV}=")) {
                let v = rest.trim();
                if !v.is_empty() {
                    tracing::info!(
                        "auto KEK fallback: loaded passphrase from {}",
                        env_path.display()
                    );
                    return Ok(v.to_string());
                }
            }
        }
        tracing::warn!(
            "auto KEK fallback: {} exists but does not declare {PASSPHRASE_ENV}; provisioning fresh",
            env_path.display()
        );
    }
    std::fs::create_dir_all(&secrets_dir)
        .with_context(|| format!("create {}", secrets_dir.display()))?;
    #[cfg(unix)]
    set_unix_mode(&secrets_dir, 0o700)?;
    let passphrase = generate_passphrase();
    let body = format!("{PASSPHRASE_ENV}={passphrase}\n");
    write_file_0600(&env_path, body.as_bytes())
        .with_context(|| format!("write {}", env_path.display()))?;
    tracing::warn!(
        "auto KEK fallback: provisioned new passphrase at {} (0600). \
        SECURITY: this passphrase sits next to kek.enc in the same secrets/ \
        directory, so at-rest encryption gives ~no protection against anyone \
        who can read that directory (same-UID process, stolen backup) — this \
        is the accepted trade-off for booting without an OS keychain. For real \
        at-rest protection, install an OS keychain (gnome-keyring + dbus) or \
        run `borg service install --secure` to move the passphrase into a \
        root-only systemd EnvironmentFile. Back up this file — losing it \
        invalidates the encrypted KEK and requires re-pairing devices.",
        env_path.display()
    );
    Ok(passphrase)
}

fn generate_passphrase() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn write_file_0600(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(body)?;
    f.sync_all()?;
    Ok(())
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
async fn persist_resolved_backend(db: &Database, backend: KekBackend) {
    // Only commit if the stored setting is still `auto` — guards against
    // a concurrent installer (`borg service install --secure`) writing
    // `os_keychain` while a daemon-side probe transiently fails and
    // would otherwise stomp it back to `file`. If the user has made an
    // explicit choice, respect it.
    match db.get_setting("security.kek_backend").await {
        Ok(Some(v)) if v != "auto" => {
            tracing::debug!(
                "kek_backend already set to {v}; skipping auto-resolve persist of {backend}"
            );
            return;
        }
        Ok(_) => {}
        Err(e) => {
            // Init race on first daemon start (settings table not yet seeded).
            // Caller falls through to a write attempt; not operator-actionable.
            tracing::debug!("read security.kek_backend before persist failed: {e}");
            return;
        }
    }
    let value = backend.to_string();
    if let Err(e) = db.set_setting("security.kek_backend", &value).await {
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
    use tokio::sync::Mutex;

    /// Process-wide guard: tests in this module mutate the global
    /// `BORG_KEK_PASSPHRASE` env var, and Cargo runs them on a shared
    /// thread pool. Without serialization a "remove_var" in one test races
    /// with a "set_var" in another and produces flakes that look like
    /// production bugs. Async-aware mutex so the guard can safely live
    /// across `.await` points without tripping `clippy::await_holding_lock`.
    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    async fn fresh_db_and_home() -> (tempfile::TempDir, Database) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Database::test_db().await;
        (tmp, db)
    }

    /// Auto fallback must auto-provision a passphrase + kek.env when
    /// neither the env var nor a prior env file exist. Without this the
    /// "always boots" headless guarantee breaks: a user who runs
    /// `borgd` directly (no `borg service install`) on a CI/dev box
    /// where the OS keychain isn't usable would see a hard error.
    #[tokio::test]
    async fn auto_fallback_provisions_kek_env_when_missing() {
        let _g = ENV_LOCK.lock().await;
        std::env::remove_var(PASSPHRASE_ENV);
        let (home, _db) = fresh_db_and_home().await;
        let store =
            open_file_store_auto(home.path()).expect("auto fallback must provision and open");
        store.put("ca-key", b"PEM").expect("put");
        assert_eq!(store.get("ca-key").expect("get"), b"PEM".to_vec());
        let env_path = home.path().join("secrets").join(KEK_ENV_FILE);
        assert!(env_path.exists(), "auto must write kek.env");
        let body = std::fs::read_to_string(&env_path).expect("read");
        assert!(body.starts_with(&format!("{PASSPHRASE_ENV}=")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&env_path)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    /// Auto fallback must REUSE an existing kek.env rather than
    /// regenerate the passphrase — regenerating would invalidate the
    /// already-encrypted KEK blob and re-mint the CA on every boot,
    /// invalidating every paired device.
    #[tokio::test]
    async fn auto_fallback_reads_existing_kek_env_without_regenerating() {
        let _g = ENV_LOCK.lock().await;
        std::env::remove_var(PASSPHRASE_ENV);
        let (home, _db) = fresh_db_and_home().await;
        let secrets = home.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        let env_path = secrets.join(KEK_ENV_FILE);
        std::fs::write(&env_path, format!("{PASSPHRASE_ENV}=preexisting-pass\n")).unwrap();

        let store1 = open_file_store_auto(home.path()).expect("round 1");
        store1.put("ca-key", b"PEM").expect("put");
        drop(store1);

        let body = std::fs::read_to_string(&env_path).unwrap();
        assert_eq!(body, format!("{PASSPHRASE_ENV}=preexisting-pass\n"));

        let store2 = open_file_store_auto(home.path()).expect("round 2");
        assert_eq!(store2.get("ca-key").expect("get"), b"PEM".to_vec());
    }

    /// `persist_resolved_backend` MUST NOT overwrite an explicit
    /// `os_keychain` or `file` setting. Without this guard, a transient
    /// probe failure in a daemon restart loop could clobber the
    /// installer's choice and flip the deploy into a state where it
    /// can't find its passphrase.
    #[tokio::test]
    async fn persist_resolved_backend_respects_explicit_setting() {
        let _g = ENV_LOCK.lock().await;
        let (_home, db) = fresh_db_and_home().await;
        db.set_setting("security.kek_backend", "os_keychain")
            .await
            .unwrap();
        persist_resolved_backend(&db, KekBackend::File).await;
        let stored = db.get_setting("security.kek_backend").await.unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("os_keychain"),
            "explicit setting must survive a stale auto-resolve"
        );
    }

    /// When the setting is still `auto`, persisting the resolved value
    /// is allowed — that's how the very first daemon start records its
    /// decision so subsequent boots skip the probe.
    #[tokio::test]
    async fn persist_resolved_backend_writes_when_setting_is_auto() {
        let _g = ENV_LOCK.lock().await;
        let (_home, db) = fresh_db_and_home().await;
        db.set_setting("security.kek_backend", "auto")
            .await
            .unwrap();
        persist_resolved_backend(&db, KekBackend::File).await;
        let stored = db.get_setting("security.kek_backend").await.unwrap();
        assert_eq!(stored.as_deref(), Some("file"));
    }

    /// `auto` must not latch to `os_keychain`.
    ///
    /// The explicit `OsKeychain` arm has no probe and no fallback, so once the
    /// setting is rewritten to it, a host whose keyring is only *transiently*
    /// unavailable (a systemd user unit starting before the login keyring is
    /// unlocked) hard-fails `ensure_root_ca` and restart-loops the daemon.
    /// Only the `File` resolution is worth caching — that probe is the
    /// expensive one, and it is the headless case.
    #[tokio::test]
    async fn auto_is_not_latched_to_os_keychain_after_a_successful_probe() {
        let _g = ENV_LOCK.lock().await;
        let (home, db) = fresh_db_and_home().await;
        db.set_setting("security.kek_backend", "auto")
            .await
            .unwrap();

        let mut cfg = cortex::config::Config::default();
        cfg.security.kek_backend = KekBackend::Auto;
        // Whichever branch this host takes, the setting must never come back
        // as `os_keychain` — that is the value that removes the fallback.
        let _ = build_kek_store(home.path(), &cfg, &db).await;

        let stored = db.get_setting("security.kek_backend").await.unwrap();
        assert_ne!(
            stored.as_deref(),
            Some("os_keychain"),
            "auto must not latch to os_keychain — doing so strips the fallback \
             from every later boot and turns a locked keyring into a restart loop"
        );
    }

    #[tokio::test]
    async fn explicit_file_backend_uses_file_store_and_round_trips() {
        // Reflects the `--secure` non-Debian fallback path: the installer
        // wrote `kek_backend=file`, supplied a passphrase via env, and the
        // daemon must boot a working FileKekStore even when the OS keychain
        // is unavailable in CI.
        let _g = ENV_LOCK.lock().await;
        let (home, db) = fresh_db_and_home().await;
        std::env::set_var(PASSPHRASE_ENV, "test-passphrase");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let store = build_kek_store(home.path(), &cfg, &db)
            .await
            .expect("build file store should succeed");
        store.put("ca-key", b"PEM").expect("put");
        assert_eq!(store.get("ca-key").expect("get"), b"PEM".to_vec());
        std::env::remove_var(PASSPHRASE_ENV);
    }

    #[tokio::test]
    async fn explicit_file_backend_errors_when_passphrase_missing() {
        // Real failure mode: systemd unit lost the EnvironmentFile, daemon
        // restarts, and we must hard-fail with a clear message rather than
        // silently boot with a default-passphrase empty store (which would
        // mint a fresh CA and invalidate every paired device).
        let _g = ENV_LOCK.lock().await;
        let (home, db) = fresh_db_and_home().await;
        std::env::remove_var(PASSPHRASE_ENV);
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let result = build_kek_store(home.path(), &cfg, &db).await;
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

    #[tokio::test]
    async fn explicit_file_backend_errors_when_passphrase_is_empty() {
        // Defense-in-depth: empty-string env is a common mistake (e.g. a
        // missing `=value` in EnvironmentFile). We don't want to encrypt
        // with an empty passphrase silently.
        let _g = ENV_LOCK.lock().await;
        let (home, db) = fresh_db_and_home().await;
        std::env::set_var(PASSPHRASE_ENV, "");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;
        let result = build_kek_store(home.path(), &cfg, &db).await;
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
    #[tokio::test]
    async fn file_backend_persists_kek_across_restart() {
        let _g = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        std::env::set_var(PASSPHRASE_ENV, "integration-passphrase");
        let mut cfg = Config::default();
        cfg.security.kek_backend = KekBackend::File;

        // Round 1: build store, write a sentinel CA private key.
        let db1 = Database::test_db().await;
        let store1 = build_kek_store(home, &cfg, &db1)
            .await
            .expect("kek round 1");
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
        let db2 = Database::test_db().await;
        let store2 = build_kek_store(home, &cfg, &db2)
            .await
            .expect("kek round 2");
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
