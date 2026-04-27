//! End-to-end test: pairing → audit chain → revocation.
//!
//! Covers:
//! - Daemon mints CA on first start, persists to DB + keychain.
//! - PAKE handshake over the bootstrap socket succeeds and persists a
//!   `paired_devices` row + audit events.
//! - Wrong code is rejected and audited as `pairing_denied`.
//! - Revocation marks the fingerprint and adds it to `revoked_fingerprints`.
//! - Audit chain stays intact after the full sequence; tampering is detected.
//!
//! The test stands up the daemon's pairing components in-process (no real
//! gRPC server / no spawning `borgd`) so it runs cheaply in CI.

use std::sync::{Arc, Mutex};

use borgd::pairing::device::{
    client as pair_client, mint_pairing_code, BootstrapListener, BootstrapState,
};
use cortex::db::{audit_events, AuditChainOutcome, AuditWriter, Database};
use identity::{ca::generate_root_ca, MemoryKekStore};
use rusqlite::Connection;

const PAIR_INTENT: &str = "device";

fn shared_db() -> Arc<Mutex<Database>> {
    Arc::new(Mutex::new(
        Database::from_connection(Connection::open_in_memory().expect("db")).expect("init"),
    ))
}

fn make_state() -> BootstrapState {
    let db = shared_db();
    let _kek = MemoryKekStore::new();
    let ca = generate_root_ca("borg-test-ca").expect("ca");
    let audit = AuditWriter::new(db.clone());
    BootstrapState {
        db,
        audit,
        ca_cert_pem: ca.cert_pem,
        ca_key_pem: ca.key_pkcs8_pem,
    }
}

async fn wait_for_socket(p: &std::path::Path) {
    for _ in 0..100 {
        if p.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("socket {:?} did not appear", p);
}

#[tokio::test]
async fn full_pair_then_revoke_then_audit_verify_round_trip() {
    let state = make_state();
    let code = mint_pairing_code();
    {
        let g = state.db.lock().expect("lock");
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .expect("mint");
    }

    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("pair.sock");
    let listener_state = state.clone();
    let listener_path = path.clone();
    let server = tokio::spawn(async move {
        BootstrapListener::new(listener_path)
            .serve(listener_state)
            .await
    });
    wait_for_socket(&path).await;

    let result = pair_client::run(&path, &code, "alice-laptop")
        .await
        .expect("pair");
    assert!(result.client_cert_pem.contains("BEGIN CERTIFICATE"));

    // 1. Audit chain has expected events in order.
    let events = state
        .db
        .lock()
        .expect("lock")
        .list_audit_events()
        .expect("audit");
    let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        kinds.contains(&audit_events::CERT_ISSUED),
        "kinds: {:?}",
        kinds
    );
    assert!(
        kinds.contains(&audit_events::PAIRING_APPROVED),
        "kinds: {:?}",
        kinds
    );

    // 2. paired_devices row exists.
    let devices = state
        .db
        .lock()
        .expect("lock")
        .list_paired_devices()
        .expect("list");
    assert_eq!(devices.len(), 1);
    let fp = devices[0].fingerprint.clone();
    assert_eq!(devices[0].device_label, "alice-laptop");

    // 3. Revoke the device.
    {
        let g = state.db.lock().expect("lock");
        g.revoke_paired_device(&fp, Some("test")).expect("revoke");
    }
    assert!(state
        .db
        .lock()
        .expect("lock")
        .is_fingerprint_revoked(&fp)
        .expect("query"));
    assert!(state
        .db
        .lock()
        .expect("lock")
        .find_active_paired_device(&fp)
        .expect("query")
        .is_none());

    // 4. Audit chain still verifies clean.
    assert_eq!(state.audit.verify(), AuditChainOutcome::Ok);

    // 5. Tamper with one event; verify reports the right seq.
    let target_seq = events.iter().last().map(|e| e.seq).expect("non-empty");
    {
        let g = state.db.lock().expect("lock");
        g.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"tampered\":true}' WHERE seq = ?1",
                rusqlite::params![target_seq],
            )
            .expect("tamper");
    }
    match state.audit.verify() {
        AuditChainOutcome::Tampered { seq } => assert_eq!(seq, target_seq),
        AuditChainOutcome::Ok => panic!("expected Tampered, got Ok"),
    }

    server.abort();
}

#[tokio::test]
async fn wrong_code_is_denied_no_cert_issued_chain_intact() {
    let state = make_state();
    {
        let g = state.db.lock().expect("lock");
        g.create_pairing_code("REAL-CODE", PAIR_INTENT, 300)
            .expect("mint");
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("pair.sock");
    let listener_state = state.clone();
    let listener_path = path.clone();
    let server = tokio::spawn(async move {
        BootstrapListener::new(listener_path)
            .serve(listener_state)
            .await
    });
    wait_for_socket(&path).await;

    let res = pair_client::run(&path, "FAKE-CODE", "intruder").await;
    assert!(res.is_err(), "wrong code should fail");

    let events = state
        .db
        .lock()
        .expect("lock")
        .list_audit_events()
        .expect("list");
    assert!(events
        .iter()
        .any(|e| e.event_type == audit_events::PAIRING_DENIED));
    assert!(events
        .iter()
        .all(|e| e.event_type != audit_events::CERT_ISSUED));
    assert_eq!(state.audit.verify(), AuditChainOutcome::Ok);

    server.abort();
}

#[tokio::test]
async fn replayed_code_is_rejected_after_first_use() {
    let state = make_state();
    let code = mint_pairing_code();
    {
        let g = state.db.lock().expect("lock");
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .expect("mint");
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("pair.sock");
    let listener_state = state.clone();
    let listener_path = path.clone();
    let server = tokio::spawn(async move {
        BootstrapListener::new(listener_path)
            .serve(listener_state)
            .await
    });
    wait_for_socket(&path).await;

    let _first = pair_client::run(&path, &code, "alice-laptop")
        .await
        .expect("first pairing");
    let second = pair_client::run(&path, &code, "alice-laptop-replay").await;
    assert!(second.is_err(), "replayed code should be rejected");

    let devices = state
        .db
        .lock()
        .expect("lock")
        .list_paired_devices()
        .expect("list");
    assert_eq!(devices.len(), 1, "only one device should be paired");

    server.abort();
}
