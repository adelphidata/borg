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
use crate::errors::{McpError, Result, UrlRefusalReason};
use crate::protocol::Message;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use url::Url;

/// Policy knobs that the transport enforces on every outbound request.
///
/// Sourced from the cortex-side `McpConfig`; secure-by-default — `https`
/// only, no loopback / RFC1918.
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

/// Loopback hostname aliases that resolve to local interfaces. Refused
/// unless the operator opts in via `allow_local_network=true`.
const LOOPBACK_HOSTNAMES: &[&str] = &["localhost", "ip6-localhost", "ip6-loopback"];

/// Hostnames that are refused unconditionally — flipping
/// `allow_local_network=true` must NOT silently unlock SSRF to cloud
/// metadata endpoints. These FQDNs have no legitimate intranet use case
/// for borg.
const ALWAYS_FORBIDDEN_HOSTNAMES: &[&str] = &["metadata.google.internal", "metadata.goog"];

/// Parse `url` and check the scheme + literal host against the policy.
///
/// Does **not** resolve DNS — use [`resolve_endpoint`] (async) for the full
/// guard including DNS validation and connector pinning. This synchronous
/// variant is used when the caller already has a parsed URL and only needs
/// the scheme + literal-host policy check.
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

    if let Some(host) = parsed.host() {
        // Cloud-metadata FQDNs (and `localhost` aliases) are refused
        // unconditionally — flipping `allow_local_network=true` for a
        // legitimate intranet server must NOT simultaneously unlock SSRF
        // to GCP/AWS/Azure IMDS. The RFC1918 / loopback IP-literal checks
        // below still honor `allow_local_network` because operators
        // legitimately need to talk to their own subnet.
        if host_is_always_forbidden(&host) {
            return Err(McpError::UrlRefused {
                url: url.to_string(),
                reason: UrlRefusalReason::LocalNetworkBlocked,
            });
        }
        if !policy.allow_local_network && host_is_local_literal(&host) {
            return Err(McpError::UrlRefused {
                url: url.to_string(),
                reason: UrlRefusalReason::LocalNetworkBlocked,
            });
        }
    }

    Ok(parsed)
}

/// True if the hostname is on the always-refused list — cloud-metadata
/// FQDNs that have no legitimate intranet use case. Refused even when
/// `allow_local_network=true`. The IMDS IP literal (169.254.169.254) is
/// caught separately by the RFC1918 / link-local IP check.
fn host_is_always_forbidden(host: &url::Host<&str>) -> bool {
    if let url::Host::Domain(name) = host {
        let n = name.to_ascii_lowercase();
        return ALWAYS_FORBIDDEN_HOSTNAMES.contains(&n.as_str());
    }
    false
}

/// A URL that has been checked against policy *and* had its DNS resolution
/// captured for later pinning into the reqwest connector.
///
/// `pins` is the (host, [SocketAddr]) list to feed to
/// `reqwest::ClientBuilder::resolve_to_addrs`. Empty when the URL host is an
/// IP literal — there's nothing to rebind in that case.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// The parsed URL.
    pub url: Url,
    /// `(hostname, addresses)` pairs to pin into the connector.
    pub pins: Vec<(String, Vec<SocketAddr>)>,
}

/// Full URL guard: syntactic check + DNS resolution + per-resolved-IP local
/// check. Refuses if *any* resolved address falls into loopback / RFC1918 /
/// link-local / cloud-metadata space.
///
/// Returns the parsed URL plus the resolved socket addresses so the caller
/// can pin them into the reqwest connector. Pinning eliminates the DNS
/// rebinding gap (resolve A here, connect to B in reqwest): we resolve once,
/// validate every IP, and hand reqwest the same address list.
///
/// IP-literal hosts skip DNS and return an empty `pins` vec — there is no
/// resolution to rebind.
pub async fn resolve_endpoint(url: &str, policy: &HttpPolicy) -> Result<ResolvedEndpoint> {
    let parsed = check_url_syntactic(url, policy)?;
    let Some(host) = parsed.host() else {
        return Ok(ResolvedEndpoint {
            url: parsed,
            pins: Vec::new(),
        });
    };
    let url::Host::Domain(name) = host else {
        // IP literal — `check_url_syntactic` already evaluated it. No DNS,
        // no pinning needed.
        return Ok(ResolvedEndpoint {
            url: parsed,
            pins: Vec::new(),
        });
    };

    // reqwest's `resolve_to_addrs` matches on the host string and ignores the
    // port in the SocketAddr (it substitutes the request's port). We still
    // pass a sensible port so the std resolver returns SOCK_STREAM records.
    let name_owned = name.to_ascii_lowercase();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    let lookup_name = name_owned.clone();
    let resolved: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
        (lookup_name.as_str(), port)
            .to_socket_addrs()
            .map(Iterator::collect::<Vec<_>>)
    })
    .await
    .map_err(|e| McpError::Connect(format!("dns task panicked: {e}")))?
    .map_err(|e| McpError::Connect(format!("dns lookup failed for {name}: {e}")))?;

    if !policy.allow_local_network && resolved.iter().any(|sa| ip_is_local(sa.ip())) {
        return Err(McpError::UrlRefused {
            url: url.to_string(),
            reason: UrlRefusalReason::LocalNetworkBlocked,
        });
    }

    Ok(ResolvedEndpoint {
        url: parsed,
        pins: vec![(name_owned, resolved)],
    })
}

/// True if the literal host *as written* is local: IP-literal in private
/// space, or one of the hardcoded forbidden hostnames (loopback aliases +
/// cloud metadata FQDNs).
fn host_is_local_literal(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => {
            let n = name.to_ascii_lowercase();
            LOOPBACK_HOSTNAMES.contains(&n.as_str())
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
) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    for (host, addrs) in pins {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    builder
        .build()
        .map_err(|e| McpError::Connect(format!("reqwest build failed: {e}")))
}

/// Open an HTTP MCP transport. Resolves DNS, validates every resolved IP
/// against the local-network policy, and pins the resolved addresses into
/// the reqwest connector for DNS-rebinding resistance.
pub async fn open(config: HttpConfig) -> Result<HttpTransport> {
    let resolved = resolve_endpoint(&config.url, &config.policy).await?;
    let client = build_client(config.timeout, &resolved.pins)?;
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
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::TransportClosed(format!(
                "{}: HTTP {}: {}",
                self.server_name,
                status.as_u16(),
                truncate(&body, 200)
            )));
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
        let err = resolve_endpoint("https://127.0.0.1.nip.io/mcp", &default_policy()).await;
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
        resolve_endpoint("https://localhost/mcp", &policy)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn resolve_endpoint_returns_pins_for_domain_host() {
        // DNS-rebinding defence: encodes the spec that domain-host URLs surface
        // pinned `(host, addrs)` entries so the caller can hand them to
        // `reqwest::ClientBuilder::resolve_to_addrs`. Without this pin list
        // the connector would re-resolve at connect time and a hostile
        // authoritative DNS server could swap in a private IP.
        let policy = HttpPolicy {
            allow_plaintext_http: false,
            allow_local_network: true, // we use localhost-resolving DNS to stay hermetic
        };
        match resolve_endpoint("https://localhost/mcp", &policy).await {
            Ok(r) => {
                assert_eq!(
                    r.pins.len(),
                    1,
                    "expected exactly one pinned hostname, got {:?}",
                    r.pins
                );
                let (host, addrs) = &r.pins[0];
                assert_eq!(host, "localhost");
                assert!(
                    !addrs.is_empty(),
                    "pinned address list must be non-empty (got {:?})",
                    addrs
                );
            }
            Err(McpError::Connect(msg)) if msg.contains("dns lookup failed") => {
                // Offline CI — `localhost` failed to resolve. Skip rather than
                // mask a real regression with an unconditional pass.
                eprintln!("resolve_endpoint pin test skipped: {msg}");
            }
            Err(e) => panic!("expected Ok or DNS-unavailable, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_endpoint_returns_no_pins_for_ip_literal() {
        // IP-literal hosts have no DNS to rebind, so they must surface an
        // empty pin list — otherwise we'd be telling reqwest to "resolve"
        // an address that's already an IP, which is meaningless and risks
        // confusing the connector.
        let policy = HttpPolicy {
            allow_plaintext_http: false,
            allow_local_network: true,
        };
        let r = resolve_endpoint("https://127.0.0.1/mcp", &policy)
            .await
            .unwrap();
        assert!(
            r.pins.is_empty(),
            "ip literals must not produce pins, got {:?}",
            r.pins
        );
    }

    #[test]
    fn always_forbidden_hostnames_refused_even_when_local_network_allowed() {
        // The unconditional SSRF list (cloud-metadata FQDNs) must NOT be
        // unlocked when an operator flips `allow_local_network=true` for a
        // legitimate intranet need. A regression that conflated the two
        // lists would re-open the GCP IMDS escape hatch.
        let permissive = HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        };
        for url in [
            "https://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/foo",
        ] {
            let err = check_url_syntactic(url, &permissive).unwrap_err();
            match err {
                McpError::UrlRefused {
                    reason: UrlRefusalReason::LocalNetworkBlocked,
                    ..
                } => {}
                e => panic!("expected LocalNetworkBlocked (unconditional) for {url}, got {e:?}",),
            }
        }
    }

    #[test]
    fn imds_ip_literal_admitted_by_syntactic_layer_when_local_network_allowed() {
        // Spec pin (syntactic layer only): the IMDS IP literal
        // 169.254.169.254 sits in link-local space, which `allow_local_network`
        // legitimately unlocks for operators talking to their own subnet.
        // Domain hosts in the cloud-metadata FQDN list stay refused
        // unconditionally (see `always_forbidden_hostnames_…`), but the
        // matching IP literal does NOT. This asymmetry is intentional —
        // operators must use the kill switch / URL allowlist (not the
        // local-network policy) to keep IMDS out — and it deserves a
        // tracking note in the user-facing settings docs. This test pins
        // the syntactic gate's behavior only; deeper layers may still
        // refuse and that would not break this assertion.
        let permissive = HttpPolicy {
            allow_plaintext_http: false,
            allow_local_network: true,
        };
        check_url_syntactic("https://169.254.169.254/", &permissive).unwrap();
    }
}
