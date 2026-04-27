//! Verifier trait + TOFU implementation. The trait shape is set so a
//! Tailscale verifier (or any other transport-layer principal binder)
//! can be added without touching call sites.

use std::sync::{Arc, Mutex};

use cortex::db::Database;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VerifierError {
    #[error("revoked fingerprint: {0}")]
    Revoked(String),
    #[error("unknown device fingerprint: {0}")]
    UnknownDevice(String),
    #[error("tofu pin mismatch for principal {principal_id}: expected {expected}, got {observed}")]
    TofuMismatch {
        principal_id: String,
        expected: String,
        observed: String,
    },
    #[error("verifier db failure: {0}")]
    Db(String),
}

pub trait Verifier: Send + Sync {
    fn verify(&self, fingerprint: &str, san_uri: Option<&str>) -> Result<String, VerifierError>;
}

/// Trust-on-first-use verifier — first connection from a given fingerprint
/// (already inserted into `paired_devices` during pairing) is accepted; later
/// connections must match the same fingerprint.
pub struct TofuVerifier {
    db: Arc<Mutex<Database>>,
}

impl TofuVerifier {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db }
    }
}

impl Verifier for TofuVerifier {
    fn verify(&self, fingerprint: &str, _san_uri: Option<&str>) -> Result<String, VerifierError> {
        let guard = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if guard
            .is_fingerprint_revoked(fingerprint)
            .map_err(|e| VerifierError::Db(e.to_string()))?
        {
            return Err(VerifierError::Revoked(fingerprint.to_string()));
        }

        let row = guard
            .find_active_paired_device(fingerprint)
            .map_err(|e| VerifierError::Db(e.to_string()))?;
        match row {
            Some(d) => {
                let _ = guard.touch_paired_device(fingerprint);
                Ok(d.principal_id)
            }
            None => Err(VerifierError::UnknownDevice(fingerprint.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Arc<Mutex<Database>> {
        let d = Database::from_connection(Connection::open_in_memory().expect("db")).expect("init");
        Arc::new(Mutex::new(d))
    }

    #[test]
    fn unknown_fingerprint_is_rejected() {
        let v = TofuVerifier::new(db());
        match v.verify("fp-stranger", None) {
            Err(VerifierError::UnknownDevice(fp)) => assert_eq!(fp, "fp-stranger"),
            other => panic!("expected UnknownDevice, got {:?}", other),
        }
    }

    #[test]
    fn paired_device_is_accepted_and_returns_principal() {
        let shared = db();
        {
            let g = shared.lock().expect("lock");
            let serial = g
                .insert_internal_cert("client", "PEM", "fp-A", "alice", 0, 1_000_000)
                .expect("cert");
            g.insert_paired_device("fp-A", "alice", "{}", "alice", serial)
                .expect("pair");
        }
        let v = TofuVerifier::new(shared);
        assert_eq!(v.verify("fp-A", None).expect("verify"), "alice");
    }

    #[test]
    fn revoked_fingerprint_is_rejected_even_if_paired() {
        let shared = db();
        {
            let g = shared.lock().expect("lock");
            let serial = g
                .insert_internal_cert("client", "PEM", "fp-bad", "evil", 0, 1_000_000)
                .expect("cert");
            g.insert_paired_device("fp-bad", "evil", "{}", "evil", serial)
                .expect("pair");
            g.add_revoked_fingerprint("fp-bad", Some("compromised"))
                .expect("revoke");
        }
        let v = TofuVerifier::new(shared);
        match v.verify("fp-bad", None) {
            Err(VerifierError::Revoked(fp)) => assert_eq!(fp, "fp-bad"),
            other => panic!("expected Revoked, got {:?}", other),
        }
    }
}
