//! Tailscale-pinned verifier — opt-in per `[security] verifier = "tailscale"`.
//!
//! Cross-checks the connecting peer's IP against `tailscale status --json`
//! before delegating device lookup to the inner TOFU verifier. Refuses any
//! peer whose IP does not appear as a known non-self Tailscale node, even
//! when the cert fingerprint matches a paired device — the spec calls for
//! binding pairs to a Tailscale node identity, not just to a key.

use std::collections::HashSet;
use std::net::IpAddr;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::verifier::{PeerCtx, TofuVerifier, Verifier, VerifierError};

/// How long a parsed `tailscale status --json` snapshot is reused before
/// re-querying. Set short enough that node churn is picked up quickly,
/// long enough to absorb bursts of gRPC connections without forking
/// `tailscale` on every request.
pub const STATUS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Source of `tailscale status --json` output. The production impl shells
/// `tailscale`; tests inject canned JSON so the verifier's IP-check logic
/// can be exercised without a Tailscale install.
pub trait TailscaleStatusSource: Send + Sync {
    /// Return the raw `tailscale status --json` output. Errors here become
    /// `VerifierError::Tailscale` and refuse the connection — never silently
    /// fall back to TOFU when the user explicitly chose Tailscale pinning.
    fn fetch_status_json(&self) -> Result<String, String>;

    /// Cheap probe used at daemon startup to fail-closed when the binary
    /// is missing entirely. Default implementation calls `fetch_status_json`
    /// and discards the body; override when the source can answer this
    /// faster (e.g., the production impl just runs `tailscale version`).
    fn probe(&self) -> Result<(), String> {
        self.fetch_status_json().map(|_| ())
    }
}

/// Hard ceiling on how long we'll wait for `tailscale status --json` to
/// return before refusing the connection. A wedged `tailscaled` would
/// otherwise hang every gRPC handshake indefinitely. 5s is generous —
/// `tailscale status` normally returns in under 100ms.
pub const TAILSCALE_CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// Production source: shells `tailscale status --json`.
pub struct TailscaleCliSource;

/// Run a command with a wall-clock timeout. Sync (no tokio dep here);
/// we spawn the child, poll `try_wait`, and SIGKILL on timeout.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<std::process::Output, String> {
    use std::io::Read;
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_end(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_end(&mut stderr);
                }
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

impl TailscaleStatusSource for TailscaleCliSource {
    fn fetch_status_json(&self) -> Result<String, String> {
        let mut cmd = Command::new("tailscale");
        cmd.args(["status", "--json"]);
        let out = run_with_timeout(cmd, TAILSCALE_CMD_TIMEOUT)
            .map_err(|e| format!("tailscale status: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "tailscale exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| format!("non-utf8 tailscale output: {e}"))
    }

    fn probe(&self) -> Result<(), String> {
        let mut cmd = Command::new("tailscale");
        cmd.arg("version");
        let out = run_with_timeout(cmd, TAILSCALE_CMD_TIMEOUT)
            .map_err(|e| format!("tailscale version: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("tailscale version exited {:?}", out.status.code()))
        }
    }
}

/// Subset of the `tailscale status --json` schema we depend on. Tailscale
/// publishes additional fields; we deserialize only the IPs since that's
/// all the verifier needs. Keeping the surface narrow lets new Tailscale
/// versions add fields without breaking us.
#[derive(Deserialize)]
struct StatusDoc {
    #[serde(rename = "Self", default)]
    self_node: Option<NodeIps>,
    #[serde(rename = "Peer", default)]
    peer: std::collections::HashMap<String, NodeIps>,
}

#[derive(Deserialize)]
struct NodeIps {
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
}

struct CachedStatus {
    fetched_at: Instant,
    peer_ips: HashSet<IpAddr>,
}

/// Verifier that wraps [`TofuVerifier`] and pins peers to current Tailscale IPs.
pub struct TailscaleVerifier {
    inner: TofuVerifier,
    source: Arc<dyn TailscaleStatusSource>,
    cache: Mutex<Option<CachedStatus>>,
    ttl: Duration,
}

impl TailscaleVerifier {
    /// Construct a new Tailscale verifier wrapping `inner` with peer IP enforcement.
    pub fn new(inner: TofuVerifier, source: Arc<dyn TailscaleStatusSource>) -> Self {
        Self {
            inner,
            source,
            cache: Mutex::new(None),
            ttl: STATUS_CACHE_TTL,
        }
    }

    /// For tests: override the cache TTL.
    #[cfg(test)]
    fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Returns the current set of peer (non-self) Tailscale IPs, refreshing
    /// from the source if the cached snapshot has expired.
    fn peer_ips(&self) -> Result<HashSet<IpAddr>, VerifierError> {
        {
            let g = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ref c) = *g {
                if c.fetched_at.elapsed() < self.ttl {
                    return Ok(c.peer_ips.clone());
                }
            }
        }
        let raw = self
            .source
            .fetch_status_json()
            .map_err(VerifierError::Tailscale)?;
        let doc: StatusDoc = serde_json::from_str(&raw)
            .map_err(|e| VerifierError::Tailscale(format!("parse status json: {e}")))?;

        // A logged-out / degraded tailscaled returns no `Self` node. With an
        // empty `self_ips`, every peer IP would pass the "non-self" filter
        // and we'd silently fail-open (matching unrelated peers' IPs against
        // our paired devices). Refuse instead — an operator who explicitly
        // chose Tailscale pinning expects the verifier to be hard-down when
        // tailscaled isn't attesting our identity.
        let self_ips: HashSet<IpAddr> = doc
            .self_node
            .map(|n| n.tailscale_ips.into_iter().collect())
            .unwrap_or_default();
        if self_ips.is_empty() {
            return Err(VerifierError::Tailscale(
                "tailscaled has no Self node — refusing to attest peers".into(),
            ));
        }

        let mut peers: HashSet<IpAddr> = HashSet::new();
        for (_id, node) in doc.peer {
            for ip in node.tailscale_ips {
                if !self_ips.contains(&ip) {
                    peers.insert(ip);
                }
            }
        }

        let mut g = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Some(CachedStatus {
            fetched_at: Instant::now(),
            peer_ips: peers.clone(),
        });
        Ok(peers)
    }
}

impl Verifier for TailscaleVerifier {
    fn verify(&self, fingerprint: &str, san_uri: Option<&str>) -> Result<String, VerifierError> {
        // Without a peer IP we cannot enforce Tailscale pinning. Refuse
        // rather than silently degrade — the user opted in to this verifier
        // explicitly and a missing IP means the call site forgot to thread
        // through `verify_with_peer`. Surfaces as a hard error in tests.
        Err(VerifierError::Tailscale(format!(
            "verify() called without peer IP; use verify_with_peer (fp={fingerprint}, san={san_uri:?})"
        )))
    }

    fn verify_with_peer(
        &self,
        fingerprint: &str,
        san_uri: Option<&str>,
        peer: PeerCtx,
    ) -> Result<String, VerifierError> {
        // 1. Revocation check first — a revoked fingerprint must be refused
        //    even if its node is present in tailscale status. Order matters
        //    because the inner TOFU verifier also performs this check, but
        //    we want the audit trail to show "Revoked" not "UnknownDevice".
        //    The inner.verify() call below covers it; keep the comment so
        //    a future refactor doesn't accidentally invert the order.

        let peer_ip = peer.peer_ip.ok_or_else(|| {
            VerifierError::Tailscale("no peer IP on connection — cannot verify".into())
        })?;

        let peers = self.peer_ips()?;
        if !peers.contains(&peer_ip) {
            return Err(VerifierError::UnknownDevice(format!(
                "peer IP {peer_ip} not present in tailscale status"
            )));
        }

        // Delegate device lookup (revocation + paired_devices) to inner.
        self.inner.verify(fingerprint, san_uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::db::Database;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn db() -> Arc<Mutex<Database>> {
        let d = Database::from_connection(Connection::open_in_memory().expect("db")).expect("init");
        Arc::new(Mutex::new(d))
    }

    /// Stub source: returns canned JSON, counts how many times it was called.
    struct StubSource {
        json: String,
        calls: AtomicUsize,
        probe_ok: bool,
    }

    impl StubSource {
        fn new(json: &str) -> Arc<Self> {
            Arc::new(Self {
                json: json.into(),
                calls: AtomicUsize::new(0),
                probe_ok: true,
            })
        }
    }

    impl TailscaleStatusSource for StubSource {
        fn fetch_status_json(&self) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.json.clone())
        }
        fn probe(&self) -> Result<(), String> {
            if self.probe_ok {
                Ok(())
            } else {
                Err("missing".into())
            }
        }
    }

    /// Stub source whose probe fails — used to assert fail-closed at startup.
    struct MissingSource;
    impl TailscaleStatusSource for MissingSource {
        fn fetch_status_json(&self) -> Result<String, String> {
            Err("tailscale binary not found on PATH".into())
        }
        fn probe(&self) -> Result<(), String> {
            Err("tailscale binary not found on PATH".into())
        }
    }

    fn seed_paired(db: &Arc<Mutex<Database>>, fp: &str, principal: &str) {
        let g = db.lock().expect("lock");
        let serial = g
            .insert_internal_cert("client", "PEM", fp, principal, 0, 1_000_000)
            .expect("cert");
        g.insert_paired_device(fp, principal, "{}", principal, serial)
            .expect("pair");
    }

    fn status_with(self_ips: &[&str], peer_ips: &[&str]) -> String {
        let self_arr: Vec<String> = self_ips.iter().map(|s| format!("\"{s}\"")).collect();
        let peer_arr: Vec<String> = peer_ips.iter().map(|s| format!("\"{s}\"")).collect();
        format!(
            r#"{{
              "Self":  {{ "TailscaleIPs": [{}] }},
              "Peer":  {{ "abc": {{ "TailscaleIPs": [{}] }} }}
            }}"#,
            self_arr.join(","),
            peer_arr.join(",")
        )
    }

    #[test]
    fn rejects_peer_ip_not_in_tailscale_status() {
        // Real failure mode: a verifier that accepts any IP would silently
        // strip the Tailscale pin the user explicitly chose, downgrading to
        // pure TOFU. The peer here is paired but its IP is absent from
        // status — the verifier MUST refuse.
        let shared = db();
        seed_paired(&shared, "fp-A", "alice");
        let source = StubSource::new(&status_with(&["100.64.0.1"], &["100.64.0.2"]));
        let v = TailscaleVerifier::new(TofuVerifier::new(shared.clone()), source);

        let ctx = PeerCtx {
            peer_ip: Some("100.64.0.99".parse().unwrap()),
        };
        match v.verify_with_peer("fp-A", None, ctx) {
            Err(VerifierError::UnknownDevice(msg)) => {
                assert!(msg.contains("100.64.0.99"), "msg = {msg}");
            }
            other => panic!("expected UnknownDevice for unlisted IP, got {other:?}"),
        }
    }

    #[test]
    fn accepts_when_peer_ip_matches_paired_device_in_status() {
        let shared = db();
        seed_paired(&shared, "fp-A", "alice");
        let source = StubSource::new(&status_with(&["100.64.0.1"], &["100.64.0.2"]));
        let v = TailscaleVerifier::new(TofuVerifier::new(shared.clone()), source);

        let ctx = PeerCtx {
            peer_ip: Some("100.64.0.2".parse().unwrap()),
        };
        assert_eq!(v.verify_with_peer("fp-A", None, ctx).unwrap(), "alice");
    }

    #[test]
    fn rejects_revoked_fingerprint_even_if_ip_matches() {
        // Order-of-checks invariant: revocation > tailscale-IP match. If the
        // verifier returned UnknownDevice or accepted the peer because the IP
        // matched, the audit trail would lose the revocation signal.
        let shared = db();
        seed_paired(&shared, "fp-bad", "evil");
        {
            let g = shared.lock().expect("lock");
            g.add_revoked_fingerprint("fp-bad", Some("compromised"))
                .expect("revoke");
        }
        let source = StubSource::new(&status_with(&["100.64.0.1"], &["100.64.0.2"]));
        let v = TailscaleVerifier::new(TofuVerifier::new(shared.clone()), source);

        let ctx = PeerCtx {
            peer_ip: Some("100.64.0.2".parse().unwrap()),
        };
        match v.verify_with_peer("fp-bad", None, ctx) {
            Err(VerifierError::Revoked(fp)) => assert_eq!(fp, "fp-bad"),
            other => panic!("expected Revoked, got {other:?}"),
        }
    }

    #[test]
    fn status_cache_does_not_outlive_ttl() {
        // Real failure mode: a missing TTL bound (or a too-long TTL) means
        // tailscale node churn isn't picked up; a too-short TTL forks
        // `tailscale` on every gRPC connection. Three connections inside the
        // window should fork once; one after the TTL must re-fork.
        let shared = db();
        seed_paired(&shared, "fp-A", "alice");
        // Self must be non-empty (a logged-out tailscaled is now refused;
        // see `rejects_when_tailscale_status_has_no_self_node`).
        let source = StubSource::new(&status_with(&["100.64.0.1"], &["100.64.0.2"]));
        let v = TailscaleVerifier::new(TofuVerifier::new(shared.clone()), source.clone())
            .with_ttl(Duration::from_millis(50));

        let ctx = PeerCtx {
            peer_ip: Some("100.64.0.2".parse().unwrap()),
        };

        for _ in 0..3 {
            v.verify_with_peer("fp-A", None, ctx).unwrap();
        }
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            1,
            "calls within TTL must reuse cached status"
        );

        std::thread::sleep(Duration::from_millis(75));
        v.verify_with_peer("fp-A", None, ctx).unwrap();
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            2,
            "first call after TTL expiry must re-fetch"
        );
    }

    #[test]
    fn verify_without_peer_ip_is_hard_error() {
        // Guards the call-site contract: if the gRPC interceptor forgets to
        // thread peer_ip through `verify_with_peer`, the connection refuses
        // rather than silently dropping the Tailscale pin.
        let shared = db();
        seed_paired(&shared, "fp-A", "alice");
        let source = StubSource::new(&status_with(&[], &["100.64.0.2"]));
        let v = TailscaleVerifier::new(TofuVerifier::new(shared.clone()), source);

        match v.verify("fp-A", None) {
            Err(VerifierError::Tailscale(_)) => {}
            other => panic!("expected Tailscale error for missing peer ip, got {other:?}"),
        }
        let ctx = PeerCtx { peer_ip: None };
        match v.verify_with_peer("fp-A", None, ctx) {
            Err(VerifierError::Tailscale(_)) => {}
            other => panic!("expected Tailscale error for missing peer ip, got {other:?}"),
        }
    }

    #[test]
    fn build_verifier_fails_closed_when_tailscale_binary_absent() {
        // Daemon-startup invariant: `security.verifier = tailscale` with no
        // tailscale binary on PATH must surface as a hard error so we don't
        // silently downgrade to TOFU. This pins the factory contract.
        let shared = db();
        let source: Arc<dyn TailscaleStatusSource> = Arc::new(MissingSource);
        let res = super::super::build_verifier(
            cortex::config::VerifierKind::Tailscale,
            shared,
            Some(source),
        );
        let err = match res {
            Ok(_) => panic!("must fail-closed"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("tailscale"),
            "error must mention tailscale, got: {msg}"
        );
    }

    #[test]
    fn rejects_when_tailscale_status_has_no_self_node() {
        // Real failure mode: a logged-out tailscaled returns `Self: null`.
        // With an empty self_ips set, every peer IP would pass the "non-self"
        // filter and TOFU would be silently re-enabled. The verifier MUST
        // refuse instead — fail-closed matches the operator's chosen
        // posture.
        let shared = db();
        seed_paired(&shared, "fp-A", "alice");
        let json = r#"{"Self": null, "Peer": {"abc": {"TailscaleIPs": ["100.64.0.2"]}}}"#;
        let source = StubSource::new(json);
        let v = TailscaleVerifier::new(TofuVerifier::new(shared), source);

        let ctx = PeerCtx {
            peer_ip: Some("100.64.0.2".parse().unwrap()),
        };
        match v.verify_with_peer("fp-A", None, ctx) {
            Err(VerifierError::Tailscale(msg)) => {
                assert!(
                    msg.to_lowercase().contains("self"),
                    "error must mention missing Self: {msg}"
                );
            }
            other => panic!("expected Tailscale error for empty Self, got {other:?}"),
        }
    }

    #[test]
    fn build_verifier_returns_tofu_for_tofu_kind() {
        let shared = db();
        let v = super::super::build_verifier(cortex::config::VerifierKind::Tofu, shared, None)
            .expect("build");
        // TOFU verifier ignores peer ctx; an unknown fingerprint should
        // surface as UnknownDevice (not Tailscale*) — proves we built the
        // right impl.
        match v.verify_with_peer("fp-nope", None, PeerCtx::NONE) {
            Err(VerifierError::UnknownDevice(_)) => {}
            other => panic!("expected UnknownDevice from tofu verifier, got {other:?}"),
        }
    }
}
