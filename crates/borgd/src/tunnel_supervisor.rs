//! Daemon-side owner of the optional public-internet tunnel.
//!
//! Holds the active `Box<dyn Tunnel>` behind an async lock so the admin
//! gRPC handlers (`TunnelStart`/`TunnelStop`/`TunnelStatus`) can drive it
//! out-of-band while the daemon's normal startup sequence kicks off an
//! initial start when `tunnel.provider != none`.
//!
//! Why a custom state holder instead of `spawn_supervised`: the underlying
//! tunnel providers each spawn their own external child process (or are
//! fire-and-forget like `tailscale --bg`) and own pipe-draining themselves.
//! What we supervise here is the *operator's intent* — "tunnel should be
//! running" — and re-issue `start()` if the daemon restarts. There's no
//! long-running Rust task to wrap.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use cortex::config::TunnelConfig;
use gateway::tunnel::{create_tunnel, Tunnel};

/// Current tunnel snapshot exposed via `TunnelStatus`.
#[derive(Debug, Clone)]
pub struct TunnelSnapshot {
    /// Provider name from `tunnel.provider` (`"none"` when unconfigured).
    pub provider: String,
    /// True iff a tunnel handle is held and its public URL is set.
    pub running: bool,
    /// Public URL when the tunnel has surfaced one. Empty otherwise.
    pub public_url: String,
    /// Sticky error from the most recent start attempt. Cleared on success.
    pub last_error: String,
}

impl TunnelSnapshot {
    fn unconfigured() -> Self {
        Self {
            provider: "none".to_string(),
            running: false,
            public_url: String::new(),
            last_error: String::new(),
        }
    }
}

/// Live tunnel state. Cloneable across handlers via `Arc`.
#[derive(Clone)]
pub struct TunnelSupervisor {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Local gateway address — supplied at construction so admin RPCs
    /// don't have to thread it through every call.
    local_host: String,
    local_port: u16,
    /// Active tunnel handle. `None` when stopped.
    handle: Option<Box<dyn Tunnel>>,
    /// Most recent snapshot — refreshed on every state change so
    /// `TunnelStatus` is a cheap read.
    snapshot: TunnelSnapshot,
}

impl TunnelSupervisor {
    /// Construct a supervisor bound to the gateway's local origin.
    pub fn new(local_host: impl Into<String>, local_port: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                local_host: local_host.into(),
                local_port,
                handle: None,
                snapshot: TunnelSnapshot::unconfigured(),
            })),
        }
    }

    /// Apply the resolved tunnel config. Starts the tunnel if the provider
    /// is anything other than `none`. Idempotent — re-applying the same
    /// config while a tunnel is already running is a no-op.
    ///
    /// Returns the post-apply snapshot. Errors are *also* recorded into
    /// the snapshot's `last_error`, so a UI that only reads `status()`
    /// still sees what went wrong.
    pub async fn apply(&self, config: &TunnelConfig) -> Result<TunnelSnapshot> {
        // Hold the lock only for cheap state inspection + handle hand-off.
        // The actual `tunnel.start(...).await` runs unlocked (cloudflared
        // can take 30s, ngrok ~15s); without this split, a concurrent
        // `TunnelStatus` RPC would block on the same Mutex for the full
        // start deadline.
        let tunnel_opt = match create_tunnel(config) {
            Ok(opt) => opt,
            Err(e) => {
                let msg = format!("{e}");
                let mut inner = self.inner.lock().await;
                inner.snapshot = TunnelSnapshot {
                    provider: config.provider.to_string(),
                    running: false,
                    public_url: String::new(),
                    last_error: msg.clone(),
                };
                return Err(anyhow!(msg));
            }
        };

        let Some(tunnel) = tunnel_opt else {
            // Provider is "none" — tear down any previously-running tunnel.
            let prev = {
                let mut inner = self.inner.lock().await;
                inner.handle.take()
            };
            if let Some(prev) = prev {
                if let Err(e) = prev.stop().await {
                    tracing::warn!("tunnel stop on provider-change failed: {e}");
                }
            }
            let mut inner = self.inner.lock().await;
            inner.snapshot = TunnelSnapshot::unconfigured();
            return Ok(inner.snapshot.clone());
        };

        // Same-provider short-circuit + take previous handle out of the
        // slot, both under one brief lock acquisition.
        let (prev, local_host, local_port) = {
            let mut inner = self.inner.lock().await;
            if let Some(existing) = inner.handle.as_ref() {
                if existing.name() == tunnel.name() && existing.public_url().is_some() {
                    inner.snapshot = TunnelSnapshot {
                        provider: existing.name().to_string(),
                        running: true,
                        public_url: existing.public_url().unwrap_or_default(),
                        last_error: String::new(),
                    };
                    return Ok(inner.snapshot.clone());
                }
            }
            (
                inner.handle.take(),
                inner.local_host.clone(),
                inner.local_port,
            )
        };

        if let Some(prev) = prev {
            if let Err(e) = prev.stop().await {
                tracing::warn!("tunnel stop before re-apply failed: {e}");
            }
        }

        // Unlocked: external subprocess spawn + line-reading deadlines.
        let start_result = tunnel.start(&local_host, local_port).await;

        // Re-acquire the lock briefly to record outcome.
        let mut inner = self.inner.lock().await;
        match start_result {
            Ok(url) => {
                let provider = tunnel.name().to_string();
                inner.handle = Some(tunnel);
                inner.snapshot = TunnelSnapshot {
                    provider,
                    running: true,
                    public_url: url,
                    last_error: String::new(),
                };
                Ok(inner.snapshot.clone())
            }
            Err(e) => {
                let msg = format!("{e}");
                inner.snapshot = TunnelSnapshot {
                    provider: tunnel.name().to_string(),
                    running: false,
                    public_url: String::new(),
                    last_error: msg.clone(),
                };
                Err(anyhow!(msg))
            }
        }
    }

    /// Stop the tunnel if running. No-op when stopped; never errors out
    /// the way `stop()` on a fresh tunnel might (idempotency invariant).
    pub async fn stop(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if let Some(t) = inner.handle.take() {
            t.stop().await?;
        }
        inner.snapshot = TunnelSnapshot {
            provider: inner.snapshot.provider.clone(),
            running: false,
            public_url: String::new(),
            last_error: String::new(),
        };
        Ok(())
    }

    /// Cheap read of the most recent snapshot. Refreshes `public_url` /
    /// `running` from the live handle in case the tunnel surfaced a URL
    /// after `start()` returned (rare, but cloudflared's named-tunnel
    /// mode can do this).
    pub async fn status(&self) -> TunnelSnapshot {
        let mut inner = self.inner.lock().await;
        if let Some(t) = inner.handle.as_ref() {
            let live_url = t.public_url();
            let alive = t.health_check().await;
            inner.snapshot.public_url = live_url.unwrap_or_default();
            inner.snapshot.running = alive && !inner.snapshot.public_url.is_empty();
        }
        inner.snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::{
        CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TailscaleTunnelConfig,
        TunnelProvider,
    };

    fn none_cfg() -> TunnelConfig {
        TunnelConfig {
            provider: TunnelProvider::None,
            cloudflare: CloudflareTunnelConfig::default(),
            ngrok: NgrokTunnelConfig::default(),
            tailscale: TailscaleTunnelConfig::default(),
            custom: CustomTunnelConfig::default(),
        }
    }

    #[tokio::test]
    async fn apply_none_provider_leaves_unconfigured_snapshot() {
        // Regression target: an early version called start() unconditionally
        // and crashed on the `none` provider (NoneTunnel::start errors loudly).
        // The supervisor must short-circuit Provider::None to a "no tunnel"
        // snapshot, not surface the would-be-loud error.
        let sup = TunnelSupervisor::new("127.0.0.1", 8080);
        let snap = sup
            .apply(&none_cfg())
            .await
            .expect("none apply must succeed");
        assert_eq!(snap.provider, "none");
        assert!(!snap.running);
        assert!(snap.public_url.is_empty());
        assert!(snap.last_error.is_empty());
    }

    #[tokio::test]
    async fn apply_cloudflare_without_token_records_error_in_snapshot() {
        // The operator needs to see *why* the tunnel didn't come up. A
        // bare `Err` from apply() with no snapshot trace would force them
        // to scrape logs; the snapshot's last_error must contain the
        // actionable settings-key string.
        let mut cfg = none_cfg();
        cfg.provider = TunnelProvider::Cloudflare;
        let sup = TunnelSupervisor::new("127.0.0.1", 8080);
        let res = sup.apply(&cfg).await;
        assert!(res.is_err(), "missing-token must propagate as Err");
        let snap = sup.status().await;
        assert_eq!(snap.provider, "cloudflare");
        assert!(
            snap.last_error.contains("tunnel.cloudflare.token"),
            "snapshot must surface the actionable setting key, got: {}",
            snap.last_error
        );
    }

    #[tokio::test]
    async fn stop_when_never_started_is_idempotent_and_clears_state() {
        // Real failure mode: `stop()` racing with `apply()` on a fresh
        // supervisor must not double-take the handle (panic on .take().take()).
        let sup = TunnelSupervisor::new("127.0.0.1", 8080);
        sup.stop().await.expect("first stop must be Ok");
        sup.stop().await.expect("second stop must be Ok");
        let snap = sup.status().await;
        assert!(!snap.running);
        assert!(snap.public_url.is_empty());
    }
}
