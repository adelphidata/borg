//! Short-lived cache of endpoints whose TCP connects go nowhere.
//!
//! A routable-but-dead endpoint — a local server that isn't running, a LAN
//! address reachable only over a VPN that's currently down — blackholes SYN
//! packets. Nothing answers and nothing refuses, so every attempt waits out the
//! full connect timeout. With a fallback chain and a retry ladder on top, one
//! dead host can burn minutes of wall-clock per turn while the agent looks
//! hung.
//!
//! Recording the host for a short window collapses that burst into a single
//! stall: subsequent attempts fail immediately and failover moves on. The TTL
//! is deliberately short so a VPN coming back up is picked up on its own,
//! without a restart.
//!
//! Scope note: this tracks *host reachability*, which is a different axis from
//! [`crate::llm::ProviderCooldown`] (provider slot health) and the credential
//! pool's per-key cooldown (credential health). A host can be dead while its
//! credentials are perfectly good, and two providers can share one dead host.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a host stays marked dead.
///
/// Long enough to collapse one startup/failover burst, short enough that a
/// recovering network is retried within a turn or two rather than after a
/// restart.
pub(crate) const BLACKHOLE_TTL: Duration = Duration::from_secs(30);

/// Bound on how long a TCP connect may take before we call it dead.
///
/// Distinct from the request timeout, which covers the whole exchange
/// (including a slow model thinking for minutes). Establishing a connection is
/// fast or it is broken.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn registry() -> &'static Mutex<HashMap<String, Instant>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `host:port` for `url`, or `None` when it has no host to key on.
///
/// The port is part of the key: one host commonly runs a live service on one
/// port and nothing on another, and marking the whole host dead would take out
/// a working endpoint.
fn endpoint_key(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    match parsed.port_or_known_default() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_string()),
    }
}

/// Record that a connect attempt to `url`'s endpoint went unanswered.
///
/// Call this only for *connect* failures. A read timeout means the connection
/// was accepted and the server is merely slow — blackholing it would abandon a
/// live endpoint mid-request.
pub(crate) fn record_connect_failure(url: &str, now: Instant) {
    let Some(key) = endpoint_key(url) else {
        return;
    };
    if let Ok(mut map) = registry().lock() {
        map.insert(key, now);
    }
}

/// Whether `url`'s endpoint is currently marked dead.
///
/// Expired entries are dropped as they're observed, so the map stays bounded by
/// the number of distinct endpoints seen in one TTL window.
pub(crate) fn is_blackholed(url: &str, now: Instant) -> bool {
    let Some(key) = endpoint_key(url) else {
        return false;
    };
    let Ok(mut map) = registry().lock() else {
        // A poisoned lock must never wedge the request path shut; treat the
        // endpoint as reachable and let the real connect decide.
        return false;
    };
    match map.get(&key) {
        Some(recorded) if now.duration_since(*recorded) < BLACKHOLE_TTL => true,
        Some(_) => {
            map.remove(&key);
            false
        }
        None => false,
    }
}

/// Forget `url`'s endpoint. Called when a request to it succeeds, so a host
/// that recovers early isn't held down for the rest of the TTL.
pub(crate) fn clear(url: &str) {
    let Some(key) = endpoint_key(url) else {
        return;
    };
    if let Ok(mut map) = registry().lock() {
        map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries must expire on their own — a host that only looked dead because
    /// the VPN was down has to become reachable again without a restart.
    #[test]
    fn a_recorded_endpoint_is_dead_until_the_ttl_elapses() {
        let url = "https://ttl-probe.invalid:9443/v1/chat";
        let t0 = Instant::now();
        record_connect_failure(url, t0);

        assert!(is_blackholed(url, t0));
        assert!(is_blackholed(
            url,
            t0 + BLACKHOLE_TTL - Duration::from_millis(1)
        ));
        assert!(!is_blackholed(url, t0 + BLACKHOLE_TTL));
        // And the expired entry is gone, not merely reported stale.
        assert!(!is_blackholed(url, t0));
    }

    /// The key carries the port. A dead sidecar on :9444 must not take out a
    /// live service on :9443 of the same host.
    #[test]
    fn ports_are_distinct_endpoints() {
        let dead = "http://port-probe.invalid:9444/v1";
        let live = "http://port-probe.invalid:9443/v1";
        let now = Instant::now();
        record_connect_failure(dead, now);

        assert!(is_blackholed(dead, now));
        assert!(!is_blackholed(live, now));
    }

    /// Different paths on the same endpoint share one entry — the blackhole is
    /// a property of the socket, not the route.
    #[test]
    fn paths_on_one_endpoint_share_an_entry_and_clear_together() {
        let chat = "https://path-probe.invalid/v1/chat/completions";
        let models = "https://path-probe.invalid/v1/models";
        let now = Instant::now();
        record_connect_failure(chat, now);
        assert!(is_blackholed(models, now));

        clear(models);
        assert!(!is_blackholed(chat, now));
    }

    /// A URL with no host can't be keyed. It must be treated as reachable
    /// rather than silently blackholing every such request.
    #[test]
    fn unparseable_urls_are_never_blackholed() {
        let junk = "not a url";
        record_connect_failure(junk, Instant::now());
        assert!(!is_blackholed(junk, Instant::now()));
    }
}
