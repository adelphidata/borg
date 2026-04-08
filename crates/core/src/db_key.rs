//! Database encryption key management for SQLCipher.
//!
//! Generates a random 256-bit key on first use, stores it in the OS keychain
//! (macOS Keychain / Linux secret-tool), and falls back to a file at
//! `~/.borg/.db_key` (mode 0600) when no keychain is available.
//!
//! The key is never logged, printed, or persisted in config files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

const KEYCHAIN_SERVICE: &str = "borg";
const KEYCHAIN_ACCOUNT: &str = "db-encryption-key";
const KEY_FILE_NAME: &str = ".db_key";
const KEY_LEN: usize = 32; // 256-bit

/// Get or create the database encryption key.
/// Tries OS keychain first, falls back to file at `~/.borg/.db_key` (mode 0600).
pub fn get_or_create_db_key() -> Result<Vec<u8>> {
    // Try reading from keychain
    if let Some(key) = read_key_keychain() {
        tracing::debug!("DB encryption key loaded from keychain");
        return Ok(key);
    }

    // Try reading from file
    let key_path = key_file_path()?;
    if key_path.exists() {
        let hex = std::fs::read_to_string(&key_path).context("Failed to read DB key file")?;
        let key = hex_decode(hex.trim())?;
        tracing::debug!("DB encryption key loaded from file");
        return Ok(key);
    }

    // Generate new key
    let key = generate_random_key();

    // Try storing in keychain first
    if store_key_keychain(&key) {
        tracing::info!("DB encryption key generated and stored in keychain");
        return Ok(key);
    }

    // Fall back to file
    write_key_file(&key_path, &key)?;
    tracing::info!("DB encryption key generated and stored in file");
    Ok(key)
}

fn generate_random_key() -> Vec<u8> {
    use rand::RngCore;
    let mut key = vec![0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut key);
    key
}

fn key_file_path() -> Result<PathBuf> {
    Ok(Config::data_dir()?.join(KEY_FILE_NAME))
}

fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
    let hex = hex_encode(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(hex.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, &hex)?;
    }
    Ok(())
}

fn read_key_keychain() -> Option<Vec<u8>> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
                "-w",
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let hex = String::from_utf8(output.stdout).ok()?;
            return hex_decode(hex.trim()).ok();
        }
    } else if cfg!(target_os = "linux") {
        let output = std::process::Command::new("secret-tool")
            .args([
                "lookup",
                "service",
                KEYCHAIN_SERVICE,
                "key",
                KEYCHAIN_ACCOUNT,
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let hex = String::from_utf8(output.stdout).ok()?;
            return hex_decode(hex.trim()).ok();
        }
    }
    None
}

/// Store key in keychain. Returns true on success.
fn store_key_keychain(key: &[u8]) -> bool {
    let hex = hex_encode(key);
    if cfg!(target_os = "macos") {
        std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
                "-w",
                &hex,
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        use std::io::Write;
        let mut child = match std::process::Command::new("secret-tool")
            .args([
                "store",
                "--label=borg-db-encryption-key",
                "service",
                KEYCHAIN_SERVICE,
                "key",
                KEYCHAIN_ACCOUNT,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(hex.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    } else {
        false
    }
}

/// Hex-encode a byte slice to a lowercase hex string.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a hex string into bytes.
pub(crate) fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("Invalid hex string length: {}", hex.len());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("Invalid hex at position {i}"))
        })
        .collect()
}

/// Format a raw key as a SQLCipher hex key literal: `x'...'`
pub fn format_sqlcipher_key(key: &[u8]) -> String {
    format!("x'{}'", hex_encode(key))
}

/// Generate a random 256-bit key. Exposed for integration tests.
pub fn generate_random_key_for_test() -> Vec<u8> {
    generate_random_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_generation_produces_correct_length() {
        let key = generate_random_key();
        assert_eq!(key.len(), KEY_LEN);
    }

    #[test]
    fn key_generation_not_all_zeros() {
        let key = generate_random_key();
        assert!(key.iter().any(|&b| b != 0), "key should not be all zeros");
    }

    #[test]
    fn key_generation_unique() {
        let k1 = generate_random_key();
        let k2 = generate_random_key();
        assert_ne!(k1, k2, "two generated keys should differ");
    }

    #[test]
    fn hex_round_trip() {
        let original = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0xff];
        let encoded = hex_encode(&original);
        assert_eq!(encoded, "deadbeef00ff");
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn hex_decode_invalid_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_invalid_chars() {
        assert!(hex_decode("zzzz").is_err());
    }

    #[test]
    fn format_sqlcipher_key_format() {
        let key = vec![0xab, 0xcd, 0xef];
        assert_eq!(format_sqlcipher_key(&key), "x'abcdef'");
    }

    #[test]
    fn key_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".db_key");
        let key = generate_random_key();
        write_key_file(&path, &key).unwrap();

        let hex = std::fs::read_to_string(&path).unwrap();
        let loaded = hex_decode(hex.trim()).unwrap();
        assert_eq!(loaded, key);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".db_key");
        let key = generate_random_key();
        write_key_file(&path, &key).unwrap();

        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}
