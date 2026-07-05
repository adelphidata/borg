//! `borg tunnel {start|stop|status}` — operator surface for the public-
//! internet tunnel that the daemon spawns under `tunnel_supervisor`.

use anyhow::Result;
use clap::Subcommand;

use protocols::admin::TunnelStatusReply;

/// `borg tunnel <subcommand>` action.
#[derive(Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunnelCliAction {
    /// Apply the current `tunnel.*` settings and bring the tunnel up.
    Start,
    /// Tear the tunnel down. Idempotent.
    Stop,
    /// Print the current snapshot.
    Status,
}

pub(crate) async fn run_tunnel(action: TunnelCliAction) -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;

    let reply = match action {
        TunnelCliAction::Start => client.tunnel_start().await?,
        TunnelCliAction::Stop => {
            client.tunnel_stop().await?;
            // Re-fetch so the printed snapshot reflects the post-stop state.
            client.tunnel_status().await?
        }
        TunnelCliAction::Status => client.tunnel_status().await?,
    };

    print_status(&reply);
    Ok(())
}

fn print_status(s: &TunnelStatusReply) {
    println!("Tunnel");
    println!("──────");
    println!("Provider:  {}", display_provider(&s.provider));
    println!("Running:   {}", if s.running { "yes" } else { "no" });
    if !s.public_url.is_empty() {
        println!("URL:       {}", s.public_url);
    }
    if !s.error.is_empty() {
        println!("Error:     {}", s.error);
    }
    if s.provider == "none" || s.provider.is_empty() {
        println!();
        println!("No tunnel provider configured. Pick one with:");
        println!("  borg settings set tunnel.provider <cloudflare|ngrok|tailscale|custom>");
        println!("then `borg tunnel start`.");
    }
}

fn display_provider(p: &str) -> &str {
    // An empty `provider` means the daemon couldn't load config; surface it
    // explicitly rather than printing a confusing blank line.
    if p.is_empty() {
        "<unknown — daemon config load failed>"
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_provider_substitutes_helpful_string_for_empty_provider() {
        // A blank line in CLI output is operator-hostile. An empty
        // provider field on the reply means the daemon's config load
        // failed; the printout must call that out, not hide it.
        assert!(display_provider("").contains("unknown"));
        assert!(display_provider("").contains("config load failed"));
    }

    #[test]
    fn display_provider_passes_through_real_provider_names() {
        for p in ["cloudflare", "ngrok", "tailscale", "custom", "none"] {
            assert_eq!(display_provider(p), p);
        }
    }
}
