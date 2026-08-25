use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::format::{chunk_with_styles, markdown_to_signal};
use super::types::{JsonRpcRequest, JsonRpcResponse};
use crate::resilience::circuit_breaker::CircuitBreaker;
use anyhow::{bail, Context, Result};
use reqwest::Client;

use cortex::constants;

use crate::constants::{
    SIGNAL_CIRCUIT_FAILURE_THRESHOLD, SIGNAL_CIRCUIT_SUSPENSION_SECS, SIGNAL_HTTP_CONNECT_TIMEOUT,
};

const MESSAGE_CHUNK_SIZE: usize = constants::SIGNAL_MESSAGE_CHUNK_SIZE;
const RPC_TIMEOUT: Duration = Duration::from_secs(constants::SIGNAL_RPC_TIMEOUT_SECS);

/// Monotonically increasing counter for JSON-RPC request IDs.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Where an outbound message is addressed: a 1:1 recipient or a group.
enum SendTarget<'a> {
    Recipient(&'a str),
    Group(&'a str),
}

/// A client for the signal-cli JSON-RPC daemon.
#[derive(Clone)]
pub struct SignalClient {
    client: Client,
    sse_client: Client,
    base_url: String,
    account: String,
    circuit_breaker: Arc<CircuitBreaker>,
}

impl SignalClient {
    pub fn new(base_url: &str, account: &str) -> Result<Self> {
        let sse_client = Client::builder()
            .connect_timeout(SIGNAL_HTTP_CONNECT_TIMEOUT)
            .build()
            .context("build Signal SSE client")?;
        Ok(Self {
            client: crate::resilience::http_retry::build_channel_client(
                "Signal",
                RPC_TIMEOUT,
                Some(SIGNAL_HTTP_CONNECT_TIMEOUT),
            )?,
            sse_client,
            base_url: base_url.trim_end_matches('/').to_string(),
            account: account.to_string(),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                SIGNAL_CIRCUIT_FAILURE_THRESHOLD,
                SIGNAL_CIRCUIT_SUSPENSION_SECS,
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

    /// Make a GET request for SSE streaming. Reuses the long-lived
    /// `sse_client` built at construction so reconnects don't pay the
    /// TLS / connection-pool setup cost on every retry.
    pub async fn get_sse(&self, url: &str) -> Result<reqwest::Response, reqwest::Error> {
        self.sse_client
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
    ///
    /// `text` is treated as markdown: it is converted to Signal's native text
    /// style ranges (bold/italic/monospace/strikethrough/spoiler) and chunked
    /// to fit the per-message size limit. See [`Self::send_to`].
    pub async fn send_message(&self, recipient: &str, text: &str) -> Result<()> {
        self.send_to(SendTarget::Recipient(recipient), text).await
    }

    /// Send a text message to a group. `text` is rendered from markdown like
    /// [`Self::send_message`].
    pub async fn send_group_message(&self, group_id: &str, text: &str) -> Result<()> {
        self.send_to(SendTarget::Group(group_id), text).await
    }

    /// Render `text` from markdown into Signal style ranges, chunk it, and send
    /// each chunk to `target`. Empty chunks are skipped so we never POST a blank
    /// message; the `textStyle` array is included only when a chunk carries styles.
    async fn send_to(&self, target: SendTarget<'_>, text: &str) -> Result<()> {
        let formatted = markdown_to_signal(text);
        let chunks = chunk_with_styles(&formatted, MESSAGE_CHUNK_SIZE);
        for chunk in &chunks {
            if chunk.text.is_empty() {
                continue;
            }
            let mut params = serde_json::json!({
                "account": self.account,
                "message": chunk.text,
            });
            match target {
                SendTarget::Recipient(r) => params["recipient"] = serde_json::json!([r]),
                SendTarget::Group(g) => params["groupId"] = serde_json::json!(g),
            }
            if !chunk.styles.is_empty() {
                // TextStyle derives Serialize with the exact wire shape
                // ({start, length, style: "SCREAMING_SNAKE_CASE"}).
                params["textStyle"] = serde_json::to_value(&chunk.styles)?;
            }
            let resp = self.rpc_call("send", params).await?;
            if let Some(ref err) = resp.error {
                bail!("Signal send failed (code {}): {}", err.code, err.message);
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

        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk while streaming.
        cortex::media::read_body_capped(resp, max_bytes, "Signal attachment").await
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
    fn request_ids_are_unique() {
        let id1 = next_request_id();
        let id2 = next_request_id();
        assert_ne!(id1, id2);
    }

    // ── Mock signal-cli daemon over a raw TCP socket ──

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Read a full HTTP/1.1 request (headers + Content-Length body) from `sock`.
    /// Returns the decoded request body as a string.
    async fn read_request_body(sock: &mut TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let content_len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let body_start = pos + 4;
                while buf.len() - body_start < content_len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                return String::from_utf8_lossy(&buf[body_start..]).to_string();
            }
        }
        String::new()
    }

    /// Canned success response for RPC methods whose result payload is unused.
    const OK_RESULT: &str = r#"{"jsonrpc":"2.0","result":{},"id":"1"}"#;

    /// Spawn a one-shot mock signal-cli daemon on a loopback socket. It
    /// answers the first request with `resp_body` and resolves its join
    /// handle to the raw request body it received.
    async fn spawn_rpc_mock(resp_body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let body = read_request_body(&mut sock).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            body
        });
        (base, server)
    }

    /// Await the mock daemon's recorded request body and parse it as JSON.
    /// Fails fast if `what` never issued an RPC at all — the guard that
    /// catches a client method silently short-circuiting to `Ok(())`.
    async fn recv_rpc(server: tokio::task::JoinHandle<String>, what: &str) -> serde_json::Value {
        let body = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap_or_else(|_| panic!("{what} must issue an RPC request"))
            .unwrap();
        serde_json::from_str(&body).unwrap()
    }

    /// Send markdown text through a real `send_message` call against a mock
    /// daemon and assert the daemon receives delimiter-stripped text plus the
    /// matching Signal `textStyle` ranges. Regression guard: the send path
    /// previously dropped all styling (callers passed `None`), so `**bold**`
    /// reached the wire verbatim with no `textStyle`.
    #[tokio::test]
    async fn send_message_converts_markdown_to_text_styles_on_the_wire() {
        let (base, server) = spawn_rpc_mock(OK_RESULT).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        client
            .send_message("+15551234567", "**bold**")
            .await
            .unwrap();

        let json = recv_rpc(server, "send_message").await;
        assert_eq!(json["method"], "send");
        let params = &json["params"];
        assert_eq!(
            params["message"], "bold",
            "markdown delimiters must be stripped from the wire text"
        );
        assert_eq!(params["recipient"], serde_json::json!(["+15551234567"]));
        assert_eq!(
            params["textStyle"],
            serde_json::json!([{ "start": 0, "length": 4, "style": "BOLD" }]),
            "bold span must be sent as a Signal textStyle range"
        );
    }

    /// `send_group_message` must address the RPC by `groupId` (not
    /// `recipient`) and carry the rendered markdown like the 1:1 path.
    #[tokio::test]
    async fn send_group_message_posts_send_rpc_with_group_id() {
        let (base, server) = spawn_rpc_mock(OK_RESULT).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        client
            .send_group_message("grp-123", "hello **team**")
            .await
            .unwrap();

        let json = recv_rpc(server, "send_group_message").await;
        assert_eq!(json["method"], "send");
        let params = &json["params"];
        assert_eq!(params["groupId"], "grp-123");
        assert_eq!(params["message"], "hello team");
        assert_eq!(
            params["textStyle"],
            serde_json::json!([{ "start": 6, "length": 4, "style": "BOLD" }])
        );
        assert!(
            params.get("recipient").is_none(),
            "group sends must not carry a recipient field"
        );
    }

    /// `send_typing` must issue a `sendTyping` RPC addressed to the recipient.
    #[tokio::test]
    async fn send_typing_posts_send_typing_rpc() {
        let (base, server) = spawn_rpc_mock(OK_RESULT).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        client
            .send_typing(Some("+15551230000"), None)
            .await
            .unwrap();

        let json = recv_rpc(server, "send_typing").await;
        assert_eq!(json["method"], "sendTyping");
        let params = &json["params"];
        assert_eq!(params["account"], "+15559876543");
        assert_eq!(params["recipient"], serde_json::json!(["+15551230000"]));
        assert!(
            params.get("groupId").is_none(),
            "no groupId for a 1:1 typing indicator"
        );
    }

    /// `send_read_receipt` must issue a `sendReceipt` RPC of type `read`
    /// covering every supplied timestamp.
    #[tokio::test]
    async fn send_read_receipt_posts_send_receipt_rpc() {
        let (base, server) = spawn_rpc_mock(OK_RESULT).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        client
            .send_read_receipt("+15551230000", &[1_700_000_000_123, 1_700_000_000_456])
            .await
            .unwrap();

        let json = recv_rpc(server, "send_read_receipt").await;
        assert_eq!(json["method"], "sendReceipt");
        let params = &json["params"];
        assert_eq!(params["type"], "read");
        assert_eq!(params["recipient"], "+15551230000");
        assert_eq!(
            params["targetTimestamp"],
            serde_json::json!([1_700_000_000_123i64, 1_700_000_000_456i64])
        );
    }

    /// `send_reaction` must issue a `sendReaction` RPC carrying the emoji,
    /// target message coordinates, and the remove flag.
    #[tokio::test]
    async fn send_reaction_posts_send_reaction_rpc() {
        let (base, server) = spawn_rpc_mock(OK_RESULT).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        client
            .send_reaction(
                "+15551230000",
                None,
                "\u{1F44D}",
                "+15559999999",
                1_700_000_000_123,
                false,
            )
            .await
            .unwrap();

        let json = recv_rpc(server, "send_reaction").await;
        assert_eq!(json["method"], "sendReaction");
        let params = &json["params"];
        assert_eq!(params["emoji"], "\u{1F44D}");
        assert_eq!(params["targetAuthor"], "+15559999999");
        assert_eq!(params["targetTimestamp"], 1_700_000_000_123i64);
        assert_eq!(params["remove"], false);
        assert_eq!(params["recipient"], serde_json::json!(["+15551230000"]));
    }

    /// `probe` must call the `version` RPC and surface the daemon's actual
    /// version string, not a placeholder.
    #[tokio::test]
    async fn probe_returns_daemon_version_string() {
        let (base, server) =
            spawn_rpc_mock(r#"{"jsonrpc":"2.0","result":{"version":"0.13.4"},"id":"1"}"#).await;
        let client = SignalClient::new(&base, "+15559876543").unwrap();
        let version = client.probe().await.unwrap();
        assert_eq!(version, "0.13.4");

        let json = recv_rpc(server, "probe").await;
        assert_eq!(json["method"], "version");
    }

    /// `download_attachment` must accept bodies far larger than the old, broken
    /// 10 KB inline cap (the production cap is `SIGNAL_MAX_ATTACHMENT_BYTES` =
    /// 10 MB) while still rejecting anything over the caller-supplied limit.
    #[tokio::test]
    async fn download_attachment_honors_size_cap_boundary() {
        // 20 KB body — bigger than the old 10 KB cap that silently broke
        // every real attachment, small enough to fit a socket buffer.
        const BODY_LEN: usize = 20 * 1024;

        async fn serve_body(len: usize) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base = format!("http://{addr}");
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut tmp = [0u8; 1024];
                let _ = sock.read(&mut tmp).await; // drain request headers
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                );
                sock.write_all(header.as_bytes()).await.unwrap();
                sock.write_all(&vec![b'a'; len]).await.unwrap();
                sock.shutdown().await.ok();
            });
            base
        }

        // Under the 10 MB production cap: full body returned.
        let base = serve_body(BODY_LEN).await;
        let client = SignalClient::new(&base, "+15550000000").unwrap();
        let bytes = client
            .download_attachment("att-1", constants::SIGNAL_MAX_ATTACHMENT_BYTES)
            .await
            .expect("20 KB attachment must download under the 10 MB cap");
        assert_eq!(bytes.len(), BODY_LEN);

        // Over an explicit small cap: rejected via the Content-Length pre-check.
        let base = serve_body(BODY_LEN).await;
        let client = SignalClient::new(&base, "+15550000000").unwrap();
        let err = client
            .download_attachment("att-1", 1_000)
            .await
            .expect_err("body exceeding max_bytes must be rejected");
        assert!(
            err.to_string().contains("too large"),
            "error should explain the size rejection, got: {err}"
        );
    }
}
