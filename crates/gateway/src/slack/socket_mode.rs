//! Slack Socket Mode WebSocket client.
//!
//! Socket Mode is the WebSocket-based alternative to public webhook
//! delivery. It lets borg run behind a NAT / on a laptop without a public
//! HTTPS endpoint, mirroring Telegram's polling-vs-webhook duality.
//!
//! Flow:
//! 1. POST to `apps.connections.open` with the `xapp-` app-level token to
//!    obtain a single-use `wss://` URL.
//! 2. Connect, receive a `hello` envelope.
//! 3. For each `events_api` / `interactive` / `slash_commands` envelope,
//!    immediately reply with `{"envelope_id": "..."}` to ack delivery, then
//!    dispatch the payload through the same parse pipeline as webhooks.
//! 4. On `disconnect` envelope (`reason: "warning"` → server-initiated
//!    refresh) drain in-flight work and re-open. On socket close, reconnect
//!    with capped exponential backoff.
//!
//! This module is split into pure logic (envelope parsing, ack shape,
//! reconnect decision, backoff curve) and a runtime (`run`) that owns the
//! WebSocket loop. The pure pieces have direct unit tests; the runtime is
//! exercised by integration tests against a stub WebSocket server.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, info, warn};

const SLACK_API_BASE: &str = crate::constants::SLACK_API_BASE;

/// Select Slack transport from configured credentials, with no user-visible
/// toggle. The rule is intentionally tight: if a Slack **app-level** token
/// (`xapp-…`) is present, run Socket Mode; otherwise fall back to webhook
/// delivery. Mirrors how Telegram silently chooses polling vs webhook.
///
/// We check the prefix rather than just "is set" so that a user who pastes
/// an `xoxb-` bot token into the wrong slot doesn't accidentally try to
/// open a WebSocket and fail loudly — they get the webhook path instead.
pub fn should_use_socket_mode(app_token: Option<&str>) -> bool {
    matches!(app_token, Some(t) if t.starts_with("xapp-"))
}

/// Min backoff between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_millis(500);
/// Max backoff cap. Slack docs suggest capping reconnect at ~30s; we cap
/// lower so an outage doesn't leave the agent silent for half a minute.
const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// One Socket Mode envelope as delivered by the Slack WebSocket.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SocketEnvelope {
    /// Envelope kind. Common values: `hello`, `disconnect`, `events_api`,
    /// `interactive`, `slash_commands`.
    #[serde(rename = "type")]
    pub envelope_type: String,
    /// Per-envelope id used to ack delivery. Absent on `hello`/`disconnect`.
    #[serde(default)]
    pub envelope_id: Option<String>,
    /// Reason field present on `disconnect` envelopes.
    #[serde(default)]
    pub reason: Option<String>,
    /// The inner event/interaction/command payload, opaque at this layer.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// If true, the ack body MAY include a `payload` field that Slack will
    /// surface to the user (for slash commands and view submissions).
    #[serde(default)]
    pub accepts_response_payload: bool,
}

/// Decide whether an envelope tells the runtime to drop and reopen the
/// socket. We currently treat all `disconnect` envelopes the same — Slack
/// docs distinguish `warning` (advance notice ~10s before the socket
/// closes) and `refresh_requested` (cycle now), but the handler is
/// synchronous and we own no in-flight queue, so there's nothing to
/// "drain" — we just reconnect. If a real drain becomes meaningful later
/// (e.g. when the runtime owns a pending-ack buffer) split this back out.
pub fn should_reconnect(env: &SocketEnvelope) -> bool {
    env.envelope_type == "disconnect"
}

/// Build the JSON body to send back as ack for an envelope.
///
/// `optional_response` is forwarded only when `accepts_response_payload` is
/// true on the envelope (slash commands and view submissions).
pub fn build_ack(
    envelope_id: &str,
    accepts_payload: bool,
    optional_response: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut ack = serde_json::json!({ "envelope_id": envelope_id });
    if accepts_payload {
        if let Some(payload) = optional_response {
            ack["payload"] = payload;
        }
    }
    ack
}

/// Capped exponential backoff for reconnect attempts.
///
/// Sequence: 0.5s, 1s, 2s, 4s, 8s, 16s, 20s (cap), 20s, …
pub fn backoff_for(attempt: u32) -> Duration {
    let base_ms = MIN_BACKOFF.as_millis() as u64;
    let factor = 1u64 << attempt.min(8); // cap shift to avoid overflow
    let ms = base_ms.saturating_mul(factor);
    Duration::from_millis(ms.min(MAX_BACKOFF.as_millis() as u64))
}

/// Response from `apps.connections.open`.
#[derive(Debug, Deserialize)]
struct OpenResponse {
    ok: bool,
    url: Option<String>,
    error: Option<String>,
}

/// Open a Socket Mode WebSocket URL using the app-level (xapp-) token.
///
/// `api_base_override` lets tests point at a stub server. In production,
/// pass `None` to hit `https://slack.com/api/apps.connections.open`.
pub async fn open_connection_url(
    http: &reqwest::Client,
    app_token: &str,
    api_base_override: Option<&str>,
) -> Result<String> {
    let base = api_base_override.unwrap_or(SLACK_API_BASE);
    let url = format!("{base}/apps.connections.open");
    let resp: OpenResponse = http
        .post(url)
        .bearer_auth(app_token)
        .send()
        .await
        .context("apps.connections.open request failed")?
        .json()
        .await
        .context("apps.connections.open response was not valid JSON")?;
    if !resp.ok {
        bail!(
            "apps.connections.open failed: {}",
            resp.error.unwrap_or_else(|| "unknown error".into())
        );
    }
    resp.url
        .ok_or_else(|| anyhow::anyhow!("apps.connections.open missing 'url' field"))
}

/// What to do with one inbound envelope.
///
/// The runtime calls a user-supplied handler that returns a `Dispatch` so the
/// runtime knows whether the envelope needs an enriched ack body (slash /
/// view) or a plain `{envelope_id}` ack.
#[derive(Debug, Clone)]
pub enum Dispatch {
    /// Ack with `{envelope_id}` only. The handler has already enqueued any
    /// follow-up work asynchronously.
    Ack,
    /// Ack with `{envelope_id, payload: <json>}`. Slack surfaces the payload
    /// to the user (slash commands, view submissions).
    AckWith(serde_json::Value),
    /// Don't ack — the handler refused this envelope (parse error, etc.).
    /// Slack will retry, so this is the rare path.
    Drop,
}

/// Run the Socket Mode loop until `shutdown` fires or a fatal error occurs.
///
/// `handler` is called for every non-`hello`/`disconnect` envelope. It must
/// return quickly (Slack docs: ack within 3s); enqueue real work onto an
/// async queue inside the handler.
pub async fn run<F>(
    http: reqwest::Client,
    app_token: String,
    mut handler: F,
    mut shutdown: mpsc::Receiver<()>,
) -> Result<()>
where
    F: FnMut(SocketEnvelope) -> Dispatch + Send + 'static,
{
    let mut attempt: u32 = 0;
    loop {
        // Honor shutdown between reconnect attempts.
        if shutdown.try_recv().is_ok() {
            info!("Slack Socket Mode: shutdown signal received");
            return Ok(());
        }

        let url = match open_connection_url(&http, &app_token, None).await {
            Ok(u) => u,
            Err(e) => {
                let backoff = backoff_for(attempt);
                warn!("Slack Socket Mode: open failed ({e}); reconnecting in {backoff:?}");
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => continue,
                    _ = shutdown.recv() => return Ok(()),
                }
            }
        };

        match run_session(&url, &mut handler, &mut shutdown).await {
            Ok(SessionEnd::Shutdown) => return Ok(()),
            Ok(SessionEnd::Reconnect) => {
                attempt = 0; // successful session resets backoff
                debug!("Slack Socket Mode: graceful reconnect requested by server");
            }
            Err(e) => {
                let backoff = backoff_for(attempt);
                warn!("Slack Socket Mode: session ended ({e}); reconnecting in {backoff:?}");
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.recv() => return Ok(()),
                }
            }
        }
    }
}

enum SessionEnd {
    Shutdown,
    Reconnect,
}

async fn run_session<F>(
    url: &str,
    handler: &mut F,
    shutdown: &mut mpsc::Receiver<()>,
) -> Result<SessionEnd>
where
    F: FnMut(SocketEnvelope) -> Dispatch + Send,
{
    let (ws, _) = connect_async(url)
        .await
        .context("WebSocket connect failed")?;
    let (mut sink, mut stream) = ws.split();

    loop {
        tokio::select! {
            _ = shutdown.recv() => return Ok(SessionEnd::Shutdown),
            msg = stream.next() => {
                let Some(msg) = msg else { return Ok(SessionEnd::Reconnect) };
                let msg = msg.context("WebSocket read failed")?;
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Ping(p) => {
                        if let Err(e) = sink.send(Message::Pong(p)).await {
                            warn!("Slack Socket Mode: pong send failed ({e}); reconnecting");
                            return Ok(SessionEnd::Reconnect);
                        }
                        continue;
                    }
                    Message::Close(_) => return Ok(SessionEnd::Reconnect),
                    _ => continue,
                };

                let env: SocketEnvelope = match serde_json::from_str(&text) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Slack Socket Mode: bad envelope JSON ({e}): {text}");
                        continue;
                    }
                };

                if env.envelope_type == "hello" {
                    debug!("Slack Socket Mode: hello received");
                    continue;
                }

                if should_reconnect(&env) {
                    return Ok(SessionEnd::Reconnect);
                }

                let envelope_id = match env.envelope_id.clone() {
                    Some(id) => id,
                    None => continue, // hello/disconnect already handled; nothing else should be id-less
                };
                let accepts_payload = env.accepts_response_payload;

                match handler(env) {
                    Dispatch::Ack => {
                        let ack = build_ack(&envelope_id, false, None);
                        if let Err(e) = sink.send(Message::Text(ack.to_string())).await {
                            warn!("Slack Socket Mode: ack send failed for envelope {envelope_id} ({e}); reconnecting");
                            return Ok(SessionEnd::Reconnect);
                        }
                    }
                    Dispatch::AckWith(p) => {
                        let ack = build_ack(&envelope_id, accepts_payload, Some(p));
                        if let Err(e) = sink.send(Message::Text(ack.to_string())).await {
                            warn!("Slack Socket Mode: ack send failed for envelope {envelope_id} ({e}); reconnecting");
                            return Ok(SessionEnd::Reconnect);
                        }
                    }
                    Dispatch::Drop => {
                        // Skip ack so Slack retries. Logged at debug because
                        // this should be rare (the parse layer surfaces
                        // BadRequest synchronously instead of dropping).
                        debug!("Slack Socket Mode: handler dropped envelope {envelope_id}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_selection_uses_xapp_prefix() {
        assert!(should_use_socket_mode(Some("xapp-1-A123-456-abc")));
        assert!(!should_use_socket_mode(None));
        // An xoxb- bot token in the wrong slot must NOT trigger Socket Mode.
        // Otherwise we'd open a WebSocket with a token Slack rejects, and
        // the user would see opaque connect errors instead of falling back.
        assert!(!should_use_socket_mode(Some("xoxb-real-bot-token")));
        assert!(!should_use_socket_mode(Some("")));
        // Accidental whitespace must NOT be accepted as a valid prefix.
        assert!(!should_use_socket_mode(Some(" xapp-trim-me")));
    }

    #[test]
    fn build_ack_minimal_shape() {
        let v = build_ack("env-1", false, None);
        assert_eq!(v, serde_json::json!({ "envelope_id": "env-1" }));
        // Even if a response payload is supplied, when accepts_payload=false
        // it MUST be dropped — Slack rejects unexpected fields on most acks.
        let v = build_ack("env-1", false, Some(serde_json::json!({"text":"hi"})));
        assert_eq!(v, serde_json::json!({ "envelope_id": "env-1" }));
    }

    #[test]
    fn build_ack_with_response_payload_when_accepted() {
        let v = build_ack(
            "env-2",
            true,
            Some(serde_json::json!({"response_action": "clear"})),
        );
        assert_eq!(v["envelope_id"], "env-2");
        assert_eq!(v["payload"]["response_action"], "clear");
    }

    #[test]
    fn build_ack_omits_payload_when_none_even_if_accepted() {
        let v = build_ack("env-3", true, None);
        // No `payload` key — slash commands without a sync reply just ack.
        assert_eq!(v, serde_json::json!({ "envelope_id": "env-3" }));
    }

    #[test]
    fn should_reconnect_distinguishes_disconnect_from_normal_envelopes() {
        // All `disconnect` reasons (warning, refresh_requested, link_disabled,
        // and any future ones) trigger reconnect — never silently stop.
        // Non-disconnect envelopes never do.
        let cases: &[(&str, Option<&str>, bool)] = &[
            ("disconnect", Some("warning"), true),
            ("disconnect", Some("refresh_requested"), true),
            ("disconnect", Some("link_disabled"), true),
            ("disconnect", None, true),
            ("events_api", None, false),
            ("interactive", None, false),
            ("hello", None, false),
        ];
        for (kind, reason, expected) in cases {
            let env = SocketEnvelope {
                envelope_type: (*kind).into(),
                envelope_id: None,
                reason: reason.map(str::to_string),
                payload: None,
                accepts_response_payload: false,
            };
            assert_eq!(
                should_reconnect(&env),
                *expected,
                "should_reconnect({kind}, {reason:?})"
            );
        }
    }

    #[test]
    fn backoff_is_monotonic_and_capped() {
        // Walk a long sequence of attempts; each step must be >= the previous,
        // and we must never exceed the configured ceiling.
        let mut last = Duration::ZERO;
        for attempt in 0..30 {
            let b = backoff_for(attempt);
            assert!(b >= last, "backoff regressed at attempt {attempt}");
            assert!(
                b <= MAX_BACKOFF,
                "backoff exceeded cap at attempt {attempt}"
            );
            last = b;
        }
        // After enough attempts, we hit the cap and stay there.
        assert_eq!(backoff_for(30), MAX_BACKOFF);
    }

    #[test]
    fn backoff_starts_at_min() {
        assert_eq!(backoff_for(0), MIN_BACKOFF);
    }

    #[test]
    fn envelope_deserializes_events_api_payload() {
        let json = r#"{
            "type": "events_api",
            "envelope_id": "abc-123",
            "accepts_response_payload": false,
            "payload": {
                "type": "event_callback",
                "event": {"type": "message", "text": "hi"}
            }
        }"#;
        let env: SocketEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.envelope_type, "events_api");
        assert_eq!(env.envelope_id.as_deref(), Some("abc-123"));
        assert!(!env.accepts_response_payload);
        assert_eq!(env.payload.unwrap()["event"]["text"], "hi");
    }

    #[test]
    fn envelope_deserializes_slash_command_with_accepts_payload() {
        let json = r#"{
            "type": "slash_commands",
            "envelope_id": "env-7",
            "accepts_response_payload": true,
            "payload": {"command": "/borg", "user_id": "U1"}
        }"#;
        let env: SocketEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.envelope_type, "slash_commands");
        assert!(env.accepts_response_payload);
    }

    #[test]
    fn envelope_deserializes_disconnect_with_reason() {
        let json = r#"{ "type":"disconnect", "reason":"warning" }"#;
        let env: SocketEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.envelope_type, "disconnect");
        assert_eq!(env.reason.as_deref(), Some("warning"));
        assert!(env.envelope_id.is_none());
    }

    #[test]
    fn envelope_deserializes_hello() {
        // `hello` envelope arrives on connect; we just need it to parse
        // without choking on the missing envelope_id.
        let json = r#"{ "type": "hello" }"#;
        let env: SocketEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.envelope_type, "hello");
        assert!(env.envelope_id.is_none());
        assert!(env.reason.is_none());
    }

    // ─── Integration test: open_connection_url against a stub HTTP server ───

    #[tokio::test]
    async fn open_connection_url_reads_url_from_response() {
        // Use a one-shot tokio listener as a tiny HTTP server. We don't need
        // a full axum handler; reading the request and writing a static
        // response is enough to exercise the JSON parse path.
        use std::io::Write;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            // Drain headers — we don't validate them in this test, but we
            // must read enough to release the writer.
            let _ = sock.read(&mut buf).await.unwrap();
            let body = r#"{"ok":true,"url":"wss://example/socket?ticket=abc"}"#;
            let mut resp = Vec::new();
            write!(
                resp,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            sock.write_all(&resp).await.unwrap();
            sock.shutdown().await.ok();
        });

        let http = reqwest::Client::new();
        let url = open_connection_url(&http, "xapp-test", Some(&base))
            .await
            .expect("open_connection_url succeeds");
        assert_eq!(url, "wss://example/socket?ticket=abc");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn open_connection_url_surfaces_slack_error_field() {
        use std::io::Write;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            let body = r#"{"ok":false,"error":"invalid_auth"}"#;
            let mut resp = Vec::new();
            write!(
                resp,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            sock.write_all(&resp).await.unwrap();
            sock.shutdown().await.ok();
        });

        let http = reqwest::Client::new();
        let result = open_connection_url(&http, "xapp-bad", Some(&base)).await;
        let err = result.expect_err("expected error on ok:false response");
        assert!(
            err.to_string().contains("invalid_auth"),
            "error must surface the Slack error code: {err}"
        );
        server.await.unwrap();
    }
}
