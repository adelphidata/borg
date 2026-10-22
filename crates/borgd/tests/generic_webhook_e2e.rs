//! End-to-end smoke for the generic-webhook surface mounted by
//! `gateway::GatewayServer`.
//!
//! Real failure modes guarded:
//! 1. A regression that drops the
//!    `.merge(build_generic_webhook_router(...))` line from
//!    `crates/gateway/src/server.rs`. Asserted via "POST /v1/webhook/{name}
//!    is reachable, not 404 from a different handler."
//! 2. Signature/replay tampering that the inline unit tests already
//!    cover at the route layer — re-asserted here through the live
//!    HTTP path so the assembled middleware stack (size limits,
//!    state) doesn't accidentally let bad signatures through.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use cortex::agent::AgentEvent;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

#[path = "ingress_e2e_common.rs"]
mod common;

/// Set up a webhook definition row and return the plaintext secret
/// the user would see once.
async fn create_webhook(name: &str, sender_path: &str) -> String {
    let db = cortex::db::Database::open().await.expect("open db");
    let created =
        gateway::generic_webhook::definitions::create(&db, name, sender_path, None, "$.body")
            .await
            .expect("create webhook");
    created.secret
}

/// Sign `{ts}.{body}` with the same scheme `extract::verify_signature`
/// uses on the server side.
///
/// The verifier reads `secret_hash` (binary 32 bytes) from the DB row
/// and hex-encodes it to get the HMAC key. To produce a matching
/// signature client-side, hash the plaintext secret with SHA-256, then
/// hex-encode the digest — that's the same key both sides derive.
fn sign(secret_plaintext: &str, ts: i64, body: &[u8]) -> String {
    use sha2::Digest;
    let digest = Sha256::digest(secret_plaintext.as_bytes());
    let key_hex = cortex::util::hex::hex_encode(&digest);
    let mut mac = Hmac::<Sha256>::new_from_slice(key_hex.as_bytes()).expect("hmac");
    mac.update(format!("{ts}.").as_bytes());
    mac.update(body);
    let sig = cortex::util::hex::hex_encode(&mac.finalize().into_bytes());
    format!("v1={sig}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_webhook_signed_post_dispatches_to_agent() {
    let harness = common::boot_gateway(true).await;
    let secret = create_webhook("e2e-hook", "$.user").await;
    // Webhook session key = `<extracted-sender>:`; the JSONPath
    // `$.user` returns "alice" from the payload below.
    common::register_stub_for_session("alice:", vec![AgentEvent::TurnComplete]);
    let body = serde_json::json!({"user": "alice", "body": "hello"})
        .to_string()
        .into_bytes();
    let ts = chrono::Utc::now().timestamp();
    let sig = sign(&secret, ts, &body);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    // 1. Unknown webhook → 404 (anti-enumeration: the route is reached
    //    and returns the webhook's documented 404, not the gateway's
    //    "Unknown channel" 404 from the legacy `/webhook/{name}` path
    //    — those are different bodies).
    let resp = client
        .post(harness.url("/v1/webhook/does-not-exist"))
        .header("x-borg-signature", &sig)
        .header("x-borg-timestamp", ts.to_string())
        .body(body.clone())
        .send()
        .await
        .expect("unknown");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let unknown_body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        unknown_body["error"]["message"], "no such webhook",
        "must be the generic_webhook 404 body, not the legacy `/webhook/{{name}}` 'Unknown channel' (proves the right router answered)"
    );

    // 2. Valid signed POST → 202 Accepted (route mounted, signature
    //    verified, sink invoked).
    let resp = client
        .post(harness.url("/v1/webhook/e2e-hook"))
        .header("x-borg-signature", &sig)
        .header("x-borg-timestamp", ts.to_string())
        .body(body.clone())
        .send()
        .await
        .expect("send signed");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::ACCEPTED,
        "valid signed payload must be accepted"
    );

    // 3. Replay outside the window → 401.
    let stale_ts = ts - 1_000_000;
    let stale_sig = sign(&secret, stale_ts, &body);
    let resp = client
        .post(harness.url("/v1/webhook/e2e-hook"))
        .header("x-borg-signature", stale_sig)
        .header("x-borg-timestamp", stale_ts.to_string())
        .body(body.clone())
        .send()
        .await
        .expect("send stale");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 4. Wrong signature → 401.
    let resp = client
        .post(harness.url("/v1/webhook/e2e-hook"))
        .header("x-borg-signature", "v1=deadbeef")
        .header("x-borg-timestamp", ts.to_string())
        .body(body)
        .send()
        .await
        .expect("send bad-sig");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    harness.shutdown().await;
}
