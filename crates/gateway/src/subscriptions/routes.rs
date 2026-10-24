//! `POST /v1/events/{subscription_id}`: where outside services send events.
//!
//! The handler stores the event and returns. It never runs the agent: the
//! row is the hand-off, and the dispatcher in `borgd` picks it up. That keeps
//! the response fast (providers time out in a few seconds) and means a burst
//! costs rows, not turns.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use cortex::db::{IngestLimits, InsertOutcome, NewSubscriptionEvent, SubscriptionRow};
use cortex::security::secrets_resolve::SecretRef;
use cortex::subscriptions::filter::{EventView, Filter};
use cortex::subscriptions::presets::{self, AuthScheme};
use cortex::subscriptions::{cap_payload, status, wake, Source};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::verify::{verify, AuthError, Secrets};

/// Largest body accepted, checked before any signature work.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Path the endpoint is mounted at, without the id.
pub const ENDPOINT_PREFIX: &str = "/v1/events/";

/// State for the endpoint.
#[derive(Clone)]
pub struct EventsState {
    /// Opens a DB connection per request.
    pub db_open: crate::DbOpener,
    /// `subscriptions.enabled` when the router was built.
    pub enabled: bool,
    /// The ingest budget.
    pub limits: IngestLimits,
    /// Bytes of payload stored.
    pub payload_max_bytes: usize,
}

/// Build the router.
pub fn router(state: EventsState) -> Router {
    Router::new()
        .route("/v1/events/{id}", post(events_handler))
        .with_state(state)
}

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn refuse(status: StatusCode, message: &str) -> Response {
    reply(status, json!({ "ok": false, "error": message }))
}

fn too_many(message: &str) -> Response {
    let mut resp = refuse(StatusCode::TOO_MANY_REQUESTS, message);
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    resp
}

async fn resolve_secret(json_ref: Option<String>) -> Result<Option<String>, ()> {
    let Some(text) = json_ref else {
        return Ok(None);
    };
    let secret_ref: SecretRef = serde_json::from_str(&text).map_err(|_| ())?;
    // A keychain or exec reference may block; keep it off the reactor.
    tokio::task::spawn_blocking(move || secret_ref.resolve())
        .await
        .map_err(|_| ())?
        .map(Some)
        .map_err(|_| ())
}

async fn events_handler(
    State(state): State<EventsState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("The body is larger than {MAX_BODY_BYTES} bytes."),
        );
    }
    if !state.enabled {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "Subscriptions are turned off in Borg's settings.",
        );
    }
    let db = match (state.db_open)().await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("subscription endpoint: open db: {e:#}");
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "Borg is not ready. Try again.",
            );
        }
    };
    let sub = match db.get_subscription(&id).await {
        Ok(Some(sub)) if Source::parse(&sub.source).is_some_and(Source::uses_endpoint) => sub,
        Ok(_) => return refuse(StatusCode::NOT_FOUND, "No such subscription."),
        Err(e) => {
            tracing::warn!(subscription = %id, "subscription endpoint: lookup: {e:#}");
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "Borg is not ready. Try again.",
            );
        }
    };

    // Authenticate before saying anything about the subscription's state, so
    // an unauthenticated caller learns nothing beyond "it exists".
    if let Err(resp) = authenticate(&sub, &headers, &body).await {
        return resp;
    }

    let Some(preset) = presets::get(&sub.provider) else {
        return refuse(StatusCode::NOT_FOUND, "No such subscription.");
    };
    let payload = parse_body(&headers, &body);
    let header_fn = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let normalized = preset.normalize(&header_fn, &payload);
    // A handshake (GitHub's ping) is answered even while paused: new
    // subscriptions start paused, and the provider's setup check must pass.
    if normalized.handshake {
        return reply(StatusCode::OK, json!({ "ok": true, "handshake": true }));
    }

    if sub.status != status::ACTIVE {
        let message = if sub.status == status::STORM {
            "This subscription paused itself after a burst. Resume it in Borg."
        } else {
            "This subscription is paused. Enable it in Borg."
        };
        return reply(
            StatusCode::CONFLICT,
            json!({ "ok": false, "status": sub.status, "error": message }),
        );
    }

    // The filter sees the full payload, before the storage cap cuts it, and
    // an event it rejects costs nothing: no row, no burst budget.
    let text = preset.text(&payload);
    let view = EventView {
        kind: normalized.kind.as_deref(),
        summary: &normalized.summary,
        text: text.as_deref(),
        severity: preset.severity(&payload),
        payload: &payload,
    };
    let wanted = match Filter::parse(&sub.filter) {
        Some(filter) => filter.matches(&view),
        None => {
            tracing::warn!(subscription = %sub.id, "subscription filter is unreadable; skipping event");
            false
        }
    };
    if !wanted {
        return reply(StatusCode::OK, json!({ "ok": true, "filtered": true }));
    }

    let dedup = normalized
        .dedup_key
        .clone()
        .unwrap_or_else(|| hex::encode(Sha256::digest(&body)));
    let (stored, truncated) = cap_payload(&payload, state.payload_max_bytes);
    let event = NewSubscriptionEvent {
        subscription_id: &sub.id,
        kind: normalized.kind.as_deref(),
        dedup_key: Some(&dedup),
        summary: &normalized.summary,
        payload: &stored,
        payload_truncated: truncated,
        origin: None,
        received_at: chrono::Utc::now().timestamp(),
    };
    match db.insert_subscription_event(&event, state.limits).await {
        Ok(InsertOutcome::Inserted(event_id)) => {
            wake::poke();
            reply(
                StatusCode::ACCEPTED,
                json!({ "ok": true, "event_id": event_id }),
            )
        }
        Ok(InsertOutcome::Duplicate) => {
            reply(StatusCode::OK, json!({ "ok": true, "duplicate": true }))
        }
        Ok(InsertOutcome::RateLimited) => {
            too_many("Too many events this minute. Borg keeps the first ones and drops the rest.")
        }
        Ok(InsertOutcome::QueueFull(_)) => {
            too_many("Too many events are waiting. This one was recorded but will not run.")
        }
        Ok(InsertOutcome::StormTripped { events_in_window }) => {
            wake::report_storm(&sub.id, events_in_window);
            reply(
                StatusCode::CONFLICT,
                json!({
                    "ok": false,
                    "status": status::STORM,
                    "error": "Too many events. This subscription paused itself; resume it in Borg.",
                }),
            )
        }
        Ok(InsertOutcome::NotAccepting) => refuse(
            StatusCode::CONFLICT,
            "This subscription is not accepting events. Check it in Borg.",
        ),
        Err(e) => {
            tracing::warn!(subscription = %sub.id, "subscription endpoint: insert: {e:#}");
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "Borg could not store the event. Try again.",
            )
        }
    }
}

#[allow(clippy::result_large_err)]
async fn authenticate(
    sub: &SubscriptionRow,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), Response> {
    let unavailable = || {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "Borg could not read this subscription's key.",
        )
    };
    let key = match resolve_secret(sub.secret_ref.clone()).await {
        Ok(Some(k)) => k,
        Ok(None) | Err(()) => {
            tracing::warn!(subscription = %sub.id, "subscription endpoint: key unreadable");
            return Err(unavailable());
        }
    };
    let signing = match resolve_secret(sub.signing_secret_ref.clone()).await {
        Ok(s) => s,
        Err(()) => {
            tracing::warn!(subscription = %sub.id, "subscription endpoint: signing secret unreadable");
            return Err(unavailable());
        }
    };
    let scheme = AuthScheme::parse(&sub.auth_scheme).unwrap_or(AuthScheme::Bearer);
    let now = chrono::Utc::now().timestamp();
    let secrets = Secrets {
        key: &key,
        signing_secret: signing.as_deref(),
    };
    verify(scheme, headers, body, secrets, now).map_err(|e| {
        tracing::info!(subscription = %sub.id, "subscription endpoint refused: {e}");
        let code = match e {
            AuthError::Malformed(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::UNAUTHORIZED,
        };
        refuse(code, &e.to_string())
    })
}

/// JSON bodies are kept as JSON; anything else (a form post, plain text) is
/// kept as text so the agent can still read it.
fn parse_body(headers: &HeaderMap, body: &[u8]) -> serde_json::Value {
    if body.is_empty() {
        return json!({});
    }
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        return v;
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    json!({ "raw": String::from_utf8_lossy(body), "content_type": content_type })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use cortex::db::{Database, NewSubscription};
    use std::sync::Arc;
    use tower::ServiceExt;

    const KEY: &str = "bsk_test_key";

    async fn setup(
        provider: &str,
        auth: &str,
        st: &str,
    ) -> (Router, Database, String, tempfile::TempDir) {
        setup_with(provider, auth, st, "{}").await
    }

    async fn setup_with(
        provider: &str,
        auth: &str,
        st: &str,
        filter: &str,
    ) -> (Router, Database, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let for_factory = path.clone();
        let opener: crate::DbOpener = Arc::new(move || {
            let p = for_factory.clone();
            Box::pin(async move { Database::open_at(&p).await })
        });
        let db = (opener)().await.expect("open setup db");
        let secret = serde_json::to_string(&SecretRef::Literal { value: KEY.into() }).unwrap();
        let sub = db
            .create_subscription(&NewSubscription {
                name: "t",
                source: "webhook",
                provider,
                auth_scheme: auth,
                secret_ref: Some(&secret),
                filter,
                instruction: "x",
                delivery_channel: None,
                delivery_target: None,
                status: st,
                rate_limit_per_hour: 60,
                timeout_secs: 60,
            })
            .await
            .unwrap();
        let state = EventsState {
            db_open: opener,
            enabled: true,
            limits: IngestLimits {
                per_minute: 5,
                storm_threshold: 8,
                max_pending: 100,
            },
            payload_max_bytes: 64 * 1024,
        };
        (router(state), db, sub.id, tmp)
    }

    fn post(id: &str, headers: &[(&str, &str)], body: &str) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(format!("/v1/events/{id}"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(json!(null)),
        )
    }

    const BEARER: (&str, &str) = ("authorization", "Bearer bsk_test_key");

    #[tokio::test]
    async fn accepted_events_are_stored_before_the_202() {
        let (app, db, id, _tmp) = setup("custom", "bearer", "active").await;
        let (code, body) = send(
            &app,
            post(&id, &[BEARER], r#"{"id":"e1","title":"Deploy failed"}"#),
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        let event_id = body["event_id"].as_str().unwrap().to_string();
        let row = db.get_subscription_event(&event_id).await.unwrap().unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.summary, "Deploy failed");
        assert_eq!(row.dedup_key.as_deref(), Some("e1"));

        let (code, body) = send(&app, post(&id, &[BEARER], r#"{"id":"e1","title":"again"}"#)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["duplicate"], true);
    }

    #[tokio::test]
    async fn refusals_have_the_right_codes() {
        let (app, _db, id, _tmp) = setup("custom", "bearer", "active").await;
        assert_eq!(
            send(&app, post("sub_missing", &[BEARER], "{}")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, post(&id, &[], "{}")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&app, post(&id, &[("authorization", "Bearer wrong")], "{}"))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(
            send(&app, post(&id, &[BEARER], &big)).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn a_paused_subscription_answers_409_only_after_auth() {
        let (app, _db, id, _tmp) = setup("custom", "bearer", "paused").await;
        assert_eq!(
            send(&app, post(&id, &[], "{}")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (code, body) = send(&app, post(&id, &[BEARER], "{}")).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(body["status"], "paused");
    }

    #[tokio::test]
    async fn a_burst_gets_429_then_the_breaker_pauses_the_subscription() {
        let (app, db, id, _tmp) = setup("custom", "bearer", "active").await;
        let mut codes = Vec::new();
        for i in 0..20 {
            let body = format!(r#"{{"id":"b{i}"}}"#);
            codes.push(send(&app, post(&id, &[BEARER], &body)).await.0);
        }
        assert_eq!(
            codes.iter().filter(|c| **c == StatusCode::ACCEPTED).count(),
            5
        );
        assert!(codes[5..]
            .iter()
            .all(|c| *c == StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(
            db.list_subscription_events(&id, 100).await.unwrap().len(),
            5
        );
        assert_eq!(
            db.get_subscription(&id)
                .await
                .unwrap()
                .unwrap()
                .dropped_count,
            15
        );
    }

    #[tokio::test]
    async fn github_ping_is_answered_without_storing_and_a_signed_push_is_stored() {
        use hmac::{Hmac, KeyInit, Mac};
        // New subscriptions start paused; GitHub's setup ping must still pass.
        let (app, db, id, _tmp) = setup("github", "github", "paused").await;
        let sign = |body: &str| {
            let mut mac = Hmac::<Sha256>::new_from_slice(KEY.as_bytes()).unwrap();
            mac.update(body.as_bytes());
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        };
        let ping = r#"{"zen":"Keep it logically awesome."}"#;
        let sig = sign(ping);
        let (code, body) = send(
            &app,
            post(
                &id,
                &[("x-github-event", "ping"), ("x-hub-signature-256", &sig)],
                ping,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["handshake"], true);
        assert!(db
            .list_subscription_events(&id, 10)
            .await
            .unwrap()
            .is_empty());
        let unsigned_ping = [("x-github-event", "ping")];
        assert_eq!(
            send(&app, post(&id, &unsigned_ping, ping)).await.0,
            StatusCode::UNAUTHORIZED,
            "the handshake is still authenticated"
        );

        db.set_subscription_status(&id, status::ACTIVE, None)
            .await
            .unwrap();
        let push = r#"{"ref":"refs/heads/main","commits":[{}],"repository":{"full_name":"octo/app"},"sender":{"login":"mona"}}"#;
        let sig = sign(push);
        let headers = [
            ("x-github-event", "push"),
            ("x-github-delivery", "del-1"),
            ("x-hub-signature-256", sig.as_str()),
        ];
        assert_eq!(
            send(&app, post(&id, &headers, push)).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            send(&app, post(&id, &headers, push)).await.0,
            StatusCode::OK,
            "redelivery is a duplicate"
        );
        let events = db.list_subscription_events(&id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind.as_deref(), Some("push"));
        assert_eq!(events[0].dedup_key.as_deref(), Some("del-1"));

        let forged = [
            ("x-github-event", "push"),
            ("x-hub-signature-256", "sha256=00"),
        ];
        assert_eq!(
            send(&app, post(&id, &forged, push)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn events_the_filter_rejects_cost_nothing() {
        let (app, db, id, _tmp) =
            setup_with("custom", "bearer", "active", r#"{"severity_min":"error"}"#).await;
        for i in 0..10 {
            let body = format!(r#"{{"id":"w{i}","severity":"warning"}}"#);
            let (code, reply) = send(&app, post(&id, &[BEARER], &body)).await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(reply["filtered"], true);
        }
        assert!(db
            .list_subscription_events(&id, 100)
            .await
            .unwrap()
            .is_empty());
        let (code, _) = send(
            &app,
            post(&id, &[BEARER], r#"{"id":"e1","severity":"critical"}"#),
        )
        .await;
        assert_eq!(
            code,
            StatusCode::ACCEPTED,
            "rejected events used none of the per-minute budget"
        );
    }

    #[tokio::test]
    async fn the_filter_reads_the_payload_beyond_the_storage_cap() {
        let (app, db, id, _tmp) = setup_with(
            "custom",
            "bearer",
            "active",
            r#"{"field_equals":[{"path":"tail.env","value":"prod"}]}"#,
        )
        .await;
        let body = json!({"id": "big", "pad": "x".repeat(100 * 1024), "tail": {"env": "prod"}})
            .to_string();
        assert_eq!(
            send(&app, post(&id, &[BEARER], &body)).await.0,
            StatusCode::ACCEPTED
        );
        let row = &db.list_subscription_events(&id, 10).await.unwrap()[0];
        assert!(row.payload_truncated, "the stored copy is cut");
    }

    #[tokio::test]
    async fn an_unreadable_filter_stores_nothing() {
        let (app, db, id, _tmp) =
            setup_with("custom", "bearer", "active", r#"{"severity":"error"}"#).await;
        let (code, reply) = send(&app, post(&id, &[BEARER], r#"{"id":"e1"}"#)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(reply["filtered"], true);
        assert!(db
            .list_subscription_events(&id, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn non_json_bodies_are_kept_as_text() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let v = parse_body(&h, b"hello there");
        assert_eq!(v["raw"], "hello there");
        assert_eq!(v["content_type"], "text/plain");
        assert_eq!(parse_body(&h, b""), json!({}));
    }
}
