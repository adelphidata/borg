//! SSRF defense — host/IP blocklist used by [`super::classify`].
//!
//! The lists are intentionally narrow: only cloud-metadata endpoints and
//! link-local space. Everything else is gated by the per-IP-class check in
//! [`super::is_private_class`] which is in turn gated by the
//! `allow_private_urls` config toggle. Cloud-metadata IPs/hosts in this
//! module ignore that toggle — they are never legitimate agent targets.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Hostnames that always block, even when `allow_private_urls=true`.
/// These resolve (or are short-named to resolve) to cloud-metadata IPs.
///
/// Scope: hosts that *don't* live in the link-local range and so would
/// otherwise slip past the IP-class check. Hosts like
/// `169.254.169.254` are caught by the IP sweep — no need to list them
/// here. Punycode / IDNA homograph spoofing of these names is a known
/// limitation; the matcher is ASCII-literal.
pub fn is_always_blocked_host(host: &str) -> bool {
    matches!(
        host,
        // GCP
        "metadata.google.internal"
            | "metadata.goog"
            // DigitalOcean
            | "metadata.digitalocean.com"
            // Packet / Equinix Metal
            | "metadata.packet.net"
            // Tencent Cloud
            | "metadata.tencentyun.com"
            // Generic short-form ("metadata" / "meta-data") that some
            // DHCP search-paths auto-complete into a metadata host.
            | "metadata"
            | "meta-data"
    )
}

/// IPs/networks always blocked: cloud metadata endpoints and the entire
/// IPv4 link-local range (`169.254.0.0/16`). The link-local range as a
/// whole is included because no legitimate fetch target lives there.
///
/// IPv4-mapped IPv6 (`::ffff:x.x.x.x`) and IPv4-compatible IPv6
/// (`::x.x.x.x`) are unwrapped and re-checked against the v4 list so a
/// resolver returning `::ffff:169.254.169.254` cannot bypass us.
pub fn is_always_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_always_blocked_v4(*v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = ipv6_to_v4_equivalent(*v6) {
                return is_always_blocked_v4(v4);
            }
            is_always_blocked_v6(*v6)
        }
    }
}

fn is_always_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    // Whole IPv4 link-local range — covers AWS/GCP/Azure/Oracle/DO
    // metadata (169.254.169.254), Azure IMDS wire server
    // (169.254.169.253), and AWS ECS task metadata (169.254.170.2).
    if o[0] == 169 && o[1] == 254 {
        return true;
    }
    // Alibaba Cloud metadata (sits outside link-local).
    if o == [100, 100, 100, 200] {
        return true;
    }
    // Azure WireServer / host-internal IMDS (NOT link-local — routable).
    if o == [168, 63, 129, 16] {
        return true;
    }
    false
}

fn is_always_blocked_v6(v6: Ipv6Addr) -> bool {
    // AWS IPv6 metadata endpoint: fd00:ec2::254
    v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254]
}

/// Public version of [`ipv6_to_v4_equivalent`] used by the private-IP
/// gate in [`super::is_private_class`].
pub(crate) fn ipv6_to_v4_for_private_check(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    ipv6_to_v4_equivalent(v6)
}

/// Extract an equivalent IPv4 address from an IPv6 mapped/compat form so
/// the v4 checks apply. Covers:
///   - `::ffff:a.b.c.d` (IPv4-mapped)
///   - `::a.b.c.d`      (IPv4-compatible, deprecated but accepted)
///   - `2002:abcd:ef01::*` (6to4 — `a.b.c.d` encoded in segments 1-2)
fn ipv6_to_v4_equivalent(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    // `to_ipv4()` returns Some for both mapped and compatible forms, but
    // also for some well-known v6 addresses like `::1`; we already
    // handled mapped above, so only treat the remaining case as compat
    // when the high 96 bits are zero.
    let segs = v6.segments();
    if segs[0..6].iter().all(|&s| s == 0) {
        if let Some(v4) = v6.to_ipv4() {
            // Skip ::1 (loopback) and :: (unspecified) — handled elsewhere.
            if !v4.is_loopback() && !v4.is_unspecified() {
                return Some(v4);
            }
        }
    }
    // 6to4: 2002::/16 with the next 32 bits being the embedded v4.
    if segs[0] == 0x2002 {
        let b = u32::from(segs[1]) << 16 | u32::from(segs[2]);
        return Some(Ipv4Addr::from(b));
    }
    None
}

/// `100.64.0.0/10` — CGNAT / shared address space (RFC 6598). Not covered by
/// `Ipv4Addr::is_private`. Used by Tailscale and carrier-grade NAT.
pub fn is_cgnat_v4(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000 // 64..=127
}

/// Resolve `host` to a list of IPs using `tokio::net::lookup_host`.
/// Returns the unique set of resolved addresses. We append a dummy port so
/// `lookup_host` accepts the input — the port is discarded.
pub async fn resolve_host(host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
    // If `host` is itself an IP literal, skip DNS entirely.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    let target = format!("{host}:80");
    let mut out = Vec::new();
    for addr in tokio::net::lookup_host(target).await? {
        let ip = addr.ip();
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}
