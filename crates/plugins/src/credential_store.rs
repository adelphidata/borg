//! Credential storage with OS keychain primary + on-disk fallback.
//!
//! Tries the OS keychain first via [`crate::keychain`]. If the keychain is
//! unavailable or the user denies the prompt, writes to
//! `~/.borg/.credentials.json` with mode 0600. All callers in the installer
//! and gateway go through this module rather than [`crate::keychain`] directly.
//!
//! The fallback file exists only to keep onboarding unblocked for users who
//! cannot or will not grant keychain access — keychain remains the preferred
//! backend and is always attempted first.
//!
//! Precedent: `borg_core::scripts::get_or_create_hmac_key` uses the same
//! keychain-first/file-fallback pattern for the script HMAC signing key.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::keychain;

const CREDENTIALS_FILENAME: &str = ".credentials.json";

/// On-disk credentials file: `{ service: { account: value } }`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(flatten)]
    services: BTreeMap<String, BTreeMap<String, String>>,
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("BORG_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".borg"))
}

fn credentials_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(CREDENTIALS_FILENAME))
}

fn load_file() -> Result<CredentialsFile> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(CredentialsFile::default());
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(CredentialsFile::default());
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse {} as JSON", path.display()))
}

fn save_file(file: &CredentialsFile) -> Result<()> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(file)?;
    std::fs::write(&tmp, &json).with_context(|| format!("Failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to chmod {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Store a credential. Tries the OS keychain first; on failure writes to the
/// fallback file at `~/.borg/.credentials.json` (mode 0600).
pub fn store(service: &str, account: &str, value: &str) -> Result<()> {
    if let Err(e) = keychain::store(service, account, value) {
        tracing::warn!(
            "Keychain store failed for service={service} account={account}, falling back to file: {e}"
        );
        let mut file = load_file()?;
        file.services
            .entry(service.to_string())
            .or_default()
            .insert(account.to_string(), value.to_string());
        save_file(&file)?;
    }
    Ok(())
}

/// Read a credential. Tries the OS keychain first, then the fallback file.
pub fn read(service: &str, account: &str) -> Option<String> {
    if let Some(v) = read_keychain(service, account) {
        return Some(v);
    }
    let file = load_file().ok()?;
    file.services.get(service)?.get(account).cloned()
}

fn read_keychain(service: &str, account: &str) -> Option<String> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .ok()?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .ok()
                .map(|s| s.trim().to_string());
        }
    } else if cfg!(target_os = "linux") {
        let output = std::process::Command::new("secret-tool")
            .args(["lookup", "service", service, "account", account])
            .output()
            .ok()?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .ok()
                .map(|s| s.trim().to_string());
        }
    }
    None
}

/// Check whether a credential exists in either backend.
pub fn check(service: &str, account: &str) -> bool {
    if keychain::check(service, account) {
        return true;
    }
    load_file()
        .ok()
        .and_then(|f| {
            f.services
                .get(service)
                .and_then(|m| m.get(account))
                .cloned()
        })
        .is_some()
}

/// Remove a credential from both backends.
pub fn remove(service: &str, account: &str) {
    keychain::remove(service, account);
    let mut file = match load_file() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("Failed to load credentials file during remove: {e}");
            return;
        }
    };
    let removed = file
        .services
        .get_mut(service)
        .map(|m| m.remove(account).is_some())
        .unwrap_or(false);
    if !removed {
        return;
    }
    if file
        .services
        .get(service)
        .is_some_and(std::collections::BTreeMap::is_empty)
    {
        file.services.remove(service);
    }
    if let Err(e) = save_file(&file) {
        tracing::warn!("Failed to save credentials file after remove: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::TempDir;

    /// Serializes tests that mutate `$BORG_DATA_DIR`. Cargo runs tests in one
    /// process across multiple threads; concurrent tests would clobber each
    /// other's tempdir pointer and race on the shared credentials file.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        // Poisoned locks are fine — we only guard env exclusivity, not invariants.
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        _tmp: TempDir,
        prev: Option<String>,
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = env_lock();
            let tmp = TempDir::new().unwrap();
            let prev = std::env::var("BORG_DATA_DIR").ok();
            unsafe {
                std::env::set_var("BORG_DATA_DIR", tmp.path());
            }
            Self {
                _lock: lock,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("BORG_DATA_DIR", v),
                    None => std::env::remove_var("BORG_DATA_DIR"),
                }
            }
        }
    }

    fn file_only_store(service: &str, account: &str, value: &str) -> Result<()> {
        let mut file = load_file()?;
        file.services
            .entry(service.to_string())
            .or_default()
            .insert(account.to_string(), value.to_string());
        save_file(&file)
    }

    #[test]
    fn file_round_trip_store_read_remove() {
        let _env = TestEnv::new();

        file_only_store("svc-a", "acct-1", "secret-1").unwrap();
        let file = load_file().unwrap();
        assert_eq!(
            file.services.get("svc-a").unwrap().get("acct-1").unwrap(),
            "secret-1"
        );

        remove("svc-a", "acct-1");
        let file = load_file().unwrap();
        assert!(!file.services.contains_key("svc-a"));
    }

    #[test]
    #[cfg(unix)]
    fn file_is_mode_0600_after_write() {
        use std::os::unix::fs::PermissionsExt;

        let _env = TestEnv::new();

        file_only_store("svc", "acct", "secret").unwrap();

        let path = credentials_path().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "credentials file must be mode 0600, got {mode:o}"
        );
    }

    #[test]
    fn multiple_services_coexist_without_clobber() {
        let _env = TestEnv::new();

        file_only_store("svc-a", "acct-1", "val-a1").unwrap();
        file_only_store("svc-b", "acct-2", "val-b2").unwrap();
        file_only_store("svc-a", "acct-3", "val-a3").unwrap();

        let file = load_file().unwrap();
        assert_eq!(
            file.services.get("svc-a").unwrap().get("acct-1").unwrap(),
            "val-a1"
        );
        assert_eq!(
            file.services.get("svc-a").unwrap().get("acct-3").unwrap(),
            "val-a3"
        );
        assert_eq!(
            file.services.get("svc-b").unwrap().get("acct-2").unwrap(),
            "val-b2"
        );
    }

    #[test]
    fn read_returns_none_when_file_missing_and_keychain_miss() {
        let _env = TestEnv::new();
        // Use a service name no keychain on CI would have.
        assert!(read("borg-test-nonexistent-svc", "borg-nope").is_none());
    }

    // Unique service prefix makes these safe even on a dev machine that might
    // (vanishingly unlikely) have a matching keychain entry from a prior run.
    const TEST_SVC_PREFIX: &str = "borg-test-credstore-8f3a";

    #[test]
    fn read_returns_file_value_when_keychain_misses() {
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-read");
        file_only_store(&svc, "acct", "from-file").unwrap();
        assert_eq!(read(&svc, "acct"), Some("from-file".to_string()));
    }

    #[test]
    fn check_is_true_when_only_file_has_entry() {
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-check");
        assert!(!check(&svc, "acct"));
        file_only_store(&svc, "acct", "x").unwrap();
        assert!(check(&svc, "acct"));
    }

    #[test]
    fn remove_of_absent_entry_is_noop() {
        let _env = TestEnv::new();
        // File doesn't exist yet — must not panic or error.
        remove("never-stored", "never-there");
        // And with an existing file but missing service.
        let svc = format!("{TEST_SVC_PREFIX}-noop");
        file_only_store(&svc, "acct", "x").unwrap();
        remove("never-stored", "never-there");
        // Our real entry should still be there.
        let file = load_file().unwrap();
        assert_eq!(file.services.get(&svc).unwrap().get("acct").unwrap(), "x");
    }

    #[test]
    fn remove_prunes_empty_service_map() {
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-prune");
        file_only_store(&svc, "only", "x").unwrap();
        remove(&svc, "only");
        let file = load_file().unwrap();
        // Service key should be gone entirely, not an empty map left behind.
        assert!(
            !file.services.contains_key(&svc),
            "empty service map should be pruned"
        );
    }

    #[test]
    fn remove_keeps_sibling_accounts_intact() {
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-sibling");
        file_only_store(&svc, "keep-me", "v1").unwrap();
        file_only_store(&svc, "drop-me", "v2").unwrap();
        remove(&svc, "drop-me");
        let file = load_file().unwrap();
        let accts = file.services.get(&svc).unwrap();
        assert_eq!(accts.get("keep-me").unwrap(), "v1");
        assert!(accts.get("drop-me").is_none());
    }

    #[test]
    fn load_file_returns_empty_when_file_missing() {
        let _env = TestEnv::new();
        let file = load_file().unwrap();
        assert!(file.services.is_empty());
    }

    #[test]
    fn load_file_returns_empty_when_file_is_empty_bytes() {
        let _env = TestEnv::new();
        let path = credentials_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let file = load_file().unwrap();
        assert!(file.services.is_empty());
    }

    #[test]
    fn load_file_errors_on_corrupt_json() {
        let _env = TestEnv::new();
        let path = credentials_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not valid json").unwrap();
        assert!(
            load_file().is_err(),
            "corrupt JSON must not silently succeed"
        );
    }

    #[test]
    fn save_file_overwrites_preserves_mode_0600() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-overwrite");
        file_only_store(&svc, "a", "v1").unwrap();
        file_only_store(&svc, "a", "v2").unwrap();
        let file = load_file().unwrap();
        assert_eq!(file.services.get(&svc).unwrap().get("a").unwrap(), "v2");

        #[cfg(unix)]
        {
            let path = credentials_path().unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn read_from_fallback_via_public_api_without_keychain() {
        // Simulates the denial path: keychain write failed, so `store` wrote
        // only to the file. `read` must transparently surface it.
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-public");
        file_only_store(&svc, "account-x", "token-abc").unwrap();
        assert_eq!(
            read(&svc, "account-x"),
            Some("token-abc".to_string()),
            "read() must fall back to the file when keychain misses"
        );
    }

    // Guard against serde_json flattening quirks: the `#[serde(flatten)]` on
    // `CredentialsFile` must produce a flat `{service: {account: value}}`
    // shape, not wrap it in a `{"services": {...}}` object.
    #[test]
    fn file_format_is_flat_service_map_json() {
        let _env = TestEnv::new();
        let svc = format!("{TEST_SVC_PREFIX}-shape");
        file_only_store(&svc, "acct", "v").unwrap();
        let bytes = std::fs::read(credentials_path().unwrap()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json.get(&svc).is_some(),
            "service name must be a top-level JSON key: {json}"
        );
        assert_eq!(
            json.get(&svc).unwrap().get("acct").unwrap().as_str(),
            Some("v")
        );
    }
}
