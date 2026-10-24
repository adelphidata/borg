//! `POST /v1/events/{subscription_id}`: where outside services send events.
//!
//! The handler stores the event and returns. It never runs the agent: the
//! row is the hand-off, and the dispatcher in `borgd` picks it up. That keeps
//! the response fast (providers time out in a few seconds) and means a burst
//! costs rows, not turns.
//!
//! What a sender sees is chosen for how providers retry. Every refusal that
//! a retry cannot fix (paused, paused itself, queue full) is a 200 with
//! `accepted: false`: Stripe, PagerDuty and Linear retry any other status for
//! days and then disable the endpoint. Only the per-minute cap answers 429,
//! because waiting a minute does fix it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
use cortex::subscriptions::{cap_payload, ingest, status, wake, Source};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::verify::{verify, AuthError, Secrets};

/// Largest body accepted, checked before any signature work.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Path the endpoint is mounted at, without the id.
pub const ENDPOINT_PREFIX: &str = "/v1/events/";

/// The endpoint's database wait. Short: past a few seconds the provider has
/// given up anyway, and a request queued behind a burst only adds to it.
pub const BUSY_TIMEOUT_MS: u64 = 2_000;

/// How long a resolved key is reused. A keychain or `exec` reference is
/// not re-run for every request of a burst.
const SECRET_TTL: Duration = Duration::from_secs(60);

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

/// A refusal a retry cannot fix: 200, so the provider does not retry.
fn not_accepted(state: &str, message: &str) -> Response {
    reply(
        StatusCode::OK,
        json!({ "ok": true, "accepted": false, "status": state, "note": message }),
    )
}

fn too_many(message: &str) -> Response {
    let mut resp = refuse(StatusCode::TOO_MANY_REQUESTS, message);
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    resp
}

/// Requests per subscription per minute, counted in memory before any
/// secret or database work. It admits twice the ledger's cap, so the
/// ledger stays the authority for normal traffic; past it, a burst costs a
/// map lookup per request instead of a keychain read and a write lock.
struct Gate {
    windows: Mutex<HashMap<String, Window>>,
}

struct Window {
    started: i64,
    count: u32,
    /// Refused here and not yet added to the subscription's counter.
    dropped: i64,
}

/// Distinct ids tracked at once; probes for unknown ids cannot grow it
/// without bound.
const GATE_MAX_IDS: usize = 4_096;

enum Admit {
    /// Go on; add `dropped` refusals to the counter once the id is known.
    Yes { dropped: i64 },
    /// Over the in-memory cap.
    No,
}

impl Gate {
    fn global() -> &'static Gate {
        static GATE: OnceLock<Gate> = OnceLock::new();
        GATE.get_or_init(|| Gate {
            windows: Mutex::new(HashMap::new()),
        })
    }

    fn admit(&self, id: &str, now: i64, per_minute: u32) -> Admit {
        let cap = per_minute.saturating_mul(2).max(10);
        let mut windows = match self.windows.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if windows.len() >= GATE_MAX_IDS && !windows.contains_key(id) {
            windows.retain(|_, w| now - w.started < 60);
            if windows.len() >= GATE_MAX_IDS {
                return Admit::Yes { dropped: 0 };
            }
        }
        let w = windows.entry(id.to_string()).or_insert(Window {
            started: now,
            count: 0,
            dropped: 0,
        });
        if now - w.started >= 60 {
            w.started = now;
            w.count = 0;
        }
        if w.count >= cap {
            w.dropped += 1;
            return Admit::No;
        }
        w.count += 1;
        Admit::Yes {
            dropped: std::mem::take(&mut w.dropped),
        }
    }

    /// Give back the slot of an authenticated request the filter rejected:
    /// a provider's unwanted events (pushes to other branches) must not use
    /// up the budget of the ones the subscription wants.
    fn refund(&self, id: &str) {
        let mut windows = match self.windows.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(w) = windows.get_mut(id) {
            w.count = w.count.saturating_sub(1);
        }
    }
}

async fn resolve_secret(json_ref: Option<String>) -> Result<Option<String>, ()> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    let Some(text) = json_ref else {
        return Ok(None);
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = match cache.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((at, value)) = guard.get(&text) {
            if at.elapsed() < SECRET_TTL {
                return Ok(Some(value.clone()));
            }
        }
    }
    let secret_ref: SecretRef = serde_json::from_str(&text).map_err(|_| ())?;
    // A keychain or exec reference may block; keep it off the reactor.
    let value = tokio::task::spawn_blocking(move || secret_ref.resolve())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() >= GATE_MAX_IDS {
        guard.clear();
    }
    guard.insert(text, (Instant::now(), value.clone()));
    Ok(Some(value))
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
    let now = chrono::Utc::now().timestamp();
    let Admit::Yes { dropped } = Gate::global().admit(&id, now, state.limits.per_minute) else {
        return too_many(
            "Too many events this minute. Borg keeps the first ones and drops the rest.",
        );
    };
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
    if let Err(e) = db.add_subscription_dropped(&sub.id, dropped).await {
        tracing::warn!(subscription = %sub.id, "subscription endpoint: count drops: {e:#}");
    }

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
        wake::report_seen(&sub.id);
        return reply(StatusCode::OK, json!({ "ok": true, "handshake": true }));
    }

    if sub.status == status::PAUSED {
        if let Err(e) = db.mark_subscription_seen(&sub.id, now).await {
            tracing::warn!(subscription = %sub.id, "subscription endpoint: mark seen: {e:#}");
        }
        wake::report_seen(&sub.id);
        return not_accepted(
            status::PAUSED,
            "This subscription is paused, so the event was not kept. Enable it in Borg.",
        );
    }
    if sub.status != status::ACTIVE {
        if let Err(e) = db.add_subscription_dropped(&sub.id, 1).await {
            tracing::warn!(subscription = %sub.id, "subscription endpoint: count drops: {e:#}");
        }
        return not_accepted(
            &sub.status,
            "This subscription paused itself after a burst, so the event was not kept. Resume it in Borg.",
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
            ingest::report_unreadable_filter(&db, &sub.id).await;
            false
        }
    };
    if !wanted {
        Gate::global().refund(&sub.id);
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
        received_at: now,
    };
    match db.insert_subscription_event(&event, state.limits).await {
        Ok(InsertOutcome::Inserted(event_id)) => {
            wake::report_seen(&sub.id);
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
        Ok(InsertOutcome::QueueFull(_)) => not_accepted(
            "queue_full",
            "Too many events are waiting. This one was noted in the history but will not run.",
        ),
        Ok(InsertOutcome::StormTripped { events_in_window }) => {
            wake::report_storm(&sub.id, events_in_window);
            not_accepted(
                status::STORM,
                "Too many events. This subscription paused itself; resume it in Borg.",
            )
        }
        Ok(InsertOutcome::NotAccepting) => not_accepted(
            "not_accepting",
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

    const TEST_LIMITS: IngestLimits = IngestLimits {
        per_minute: 5,
        storm_threshold: 8,
        max_pending: 100,
    };

    async fn setup_with(
        provider: &str,
        auth: &str,
        st: &str,
        filter: &str,
    ) -> (Router, Database, String, tempfile::TempDir) {
        setup_full(provider, auth, st, filter, TEST_LIMITS).await
    }

    async fn setup_full(
        provider: &str,
        auth: &str,
        st: &str,
        filter: &str,
        limits: IngestLimits,
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
            limits,
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
    async fn a_paused_subscription_keeps_nothing_and_asks_for_no_retry() {
        let (app, db, id, _tmp) = setup("custom", "bearer", "paused").await;
        assert_eq!(
            send(&app, post(&id, &[], "{}")).await.0,
            StatusCode::UNAUTHORIZED,
            "the state is told only after auth"
        );
        let (code, body) = send(&app, post(&id, &[BEARER], "{}")).await;
        assert_eq!(
            code,
            StatusCode::OK,
            "any other status makes providers retry for days"
        );
        assert_eq!(body["accepted"], false);
        assert_eq!(body["status"], "paused");
        let row = db.get_subscription(&id).await.unwrap().unwrap();
        assert!(
            row.last_event_at.is_some(),
            "the setup card can see the sender is wired up"
        );
        assert!(
            db.list_subscription_events(&id, 10)
                .await
                .unwrap()
                .is_empty(),
            "nothing is stored"
        );
    }

    #[tokio::test]
    async fn a_burst_gets_429_past_the_per_minute_cap() {
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
        // Five more reached the ledger and were counted there; the last ten
        // stopped at the in-memory gate, which hands its count over with the
        // next admitted request.
        assert_eq!(
            db.get_subscription(&id)
                .await
                .unwrap()
                .unwrap()
                .dropped_count,
            5
        );
    }

    #[tokio::test]
    async fn a_storm_pauses_the_subscription_once_and_counts_what_follows() {
        let limits = IngestLimits {
            per_minute: 50,
            storm_threshold: 8,
            max_pending: 100,
        };
        let (app, db, id, _tmp) = setup_full("custom", "bearer", "active", "{}", limits).await;
        let mut replies = Vec::new();
        for i in 0..12 {
            let body = format!(r#"{{"id":"s{i}"}}"#);
            replies.push(send(&app, post(&id, &[BEARER], &body)).await);
        }
        assert!(replies[..8].iter().all(|(c, _)| *c == StatusCode::ACCEPTED));
        for (code, body) in &replies[8..] {
            assert_eq!(*code, StatusCode::OK);
            assert_eq!(body["accepted"], false);
            assert_eq!(body["status"], "storm");
        }
        let row = db.get_subscription(&id).await.unwrap().unwrap();
        assert_eq!(row.status, "storm");
        assert!(
            row.dropped_count >= 3,
            "events after the storm are counted: {}",
            row.dropped_count
        );
        let notices: Vec<_> = wake::take_storm_notices()
            .into_iter()
            .filter(|n| n.subscription_id == id)
            .collect();
        assert_eq!(notices.len(), 1, "the user is told once");
    }

    #[test]
    fn the_gate_refuses_past_twice_the_cap_and_hands_the_count_back() {
        let gate = Gate {
            windows: Mutex::new(HashMap::new()),
        };
        for _ in 0..10 {
            assert!(matches!(gate.admit("s", 100, 5), Admit::Yes { dropped: 0 }));
        }
        for _ in 0..3 {
            assert!(matches!(gate.admit("s", 100, 5), Admit::No));
        }
        assert!(
            matches!(gate.admit("t", 100, 5), Admit::Yes { dropped: 0 }),
            "another subscription has its own window"
        );
        match gate.admit("s", 161, 5) {
            Admit::Yes { dropped } => assert_eq!(dropped, 3, "a new minute hands the drops back"),
            Admit::No => panic!("a new minute must admit again"),
        }
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
        let replayed = [
            ("x-github-event", "release"),
            ("x-github-delivery", "del-forged"),
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
        assert_eq!(
            send(&app, post(&id, &replayed, push)).await.0,
            StatusCode::OK,
            "a replay with rewritten unsigned headers is still a duplicate"
        );
        let events = db.list_subscription_events(&id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind.as_deref(), Some("push"));
        assert_eq!(
            events[0].dedup_key.as_deref(),
            Some(hex::encode(Sha256::digest(push.as_bytes())).as_str()),
            "GitHub is deduped on the signed body"
        );

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
    async fn an_unreadable_filter_stores_nothing_and_says_so() {
        let (app, db, id, _tmp) = setup("custom", "bearer", "active").await;
        // Saving validates the filter, so only an older build could have
        // stored this one.
        db.conn()
            .execute(
                "UPDATE subscriptions SET filter = ?2 WHERE id = ?1",
                libsql::params!(id.clone(), r#"{"severity":"error"}"#),
            )
            .await
            .unwrap();
        let (code, reply) = send(&app, post(&id, &[BEARER], r#"{"id":"e1"}"#)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(reply["filtered"], true);
        assert!(db
            .list_subscription_events(&id, 10)
            .await
            .unwrap()
            .is_empty());
        let row = db.get_subscription(&id).await.unwrap().unwrap();
        assert_eq!(row.last_error.as_deref(), Some(ingest::UNREADABLE_FILTER));
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
