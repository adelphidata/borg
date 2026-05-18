//! LAN/peer discovery — mDNS advertisement and Tailscale peer enumeration.
//!
//! - [`mdns`] publishes `_borgd._tcp.local.` so neighbouring borgds and
//!   `borg device pair --discover` can find this daemon without manual
//!   hostname plumbing. TXT records include the server-cert fingerprint
//!   so the pairing flow can pin the right key on first sight.
//! - [`tailscale`] shells out to `tailscale status --json` to enumerate
//!   tailnet peers; surfaces them in `borg device pair --discover`.

/// mDNS announcer and parsing helpers for `_borgd._tcp.local.`.
pub mod mdns;
/// Tailscale peer enumeration via the host `tailscale` CLI.
pub mod tailscale;
