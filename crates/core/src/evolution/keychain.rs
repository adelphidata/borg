//! Persistent storage for the evolution Ed25519 signing keypair.
//!
//! The private signing key never leaves the OS keychain (macOS Keychain via
//! the `security` CLI; Linux Secret Service via `secret-tool`). The matching
//! public key is mirrored into the `device_keys` SQLite table so replay can
//! verify rows without touching the keychain.
//!
//! Reinstall semantics: a keychain miss → fresh keypair generated → new
//! `device_keys` row inserted. Prior rows continue to verify against their
//! original `pubkey_id`.

use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, SECRET_KEY_LENGTH};

/// Service name used for both macOS Keychain and libsecret.
pub const KEYCHAIN_SERVICE: &str = "com.borg.evolution.signing";
/// Account/key name within the service.
pub const KEYCHAIN_ACCOUNT: &str = "ed25519-secret";

/// Generate a fresh Ed25519 signing key. Uses `rand::rng().fill()` (rand 0.9
/// CSPRNG) and feeds the bytes to `SigningKey::from_bytes` rather than
/// `SigningKey::generate`, because `ed25519-dalek` v2 still depends on
/// `rand_core` 0.6 and does not interop with `rand` 0.9's OsRng directly.
fn generate_signing_key() -> SigningKey {
    use rand::Rng;
    let mut seed = [0u8; SECRET_KEY_LENGTH];
    rand::rng().fill(&mut seed[..]);
    SigningKey::from_bytes(&seed)
}

/// Abstraction over OS keystores. Implementations must be deterministic:
/// `load_or_create()` called twice in a row returns the same key bytes.
pub trait KeyStore: Send + Sync {
    /// Return the stored 32-byte secret key seed, generating + persisting one
    /// on miss. Returns `(seed, freshly_created)`.
    fn load_or_create(&self) -> Result<([u8; SECRET_KEY_LENGTH], bool)>;
}

/// Construct the platform-default keystore. macOS uses `security`; Linux uses
/// `secret-tool`. Falls back to in-memory on unsupported platforms with a
/// warning — a future install will not retain the key, but the run still
/// works.
#[cfg(target_os = "macos")]
pub fn default_keystore() -> Box<dyn KeyStore> {
    Box::new(MacKeychain::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT))
}

#[cfg(target_os = "linux")]
pub fn default_keystore() -> Box<dyn KeyStore> {
    Box::new(LinuxSecretService::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn default_keystore() -> Box<dyn KeyStore> {
    tracing::warn!(
        "no OS keystore available on this platform; evolution signing key is in-memory only"
    );
    Box::new(InMemoryKeyStore::default())
}

/// Convenience wrapper: return a usable `SigningKey` plus a bool indicating
/// whether it was freshly generated this call.
pub fn load_or_create_signing_key(store: &dyn KeyStore) -> Result<(SigningKey, bool)> {
    let (seed, created) = store.load_or_create()?;
    Ok((SigningKey::from_bytes(&seed), created))
}

// ── In-memory test impl ──

/// Test-only keystore. Persists the key for the lifetime of the process so
/// `load_or_create()` is deterministic across calls within a test.
#[derive(Default)]
pub struct InMemoryKeyStore {
    inner: std::sync::Mutex<Option<[u8; SECRET_KEY_LENGTH]>>,
}

impl InMemoryKeyStore {
    /// Create a fresh empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for InMemoryKeyStore {
    fn load_or_create(&self) -> Result<([u8; SECRET_KEY_LENGTH], bool)> {
        let mut slot = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("InMemoryKeyStore mutex poisoned: {e}"))?;
        if let Some(seed) = *slot {
            return Ok((seed, false));
        }
        let key = generate_signing_key();
        let seed = key.to_bytes();
        *slot = Some(seed);
        Ok((seed, true))
    }
}

// ── Platform impls ──

#[cfg(target_os = "macos")]
struct MacKeychain {
    service: &'static str,
    account: &'static str,
}

#[cfg(target_os = "macos")]
impl MacKeychain {
    fn new(service: &'static str, account: &'static str) -> Self {
        Self { service, account }
    }
}

#[cfg(target_os = "macos")]
impl KeyStore for MacKeychain {
    fn load_or_create(&self) -> Result<([u8; SECRET_KEY_LENGTH], bool)> {
        if let Some(seed) = read_macos(self.service, self.account)? {
            return Ok((seed, false));
        }
        let key = generate_signing_key();
        let seed = key.to_bytes();
        write_macos(self.service, self.account, &seed)?;
        Ok((seed, true))
    }
}

#[cfg(target_os = "macos")]
fn read_macos(service: &str, account: &str) -> Result<Option<[u8; SECRET_KEY_LENGTH]>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("failed to invoke `security find-generic-password`")?;
    if !output.status.success() {
        return Ok(None);
    }
    let hex = String::from_utf8(output.stdout)
        .context("keychain returned non-utf8 secret")?
        .trim()
        .to_string();
    let bytes = hex_decode(&hex).context("keychain secret is not valid hex")?;
    if bytes.len() != SECRET_KEY_LENGTH {
        anyhow::bail!(
            "keychain secret has wrong length: expected {}, got {}",
            SECRET_KEY_LENGTH,
            bytes.len()
        );
    }
    let mut arr = [0u8; SECRET_KEY_LENGTH];
    arr.copy_from_slice(&bytes);
    Ok(Some(arr))
}

#[cfg(target_os = "macos")]
fn write_macos(service: &str, account: &str, seed: &[u8; SECRET_KEY_LENGTH]) -> Result<()> {
    // Known tradeoff: `security add-generic-password -w <hex>` exposes the
    // hex secret on the process command line for the duration of the call,
    // visible to `ps`. Matches the project's existing pattern in
    // `crates/core/src/scripts.rs::store_key_keychain`. Switching to the
    // `security-framework` crate would remove this exposure but is out of
    // scope here. The window is brief (single short-lived child process)
    // and any local user with `ps` access already has read access to
    // `~/.borg/borg.db`, so the practical leak is bounded.
    let hex = hex_encode(seed);
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U", // update if exists
            "-s",
            service,
            "-a",
            account,
            "-w",
            &hex,
        ])
        .status()
        .context("failed to invoke `security add-generic-password`")?;
    if !status.success() {
        anyhow::bail!("`security add-generic-password` returned non-zero status");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct LinuxSecretService {
    service: &'static str,
    account: &'static str,
}

#[cfg(target_os = "linux")]
impl LinuxSecretService {
    fn new(service: &'static str, account: &'static str) -> Self {
        Self { service, account }
    }
}

#[cfg(target_os = "linux")]
impl KeyStore for LinuxSecretService {
    fn load_or_create(&self) -> Result<([u8; SECRET_KEY_LENGTH], bool)> {
        if let Some(seed) = read_linux(self.service, self.account)? {
            return Ok((seed, false));
        }
        let key = generate_signing_key();
        let seed = key.to_bytes();
        write_linux(self.service, self.account, &seed)?;
        Ok((seed, true))
    }
}

#[cfg(target_os = "linux")]
fn read_linux(service: &str, account: &str) -> Result<Option<[u8; SECRET_KEY_LENGTH]>> {
    let output = std::process::Command::new("secret-tool")
        .args(["lookup", "service", service, "key", account])
        .output()
        .context("failed to invoke `secret-tool lookup`")?;
    if !output.status.success() {
        return Ok(None);
    }
    let hex = String::from_utf8(output.stdout)
        .context("secret-tool returned non-utf8 secret")?
        .trim()
        .to_string();
    if hex.is_empty() {
        return Ok(None);
    }
    let bytes = hex_decode(&hex).context("secret-tool secret is not valid hex")?;
    if bytes.len() != SECRET_KEY_LENGTH {
        anyhow::bail!(
            "secret-tool secret has wrong length: expected {}, got {}",
            SECRET_KEY_LENGTH,
            bytes.len()
        );
    }
    let mut arr = [0u8; SECRET_KEY_LENGTH];
    arr.copy_from_slice(&bytes);
    Ok(Some(arr))
}

#[cfg(target_os = "linux")]
fn write_linux(service: &str, account: &str, seed: &[u8; SECRET_KEY_LENGTH]) -> Result<()> {
    use std::io::Write;
    let hex = hex_encode(seed);
    let mut child = std::process::Command::new("secret-tool")
        .args([
            "store",
            "--label=borg-evolution-signing",
            "service",
            service,
            "key",
            account,
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn `secret-tool store`")?;
    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(hex.as_bytes())
            .context("failed to write secret to secret-tool stdin")?;
    }
    let status = child.wait().context("secret-tool store wait failed")?;
    if !status.success() {
        anyhow::bail!("`secret-tool store` returned non-zero status");
    }
    Ok(())
}

// ── Hex helpers (kept private, identical to signature.rs but local to avoid
// crate-internal coupling) ──

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        anyhow::bail!("hex string must have even length");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = nibble(chunk[0])?;
        let lo = nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => anyhow::bail!("invalid hex character: {c}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_returns_same_seed_across_calls() {
        let store = InMemoryKeyStore::new();
        let (a, created_a) = store.load_or_create().unwrap();
        let (b, created_b) = store.load_or_create().unwrap();
        assert_eq!(a, b, "second call must return the cached seed");
        assert!(created_a, "first call should report freshly created");
        assert!(!created_b, "second call must not report freshly created");
    }

    #[test]
    fn in_memory_different_instances_diverge() {
        let a = InMemoryKeyStore::new();
        let b = InMemoryKeyStore::new();
        let (sa, _) = a.load_or_create().unwrap();
        let (sb, _) = b.load_or_create().unwrap();
        assert_ne!(sa, sb, "fresh InMemory stores must yield independent seeds");
    }

    #[test]
    fn load_or_create_signing_key_yields_same_pubkey() {
        let store = InMemoryKeyStore::new();
        let (k1, _) = load_or_create_signing_key(&store).unwrap();
        let (k2, _) = load_or_create_signing_key(&store).unwrap();
        assert_eq!(k1.verifying_key().to_bytes(), k2.verifying_key().to_bytes());
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0u8, 15, 16, 254, 255];
        let s = hex_encode(&bytes);
        assert_eq!(s, "000f10feff");
        assert_eq!(hex_decode(&s).unwrap(), bytes);
        assert!(hex_decode("zz").is_err());
        assert!(hex_decode("abc").is_err()); // odd length
    }
}
