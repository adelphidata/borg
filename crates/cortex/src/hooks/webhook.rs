//! Outbound signed webhook deliveries for lifecycle events.
//!
//! [`WebhookHook`] is a second [`Hook`] implementation alongside
//! [`super::ScriptHook`]: endpoints configured under `webhooks.endpoints`
//! (DB settings table, see [`crate::config::webhooks`]) receive an HTTP POST
//! for each subscribed lifecycle event.
//!
//! Delivery is fire-and-forget and strictly best-effort:
//! - Events are pushed onto a bounded in-memory queue (capacity
//!   [`QUEUE_CAPACITY`]) drained by a single worker task. A dead or slow
//!   endpoint can never stall a tool call or turn.
//! - When the queue is full the *oldest* pending delivery is dropped with a
//!   warning; enqueueing never blocks.
//! - Failures (connect errors, non-2xx) are logged with endpoint + status.
//!   A 5xx or connect error is retried exactly once; 4xx is never retried.
//! - Redirects are never followed — a 3xx would let reqwest replay the signed
//!   POST as a body-less GET. A 3xx response is logged as an endpoint
//!   misconfiguration and counts as failure.
//! - On `SessionEnd` the hook waits up to [`SHUTDOWN_FLUSH`] for the queue to
//!   drain so a delivery fired during shutdown gets a chance to go out.
//!   Anything still pending after that is lost; there is no durable spool.
//!
//! Each delivery body is JSON mirroring the script-hook stdin payload
//! (`event`, `session_id`, `turn`, `tool`) plus `timestamp` and a
//! `delivery_id` that also rides in the `X-Borg-Delivery` header. The body is
//! passed through the secret-redaction pass before signing, and endpoints
//! with a configured secret get `X-Borg-Signature-256: sha256=<hex>` — a plain
//! HMAC-SHA256 over the exact body bytes sent, matching the widely-implemented
//! `X-Hub-Signature-256` convention. Receivers are third-party code we do not
//! control, so this MAC must stay a bare `HMAC(secret, body)`: it deliberately
//! does not go through `hmac_chain::compute_hmac`, whose length-prefix framing
//! no standard verifier reproduces.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::config::webhooks::{validate_webhook_url, WebhooksConfig};
use crate::hooks::script::{event_name_for_point, parse_event_name, EVENT_NAMES};
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

/// Maximum queued deliveries; beyond this the oldest is dropped.
const QUEUE_CAPACITY: usize = 256;
/// Per-attempt HTTP timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between the first attempt and the single retry.
const RETRY_DELAY: Duration = Duration::from_millis(250);
/// Bounded flush window for deliveries pending when `SessionEnd` fires.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(5);

/// A validated endpoint with its resolved hook-point subscription.
#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    secret: Option<String>,
    points: Vec<HookPoint>,
}

/// One pending HTTP delivery. The body is final (redacted, contains
/// `delivery_id`); the signature is computed at send time over these exact
/// bytes.
#[derive(Debug)]
struct Delivery {
    url: String,
    secret: Option<String>,
    delivery_id: String,
    body: String,
}

/// Bounded drop-oldest delivery queue shared between the hook and its worker.
struct DeliveryQueue {
    inner: Mutex<VecDeque<Delivery>>,
    /// Queued + in-flight deliveries; drained-flush waits on this reaching 0.
    pending: AtomicUsize,
    /// Wakes the worker when work arrives or the hook is dropped.
    notify: Notify,
    /// Set when the owning `WebhookHook` is dropped so the worker can exit.
    closed: AtomicBool,
    capacity: usize,
}

impl DeliveryQueue {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            pending: AtomicUsize::new(0),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            capacity,
        }
    }

    /// Enqueue a delivery. Never blocks: at capacity the oldest pending
    /// delivery is dropped (with a warning) to make room.
    fn push(&self, delivery: Delivery) {
        {
            let mut q = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if q.len() >= self.capacity {
                if let Some(dropped) = q.pop_front() {
                    self.pending.fetch_sub(1, Ordering::SeqCst);
                    tracing::warn!(
                        endpoint = %dropped.url,
                        delivery_id = %dropped.delivery_id,
                        "webhook queue full ({}); dropping oldest delivery",
                        self.capacity
                    );
                }
            }
            self.pending.fetch_add(1, Ordering::SeqCst);
            q.push_back(delivery);
        }
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<Delivery> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

/// Lifecycle hook that POSTs signed JSON deliveries to configured endpoints.
pub struct WebhookHook {
    points: Vec<HookPoint>,
    endpoints: Vec<Endpoint>,
    queue: Arc<DeliveryQueue>,
}

impl WebhookHook {
    /// Build from config, spawning the single delivery worker. Returns `None`
    /// when no usable endpoints are configured (or the HTTP client cannot be
    /// built — logged, never fatal: hooks can never break the agent).
    pub fn from_config(config: &WebhooksConfig) -> Option<Self> {
        Self::with_capacity(config, QUEUE_CAPACITY)
    }

    fn with_capacity(config: &WebhooksConfig, capacity: usize) -> Option<Self> {
        let mut endpoints = Vec::new();
        for ep in &config.endpoints {
            // Deserialization already enforces the URL policy; re-check
            // defensively in case the config was constructed in code.
            if let Err(e) = validate_webhook_url(&ep.url) {
                tracing::warn!(endpoint = %ep.url, "webhook endpoint rejected: {e}");
                continue;
            }
            let points = resolve_points(&ep.url, &ep.events);
            if points.is_empty() {
                tracing::warn!(
                    endpoint = %ep.url,
                    "webhook endpoint subscribes to no known events; skipping"
                );
                continue;
            }
            if ep.secret.is_none() {
                tracing::warn!(
                    endpoint = %ep.url,
                    "webhook endpoint has no secret; deliveries will be UNSIGNED"
                );
            }
            endpoints.push(Endpoint {
                url: ep.url.clone(),
                secret: ep.secret.clone(),
                points,
            });
        }
        if endpoints.is_empty() {
            return None;
        }

        // Redirects disabled: following a 3xx would resend the signed POST as
        // a body-less GET. A 3xx response is treated as a failure instead.
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("webhooks disabled: failed to build HTTP client: {e}");
                return None;
            }
        };

        let mut points: Vec<HookPoint> = Vec::new();
        for ep in &endpoints {
            for p in &ep.points {
                if !points.contains(p) {
                    points.push(*p);
                }
            }
        }

        let queue = Arc::new(DeliveryQueue::new(capacity));
        tokio::spawn(delivery_worker(queue.clone(), client));

        Some(Self {
            points,
            endpoints,
            queue,
        })
    }

    fn build_body(&self, ctx: &HookContext, delivery_id: &str) -> String {
        let tool = match &ctx.data {
            HookData::ToolCall { name, .. } => {
                Some(serde_json::json!({ "name": name, "is_error": false }))
            }
            HookData::ToolResult { name, is_error, .. } => {
                Some(serde_json::json!({ "name": name, "is_error": *is_error }))
            }
            _ => None,
        };
        let body = serde_json::json!({
            "event": event_name_for_point(ctx.point),
            "delivery_id": delivery_id,
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "session_id": ctx.session_id,
            "turn": ctx.turn_count,
            "tool": tool,
        })
        .to_string();
        // Payloads can carry tool metadata; run the standard secret-redaction
        // pass over the serialized body. The redacted string is the exact
        // byte sequence that gets signed and sent.
        crate::security::secrets::redact_secrets(&body)
    }
}

#[async_trait]
impl Hook for WebhookHook {
    fn name(&self) -> &str {
        "webhooks"
    }

    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        for ep in &self.endpoints {
            if !ep.points.contains(&ctx.point) {
                continue;
            }
            let delivery_id = uuid::Uuid::new_v4().to_string();
            let body = self.build_body(ctx, &delivery_id);
            self.queue.push(Delivery {
                url: ep.url.clone(),
                secret: ep.secret.clone(),
                delivery_id,
                body,
            });
        }

        // SessionEnd usually fires during shutdown; give in-flight deliveries
        // a bounded window instead of racing process exit. Still best-effort:
        // whatever is left after the window is dropped.
        if ctx.point == HookPoint::SessionEnd {
            let deadline = tokio::time::Instant::now() + SHUTDOWN_FLUSH;
            while self.queue.pending() > 0 && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let left = self.queue.pending();
            if left > 0 {
                tracing::warn!(
                    pending = left,
                    "webhook flush window elapsed with deliveries still pending"
                );
            }
        }

        HookAction::Continue
    }
}

impl Drop for WebhookHook {
    fn drop(&mut self) {
        self.queue.closed.store(true, Ordering::SeqCst);
        self.queue.notify.notify_waiters();
    }
}

impl std::fmt::Debug for WebhookHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookHook")
            .field("endpoints", &self.endpoints.len())
            .field("points", &self.points)
            .finish()
    }
}

/// Map configured event names to hook points; empty subscribes to all.
/// Unknown names are warned and skipped (same posture as `hooks.json`
/// loading) so a typo disables one subscription, not the endpoint.
fn resolve_points(url: &str, events: &[String]) -> Vec<HookPoint> {
    if events.is_empty() {
        return EVENT_NAMES.iter().map(|(_, p)| *p).collect();
    }
    let mut points = Vec::new();
    for name in events {
        match parse_event_name(name) {
            Some(p) if !points.contains(&p) => points.push(p),
            Some(_) => {}
            None => tracing::warn!(
                endpoint = %url,
                event = %name,
                "webhook endpoint subscribes to unknown event; skipping it"
            ),
        }
    }
    points
}

/// The single worker draining the queue. Exits when the owning hook has been
/// dropped and the queue is empty.
async fn delivery_worker(queue: Arc<DeliveryQueue>, client: reqwest::Client) {
    loop {
        let Some(delivery) = queue.pop() else {
            if queue.closed.load(Ordering::SeqCst) {
                return;
            }
            queue.notify.notified().await;
            continue;
        };
        deliver(&client, &delivery).await;
        queue.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Outcome of a single HTTP attempt.
enum Attempt {
    Success,
    /// 5xx or connect error — worth one retry.
    Retryable(String),
    /// 3xx/4xx or other terminal condition — never retried.
    Fatal(String),
}

/// Deliver with the retry policy: one retry for 5xx/connect errors, never for
/// 3xx/4xx. Every failure is logged with endpoint + status.
async fn deliver(client: &reqwest::Client, delivery: &Delivery) {
    for attempt in 0u8..2 {
        match send_once(client, delivery).await {
            Attempt::Success => {
                tracing::debug!(
                    endpoint = %delivery.url,
                    delivery_id = %delivery.delivery_id,
                    attempt,
                    "webhook delivered"
                );
                return;
            }
            Attempt::Retryable(reason) if attempt == 0 => {
                tracing::warn!(
                    endpoint = %delivery.url,
                    delivery_id = %delivery.delivery_id,
                    "webhook delivery failed ({reason}); retrying once"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Attempt::Retryable(reason) => {
                tracing::warn!(
                    endpoint = %delivery.url,
                    delivery_id = %delivery.delivery_id,
                    "webhook delivery failed after retry ({reason}); giving up"
                );
                return;
            }
            Attempt::Fatal(reason) => {
                tracing::warn!(
                    endpoint = %delivery.url,
                    delivery_id = %delivery.delivery_id,
                    "webhook delivery failed ({reason}); not retrying"
                );
                return;
            }
        }
    }
}

/// Bare `HMAC-SHA256(secret, body)`, hex-encoded — the `X-Hub-Signature-256`
/// shape every off-the-shelf webhook verifier implements. Kept separate from
/// `hmac_chain::compute_hmac` on purpose: that one length-prefixes each field
/// for injectivity within Borg's own chains, and a receiver has no way to
/// reproduce the prefix.
#[allow(clippy::expect_used)]
fn sign_body(secret: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC accepts any key size");
    mac.update(body);
    crate::util::hex::hex_encode(&mac.finalize().into_bytes())
}

async fn send_once(client: &reqwest::Client, delivery: &Delivery) -> Attempt {
    let mut req = client
        .post(&delivery.url)
        .header("content-type", "application/json")
        .header("X-Borg-Delivery", &delivery.delivery_id);
    if let Some(secret) = &delivery.secret {
        let sig = sign_body(secret.as_bytes(), delivery.body.as_bytes());
        req = req.header("X-Borg-Signature-256", format!("sha256={sig}"));
    }
    match req.body(delivery.body.clone()).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Attempt::Success
            } else if status.is_redirection() {
                // Never followed (see client construction) — a redirecting
                // endpoint is a misconfiguration, not a transient fault.
                Attempt::Fatal(format!(
                    "status {status} — redirects are not followed; \
                     point the endpoint at its final URL"
                ))
            } else if status.is_server_error() {
                Attempt::Retryable(format!("status {status}"))
            } else {
                Attempt::Fatal(format!("status {status}"))
            }
        }
        Err(e) => Attempt::Retryable(format!("request error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::webhooks::WebhookEndpoint;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(endpoints: Vec<WebhookEndpoint>) -> WebhooksConfig {
        WebhooksConfig { endpoints }
    }

    fn endpoint(url: &str, secret: Option<&str>, events: &[&str]) -> WebhookEndpoint {
        // Route through serde so the same validated construction path as
        // config load is exercised.
        serde_json::from_value(serde_json::json!({
            "url": url,
            "secret": secret,
            "events": events,
        }))
        .expect("valid endpoint")
    }

    fn tool_result_ctx(point: HookPoint) -> HookContext {
        HookContext {
            point,
            session_id: "s_webhook".to_string(),
            turn_count: 7,
            data: HookData::ToolResult {
                name: "run_shell".to_string(),
                result: "ok".to_string(),
                is_error: false,
            },
        }
    }

    fn session_end_ctx() -> HookContext {
        HookContext {
            point: HookPoint::SessionEnd,
            session_id: "s_webhook".to_string(),
            turn_count: 9,
            data: HookData::SessionEnd {
                session_id: "s_webhook".to_string(),
                total_turns: 9,
            },
        }
    }

    /// Poll until the server has seen `n` requests (deliveries are async).
    async fn wait_for_requests(server: &MockServer, n: usize) -> Vec<wiremock::Request> {
        for _ in 0..200 {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n {
                return reqs;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "expected {n} requests, got {:?}",
            server.received_requests().await
        );
    }

    /// Let the async worker settle so "no more requests arrive" assertions
    /// are meaningful (covers the retry delay plus scheduling slack).
    async fn settle() {
        tokio::time::sleep(RETRY_DELAY + Duration::from_millis(500)).await;
    }

    #[tokio::test]
    async fn delivery_is_signed_over_exact_body_and_ids_match() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("top-secret"),
            &["Stop"],
        )]))
        .expect("endpoint configured");

        let action = hook
            .execute(&tool_result_ctx(HookPoint::TurnComplete))
            .await;
        assert!(matches!(action, HookAction::Continue));

        let reqs = wait_for_requests(&server, 1).await;
        let req = &reqs[0];

        // Signature must verify against the exact body bytes received.
        let sig_header = req
            .headers
            .get("X-Borg-Signature-256")
            .expect("signature header present")
            .to_str()
            .unwrap()
            .to_string();
        // Recomputed here from primitives rather than via `sign_body`, so this
        // asserts the published `X-Hub-Signature-256` contract instead of
        // agreeing with whatever the implementation happens to do. A framed or
        // salted MAC must fail this.
        let expected = {
            use hmac::{Hmac, KeyInit, Mac};
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"top-secret").unwrap();
            mac.update(&req.body);
            format!(
                "sha256={}",
                mac.finalize()
                    .into_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            )
        };
        assert_eq!(
            sig_header, expected,
            "signature must be a bare HMAC-SHA256 over the body bytes sent"
        );

        // Delivery id header must equal the delivery_id field inside the
        // signed body — one id, generated once.
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
        let header_id = req
            .headers
            .get("X-Borg-Delivery")
            .expect("delivery header present")
            .to_str()
            .unwrap();
        assert_eq!(body["delivery_id"].as_str().unwrap(), header_id);

        // Payload mirrors the script-hook stdin fields.
        assert_eq!(body["event"], "Stop");
        assert_eq!(body["session_id"], "s_webhook");
        assert_eq!(body["turn"], 7);
        assert!(body["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn endpoint_without_secret_sends_unsigned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            None,
            &["PostToolUse"],
        )]))
        .expect("endpoint configured");
        hook.execute(&tool_result_ctx(HookPoint::AfterToolCall))
            .await;

        let reqs = wait_for_requests(&server, 1).await;
        assert!(reqs[0].headers.get("X-Borg-Signature-256").is_none());
        assert!(reqs[0].headers.get("X-Borg-Delivery").is_some());
    }

    #[tokio::test]
    async fn redirect_is_not_followed_and_is_terminal() {
        let server = MockServer::start().await;
        // A 3xx pointing at a sibling path that would happily 200.
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("s"),
            &["Stop"],
        )]))
        .expect("endpoint configured");
        hook.execute(&tool_result_ctx(HookPoint::TurnComplete))
            .await;

        let _ = wait_for_requests(&server, 1).await;
        settle().await;
        let reqs = server.received_requests().await.unwrap();
        // Exactly one request: the original POST. No GET to the redirect
        // target (redirect not followed) and no retry (3xx is terminal).
        assert_eq!(reqs.len(), 1, "got {reqs:?}");
        assert_eq!(reqs[0].url.path(), "/hook");
        assert_eq!(reqs[0].method.as_str(), "POST");
    }

    #[tokio::test]
    async fn server_error_is_retried_exactly_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("s"),
            &["Stop"],
        )]))
        .expect("endpoint configured");
        hook.execute(&tool_result_ctx(HookPoint::TurnComplete))
            .await;

        let _ = wait_for_requests(&server, 2).await;
        settle().await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2, "5xx must get exactly one retry");
    }

    #[tokio::test]
    async fn client_error_is_never_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("s"),
            &["Stop"],
        )]))
        .expect("endpoint configured");
        hook.execute(&tool_result_ctx(HookPoint::TurnComplete))
            .await;

        let _ = wait_for_requests(&server, 1).await;
        settle().await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "4xx must not be retried");
    }

    #[tokio::test]
    async fn queue_full_drops_oldest_without_blocking() {
        // Exercise the queue directly with no worker draining it: pushes past
        // capacity must complete immediately and evict from the front.
        let queue = DeliveryQueue::new(3);
        for i in 0..5 {
            queue.push(Delivery {
                url: "https://example.invalid/hook".to_string(),
                secret: None,
                delivery_id: format!("d{i}"),
                body: "{}".to_string(),
            });
        }
        assert_eq!(queue.pending(), 3, "capacity must bound the queue");
        let ids: Vec<String> = std::iter::from_fn(|| queue.pop())
            .map(|d| d.delivery_id)
            .collect();
        assert_eq!(
            ids,
            vec!["d2", "d3", "d4"],
            "the oldest deliveries must be the ones dropped"
        );
    }

    #[tokio::test]
    async fn session_end_flushes_pending_deliveries_before_returning() {
        // A SessionEnd fired during shutdown must get its bounded flush: by
        // the time execute() returns, the delivery has reached the endpoint
        // (no post-hoc waiting in this test).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("s"),
            &["SessionEnd"],
        )]))
        .expect("endpoint configured");
        hook.execute(&session_end_ctx()).await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs.len(),
            1,
            "SessionEnd delivery must be flushed before execute() returns"
        );
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["event"], "SessionEnd");
    }

    #[tokio::test]
    async fn events_filter_limits_deliveries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let hook = WebhookHook::from_config(&config(vec![endpoint(
            &format!("{}/hook", server.uri()),
            Some("s"),
            &["SessionEnd"],
        )]))
        .expect("endpoint configured");

        // Not subscribed: nothing may be enqueued.
        assert!(!hook.points().contains(&HookPoint::TurnComplete));
        hook.execute(&tool_result_ctx(HookPoint::TurnComplete))
            .await;
        assert_eq!(
            hook.queue.pending(),
            0,
            "unsubscribed event must not enqueue"
        );
    }

    #[test]
    fn no_endpoints_yields_no_hook() {
        // Avoids registering a dead hook (and spawning a worker) when the
        // feature is unconfigured. Runs outside a runtime on purpose: the
        // no-endpoint path must not require tokio.
        assert!(WebhookHook::from_config(&WebhooksConfig::default()).is_none());
    }
}
