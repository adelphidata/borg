//! Pre-flight URL classifier for outbound web tools.
//!
//! Every URL that `web_fetch` or `browser.navigate` would request is run
//! through [`classify`] first. Composed gates (in order):
//!
//! 1. **Kill switch** — `enabled=false` short-circuits to `Allow`.
//! 2. **Scheme** — only `http`/`https` pass.
//! 3. **SSRF host blocklist** — cloud-metadata hostnames are blocked even
//!    with `allow_private_urls=true`. Cannot be overridden.
//! 4. **Transforms** — applied in declared order to produce the
//!    `effective_url` returned in `Allow`.
//! 5. **Allowlist** — when non-empty, the host must match.
//! 6. **Denylist** — host must not match.
//! 7. **DNS + IP class** — resolved IPs checked against cloud-metadata IPs
//!    (always blocked) and the private-IP family (blocked unless
//!    `allow_private_urls`). DNS failure is `Block` (fail-closed).
//! 8. **Throttle** — per-host min-interval enforced via in-memory map.
//!
//! [`classify_offline`] is a no-DNS, no-throttle variant used by the
//! `reqwest` redirect callback (full DNS on every hop would be too slow);
//! `web_fetch` re-runs the full [`classify`] on the final URL after the
//! response lands to defend against DNS rebinding.

use std::net::IpAddr;

use crate::config::UrlPolicyConfig;

pub mod rules;
pub mod ssrf;
pub mod throttle;
pub mod transforms;

#[cfg(test)]
mod tests;

/// Result of running a URL through the policy pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyOutcome {
    /// URL is approved. Callers MUST request `effective_url`, not the input,
    /// because transforms may have rewritten it.
    Allow { effective_url: String },
    /// A throttle rule matched. Caller should back off for `retry_after_ms`.
    Throttle { retry_after_ms: u64, host: String },
    /// URL rejected. `rule` identifies which rule fired (for logging/UX);
    /// `reason` is a human-readable sentence.
    Block { reason: String, rule: String },
}

impl ClassifyOutcome {
    /// Convenience: build a `Block` with a `reason` and `rule` slug.
    fn block(reason: impl Into<String>, rule: impl Into<String>) -> Self {
        Self::Block {
            reason: reason.into(),
            rule: rule.into(),
        }
    }
}

/// Inspect a URL against `cfg`, performing DNS resolution and updating
/// throttle bookkeeping. Use this for the actual pre-flight check.
pub async fn classify(url: &str, cfg: &UrlPolicyConfig) -> ClassifyOutcome {
    classify_inner(url, cfg, /*record_throttle=*/ true).await
}

/// Variant of [`classify`] that performs every check (scheme, SSRF, DNS,
/// allow/deny) but does NOT advance the throttle bucket. Used by
/// `web_fetch`'s post-fetch DNS-rebinding recheck so the same request
/// doesn't double-charge throttle.
pub async fn classify_no_throttle(url: &str, cfg: &UrlPolicyConfig) -> ClassifyOutcome {
    classify_inner(url, cfg, /*record_throttle=*/ false).await
}

async fn classify_inner(
    url: &str,
    cfg: &UrlPolicyConfig,
    record_throttle: bool,
) -> ClassifyOutcome {
    // 1. Kill switch
    if !cfg.enabled {
        return ClassifyOutcome::Allow {
            effective_url: url.to_string(),
        };
    }

    // 2. Parse + scheme
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => {
            return ClassifyOutcome::block(format!("Unparseable URL: {e}"), "malformed_url");
        }
    };
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return ClassifyOutcome::block(
            format!("Scheme '{scheme}' is not allowed (only http/https)"),
            "scheme_blocked",
        );
    }
    let host_in = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return ClassifyOutcome::block("URL has no host", "missing_host"),
    };
    let host_lower = rules::normalize_host(&host_in);

    // 3. SSRF host blocklist (hard, before transforms — can't be transformed away)
    if ssrf::is_always_blocked_host(&host_lower) {
        return ClassifyOutcome::block(
            format!("Host '{host_lower}' is a cloud-metadata endpoint"),
            "ssrf_blocked_host",
        );
    }

    // 4. Transforms
    let transformed = match transforms::apply(&parsed, &cfg.transform_rules) {
        Ok(u) => u,
        Err(e) => {
            return ClassifyOutcome::block(format!("Transform failed: {e}"), "transform_error");
        }
    };
    let effective_host_in = transformed
        .host_str()
        .map(str::to_string)
        .unwrap_or_default();
    let effective_host = rules::normalize_host(&effective_host_in);
    if effective_host.is_empty() {
        return ClassifyOutcome::block("Transform produced URL without host", "transform_error");
    }

    // Re-check SSRF after transforms (a RewriteHost transform could try to
    // route to a metadata host — close the loop).
    if ssrf::is_always_blocked_host(&effective_host) {
        return ClassifyOutcome::block(
            format!("Transformed host '{effective_host}' is a cloud-metadata endpoint"),
            "ssrf_blocked_host",
        );
    }

    // 5. Allowlist
    if !cfg.allowlist.is_empty() && !rules::any_match(&effective_host, &cfg.allowlist) {
        return ClassifyOutcome::block(
            format!("Host '{effective_host}' is not in the allowlist"),
            "not_in_allowlist",
        );
    }

    // 6. Denylist
    if let Some(pat) = rules::first_match(&effective_host, &cfg.denylist) {
        return ClassifyOutcome::block(
            format!("Host '{effective_host}' matches denylist rule '{pat}'"),
            pat,
        );
    }

    // 7. DNS + IP class
    let ips = match ssrf::resolve_host(&effective_host).await {
        Ok(v) => v,
        Err(e) => {
            return ClassifyOutcome::block(
                format!("DNS resolution failed for '{effective_host}': {e}"),
                "dns_failed",
            );
        }
    };
    if ips.is_empty() {
        return ClassifyOutcome::block(
            format!("DNS returned no addresses for '{effective_host}'"),
            "dns_empty",
        );
    }
    for ip in &ips {
        if ssrf::is_always_blocked_ip(ip) {
            return ClassifyOutcome::block(
                format!("Host '{effective_host}' resolves to blocked address {ip}"),
                "ssrf_blocked_ip",
            );
        }
        if !cfg.allow_private_urls && is_private_class(ip) {
            return ClassifyOutcome::block(
                format!("Host '{effective_host}' resolves to private address {ip}"),
                "private_ip",
            );
        }
    }

    // 8. Throttle — key by host:port so two services on the same host
    // (and tests with multiple wiremock servers) get isolated buckets.
    if record_throttle {
        let throttle_key = match transformed.port_or_known_default() {
            Some(p) => format!("{effective_host}:{p}"),
            None => effective_host.clone(),
        };
        if let Some((pattern, retry_after_ms)) =
            throttle::check_and_record(&throttle_key, &effective_host, &cfg.throttle_rules)
        {
            return ClassifyOutcome::Throttle {
                retry_after_ms,
                host: format!("{throttle_key} (rule: {pattern})"),
            };
        }
    }

    ClassifyOutcome::Allow {
        effective_url: transformed.into(),
    }
}

/// No-DNS, no-throttle, no-transform variant. Used by the `reqwest` redirect
/// callback to gate every hop without paying for re-resolution. Throttle is
/// skipped because every callback would otherwise debit the bucket.
/// Transforms are skipped because the response body has already been
/// committed to the original URL's redirect target.
pub fn classify_offline(url: &str, cfg: &UrlPolicyConfig) -> ClassifyOutcome {
    if !cfg.enabled {
        return ClassifyOutcome::Allow {
            effective_url: url.to_string(),
        };
    }
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => {
            return ClassifyOutcome::block(format!("Unparseable URL: {e}"), "malformed_url");
        }
    };
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return ClassifyOutcome::block(
            format!("Scheme '{scheme}' is not allowed (only http/https)"),
            "scheme_blocked",
        );
    }
    let host_in = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return ClassifyOutcome::block("URL has no host", "missing_host"),
    };
    let host = rules::normalize_host(&host_in);

    if ssrf::is_always_blocked_host(&host) {
        return ClassifyOutcome::block(
            format!("Host '{host}' is a cloud-metadata endpoint"),
            "ssrf_blocked_host",
        );
    }
    // Literal-IP hosts (e.g. http://10.0.0.1/) can be classified without DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ssrf::is_always_blocked_ip(&ip) {
            return ClassifyOutcome::block(
                format!("Host resolves to blocked address {ip}"),
                "ssrf_blocked_ip",
            );
        }
        if !cfg.allow_private_urls && is_private_class(&ip) {
            return ClassifyOutcome::block(format!("Host is a private address {ip}"), "private_ip");
        }
    }
    if !cfg.allowlist.is_empty() && !rules::any_match(&host, &cfg.allowlist) {
        return ClassifyOutcome::block(
            format!("Host '{host}' is not in the allowlist"),
            "not_in_allowlist",
        );
    }
    if let Some(pat) = rules::first_match(&host, &cfg.denylist) {
        return ClassifyOutcome::block(format!("Host '{host}' matches denylist rule '{pat}'"), pat);
    }

    ClassifyOutcome::Allow {
        effective_url: url.to_string(),
    }
}

/// True if `ip` is in any of the private/loopback/link-local/CGNAT/multicast
/// families that we never want to fetch from by default. `std`'s
/// `Ipv4Addr::is_private` is *not* sufficient — it omits the CGNAT range
/// (`100.64.0.0/10`) which Tailscale and carrier-grade NAT use heavily.
///
/// IPv6 addresses that encode a v4 equivalent (mapped, compat, 6to4) are
/// unwrapped and re-checked as v4. Otherwise the bypass `::ffff:10.0.0.1`
/// would slip past the v6-only check below.
pub(crate) fn is_private_class(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(*v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = ssrf::ipv6_to_v4_for_private_check(*v6) {
                return is_private_v4(v4);
            }
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // unique-local (fc00::/7)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link-local (fe80::/10)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_unspecified()
        || ssrf::is_cgnat_v4(&v4)
        || v4.is_documentation()
}

/// Render a classify outcome as a single short string suitable for surfacing
/// to the LLM via a tool result.
pub fn describe_block_or_throttle(outcome: &ClassifyOutcome) -> Option<String> {
    match outcome {
        ClassifyOutcome::Allow { .. } => None,
        ClassifyOutcome::Block { reason, rule } => {
            Some(format!("URL policy blocked: {reason} (rule: {rule})"))
        }
        ClassifyOutcome::Throttle {
            retry_after_ms,
            host,
        } => Some(format!(
            "URL throttled for {host}: retry in {retry_after_ms}ms"
        )),
    }
}
