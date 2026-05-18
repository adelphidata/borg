//! Tunnel abstraction for exposing the borg gateway to the public internet.
//!
//! Wraps external tunnel binaries (`cloudflared`, `ngrok`, `tailscale`)
//! behind a common trait. The gateway starts a tunnel after binding its
//! local port and stops it on shutdown.
//!
//! All providers assume their binary is already on PATH. Missing binaries
//! surface as an actionable [`TunnelError::BinaryMissing`] with the
//! install URL — never a generic "No such file or directory" from execvp.
//!
//! Supported providers:
//! - **cloudflare** — Zero Trust tunnels via `cloudflared`
//! - **tailscale**  — `tailscale serve` (tailnet) or `tailscale funnel` (public)
//! - **ngrok**      — instant public URLs via `ngrok`
//! - **custom**     — any command with `{host}`/`{port}` placeholders
//! - **none**       — local-only, no external exposure

mod cloudflare;
mod custom;
mod ngrok;
mod none;
mod tailscale;

pub use cloudflare::{build_cloudflared_args, parse_cloudflared_url_from_line, CloudflareTunnel};
pub use custom::CustomTunnel;
pub use ngrok::{build_ngrok_args, parse_ngrok_url_from_line, NgrokTunnel};
pub use none::NoneTunnel;
pub use tailscale::{parse_tailscale_self_dns_name, TailscaleTunnel};

use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

use cortex::config::{TunnelConfig, TunnelProvider};

/// Lock-free URL storage. Uses `std::sync::RwLock` so `public_url()` (sync)
/// never returns a spurious `None` due to async lock contention.
pub(crate) type SharedUrl = Arc<std::sync::RwLock<Option<String>>>;

pub(crate) fn new_shared_url() -> SharedUrl {
    Arc::new(std::sync::RwLock::new(None))
}

/// Provider-agnostic tunnel with lifecycle management.
///
/// Implementations wrap an external tunnel binary. Borg's gateway calls
/// `start()` after binding its local port and `stop()` on shutdown. The
/// supervisor (`borgd::tunnel_supervisor`) wraps the lifecycle with the
/// standard `spawn_supervised` panic isolation + restart back-off.
#[async_trait::async_trait]
pub trait Tunnel: Send + Sync {
    /// Human-readable provider name (e.g. `"cloudflare"`).
    fn name(&self) -> &str;

    /// Start the tunnel exposing `local_host:local_port` externally.
    /// Returns the public URL on success.
    async fn start(&self, local_host: &str, local_port: u16) -> Result<String>;

    /// Stop the tunnel process gracefully.
    async fn stop(&self) -> Result<()>;

    /// Whether the tunnel process is still alive.
    async fn health_check(&self) -> bool;

    /// Return the public URL if the tunnel is running, `None` otherwise.
    fn public_url(&self) -> Option<String>;
}

/// Shared child-process handle.
///
/// The `_pipe_drain` task MUST stay alive for the process's lifetime — if
/// it drops, the OS pipe closes and the child gets SIGPIPE on its next
/// write; if the task doesn't read, the pipe buffer (~64 KiB) fills and
/// the child blocks indefinitely. Both failure modes look like "the
/// tunnel works for a minute then dies" in the wild.
pub(crate) struct TunnelProcess {
    pub child: tokio::process::Child,
    pub _pipe_drain: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) type SharedProcess = Arc<Mutex<Option<TunnelProcess>>>;

pub(crate) fn new_shared_process() -> SharedProcess {
    Arc::new(Mutex::new(None))
}

/// Kill a shared tunnel process if running. Idempotent — calling on an
/// empty slot is a no-op (`stop_without_start_is_ok` invariant).
pub(crate) async fn kill_shared(proc: &SharedProcess) -> Result<()> {
    let mut guard = proc.lock().await;
    if let Some(ref mut tp) = *guard {
        if let Err(e) = tp.child.kill().await {
            tracing::debug!("tunnel child.kill: {e}");
        }
        if let Err(e) = tp.child.wait().await {
            tracing::debug!("tunnel child.wait: {e}");
        }
    }
    *guard = None;
    Ok(())
}

/// Stable, install-URL-bearing errors for the tunnel surface.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// The required tunnel binary isn't on PATH.
    #[error("`{binary}` not found on PATH. Install: {install_url}")]
    BinaryMissing {
        binary: &'static str,
        install_url: &'static str,
    },
    /// The provider was selected but its required credential is missing.
    #[error("tunnel provider `{provider}` selected but no `{field}` configured. Set it via `borg settings set {setting_key} <value>`.")]
    MissingCredential {
        provider: &'static str,
        field: &'static str,
        setting_key: &'static str,
    },
}

/// Create a tunnel from the resolved config. Returns `None` for provider
/// `None` so callers can short-circuit cleanly.
///
/// Validation is up-front: missing credentials surface as
/// [`TunnelError::MissingCredential`] with the exact `borg settings set`
/// invocation that fixes them. Missing binaries are checked lazily on
/// `start()` so a misconfigured provider never blocks daemon startup.
pub fn create_tunnel(config: &TunnelConfig) -> Result<Option<Box<dyn Tunnel>>> {
    match config.provider {
        TunnelProvider::None => Ok(None),

        TunnelProvider::Cloudflare => {
            if config.cloudflare.token.is_empty() {
                bail!(TunnelError::MissingCredential {
                    provider: "cloudflare",
                    field: "token",
                    setting_key: "tunnel.cloudflare.token",
                });
            }
            Ok(Some(Box::new(CloudflareTunnel::new(
                config.cloudflare.token.clone(),
            ))))
        }

        TunnelProvider::Ngrok => {
            if config.ngrok.auth_token.is_empty() {
                bail!(TunnelError::MissingCredential {
                    provider: "ngrok",
                    field: "auth_token",
                    setting_key: "tunnel.ngrok.auth_token",
                });
            }
            Ok(Some(Box::new(NgrokTunnel::new(
                config.ngrok.auth_token.clone(),
                config.ngrok.domain.clone(),
            ))))
        }

        TunnelProvider::Tailscale => Ok(Some(Box::new(TailscaleTunnel::new(
            config.tailscale.funnel,
            config.tailscale.hostname.clone(),
        )))),

        TunnelProvider::Custom => {
            if config.custom.start_command.is_empty() {
                bail!(TunnelError::MissingCredential {
                    provider: "custom",
                    field: "start_command",
                    setting_key: "tunnel.custom.start_command",
                });
            }
            Ok(Some(Box::new(CustomTunnel::new(
                config.custom.start_command.clone(),
                config.custom.health_url.clone(),
                config.custom.url_pattern.clone(),
            )?)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::{
        CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TailscaleTunnelConfig,
    };

    fn cfg(provider: TunnelProvider) -> TunnelConfig {
        TunnelConfig {
            provider,
            cloudflare: CloudflareTunnelConfig::default(),
            ngrok: NgrokTunnelConfig::default(),
            tailscale: TailscaleTunnelConfig::default(),
            custom: CustomTunnelConfig::default(),
        }
    }

    #[test]
    fn create_tunnel_none_returns_no_tunnel() {
        let result =
            create_tunnel(&cfg(TunnelProvider::None)).expect("None provider must not error");
        assert!(
            result.is_none(),
            "TunnelProvider::None must short-circuit to Ok(None) — bailing instead would block daemon startup whenever the user hadn't picked a provider"
        );
    }

    #[test]
    fn create_tunnel_cloudflare_without_token_returns_actionable_error() {
        let err = create_tunnel(&cfg(TunnelProvider::Cloudflare))
            .err()
            .expect("missing token must produce an error");
        let msg = format!("{err}");
        // Must name the provider, the missing field, AND the exact
        // settings key. A vague "token missing" lets users grep source
        // for the right key — operator-hostile.
        assert!(msg.contains("cloudflare"), "{msg}");
        assert!(msg.contains("token"), "{msg}");
        assert!(msg.contains("tunnel.cloudflare.token"), "{msg}");
        assert!(msg.contains("borg settings set"), "{msg}");
    }

    #[test]
    fn create_tunnel_ngrok_without_token_returns_actionable_error() {
        let err = create_tunnel(&cfg(TunnelProvider::Ngrok))
            .err()
            .expect("missing token must produce an error");
        let msg = format!("{err}");
        assert!(msg.contains("ngrok"), "{msg}");
        assert!(msg.contains("auth_token"), "{msg}");
        assert!(msg.contains("tunnel.ngrok.auth_token"), "{msg}");
    }

    #[test]
    fn create_tunnel_custom_without_start_command_returns_actionable_error() {
        let err = create_tunnel(&cfg(TunnelProvider::Custom))
            .err()
            .expect("missing start_command must produce an error");
        let msg = format!("{err}");
        assert!(msg.contains("custom"), "{msg}");
        assert!(msg.contains("start_command"), "{msg}");
    }

    #[test]
    fn create_tunnel_tailscale_has_no_required_credential() {
        // tailscale leans on the host's `tailscale` binary state — no
        // credential lives in borg config. So Tailscale must construct
        // even with the all-defaults config; the binary-missing error
        // surfaces at start() time. Regression target: if someone adds
        // a tailscale.auth_key field and gates create_tunnel on it,
        // existing `tunnel.provider=tailscale` installs break silently.
        let tunnel = create_tunnel(&cfg(TunnelProvider::Tailscale))
            .expect("tailscale construction must not error on default config")
            .expect("tailscale provider must produce a tunnel");
        assert_eq!(tunnel.name(), "tailscale");
    }

    #[test]
    fn create_tunnel_cloudflare_with_token_yields_cloudflare_provider() {
        let mut c = cfg(TunnelProvider::Cloudflare);
        c.cloudflare.token = "cf-test-token".into();
        let tunnel = create_tunnel(&c)
            .expect("cloudflare with token must not error")
            .expect("cloudflare provider must produce a tunnel");
        assert_eq!(tunnel.name(), "cloudflare");
        // Public URL must be None until start() runs.
        assert!(tunnel.public_url().is_none());
    }
}
