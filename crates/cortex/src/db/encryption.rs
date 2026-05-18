//! libsql DB encryption key derivation.
//!
//! libsql ships SQLCipher (AES-256-CBC). Its `EncryptionConfig` takes a raw
//! 32-byte key. We derive that key from the existing KEK
//! (`security.kek_backend`) via HKDF-SHA256 so the DB encryption key shares
//! the same OS-keychain / passphrase-file trust root as the rest of borg's
//! secret storage. The KEK never leaves the keystore; only the derived
//! sub-key reaches libsql.
//!
//! The derivation primitives are kept available even though the only caller
//! (`Database::open_encrypted`) is currently dormant — flipping the encrypted
//! at-rest lane on later wires this in without re-deriving from scratch.

#![allow(dead_code)]

use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;

/// HKDF salt — domain-separates the DB key from any other sub-key derived
/// from the same KEK. Versioned so future rotations can re-key without
/// colliding with old derivations.
const HKDF_SALT: &[u8] = b"borg/db-encryption/v1";

/// HKDF info — pins the output to the cipher we hand to libsql.
const HKDF_INFO: &[u8] = b"libsql-sqlcipher-aes256cbc";

/// Length of the derived key in bytes (AES-256).
pub const KEY_LEN: usize = 32;

/// Derive a 32-byte AES-256 key from raw KEK bytes via HKDF-SHA256.
///
/// `kek_bytes` is the secret pulled from the configured `KekStore`. The
/// derivation is deterministic — same KEK + salt + info always produces the
/// same bytes, which is what lets libsql reopen an existing encrypted DB
/// across restarts without persisting the derived key anywhere.
pub fn derive_db_key_bytes(kek_bytes: &[u8]) -> Result<[u8; KEY_LEN]> {
    if kek_bytes.is_empty() {
        anyhow::bail!("derive_db_key_bytes: KEK material is empty");
    }
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), kek_bytes);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(HKDF_INFO, &mut okm)
        .map_err(|e| anyhow::anyhow!("HKDF expand failed for libsql db key: {e}"))?;
    Ok(okm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_key_is_32_bytes() {
        let kek = b"some-kek-material-32-bytes-long!";
        let key = derive_db_key_bytes(kek).expect("derive");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn derivation_is_deterministic() {
        let kek = b"stable-kek-bytes";
        let a = derive_db_key_bytes(kek).unwrap();
        let b = derive_db_key_bytes(kek).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_kek_yields_different_key() {
        let a = derive_db_key_bytes(b"kek-one").unwrap();
        let b = derive_db_key_bytes(b"kek-two").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_kek_errors() {
        let err = derive_db_key_bytes(&[]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
