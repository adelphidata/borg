//! Fail-closed endpoint gate for external memory providers.
//!
//! Every external provider sends its API key to an operator-configured
//! `base_url`. Without a gate, a poisoned setting (or a typo) points that
//! credential at a cloud metadata service (`169.254.169.254`,
//! `metadata.google.internal`, …) or ships it in cleartext to a public
//! host. Two layers, both fail-closed:
//!
//! 1. [`validate_endpoint_syntax`] — cheap, sync, at provider
//!    construction. A rejected URL surfaces as [`MemoryError::NotConfigured`]
//!    so the registry falls back to Native with a one-shot warn.
//! 2. [`check_request_url`] — async, at every request, after DNS. Catches
//!    hostnames that *resolve* into blocked space (rebinding, poisoned
//!    resolver), which the constructor check cannot see.
//!
//! Policy: metadata endpoints are always blocked, by name and by resolved
//! address. Plain `http` is permitted only toward loopback/private
//! space — a self-hosted memory server on the LAN is the legitimate case —
//! and every other target must use `https` so the credential never travels
//! in cleartext.
//!
//! Known limitation (TOCTOU): [`check_request_url`] resolves the host and
//! reqwest re-resolves it at connect time, so a rebinding resolver that
//! answers clean here and blocked-space at connect is not fully closed off.
//! Closing it would need connect-to-pinned-IP (`ClientBuilder::resolve`);
//! the same residual risk is accepted on the LLM-side gates.

use super::MemoryError;
use crate::url_policy::ssrf;

/// Validate an operator-supplied base URL at provider construction.
/// Returns the parsed URL so callers can keep the normalized form.
pub(crate) fn validate_endpoint_syntax(
    raw: &str,
    provider: &'static str,
) -> Result<url::Url, MemoryError> {
    let not_configured = |detail: String| MemoryError::NotConfigured { provider, detail };

    let trimmed = raw.trim();
    let url = url::Url::parse(trimmed)
        .map_err(|e| not_configured(format!("base_url is not a valid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(not_configured(format!(
            "base_url scheme must be http or https (got {}://)",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| not_configured("base_url has no host".to_string()))?
        .to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if ssrf::is_always_blocked_host(bare) {
        return Err(not_configured(format!(
            "base_url host '{bare}' is a cloud metadata endpoint; refusing to send credentials there"
        )));
    }
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if ssrf::is_always_blocked_ip(&ip) {
            return Err(not_configured(format!(
                "base_url address {ip} is a cloud metadata / link-local endpoint; \
                 refusing to send credentials there"
            )));
        }
        if url.scheme() == "http" && !crate::url_policy::is_private_class(&ip) {
            return Err(not_configured(format!(
                "base_url {ip} is publicly routable but uses plain http; \
                 the API key would travel in cleartext — use https"
            )));
        }
    }
    // A plain-http *hostname* (e.g. `http://nas.local:8080`) is deliberately
    // ACCEPTED here: the constructor is sync and cannot resolve DNS, and
    // rejecting it would break existing LAN configs. Safety is preserved by
    // [`check_request_url`], which resolves the name on every request and
    // refuses http toward anything that is not loopback/private space.
    Ok(url)
}

/// Per-request gate, applied after the request is built and before it is
/// sent. Resolves the host and refuses when any resolved address lands in
/// always-blocked (metadata / link-local) space. DNS failure is refusal:
/// the send would fail anyway, and guessing is how rebinding wins.
pub(crate) async fn check_request_url(
    url: &url::Url,
    provider: &'static str,
) -> Result<(), MemoryError> {
    let host = url
        .host_str()
        .ok_or_else(|| MemoryError::Transport {
            provider,
            detail: "request URL has no host".to_string(),
        })?
        .to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if ssrf::is_always_blocked_host(bare) {
        return Err(blocked(provider, bare));
    }
    let addrs = ssrf::resolve_host(bare)
        .await
        .map_err(|e| MemoryError::Transport {
            provider,
            detail: format!("endpoint host '{bare}' did not resolve: {e}"),
        })?;
    if addrs.is_empty() {
        return Err(MemoryError::Transport {
            provider,
            detail: format!("endpoint host '{bare}' resolved to no addresses"),
        });
    }
    if addrs.iter().any(ssrf::is_always_blocked_ip) {
        return Err(blocked(provider, bare));
    }
    // Cleartext http may only target loopback/private space. The syntax
    // gate already restricts http to localhost/private literals, but a
    // poisoned resolver can point "localhost-like" names anywhere.
    if url.scheme() == "http" && !addrs.iter().all(crate::url_policy::is_private_class) {
        return Err(MemoryError::Transport {
            provider,
            detail: format!(
                "endpoint host '{bare}' uses plain http but resolves to a \
                 publicly routable address; refusing to send the API key in cleartext"
            ),
        });
    }
    Ok(())
}

fn blocked(provider: &'static str, host: &str) -> MemoryError {
    MemoryError::Transport {
        provider,
        detail: format!(
            "endpoint host '{host}' is (or resolves to) a cloud metadata / \
             link-local address; refusing to send credentials there"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "test";

    fn syntax_ok(url: &str) -> bool {
        validate_endpoint_syntax(url, P).is_ok()
    }

    #[test]
    fn syntax_gate_verdicts() {
        // (url, allowed, why)
        let cases: &[(&str, bool)] = &[
            // Metadata endpoints: always blocked, both schemes.
            ("http://169.254.169.254/latest/meta-data/", false),
            ("https://169.254.169.254/", false),
            (
                "https://metadata.google.internal/computeMetadata/v1/",
                false,
            ),
            ("http://metadata/", false),
            ("https://[fd00:ec2::254]/", false),
            ("https://168.63.129.16/", false),
            // Plain http: localhost / private literals pass; public IP
            // literals fail here. Hostnames pass the sync gate (LAN names
            // like nas.local can't be resolved synchronously) and are
            // enforced per-request by check_request_url after DNS.
            ("http://localhost:8080", true),
            ("http://api.localhost:8080", true),
            ("http://127.0.0.1:1234", true),
            ("http://[::1]:1234", true),
            ("http://192.168.1.50:8000", true),
            ("http://10.2.3.4:9000", true),
            ("http://mem0.example.com/", true),
            ("http://8.8.8.8/", false),
            // https to a normal public host: allowed.
            ("https://api.mem0.ai", true),
            ("https://api.getzep.com/", true),
            // https to a private host: allowed (self-hosted TLS).
            ("https://192.168.1.50:8443", true),
            // Garbage.
            ("not a url", false),
            ("ftp://api.mem0.ai", false),
        ];
        for (url, allowed) in cases {
            assert_eq!(
                syntax_ok(url),
                *allowed,
                "syntax verdict mismatch for {url}: expected allowed={allowed}"
            );
        }
    }

    #[tokio::test]
    async fn request_gate_blocks_metadata_ip_literal() {
        let url = url::Url::parse("https://169.254.170.2/creds").unwrap();
        let err = check_request_url(&url, P).await.expect_err("must block");
        assert!(
            err.to_string().contains("metadata"),
            "error must name the metadata block: {err}"
        );
    }

    #[tokio::test]
    async fn request_gate_allows_loopback_http() {
        let url = url::Url::parse("http://127.0.0.1:8321/v1").unwrap();
        check_request_url(&url, P)
            .await
            .expect("loopback http must pass the request gate");
    }
}
