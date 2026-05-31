//! SSE flavour of MCP HTTP transport.
//!
//! The server holds open a long-lived SSE stream on which it pushes
//! responses and notifications; the client POSTs requests to a sibling
//! endpoint. The SSE `endpoint` event carries the POST URL (per MCP spec).
//!
//! Same URL policy as [`super::http`] applies.

use super::Transport;
use crate::errors::{McpError, Result};
use crate::protocol::Message;
use crate::transport::http::{build_client, resolve_endpoint, HttpPolicy};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

/// Config for an SSE MCP endpoint.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// Server name (for diagnostics).
    pub server_name: String,
    /// SSE GET URL.
    pub url: String,
    /// Headers to send on both GET (SSE) and POST (requests).
    pub headers: HashMap<String, String>,
    /// Per-call timeout for the POST side. The SSE side is long-lived.
    pub timeout: Duration,
    /// Policy gates.
    pub policy: HttpPolicy,
    /// Optional client certificate for mutual TLS. Loaded at connect time.
    pub client_identity: Option<crate::manifest::ClientIdentity>,
}

/// Connected SSE transport.
///
/// One background task pumps inbound events into a channel; `recv` drains
/// that channel. `send` POSTs to the per-session endpoint advertised by the
/// server's first `endpoint` event.
pub struct SseTransport {
    server_name: String,
    client: Client,
    post_url: Url,
    headers: HashMap<String, String>,
    inbox: mpsc::Receiver<Message>,
    /// Set by the pump when it exits due to an error. `recv` checks this so
    /// pump errors surface as `TransportClosed` rather than the silent
    /// `Ok(None)` you'd get from a clean `mpsc::Receiver::recv` after the
    /// sender drops.
    pump_error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pump: Option<JoinHandle<()>>,
}

impl SseTransport {
    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Open an SSE transport. Waits up to `connect_timeout` for the server's
/// `endpoint` event (which carries the POST URL).
///
/// DNS is resolved once for the SSE URL up-front and pinned into the GET
/// client; the POST URL advertised by the server is resolved separately
/// (full async policy re-check) and pinned into the POST client so
/// rebinding is closed on both sides.
pub async fn open(config: SseConfig, connect_timeout: Duration) -> Result<SseTransport> {
    let sse_resolved = resolve_endpoint(&config.url, &config.policy).await?;
    let sse_url = sse_resolved.url.clone();
    let client = build_client(
        config.timeout,
        &sse_resolved.pins,
        config.client_identity.as_ref(),
    )?;

    let mut req = client
        .get(sse_url.clone())
        .header(reqwest::header::ACCEPT, "text/event-stream");
    for (k, v) in &config.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| {
        McpError::Connect(format!("{}: sse connect failed: {e}", config.server_name))
    })?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(McpError::Auth(format!(
            "{}: SSE HTTP {}",
            config.server_name,
            status.as_u16()
        )));
    }
    if !status.is_success() {
        return Err(McpError::Connect(format!(
            "{}: SSE HTTP {}",
            config.server_name,
            status.as_u16()
        )));
    }

    let mut stream = resp.bytes_stream().eventsource();
    let (tx, rx) = mpsc::channel::<Message>(64);

    // Wait for the first `endpoint` event so we know where to POST.
    let endpoint_fut = async {
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) if ev.event == "endpoint" => {
                    let s: String = ev.data;
                    return Ok(s);
                }
                Ok(_) => continue,
                Err(e) => {
                    return Err(McpError::Connect(format!("sse stream error: {e}")));
                }
            }
        }
        Err(McpError::Connect(
            "sse closed before endpoint event".to_string(),
        ))
    };
    let endpoint: String = match tokio::time::timeout(connect_timeout, endpoint_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(McpError::Timeout {
                ms: connect_timeout.as_millis() as u64,
            })
        }
    };

    // The endpoint event may be either an absolute URL or a path relative to
    // the SSE URL; resolve relative against the SSE URL.
    let post_url_parsed = sse_url
        .join(&endpoint)
        .map_err(|e| McpError::Connect(format!("invalid endpoint url '{endpoint}': {e}")))?;
    // Full async re-check against policy — a hostile server cannot redirect
    // us at an internal IP via the `endpoint` event, even if the POST URL
    // host differs from the SSE host. Resolution is recorded so we can pin
    // the POST host into the POST client too.
    let post_resolved = resolve_endpoint(post_url_parsed.as_str(), &config.policy).await?;
    let post_url = post_resolved.url;

    // Build a second client that's pinned to both the SSE host (so any
    // ambient code path that reuses this client for the SSE host still
    // can't be rebound) and the POST host. reqwest's `resolve_to_addrs`
    // is per-domain and accumulates across calls, so combining the two
    // pin lists is safe.
    let mut combined_pins = sse_resolved.pins.clone();
    combined_pins.extend(post_resolved.pins);
    let client = build_client(
        config.timeout,
        &combined_pins,
        config.client_identity.as_ref(),
    )?;

    // Spawn the pump for the remaining lifetime of the stream.
    let server_name_pump = config.server_name.clone();
    let pump_error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let pump_error_inner = pump_error.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) if ev.event == "message" || ev.event.is_empty() => {
                    match crate::protocol::decode_line(ev.data.as_bytes()) {
                        Ok(msg) => {
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                server = %server_name_pump,
                                error = %e,
                                "mcp sse decode failed"
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        server = %server_name_pump,
                        error = %e,
                        "mcp sse stream error"
                    );
                    if let Ok(mut slot) = pump_error_inner.lock() {
                        *slot = Some(format!("{e}"));
                    }
                    break;
                }
            }
        }
    });

    Ok(SseTransport {
        server_name: config.server_name,
        client,
        post_url,
        headers: config.headers,
        inbox: rx,
        pump_error,
        pump: Some(pump),
    })
}

#[async_trait]
impl Transport for SseTransport {
    async fn send(&mut self, msg: Value) -> Result<()> {
        let mut req = self
            .client
            .post(self.post_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(&msg).send().await.map_err(|e| {
            McpError::TransportClosed(format!("{}: sse post failed: {e}", self.server_name))
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(McpError::Auth(format!(
                "{}: SSE POST HTTP {}",
                self.server_name,
                status.as_u16()
            )));
        }
        if !status.is_success() && status != reqwest::StatusCode::ACCEPTED {
            return Err(McpError::TransportClosed(format!(
                "{}: SSE POST HTTP {}",
                self.server_name,
                status.as_u16()
            )));
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        match self.inbox.recv().await {
            Some(m) => Ok(Some(m)),
            None => {
                // mpsc::Receiver::recv returns None when the sender is
                // dropped. That happens (1) on clean shutdown or (2) when
                // the pump exited due to an error. Distinguish via the
                // shared error slot.
                if let Some(reason) = self.pump_error.lock().ok().and_then(|g| g.clone()) {
                    return Err(McpError::TransportClosed(format!(
                        "{}: sse stream error: {reason}",
                        self.server_name
                    )));
                }
                Ok(None)
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(p) = self.pump.take() {
            p.abort();
        }
        Ok(())
    }
}
