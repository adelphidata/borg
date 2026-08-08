//! Streamable HTTP transport.
//!
//! Each outbound JSON-RPC message is POSTed; responses are parsed from the
//! HTTP body. Servers that prefer SSE for streaming responses use the [`super::sse`]
//! transport instead.
//!
//! Security posture:
//!
//! - `https://` only unless `allow_plaintext_http` is explicitly set.
//! - Loopback / RFC1918 destinations refused unless `allow_local_network` is
//!   set (SSRF guard).
//! - Redirects disabled — a redirect from an allowlisted endpoint to an
//!   internal IP would otherwise bypass the URL guard.
//! - DNS hostnames are resolved at check time, every resolved IP is checked
//!   against the local-network policy, and the resolved addresses are pinned
//!   into the reqwest connector via `resolve_to_addrs` so a hostile
//!   authoritative DNS server cannot rebind the host between the check and
//!   the connect.

use super::Transport;
use crate::errors::{McpError, Result};
use crate::protocol::Message;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;
use url::Url;

pub use super::url_guard::{check_url_syntactic, resolve_endpoint, HttpPolicy, ResolvedEndpoint};

/// Config for an HTTP MCP endpoint.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Server name (for diagnostics).
    pub server_name: String,
    /// Endpoint URL.
    pub url: String,
    /// Headers to send on every request (typically Authorization).
    pub headers: HashMap<String, String>,
    /// Per-call timeout.
    pub timeout: Duration,
    /// Policy gates.
    pub policy: HttpPolicy,
    /// Optional client certificate for mutual TLS. Loaded at connect time.
    pub client_identity: Option<crate::manifest::ClientIdentity>,
    /// Hard cap on a single response body, enforced *while streaming* so a
    /// hostile/compromised server can't OOM the process by streaming a huge
    /// chunked body before any downstream truncation runs. 0 means "unset" and
    /// falls back to [`DEFAULT_MAX_RESPONSE_BYTES`].
    pub max_response_bytes: usize,
}

/// Fallback body cap when `HttpConfig::max_response_bytes` is 0 (unset). Matches
/// the cortex-side 1 MiB default; a real per-server value is threaded in from
/// `mcp.max_response_bytes` at construction.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Build a reqwest client with: timeout, no redirects, rustls TLS, and DNS
/// resolution pinned to the supplied `(host, addrs)` pairs.
///
/// Pinning is what closes the DNS rebinding gap: the connector will never
/// re-resolve a pinned hostname, so it can only connect to the addresses we
/// already validated against the local-network policy. Unpinned hostnames
/// (e.g. an IP literal in the URL) fall back to the OS resolver, which is
/// safe because the syntactic check has already rejected local IP literals.
pub(crate) fn build_client(
    timeout: Duration,
    pins: &[(String, Vec<SocketAddr>)],
    client_identity: Option<&crate::manifest::ClientIdentity>,
) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(timeout)
        // Per-client in-memory cookie jar. Streamable-HTTP MCP servers fronted
        // by a load balancer (e.g. Cloudflare `__cf*`, or a sticky-session
        // affinity cookie) depend on the client echoing `Set-Cookie` values so
        // every request in a session lands on the same backend. Without this,
        // only the explicit `Mcp-Session-Id` header round-trips and affinity is
        // lost mid-session. Each transport builds its own `Client`, so the jar
        // is scoped to a single server — cookies never cross server boundaries,
        // and with redirects disabled they can't leak cross-origin either.
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none());
    for (host, addrs) in pins {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    if let Some(identity) = client_identity {
        builder = builder.identity(load_client_identity(identity)?);
    }
    builder
        .build()
        .map_err(|e| McpError::Connect(format!("reqwest build failed: {e}")))
}

/// Load a client certificate + key into a `reqwest::Identity` for mutual TLS.
///
/// reqwest expects a single PEM containing both the certificate chain and the
/// private key; when the key lives in a separate file we concatenate them. Any
/// missing file or malformed PEM fails fast with a `Connect` error rather than
/// silently downgrading to no client cert.
fn load_client_identity(identity: &crate::manifest::ClientIdentity) -> Result<reqwest::Identity> {
    let mut pem = std::fs::read(&identity.cert_path).map_err(|e| {
        McpError::Connect(format!(
            "failed to read client certificate '{}': {e}",
            identity.cert_path
        ))
    })?;
    if let Some(key_path) = &identity.key_path {
        let key = std::fs::read(key_path).map_err(|e| {
            McpError::Connect(format!("failed to read client key '{key_path}': {e}"))
        })?;
        if !pem.ends_with(b"\n") {
            pem.push(b'\n');
        }
        pem.extend_from_slice(&key);
    }
    reqwest::Identity::from_pem(&pem)
        .map_err(|e| McpError::Connect(format!("invalid client identity PEM: {e}")))
}

/// Open an HTTP MCP transport. Resolves DNS, validates every resolved IP
/// against the local-network policy, and pins the resolved addresses into
/// the reqwest connector for DNS-rebinding resistance.
pub async fn open(config: HttpConfig) -> Result<HttpTransport> {
    let resolved = resolve_endpoint(&config.url, &config.policy).await?;
    let client = build_client(
        config.timeout,
        &resolved.pins,
        config.client_identity.as_ref(),
    )?;
    let max_response_bytes = if config.max_response_bytes == 0 {
        DEFAULT_MAX_RESPONSE_BYTES
    } else {
        config.max_response_bytes
    };
    Ok(HttpTransport {
        client,
        url: resolved.url,
        headers: config.headers,
        server_name: config.server_name,
        inbox: VecDeque::new(),
        session_id: None,
        max_response_bytes,
    })
}

/// Header carrying the resumable session id, per the MCP streamable-HTTP
/// spec. The server may emit this on `initialize` (or later); we echo it on
/// every subsequent request. A server that returns 404 after the id was
/// once valid invalidates our cached id and we re-initialize transparently.
pub const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";

/// Connected HTTP transport.
///
/// Streamable HTTP MCP responses can contain multiple JSON-RPC messages in
/// one body (e.g. a response plus a `tools/list_changed` notification). We
/// buffer them in `inbox` and drain via `recv`.
///
/// `session_id` holds the most recent `Mcp-Session-Id` echoed by the server;
/// it is sent on every outbound request so the server can route us back to
/// the same session state. On `404 Not Found` the cached id is dropped and
/// `send` returns `TransportClosed` — the cortex session task tears the
/// session down so the operator sees one clear failure rather than a
/// wedged transport that keeps POSTing without the header.
pub struct HttpTransport {
    client: Client,
    url: Url,
    headers: HashMap<String, String>,
    server_name: String,
    inbox: VecDeque<Message>,
    session_id: Option<String>,
    max_response_bytes: usize,
}

impl HttpTransport {
    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The cached `Mcp-Session-Id`, if the server has emitted one. Surfaced
    /// for tests + diagnostics; production code should not poke at it.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&mut self, msg: Value) -> Result<()> {
        let mut req = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        // Echo back any `Mcp-Session-Id` the server has previously assigned
        // us. Per MCP streamable-HTTP spec this is how a resumable session
        // routes a follow-up request back to its state. Without this echo
        // every request reads from a cold session and `tools/list_changed`
        // notifications can't be honored across reconnects.
        if let Some(sid) = &self.session_id {
            req = req.header(MCP_SESSION_ID_HEADER, sid);
        }
        let mut resp = req.json(&msg).send().await.map_err(|e| {
            // A connect/timeout failure (server not yet listening, momentary
            // network blip) is transient and retryable on the idempotent startup
            // path; any other send failure tears the transport down.
            let detail = format!("{}: http send failed: {e}", self.server_name);
            if e.is_connect() || e.is_timeout() {
                McpError::Connect(detail)
            } else {
                McpError::TransportClosed(detail)
            }
        })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let body = read_error_body_capped(resp).await;
            return Err(McpError::Auth(format!(
                "{}: HTTP {}: {}",
                self.server_name,
                status.as_u16(),
                truncate(&body, 200)
            )));
        }
        // 404 with a stale session id means "session not found, re-init."
        // Drop the cached id and surface a TransportClosed so the supervisor
        // (one level up) discards this transport and opens a fresh one.
        // Without this branch a server-side restart would loop forever
        // sending invalid session ids.
        if status == reqwest::StatusCode::NOT_FOUND && self.session_id.is_some() {
            tracing::info!(
                server = %self.server_name,
                "MCP session id rejected (404); will re-initialize",
            );
            self.session_id = None;
            return Err(McpError::TransportClosed(format!(
                "{}: session id invalidated (404); transport torn down — \
                 re-init requires daemon restart",
                self.server_name
            )));
        }
        if !status.is_success() {
            let code = status.as_u16();
            let body = read_error_body_capped(resp).await;
            let detail = format!(
                "{}: HTTP {}: {}",
                self.server_name,
                code,
                truncate(&body, 200)
            );
            // Transient statuses (timeouts, too-early, rate-limit, 5xx) are safe
            // to retry by recreating the transport — but only on the idempotent
            // startup path (`initialize` / read-only `tools/list`). Everything
            // else tears the transport down via `TransportClosed`.
            if matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504) {
                return Err(McpError::TransientHttp {
                    status: code,
                    msg: detail,
                });
            }
            return Err(McpError::TransportClosed(detail));
        }

        // Capture / refresh the session id from the response headers. The
        // server may emit it on `initialize` or rotate it later; we always
        // adopt the most recently seen value.
        if let Some(hv) = resp.headers().get(MCP_SESSION_ID_HEADER) {
            if let Ok(s) = hv.to_str() {
                if self.session_id.as_deref() != Some(s) {
                    tracing::debug!(
                        server = %self.server_name,
                        "adopted Mcp-Session-Id from server",
                    );
                }
                self.session_id = Some(s.to_string());
            }
        }

        // Read the body chunk-by-chunk with a hard cap enforced *as we read*,
        // rather than `resp.bytes()` which buffers the whole body first. A
        // hostile server streaming a multi-GB chunked response is aborted after
        // ~max_response_bytes instead of OOMing the process.
        let (body, truncated) = read_body_capped(&mut resp, self.max_response_bytes)
            .await
            .map_err(|e| {
                McpError::TransportClosed(format!("{}: body read failed: {e}", self.server_name))
            })?;
        if truncated {
            return Err(McpError::Protocol(format!(
                "{}: response body exceeds max_response_bytes ({} bytes)",
                self.server_name, self.max_response_bytes
            )));
        }
        // Streamable HTTP framing: either one JSON object, or newline-delimited
        // JSON objects when the server batches a response + notifications.
        for line in body.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let msg = crate::protocol::decode_line(line)?;
            self.inbox.push_back(msg);
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        // HTTP is request-response: every `send` populates `inbox` with the
        // response body's messages. The session loop calls `recv` until it
        // sees the matching id. If the inbox is empty here, the peer
        // responded but *not* to our latest request — that's a protocol
        // violation, not a transport close. Surface as `Protocol` so the
        // session classifies it correctly (it would otherwise treat
        // `Ok(None)` as EOF and stop reconnect attempts).
        match self.inbox.pop_front() {
            Some(m) => Ok(Some(m)),
            None => Err(McpError::Protocol(format!(
                "{}: http response drained without matching id",
                self.server_name
            ))),
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Streamable HTTP is request-scoped; there is no long-lived socket to
        // tear down. The server-side session expires on its own TTL — we
        // deliberately do not send the spec's `DELETE` since a hostile or
        // unreachable server must not be able to block shutdown.
        Ok(())
    }
}

/// How much of a non-2xx body is shown in the error message. The status code
/// carries the diagnosis; the body is best-effort color.
const ERROR_BODY_DISPLAY_BYTES: usize = 200;

/// Read a response body chunk-by-chunk, keeping at most `cap` bytes. Returns
/// the (possibly truncated) bytes and whether truncation happened; the caller
/// decides if truncation is an error (success path) or fine (error display).
/// Enforcing the cap *as we read* is the point — `resp.bytes()`/`resp.text()`
/// buffer the whole body first, letting a hostile server stream gigabytes.
async fn read_body_capped(
    resp: &mut reqwest::Response,
    cap: usize,
) -> std::result::Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let remaining = cap - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Display-ready prefix of an error response's body. Read failures degrade to
/// an empty string — a server that 500s and then breaks the stream still gets
/// its status reported.
async fn read_error_body_capped(mut resp: reqwest::Response) -> String {
    let body = match read_body_capped(&mut resp, ERROR_BODY_DISPLAY_BYTES).await {
        Ok((body, _)) => body,
        Err(_) => Vec::new(),
    };
    String::from_utf8_lossy(&body).into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Back up to a char boundary — byte-slicing mid-codepoint panics, and
    // lossy-decoded error bodies routinely end mid-character at the cap.
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a throwaway self-signed cert + key PEM pair for mTLS tests.
    fn self_signed_pem() -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["client.test".to_string()])
            .expect("generate self-signed cert");
        (cert.cert.pem(), cert.key_pair.serialize_pem())
    }

    #[test]
    fn build_client_with_separate_cert_and_key_succeeds() {
        // A valid client cert + key in separate PEM files must build a usable
        // reqwest client (the mTLS identity is accepted by reqwest's rustls
        // backend). This exercises the cert+key concatenation path.
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem) = self_signed_pem();
        let cert_path = dir.path().join("client.crt");
        let key_path = dir.path().join("client.key");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        let identity = crate::manifest::ClientIdentity {
            cert_path: cert_path.to_string_lossy().into_owned(),
            key_path: Some(key_path.to_string_lossy().into_owned()),
        };
        build_client(Duration::from_secs(5), &[], Some(&identity))
            .expect("valid client identity should build a client");
    }

    #[test]
    fn build_client_with_combined_cert_key_pem_succeeds() {
        // reqwest also accepts a single PEM holding both cert and key; with no
        // separate key file the bytes are used as-is.
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem) = self_signed_pem();
        let combined = format!("{cert_pem}{key_pem}");
        let combined_path = dir.path().join("combined.pem");
        std::fs::write(&combined_path, combined).unwrap();

        let identity = crate::manifest::ClientIdentity {
            cert_path: combined_path.to_string_lossy().into_owned(),
            key_path: None,
        };
        build_client(Duration::from_secs(5), &[], Some(&identity))
            .expect("combined cert+key PEM should build a client");
    }

    #[test]
    fn build_client_missing_cert_file_fails_fast() {
        // A configured-but-absent cert must surface a Connect error rather than
        // silently downgrading to no client certificate.
        let identity = crate::manifest::ClientIdentity {
            cert_path: "/nonexistent/borg-test/client.crt".to_string(),
            key_path: None,
        };
        let err = build_client(Duration::from_secs(5), &[], Some(&identity))
            .expect_err("missing cert file must error");
        match err {
            McpError::Connect(msg) => assert!(
                msg.contains("failed to read client certificate"),
                "unexpected message: {msg}"
            ),
            e => panic!("expected Connect error, got {e:?}"),
        }
    }

    #[test]
    fn build_client_malformed_pem_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("garbage.pem");
        std::fs::write(&cert_path, "not a pem file").unwrap();
        let identity = crate::manifest::ClientIdentity {
            cert_path: cert_path.to_string_lossy().into_owned(),
            key_path: None,
        };
        let err = build_client(Duration::from_secs(5), &[], Some(&identity))
            .expect_err("malformed PEM must error");
        match err {
            McpError::Connect(msg) => assert!(
                msg.contains("invalid client identity PEM"),
                "unexpected message: {msg}"
            ),
            e => panic!("expected Connect error, got {e:?}"),
        }
    }

    #[test]
    fn build_client_without_identity_still_builds() {
        // Regression guard: the common server-auth-only path (no client cert)
        // must keep working after the mTLS plumbing was added.
        build_client(Duration::from_secs(5), &[], None).expect("no-identity client should build");
    }

    /// Bind a one-shot plaintext HTTP server on loopback that answers the first
    /// request with `body`, then returns the bound port. Used to drive the
    /// streaming body-cap path without a real MCP server.
    async fn spawn_oneshot_http(body: Vec<u8>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request headers so the client's write completes
                // before we reply; we don't care about the contents.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    fn local_http_config(port: u16, max_response_bytes: usize) -> HttpConfig {
        HttpConfig {
            server_name: "capped".to_string(),
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
            timeout: Duration::from_secs(5),
            policy: HttpPolicy {
                // Loopback plaintext is required to talk to the throwaway test
                // server; production endpoints stay HTTPS + non-local.
                allow_plaintext_http: true,
                allow_local_network: true,
            },
            client_identity: None,
            max_response_bytes,
        }
    }

    #[tokio::test]
    async fn response_body_over_cap_is_rejected_not_buffered() {
        // A server that returns a body larger than the transport ceiling must be
        // stopped with a Protocol error rather than buffered whole — the guard
        // against a hostile server OOMing the process.
        let oversized = vec![b'x'; 8192];
        let port = spawn_oneshot_http(oversized).await;
        let mut transport = open(local_http_config(port, 64)).await.unwrap();
        let err = transport
            .send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .await
            .expect_err("body over the cap must error");
        match err {
            McpError::Protocol(msg) => assert!(
                msg.contains("exceeds max_response_bytes"),
                "unexpected message: {msg}"
            ),
            e => panic!("expected Protocol error, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn response_body_under_cap_is_parsed() {
        // Control case: a well-formed response comfortably under the ceiling is
        // read and decoded normally, proving the cap doesn't reject valid bodies.
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_vec();
        let port = spawn_oneshot_http(body).await;
        let mut transport = open(local_http_config(port, 1024)).await.unwrap();
        transport
            .send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .await
            .expect("body under the cap should parse");
        let msg = transport.recv().await.expect("recv should succeed");
        assert!(msg.is_some(), "a message should have been decoded");
    }
}
