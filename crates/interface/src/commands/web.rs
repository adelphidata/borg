//! `borg web` — open the browser UI already signed in.
//!
//! The browser surface authenticates with a client token, and until now the
//! only way to get one was `borg client-token issue`, copy, paste. This
//! command does the three steps: it issues a token, builds the `/ui/` URL
//! with the token in the fragment, and opens it. The fragment never reaches
//! the gateway (browsers do not send it), so the token does not land in any
//! access log; the page stores it and strips it from the address bar.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// The single client token `borg web` owns. Re-issued on every run.
const WEB_TOKEN_LABEL: &str = "web";

/// The sign-in URL for the web UI served by the gateway at `host:port`.
///
/// Only loopback hosts are accepted: the URL carries a live token, and a
/// gateway reachable from elsewhere is not the daemon on this machine.
pub(crate) fn sign_in_url(host: &str, port: u16, token: &str) -> Result<String> {
    let host = host.trim();
    let loopback = matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "[::1]" | "0.0.0.0"
    );
    if !loopback {
        bail!(
            "gateway.host is `{host}`, not this machine. `borg web` only opens a local gateway; \
             issue a token with `borg client-token issue web` and paste it into the UI instead."
        );
    }
    // `0.0.0.0` means "every interface", which for the opener is this
    // machine; a bare IPv6 literal needs brackets before a `:port`.
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::1" => "[::1]",
        other => other,
    };
    // Hash router: `#/` is the app's root route and `?token=` its query, so
    // the whole thing stays client-side.
    Ok(format!("http://{host}:{port}/ui/#/?token={token}"))
}

/// Hand `url` to the desktop's default browser. Best effort by nature; the
/// caller has already printed the URL as the fallback.
fn open_in_browser(url: &str) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    }
    .context("launching the default browser")?;
    if !status.success() {
        bail!("the browser launcher exited with {status}");
    }
    Ok(())
}

/// `borg web [--no-open]`.
pub(crate) async fn run_web(no_open: bool) -> Result<()> {
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    let config = cortex::config::Config::load_from_db()
        .await
        .context("loading gateway settings")?;
    // One label, reused. Labels are unique, so a fresh token needs the old
    // one gone first — that also keeps `borg web` from leaving a trail of
    // live full-access credentials behind, one per run.
    let label = WEB_TOKEN_LABEL;
    if let Err(e) = client.revoke_client_token(label).await {
        tracing::debug!("no earlier `{label}` token to revoke: {e}");
    }
    let issued = client
        .issue_client_token(label, "")
        .await
        .context("issuing a client token for the web UI")?;
    let url = sign_in_url(&config.gateway.host, config.gateway.port, &issued.plaintext)?;

    if no_open {
        println!("{url}");
        return Ok(());
    }
    match open_in_browser(&url) {
        Ok(()) => println!("Opened the Borg web UI in your browser (token `{label}`)."),
        Err(e) => {
            tracing::warn!("could not open the browser: {e}");
            println!("Could not open a browser ({e}). Open this yourself:");
            println!();
            println!("    {url}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rides_in_the_fragment_never_the_query() {
        // A `?token=` before the `#` would be sent to the gateway and logged.
        let url = sign_in_url("127.0.0.1", 7842, "tok_live").expect("loopback");
        assert_eq!(url, "http://127.0.0.1:7842/ui/#/?token=tok_live");
        let (before_hash, _) = url.split_once('#').expect("has a fragment");
        assert!(
            !before_hash.contains("token"),
            "token leaked into the request URL: {url}"
        );
    }

    #[test]
    fn only_a_local_gateway_is_opened() {
        assert_eq!(
            sign_in_url("localhost", 1, "t").expect("loopback name"),
            "http://localhost:1/ui/#/?token=t"
        );
        // A bare `::1:1` is not a URL any browser can parse.
        assert_eq!(
            sign_in_url("::1", 1, "t").expect("ipv6 loopback"),
            "http://[::1]:1/ui/#/?token=t"
        );
        assert_eq!(
            sign_in_url("[::1]", 1, "t").expect("already bracketed"),
            "http://[::1]:1/ui/#/?token=t"
        );
        // Bound on every interface still means "this machine" for the opener.
        assert_eq!(
            sign_in_url("0.0.0.0", 7842, "t").expect("all interfaces"),
            "http://127.0.0.1:7842/ui/#/?token=t"
        );
        let err = sign_in_url("borg.example.com", 7842, "t").expect_err("remote host");
        assert!(err.to_string().contains("client-token issue web"), "{err}");
    }
}
