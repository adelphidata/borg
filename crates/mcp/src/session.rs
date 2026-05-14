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
    InitializeResult, Message, Request, ToolCallResult, ToolsListResult,
};
use crate::transport::Transport;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;

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
        }
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
        // wrapping_add is conservative — at 1M calls/sec wrap takes ~580k
        // years. The earlier `.max(1)` was a tell that we'd thought about
        // wrap but only half-covered it; for now we accept the unreachable
        // wrap and drop the misleading floor.
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
                    // Notification — stash for the supervisor (bounded).
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
}
