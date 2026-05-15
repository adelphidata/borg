//! Turso DB encryption key derivation.
//!
//! Turso's `with_encryption()` accepts a 64-char hex string (32 bytes of key
//! material) for AES-256-GCM. We derive that key from the existing KEK
//! (`security.kek_backend`) via HKDF-SHA256 so the DB encryption key shares
//! the same OS-keychain / passphrase-file trust root as the rest of borg's
//! secret storage. The KEK never leaves the keystore; only the derived
//! sub-key reaches Turso.

use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;

/// HKDF salt — domain-separates the DB key from any other sub-key derived
/// from the same KEK. Versioned so future rotations can re-key without
/// colliding with old derivations.
const HKDF_SALT: &[u8] = b"borg/db-encryption/v1";

/// HKDF info — pins the output to the cipher we hand to Turso.
const HKDF_INFO: &[u8] = b"turso-aes256gcm";

/// Length of the derived key in bytes (AES-256).
const KEY_LEN: usize = 32;

/// Derive a 64-char hex-encoded AES-256 key from raw KEK bytes via HKDF-SHA256.
///
/// `kek_bytes` is the secret pulled from the configured `KekStore`. The
/// derivation is deterministic — same KEK + salt + info always produces the
/// same hex string, which is what lets Turso reopen an existing encrypted
/// DB across restarts without persisting the derived key anywhere.
pub fn derive_db_hexkey(kek_bytes: &[u8]) -> Result<String> {
    if kek_bytes.is_empty() {
        anyhow::bail!("derive_db_hexkey: KEK material is empty");
    }
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), kek_bytes);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(HKDF_INFO, &mut okm)
        .map_err(|e| anyhow::anyhow!("HKDF expand failed for turso db key: {e}"))?;
    Ok(hex_encode(&okm))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_key_is_64_hex_chars() {
        let kek = b"some-kek-material-32-bytes-long!";
        let hex = derive_db_hexkey(kek).expect("derive");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn derivation_is_deterministic() {
        let kek = b"stable-kek-bytes";
        let a = derive_db_hexkey(kek).unwrap();
        let b = derive_db_hexkey(kek).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_kek_yields_different_key() {
        let a = derive_db_hexkey(b"kek-one").unwrap();
        let b = derive_db_hexkey(b"kek-two").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_kek_errors() {
        let err = derive_db_hexkey(&[]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
