//! URL / SSRF policy guard for HTTP-based MCP transports.
//!
//! Everything security-critical about *where* an MCP request may go lives
//! here, isolated from the transport mechanics in [`super::http`]:
//!
//! - `https://` only unless `allow_plaintext_http` is explicitly set.
//! - Loopback / RFC1918 / link-local destinations refused unless
//!   `allow_local_network` is set.
//! - Cloud-metadata FQDNs refused unconditionally — `allow_local_network`
//!   must never unlock SSRF to IMDS.
//! - DNS hostnames are resolved at check time, every resolved IP is checked,
//!   and the addresses are returned for pinning into the connector so a
//!   hostile authoritative DNS server cannot rebind the host between the
//!   check and the connect.

use crate::errors::{McpError, Result, UrlRefusalReason};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
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
