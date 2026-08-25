//! Tailscale tunnel via `tailscale serve` (tailnet) or `tailscale funnel`
//! (public HTTPS). Both are fire-and-forget — the `--bg` flag tells
//! tailscale to install the config and exit. No long-running child
//! process to supervise; teardown is `tailscale {serve|funnel} off`.

use anyhow::{anyhow, bail, Result};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::tunnel::{
    kill_shared, new_shared_process, new_shared_url, SharedProcess, SharedUrl, Tunnel, TunnelError,
};

const TAILSCALE_INSTALL_URL: &str = "https://tailscale.com/download";
/// `tailscale serve` can block on tailnet re-auth, so it gets the longest
/// budget; status/health are local IPC and must fail fast to stay out of the
/// startup path.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const SERVE_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Tailscale-managed tunnel. With `funnel = true`, exposes the local port
/// publicly via Tailscale's edge; otherwise serves only to authenticated
/// tailnet peers.
///
/// Requires `tailscale up` to have completed at least once on this host.
pub struct TailscaleTunnel {
    funnel: bool,
    hostname: Option<String>,
    proc: SharedProcess,
    url: SharedUrl,
}

impl TailscaleTunnel {
    pub fn new(funnel: bool, hostname: Option<String>) -> Self {
        Self {
            funnel,
            hostname,
            proc: new_shared_process(),
            url: new_shared_url(),
        }
    }

    fn subcommand(&self) -> &'static str {
        if self.funnel {
            "funnel"
        } else {
            "serve"
        }
    }
}

/// Pure helper: extract `Self.DNSName` from a `tailscale status --json`
/// payload, stripping the trailing FQDN dot. Returns an error rather than
/// silently falling back to `localhost` when the field is missing — that
/// would produce a URL like `https://localhost` and the caller wouldn't
/// know discovery failed.
pub fn parse_tailscale_self_dns_name(json: &[u8]) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| anyhow!("parse tailscale status JSON: {e}"))?;
    let dns = value["Self"]["DNSName"]
        .as_str()
        .ok_or_else(|| anyhow!("tailscale status JSON missing Self.DNSName field"))?;
    Ok(dns.trim_end_matches('.').to_string())
}

#[async_trait::async_trait]
impl Tunnel for TailscaleTunnel {
    fn name(&self) -> &str {
        "tailscale"
    }

    async fn start(&self, local_host: &str, local_port: u16) -> Result<String> {
        if which::which("tailscale").is_err() {
            bail!(TunnelError::BinaryMissing {
                binary: "tailscale",
                install_url: TAILSCALE_INSTALL_URL,
            });
        }

        let subcommand = self.subcommand();

        let hostname = if let Some(ref h) = self.hostname {
            h.clone()
        } else {
            let output = timeout(
                STATUS_TIMEOUT,
                Command::new("tailscale")
                    .args(["status", "--json"])
                    .output(),
            )
            .await
            .map_err(|_| anyhow!("`tailscale status --json` timed out after 10s"))??;
            if !output.status.success() {
                bail!(
                    "`tailscale status` failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            parse_tailscale_self_dns_name(&output.stdout)?
        };

        let target = format!("http://{local_host}:{local_port}");

        // `--bg` makes tailscale install the config and exit. Without it
        // the command hangs forever, never returning the URL.
        let output = timeout(
            SERVE_TIMEOUT,
            Command::new("tailscale")
                .args([subcommand, "--bg", &target])
                .output(),
        )
        .await
        .map_err(|_| anyhow!("`tailscale {subcommand} --bg {target}` timed out after 15s"))??;

        if !output.status.success() {
            bail!(
                "`tailscale {subcommand}` failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let public_url = format!("https://{hostname}");
        if let Ok(mut guard) = self.url.write() {
            *guard = Some(public_url.clone());
        }
        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        // Even if the binary disappeared after start (`brew uninstall
        // tailscale` between sessions), stopping must clear our local
        // state so a fresh start doesn't think the tunnel is still live.
        let subcommand = self.subcommand();
        if which::which("tailscale").is_ok() {
            if let Err(e) = Command::new("tailscale")
                .args([subcommand, "off"])
                .output()
                .await
            {
                tracing::warn!("`tailscale {subcommand} off` failed: {e}");
            }
        }
        if let Ok(mut guard) = self.url.write() {
            *guard = None;
        }
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        if self.url.read().ok().is_none_or(|g| g.is_none()) {
            return false;
        }
        match timeout(
            HEALTH_TIMEOUT,
            Command::new("tailscale")
                .args(["status", "--json"])
                .output(),
        )
        .await
        {
            Ok(Ok(output)) => output.status.success(),
            _ => false,
        }
    }

    fn public_url(&self) -> Option<String> {
        self.url.read().ok().and_then(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `tailscale status --json` shape (trimmed). Fields irrelevant
    /// to DNS-name extraction are omitted for readability but the schema
    /// matches what tailscale 1.x prints.
    const REAL_STATUS_JSON: &[u8] = br#"{
  "Version": "1.78.1",
  "Self": {
    "ID": "abc123",
    "HostName": "mac-mini",
    "DNSName": "mac-mini.tail-net-abc.ts.net.",
    "OS": "macOS",
    "TailscaleIPs": ["100.64.0.1"]
  },
  "Peer": {
    "xyz789": {
      "DNSName": "phone.tail-net-abc.ts.net.",
      "OS": "iOS"
    }
  }
}"#;

    #[test]
    fn parse_self_dns_name_extracts_and_strips_trailing_dot() {
        let name = parse_tailscale_self_dns_name(REAL_STATUS_JSON).unwrap();
        assert_eq!(
            name, "mac-mini.tail-net-abc.ts.net",
            "trailing FQDN dot must be stripped — leaving it produces an invalid SNI hostname"
        );
    }

    #[test]
    fn parse_self_dns_name_errors_on_missing_field() {
        // Negative case: tailscale not logged in returns `{ "BackendState": "NeedsLogin", ... }`
        // with no `Self.DNSName`. Falling back to a default would publish
        // a bogus URL; we must error so the operator sees the real fix
        // ("run `tailscale up`").
        let no_self = br#"{"Version":"1.78.1","BackendState":"NeedsLogin"}"#;
        let err = parse_tailscale_self_dns_name(no_self).unwrap_err();
        assert!(
            format!("{err}").contains("Self.DNSName"),
            "error must name the missing field: {err}"
        );
    }

    #[test]
    fn parse_self_dns_name_errors_on_invalid_json() {
        let err = parse_tailscale_self_dns_name(b"not json").unwrap_err();
        assert!(format!("{err}").contains("parse tailscale status JSON"));
    }

    #[test]
    fn subcommand_funnel_or_serve_matches_funnel_flag() {
        // Regression target: the funnel vs serve branch decides whether
        // the tunnel is publicly reachable. A drift here would
        // accidentally publish a tailnet-only tunnel to the internet
        // (or vice versa), both serious operator-surprise scenarios.
        let funnel = TailscaleTunnel::new(true, None);
        assert_eq!(funnel.subcommand(), "funnel");
        let serve = TailscaleTunnel::new(false, None);
        assert_eq!(serve.subcommand(), "serve");
    }

    #[test]
    fn constructor_stores_hostname_override() {
        let tunnel = TailscaleTunnel::new(true, Some("myhost.ts.net".into()));
        assert!(tunnel.funnel);
        assert_eq!(tunnel.hostname.as_deref(), Some("myhost.ts.net"));
    }

    #[test]
    fn public_url_none_before_start() {
        assert!(TailscaleTunnel::new(false, None).public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_start_is_idempotent() {
        let tunnel = TailscaleTunnel::new(false, None);
        assert!(tunnel.stop().await.is_ok());
        assert!(tunnel.stop().await.is_ok());
    }

    /// Full lifecycle against a fake `tailscale` on PATH: `start()` must
    /// discover the hostname from `status --json`, install the serve config,
    /// publish `https://<dnsname>`, report healthy while the config is live,
    /// and `stop()` must clear the published URL. A `stop()` that silently
    /// no-ops leaves `borg tunnel status` showing a URL that no longer routes.
    #[tokio::test]
    async fn start_discovers_hostname_health_and_stop_clears_state() {
        let _guard = crate::test_env::borg_home_guard();
        let _shim = crate::tunnel::test_support::BinShim::install(
            "tailscale",
            concat!(
                "case \"$1\" in\n",
                "status) printf '%s' '{\"Self\":{\"DNSName\":\"fake-host.tail-net.ts.net.\"}}' ;;\n",
                "*) exit 0 ;;\n",
                "esac",
            ),
        );
        let tunnel = TailscaleTunnel::new(false, None);
        let url = tunnel
            .start("127.0.0.1", 18090)
            .await
            .expect("fake tailscale must yield a URL");
        assert_eq!(
            url, "https://fake-host.tail-net.ts.net",
            "URL must come from the discovered Self.DNSName, dot stripped"
        );
        assert_eq!(
            tunnel.public_url().as_deref(),
            Some("https://fake-host.tail-net.ts.net"),
            "public_url must reflect the started tunnel"
        );
        assert!(
            tunnel.health_check().await,
            "healthy while `tailscale status` succeeds and a URL is published"
        );

        tunnel.stop().await.expect("stop succeeds");
        assert!(
            tunnel.public_url().is_none(),
            "stop must clear the published URL"
        );
        assert!(
            !tunnel.health_check().await,
            "stopped tunnel must report unhealthy"
        );
    }

    /// `health_check` must report the daemon's actual state: a `tailscale
    /// status` that exits non-zero (daemon stopped, logged out) means the
    /// tunnel is not reachable even though a URL was published.
    #[tokio::test]
    async fn health_check_false_when_status_command_fails() {
        let _guard = crate::test_env::borg_home_guard();
        {
            let _shim = crate::tunnel::test_support::BinShim::install(
                "tailscale",
                concat!(
                    "case \"$1\" in\n",
                    "status) printf '%s' '{\"Self\":{\"DNSName\":\"fake-host.tail-net.ts.net.\"}}' ;;\n",
                    "*) exit 0 ;;\n",
                    "esac",
                ),
            );
            let tunnel = TailscaleTunnel::new(false, None);
            tunnel.start("127.0.0.1", 18091).await.expect("start");

            // Swap in a shim whose `status` fails, as if tailscaled stopped.
            let _down = crate::tunnel::test_support::BinShim::install("tailscale", "exit 1");
            assert!(
                !tunnel.health_check().await,
                "failing `tailscale status` must report unhealthy"
            );
        }
    }

    #[tokio::test]
    async fn health_check_false_when_url_unset() {
        assert!(!TailscaleTunnel::new(false, None).health_check().await);
    }
}
