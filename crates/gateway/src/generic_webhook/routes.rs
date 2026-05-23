//! `POST /v1/webhook/{name}` route.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use cortex::ingress::{
    AdmittedIngress, IngressGate, IngressMessage, IngressSource, MessageBody, PrincipalId,
};
use cortex::util::hex::hex_encode;

use super::definitions;
use super::extract::{extract_string, extract_string_opt, verify_signature, HmacError};

/// Maximum body size accepted by the webhook surface — 1 MiB.
/// Bodies above this are 413'd before HMAC verification, which is a
/// load-shed defense (HMAC over very large bodies is CPU-bound).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Shared state for the generic-webhook routes.
#[derive(Clone)]
pub struct WebhookState {
    /// Open a fresh DB connection per request.
    pub db_open: crate::DbOpener,
    /// Pre-dispatch gate (rate-limit + sanitize + audit).
    pub gate: Arc<IngressGate>,
    /// Sink that receives admitted ingresses for downstream dispatch.
    /// Tests inject a recording sink; production wires it to the
    /// agent loop (mirrors the OpenAI-compat dispatcher pattern).
    pub sink: Arc<dyn AdmitSink>,
}

/// Hands an admitted webhook ingress to the agent loop. Trait so
/// tests can record what was admitted without spinning a real agent.
#[async_trait::async_trait]
pub trait AdmitSink: Send + Sync + 'static {
    /// Receive one admitted ingress. Errors are logged; the route
    /// returns 202 Accepted regardless so upstream services don't
    /// retry on transient agent-side issues.
    async fn handle(&self, admitted: AdmittedIngress);
}

/// Build the router for the generic webhook surface.
pub fn router(state: WebhookState) -> Router {
    Router::new()
        .route("/v1/webhook/{name}", post(webhook_handler))
        .with_state(state)
}

async fn webhook_handler(
    State(state): State<WebhookState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!(
                "body exceeds {MAX_BODY_BYTES}-byte limit ({} bytes received)",
                body.len()
            ),
        );
    }

    let db = match (state.db_open)().await {
        Ok(d) => d,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "backend unavailable"),
    };

    let definition = match definitions::lookup(&db, &name).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Anti-enumeration: missing-vs-revoked are
            // indistinguishable in the response. A successful POST
            // to `/v1/webhook/{name}` for an unknown `name` returns
            // the same 404 as one whose secret has been revoked.
            return error_response(StatusCode::NOT_FOUND, "no such webhook");
        }
        Err(_) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "webhook lookup failed");
        }
    };

    // The stored `secret_hash` IS the HMAC key both sides use —
    // SHA-256 of the random plaintext shown to the user at create
    // time. Equivalent to a single-iteration KDF over a 32-byte
    // CSPRNG secret. DB read implies sig-forge ability, same threat
    // profile as the existing `credentials` table; OS-keychain wrapping
    // for this column is a follow-up.
    let secret_for_hmac = hex_encode(&definition.secret_hash);
    let now = chrono::Utc::now().timestamp();
    let sig_header = headers
        .get("x-borg-signature")
        .and_then(|v| v.to_str().ok());
    let ts_header = headers
        .get("x-borg-timestamp")
        .and_then(|v| v.to_str().ok());

    if let Err(e) = verify_signature(&secret_for_hmac, &body, sig_header, ts_header, now) {
        let status = match e {
            HmacError::MissingHeader(_) | HmacError::MalformedHeader(_) => StatusCode::BAD_REQUEST,
            HmacError::Replay { .. } | HmacError::Mismatch | HmacError::EmptySecret => {
                StatusCode::UNAUTHORIZED
            }
        };
        return error_response(status, &e.to_string());
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("body is not JSON: {e}"));
        }
    };

    let sender_id = match extract_string(&payload, &definition.sender_path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            return error_response(StatusCode::BAD_REQUEST, "extracted sender_id is empty");
        }
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let thread_id = definition
        .thread_path
        .as_deref()
        .and_then(|p| extract_string_opt(&payload, p))
        .filter(|s| !s.is_empty());
    let body_text = match extract_string(&payload, &definition.body_path) {
        Ok(s) => s,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let message = IngressMessage {
        source: IngressSource::Webhook(definition.name.clone()),
        principal: PrincipalId::new(format!("webhook:{}", definition.name)),
        sender_id,
        thread_id,
        body: MessageBody::text(body_text),
        meta: payload,
        received_at: now,
    };

    let admitted = match state.gate.admit(message) {
        Ok(a) => a,
        Err(e) => {
            let status = match e {
                cortex::ingress::IngressReject::EmptyBody => StatusCode::BAD_REQUEST,
                cortex::ingress::IngressReject::RateLimited { .. }
                | cortex::ingress::IngressReject::PrincipalCapacityExhausted => {
                    StatusCode::TOO_MANY_REQUESTS
                }
            };
            return error_response(status, &e.to_string());
        }
    };

    state.sink.handle(admitted).await;

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "accepted"})),
    )
        .into_response()
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({"error": {"message": msg}}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use cortex::db::Database;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Records the most recent admitted ingress so tests can assert
    /// the gate passed expected fields through to the agent layer.
    #[derive(Default)]
    struct RecordingSink {
        admitted: Mutex<Vec<AdmittedIngress>>,
    }

    #[async_trait::async_trait]
    impl AdmitSink for RecordingSink {
        async fn handle(&self, admitted: AdmittedIngress) {
            self.admitted
                .lock()
                .expect("test mutex unpoisoned")
                .push(admitted);
        }
    }

    async fn make_state() -> (
        WebhookState,
        Arc<RecordingSink>,
        super::super::definitions::CreatedWebhook,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let path_for_factory = path.clone();
        let db_open: crate::DbOpener = Arc::new(move || {
            let p = path_for_factory.clone();
            Box::pin(async move { Database::open_at(&p).await })
        });

        let setup_db = (db_open)().await.expect("open setup db");
        let created = super::super::definitions::create(
            &setup_db,
            "alerts",
            "$.user.id",
            Some("$.thread_id"),
            "$.text",
        )
        .await
        .expect("create webhook");

        let sink = Arc::new(RecordingSink::default());
        let gate = Arc::new(IngressGate::with_noop_audit(
            100,
            std::time::Duration::from_secs(1),
        ));
        let state = WebhookState {
            db_open,
            gate,
            sink: Arc::clone(&sink) as Arc<dyn AdmitSink>,
        };
        (state, sink, created, tmp)
    }

    /// Sign a body with the same hashed-secret-as-key model the
    /// production code uses. Upstream documentation will tell users
    /// this is what to compute; the CLI prints the hex-hash secret
    /// at creation time.
    fn sign(secret_hex: &str, ts: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret_hex.as_bytes()).expect("hmac");
        mac.update(format!("{ts}.").as_bytes());
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let hex = hex_encode(&bytes);
        format!("v1={hex}")
    }

    fn secret_hex(created: &super::super::definitions::CreatedWebhook) -> String {
        super::hex_encode(&created.row.secret_hash)
    }

    fn req(name: &str, sig: Option<&str>, ts: Option<&str>, body: &str) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(format!("/v1/webhook/{name}"))
            .header("content-type", "application/json");
        if let Some(s) = sig {
            b = b.header("x-borg-signature", s);
        }
        if let Some(t) = ts {
            b = b.header("x-borg-timestamp", t);
        }
        b.body(Body::from(body.to_string())).expect("build")
    }

    #[tokio::test]
    async fn valid_signed_payload_admits_and_dispatches() {
        // Round-trip happy path: a signed payload extracts the
        // configured fields, passes the gate, and reaches the sink
        // with the right session key + source. Regression here means
        // upstream events silently drop on the floor.
        let (state, sink, created, _tmp) = make_state().await;
        let secret = secret_hex(&created);
        let body = r#"{"user":{"id":"u-42"},"text":"deploy started","thread_id":"t-7"}"#;
        let now = chrono::Utc::now().timestamp();
        let sig = sign(&secret, now, body.as_bytes());

        let resp = router(state)
            .oneshot(req("alerts", Some(&sig), Some(&now.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let recorded = sink.admitted.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        let a = &recorded[0];
        assert!(matches!(&a.message.source, IngressSource::Webhook(n) if n == "alerts"));
        assert_eq!(a.message.sender_id, "u-42");
        assert_eq!(a.message.thread_id.as_deref(), Some("t-7"));
        assert_eq!(a.message.body.text, "deploy started");
        assert_eq!(a.session_key, "u-42:t-7");
    }

    #[tokio::test]
    async fn missing_signature_returns_400() {
        // Distinct from 401 (presented but bad): a missing header is
        // a client-misconfiguration error.
        let (state, _sink, _created, _tmp) = make_state().await;
        let body = r#"{"user":{"id":"u"},"text":"x"}"#;
        let now = chrono::Utc::now().timestamp();

        let resp = router(state)
            .oneshot(req("alerts", None, Some(&now.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn bad_signature_returns_401() {
        // Forged sig must hit 401, not 4xx-other and not 200.
        let (state, sink, _created, _tmp) = make_state().await;
        let body = r#"{"user":{"id":"u"},"text":"x"}"#;
        let now = chrono::Utc::now().timestamp();
        let bad = "v1=00000000000000000000000000000000000000000000000000000000000000ff";

        let resp = router(state)
            .oneshot(req("alerts", Some(bad), Some(&now.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // And the sink stays empty — no agent dispatch on auth fail.
        assert!(sink.admitted.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn replay_outside_window_returns_401() {
        // Stale timestamp: 6 minutes ago. Even with a perfectly
        // valid HMAC over `{stale_ts}.{body}`, the verifier rejects.
        let (state, sink, created, _tmp) = make_state().await;
        let secret = secret_hex(&created);
        let body = r#"{"user":{"id":"u"},"text":"x"}"#;
        let stale = chrono::Utc::now().timestamp() - 360;
        let sig = sign(&secret, stale, body.as_bytes());

        let resp = router(state)
            .oneshot(req("alerts", Some(&sig), Some(&stale.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(sink.admitted.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn unknown_webhook_name_returns_404() {
        // Anti-enumeration: returns 404 before HMAC verify, same as
        // a revoked webhook would.
        let (state, _sink, _created, _tmp) = make_state().await;
        let body = "{}";
        let now = chrono::Utc::now().timestamp();
        let resp = router(state)
            .oneshot(req(
                "does-not-exist",
                Some("v1=00"),
                Some(&now.to_string()),
                body,
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_required_field_returns_400() {
        // sender_id missing → 400 with the path mentioned. Helps
        // operators debug their JSONPath config without exposing the
        // entire payload back to the upstream caller.
        let (state, sink, created, _tmp) = make_state().await;
        let secret = secret_hex(&created);
        let body = r#"{"text":"hello"}"#; // no `user.id`
        let now = chrono::Utc::now().timestamp();
        let sig = sign(&secret, now, body.as_bytes());

        let resp = router(state)
            .oneshot(req("alerts", Some(&sig), Some(&now.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(sink.admitted.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn omitted_thread_path_is_optional() {
        // thread_id is configured at `$.thread_id` but the payload
        // doesn't include it — that's allowed for non-threaded
        // sources. The session key collapses to `sender:`.
        let (state, sink, created, _tmp) = make_state().await;
        let secret = secret_hex(&created);
        let body = r#"{"user":{"id":"u-1"},"text":"x"}"#;
        let now = chrono::Utc::now().timestamp();
        let sig = sign(&secret, now, body.as_bytes());

        let resp = router(state)
            .oneshot(req("alerts", Some(&sig), Some(&now.to_string()), body))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let recorded = sink.admitted.lock().expect("lock");
        assert_eq!(recorded[0].message.thread_id, None);
        assert_eq!(recorded[0].session_key, "u-1:");
    }

    #[tokio::test]
    async fn body_too_large_returns_413() {
        // Abuse defense: a 2 MiB payload should never reach the
        // (CPU-bound) HMAC verifier. The size cap is checked first.
        let (state, _sink, _created, _tmp) = make_state().await;
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let resp = router(state)
            .oneshot(req("alerts", Some("v1=00"), Some("0"), &big))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
