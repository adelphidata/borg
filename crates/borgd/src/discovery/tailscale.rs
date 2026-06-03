//! Tailscale peer enumeration via the host `tailscale` CLI.
//!
//! Used by `borg device pair --discover` to surface tailnet peers as
//! pairing candidates alongside mDNS-discovered borgds. The CLI is the
//! supported interface — Tailscale's `tailscaled` socket protocol is not
//! a public API and changes between releases.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const TAILSCALE_INSTALL_URL: &str = "https://tailscale.com/download";
// Cap the `tailscale status` probe so a hung tailscaled can't stall discovery.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// One peer in the local tailnet, as surfaced by `tailscale status --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetPeer {
    /// Tailscale-issued FQDN (trailing dot stripped) — e.g.
    /// `phone.tail-net-abc.ts.net`.
    pub dns_name: String,
    /// Display hostname as registered with the tailnet.
    pub host_name: String,
    /// Tailnet IPs (typically one v4 and one v6).
    pub addrs: Vec<String>,
    /// OS string reported by tailscaled (`macOS`, `iOS`, `linux`, …).
    pub os: String,
    /// `true` if this peer is the local node (`Self` in the JSON).
    pub is_self: bool,
}

/// Enumerate this tailnet's peers (including `Self`). Returns an empty
/// list when tailscale is signed out / not running, not an error — the
/// caller wants "nobody to show" in that case, not a panic.
pub async fn list_tailnet_peers() -> Result<Vec<TailnetPeer>> {
    if which::which("tailscale").is_err() {
        bail!("`tailscale` not found on PATH. Install: {TAILSCALE_INSTALL_URL}");
    }
    let output = timeout(
        STATUS_TIMEOUT,
        Command::new("tailscale")
            .args(["status", "--json"])
            .output(),
    )
    .await
    .map_err(|_| anyhow!("`tailscale status --json` timed out after 10s"))?
    .context("spawn `tailscale status --json`")?;

    if !output.status.success() {
        // BackendState=NeedsLogin is the most common "fail" path here;
        // surface stderr verbatim so the operator sees `tailscale up`.
        bail!(
            "`tailscale status` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    parse_tailnet_peers(&output.stdout)
}

/// Pure parser for the `tailscale status --json` output. Extracted so
/// tests can pin the wire shape without needing tailscale installed.
pub fn parse_tailnet_peers(json: &[u8]) -> Result<Vec<TailnetPeer>> {
    let value: Value =
        serde_json::from_slice(json).map_err(|e| anyhow!("parse tailscale status JSON: {e}"))?;

    let mut peers = Vec::new();

    if let Some(self_obj) = value.get("Self") {
        if let Some(p) = peer_from_node(self_obj, true) {
            peers.push(p);
        }
    }
    if let Some(peer_map) = value.get("Peer").and_then(Value::as_object) {
        // BTreeMap-style ordering isn't guaranteed by serde_json, but the
        // ordering inside the JSON object is preserved. Iterate stable.
        for (_, node) in peer_map {
            if let Some(p) = peer_from_node(node, false) {
                peers.push(p);
            }
        }
    }
    Ok(peers)
}

fn peer_from_node(node: &Value, is_self: bool) -> Option<TailnetPeer> {
    let dns_name = node
        .get("DNSName")
        .and_then(Value::as_str)?
        .trim_end_matches('.')
        .to_string();
    let host_name = node
        .get("HostName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let os = node
        .get("OS")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let addrs = node
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(TailnetPeer {
        dns_name,
        host_name,
        addrs,
        os,
        is_self,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canned `tailscale status --json` from a 2-node tailnet. Trimmed of
    /// fields irrelevant to pairing discovery but the schema matches
    /// what tailscale 1.x prints (Self + Peer map + TailscaleIPs).
    const REAL_STATUS_JSON: &[u8] = br#"{
      "Version": "1.78.1",
      "BackendState": "Running",
      "Self": {
        "ID": "abc123",
        "HostName": "mac-mini",
        "DNSName": "mac-mini.tail-net-abc.ts.net.",
        "OS": "macOS",
        "TailscaleIPs": ["100.64.0.1", "fd7a::1"]
      },
      "Peer": {
        "xyz789": {
          "HostName": "phone",
          "DNSName": "phone.tail-net-abc.ts.net.",
          "OS": "iOS",
          "TailscaleIPs": ["100.64.0.2"]
        }
      }
    }"#;

    #[test]
    fn parse_extracts_self_first_with_is_self_flag_set() {
        let peers = parse_tailnet_peers(REAL_STATUS_JSON).unwrap();
        assert_eq!(peers.len(), 2, "expected Self + 1 peer; got {peers:?}");
        let me = &peers[0];
        assert!(me.is_self, "first entry must be Self");
        assert_eq!(me.dns_name, "mac-mini.tail-net-abc.ts.net");
        assert_eq!(me.host_name, "mac-mini");
        assert_eq!(me.os, "macOS");
        // Both v4 and v6 must come through — pairing UIs prefer v4 for
        // display but the v6 is sometimes the only routable address.
        assert!(me.addrs.contains(&"100.64.0.1".to_string()));
        assert!(me.addrs.contains(&"fd7a::1".to_string()));
    }

    #[test]
    fn parse_strips_trailing_fqdn_dot_from_dns_names() {
        // Real failure mode: leaving the trailing `.` produces an
        // invalid SNI hostname when the operator dials the peer. Both
        // Self and Peer DNSName fields must be normalized.
        let peers = parse_tailnet_peers(REAL_STATUS_JSON).unwrap();
        for p in &peers {
            assert!(
                !p.dns_name.ends_with('.'),
                "dns_name must have no trailing dot: {p:?}"
            );
        }
    }

    #[test]
    fn parse_marks_only_self_as_is_self_not_peers() {
        let peers = parse_tailnet_peers(REAL_STATUS_JSON).unwrap();
        let me_count = peers.iter().filter(|p| p.is_self).count();
        assert_eq!(me_count, 1, "exactly one Self expected, got {me_count}");
        // The non-Self entry must NOT have `is_self=true` — that would
        // confuse the pair UI into offering the local node as a peer.
        let phone = peers.iter().find(|p| p.host_name == "phone").unwrap();
        assert!(!phone.is_self, "remote peer must not be flagged is_self");
        assert_eq!(phone.os, "iOS");
    }

    #[test]
    fn parse_returns_empty_list_when_peer_map_absent() {
        // `tailscale status --json` with only Self (no peers yet) must
        // still produce a usable result — returning Err would push the
        // CLI into an error path for a normal "fresh tailnet" state.
        let json = br#"{
          "Self": {
            "HostName": "only-host",
            "DNSName": "only-host.tail-net.ts.net.",
            "OS": "linux",
            "TailscaleIPs": ["100.64.0.10"]
          }
        }"#;
        let peers = parse_tailnet_peers(json).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].host_name, "only-host");
    }

    #[test]
    fn parse_errors_on_invalid_json() {
        let err = parse_tailnet_peers(b"not json").unwrap_err();
        assert!(format!("{err}").contains("parse tailscale status JSON"));
    }

    #[test]
    fn parse_skips_node_missing_dns_name_without_failing_whole_call() {
        // A peer mid-handshake can briefly lack DNSName. Returning Err
        // for the whole call would make the discovery UI useless during
        // that window; instead we silently drop the malformed peer.
        let json = br#"{
          "Self": {
            "HostName": "me",
            "DNSName": "me.ts.net.",
            "OS": "macOS",
            "TailscaleIPs": ["100.64.0.1"]
          },
          "Peer": {
            "incomplete": {
              "HostName": "in-progress",
              "OS": "linux"
            }
          }
        }"#;
        let peers = parse_tailnet_peers(json).unwrap();
        assert_eq!(peers.len(), 1, "incomplete peer must be skipped");
        assert!(peers[0].is_self);
    }
}
