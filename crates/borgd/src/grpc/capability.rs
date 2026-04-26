//! Capability bidi-stream service + the daemon's capability router.
//!
//! Each connected client opens a single `Channel` bidi-stream and sends an
//! initial `Advertise` message listing the capabilities it can fulfill on
//! the client's host (terminal.exec, clipboard.read, file.pick, notify, …).
//! The daemon registers that client and may push `Invoke` requests; the
//! client replies with `InvokeResult` carrying the same `invocation_id`.
//!
//! The [`CapabilityRouter`] is the in-memory state used by the agent's tool
//! layer to ask "is anyone offering capability X?" and to dispatch one
//! invocation. When no client offers a capability, the router applies the
//! routing rule from `borg-proto::capabilities` (HostOnly / Fallback /
//! Required) — see [`borg_proto::capabilities::Routing`].

use async_trait::async_trait;
use borg_proto::capabilities::{routing, Routing};
use borg_proto::capability::{
    capability_server::Capability, client_message::Kind as CKind, server_message::Kind as SKind,
    AdvertiseAck, ClientMessage, Invoke, ServerMessage,
};
use futures_util::{Stream, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status as TStatus, Streaming};
use uuid::Uuid;

/// Outcome of a capability invocation.
#[derive(Debug, Clone)]
pub enum InvocationResult {
    /// Client returned a JSON-encoded result string.
    Ok(String),
    /// Client returned an error string.
    ClientError(String),
    /// No client advertises this capability and there's no host fallback.
    Unavailable,
    /// Capability name is unknown to the daemon.
    Unknown,
    /// Client disconnected before responding.
    Disconnected,
}

/// One connected client.
struct ClientEntry {
    /// Human-readable label for logs/admin debugging.
    #[allow(dead_code)]
    label: String,
    capabilities: Vec<String>,
    /// Outbound channel for `Invoke` messages.
    invoke_tx: mpsc::Sender<ServerMessage>,
}

/// Daemon-wide state for client capabilities + in-flight invocations.
pub struct CapabilityRouter {
    next_client_id: AtomicU64,
    clients: Mutex<HashMap<u64, ClientEntry>>,
    pending: Mutex<HashMap<String, oneshot::Sender<InvocationResult>>>,
}

impl CapabilityRouter {
    /// New empty router.
    pub fn new() -> Self {
        Self {
            next_client_id: AtomicU64::new(1),
            clients: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn register(
        &self,
        label: String,
        capabilities: Vec<String>,
        invoke_tx: mpsc::Sender<ServerMessage>,
    ) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let mut g = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert(
            id,
            ClientEntry {
                label,
                capabilities,
                invoke_tx,
            },
        );
        id
    }

    fn deregister(&self, client_id: u64) {
        let mut g = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.remove(&client_id);
    }

    /// Pick the first registered client offering `capability`. Future work:
    /// score by liveness / round-robin / preference.
    fn pick_client(&self, capability: &str) -> Option<(u64, mpsc::Sender<ServerMessage>)> {
        let g = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.iter().find_map(|(id, c)| {
            c.capabilities
                .iter()
                .any(|cap| cap == capability)
                .then(|| (*id, c.invoke_tx.clone()))
        })
    }

    /// Invoke a capability and await the result. Routing:
    /// - HostOnly / unknown → return Unknown so the agent dispatches the
    ///   built-in tool itself (this router is only consulted for capability-
    ///   routed tool calls in the first place).
    /// - ClientWithHostFallback: if no client advertises it, return
    ///   `Unavailable` and let the caller substitute the host built-in.
    /// - ClientRequired: if no client advertises it, return `Unavailable`.
    pub async fn invoke(&self, capability: &str, args_json: String) -> InvocationResult {
        let rule = match routing(capability) {
            Some(r) => r,
            None => return InvocationResult::Unknown,
        };
        if matches!(rule, Routing::HostOnly) {
            // The agent should never route a host-only capability through us.
            return InvocationResult::Unknown;
        }
        let Some((_client_id, tx)) = self.pick_client(capability) else {
            return InvocationResult::Unavailable;
        };

        let invocation_id = Uuid::new_v4().to_string();
        let (result_tx, result_rx) = oneshot::channel();
        {
            let mut p = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            p.insert(invocation_id.clone(), result_tx);
        }

        let send = ServerMessage {
            kind: Some(SKind::Invoke(Invoke {
                invocation_id: invocation_id.clone(),
                capability: capability.to_string(),
                args_json,
            })),
        };
        if tx.send(send).await.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&invocation_id);
            return InvocationResult::Disconnected;
        }
        result_rx.await.unwrap_or(InvocationResult::Disconnected)
    }

    fn deliver(&self, invocation_id: &str, result: InvocationResult) {
        let tx = {
            let mut p = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            p.remove(invocation_id)
        };
        match tx {
            Some(tx) => {
                let _ = tx.send(result);
            }
            None => tracing::warn!(
                invocation_id,
                "received InvokeResult for unknown invocation_id"
            ),
        }
    }

    /// Snapshot of currently advertised capabilities across all clients
    /// (deduplicated). Used by tests + admin debug.
    pub fn advertised(&self) -> Vec<String> {
        let g = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<String> = g
            .values()
            .flat_map(|c| c.capabilities.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Number of currently connected clients (admin/debug).
    pub fn client_count(&self) -> usize {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Default for CapabilityRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// gRPC service implementation.
pub struct CapabilitySvc {
    router: Arc<CapabilityRouter>,
}

impl CapabilitySvc {
    /// Construct a service against `router`.
    pub fn new(router: Arc<CapabilityRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl Capability for CapabilitySvc {
    type ChannelStream =
        Pin<Box<dyn Stream<Item = Result<ServerMessage, TStatus>> + Send + 'static>>;

    async fn channel(
        &self,
        req: Request<Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ChannelStream>, TStatus> {
        let mut inbound = req.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<ServerMessage>(64);
        let (forward_tx, forward_rx) = mpsc::channel::<Result<ServerMessage, TStatus>>(64);

        // Wait for the initial Advertise (with timeout) so we can register
        // the client before processing further messages.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.next())
            .await
            .map_err(|_| TStatus::deadline_exceeded("client did not advertise within 5s"))?;
        let first = first
            .ok_or_else(|| TStatus::invalid_argument("client closed before advertising"))?
            .map_err(|e| TStatus::internal(format!("inbound error: {e}")))?;
        let advertise = match first.kind {
            Some(CKind::Advertise(a)) => a,
            _ => return Err(TStatus::invalid_argument("first message must be Advertise")),
        };

        let client_id = self.router.register(
            advertise.client_label,
            advertise.capabilities,
            out_tx.clone(),
        );

        // Send AdvertiseAck.
        let _ = out_tx
            .send(ServerMessage {
                kind: Some(SKind::AdvertiseAck(AdvertiseAck { client_id })),
            })
            .await;

        // Spawn the inbound→router loop.
        let router = self.router.clone();
        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(ClientMessage {
                        kind: Some(CKind::Result(r)),
                    }) => {
                        let result = if !r.error.is_empty() {
                            InvocationResult::ClientError(r.error)
                        } else {
                            InvocationResult::Ok(r.result_json)
                        };
                        router.deliver(&r.invocation_id, result);
                    }
                    Ok(ClientMessage {
                        kind: Some(CKind::Advertise(_)),
                    }) => {
                        tracing::warn!(client_id, "ignoring re-advertise after registration");
                    }
                    Ok(ClientMessage { kind: None }) => {}
                    Err(e) => {
                        tracing::warn!(client_id, error = %e, "capability inbound errored");
                        break;
                    }
                }
            }
            router.deregister(client_id);
        });

        // Spawn the outbound forwarder (server → client).
        tokio::spawn(async move {
            let mut rx = out_rx;
            while let Some(m) = rx.recv().await {
                if forward_tx.send(Ok(m)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(forward_rx))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borg_proto::capabilities::{CLIPBOARD_READ, HOST_RUN_SHELL, TERMINAL_EXEC};

    #[tokio::test]
    async fn invoke_with_no_advertising_client_returns_unavailable() {
        // Real failure mode: returning `Ok("")` here would cause silent
        // success when the agent expected a real clipboard read.
        let r = CapabilityRouter::new();
        match r.invoke(CLIPBOARD_READ, "{}".into()).await {
            InvocationResult::Unavailable => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_routes_to_advertising_client_and_back() {
        // End-to-end: register a fake client, invoke clipboard.read, deliver
        // a result via the router, assert the `invoke()` future resolves.
        let r = Arc::new(CapabilityRouter::new());
        let (tx, mut rx) = mpsc::channel::<ServerMessage>(8);
        let _id = r.register("test-tui".into(), vec![CLIPBOARD_READ.into()], tx);

        let r2 = r.clone();
        let invoke_handle =
            tokio::spawn(async move { r2.invoke(CLIPBOARD_READ, "{}".into()).await });

        // Receive the Invoke, send back a result.
        let msg = rx.recv().await.expect("invoke msg");
        let invocation_id = match msg.kind {
            Some(SKind::Invoke(i)) => i.invocation_id,
            other => panic!("expected Invoke, got {other:?}"),
        };
        r.deliver(&invocation_id, InvocationResult::Ok("\"hello\"".into()));

        match invoke_handle.await.expect("join") {
            InvocationResult::Ok(s) => assert_eq!(s, "\"hello\""),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn host_only_capability_is_never_routed_through_router() {
        // Real failure mode: routing a host-only capability through the
        // router would deadlock the agent waiting for a non-existent client.
        let r = CapabilityRouter::new();
        match r.invoke(HOST_RUN_SHELL, "{}".into()).await {
            InvocationResult::Unknown => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_exec_with_no_client_returns_unavailable_so_caller_can_fallback() {
        // The caller (capability bridge) translates Unavailable into a host
        // run_shell call for terminal.exec specifically — the router itself
        // does not perform the fallback.
        let r = CapabilityRouter::new();
        match r.invoke(TERMINAL_EXEC, "{}".into()).await {
            InvocationResult::Unavailable => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
