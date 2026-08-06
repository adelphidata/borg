//! Audit chain emitter — single-writer wrapper around `Database::append_audit_event`.

use std::sync::{Arc, OnceLock};

use serde::Serialize;
use tokio::sync::Mutex;

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
pub async fn try_emit_global<T: Serialize>(event_type: &str, principal_id: &str, payload: &T) {
    if let Some(w) = GLOBAL_AUDIT.get() {
        w.emit(event_type, principal_id, payload).await;
    }
}

pub mod audit_events {
    pub const DAEMON_START: &str = "daemon_start";
    pub const DAEMON_STOP: &str = "daemon_stop";
    /// A previous daemon's `daemon_lock` row was still present at boot,
    /// meaning it never reached its shutdown arm — OOM kill, SIGKILL, or
    /// host reset.
    pub const DAEMON_UNCLEAN_EXIT: &str = "daemon_unclean_exit";
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
    pub const WASM_PLUGIN_RATE_LIMITED: &str = "wasm_plugin_rate_limited";
    pub const WASM_PLUGIN_RESEALED: &str = "wasm_plugin_resealed";
    pub const WASM_PLUGIN_DISABLED: &str = "wasm_plugin_disabled";
    pub const WASM_PLUGIN_ENABLED: &str = "wasm_plugin_enabled";
    /// An installed plugin was refused at dispatch because its publisher key is
    /// no longer in `security.trusted_publisher_keys` (trust revoked after
    /// install). Distinct from `WASM_PLUGIN_DISABLED` (explicit kill-switch).
    pub const WASM_PLUGIN_UNTRUSTED: &str = "wasm_plugin_untrusted";

    pub const MCP_TOOL_EXECUTED: &str = "mcp_tool_executed";
    pub const MCP_RATE_LIMITED: &str = "mcp_rate_limited";
    pub const MCP_SCHEMA_REJECTED: &str = "mcp_schema_rejected";
    /// A stdio server config was refused before probe by the static
    /// exfiltration-shape heuristic (shell interpreter + network egress).
    pub const MCP_CONFIG_REJECTED: &str = "mcp_config_rejected";
    pub const MCP_SERVER_STARTED: &str = "mcp_server_started";
    pub const MCP_SERVER_FAILED: &str = "mcp_server_failed";
    pub const MCP_TOOL_REGISTERED: &str = "mcp_tool_registered";
    pub const MCP_RESPONSE_TRUNCATED: &str = "mcp_response_truncated";
    pub const MCP_SAMPLING_REFUSED: &str = "mcp_sampling_refused";
    pub const MCP_BLOCKED_PATH_ATTEMPT: &str = "mcp_blocked_path_attempt";
}

#[derive(Clone)]
pub struct AuditWriter {
    db: Arc<Mutex<Database>>,
}

impl AuditWriter {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db }
    }

    pub async fn emit<T: Serialize>(&self, event_type: &str, principal_id: &str, payload: &T) {
        match serde_json::to_string(payload) {
            Ok(json) => self.emit_raw(event_type, principal_id, &json).await,
            Err(e) => tracing::warn!(
                event = event_type,
                error = %e,
                "audit: failed to serialize event payload — dropping",
            ),
        }
    }

    pub async fn emit_raw(&self, event_type: &str, principal_id: &str, event_json: &str) {
        let ts = chrono::Utc::now().timestamp();
        let guard = self.db.lock().await;
        if let Err(e) = guard
            .append_audit_event(event_type, event_json, principal_id, ts)
            .await
        {
            tracing::warn!(
                event = event_type,
                error = %e,
                "audit: append_audit_event failed",
            );
        }
    }

    pub async fn verify(&self) -> AuditChainOutcome {
        let guard = self.db.lock().await;
        guard
            .verify_audit_chain()
            .await
            .unwrap_or(AuditChainOutcome::Tampered { seq: -1 })
    }

    pub fn db(&self) -> Arc<Mutex<Database>> {
        self.db.clone()
    }
}

#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
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

    async fn make_writer() -> AuditWriter {
        let db = Database::test_db().await;
        AuditWriter::new(Arc::new(Mutex::new(db)))
    }

    #[tokio::test]
    async fn emit_then_verify_chain_intact() {
        let w = make_writer().await;
        for i in 0..5 {
            let p = payload(&[("i", Value::from(i))]);
            w.emit(audit_events::PAIRING_REQUESTED, "system", &p).await;
        }
        assert_eq!(w.verify().await, AuditChainOutcome::Ok);
    }

    #[tokio::test]
    async fn emit_with_unserializable_payload_logs_and_does_not_panic() {
        #[derive(Serialize)]
        struct Bad {
            #[serde(serialize_with = "fail")]
            _x: (),
        }
        fn fail<S: serde::Serializer>(_v: &(), _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
        let w = make_writer().await;
        w.emit(audit_events::PAIRING_REQUESTED, "system", &Bad { _x: () })
            .await;
        assert_eq!(w.verify().await, AuditChainOutcome::Ok);
    }

    #[tokio::test]
    async fn try_emit_global_is_a_silent_noop_when_uninstalled() {
        // Real failure mode: CLI/REPL/test contexts don't initialize the
        // global audit writer. If `try_emit_global` panicked or unwrapped
        // a `None`, every shell denial in a unit test would crash the
        // process. Asserting silent no-op locks the contract in.
        //
        // We can't observe "nothing happened" directly, so we just call
        // it and rely on this not panicking. The "actually emits when
        // installed" path is covered indirectly by the daemon's
        // `pairing_audit_e2e` tests.
        try_emit_global(audit_events::SANDBOX_DENIAL, "system", &payload(&[])).await;
    }
}
