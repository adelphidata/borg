//! JSON-RPC transport for the ACP stdio bridge: envelope types, the
//! line-delimited frame writers, the client-callback plumbing, and the
//! inbound dispatch loop (`run_transport`).

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::daemon_client::AcpFsClient;

use super::handlers::handle_request;
use super::{AcpDaemon, AcpRequestError, AcpState, ClientCapabilities};

pub(super) type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

pub(super) type JsonRpcResponseSender = oneshot::Sender<Result<Value, String>>;
pub(super) type PendingJsonRpcRequests = Arc<Mutex<HashMap<String, JsonRpcResponseSender>>>;

#[derive(Clone)]
pub(super) struct JsonRpcCallbackClient {
    pub(super) writer: SharedWriter,
    pub(super) pending: PendingJsonRpcRequests,
    pub(super) next_id: Arc<AtomicU64>,
    pub(super) capabilities: Arc<StdMutex<ClientCapabilities>>,
}

impl JsonRpcCallbackClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = Value::Number(serde_json::Number::from(id_num));
        let key = rpc_id_key(&id);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(key, tx);
        }
        write_json(
            &self.writer,
            &serde_json::to_value(JsonRpcOutboundRequest {
                jsonrpc: "2.0",
                id,
                method: method.to_string(),
                params,
            })?,
        )
        .await?;
        rx.await
            .map_err(|_| anyhow!("ACP callback response channel closed"))?
            .map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl AcpFsClient for JsonRpcCallbackClient {
    async fn read_text_file(&self, path: &str) -> Result<String> {
        let result = self
            .request("fs/read_text_file", json!({ "path": path }))
            .await?;
        let content = result
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("ACP fs/read_text_file missing string content"))?;
        Ok(content.to_string())
    }

    async fn write_text_file(&self, path: &str, content: &str) -> Result<()> {
        // fs/write returns an ack payload we don't consume; only the error matters.
        let _ = self
            .request(
                "fs/write_text_file",
                json!({ "path": path, "content": content }),
            )
            .await?;
        Ok(())
    }

    fn capabilities(&self) -> protocols::session::AcpFsCapabilities {
        let caps = self
            .capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        protocols::session::AcpFsCapabilities {
            read_text_file: caps.read_text_file,
            write_text_file: caps.write_text_file,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct JsonRpcRequest {
    pub(super) jsonrpc: String,
    #[serde(default)]
    pub(super) id: Option<Value>,
    pub(super) method: String,
    #[serde(default)]
    pub(super) params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponseEnvelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<IncomingJsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct IncomingJsonRpcError {
    message: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcOutboundRequest {
    jsonrpc: &'static str,
    id: Value,
    method: String,
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorResponse<'a> {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcError<'a>,
}

#[derive(Debug, Serialize)]
struct JsonRpcSuccessResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError<'a> {
    code: i64,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

enum InboundTransportMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponseEnvelope),
    ParseError(String),
}

pub(super) async fn run_transport<D, R, W>(daemon: D, reader: R, writer: W) -> Result<i32>
where
    D: AcpDaemon,
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
    let pending = Arc::new(Mutex::new(HashMap::<
        String,
        oneshot::Sender<Result<Value, String>>,
    >::new()));
    let callback_client = JsonRpcCallbackClient {
        writer: writer.clone(),
        pending: pending.clone(),
        next_id: Arc::new(AtomicU64::new(1)),
        capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
    };
    let state = AcpState {
        daemon,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        client_capabilities: callback_client.capabilities.clone(),
        onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    };
    let daemon_failed = state.daemon_failed.clone();
    let shutdown = state.shutdown.clone();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundTransportMessage>(64);
    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(&line) {
                        Ok(value) => value,
                        Err(e) => {
                            if inbound_tx
                                .send(InboundTransportMessage::ParseError(e.to_string()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    if value.get("method").is_some() {
                        match serde_json::from_value::<JsonRpcRequest>(value) {
                            Ok(req) => {
                                if inbound_tx
                                    .send(InboundTransportMessage::Request(req))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                if inbound_tx
                                    .send(InboundTransportMessage::ParseError(e.to_string()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    } else if inbound_tx
                        .send(
                            serde_json::from_value::<JsonRpcResponseEnvelope>(value)
                                .map(InboundTransportMessage::Response)
                                .unwrap_or_else(|e| {
                                    InboundTransportMessage::ParseError(e.to_string())
                                }),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    // Receiver may already be gone during teardown; drop is safe.
                    let _ = inbound_tx
                        .send(InboundTransportMessage::ParseError(e.to_string()))
                        .await;
                    break;
                }
            }
        }
    });
    let mut request_tasks = tokio::task::JoinSet::new();
    /// Soft cap on concurrently-in-flight ACP requests so a misbehaving
    /// client can't pile up handlers and exhaust memory. Plenty for any
    /// real IDE workload — ACP turns are sequential per session.
    const MAX_INFLIGHT_REQUESTS: usize = 64;

    loop {
        let msg = tokio::select! {
            msg = inbound_rx.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            _ = shutdown.notified() => break,
        };
        // Reap any completed task slots before deciding whether the
        // dispatcher is at capacity. Surface join errors so a panicked
        // handler doesn't get swallowed silently.
        while let Some(result) = request_tasks.try_join_next() {
            if let Err(e) = result {
                tracing::error!("ACP request task ended with error: {e}");
            }
        }
        match msg {
            InboundTransportMessage::Request(req) => {
                if req.jsonrpc != "2.0" {
                    send_error(
                        &writer,
                        req.id.unwrap_or(Value::Null),
                        -32600,
                        "Invalid Request",
                        Some(json!("jsonrpc must be 2.0")),
                    )
                    .await?;
                    continue;
                }
                if request_tasks.len() >= MAX_INFLIGHT_REQUESTS {
                    if let Some(id) = req.id.clone() {
                        send_error(
                            &writer,
                            id,
                            -32000,
                            "Server busy",
                            Some(json!({
                                "kind": "too_many_inflight",
                                "max": MAX_INFLIGHT_REQUESTS,
                            })),
                        )
                        .await?;
                    } else {
                        tracing::warn!("dropped ACP notification {} — at inflight cap", req.method);
                    }
                    continue;
                }
                let writer = writer.clone();
                let state = state.clone();
                let callback_client = callback_client.clone();
                let req_id = req.id.clone();
                let shutdown_signal = state.shutdown.clone();
                let daemon_failed_flag = state.daemon_failed.clone();
                request_tasks.spawn(async move {
                    if let Err(e) =
                        handle_request(state, req, writer.clone(), callback_client).await
                    {
                        let mapped = AcpRequestError::from_anyhow(&e);
                        match req_id {
                            Some(id) => {
                                if let Err(send_err) = send_kind_error(&writer, id, &mapped).await {
                                    tracing::error!("acp send_error failed: {send_err:#}");
                                }
                            }
                            None => {
                                // Notification — JSON-RPC says don't reply,
                                // even on error. Log and drop.
                                tracing::warn!("acp notification handler errored: {e:#}");
                            }
                        }
                        // After the error frame flushes, tear the inbound
                        // loop down so the IDE sees process exit non-zero
                        // instead of a hung agent.
                        if daemon_failed_flag.load(Ordering::SeqCst) {
                            shutdown_signal.notify_one();
                        }
                    }
                });
            }
            InboundTransportMessage::Response(response) => {
                if let Some(id) = response.id {
                    let key = rpc_id_key(&id);
                    let tx = {
                        let mut pending = pending.lock().await;
                        pending.remove(&key)
                    };
                    if let Some(tx) = tx {
                        // Waiting caller may have dropped the oneshot; nothing to do.
                        let _ = tx.send(match (response.result, response.error) {
                            (Some(result), None) => Ok(result),
                            (_, Some(error)) => Err(error.message),
                            _ => Err("invalid ACP callback response envelope".to_string()),
                        });
                    }
                }
            }
            InboundTransportMessage::ParseError(e) => {
                send_error(
                    &writer,
                    Value::Null,
                    -32700,
                    "Parse error",
                    Some(json!(e.to_string())),
                )
                .await?;
            }
        }
    }

    while let Some(result) = request_tasks.join_next().await {
        if let Err(e) = result {
            tracing::error!("ACP request task join failed: {e}");
        }
    }

    Ok(if daemon_failed.load(Ordering::SeqCst) {
        1
    } else {
        0
    })
}

fn rpc_id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

pub(super) async fn send_result(
    writer: &SharedWriter,
    id: Option<Value>,
    result: Value,
) -> Result<()> {
    let response = JsonRpcSuccessResponse {
        jsonrpc: "2.0",
        id: id.unwrap_or(Value::Null),
        result,
    };
    write_json(writer, &serde_json::to_value(response)?).await
}

pub(super) async fn send_error(
    writer: &SharedWriter,
    id: Value,
    code: i64,
    message: &'static str,
    data: Option<Value>,
) -> Result<()> {
    let response = JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message,
            data,
        },
    };
    write_json(writer, &serde_json::to_value(response)?).await
}

async fn send_kind_error(writer: &SharedWriter, id: Value, err: &AcpRequestError) -> Result<()> {
    send_error(writer, id, err.code, err.message, err.data.clone()).await
}

pub(super) async fn send_notification(
    writer: &SharedWriter,
    method: &str,
    params: Value,
) -> Result<()> {
    let notification = JsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    };
    write_json(writer, &serde_json::to_value(notification)?).await
}

async fn write_json(writer: &SharedWriter, value: &Value) -> Result<()> {
    let line = serde_json::to_vec(value)?;
    let mut writer = writer.lock().await;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
