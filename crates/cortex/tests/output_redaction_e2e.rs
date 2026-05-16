#![cfg(feature = "phase9-tests-disabled")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! End-to-end coverage for the output-redaction policy: install the
//! process-global policy with a real SQLite-backed audit sink, drive it
//! through the same call shape that the gateway dispatch uses
//! (`current_policy().apply(OutputSink::Channel(...), ...)`), and assert
//! that:
//!
//!  - secrets are stripped before delivery,
//!  - the `redaction_audit` table records label + count + context_key,
//!  - the raw secret value never reaches the audit row,
//!  - toggling the policy live (re-installing `disabled()`) suppresses
//!    further redaction and audit writes immediately.
//!
//! This test serializes itself behind a mutex because the redaction policy
//! is a process-global; concurrent tests would race over the install.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use cortex::output_redaction::{current_policy, install_global, OutputSink, RedactionPolicy};

#[path = "common/mod.rs"]
mod common;

fn global_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct PolicyGuard {
    prev: RedactionPolicy,
    _lock: MutexGuard<'static, ()>,
}
impl PolicyGuard {
    fn install(policy: RedactionPolicy) -> Self {
        let lock = global_lock();
        let prev = install_global(policy);
        Self { prev, _lock: lock }
    }
}
impl Drop for PolicyGuard {
    fn drop(&mut self) {
        install_global(self.prev.clone());
    }
}

#[test]
fn gateway_call_shape_redacts_and_audits() {
    // Build a fresh test DB and install a policy backed by it.
    let db = Arc::new(Mutex::new(common::test_db()));
    let guard = PolicyGuard::install(RedactionPolicy::from_db(true, db.clone()));

    // This is the exact call shape `gateway::dispatch_webhook` uses today.
    // If the gateway later refactors to a different sink type, the
    // chokepoint guard catches the rename — this test catches a silent
    // behavioural regression.
    let channel_name = "telegram".to_string();
    let context = "user-42:thread-9";
    let response_text = "agent says: AKIAIOSFODNN7EXAMPLE — done".to_string();
    let out = current_policy()
        .apply(
            OutputSink::Channel(&channel_name),
            Some(context),
            &response_text,
        )
        .into_owned();

    assert!(
        !out.contains("AKIAIOSFODNN7EXAMPLE"),
        "gateway sink leaked raw secret: {out:?}"
    );
    assert!(out.contains("[REDACTED AWS Access Key]"));

    // Audit row was written into the same DB the policy was constructed
    // against. Cross-process operators reading the table see the row.
    let rows = db
        .lock()
        .unwrap()
        .recent_redactions(10)
        .expect("read audit");
    assert_eq!(rows.len(), 1, "expected one audit row, got {rows:?}");
    assert_eq!(rows[0].sink, "channel:telegram");
    assert_eq!(rows[0].label, "AWS Access Key");
    assert_eq!(rows[0].hit_count, 1);
    assert_eq!(rows[0].context_key.as_deref(), Some(context));

    // Belt-and-suspenders: the raw secret must not appear anywhere in the
    // audit row's serialized form. A future change that adds a "context
    // preview" column populated from `text` would fail here.
    let secret = "AKIAIOSFODNN7EXAMPLE";
    assert!(!rows[0].sink.contains(secret));
    assert!(!rows[0].label.contains(secret));
    assert!(!rows[0]
        .context_key
        .as_deref()
        .unwrap_or("")
        .contains(secret));

    drop(guard);
}

#[test]
fn live_toggle_disables_redaction_immediately() {
    // Operator-toggle contract: flipping `security.output_redaction` from
    // true to false reinstalls the global policy. The very next message
    // must pass through unchanged, and no audit row may be written.
    let db = Arc::new(Mutex::new(common::test_db()));
    let guard = PolicyGuard::install(RedactionPolicy::from_db(true, db.clone()));

    // First call with policy ON — produces an audit row.
    let _ = current_policy()
        .apply(
            OutputSink::Channel("slack"),
            Some("u:t"),
            "k=AKIAIOSFODNN7EXAMPLE",
        )
        .into_owned();
    let before = db.lock().unwrap().count_redactions().unwrap();
    assert_eq!(before, 1);

    // Re-install with policy OFF — emulates `/settings` toggling
    // `security.output_redaction` to false at runtime.
    install_global(RedactionPolicy::disabled());

    let raw = "k=AKIAIOSFODNN7EXAMPLE";
    let out = current_policy()
        .apply(OutputSink::Channel("slack"), Some("u:t"), raw)
        .into_owned();
    assert_eq!(out, raw, "disabled policy must pass through verbatim");
    let after = db.lock().unwrap().count_redactions().unwrap();
    assert_eq!(after, before, "no new audit row after live disable");

    drop(guard);
}
