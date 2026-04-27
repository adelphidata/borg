//! OS keychain wrapper for storing PKI private keys at rest.

use crate::error::PkiError;
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// Service name passed to the OS keychain.
pub const SERVICE: &str = "borg-pki";

/// Trait abstracting the keychain so tests can substitute an in-memory store.
pub trait KekStore: Send + Sync {
    /// Store `secret` under `account`. Replaces any existing value.
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError>;
    /// Retrieve the secret previously stored under `account`.
    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError>;
    /// Remove the secret under `account`. No-op if absent.
    fn delete(&self, account: &str) -> Result<(), PkiError>;
}

/// Concrete `KekStore` backed by the OS keychain via `keyring`.
#[derive(Debug, Default)]
pub struct KeychainStore;

impl KeychainStore {
    /// Construct a new keychain-backed store.
    pub fn new() -> Self {
        Self
    }

    fn entry(account: &str) -> Result<keyring::Entry, PkiError> {
        keyring::Entry::new(SERVICE, account)
            .map_err(|e| PkiError::Keychain(format!("entry({}): {}", account, e)))
    }
}

impl KekStore for KeychainStore {
    fn put(&self, account: &str, secret: &[u8]) -> Result<(), PkiError> {
        let entry = Self::entry(account)?;
        entry
            .set_secret(secret)
            .map_err(|e| PkiError::Keychain(format!("set_secret({}): {}", account, e)))
    }

    fn get(&self, account: &str) -> Result<Vec<u8>, PkiError> {
        let entry = Self::entry(account)?;
        entry.get_secret().map_err(|e| match e {
            keyring::Error::NoEntry => PkiError::NotFound(account.to_string()),
            other => PkiError::Keychain(format!("get_secret({}): {}", account, other)),
        })
    }

    fn delete(&self, account: &str) -> Result<(), PkiError> {
        let entry = Self::entry(account)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(PkiError::Keychain(format!(
                "delete({}): {}",
                account, e
            ))),
        }
    }
}

/// In-memory `KekStore` for tests and CI hosts without a working keychain.
#[derive(Debug, Default)]
pub struct MemoryKekStore {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryKekStore {
    /// Build an empty in-memory store.
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

#[cfg(test)]
mod tests {
    use super::*;

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
            other => panic!("expected NotFound, got {:?}", other),
        }
    }
}
