//! mDNS / Bonjour announcement for `_borgd._tcp.local.`.
//!
//! Publishes the daemon's TCP listener so peers on the same LAN can
//! discover it without manual hostname plumbing. TXT records include
//! the server-cert fingerprint so the pairing flow can pin the right
//! cert on first sight — without this, an attacker on the same LAN
//! could impersonate the announcement and route pairing through them.
//!
//! Backed by `mdns-sd` (pure-Rust, no system avahi/bonjour link).

use std::collections::HashMap;
use std::net::IpAddr;

use anyhow::{anyhow, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Service type advertised on the LAN. The trailing `.local.` is part of
/// the mDNS protocol; clients browse `_borgd._tcp.local.` and match.
pub const SERVICE_TYPE: &str = "_borgd._tcp.local.";

/// Live mDNS announcement. Dropping this calls [`Self::shutdown`]
/// implicitly via the inner `ServiceDaemon` — but call `shutdown()`
/// explicitly for predictable de-announce timing on graceful exit.
pub struct MdnsAnnouncer {
    daemon: ServiceDaemon,
    instance_fullname: String,
}

impl MdnsAnnouncer {
    /// Announce this borgd on the LAN.
    ///
    /// `instance_name` is the operator-visible name (e.g.
    /// `mac-mini-borgd`); the protocol fullname appended by `mdns-sd`
    /// is `<instance_name>._borgd._tcp.local.`.
    ///
    /// `fingerprint` is the hex SHA-256 of the DER-encoded server cert
    /// — pairing peers pin this on first sight to detect impersonation.
    pub fn announce(
        instance_name: &str,
        hostname: &str,
        port: u16,
        ips: &[IpAddr],
        fingerprint: &str,
        node_id: &str,
    ) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("mdns: create ServiceDaemon")?;
        let props = txt_records(fingerprint, node_id);
        // mdns-sd requires the hostname end with a dot. Normalize before
        // passing in so callers don't have to think about it.
        let host = if hostname.ends_with('.') {
            hostname.to_string()
        } else {
            format!("{hostname}.")
        };
        let info = ServiceInfo::new(SERVICE_TYPE, instance_name, &host, ips, port, Some(props))
            .map_err(|e| anyhow!("mdns: build ServiceInfo: {e}"))?;
        let instance_fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| anyhow!("mdns: register {instance_name}: {e}"))?;
        tracing::info!(
            "mdns: announced {instance_fullname} -> {hostname}:{port} (fp prefix {})",
            short_fp(fingerprint)
        );
        Ok(Self {
            daemon,
            instance_fullname,
        })
    }

    /// De-announce and stop the underlying mDNS daemon. Idempotent — a
    /// second call is a no-op.
    pub fn shutdown(self) {
        if let Err(e) = self.daemon.unregister(&self.instance_fullname) {
            tracing::debug!("mdns: unregister {}: {e}", self.instance_fullname);
        }
        if let Err(e) = self.daemon.shutdown() {
            tracing::debug!("mdns: shutdown: {e}");
        }
    }
}

/// Construct the TXT-record map for a borgd announcement. Pulled out as
/// a pure function so tests can assert the wire shape without spinning up
/// a `ServiceDaemon`.
///
/// Keys are intentionally short — mDNS TXT entries cap at 255 bytes per
/// k=v pair (and at total UDP packet size). `v=1` lets future versions
/// bump cleanly without breaking older browsers.
pub fn txt_records(fingerprint: &str, node_id: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("v".to_string(), "1".to_string());
    m.insert("fp".to_string(), fingerprint.to_string());
    m.insert("node".to_string(), node_id.to_string());
    m
}

/// Default instance name. The plan's choice — `<hostname>-borgd` — keeps
/// borgd announcements distinguishable from other services on a host that
/// might publish multiple `_*._tcp.local.` entries.
pub fn default_instance_name(hostname: &str) -> String {
    let base = hostname.trim_matches('.');
    if base.is_empty() {
        "borgd".to_string()
    } else {
        format!("{base}-borgd")
    }
}

fn short_fp(fp: &str) -> &str {
    &fp[..fp.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_records_carry_v1_fp_and_node() {
        // Wire-shape regression: pairing clients key off `v` (schema
        // version), `fp` (cert pin), and `node` (target node id). Drop
        // any one and pairing degrades — `fp` missing in particular
        // lets a same-LAN attacker substitute their cert undetected.
        let txt = txt_records("deadbeef", "node-123");
        assert_eq!(txt.get("v").map(String::as_str), Some("1"));
        assert_eq!(txt.get("fp").map(String::as_str), Some("deadbeef"));
        assert_eq!(txt.get("node").map(String::as_str), Some("node-123"));
        // Exactly three keys — adding new ones is a wire change and
        // should require updating this test consciously.
        assert_eq!(txt.len(), 3, "unexpected TXT keys: {txt:?}");
    }

    #[test]
    fn default_instance_name_appends_borgd_to_hostname() {
        assert_eq!(default_instance_name("mac-mini"), "mac-mini-borgd");
        // FQDN trailing dot is mDNS protocol noise — strip before use.
        assert_eq!(default_instance_name("mac-mini."), "mac-mini-borgd");
    }

    #[test]
    fn default_instance_name_falls_back_when_hostname_blank() {
        // Edge case: minimal containers sometimes have empty hostnames.
        // We still need a usable instance name; "borgd" is a sensible
        // default rather than an empty-string announcement that mDNS
        // would silently drop.
        assert_eq!(default_instance_name(""), "borgd");
        assert_eq!(default_instance_name("."), "borgd");
    }

    #[test]
    fn short_fp_truncates_to_eight_chars() {
        assert_eq!(short_fp("deadbeefcafebabe"), "deadbeef");
        // Short fingerprints (test fixtures) must not panic.
        assert_eq!(short_fp("abc"), "abc");
        assert_eq!(short_fp(""), "");
    }
}
