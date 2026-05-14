//! Verifier trait + TOFU implementation. The trait shape is set so a
//! Tailscale verifier (or any other transport-layer principal binder)
//! can be added without touching call sites.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use cortex::db::Database;
use thiserror::Error;

/// Per-connection peer context surfaced to verifier implementations.
///
/// The `Tofu` verifier ignores this; the `Tailscale` verifier uses
/// `peer_ip` to cross-check against `tailscale status --json`. Threaded
/// through the gRPC interceptor so every verify path has the same shape.
#[derive(Debug, Clone, Copy)]
pub struct PeerCtx {
    /// Peer IP for TCP connections; `None` over UDS.
    pub peer_ip: Option<IpAddr>,
}

impl PeerCtx {
    /// Empty peer context — useful for in-process or test call sites.
    pub const NONE: PeerCtx = PeerCtx { peer_ip: None };
}

/// Renewal window for client certs: the daemon flags any cert whose
/// `not_after` falls inside the next `RENEWAL_WINDOW_SECS` so the client
/// can hot-swap before connections start failing. Mirrors the 14-day
/// `RENEWAL_WINDOW_DAYS` used for the server cert in
/// `crate::auth::ca_bootstrap`.
pub const RENEWAL_WINDOW_SECS: i64 = 14 * 24 * 60 * 60;

/// Errors returned by [`Verifier`] implementations.
#[derive(Debug, Error)]
pub enum VerifierError {
    /// Fingerprint was found in the revocation list.
    #[error("revoked fingerprint: {0}")]
    Revoked(String),
    /// Fingerprint is not present in `paired_devices`.
    #[error("unknown device fingerprint: {0}")]
    UnknownDevice(String),
    /// Stored fingerprint for a principal differs from the one observed.
    #[error("tofu pin mismatch for principal {principal_id}: expected {expected}, got {observed}")]
    TofuMismatch {
        /// Principal whose pin failed.
        principal_id: String,
        /// Pin currently stored for the principal.
        expected: String,
        /// Pin observed on the connection.
        observed: String,
    },
    /// Database access failed during verification.
    #[error("verifier db failure: {0}")]
    Db(String),
    /// Tailscale status query failed during peer-IP enforcement.
    #[error("tailscale status query failed: {0}")]
    Tailscale(String),
}

/// Sealed marker — see [`Verifier`]. Lives in a module that is reachable to
/// sibling `auth/` submodules but unreachable from outside the crate, so
/// only in-crate types can impl [`Verifier`].
pub(super) mod sealed {
    /// Implemented for every in-crate `Verifier`. The trait itself is
    /// unreachable from outside `borgd::auth`, which seals the [`Verifier`]
    /// trait at the type level.
    pub trait Sealed {}
}

/// Authenticates a peer by certificate fingerprint and returns the principal.
///
/// **Sealed.** Implementations live exclusively inside `borgd::auth` so the
/// auth interceptor's contract (revocation check + paired-device lookup +
/// optional transport pinning) cannot be silently bypassed by an out-of-crate
/// impl. Construction goes through [`super::build_verifier`]; never call
/// [`TofuVerifier::new`] from outside this crate. The sealed constructor
/// enforces this at compile time.
pub trait Verifier: sealed::Sealed + Send + Sync {
    /// Verify `fingerprint` (optionally with a SAN URI hint) and return the principal id.
    fn verify(&self, fingerprint: &str, san_uri: Option<&str>) -> Result<String, VerifierError>;

    /// Verify with peer-connection context. Default delegates to `verify`
    /// so existing impls keep working; verifiers that need transport-layer
    /// info (e.g., Tailscale peer-IP pinning) override this.
    fn verify_with_peer(
        &self,
        fingerprint: &str,
        san_uri: Option<&str>,
        _peer: PeerCtx,
    ) -> Result<String, VerifierError> {
        self.verify(fingerprint, san_uri)
    }
}

/// Trust-on-first-use verifier — first connection from a given fingerprint
/// (already inserted into `paired_devices` during pairing) is accepted; later
/// connections must match the same fingerprint.
pub struct TofuVerifier {
    db: Arc<Mutex<Database>>,
}

impl TofuVerifier {
    /// Build a TOFU verifier backed by the shared database handle.
    ///
    /// Crate-private on purpose: external callers MUST go through
    /// [`super::build_verifier`] so the operator's `security.verifier`
    /// choice is honored and the optional Tailscale wrapper is composed in.
    /// A direct construction would silently downgrade the security posture.
    pub(crate) fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db }
    }
}

impl sealed::Sealed for TofuVerifier {}

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

/// `Some(not_after_unix_seconds)` when the paired device's active cert
/// expires within `RENEWAL_WINDOW_SECS` of `now`, otherwise `None`. Pure
/// over the DB so the auth interceptor can call it on every successful
/// verify without taking on the lifetime cost of a full
/// `latest_active_cert` scan. Returns `None` for unknown / revoked /
/// already-renewed fingerprints — they're either rejected upstream
/// (revoked) or won't reach this codepath (unknown).
pub fn near_expiry(db: &Arc<Mutex<Database>>, fingerprint: &str, now: i64) -> Option<i64> {
    let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let not_after = guard
        .paired_device_cert_expiry(fingerprint)
        .ok()
        .flatten()?;
    let remaining = not_after.saturating_sub(now);
    if (0..=RENEWAL_WINDOW_SECS).contains(&remaining) {
        Some(not_after)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
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
            other => panic!("expected UnknownDevice, got {other:?}"),
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

    fn seed_paired(db: &Arc<Mutex<Database>>, fp: &str, not_after: i64) {
        let g = db.lock().expect("lock");
        let serial = g
            .insert_internal_cert("client", "PEM", fp, "principal", 0, not_after)
            .expect("cert");
        g.insert_paired_device(fp, fp, "{}", fp, serial)
            .expect("pair");
    }

    #[test]
    fn near_expiry_returns_some_inside_window_none_outside() {
        // Real failure mode: a wrong comparator (`<` vs `<=`, signed
        // subtraction, off-by-one) causes the renewal hint to either fire
        // every connection (chain bloat) or never fire (cert silently
        // expires, client is locked out). Three rows pin the boundaries.
        let now = 1_000_000_i64;
        let cases: &[(&str, i64, bool)] = &[
            ("at-now-zero-remaining", now, true),
            ("inside-window", now + RENEWAL_WINDOW_SECS - 1, true),
            ("at-window-boundary", now + RENEWAL_WINDOW_SECS, true),
            ("outside-window", now + RENEWAL_WINDOW_SECS + 1, false),
        ];
        for (fp, not_after, expect_some) in cases {
            let shared = db();
            seed_paired(&shared, fp, *not_after);
            let got = near_expiry(&shared, fp, now);
            assert_eq!(
                got.is_some(),
                *expect_some,
                "{fp}: not_after={not_after} expected_some={expect_some} got={got:?}"
            );
            if *expect_some {
                assert_eq!(got, Some(*not_after));
            }
        }
    }

    #[test]
    fn near_expiry_unknown_fingerprint_returns_none() {
        let shared = db();
        assert_eq!(near_expiry(&shared, "fp-nope", 1_000_000), None);
    }

    #[test]
    fn near_expiry_already_expired_cert_returns_none() {
        // Already-expired certs shouldn't trigger silent renewal — the
        // connection itself should fail at TLS handshake before we ever
        // reach the verifier. Returning None keeps the renewal hint a
        // pure pre-expiry signal.
        let now = 1_000_000_i64;
        let shared = db();
        seed_paired(&shared, "fp-past", now - 1);
        assert_eq!(near_expiry(&shared, "fp-past", now), None);
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
            other => panic!("expected Revoked, got {other:?}"),
        }
    }
}
