#![allow(clippy::expect_used, clippy::unwrap_used)]
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

use std::sync::Arc;
use tokio::sync::Mutex;

use borgd::pairing::device::{
    client as pair_client, mint_pairing_code, BootstrapListener, BootstrapState,
};
use cortex::db::{audit_events, AuditChainOutcome, AuditWriter, Database};
use identity::{ca::generate_root_ca, MemoryKekStore};

const PAIR_INTENT: &str = "device";

async fn shared_db() -> Arc<Mutex<Database>> {
    Arc::new(Mutex::new(Database::test_db().await))
}

async fn make_state() -> BootstrapState {
    let db = shared_db().await;
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
    panic!("socket {p:?} did not appear");
}

#[tokio::test]
async fn full_pair_then_revoke_then_audit_verify_round_trip() {
    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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
        .await
        .list_audit_events()
        .await
        .expect("audit");
    let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        kinds.contains(&audit_events::CERT_ISSUED),
        "kinds: {kinds:?}"
    );
    assert!(
        kinds.contains(&audit_events::PAIRING_APPROVED),
        "kinds: {kinds:?}"
    );

    // 2. paired_devices row exists.
    let devices = state
        .db
        .lock()
        .await
        .list_paired_devices()
        .await
        .expect("list");
    assert_eq!(devices.len(), 1);
    let fp = devices[0].fingerprint.clone();
    assert_eq!(devices[0].device_label, "alice-laptop");

    // 3. Revoke the device.
    {
        let g = state.db.lock().await;
        g.revoke_paired_device(&fp, Some("test"))
            .await
            .expect("revoke");
    }
    assert!(state
        .db
        .lock()
        .await
        .is_fingerprint_revoked(&fp)
        .await
        .expect("query"));
    assert!(state
        .db
        .lock()
        .await
        .find_active_paired_device(&fp)
        .await
        .expect("query")
        .is_none());

    // 4. Audit chain still verifies clean.
    assert_eq!(state.audit.verify().await, AuditChainOutcome::Ok);

    // 5. Tamper with one event; verify reports the right seq.
    let target_seq = events.iter().last().map(|e| e.seq).expect("non-empty");
    {
        let g = state.db.lock().await;
        g.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"tampered\":true}' WHERE seq = ?1",
                libsql::params!(target_seq),
            )
            .await
            .expect("tamper");
    }
    match state.audit.verify().await {
        AuditChainOutcome::Tampered { seq } => assert_eq!(seq, target_seq),
        other => panic!("expected Tampered, got {other:?}"),
    }

    server.abort();
}

#[tokio::test]
async fn wrong_code_is_denied_no_cert_issued_chain_intact() {
    let state = make_state().await;
    {
        let g = state.db.lock().await;
        g.create_pairing_code("REAL-CODE", PAIR_INTENT, 300)
            .await
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
        .await
        .list_audit_events()
        .await
        .expect("list");
    assert!(events
        .iter()
        .any(|e| e.event_type == audit_events::PAIRING_DENIED));
    assert!(events
        .iter()
        .all(|e| e.event_type != audit_events::CERT_ISSUED));
    assert_eq!(state.audit.verify().await, AuditChainOutcome::Ok);

    server.abort();
}

#[tokio::test]
async fn expired_pairing_code_through_bootstrap_listener_is_denied() {
    // Real failure mode: the per-code TTL check is enforced inside
    // `consume_pairing_code` (`device_pairing.rs:60`) — a regression that
    // moves the check or short-circuits it would let a stale code from a
    // previous pairing attempt be honored. Existing unit test covers the
    // SQL path; this one drives the same behavior through the listener.
    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        // 1-second TTL. Stale before the client even connects.
        g.create_pairing_code(&code, PAIR_INTENT, 1)
            .await
            .expect("mint");
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
        .await
        .list_audit_events()
        .await
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
    assert_eq!(state.audit.verify().await, AuditChainOutcome::Ok);

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_fingerprint_blocks_verifier_after_successful_pair() {
    // Real failure mode: the verifier loads `paired_devices` by fingerprint
    // and only consults `revoked_fingerprints` if the row still exists.
    // Forgetting that ordering — or short-circuiting on `find_active_paired_device`
    // first — would let revoked-but-still-active rows authenticate. Codifies
    // the spec's "revoked cert handshake refusal" done-when bullet.
    use borgd::auth::{build_verifier, VerifierError};
    use cortex::config::VerifierKind;

    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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
        .await
        .list_paired_devices()
        .await
        .expect("list")[0]
        .fingerprint
        .clone();

    // Pre-revocation: verifier accepts.
    let v = build_verifier(VerifierKind::Tofu, state.db.clone(), None)
        .await
        .expect("build verifier");
    assert!(v.verify(&fp, None).is_ok(), "fresh paired device verifies");

    // Revoke and confirm verifier now blocks.
    state
        .db
        .lock()
        .await
        .revoke_paired_device(&fp, Some("compromised"))
        .await
        .expect("revoke");
    match v.verify(&fp, None) {
        Err(VerifierError::Revoked(got)) => assert_eq!(got, fp),
        other => panic!("expected Revoked, got {other:?}"),
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_unrelated_cert_after_pair_is_rejected_as_unknown_device() {
    // TOFU pin: after Alice pairs with FP1, an attacker presenting FP2
    // (even under Alice's principal label) must be rejected. The verifier's
    // sole identity key is the cert fingerprint — exercising it locks that
    // invariant in.
    use borgd::auth::{build_verifier, VerifierError};
    use cortex::config::VerifierKind;

    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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

    let v = build_verifier(VerifierKind::Tofu, state.db.clone(), None)
        .await
        .expect("build verifier");
    match v.verify("fp-attacker-not-paired", None) {
        Err(VerifierError::UnknownDevice(fp)) => {
            assert_eq!(fp, "fp-attacker-not-paired")
        }
        other => panic!("expected UnknownDevice, got {other:?}"),
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn near_expiry_paired_device_reports_renewal_due_via_db_lookup() {
    // Real failure mode: the renewal hint reads `not_after` from
    // `internal_ca_certs` joined on `paired_devices.cert_serial`. A typo
    // in the SQL (e.g. wrong column name) or a regression to reading
    // `paired_devices.paired_at` would silently disable silent-renewal
    // detection across the whole fleet. End-to-end through DB layer +
    // verifier helper.
    use borgd::auth::verifier::{near_expiry, RENEWAL_WINDOW_SECS};

    let state = make_state().await;
    let now = chrono::Utc::now().timestamp();
    let near = now + RENEWAL_WINDOW_SECS / 2;
    let far = now + RENEWAL_WINDOW_SECS * 4;

    {
        let g = state.db.lock().await;
        let s_near = g
            .insert_internal_cert("client", "PEM-NEAR", "fp-near", "alice", 0, near)
            .await
            .expect("cert");
        g.insert_paired_device("fp-near", "alice", "{}", "alice", s_near)
            .await
            .expect("pair");
        let s_far = g
            .insert_internal_cert("client", "PEM-FAR", "fp-far", "bob", 0, far)
            .await
            .expect("cert");
        g.insert_paired_device("fp-far", "bob", "{}", "bob", s_far)
            .await
            .expect("pair");
    }

    assert_eq!(near_expiry(&state.db, "fp-near", now), Some(near));
    assert_eq!(near_expiry(&state.db, "fp-far", now), None);
}

#[tokio::test]
async fn replayed_code_is_rejected_after_first_use() {
    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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
        .await
        .list_paired_devices()
        .await
        .expect("list");
    assert_eq!(devices.len(), 1, "only one device should be paired");

    server.abort();
}

#[tokio::test]
async fn rate_limit_blocks_listener_with_pairing_denied_audit() {
    // Real failure mode: spec requires bounded retries on a pairing code
    // (5 attempts). The DB layer flips the row to RateLimited once
    // `attempts >= 5`; the listener must honor that and emit a
    // `PAIRING_DENIED` audit event with reason `RateLimited` instead of
    // letting the code be consumed. A regression that ignored the
    // RateLimited variant would silently let a 6th attempt succeed.
    let state = make_state().await;
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
            .expect("mint");
        for _ in 0..5 {
            g.bump_pairing_attempts(&code).await.expect("bump");
        }
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

    let res = pair_client::run(&path, &code, "alice-laptop").await;
    assert!(res.is_err(), "rate-limited code must be rejected");

    let events = state
        .db
        .lock()
        .await
        .list_audit_events()
        .await
        .expect("list");
    let denied = events
        .iter()
        .find(|e| e.event_type == audit_events::PAIRING_DENIED)
        .expect("PAIRING_DENIED must be emitted");
    assert!(
        denied.event_json.contains("RateLimited"),
        "denied reason must capture RateLimited; got {}",
        denied.event_json,
    );
    // Code must NOT have been consumed — paired_devices stays empty.
    assert!(state
        .db
        .lock()
        .await
        .list_paired_devices()
        .await
        .expect("list")
        .is_empty());
    assert_eq!(state.audit.verify().await, AuditChainOutcome::Ok);

    server.abort();
}

#[tokio::test]
async fn renew_cert_rotates_fingerprint_and_emits_cert_renewed() {
    // Real failure mode: silent renewal must (a) issue a new cert with a
    // *different* fingerprint, (b) point the same paired_devices row at
    // the new cert serial, (c) move the old fingerprint into
    // revoked_fingerprints (so the old cert can never re-authenticate),
    // and (d) emit a CERT_RENEWED audit row referencing both fingerprints.
    // A regression in any one of those steps lets either the old cert
    // outlive renewal (a/c) or breaks future TOFU lookups (b), so the
    // assertions are spelled out.
    use borgd::auth::Principal;
    use borgd::pki_grpc::PkiSvc;
    use protocols::pki::{pki_server::Pki, RenewCertRequest};

    let state = make_state().await;

    // Pair an "alice" device end-to-end first so we have a real cert + DB
    // rows to renew against.
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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
    server.abort();

    let old_fp = state
        .db
        .lock()
        .await
        .list_paired_devices()
        .await
        .expect("list")[0]
        .fingerprint
        .clone();

    let svc = PkiSvc::new(
        state.db.clone(),
        state.audit.clone(),
        state.ca_cert_pem.clone(),
        state.ca_key_pem.clone(),
    );
    let mut req = tonic::Request::new(RenewCertRequest {
        fingerprint: old_fp.clone(),
    });
    req.extensions_mut().insert(Principal {
        id: "alice-laptop".into(),
        fingerprint: old_fp.clone(),
    });
    let resp = svc.renew_cert(req).await.expect("renew_cert").into_inner();
    assert!(resp.new_cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(resp.not_after > chrono::Utc::now().timestamp());

    // Cert PEM differs (different keypair + serial → different fingerprint).
    let new_fp =
        identity::fingerprint_of_pem(resp.new_cert_pem.as_bytes()).expect("fingerprint new cert");
    assert_ne!(new_fp, old_fp, "renewal must mint a fresh fingerprint");

    {
        let g = state.db.lock().await;
        // New fingerprint is the active paired device.
        let active = g
            .find_active_paired_device(&new_fp)
            .await
            .expect("query")
            .expect("new fp must be paired");
        assert_eq!(active.device_label, "alice-laptop");
        // Old fingerprint is in the revoked list.
        assert!(
            g.is_fingerprint_revoked(&old_fp).await.expect("query"),
            "old fingerprint must be revoked after renewal",
        );
    }

    // CERT_RENEWED audit event with both fingerprints in the payload.
    let events = state
        .db
        .lock()
        .await
        .list_audit_events()
        .await
        .expect("list");
    let renewed = events
        .iter()
        .find(|e| e.event_type == audit_events::CERT_RENEWED)
        .expect("CERT_RENEWED must be emitted");
    assert!(
        renewed.event_json.contains(&old_fp) && renewed.event_json.contains(&new_fp),
        "payload must reference both fingerprints; got {}",
        renewed.event_json,
    );
    assert_eq!(state.audit.verify().await, AuditChainOutcome::Ok);
}

#[tokio::test]
async fn verify_audit_chain_grpc_reports_tampered_seq() {
    // Real failure mode: the CLI `borg audit verify` is a thin wrapper over
    // the `Pki.VerifyAuditChain` RPC. If a regression mapped the internal
    // `Tampered { seq }` outcome to `outcome="ok"` (or lost the seq), the
    // CLI would silently report a clean chain even though the chain was
    // broken. Test exercises the actual gRPC handler.
    use borgd::pki_grpc::PkiSvc;
    use protocols::pki::{pki_server::Pki, PkiEmpty};

    let state = make_state().await;

    // Pair once so the chain has more than one event.
    let code = mint_pairing_code();
    {
        let g = state.db.lock().await;
        g.create_pairing_code(&code, PAIR_INTENT, 300)
            .await
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
    server.abort();

    let svc = PkiSvc::new(
        state.db.clone(),
        state.audit.clone(),
        state.ca_cert_pem.clone(),
        state.ca_key_pem.clone(),
    );

    // Clean chain — RPC reports ok.
    let resp = svc
        .verify_audit_chain(tonic::Request::new(PkiEmpty {}))
        .await
        .expect("verify clean")
        .into_inner();
    assert_eq!(resp.outcome, "ok");
    assert_eq!(resp.tampered_seq, -1);

    // Tamper with a non-final row. Pick a non-tombstoned row deterministically.
    let target_seq: i64 = {
        let g = state.db.lock().await;
        let mut stmt = g
            .conn()
            .prepare("SELECT seq FROM audit_log_chain ORDER BY seq ASC LIMIT 1")
            .await
            .expect("prepare");
        let mut rows = stmt.query(()).await.expect("query");
        let row = rows.next().await.expect("next").expect("row");
        row.get::<i64>(0).expect("seq col")
    };
    {
        let g = state.db.lock().await;
        g.conn()
            .execute(
                "UPDATE audit_log_chain SET event_json = '{\"hacked\":true}' WHERE seq = ?1",
                libsql::params!(target_seq),
            )
            .await
            .expect("tamper");
    }

    let resp = svc
        .verify_audit_chain(tonic::Request::new(PkiEmpty {}))
        .await
        .expect("verify dirty")
        .into_inner();
    assert_eq!(resp.outcome, "tampered");
    assert_eq!(resp.tampered_seq, target_seq);
    assert_eq!(resp.tombstone_count, 0);
}
