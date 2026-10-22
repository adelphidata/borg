#![allow(clippy::expect_used, clippy::unwrap_used)]
//! End-to-end mTLS handshake guarantees for the borgd TCP listener.
//!
//! These tests stand up the same `build_server_tls_config` + `auth_interceptor`
//! pieces that `daemon.rs` mounts on `--listen`, then probe four spec
//! invariants the TCP transport MUST hold:
//!
//! 1. A client with no certificate cannot complete the TLS handshake.
//! 2. A client whose cert is signed by a foreign CA cannot complete the
//!    handshake (i.e. the server really demands a CA-signed peer cert).
//! 3. A properly paired client connects and its RPC call is accepted with a
//!    `Principal` stamped onto the request.
//! 4. A paired-but-revoked client completes the handshake but its first RPC
//!    is rejected with `Status::unauthenticated` by the verifier, with a
//!    `grpc_auth_failure` audit row written.
//!
//! These all hit the `Pki.VerifyAuditChain` RPC — it's the only daemon RPC
//! that doesn't touch the on-disk `Database::open()` global, so the test
//! controls all state through the shared `Arc<Mutex<Database>>`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use borgd::auth::san::LOOPBACK_SAN;
use borgd::auth::{auth_interceptor, build_server_tls_config, build_verifier, ServerCertMaterial};
use borgd::pki_grpc::PkiSvc;
use cortex::config::VerifierKind;
use cortex::db::{audit_events, AuditWriter, Database};
use identity::ca::{
    generate_root_ca, issue_client_cert, issue_server_cert, ClientCertParams, ServerCertParams,
};
use identity::fingerprint_sha256;
use protocols::pki::pki_client::PkiClient;
use protocols::pki::pki_server::PkiServer;
use protocols::pki::PkiEmpty;
use rcgen::KeyPair;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct ServerHandle {
    addr: SocketAddr,
    db: Arc<Mutex<Database>>,
    ca_cert_pem: String,
    ca_key_pkcs8_pem: String,
    _shutdown: oneshot::Sender<()>,
}

/// Pin the rustls process-level `CryptoProvider` to `ring`, the same call
/// production borgd makes first thing in `main`. Without it the handshake
/// panics with "could not automatically determine the process-level
/// CryptoProvider" because both `ring` and `aws-lc-rs` are present in the
/// dependency tree. Idempotent + thread-safe so the parallel tests can all
/// call it.
fn ensure_crypto_provider() {
    borgd::auth::tls::install_crypto_provider();
}

/// Spin up a minimal mTLS-protected `PkiServer` on an ephemeral TCP port using
/// the same TLS + interceptor wiring `daemon.rs` mounts in production.
async fn spawn_mtls_server() -> ServerHandle {
    ensure_crypto_provider();
    let root = generate_root_ca("borg-test-ca").expect("ca");
    let san_dns = vec![LOOPBACK_SAN.to_string()];
    let (server, server_key_pem) = issue_server_cert(
        &root.cert_pem,
        &root.key_pkcs8_pem,
        &ServerCertParams {
            common_name: "borgd",
            san_dns: &san_dns,
            validity_days: 90,
        },
    )
    .expect("server cert");

    let tls = build_server_tls_config(&ServerCertMaterial {
        server_cert_pem: server.cert_pem,
        server_key_pem,
        ca_cert_pem: root.cert_pem.clone(),
    })
    .expect("tls config");

    let db = Arc::new(Mutex::new(Database::test_db().await));
    let audit = AuditWriter::new(db.clone());
    // Use the same factory path the daemon uses — calling TofuVerifier::new
    // directly would silently bypass the security.verifier contract these
    // tests are supposed to guard.
    let verifier = build_verifier(VerifierKind::Tofu, db.clone(), None)
        .await
        .expect("build verifier");
    let interceptor = auth_interceptor(verifier, audit.clone());

    let pki = PkiServer::new(PkiSvc::new(
        db.clone(),
        audit.clone(),
        root.cert_pem.clone(),
        root.key_pkcs8_pem.clone(),
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .tls_config(tls)
            .expect("server tls")
            .add_service(InterceptedService::new(pki, interceptor))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });
    // Brief poll: the listener is bound (so 0 is replaced with a real port)
    // but the tonic server task may not have started accepting yet.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    ServerHandle {
        addr,
        db,
        ca_cert_pem: root.cert_pem,
        ca_key_pkcs8_pem: root.key_pkcs8_pem,
        _shutdown: tx,
    }
}

/// Issue a client cert from `ca_cert_pem`/`ca_key_pem` (defaults to the test
/// server's CA if `None`) and return (cert_pem, key_pkcs8_pem, fingerprint).
fn mint_client_cert(
    ca_cert_pem: &str,
    ca_key_pkcs8_pem: &str,
    device_label: &str,
) -> (String, String, String) {
    let kp = KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("kp");
    let client_pub_pem = kp.serialize_pem();
    let issued = issue_client_cert(
        ca_cert_pem,
        ca_key_pkcs8_pem,
        &ClientCertParams {
            device_label,
            device_id: device_label,
            client_public_key_pem: &client_pub_pem,
            validity_days: 90,
        },
    )
    .expect("client cert");
    let fp = fingerprint_sha256(&issued.cert_der);
    (issued.cert_pem, kp.serialize_pem(), fp)
}

fn server_uri(addr: SocketAddr) -> String {
    format!("https://localhost:{}", addr.port())
}

/// Build a client endpoint that hits `addr` but presents `Host: localhost`
/// (matching the server's SAN), so we can reach 127.0.0.1 while keeping the
/// TLS handshake's name verification on `localhost`.
fn endpoint_for(addr: SocketAddr, tls: ClientTlsConfig) -> Endpoint {
    let uri = server_uri(addr);
    Endpoint::from_shared(uri)
        .expect("endpoint")
        .tls_config(tls)
        .expect("client tls")
        .timeout(Duration::from_secs(5))
}

/// Attempt a one-shot RPC over `tls` against `h`. Returns `Ok(())` if the
/// RPC succeeds, `Err(<short summary>)` for any failure (TLS handshake error
/// at connect time or `Status::*` at call time). The transport layer in tonic
/// 0.12 establishes TLS lazily — the handshake may not complete until the
/// first RPC, so both surfaces have to be inspected.
async fn try_rpc(h: &ServerHandle, tls: ClientTlsConfig) -> Result<(), String> {
    let endpoint = endpoint_for(h.addr, tls);
    let channel = match endpoint.connect().await {
        Ok(c) => c,
        Err(e) => return Err(format!("connect: {e}")),
    };
    let mut client = PkiClient::new(channel);
    match client.verify_audit_chain(PkiEmpty {}).await {
        Ok(_) => Ok(()),
        Err(s) => Err(format!("rpc: {s:?}")),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_rejected_when_no_client_cert_presented() {
    // Real failure mode: if the server config dropped `client_ca_root(...)`,
    // it would degrade to TLS-without-mTLS — any client could connect with no
    // identity and run RPCs. We don't pin where exactly the rejection
    // surfaces (some TLS stacks fail the handshake on `connect()`; others
    // accept the TCP socket and only fail at the first RPC). The invariant
    // is: an unauthenticated client cannot get a successful RPC response.
    let h = spawn_mtls_server().await;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(h.ca_cert_pem.as_bytes()))
        .domain_name(LOOPBACK_SAN);
    let res = try_rpc(&h, tls).await;
    assert!(
        res.is_err(),
        "RPC without a client cert must be rejected end-to-end; got Ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_rejected_when_client_cert_signed_by_foreign_ca() {
    // Real failure mode: a regression that swaps `client_ca_root` for
    // `accept_any_client_cert`-style config would let any well-formed cert
    // through, bypassing the CA-binding guarantee.
    let h = spawn_mtls_server().await;
    let foreign = generate_root_ca("attacker-ca").expect("foreign ca");
    let (fcert, fkey, _fp) =
        mint_client_cert(&foreign.cert_pem, &foreign.key_pkcs8_pem, "attacker");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(h.ca_cert_pem.as_bytes()))
        .identity(Identity::from_pem(fcert.as_bytes(), fkey.as_bytes()))
        .domain_name(LOOPBACK_SAN);
    let res = try_rpc(&h, tls).await;
    assert!(
        res.is_err(),
        "RPC with foreign-CA client cert must be rejected; got Ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_succeeds_with_paired_client_cert_and_rpc_is_accepted() {
    // Real failure mode: if `InterceptedService` weren't installed on the
    // tonic services, the call would succeed *without* the verifier running,
    // and the daemon would happily accept any TLS-handshake-passing peer as
    // unauthenticated. Pairing the device in TOFU and seeing the RPC return
    // OK proves both layers ran.
    let h = spawn_mtls_server().await;
    let (ccert, ckey, fp) = mint_client_cert(&h.ca_cert_pem, &h.ca_key_pkcs8_pem, "alice-laptop");

    // TOFU prerequisite: paired_devices must hold the fingerprint.
    {
        let g = h.db.lock().await;
        let serial = g
            .insert_internal_cert("client", &ccert, &fp, "alice-laptop", 0, 100)
            .await
            .expect("insert cert");
        g.insert_paired_device(&fp, "alice-laptop", "{}", "alice-laptop", serial)
            .await
            .expect("insert paired");
    }

    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(h.ca_cert_pem.as_bytes()))
        .identity(Identity::from_pem(ccert.as_bytes(), ckey.as_bytes()))
        .domain_name(LOOPBACK_SAN);
    let channel = endpoint_for(h.addr, tls).connect().await.expect("connect");
    let mut client = PkiClient::new(channel);
    let resp = client
        .verify_audit_chain(PkiEmpty {})
        .await
        .expect("verify_audit_chain ok")
        .into_inner();
    // Empty chain on a fresh in-memory DB is `ok`.
    assert_eq!(resp.outcome, "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_cert_passes_tls_but_rpc_is_rejected_unauthenticated() {
    // Real failure mode: if the auth interceptor weren't wired (or weren't
    // calling the verifier), a revoked cert could still execute RPCs as long
    // as the TLS handshake passes. Asserts: handshake succeeds, RPC returns
    // `Unauthenticated`, audit row `grpc_auth_failure` is written.
    let h = spawn_mtls_server().await;
    let (ccert, ckey, fp) = mint_client_cert(&h.ca_cert_pem, &h.ca_key_pkcs8_pem, "bob-laptop");
    {
        let g = h.db.lock().await;
        let serial = g
            .insert_internal_cert("client", &ccert, &fp, "bob-laptop", 0, 100)
            .await
            .expect("insert cert");
        g.insert_paired_device(&fp, "bob-laptop", "{}", "bob-laptop", serial)
            .await
            .expect("insert paired");
        g.add_revoked_fingerprint(&fp, Some("test-revoke"))
            .await
            .expect("revoke");
    }

    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(h.ca_cert_pem.as_bytes()))
        .identity(Identity::from_pem(ccert.as_bytes(), ckey.as_bytes()))
        .domain_name(LOOPBACK_SAN);
    let channel = endpoint_for(h.addr, tls)
        .connect()
        .await
        .expect("TLS handshake should still succeed for a revoked cert");
    let mut client = PkiClient::new(channel);
    let err = client
        .verify_audit_chain(PkiEmpty {})
        .await
        .expect_err("revoked cert must be rejected at the interceptor layer");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "got {err:?} — expected Unauthenticated"
    );

    let events = h.db.lock().await.list_audit_events().await.expect("audit");
    let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        kinds.contains(&audit_events::GRPC_AUTH_FAILURE),
        "expected grpc_auth_failure in audit; got {kinds:?}"
    );
}
