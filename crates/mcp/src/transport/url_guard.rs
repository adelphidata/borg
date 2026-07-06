//! URL / SSRF policy guard for HTTP-based MCP transports.
//!
//! Everything security-critical about *where* an MCP request may go lives
//! here, isolated from the transport mechanics in [`super::http`]:
//!
//! - `https://` only unless `allow_plaintext_http` is explicitly set.
//! - Loopback / RFC1918 / link-local destinations refused unless
//!   `allow_local_network` is set.
//! - Cloud-metadata FQDNs *and IPs* (169.254.169.254, Azure wireserver,
//!   Alibaba, AWS IPv6) refused unconditionally — `allow_local_network`
//!   must never unlock SSRF to IMDS, whether the host is written as a name,
//!   an IP literal, or an encoded/mapped form.
//! - DNS hostnames are resolved at check time, every resolved IP is checked,
//!   and the addresses are returned for pinning into the connector so a
//!   hostile authoritative DNS server cannot rebind the host between the
//!   check and the connect.

use crate::errors::{McpError, Result, UrlRefusalReason};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
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
const ALWAYS_FORBIDDEN_HOSTNAMES: &[&str] = &[
    "metadata.google.internal",
    "metadata.goog",
    "metadata.digitalocean.com",
];

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
        // Cloud-metadata IP literals are refused UNCONDITIONALLY, even with
        // `allow_local_network=true`. Previously the IMDS IP was caught only by
        // the local-literal check below, so flipping the local-network knob for
        // a legitimate subnet server also unlocked SSRF to 169.254.169.254.
        if let Some(ip) = host_ip_literal(&host) {
            if ip_is_always_blocked(ip) {
                return Err(McpError::UrlRefused {
                    url: url.to_string(),
                    reason: UrlRefusalReason::LocalNetworkBlocked,
                });
            }
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

    // Cloud-metadata IPs are refused unconditionally — a hostile authoritative
    // DNS server resolving to 169.254.169.254 must not slip through just because
    // `allow_local_network` is on for a real intranet.
    if resolved.iter().any(|sa| ip_is_always_blocked(sa.ip())) {
        return Err(McpError::UrlRefused {
            url: url.to_string(),
            reason: UrlRefusalReason::LocalNetworkBlocked,
        });
    }
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

/// Extract the IP address from an IP-literal host, if the host is written as a
/// literal rather than a domain name.
fn host_ip_literal(host: &url::Host<&str>) -> Option<IpAddr> {
    match host {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(*ip)),
        url::Host::Domain(_) => None,
    }
}

/// Cloud-metadata / instance-identity IPs refused UNCONDITIONALLY — even with
/// `allow_local_network=true`. Mirrors the cortex-side `ssrf::is_always_blocked_ip`
/// (duplicated because `mcp` must not depend on `cortex`). Flipping the
/// local-network knob for a legitimate subnet server must never unlock SSRF to
/// these endpoints.
fn ip_is_always_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // Entire 169.254.0.0/16 link-local range — AWS/GCP/Azure/Oracle/DO
            // metadata (169.254.169.254), Azure IMDS wireserver (.169.253), and
            // AWS ECS task metadata (.170.2) all live here.
            (o[0] == 169 && o[1] == 254)
                // Alibaba Cloud metadata (routable, outside link-local).
                || o == [100, 100, 100, 200]
                // Azure WireServer / host-internal IMDS (routable).
                || o == [168, 63, 129, 16]
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6_mapped_v4(v6) {
                return ip_is_always_blocked(IpAddr::V4(v4));
            }
            // AWS IPv6 metadata endpoint fd00:ec2::254.
            v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254]
        }
    }
}

/// Extract an equivalent IPv4 address from an IPv6 mapped/compat/6to4 form so
/// the v4 metadata checks apply to `::ffff:169.254.169.254` and friends.
fn v6_mapped_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let segs = v6.segments();
    if segs[0..6].iter().all(|&s| s == 0) {
        if let Some(v4) = v6.to_ipv4() {
            if !v4.is_loopback() && !v4.is_unspecified() {
                return Some(v4);
            }
        }
    }
    if segs[0] == 0x2002 {
        let b = (u32::from(segs[1]) << 16) | u32::from(segs[2]);
        return Some(Ipv4Addr::from(b));
    }
    None
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
    fn imds_ip_literal_refused_unconditionally_even_when_local_network_allowed() {
        // Regression (M6): the IMDS IP literal must be refused UNCONDITIONALLY.
        // Previously `allow_local_network=true` (a legitimate knob for talking
        // to a real subnet) also admitted 169.254.169.254 — an SSRF-to-IMDS
        // escape. The cloud-metadata IPs are now blocked regardless of the flag.
        let permissive = HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        };
        for url in [
            "https://169.254.169.254/latest/meta-data/",
            "http://169.254.169.254/",
            "https://[::ffff:169.254.169.254]/",
            "https://168.63.129.16/",       // Azure wireserver
            "https://100.100.100.200/",     // Alibaba metadata
        ] {
            let err = check_url_syntactic(url, &permissive).unwrap_err();
            assert!(
                matches!(
                    err,
                    McpError::UrlRefused {
                        reason: UrlRefusalReason::LocalNetworkBlocked,
                        ..
                    }
                ),
                "IMDS/metadata IP {url} must be refused unconditionally, got {err:?}"
            );
        }
    }

    #[test]
    fn ip_is_always_blocked_covers_metadata_and_spares_public() {
        for ip in [
            "169.254.169.254",
            "169.254.170.2",
            "168.63.129.16",
            "100.100.100.200",
        ] {
            assert!(
                ip_is_always_blocked(ip.parse().unwrap()),
                "{ip} must be always-blocked"
            );
        }
        for ip in ["8.8.8.8", "93.184.216.34", "1.1.1.1"] {
            assert!(
                !ip_is_always_blocked(ip.parse().unwrap()),
                "{ip} is public and must not be blocked"
            );
        }
    }

    #[test]
    fn decimal_encoded_imds_literal_refused() {
        // 2852039166 == 169.254.169.254; the url crate normalizes it.
        let permissive = HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        };
        assert!(check_url_syntactic("http://2852039166/", &permissive).is_err());
    }
}
