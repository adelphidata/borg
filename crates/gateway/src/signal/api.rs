use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::format::{chunk_with_styles, FormattedText, TextStyle};
use super::types::{JsonRpcRequest, JsonRpcResponse};
use crate::chunker;
use crate::circuit_breaker::CircuitBreaker;
use anyhow::{bail, Context, Result};
use reqwest::Client;

use borg_core::constants;

const MESSAGE_CHUNK_SIZE: usize = constants::SIGNAL_MESSAGE_CHUNK_SIZE;
const RPC_TIMEOUT: Duration = Duration::from_secs(constants::SIGNAL_RPC_TIMEOUT_SECS);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Circuit breaker thresholds for Signal API calls.
const CIRCUIT_FAILURE_THRESHOLD: u32 = 5;
const CIRCUIT_SUSPENSION_SECS: u64 = 60;

/// Monotonically increasing counter for JSON-RPC request IDs.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// A client for the signal-cli JSON-RPC daemon.
#[derive(Clone)]
pub struct SignalClient {
    client: Client,
    base_url: String,
    account: String,
    circuit_breaker: Arc<CircuitBreaker>,
}

impl SignalClient {
    pub fn new(base_url: &str, account: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(RPC_TIMEOUT)
                .build()
                .context("Failed to build Signal HTTP client")?,
            base_url: base_url.trim_end_matches('/').to_string(),
            account: account.to_string(),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                CIRCUIT_FAILURE_THRESHOLD,
                CIRCUIT_SUSPENSION_SECS,
            )),
        })
    }

    /// Returns the account phone number this client is configured for.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Returns the base URL of the signal-cli daemon.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Make a GET request for SSE streaming (reuses the configured HTTP client).
    pub async fn get_sse(&self, url: &str) -> Result<reqwest::Response, reqwest::Error> {
        // Build a client without the short RPC timeout for long-lived SSE connections
        let sse_client = Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();

        sse_client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
    }

    /// Health check — calls the `version` JSON-RPC method.
    /// Returns the daemon version string on success.
    pub async fn probe(&self) -> Result<String> {
        let resp = self
            .rpc_call("version", serde_json::json!({}))
            .await
            .context("Signal daemon probe failed")?;

        if let Some(result) = resp.result {
            if let Some(version) = result.get("version").and_then(|v| v.as_str()) {
                return Ok(version.to_string());
            }
            // Some versions return the version string directly
            if let Some(version) = result.as_str() {
                return Ok(version.to_string());
            }
            Ok(format!("{result}"))
        } else {
            bail!("Probe returned no result")
        }
    }

    /// Send a text message to a recipient (phone number or UUID).
    /// Automatically chunks text exceeding the limit.
    /// When `styles` is provided, includes Signal text style ranges in the RPC call.
    pub async fn send_message(
        &self,
        recipient: &str,
        text: &str,
        styles: Option<&[TextStyle]>,
    ) -> Result<()> {
        match styles {
            Some(style_list) if !style_list.is_empty() => {
                let formatted = FormattedText {
                    text: text.to_string(),
                    styles: style_list.to_vec(),
                };
                let chunks = chunk_with_styles(&formatted, MESSAGE_CHUNK_SIZE);
                for chunk in &chunks {
                    let text_styles: Vec<_> = chunk
                        .styles
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "start": s.start,
                                "length": s.length,
                                "style": s.style,
                            })
                        })
                        .collect();
                    let mut params = serde_json::json!({
                        "account": self.account,
                        "recipient": [recipient],
                        "message": chunk.text,
                    });
                    if !text_styles.is_empty() {
                        params["textStyle"] = serde_json::Value::Array(text_styles);
                    }
                    let resp = self.rpc_call("send", params).await?;
                    if let Some(ref err) = resp.error {
                        bail!("Signal send failed (code {}): {}", err.code, err.message);
                    }
                }
            }
            _ => {
                let chunks = chunker::chunk_text_nonempty(text, MESSAGE_CHUNK_SIZE);
                for chunk in &chunks {
                    let params = serde_json::json!({
                        "account": self.account,
                        "recipient": [recipient],
                        "message": chunk,
                    });
                    let resp = self.rpc_call("send", params).await?;
                    if let Some(ref err) = resp.error {
                        bail!("Signal send failed (code {}): {}", err.code, err.message);
                    }
                }
            }
        }
        Ok(())
    }

    /// Send a text message to a group.
    /// When `styles` is provided, includes Signal text style ranges in the RPC call.
    pub async fn send_group_message(
        &self,
        group_id: &str,
        text: &str,
        styles: Option<&[TextStyle]>,
    ) -> Result<()> {
        match styles {
            Some(style_list) if !style_list.is_empty() => {
                let formatted = FormattedText {
                    text: text.to_string(),
                    styles: style_list.to_vec(),
                };
                let chunks = chunk_with_styles(&formatted, MESSAGE_CHUNK_SIZE);
                for chunk in &chunks {
                    let text_styles: Vec<_> = chunk
                        .styles
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "start": s.start,
                                "length": s.length,
                                "style": s.style,
                            })
                        })
                        .collect();
                    let mut params = serde_json::json!({
                        "account": self.account,
                        "groupId": group_id,
                        "message": chunk.text,
                    });
                    if !text_styles.is_empty() {
                        params["textStyle"] = serde_json::Value::Array(text_styles);
                    }
                    let resp = self.rpc_call("send", params).await?;
                    if let Some(ref err) = resp.error {
                        bail!("Signal group send failed: {}", err.message);
                    }
                }
            }
            _ => {
                let chunks = chunker::chunk_text_nonempty(text, MESSAGE_CHUNK_SIZE);
                for chunk in &chunks {
                    let params = serde_json::json!({
                        "account": self.account,
                        "groupId": group_id,
                        "message": chunk,
                    });
                    let resp = self.rpc_call("send", params).await?;
                    if let Some(ref err) = resp.error {
                        bail!("Signal group send failed: {}", err.message);
                    }
                }
            }
        }
        Ok(())
    }

    /// Download an attachment from the signal-cli daemon by ID.
    ///
    /// Returns the raw bytes. Rejects attachments exceeding `max_bytes`.
    pub async fn download_attachment(
        &self,
        attachment_id: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/api/v1/attachments/{attachment_id}", self.base_url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to download Signal attachment")?;

        if !resp.status().is_success() {
            bail!("Signal attachment download failed ({})", resp.status());
        }

        // Check Content-Length before reading body to avoid OOM
        if let Some(len) = resp.content_length() {
            if len > max_bytes {
                bail!("Signal attachment too large ({len} bytes, max {max_bytes})");
            }
        }

        let bytes = resp
            .bytes()
            .await
            .context("Failed to read attachment body")?;

        if bytes.len() as u64 > max_bytes {
            bail!(
                "Signal attachment too large ({} bytes, max {max_bytes})",
                bytes.len()
            );
        }

        Ok(bytes.to_vec())
    }

    /// Send a typing indicator to a recipient or group.
    pub async fn send_typing(&self, recipient: Option<&str>, group_id: Option<&str>) -> Result<()> {
        let mut params = serde_json::json!({
            "account": self.account,
        });
        if let Some(r) = recipient {
            params["recipient"] = serde_json::json!([r]);
        }
        if let Some(g) = group_id {
            params["groupId"] = serde_json::Value::String(g.to_string());
        }
        // Best-effort — don't fail the flow on typing indicator errors
        if let Err(e) = self.rpc_call("sendTyping", params).await {
            tracing::warn!("[signal] sendTyping failed: {e}");
        }
        Ok(())
    }

    /// Send read receipts for the given message timestamps.
    pub async fn send_read_receipt(&self, recipient: &str, timestamps: &[i64]) -> Result<()> {
        if timestamps.is_empty() {
            return Ok(());
        }
        let params = serde_json::json!({
            "account": self.account,
            "recipient": recipient,
            "targetTimestamp": timestamps,
            "type": "read",
        });
        // Best-effort
        if let Err(e) = self.rpc_call("sendReceipt", params).await {
            tracing::warn!("[signal] sendReceipt failed: {e}");
        }
        Ok(())
    }

    /// Send or remove a reaction on a message.
    pub async fn send_reaction(
        &self,
        recipient: &str,
        group_id: Option<&str>,
        emoji: &str,
        target_author: &str,
        target_timestamp: i64,
        remove: bool,
    ) -> Result<()> {
        let mut params = serde_json::json!({
            "account": self.account,
            "emoji": emoji,
            "targetAuthor": target_author,
            "targetTimestamp": target_timestamp,
            "remove": remove,
        });
        if let Some(g) = group_id {
            params["groupId"] = serde_json::Value::String(g.to_string());
        } else {
            params["recipient"] = serde_json::json!([recipient]);
        }
        let resp = self.rpc_call("sendReaction", params).await?;
        if let Some(ref err) = resp.error {
            bail!("Signal reaction failed: {}", err.message);
        }
        Ok(())
    }

    /// Make a JSON-RPC 2.0 call to the signal-cli daemon.
    async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<JsonRpcResponse> {
        if self.circuit_breaker.is_open() {
            bail!("Signal circuit breaker is open — skipping {method}");
        }

        let url = format!("{}/api/v1/rpc", self.base_url);
        let request = JsonRpcRequest::new(method, params);

        let result = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .context(format!("Signal RPC {method} request failed"));

        match result {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    // Trip circuit breaker on any persistent HTTP error from local daemon
                    self.circuit_breaker.record_failure();
                    let body = response.text().await.unwrap_or_default();
                    bail!("Signal RPC {method} returned HTTP {status}: {body}");
                }
                self.circuit_breaker.record_success();
                let rpc_resp: JsonRpcResponse = response
                    .json()
                    .await
                    .context(format!("Failed to parse Signal RPC {method} response"))?;
                Ok(rpc_resp)
            }
            Err(e) => {
                self.circuit_breaker.record_failure();
                Err(e)
            }
        }
    }
}

/// Generate a unique JSON-RPC request ID using an atomic counter.
pub(crate) fn next_request_id() -> String {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_construction() {
        let client = SignalClient::new("http://localhost:8080", "+15559876543").unwrap();
        assert_eq!(client.base_url(), "http://localhost:8080");
        assert_eq!(client.account(), "+15559876543");
    }

    #[test]
    fn client_strips_trailing_slash() {
        let client = SignalClient::new("http://localhost:8080/", "+15559876543").unwrap();
        assert_eq!(client.base_url(), "http://localhost:8080");
    }

    #[test]
    fn rpc_url_construction() {
        let client = SignalClient::new("http://localhost:8080", "+15559876543").unwrap();
        let url = format!("{}/api/v1/rpc", client.base_url());
        assert_eq!(url, "http://localhost:8080/api/v1/rpc");
    }

    #[test]
    fn chunking_short_message() {
        let chunks = chunker::chunk_text_nonempty("Hello", MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello");
    }

    #[test]
    fn chunking_long_message() {
        let long = "a".repeat(MESSAGE_CHUNK_SIZE + 100);
        let chunks = chunker::chunk_text_nonempty(&long, MESSAGE_CHUNK_SIZE);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MESSAGE_CHUNK_SIZE);
        }
    }

    #[test]
    fn request_ids_are_unique() {
        let id1 = next_request_id();
        let id2 = next_request_id();
        assert_ne!(id1, id2);
    }
}
