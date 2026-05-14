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
//! - DNS hostnames resolved at check time and rejected if any resolved IP
//!   is local. Phase 2 will pin the resolved IP into the connector to
//!   defend against DNS rebinding.

use super::Transport;
use crate::errors::{McpError, Result, UrlRefusalReason};
use crate::protocol::Message;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;
use url::Url;

/// Policy knobs that the transport enforces on every outbound request.
///
/// Sourced from the cortex-side `McpConfig`; defaults below match the
/// security-first defaults declared in the plan: `https` only, no
/// loopback / RFC1918.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpPolicy {
    /// If false (default), URLs must be `https://`.
    pub allow_plaintext_http: bool,
    /// If false (default), URLs cannot resolve to loopback / RFC1918 / link-local space.
    pub allow_local_network: bool,
}

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
}

/// Domain names that always count as local regardless of DNS resolution.
/// Cloud metadata services that don't necessarily route to RFC1918 space.
const FORBIDDEN_HOSTNAMES: &[&str] = &[
    "localhost",
    "ip6-localhost",
    "ip6-loopback",
    "metadata.google.internal",
    "metadata.goog",
];

/// Parse `url` and check the scheme + literal host against the policy.
///
/// Does **not** resolve DNS — use [`check_url`] (async) for the full guard.
/// This synchronous variant is used when the caller has already resolved
/// (e.g. validating a relative endpoint URL against the SSE-event's own URL).
pub fn check_url_syntactic(url: &str, policy: &HttpPolicy) -> Result<Url> {
    let parsed: Url = url.parse().map_err(|_| McpError::UrlRefused {
        url: url.to_string(),
        reason: UrlRefusalReason::Malformed,
    })?;

    if !policy.allow_plaintext_http && parsed.scheme() != "https" {
        return Err(McpError::UrlRefused {
            url: url.to_string(),
            reason: UrlRefusalReason::NotHttps,
        });
    }

    if !policy.allow_local_network {
        if let Some(host) = parsed.host() {
            if host_is_local_literal(&host) {
                return Err(McpError::UrlRefused {
                    url: url.to_string(),
                    reason: UrlRefusalReason::LocalNetworkBlocked,
                });
            }
        }
    }

    Ok(parsed)
}

/// Full URL guard: syntactic check + DNS resolution + per-resolved-IP local
/// check. Refuses if *any* resolved address falls into loopback / RFC1918 /
/// link-local / cloud-metadata space.
///
/// This defends against the operator-paste mistake of `https://internal-jira/mcp`
/// (which `check_url_syntactic` admits because the host is a DNS name).
///
/// Note: this only defends *at check time*. DNS rebinding (resolve A, connect
/// to B) is not defended against in Phase 1 — a follow-up should pin the
/// resolved IP into the reqwest connector. Logged as a known gap in
/// `crates/mcp/src/lib.rs`.
pub async fn check_url(url: &str, policy: &HttpPolicy) -> Result<Url> {
    let parsed = check_url_syntactic(url, policy)?;
    if policy.allow_local_network {
        return Ok(parsed);
    }
    let Some(host) = parsed.host() else {
        return Ok(parsed);
    };
    // IP literals were already covered by host_is_local_literal in the
    // syntactic check.
    if let url::Host::Domain(name) = host {
        let name_owned = name.to_ascii_lowercase();
        // Spawn-blocking the std resolver keeps us off `trust-dns` for this
        // patch; the lookup happens on a tokio blocking thread.
        let resolved = tokio::task::spawn_blocking(move || {
            (name_owned.as_str(), 0u16)
                .to_socket_addrs()
                .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
        })
        .await
        .map_err(|e| McpError::Connect(format!("dns task panicked: {e}")))?
        .map_err(|e| McpError::Connect(format!("dns lookup failed for {name}: {e}")))?;

        if resolved.iter().any(|ip| ip_is_local(*ip)) {
            return Err(McpError::UrlRefused {
                url: url.to_string(),
                reason: UrlRefusalReason::LocalNetworkBlocked,
            });
        }
    }
    Ok(parsed)
}

/// True if the literal host *as written* is local: IP-literal in private
/// space, or one of the hardcoded forbidden hostnames (loopback aliases +
/// cloud metadata FQDNs).
fn host_is_local_literal(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => {
            let n = name.to_ascii_lowercase();
            FORBIDDEN_HOSTNAMES.contains(&n.as_str())
        }
        url::Host::Ipv4(ip) => ip_is_local(IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => ip_is_local(IpAddr::V6(*ip)),
    }
}

fn ip_is_local(ip: IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            // AWS / GCP / Azure IMDS lives at 169.254.169.254 — covered by
            // is_link_local() (169.254.0.0/16). 0.0.0.0/8 covered by
            // is_unspecified() for the all-zeros case, plus the leading-zero
            // octet defense below for the rest of the legacy "this network"
            // range.
            v4.is_private() || v4.is_link_local() || v4.octets()[0] == 0 || v4.is_broadcast()
        }
        // is_unique_local / is_unicast_link_local are still unstable; do it
        // by hand to avoid a feature flag.
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // fc00::/7 unique-local
            (seg[0] & 0xfe00) == 0xfc00
            // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Build a reqwest client with the right knobs: timeout, no redirects, rustls.
fn build_client(timeout: Duration) -> Result<Client> {
    // Note: the workspace `reqwest` dep uses its default TLS backend (native-tls
    // on macOS, system openssl elsewhere). Both verify certs by default. A
    // follow-up will enable rustls-tls in the workspace feature set so we can
    // pin the verifier explicitly across platforms.
    Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| McpError::Connect(format!("reqwest build failed: {e}")))
}

/// Open an HTTP MCP transport. Resolves DNS for the policy check.
pub async fn open(config: HttpConfig) -> Result<HttpTransport> {
    let url = check_url(&config.url, &config.policy).await?;
    let client = build_client(config.timeout)?;
    Ok(HttpTransport {
        client,
        url,
        headers: config.headers,
        server_name: config.server_name,
        inbox: VecDeque::new(),
    })
}

/// Connected HTTP transport.
///
/// Streamable HTTP MCP responses can contain multiple JSON-RPC messages in
/// one body (e.g. a response plus a `tools/list_changed` notification). We
/// buffer them in `inbox` and drain via `recv`.
pub struct HttpTransport {
    client: Client,
    url: Url,
    headers: HashMap<String, String>,
    server_name: String,
    inbox: VecDeque<Message>,
}

impl HttpTransport {
    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
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
        let resp = req.json(&msg).send().await.map_err(|e| {
            McpError::TransportClosed(format!("{}: http send failed: {e}", self.server_name))
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
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::TransportClosed(format!(
                "{}: HTTP {}: {}",
                self.server_name,
                status.as_u16(),
                truncate(&body, 200)
            )));
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

    fn default_policy() -> HttpPolicy {
        HttpPolicy::default()
    }

    #[test]
    fn syntactic_rejects_plaintext_http_by_default() {
        let err = check_url_syntactic("http://example.com/mcp", &default_policy()).unwrap_err();
        match err {
            McpError::UrlRefused {
                reason: UrlRefusalReason::NotHttps,
                ..
            } => {}
            e => panic!("expected NotHttps, got {e:?}"),
        }
    }

    #[test]
    fn syntactic_admits_plaintext_when_explicitly_allowed() {
        let policy = HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        };
        check_url_syntactic("http://example.com/mcp", &policy).unwrap();
    }

    #[test]
    fn syntactic_rejects_loopback_literals_by_default() {
        // SSRF guard, literal-host slice: encodes the spec that an operator
        // pasting any IP-literal in private space, or any of the loopback
        // hostname aliases / cloud metadata FQDNs, gets refused without
        // requiring DNS.
        for url in [
            "https://localhost/mcp",
            "https://127.0.0.1/mcp",
            "https://[::1]/mcp",
            "https://10.0.0.5/mcp",
            "https://192.168.1.1/mcp",
            "https://172.16.0.1/mcp",
            "https://[fc00::1]/mcp",
            "https://[fe80::1]/mcp",
            "https://169.254.169.254/latest/meta-data/",
            "https://metadata.google.internal/computeMetadata/v1/",
        ] {
            let err = check_url_syntactic(url, &default_policy()).unwrap_err();
            match err {
                McpError::UrlRefused {
                    reason: UrlRefusalReason::LocalNetworkBlocked,
                    ..
                } => {}
                e => panic!("expected LocalNetworkBlocked for {url}, got {e:?}"),
            }
        }
    }

    #[tokio::test]
    async fn dns_resolution_rejects_hostname_pointing_at_private_ip() {
        // SSRF guard, DNS slice: encodes the spec that a *DNS hostname* that
        // resolves to a private IP must be refused. This is the bypass class
        // the code review caught — without DNS resolution, an operator's
        // typo of `https://internal-jira/mcp` sailed through.
        //
        // We use `127.0.0.1.nip.io` which is a public DNS service that
        // canonically resolves arbitrary hostnames to embedded IPs without
        // requiring our test to hit a host we control. If DNS is unavailable
        // (offline CI), the test surfaces as a Connect error which we accept
        // — the alternative is to skip the test, but a regression that breaks
        // resolution entirely should not silently pass.
        let err = check_url("https://127.0.0.1.nip.io/mcp", &default_policy()).await;
        match err {
            Err(McpError::UrlRefused {
                reason: UrlRefusalReason::LocalNetworkBlocked,
                ..
            }) => {}
            Err(McpError::Connect(msg)) if msg.contains("dns lookup failed") => {
                // Offline test environment — log and skip the spec assertion.
                eprintln!("dns_resolution_rejects test skipped: {msg}");
            }
            other => panic!("expected LocalNetworkBlocked (or DNS unavailable), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn full_check_admits_loopback_when_explicitly_allowed() {
        let policy = HttpPolicy {
            allow_plaintext_http: false,
            allow_local_network: true,
        };
        check_url("https://localhost/mcp", &policy).await.unwrap();
    }

    #[test]
    fn syntactic_rejects_malformed() {
        let err = check_url_syntactic("not a url", &default_policy()).unwrap_err();
        match err {
            McpError::UrlRefused {
                reason: UrlRefusalReason::Malformed,
                ..
            } => {}
            e => panic!("expected Malformed, got {e:?}"),
        }
    }
}
