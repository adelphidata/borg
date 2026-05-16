#![cfg(feature = "phase9-tests-disabled")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Verifies that library-only call sites (settings resolver, shell-policy
//! denial path) actually emit into the audit chain when the daemon's
//! process-global `AuditWriter` is installed.
//!
//! Single test in this file because `install_global_audit` is a `OnceLock`
//! — only one test in this binary may install. CLI/REPL code paths that
//! never install rely on `try_emit_global` no-opping silently, which is
//! covered by the unit test in `cortex/src/db/audit_writer.rs`.

use std::sync::{Arc, Mutex};

use cortex::db::{audit_events, install_global_audit, AuditWriter, Database};
use cortex::settings::SettingsResolver;
use rusqlite::Connection;

#[test]
fn global_audit_emits_settings_changed_for_security_keys_and_skips_others() {
    let db = Database::from_connection(Connection::open_in_memory().expect("db")).expect("init");
    let dbref = Arc::new(Mutex::new(db));
    let writer = AuditWriter::new(dbref.clone());
    assert!(
        install_global_audit(writer),
        "global audit must install on a fresh test binary"
    );

    // The resolver needs *a* settings-capable DB; the audit chain we
    // observe lives in `dbref` via the installed global writer. Two
    // separate in-memory connections is the cleanest setup.
    let resolver_db = Database::from_connection(Connection::open_in_memory().expect("fresh db"))
        .expect("resolver db");
    let resolver = SettingsResolver::new(resolver_db);

    // Security-relevant: should emit.
    resolver
        .set("sandbox.enabled", "false")
        .expect("set sandbox.enabled");
    // Not security-relevant: should NOT emit.
    resolver.set("temperature", "1.1").expect("set temperature");

    let events = dbref
        .lock()
        .expect("lock")
        .list_audit_events()
        .expect("list");
    let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        kinds.contains(&audit_events::SETTINGS_CHANGED),
        "expected SETTINGS_CHANGED for sandbox.enabled, got {kinds:?}"
    );
    let settings_events: Vec<&cortex::db::AuditChainRow> = events
        .iter()
        .filter(|e| e.event_type == audit_events::SETTINGS_CHANGED)
        .collect();
    assert_eq!(
        settings_events.len(),
        1,
        "exactly one settings event (sandbox.enabled), not the temperature change. got: {kinds:?}"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&settings_events[0].event_json).expect("payload json");
    assert_eq!(payload["key"], "sandbox.enabled");
    assert_eq!(payload["new"], "false");
}
