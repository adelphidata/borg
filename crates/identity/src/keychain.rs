//! OS keychain wrapper for storing PKI private keys at rest.
//!
//! Three backends:
//! - [`KeychainStore`]: OS keychain via the `keyring` crate. Default on
//!   desktop. Fails on headless Linux without dbus + secret-service.
//! - [`FileKekStore`]: passphrase-encrypted file. Always-works fallback for
//!   headless VPS deploys. Posture system reads this as `Reduced` so
//!   `KeychainStore` is the rewarded path.
//! - [`MemoryKekStore`]: tests and CI hosts only.

use crate::error::PkiError;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Service name passed to the OS keychain. Shared across every PKI account
/// (`ca-key`, `server-cert-key`, paired-device keys); the account name —
/// not the service — distinguishes entries, so this constant must stay
/// stable across borg versions or existing installs lose their keys.
pub const SERVICE: &str = "borg-pki";

/// Trait abstracting the keychain so tests can substitute an in-memory store.
pub trait KekStore: Send + Sync {
    /// Store `secret` under `account`. Replaces any existing value.
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError>;
    /// Retrieve the secret stored under `account`.
    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError>;
    /// Remove the secret under `account`. No-op if absent.
    fn delete(&self, account: &str) -> Result<(), PkiError>;
}

/// Concrete `KekStore` backed by the OS keychain via `keyring`.
#[derive(Debug, Default)]
pub struct KeychainStore;

impl KeychainStore {
    /// New OS-keychain-backed store.
    pub fn new() -> Self {
        Self
    }

    fn entry(account: &str) -> Result<keyring_core::Entry, PkiError> {
        // Registers the platform store on first use; the result is only a
        // hint, since a store installed directly (the mock in tests, or a
        // host without a Secret Service) is what the entry resolves against.
        let _ = keyring::Entry::store_status();
        keyring_core::Entry::new(SERVICE, account)
            .map_err(|e| PkiError::Keychain(format!("entry({account}): {e}")))
    }
}

impl KekStore for KeychainStore {
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError> {
        let entry = Self::entry(account)?;
        entry
            .set_secret(secret)
            .map_err(|e| PkiError::Keychain(format!("set_secret({account}): {e}")))
    }

    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError> {
        let entry = Self::entry(account)?;
        entry.get_secret().map_err(|e| match e {
            keyring_core::Error::NoEntry => PkiError::NotFound(account.to_string()),
            other => PkiError::Keychain(format!("get_secret({account}): {other}")),
        })
    }

    fn delete(&self, account: &str) -> Result<(), PkiError> {
        let entry = Self::entry(account)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(PkiError::Keychain(format!("delete({account}): {e}"))),
        }
    }
}

/// In-memory `KekStore` for tests and CI hosts without a working keychain.
#[derive(Debug, Default)]
pub struct MemoryKekStore {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryKekStore {
    /// Empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KekStore for MemoryKekStore {
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.insert(account.to_string(), secret.to_vec());
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError> {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.get(account)
            .cloned()
            .ok_or_else(|| PkiError::NotFound(account.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), PkiError> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.remove(account);
        Ok(())
    }
}

// Passphrase-encrypted file backend for headless servers. The whole secret
// map is encrypted as a single ChaCha20Poly1305 ciphertext keyed by
// scrypt(passphrase, salt) → HKDF (v2 envelope; v1 files derived with bare
// HKDF still decrypt and upgrade on next write). Salt is per-file (random at
// first put); nonce is random per write.
//
// Threat model — READ THIS BEFORE RELYING ON IT:
//   * Protection is REAL only when the passphrase comes from a source the
//     attacker can't read alongside the ciphertext — i.e. a root-only systemd
//     `EnvironmentFile` (explicit `File` backend). Then a stolen backup /
//     `rsync` of `~/.borg` yields only ciphertext, and scrypt makes an
//     offline guess of an operator passphrase memory-hard.
//   * In the DEFAULT `Auto` fallback the passphrase is auto-provisioned to
//     `~/.borg/secrets/kek.env`, which sits NEXT TO `kek.enc`. Anyone who can
//     read the ciphertext (same-UID process, stolen backup of the whole
//     `secrets/` dir) reads the passphrase too — at-rest protection is then
//     effectively nil. This is an accepted trade-off for the "always boots on
//     a headless VPS" guarantee, NOT a claim of confidentiality. Posture
//     surfaces it as `Reduced` and boot logs warn; the only real fix is the OS
//     keychain (auto-preferred when present) or a root-only EnvironmentFile.

/// On-disk envelope version. v2 derives the encryption key with memory-hard
/// scrypt. It is the only version read or written; an envelope tagged anything
/// else is rejected rather than derived under weaker parameters.
const KEK_FILE_VERSION: u32 = 2;

/// On-disk envelope. Stored as JSON; whole-map ciphertext authenticates
/// every account at once so partial-tampering attacks fail decryption.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedKekFile {
    version: u32,
    salt_b64: String,
    nonce_b64: String,
    ciphertext_b64: String,
}

/// `KekStore` backed by a passphrase-encrypted file at `path`.
///
/// `Debug` is implemented manually so an accidental `dbg!()` or
/// log-line on the store (or any wrapper that delegates `Debug`) never
/// echoes the passphrase or decrypted key material.
pub struct FileKekStore {
    path: PathBuf,
    passphrase: Vec<u8>,
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl std::fmt::Debug for FileKekStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileKekStore")
            .field("path", &self.path)
            .field("passphrase", &"<redacted>")
            .field("inner", &"<redacted>")
            .finish()
    }
}

impl FileKekStore {
    /// Open or create a file-backed store at `path` using `passphrase`. Reads
    /// any existing ciphertext and decrypts it; a wrong passphrase returns
    /// `Keychain` (auth-tag mismatch). Missing file is fine — first `put`
    /// will create it.
    pub fn open(
        path: impl Into<PathBuf>,
        passphrase: impl Into<Vec<u8>>,
    ) -> Result<Self, PkiError> {
        let path = path.into();
        let passphrase = passphrase.into();
        if passphrase.is_empty() {
            return Err(PkiError::Keychain(
                "FileKekStore: passphrase must not be empty".into(),
            ));
        }
        let inner = match std::fs::read(&path) {
            Ok(bytes) => decrypt_map(&bytes, &passphrase)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                return Err(PkiError::Keychain(format!(
                    "FileKekStore: read {}: {e}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            passphrase,
            inner: Mutex::new(inner),
        })
    }

    /// Persist the in-memory map to disk atomically: write to a sibling
    /// `.tmp`, fsync, then rename over the final path. A crash mid-flush
    /// would otherwise leave a zero-length or partial `kek.enc`, which —
    /// because the whole map is encrypted as one blob — would render the
    /// CA private key (and every paired-device cert) unrecoverable.
    /// Caller holds the in-memory lock.
    fn flush_locked(&self, map: &HashMap<String, Vec<u8>>) -> Result<(), PkiError> {
        let blob = encrypt_map(map, &self.passphrase)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                PkiError::Keychain(format!("FileKekStore: mkdir {}: {e}", parent.display()))
            })?;
            #[cfg(unix)]
            set_mode(parent, 0o700)?;
        }
        let tmp_path = match self.path.file_name() {
            Some(name) => {
                let mut t = std::ffi::OsString::from(name);
                t.push(".tmp");
                self.path.with_file_name(t)
            }
            None => {
                return Err(PkiError::Keychain(format!(
                    "FileKekStore: invalid path {}",
                    self.path.display()
                )));
            }
        };
        // Open with create+truncate so a leftover `.tmp` from a previous
        // crashed flush is overwritten cleanly (no append). On Unix we
        // pass `mode(0o600)` to OpenOptions so the file is created with
        // restrictive perms atomically with the open() syscall — no
        // window where a concurrent reader (backup agent, etc.) can
        // grab a 0644 ciphertext between create and chmod.
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp_path).map_err(|e| {
            PkiError::Keychain(format!("FileKekStore: open {}: {e}", tmp_path.display()))
        })?;
        // Belt-and-braces re-assert in case the file already existed
        // (OpenOptions::mode only applies to newly created files).
        #[cfg(unix)]
        set_mode(&tmp_path, 0o600)?;
        use std::io::Write as _;
        f.write_all(&blob).map_err(|e| {
            PkiError::Keychain(format!("FileKekStore: write {}: {e}", tmp_path.display()))
        })?;
        f.sync_all().map_err(|e| {
            PkiError::Keychain(format!("FileKekStore: fsync {}: {e}", tmp_path.display()))
        })?;
        drop(f);
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            PkiError::Keychain(format!(
                "FileKekStore: rename {} -> {}: {e}",
                tmp_path.display(),
                self.path.display()
            ))
        })?;
        // The rename inherits the target's existing mode on most file
        // systems, so re-assert 0600 on the final path. Belt-and-braces
        // against umask drift on exotic mounts.
        #[cfg(unix)]
        set_mode(&self.path, 0o600)?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), PkiError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).map_err(|e| {
        PkiError::Keychain(format!(
            "FileKekStore: chmod {} {:o}: {e}",
            path.display(),
            mode
        ))
    })
}

impl KekStore for FileKekStore {
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.insert(account.to_string(), secret.to_vec());
        self.flush_locked(&g)
    }

    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError> {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.get(account)
            .cloned()
            .ok_or_else(|| PkiError::NotFound(account.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), PkiError> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.remove(account).is_some() {
            self.flush_locked(&g)?;
        }
        Ok(())
    }
}

/// scrypt work parameters for v2 derivation. `log_n=15` → N=32768, r=8, p=1 is
/// ~32 MiB / ~50 ms per guess on commodity hardware — run once at daemon boot,
/// so the latency is invisible, but an offline attacker with `kek.enc` now pays
/// a memory-hard cost per passphrase guess instead of a single HMAC. Bumping
/// these is safe (only affects newly written files); existing v2 files stay
/// decryptable because the salt+params are fixed by the version tag.
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

/// v2 derivation: run the passphrase through memory-hard scrypt first, then
/// HKDF for domain separation. This is what closes the "offline brute-force at
/// one-HMAC-per-guess" gap for operator-chosen `BORG_KEK_PASSPHRASE` values
/// (the `Auto` path uses a 256-bit random passphrase that is brute-force-immune
/// regardless, but paying the cost there too is harmless — a one-time boot
/// cost).
fn derive_key_v2(passphrase: &[u8], salt: &[u8]) -> Result<Key, PkiError> {
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: scrypt params: {e}")))?;
    let mut stretched = [0u8; 32];
    scrypt::scrypt(passphrase, salt, &params, &mut stretched)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: scrypt: {e}")))?;
    // HKDF over the stretched key for domain separation from any other use of
    // scrypt output; salt is reused (already high-entropy and per-file).
    let hk = Hkdf::<Sha256>::new(Some(salt), &stretched);
    let mut okm = [0u8; 32];
    hk.expand(b"borg-kek-v2", &mut okm)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: hkdf: {e}")))?;
    Ok(Key::from(okm))
}

/// Derive the envelope key, refusing any version but the current one.
fn derive_key(version: u32, passphrase: &[u8], salt: &[u8]) -> Result<Key, PkiError> {
    match version {
        2 => derive_key_v2(passphrase, salt),
        other => Err(PkiError::Keychain(format!(
            "FileKekStore: unsupported file version {other}"
        ))),
    }
}

fn encrypt_map(map: &HashMap<String, Vec<u8>>, passphrase: &[u8]) -> Result<Vec<u8>, PkiError> {
    let plaintext = serde_json::to_vec(map)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: serialize: {e}")))?;
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let key = derive_key(KEK_FILE_VERSION, passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(&key);
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce_bytes), plaintext.as_ref())
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: encrypt: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let env = EncryptedKekFile {
        version: KEK_FILE_VERSION,
        salt_b64: b64.encode(salt),
        nonce_b64: b64.encode(nonce_bytes),
        ciphertext_b64: b64.encode(ciphertext),
    };
    serde_json::to_vec_pretty(&env)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: envelope serialize: {e}")))
}

fn decrypt_map(blob: &[u8], passphrase: &[u8]) -> Result<HashMap<String, Vec<u8>>, PkiError> {
    let env: EncryptedKekFile = serde_json::from_slice(blob)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: parse envelope: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let salt = b64
        .decode(&env.salt_b64)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: salt b64: {e}")))?;
    let nonce_bytes = b64
        .decode(&env.nonce_b64)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: nonce b64: {e}")))?;
    let ciphertext = b64
        .decode(&env.ciphertext_b64)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: ciphertext b64: {e}")))?;
    if nonce_bytes.len() != 12 {
        return Err(PkiError::Keychain(format!(
            "FileKekStore: nonce must be 12 bytes, got {}",
            nonce_bytes.len()
        )));
    }
    let key = derive_key(env.version, passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(&key);
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: nonce: {e}")))?;
    let plaintext = cipher
        .decrypt(&nonce, ciphertext.as_ref())
        // Wrong passphrase, tampered ciphertext, or version skew all land
        // here. We deliberately do NOT include the underlying message so
        // we don't echo cipher state into logs.
        .map_err(|_| PkiError::Keychain("FileKekStore: decryption failed".into()))?;
    serde_json::from_slice(&plaintext)
        .map_err(|e| PkiError::Keychain(format!("FileKekStore: parse map: {e}")))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A missing keychain entry must surface as PkiError::NotFound naming the
    /// account — callers key their "run `borg device pair` first" guidance on
    /// that variant, and a fabricated Ok(secret) would hand out a bogus KEK.
    /// Runs against keyring's in-process mock backend (whose entries are
    /// always absent) so the test never touches the real OS keychain.
    #[test]
    fn keychain_store_maps_missing_entry_to_notfound() {
        // Trigger the platform registration first so it cannot replace the
        // mock installed next (on a host without a keychain it simply fails).
        let _ = keyring::Entry::store_status();
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        let store = KeychainStore::new();
        let account = "borg-test-kek-missing";
        match store.get(account) {
            Err(PkiError::NotFound(a)) => assert_eq!(a, account),
            other => panic!("expected NotFound for an absent entry, got {other:?}"),
        }
    }

    /// A read error other than NotFound (here: EISDIR from opening a
    /// directory) must fail open loudly, never masquerade as an empty store —
    /// treating it as empty would silently orphan every wrapped KEK.
    #[test]
    fn file_kek_open_propagates_non_notfound_read_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = FileKekStore::open(dir.path(), "passphrase").expect_err("open must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("read"),
            "error must name the failed read: {msg}"
        );
    }

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryKekStore::new();
        store.put("ca-key", b"hunter2").expect("put");
        assert_eq!(store.get("ca-key").expect("get"), b"hunter2".to_vec());
    }

    #[test]
    fn memory_store_delete_is_idempotent() {
        let store = MemoryKekStore::new();
        store.delete("nope").expect("first delete is ok");
        store.put("k", b"v").expect("put");
        store.delete("k").expect("delete");
        assert!(matches!(store.get("k"), Err(PkiError::NotFound(_))));
        store.delete("k").expect("second delete is ok");
    }

    #[test]
    fn missing_key_returns_notfound_not_keychain_error() {
        let store = MemoryKekStore::new();
        match store.get("absent") {
            Err(PkiError::NotFound(s)) => assert_eq!(s, "absent"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    fn tmp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets").join("kek.enc");
        (dir, path)
    }

    #[test]
    fn file_kek_round_trips() {
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"hunter2".to_vec()).expect("open");
        store.put("ca-key", b"PRIVATE-CA").expect("put ca");
        store
            .put("server-cert-key", b"PRIVATE-SERVER")
            .expect("put server");
        assert_eq!(store.get("ca-key").expect("get"), b"PRIVATE-CA".to_vec());
        assert_eq!(
            store.get("server-cert-key").expect("get"),
            b"PRIVATE-SERVER".to_vec()
        );
    }

    #[test]
    fn file_kek_persists_across_open() {
        let (_dir, path) = tmp_path();
        {
            let s = FileKekStore::open(&path, b"pw".to_vec()).expect("open1");
            s.put("k", b"value").expect("put");
        }
        let s = FileKekStore::open(&path, b"pw".to_vec()).expect("open2");
        assert_eq!(s.get("k").expect("get"), b"value".to_vec());
    }

    #[test]
    fn file_kek_wrong_passphrase_rejects() {
        let (_dir, path) = tmp_path();
        {
            let s = FileKekStore::open(&path, b"correct".to_vec()).expect("open");
            s.put("k", b"v").expect("put");
        }
        let err = FileKekStore::open(&path, b"wrong".to_vec()).expect_err("must reject");
        match err {
            PkiError::Keychain(msg) => {
                // Error must NOT echo the ciphertext, key material, or the
                // attempted passphrase. We assert on the deliberate "decryption
                // failed" stub instead of a leaky underlying message.
                assert!(msg.contains("decryption failed"), "unexpected msg: {msg}");
                assert!(!msg.contains("wrong"));
                assert!(!msg.contains("correct"));
            }
            other => panic!("expected Keychain, got {other:?}"),
        }
    }

    /// Read the on-disk envelope version for a kek.enc.
    fn envelope_version(path: &Path) -> u32 {
        let bytes = std::fs::read(path).expect("read kek.enc");
        let env: EncryptedKekFile = serde_json::from_slice(&bytes).expect("parse envelope");
        env.version
    }

    #[test]
    fn new_files_are_written_as_v2_scrypt() {
        // Regression (M4): a freshly written kek.enc must use the memory-hard
        // v2 envelope, not the legacy bare-HKDF v1.
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
        store.put("k", b"v").expect("put");
        assert_eq!(envelope_version(&path), 2, "new files must be v2 (scrypt)");
    }

    #[test]
    fn v1_envelope_is_rejected_not_derived_under_weaker_params() {
        // v1 derived the key with bare HKDF — no work factor, so an offline
        // attacker paid one hash per passphrase guess. Accepting a v1 tag would
        // let an attacker who can rewrite the version field downgrade a v2 file
        // back to that. The version must fail closed, like any other unknown one.
        let (_dir, path) = tmp_path();
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let b64 = base64::engine::general_purpose::STANDARD;
        let env = EncryptedKekFile {
            version: 1,
            salt_b64: b64.encode([3u8; 16]),
            nonce_b64: b64.encode([4u8; 12]),
            ciphertext_b64: b64.encode([0u8; 16]),
        };
        std::fs::write(&path, serde_json::to_vec(&env).expect("envelope")).expect("write v1");

        let err = FileKekStore::open(&path, b"operator-secret".to_vec())
            .expect_err("a v1 envelope must not open");
        assert!(
            err.to_string().contains("unsupported file version"),
            "error must name the version, got: {err}",
        );
    }

    #[test]
    fn unsupported_future_version_is_rejected() {
        // A version the running binary doesn't understand must fail closed, not
        // silently mis-derive.
        let (_dir, path) = tmp_path();
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let b64 = base64::engine::general_purpose::STANDARD;
        let env = EncryptedKekFile {
            version: 99,
            salt_b64: b64.encode([0u8; 16]),
            nonce_b64: b64.encode([0u8; 12]),
            ciphertext_b64: b64.encode([0u8; 16]),
        };
        std::fs::write(&path, serde_json::to_vec(&env).expect("envelope")).expect("write");
        let err = FileKekStore::open(&path, b"pw".to_vec()).expect_err("must reject");
        match err {
            PkiError::Keychain(msg) => assert!(
                msg.contains("unsupported file version"),
                "unexpected msg: {msg}"
            ),
            other => panic!("expected Keychain, got {other:?}"),
        }
    }

    #[test]
    fn file_kek_empty_passphrase_rejects() {
        let (_dir, path) = tmp_path();
        let err = FileKekStore::open(&path, Vec::<u8>::new()).expect_err("must reject");
        assert!(matches!(err, PkiError::Keychain(_)));
    }

    #[cfg(unix)]
    #[test]
    fn file_kek_file_mode_0600_dir_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
        store.put("k", b"v").expect("put");
        let file_mode = std::fs::metadata(&path)
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "file mode must be 0600");
        let parent = path.parent().expect("parent");
        let dir_mode = std::fs::metadata(parent)
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "parent dir mode must be 0700");
    }

    #[test]
    fn file_kek_delete_idempotent() {
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
        store.delete("never-existed").expect("first delete");
        store.put("k", b"v").expect("put");
        store.delete("k").expect("delete");
        assert!(matches!(store.get("k"), Err(PkiError::NotFound(_))));
        store.delete("k").expect("second delete");
    }

    #[test]
    fn file_kek_delete_persists_to_disk() {
        // Ensures `delete` flushes — otherwise reopens would resurrect entries.
        let (_dir, path) = tmp_path();
        {
            let s = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
            s.put("k", b"v").expect("put");
            s.delete("k").expect("delete");
        }
        let s = FileKekStore::open(&path, b"pw".to_vec()).expect("reopen");
        assert!(matches!(s.get("k"), Err(PkiError::NotFound(_))));
    }

    /// Atomic flush: a crash mid-write must not corrupt the real file.
    /// Simulate by dropping a leftover `kek.enc.tmp` next to a healthy
    /// `kek.enc` and asserting reopen still succeeds. A non-atomic flush
    /// would have already truncated the real file at this point.
    #[test]
    fn file_kek_leftover_tmp_does_not_corrupt_real_file() {
        let (_dir, path) = tmp_path();
        {
            let s = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
            s.put("k", b"v").expect("put");
        }
        // Drop a partial .tmp adjacent to the real file — what would
        // remain on a crash *before* the rename. Reopen must still
        // decode the real file cleanly.
        let tmp_path = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::write(&tmp_path, b"garbage").expect("write garbage tmp");
        let s = FileKekStore::open(&path, b"pw".to_vec()).expect("reopen");
        assert_eq!(s.get("k").expect("get"), b"v".to_vec());
        // A subsequent flush must overwrite the leftover tmp without
        // erroring (the rename target may already exist if the previous
        // flush crashed at a different point — same code path).
        s.put("k2", b"v2").expect("second put");
    }

    #[test]
    fn file_kek_corrupted_blob_returns_keychain_error() {
        let (_dir, path) = tmp_path();
        {
            let s = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
            s.put("k", b"v").expect("put");
        }
        // Truncate ciphertext to something that fails JSON parse.
        std::fs::write(&path, b"not-json").expect("clobber");
        let err = FileKekStore::open(&path, b"pw".to_vec()).expect_err("must reject");
        assert!(matches!(err, PkiError::Keychain(_)));
    }

    #[test]
    fn file_kek_debug_redacts_passphrase_and_inner_map() {
        // A future refactor that switches to `#[derive(Debug)]` would
        // echo the passphrase bytes to any `dbg!()` / `tracing::debug!`
        // call. The custom Debug impl must keep both fields off the output.
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"super-secret-pass-123".to_vec()).expect("open");
        store.put("ca-key", b"PRIVATE_PEM").expect("put");
        let s = format!("{store:?}");
        assert!(s.contains("FileKekStore"));
        assert!(s.contains("path"));
        assert!(!s.contains("super-secret"));
        assert!(!s.contains("PRIVATE_PEM"));
    }

    #[test]
    fn file_kek_envelope_does_not_leak_plaintext() {
        // Defense-in-depth: assert the on-disk envelope never embeds raw
        // secrets. If a future refactor accidentally stores plaintext (e.g.
        // for "debug aid") this fires.
        let (_dir, path) = tmp_path();
        let store = FileKekStore::open(&path, b"pw".to_vec()).expect("open");
        let secret = b"VERY_SECRET_PEM_BLOB";
        store.put("ca-key", secret).expect("put");
        let bytes = std::fs::read(&path).expect("read file");
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret),
            "envelope must not contain plaintext secret"
        );
        assert!(
            !bytes.windows(b"ca-key".len()).any(|w| w == b"ca-key"),
            "envelope must not contain plaintext account name"
        );
    }
}
