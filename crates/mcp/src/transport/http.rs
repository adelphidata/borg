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
}

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
    Ok(HttpTransport {
        client,
        url: resolved.url,
        headers: config.headers,
        server_name: config.server_name,
        inbox: VecDeque::new(),
        session_id: None,
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
/// wedged transport that keeps POSTing without the header. Auto-reconnect
/// (calling `start_server` again on a torn-down session) is a follow-up.
pub struct HttpTransport {
    client: Client,
    url: Url,
    headers: HashMap<String, String>,
    server_name: String,
    inbox: VecDeque<Message>,
    session_id: Option<String>,
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
        let resp = req.json(&msg).send().await.map_err(|e| {
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
            let body = resp.text().await.unwrap_or_default();
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
            let body = resp.text().await.unwrap_or_default();
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

        let body = resp.bytes().await.map_err(|e| {
            McpError::TransportClosed(format!("{}: body read failed: {e}", self.server_name))
        })?;
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
        // Stateless: nothing to close.
        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
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
}
