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
use crate::transport::http::{check_url, check_url_syntactic, HttpPolicy};
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
pub async fn open(config: SseConfig, connect_timeout: Duration) -> Result<SseTransport> {
    let sse_url = check_url(&config.url, &config.policy).await?;
    let client = Client::builder()
        .timeout(config.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| McpError::Connect(format!("reqwest build failed: {e}")))?;

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
    let post_url = sse_url
        .join(&endpoint)
        .map_err(|e| McpError::Connect(format!("invalid endpoint url '{endpoint}': {e}")))?;
    // Re-check the resolved URL against policy — a hostile server cannot
    // redirect us internal. Syntactic check is sufficient here because the
    // SSE GET we already accepted resolves to the same hostname; we are
    // guarding against `endpoint` being rewritten to e.g. `localhost`.
    check_url_syntactic(post_url.as_str(), &config.policy)?;

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
