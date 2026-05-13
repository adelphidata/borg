//! Audit chain emitter — single-writer wrapper around `Database::append_audit_event`.

use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;

use super::{AuditChainOutcome, Database};

/// Process-global audit writer. Installed by `borgd::daemon::run` once the
/// daemon-owned `Database` is open. Library call sites (shell handler,
/// settings resolver, …) emit through this so they don't have to plumb an
/// `AuditWriter` through every signature.
///
/// In CLI/REPL/test contexts the global stays uninstalled and emitters
/// no-op — those paths don't share the daemon's writer-mutex and would
/// otherwise contend on the SQLite file.
static GLOBAL_AUDIT: OnceLock<AuditWriter> = OnceLock::new();

/// Install the process-global audit writer. Returns true on first install,
/// false if one was already installed (later calls are intentionally
/// ignored — single-installer invariant). Calling this from a fresh test
/// process is a no-op-safe operation.
pub fn install_global_audit(writer: AuditWriter) -> bool {
    GLOBAL_AUDIT.set(writer).is_ok()
}

/// Emit through the process-global writer if one is installed; otherwise
/// silently no-op. Use this from library-only call sites that can't accept
/// an `AuditWriter` parameter without churn (e.g. tool handlers that
/// originate from both the daemon and CLI one-shots).
pub fn try_emit_global<T: Serialize>(event_type: &str, principal_id: &str, payload: &T) {
    if let Some(w) = GLOBAL_AUDIT.get() {
        w.emit(event_type, principal_id, payload);
    }
}

pub mod audit_events {
    pub const DAEMON_START: &str = "daemon_start";
    pub const DAEMON_STOP: &str = "daemon_stop";
    pub const PAIRING_REQUESTED: &str = "pairing_requested";
    pub const PAIRING_APPROVED: &str = "pairing_approved";
    pub const PAIRING_DENIED: &str = "pairing_denied";
    pub const CERT_ISSUED: &str = "cert_issued";
    pub const CERT_RENEWED: &str = "cert_renewed";
    pub const CERT_REVOKED: &str = "cert_revoked";
    pub const GRPC_AUTH_FAILURE: &str = "grpc_auth_failure";
    pub const TOFU_MISMATCH: &str = "tofu_mismatch";
    pub const SANDBOX_DENIAL: &str = "sandbox_denial";
    pub const BUDGET_HIT: &str = "budget_hit";
    pub const POSTURE_CHANGED: &str = "posture_changed";
    pub const SETTINGS_CHANGED: &str = "settings_changed";
    pub const WASM_PLUGIN_INSTALLED: &str = "wasm_plugin_installed";
    pub const WASM_PLUGIN_EXECUTED: &str = "wasm_plugin_executed";
    pub const WASM_PLUGIN_QUARANTINED: &str = "wasm_plugin_quarantined";
}

#[derive(Clone)]
pub struct AuditWriter {
    db: Arc<Mutex<Database>>,
}

impl AuditWriter {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db }
    }

    pub fn emit<T: Serialize>(&self, event_type: &str, principal_id: &str, payload: &T) {
        match serde_json::to_string(payload) {
            Ok(json) => self.emit_raw(event_type, principal_id, &json),
            Err(e) => tracing::warn!(
                event = event_type,
                error = %e,
                "audit: failed to serialize event payload — dropping",
            ),
        }
    }

    pub fn emit_raw(&self, event_type: &str, principal_id: &str, event_json: &str) {
        let ts = chrono::Utc::now().timestamp();
        let guard = match self.db.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!(event = event_type, "audit: db mutex poisoned — recovering");
                poisoned.into_inner()
            }
        };
        if let Err(e) = guard.append_audit_event(event_type, event_json, principal_id, ts) {
            tracing::warn!(
                event = event_type,
                error = %e,
                "audit: append_audit_event failed",
            );
        }
    }

    pub fn verify(&self) -> AuditChainOutcome {
        let guard = match self.db.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .verify_audit_chain()
            .unwrap_or(AuditChainOutcome::Tampered { seq: -1 })
    }

    pub fn db(&self) -> Arc<Mutex<Database>> {
        self.db.clone()
    }
}

#[allow(dead_code)]
pub fn payload(pairs: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    Value::Object(map)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::trivially_copy_pass_by_ref
)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn make_writer() -> AuditWriter {
        let db =
            Database::from_connection(Connection::open_in_memory().expect("in-mem")).expect("init");
        AuditWriter::new(Arc::new(Mutex::new(db)))
    }

    #[test]
    fn emit_then_verify_chain_intact() {
        let w = make_writer();
        for i in 0..5 {
            let p = payload(&[("i", Value::from(i))]);
            w.emit(audit_events::PAIRING_REQUESTED, "system", &p);
        }
        assert_eq!(w.verify(), AuditChainOutcome::Ok);
    }

    #[test]
    fn emit_with_unserializable_payload_logs_and_does_not_panic() {
        #[derive(Serialize)]
        struct Bad {
            #[serde(serialize_with = "fail")]
            _x: (),
        }
        fn fail<S: serde::Serializer>(_v: &(), _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
        let w = make_writer();
        w.emit(audit_events::PAIRING_REQUESTED, "system", &Bad { _x: () });
        assert_eq!(w.verify(), AuditChainOutcome::Ok);
    }

    #[test]
    fn try_emit_global_is_a_silent_noop_when_uninstalled() {
        // Real failure mode: CLI/REPL/test contexts don't initialize the
        // global audit writer. If `try_emit_global` panicked or unwrapped
        // a `None`, every shell denial in a unit test would crash the
        // process. Asserting silent no-op locks the contract in.
        //
        // We can't observe "nothing happened" directly, so we just call
        // it and rely on this not panicking. The "actually emits when
        // installed" path is covered indirectly by the daemon's
        // `pairing_audit_e2e` tests.
        try_emit_global(audit_events::SANDBOX_DENIAL, "system", &payload(&[]));
    }

    #[test]
    fn emit_after_poisoned_mutex_recovers() {
        let w = make_writer();
        let dbref = w.db.clone();
        let _ = std::thread::spawn(move || {
            let _g = dbref.lock();
            panic!("intentional poison");
        })
        .join();
        w.emit(audit_events::DAEMON_START, "system", &payload(&[]));
        assert_eq!(w.verify(), AuditChainOutcome::Ok);
        let guard = w.db.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(guard.list_audit_events().expect("list").len(), 1);
    }
}
