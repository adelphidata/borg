//! Per-server session actor.
//!
//! One [`SessionActor`] runs per configured MCP server. It owns the
//! transport, handles request/response correlation, the `initialize`
//! handshake, `tools/list_changed` re-discovery, and per-call timeouts.
//!
//! The supervisor that *spawns* sessions and decides when to reconnect
//! lives in [`crate::registry`]. This module is concerned only with the
//! life of one healthy session: connect → handshake → serve calls until
//! something fails.

use crate::errors::{McpError, Result};
use crate::manifest::ServerManifest;
use crate::protocol::{
    methods, render_call_result, ClientCapabilities, ClientInfo, InitializeParams,
    InitializeResult, Message, Request, RpcError, ToolCallResult, ToolsListResult, JSONRPC_VERSION,
};
use crate::transport::Transport;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;

/// JSON-RPC "method not found". We use this code for every server-initiated
/// request because borg consumes MCP — it does not implement sampling, roots,
/// or any other server-driven RPC.
const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;

/// MCP method name for server-initiated sampling. We refuse this explicitly
/// (and audit the refusal) so a hostile server cannot use Borg's LLM to
/// generate text on its behalf — exactly the wrong direction for the trust
/// model documented in `docs/mcp.md`.
pub const SAMPLING_CREATE_MESSAGE: &str = "sampling/createMessage";

/// What a session reports after a `tools/list` round-trip.
#[derive(Debug, Clone)]
pub struct DiscoveredTools {
    /// Tools the server advertised. Borg will run them through the schema
    /// gauntlet before registering.
    pub tools: Vec<crate::protocol::McpToolSchema>,
    /// Server-info block from `initialize`, if the peer provided one.
    pub server_info: Option<String>,
}

/// Cap on stashed notifications while a request is in flight.
///
/// A hostile peer can otherwise spam `notifications/tools/list_changed` (or
/// any notification) during a long `tools/call` and balloon memory. When the
/// cap is reached, we drop the *oldest* notification and emit a single
/// `tracing::warn!` per session per overflow window.
pub(crate) const NOTIFICATION_BUFFER_CAP: usize = 256;

/// Cap on `notifications/message` server-log lines forwarded to `tracing`
/// per session. Same posture as [`NOTIFICATION_BUFFER_CAP`]: a chatty or
/// hostile server must not flood the operator's logs. Past the cap we emit
/// one "further server logs suppressed" warning and drop the rest.
pub(crate) const SERVER_LOG_CAP: usize = 256;

/// Byte cap applied to each server-log body (and, defensively, the level /
/// logger fields) before it reaches `tracing`. Server logs bypass the
/// cortex per-call response cap, so they need their own bound.
pub(crate) const SERVER_LOG_MAX_BYTES: usize = 2048;

/// Coarse `tracing` severity for a server-supplied RFC-5424 level string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogSeverity {
    /// debug / info / notice — and any unknown level.
    Info,
    /// warning.
    Warn,
    /// error / critical / alert / emergency.
    Error,
}

/// Map an MCP `notifications/message` level (RFC-5424 names) onto the
/// severity we emit at. Unknown levels map to `Info`: the message still
/// surfaces, and an untrusted peer can't force error-level noise by
/// inventing level names.
pub(crate) fn map_log_level(level: &str) -> LogSeverity {
    match level.to_ascii_lowercase().as_str() {
        "warning" => LogSeverity::Warn,
        "error" | "critical" | "alert" | "emergency" => LogSeverity::Error,
        // debug / info / notice, plus anything unrecognized.
        _ => LogSeverity::Info,
    }
}

/// Truncate `s` to at most `max` bytes on a char boundary. Server-supplied
/// log fields are unbounded on the wire; the operator's log is not.
fn bounded(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Neutralize control characters in a server-supplied log field.
///
/// Server log content is untrusted: embedded newlines forge additional
/// operator-looking log lines and ANSI/CSI escapes can rewrite the terminal
/// that renders `daemon.err`. Every control char except `\t` becomes a
/// space; borrows unchanged input to keep the clean path allocation-free.
fn neutralize_control_chars(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(|c| c.is_control() && c != '\t') {
        std::borrow::Cow::Owned(
            s.chars()
                .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// One open MCP session against a single server.
///
/// Methods are `async` but not internally locked — the supervisor uses an
/// actor pattern (one task owns the session, callers send commands over an
/// mpsc) to enforce single-writer ordering on the underlying stream.
pub struct SessionActor {
    server_name: String,
    transport: Box<dyn Transport>,
    next_id: u64,
    call_timeout: Duration,
    /// Notifications observed while waiting for a response. The supervisor
    /// drains these between requests to handle `tools/list_changed`. Bounded
    /// by [`NOTIFICATION_BUFFER_CAP`] to defend against notification flooding.
    pending_notifications: std::collections::VecDeque<Message>,
    /// True iff we've already logged a notification-overflow warning this
    /// session. Avoids log spam from a sustained flood.
    notification_overflow_logged: bool,
    /// Count of `notifications/message` server-log lines forwarded to
    /// `tracing` this session. Bounded by [`SERVER_LOG_CAP`].
    server_logs_emitted: usize,
    /// True once the one-shot "further server logs suppressed" warning has
    /// been emitted for this session.
    server_logs_suppressed: bool,
    /// Server-initiated requests refused this session, keyed by method.
    /// Populated by the request-loop's "not found" path; the supervisor
    /// reads + drains this so it can emit one audit row per (server, method)
    /// without growing the cortex layer's blast radius.
    refused_server_requests: Vec<(String, u64)>,
}

impl SessionActor {
    /// Build a new session over the given transport. Does **not** perform
    /// the handshake — call [`SessionActor::handshake`] separately so callers
    /// can attach timeouts / classify errors per phase.
    pub fn new(manifest: &ServerManifest, transport: Box<dyn Transport>) -> Self {
        Self {
            server_name: manifest.name.clone(),
            transport,
            next_id: 1,
            call_timeout: manifest.call_timeout,
            pending_notifications: std::collections::VecDeque::new(),
            notification_overflow_logged: false,
            server_logs_emitted: 0,
            server_logs_suppressed: false,
            refused_server_requests: Vec::new(),
        }
    }

    /// Drain the list of refused server-initiated requests since the last
    /// drain. The cortex supervisor uses this to write
    /// `MCP_SAMPLING_REFUSED` audit rows; the protocol layer doesn't have
    /// access to the audit writer (and shouldn't).
    pub fn drain_refused_server_requests(&mut self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.refused_server_requests)
    }

    /// Perform the `initialize` handshake and send the `notifications/initialized`
    /// follow-up. Returns the negotiated server-info string (for audit logs).
    pub async fn handshake(&mut self) -> Result<Option<String>> {
        let params = InitializeParams {
            protocol_version: crate::protocol::MCP_PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo::standard(),
        };
        let result: InitializeResult = self
            .request_typed(methods::INITIALIZE, Some(serde_json::to_value(params)?))
            .await?;

        // Per spec, send `notifications/initialized` (a notification, no `id`)
        // before issuing further requests.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": methods::INITIALIZED_NOTIFICATION,
        });
        self.transport.send(notif).await?;

        Ok(result
            .server_info
            .map(|si| format!("{} {}", si.name, si.version)))
    }

    /// Issue `tools/list` and return the discovered tools.
    pub async fn list_tools(&mut self) -> Result<DiscoveredTools> {
        let result: ToolsListResult = self.request_typed(methods::TOOLS_LIST, None).await?;
        Ok(DiscoveredTools {
            tools: result.tools,
            server_info: None,
        })
    }

    /// Issue `tools/call` for `tool_name` with `arguments`. Returns the
    /// rendered text + the raw [`ToolCallResult`] (the cortex adapter wants
    /// both: text for the LLM, `is_error` for the error envelope).
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<(String, ToolCallResult)> {
        let params = json!({
            "name": tool_name,
            "arguments": arguments,
        });
        let raw: ToolCallResult = self
            .request_typed(methods::TOOLS_CALL, Some(params))
            .await?;
        let text = render_call_result(&raw);
        Ok((text, raw))
    }

    /// Issue an MCP `ping` as a liveness probe. The spec requires receivers
    /// to respond promptly, so this is cheap. Callers should treat only
    /// [`McpError::Timeout`] / [`McpError::TransportClosed`] as death — an
    /// error *reply* (e.g. from a server that skipped implementing `ping`)
    /// still proves the peer is alive and reading its stdin.
    pub async fn ping(&mut self) -> Result<()> {
        self.request(methods::PING, None).await.map(|_| ())
    }

    /// Drain any notifications received while we were waiting on the last
    /// response. The supervisor calls this between requests so it can react
    /// to `tools/list_changed` without racing the next call.
    pub fn drain_notifications(&mut self) -> Vec<Message> {
        self.notification_overflow_logged = false;
        std::mem::take(&mut self.pending_notifications)
            .into_iter()
            .collect()
    }

    fn push_notification(&mut self, msg: Message) {
        if self.pending_notifications.len() >= NOTIFICATION_BUFFER_CAP {
            // Drop oldest, keep newest. `tools/list_changed` only carries
            // "something changed, re-list" — the newest one tells the same
            // story as the older. Spam is bounded.
            self.pending_notifications.pop_front();
            if !self.notification_overflow_logged {
                tracing::warn!(
                    server = %self.server_name,
                    cap = NOTIFICATION_BUFFER_CAP,
                    "mcp notification buffer overflow; dropping oldest"
                );
                self.notification_overflow_logged = true;
            }
        }
        self.pending_notifications.push_back(msg);
    }

    /// Forward one `notifications/message` server log line to `tracing`.
    ///
    /// Total: malformed params (missing, non-object, wrong-typed fields)
    /// degrade to defaults and still surface — they can never error the
    /// session. Bounded by [`SERVER_LOG_CAP`] with a one-shot suppression
    /// warning so a chatty server can't flood the operator's logs.
    fn handle_server_log(&mut self, params: Option<&Value>) {
        if self.server_logs_emitted >= SERVER_LOG_CAP {
            if !self.server_logs_suppressed {
                tracing::warn!(
                    server = %self.server_name,
                    cap = SERVER_LOG_CAP,
                    "mcp server log cap reached; further server logs suppressed this session"
                );
                self.server_logs_suppressed = true;
            }
            return;
        }
        self.server_logs_emitted += 1;

        let field = |key: &str| params.and_then(|p| p.get(key));
        let level_raw = field("level").and_then(Value::as_str).unwrap_or("");
        let logger = field("logger").and_then(Value::as_str).unwrap_or("-");
        // `data` may be any JSON value: strings pass through, everything
        // else is compact-serialized so structured payloads stay readable.
        let body = match field("data") {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
        };
        let level = neutralize_control_chars(bounded(level_raw, 32));
        let logger = neutralize_control_chars(bounded(logger, 128));
        let body = neutralize_control_chars(bounded(&body, SERVER_LOG_MAX_BYTES));
        let (level, logger, body) = (level.as_ref(), logger.as_ref(), body.as_ref());

        // `tracing` macros need a const level, so dispatch per severity.
        match map_log_level(level) {
            LogSeverity::Info => tracing::info!(
                server = %self.server_name,
                level, logger,
                "mcp server log: {body}"
            ),
            LogSeverity::Warn => tracing::warn!(
                server = %self.server_name,
                level, logger,
                "mcp server log: {body}"
            ),
            LogSeverity::Error => tracing::error!(
                server = %self.server_name,
                level, logger,
                "mcp server log: {body}"
            ),
        }
    }

    /// Close the transport. Idempotent.
    pub async fn close(mut self) -> Result<()> {
        self.transport.close().await
    }

    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn request_typed<R>(&mut self, method: &str, params: Option<Value>) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        let raw = self.request(method, params).await?;
        serde_json::from_value::<R>(raw).map_err(McpError::Json)
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        // wrapping_add: at 1M calls/sec, u64 wrap takes ~580k years; the
        // wrap branch is unreachable in any real session lifetime.
        self.next_id = self.next_id.wrapping_add(1);

        let req = Request::new(id, method, params);
        let payload = serde_json::to_value(&req)?;
        self.transport.send(payload).await?;

        let deadline = Instant::now() + self.call_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout {
                    ms: self.call_timeout.as_millis() as u64,
                });
            }

            let recv_fut = self.transport.recv();
            let msg = match tokio::time::timeout(remaining, recv_fut).await {
                Ok(Ok(Some(m))) => m,
                Ok(Ok(None)) => {
                    return Err(McpError::TransportClosed(format!(
                        "{}: peer closed mid-request",
                        self.server_name
                    )));
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(McpError::Timeout {
                        ms: self.call_timeout.as_millis() as u64,
                    });
                }
            };

            match msg.id {
                Some(resp_id) if resp_id == id => {
                    return match (msg.result, msg.error) {
                        (Some(value), _) => Ok(value),
                        (None, Some(rpc_err)) if rpc_err.looks_like_auth() => {
                            Err(McpError::Auth(rpc_err.message))
                        }
                        (None, Some(rpc_err)) => Err(McpError::Protocol(rpc_err.message)),
                        (None, None) => Err(McpError::Protocol(format!(
                            "{}: response {id} had neither result nor error",
                            self.server_name
                        ))),
                    };
                }
                Some(other_id) if msg.method.is_some() => {
                    // Server-initiated request (id + method both set).
                    // Borg is an MCP *consumer* — we don't implement
                    // sampling, roots, or any other server-driven RPC. Reply
                    // with -32601 (Method not found) so the server learns
                    // immediately rather than hanging waiting for a result.
                    //
                    // Without this branch the loop would fall into the
                    // "stale response" path below and silently drop the
                    // request, which on a determined / buggy server can
                    // produce a hang plus a confusing protocol error later.
                    //
                    // We also record the refusal so the cortex supervisor
                    // can emit an `mcp_sampling_refused` audit row.
                    let method = msg.method.clone().unwrap_or_default();
                    let err = RpcError {
                        code: JSONRPC_METHOD_NOT_FOUND,
                        message: format!("method '{method}' is not implemented by this client"),
                        data: None,
                    };
                    let reply = json!({
                        "jsonrpc": JSONRPC_VERSION,
                        "id": other_id,
                        "error": err,
                    });
                    // Best-effort: if the transport is already broken we'll
                    // catch it on the next iteration's recv. Don't unwind
                    // mid-call just because the refusal write failed.
                    if let Err(e) = self.transport.send(reply).await {
                        tracing::warn!(
                            server = %self.server_name,
                            method = %method,
                            error = %e,
                            "failed to send method-not-found refusal for server-initiated request",
                        );
                    } else {
                        tracing::info!(
                            server = %self.server_name,
                            method = %method,
                            "refused server-initiated MCP request",
                        );
                        self.refused_server_requests.push((method, other_id));
                    }
                }
                Some(other_id) => {
                    // Stale response from a previous timed-out call. Log + ignore.
                    tracing::debug!(
                        server = %self.server_name,
                        unexpected_id = other_id,
                        awaiting = id,
                        "mcp response for non-current request id"
                    );
                }
                None => {
                    // Server-log notifications are routed to `tracing`
                    // immediately (bounded) — they never enter the buffer,
                    // so a chatty logger can't crowd out `tools/list_changed`.
                    if msg.method.as_deref() == Some(methods::LOGGING_MESSAGE) {
                        self.handle_server_log(msg.params.as_ref());
                        continue;
                    }
                    // Other notifications — stash for the supervisor (bounded).
                    if let Some(method) = msg.method.as_deref() {
                        tracing::debug!(
                            server = %self.server_name,
                            method,
                            "mcp notification received during request"
                        );
                    }
                    self.push_notification(msg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ToolFilter, Transport as TransportConfig};
    use crate::transport::Transport;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// In-memory transport for unit tests. Records sent payloads and replays
    /// pre-scripted inbound messages.
    ///
    /// `recv` *blocks* when the inbound queue is empty (rather than returning
    /// `Ok(None)`) — this models a real silent peer. The transport only
    /// signals EOF when [`ScriptedTransport::close_peer`] has been called.
    struct ScriptedTransport {
        sent: Arc<Mutex<Vec<Value>>>,
        inbound: Arc<Mutex<VecDeque<Message>>>,
        eof: Arc<Mutex<bool>>,
    }

    impl ScriptedTransport {
        fn new(sent: Arc<Mutex<Vec<Value>>>, inbound: Arc<Mutex<VecDeque<Message>>>) -> Self {
            Self {
                sent,
                inbound,
                eof: Arc::new(Mutex::new(false)),
            }
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn send(&mut self, msg: Value) -> Result<()> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
        async fn recv(&mut self) -> Result<Option<Message>> {
            loop {
                if let Some(m) = self.inbound.lock().unwrap().pop_front() {
                    return Ok(Some(m));
                }
                if *self.eof.lock().unwrap() {
                    return Ok(None);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        async fn close(&mut self) -> Result<()> {
            *self.eof.lock().unwrap() = true;
            Ok(())
        }
    }

    fn manifest() -> ServerManifest {
        ServerManifest {
            name: "echo".into(),
            transport: TransportConfig::Stdio {
                command: "cat".into(),
                args: vec![],
                env: Default::default(),
            },
            connect_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_millis(500),
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
            tools: ToolFilter::default(),
        }
    }

    #[tokio::test]
    async fn handshake_emits_initialize_and_initialized_notification() {
        // Spec: client sends `initialize`, awaits result, then sends the
        // `notifications/initialized` notification. Failing this is a
        // protocol violation — many servers reject subsequent `tools/list`
        // calls if the notification is missing.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inbound = Arc::new(Mutex::new(VecDeque::new()));

        // Pre-script the `initialize` response.
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: Some(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "demo", "version": "0.1"}
            })),
            error: None,
            params: None,
        });

        let transport = Box::new(ScriptedTransport::new(sent.clone(), inbound));
        let mut session = SessionActor::new(&manifest(), transport);

        let info = session.handshake().await.unwrap();
        assert_eq!(info.as_deref(), Some("demo 0.1"));

        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "expected initialize + initialized, got {sent:?}"
        );
        assert_eq!(sent[0]["method"], "initialize");
        // initialized is a *notification* — must have no id.
        assert_eq!(sent[1]["method"], "notifications/initialized");
        assert!(
            sent[1].get("id").is_none(),
            "initialized must be a notification"
        );
    }

    #[tokio::test]
    async fn auth_shaped_rpc_error_surfaces_as_auth_variant() {
        // Spec: peer-side auth failures stop the supervisor from retrying.
        // The translation from JSON-RPC error → McpError::Auth must happen
        // here; otherwise retry loops can lock out credentials.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: None,
            error: Some(crate::protocol::RpcError {
                code: -32001,
                message: "unauthorized: bad token".into(),
                data: None,
            }),
            params: None,
        });

        let transport = Box::new(ScriptedTransport::new(
            Arc::new(Mutex::new(Vec::new())),
            inbound,
        ));
        let mut session = SessionActor::new(&manifest(), transport);
        let err = session.handshake().await.unwrap_err();
        assert!(matches!(err, McpError::Auth(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn timeout_when_peer_silent() {
        // Spec: per-call timeout fires even if the transport never closes.
        // The fixture transport blocks on empty queue (modeling a silent
        // peer), so this exercises the real wall-clock timeout path.
        let transport = Box::new(ScriptedTransport::new(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(VecDeque::new())),
        ));
        let mut session = SessionActor::new(&manifest(), transport);
        let err = session.handshake().await.unwrap_err();
        assert!(matches!(err, McpError::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn server_initiated_sampling_request_is_refused_with_method_not_found() {
        // Spec: borg consumes MCP. A server that sends an unsolicited
        // `sampling/createMessage` request (or any other server-initiated
        // RPC) must receive a JSON-RPC -32601 reply, and the refusal must
        // be drained by the supervisor so it can audit it. The wrong
        // behavior — silently dropping the request — lets a hostile server
        // hang waiting for a result it will never get, and hides the
        // attempt from the operator's audit trail.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inbound = Arc::new(Mutex::new(VecDeque::new()));

        // First inbound message: a server-initiated `sampling/createMessage`
        // request (id+method both set). The session should refuse this and
        // keep looping for the actual initialize response.
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(9001),
            method: Some(SAMPLING_CREATE_MESSAGE.into()),
            result: None,
            error: None,
            params: Some(json!({"messages": []})),
        });
        // Second: the actual initialize result, so the handshake completes.
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: Some(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "demo", "version": "0.1"}
            })),
            error: None,
            params: None,
        });

        let transport = Box::new(ScriptedTransport::new(sent.clone(), inbound));
        let mut session = SessionActor::new(&manifest(), transport);
        session.handshake().await.expect("handshake should succeed");

        let sent_guard = sent.lock().unwrap();
        // We expect three outbound frames: initialize, refusal-for-9001,
        // initialized-notification. Order: initialize first, then the
        // refusal (sent while waiting for response 1), then initialized
        // after the response arrives.
        assert!(
            sent_guard.len() >= 3,
            "expected initialize + refusal + initialized, got {sent_guard:?}",
        );
        let refusal = sent_guard
            .iter()
            .find(|v| v.get("id").and_then(Value::as_u64) == Some(9001))
            .expect("no refusal frame addressed to the server's request id");
        let err = refusal
            .get("error")
            .expect("refusal frame must carry a JSON-RPC error object");
        assert_eq!(
            err.get("code").and_then(Value::as_i64),
            Some(-32601),
            "refusal must use the JSON-RPC method-not-found code (-32601), got {err:?}",
        );
        drop(sent_guard);

        let drained = session.drain_refused_server_requests();
        assert_eq!(
            drained.len(),
            1,
            "supervisor must see exactly one refused server request, got {drained:?}",
        );
        assert_eq!(drained[0].0, SAMPLING_CREATE_MESSAGE);
        assert_eq!(drained[0].1, 9001);
    }

    #[tokio::test]
    async fn ping_distinguishes_alive_error_reply_from_silence() {
        // Liveness contract: a peer that RESPONDS to ping — even with a
        // JSON-RPC error because it never implemented the method — is alive
        // and must surface as a non-timeout error. A peer that stays silent
        // must surface as Timeout so the supervisor can tear it down.

        // Case 1: empty-result reply → Ok.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: Some(json!({})),
            error: None,
            params: None,
        });
        let mut session = SessionActor::new(
            &manifest(),
            Box::new(ScriptedTransport::new(
                Arc::new(Mutex::new(Vec::new())),
                inbound,
            )),
        );
        session.ping().await.expect("empty-result reply is a pong");

        // Case 2: method-not-found reply → Protocol error, NOT Timeout.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: None,
            error: Some(crate::protocol::RpcError {
                code: JSONRPC_METHOD_NOT_FOUND,
                message: "ping not implemented".into(),
                data: None,
            }),
            params: None,
        });
        let mut session = SessionActor::new(
            &manifest(),
            Box::new(ScriptedTransport::new(
                Arc::new(Mutex::new(Vec::new())),
                inbound,
            )),
        );
        let err = session.ping().await.unwrap_err();
        assert!(
            matches!(err, McpError::Protocol(_)),
            "an error reply proves the peer is alive; got {err:?}"
        );

        // Case 3: silent peer → Timeout (the supervisor's death signal).
        let mut session = SessionActor::new(
            &manifest(),
            Box::new(ScriptedTransport::new(
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Mutex::new(VecDeque::new())),
            )),
        );
        let err = session.ping().await.unwrap_err();
        assert!(
            matches!(err, McpError::Timeout { .. }),
            "silence must surface as Timeout; got {err:?}"
        );
    }

    #[tokio::test]
    async fn notification_buffer_caps_at_configured_max() {
        // Spec: a hostile peer spamming notifications during a long request
        // cannot grow the session's memory unboundedly. Excess is dropped
        // (oldest first) and a single warning is logged per session. This
        // is the runtime mirror of the schema-size gauntlet — both defend
        // against peer-driven memory blowup.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        // Pre-script: CAP+50 notifications, then a matching response so the
        // request returns. The session must keep at most CAP notifications.
        for _ in 0..(NOTIFICATION_BUFFER_CAP + 50) {
            inbound.lock().unwrap().push_back(Message {
                jsonrpc: Some("2.0".into()),
                id: None,
                method: Some(crate::protocol::methods::TOOLS_LIST_CHANGED.into()),
                result: None,
                error: None,
                params: None,
            });
        }
        inbound.lock().unwrap().push_back(Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: Some(serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "demo", "version": "0.1"}
            })),
            error: None,
            params: None,
        });

        let transport = Box::new(ScriptedTransport::new(
            Arc::new(Mutex::new(Vec::new())),
            inbound,
        ));
        let mut session = SessionActor::new(&manifest(), transport);
        let _info = session.handshake().await.unwrap();

        let drained = session.drain_notifications();
        assert!(
            drained.len() <= NOTIFICATION_BUFFER_CAP,
            "kept {} notifications, cap is {}",
            drained.len(),
            NOTIFICATION_BUFFER_CAP,
        );
    }

    #[test]
    fn server_log_level_mapping_follows_rfc5424_severity() {
        // Spec: debug/info/notice → info; warning → warn; error and above
        // (error/critical/alert/emergency) → error. Unknown or hostile
        // level strings must NOT escalate — they map to info, case-
        // insensitively. A wrong mapping either hides server errors from
        // the operator or lets a hostile peer spam error-level noise.
        for (level, expected) in [
            ("debug", LogSeverity::Info),
            ("info", LogSeverity::Info),
            ("notice", LogSeverity::Info),
            ("warning", LogSeverity::Warn),
            ("error", LogSeverity::Error),
            ("critical", LogSeverity::Error),
            ("alert", LogSeverity::Error),
            ("emergency", LogSeverity::Error),
            ("WARNING", LogSeverity::Warn),
            ("Error", LogSeverity::Error),
            ("", LogSeverity::Info),
            ("verbose-custom", LogSeverity::Info),
            ("<script>alert(1)</script>", LogSeverity::Info),
        ] {
            assert_eq!(
                map_log_level(level),
                expected,
                "level {level:?} mapped wrong"
            );
        }
    }

    fn log_notification(params: Option<Value>) -> Message {
        Message {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: Some(crate::protocol::methods::LOGGING_MESSAGE.into()),
            result: None,
            error: None,
            params,
        }
    }

    fn initialize_response() -> Message {
        Message {
            jsonrpc: Some("2.0".into()),
            id: Some(1),
            method: None,
            result: Some(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "demo", "version": "0.1"}
            })),
            error: None,
            params: None,
        }
    }

    #[tokio::test]
    async fn malformed_server_log_notification_never_errors_the_session() {
        // Spec: a broken (or hostile) logger on the peer can't take down
        // tool calls. Missing params, non-object params, and wrong-typed
        // fields must all be swallowed gracefully — the in-flight request
        // still completes, and the junk is not buffered for the supervisor.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut q = inbound.lock().unwrap();
            q.push_back(log_notification(None));
            q.push_back(log_notification(Some(json!("not an object"))));
            q.push_back(log_notification(Some(json!({
                "level": 42, "logger": {"nested": true}, "data": [1, 2, 3]
            }))));
            q.push_back(initialize_response());
        }

        let transport = Box::new(ScriptedTransport::new(
            Arc::new(Mutex::new(Vec::new())),
            inbound,
        ));
        let mut session = SessionActor::new(&manifest(), transport);
        session
            .handshake()
            .await
            .expect("malformed server logs must not break the handshake");

        assert_eq!(
            session.server_logs_emitted, 3,
            "all three log lines are forwarded (with defaults), not dropped"
        );
        assert!(
            session.drain_notifications().is_empty(),
            "server logs are routed to tracing, never buffered for the supervisor"
        );
    }

    #[tokio::test]
    async fn server_log_flood_is_suppressed_past_cap() {
        // Spec: a chatty server gets at most SERVER_LOG_CAP forwarded log
        // lines per session, then a one-shot suppression notice. Without
        // the cap, a hostile peer could flood the operator's logs through
        // a single long-running tool call.
        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut q = inbound.lock().unwrap();
            for i in 0..(SERVER_LOG_CAP + 25) {
                q.push_back(log_notification(Some(json!({
                    "level": "info",
                    "logger": "spammy",
                    "data": format!("line {i}"),
                }))));
            }
            q.push_back(initialize_response());
        }

        let transport = Box::new(ScriptedTransport::new(
            Arc::new(Mutex::new(Vec::new())),
            inbound,
        ));
        let mut session = SessionActor::new(&manifest(), transport);
        session.handshake().await.expect("handshake");

        assert_eq!(
            session.server_logs_emitted, SERVER_LOG_CAP,
            "forwarding must stop exactly at the cap"
        );
        assert!(
            session.server_logs_suppressed,
            "one-shot suppression notice must have fired past the cap"
        );
    }

    #[test]
    fn server_log_control_chars_are_neutralized() {
        // A hostile server must not forge operator-looking log lines (`\n`)
        // or emit terminal escapes (ESC/CSI) through forwarded log fields.
        let forged = "ok\ninfo borgd: fake line\x1b[2Jend\r";
        let clean = neutralize_control_chars(forged);
        assert!(!clean.contains('\n') && !clean.contains('\x1b') && !clean.contains('\r'));
        assert_eq!(clean, "ok info borgd: fake line [2Jend ");
        // Tabs survive and clean input stays borrowed (no allocation).
        assert!(matches!(
            neutralize_control_chars("a\tb"),
            std::borrow::Cow::Borrowed("a\tb")
        ));
    }

    #[test]
    fn bounded_truncates_on_char_boundary() {
        // Server-supplied log fields are unbounded on the wire; the bound
        // must never split a multi-byte char (that would panic at the
        // slice). Multi-byte input truncated mid-char walks back safely.
        let s = "héllo wörld";
        for max in 0..=s.len() {
            let t = bounded(s, max);
            assert!(t.len() <= max, "len {} > max {max}", t.len());
            assert!(s.starts_with(t));
        }
        assert_eq!(bounded("short", 2048), "short");
    }
}
