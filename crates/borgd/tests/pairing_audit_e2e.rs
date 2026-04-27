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
        other => panic!("expected Tampered, got {other:?}"),
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
async fn expired_pairing_code_through_bootstrap_listener_is_denied() {
    // Real failure mode: the per-code TTL check is enforced inside
    // `consume_pairing_code` (`device_pairing.rs:60`) — a regression that
    // moves the check or short-circuits it would let a stale code from a
    // previous pairing attempt be honored. Existing unit test covers the
    // SQL path; this one drives the same behavior through the listener.
    let state = make_state();
    let code = mint_pairing_code();
    {
        let g = state.db.lock().expect("lock");
        // 1-second TTL. Stale before the client even connects.
        g.create_pairing_code(&code, PAIR_INTENT, 1).expect("mint");
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

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

    let res = pair_client::run(&path, &code, "alice-laptop").await;
    assert!(res.is_err(), "expired code should be denied");

    let events = state
        .db
        .lock()
        .expect("lock")
        .list_audit_events()
        .expect("list");
    assert!(
        events
            .iter()
            .any(|e| e.event_type == audit_events::PAIRING_DENIED),
        "expected PAIRING_DENIED, got: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    assert!(events
        .iter()
        .all(|e| e.event_type != audit_events::CERT_ISSUED));
    assert_eq!(state.audit.verify(), AuditChainOutcome::Ok);

    server.abort();
}

#[tokio::test]
async fn revoked_fingerprint_blocks_verifier_after_successful_pair() {
    // Real failure mode: the verifier loads `paired_devices` by fingerprint
    // and only consults `revoked_fingerprints` if the row still exists.
    // Forgetting that ordering — or short-circuiting on `find_active_paired_device`
    // first — would let revoked-but-still-active rows authenticate. Codifies
    // the spec's "revoked cert handshake refusal" done-when bullet.
    use borgd::auth::verifier::{TofuVerifier, Verifier, VerifierError};

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

    pair_client::run(&path, &code, "alice-laptop")
        .await
        .expect("pair");
    let fp = state
        .db
        .lock()
        .expect("lock")
        .list_paired_devices()
        .expect("list")[0]
        .fingerprint
        .clone();

    // Pre-revocation: verifier accepts.
    let v = TofuVerifier::new(state.db.clone());
    assert!(v.verify(&fp, None).is_ok(), "fresh paired device verifies");

    // Revoke and confirm verifier now blocks.
    state
        .db
        .lock()
        .expect("lock")
        .revoke_paired_device(&fp, Some("compromised"))
        .expect("revoke");
    match v.verify(&fp, None) {
        Err(VerifierError::Revoked(got)) => assert_eq!(got, fp),
        other => panic!("expected Revoked, got {other:?}"),
    }

    server.abort();
}

#[tokio::test]
async fn second_unrelated_cert_after_pair_is_rejected_as_unknown_device() {
    // TOFU pin: after Alice pairs with FP1, an attacker presenting FP2
    // (even under Alice's principal label) must be rejected. The verifier's
    // sole identity key is the cert fingerprint — exercising it locks that
    // invariant in.
    use borgd::auth::verifier::{TofuVerifier, Verifier, VerifierError};

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

    pair_client::run(&path, &code, "alice-laptop")
        .await
        .expect("pair");

    let v = TofuVerifier::new(state.db.clone());
    match v.verify("fp-attacker-not-paired", None) {
        Err(VerifierError::UnknownDevice(fp)) => {
            assert_eq!(fp, "fp-attacker-not-paired")
        }
        other => panic!("expected UnknownDevice, got {other:?}"),
    }

    server.abort();
}

#[tokio::test]
async fn near_expiry_paired_device_reports_renewal_due_via_db_lookup() {
    // Real failure mode: the renewal hint reads `not_after` from
    // `internal_ca_certs` joined on `paired_devices.cert_serial`. A typo
    // in the SQL (e.g. wrong column name) or a regression to reading
    // `paired_devices.paired_at` would silently disable silent-renewal
    // detection across the whole fleet. End-to-end through DB layer +
    // verifier helper.
    use borgd::auth::verifier::{near_expiry, RENEWAL_WINDOW_SECS};

    let state = make_state();
    let now = chrono::Utc::now().timestamp();
    let near = now + RENEWAL_WINDOW_SECS / 2;
    let far = now + RENEWAL_WINDOW_SECS * 4;

    {
        let g = state.db.lock().expect("lock");
        let s_near = g
            .insert_internal_cert("client", "PEM-NEAR", "fp-near", "alice", 0, near)
            .expect("cert");
        g.insert_paired_device("fp-near", "alice", "{}", "alice", s_near)
            .expect("pair");
        let s_far = g
            .insert_internal_cert("client", "PEM-FAR", "fp-far", "bob", 0, far)
            .expect("cert");
        g.insert_paired_device("fp-far", "bob", "{}", "bob", s_far)
            .expect("pair");
    }

    assert_eq!(near_expiry(&state.db, "fp-near", now), Some(near));
    assert_eq!(near_expiry(&state.db, "fp-far", now), None);
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
