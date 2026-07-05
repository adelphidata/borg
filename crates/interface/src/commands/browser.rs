//! `borg browser {connect|disconnect|status}` — CLI parity for the TUI
//! `/browser` slash command. Both surfaces route through the same
//! `Admin.BrowserManage` RPC so URL parsing, reachability probing, and
//! DB persistence live in exactly one place.

use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum BrowserAction {
    /// Connect to a Chromium-family browser exposing Chrome DevTools Protocol.
    /// When `url` is omitted, defaults to `http://127.0.0.1:9222` and tries to
    /// auto-launch a debug Chromium with `--remote-debugging-port` if nothing
    /// is listening.
    Connect {
        /// CDP endpoint. Examples: `http://127.0.0.1:9222`,
        /// `ws://127.0.0.1:9222/devtools/browser/<id>`.
        url: Option<String>,
    },
    /// Clear the configured CDP URL. The browser tool reverts to launching
    /// its own headless Chromium on the next call.
    Disconnect,
    /// Show the configured CDP URL and whether it's currently reachable.
    Status,
}

pub(crate) async fn dispatch_browser(action: Option<BrowserAction>) -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;

    let action = action.unwrap_or(BrowserAction::Status);
    let (verb, url): (&str, Option<String>) = match action {
        BrowserAction::Connect { url } => ("connect", url),
        BrowserAction::Disconnect => ("disconnect", None),
        BrowserAction::Status => ("status", None),
    };

    let reply = client
        .browser_manage(verb, url.as_deref())
        .await
        .context("Admin.BrowserManage via borgd")?;

    for line in &reply.messages {
        println!("{line}");
    }
    match verb {
        "status" => {
            if reply.connected {
                println!("🌐 Browser: connected to {}", reply.url);
            } else {
                println!("🌐 Browser: not connected (default headless mode)");
            }
        }
        "connect" => {
            if reply.connected {
                println!("✓ Connected: {}", reply.url);
            } else {
                println!("⚠ Not connected (see messages above)");
                std::process::exit(1);
            }
        }
        "disconnect" => {
            println!("🌐 Browser reverted to default headless mode");
        }
        _ => unreachable!(),
    }
    Ok(())
}
