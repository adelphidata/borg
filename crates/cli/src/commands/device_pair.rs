//! `borg pair` — device pairing CLI surface.
//!
//! Two modes:
//! - `borg pair show` — runs on the *daemon host*; calls `Pki.StartPairing`
//!   over the local UDS to mint a code and prints it.
//! - `borg pair <code> [--label X] [--socket /path/to/borgd-pair.sock]` —
//!   runs on the *client device*; drives the SPAKE2 exchange over the
//!   bootstrap UDS and persists the issued cert to the OS keychain.

use anyhow::{Context, Result};
use borg_pki::{KeychainStore, KekStore};
use borgd::pairing::device::client as pair_client;
use std::path::PathBuf;

use crate::daemon_client::connect;

/// Keychain accounts where the client stashes its cert + key after pairing.
pub const CLIENT_CERT_KEY_ACCOUNT: &str = "client-cert-pem";
pub const CLIENT_KEY_KEY_ACCOUNT: &str = "client-key-pem";
pub const CA_CERT_ACCOUNT: &str = "client-ca-pem";

pub async fn run_show() -> Result<()> {
    let mut client = connect(None).await.context("connect to daemon")?;
    let resp = client.start_pairing("operator").await.context("start pairing")?;
    let exp = chrono::DateTime::from_timestamp(resp.expires_at, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| resp.expires_at.to_string());
    println!("Pairing code: {}", resp.code);
    println!("Expires at:   {} (5 minutes)", exp);
    println!();
    println!("On the client device, run:");
    println!("  borg pair {}", resp.code);
    Ok(())
}

pub async fn run_complete(code: &str, device_label: &str, socket: Option<PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(default_pairing_socket_path);
    println!("Pairing with daemon at {} ...", socket_path.display());
    let result = pair_client::run(&socket_path, code, device_label)
        .await
        .context("pairing handshake")?;

    let kek = KeychainStore::new();
    kek.put(CA_CERT_ACCOUNT, result.ca_cert_pem.as_bytes())
        .context("stash CA cert in keychain")?;
    kek.put(CLIENT_CERT_KEY_ACCOUNT, result.client_cert_pem.as_bytes())
        .context("stash client cert in keychain")?;
    kek.put(CLIENT_KEY_KEY_ACCOUNT, result.client_key_pem.as_bytes())
        .context("stash client key in keychain")?;

    let exp = chrono::DateTime::from_timestamp(result.not_after, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| result.not_after.to_string());
    println!("Pairing complete.");
    println!("Cert valid until {}", exp);
    Ok(())
}

fn default_pairing_socket_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".borg").join("borgd-pair.sock")
}
