//! `borg revoke <fingerprint>` — revoke a paired device.

use anyhow::{Context, Result};

use crate::daemon_client::connect;

pub async fn run(fingerprint: &str, reason: Option<&str>) -> Result<()> {
    let mut client = connect(None).await.context("connect to daemon")?;
    client
        .revoke_device(fingerprint, reason.unwrap_or("manual revoke"))
        .await
        .context("revoke device")?;
    println!("Revoked device {fingerprint}");
    Ok(())
}

pub async fn run_list() -> Result<()> {
    let mut client = connect(None).await.context("connect to daemon")?;
    let list = client.list_paired_devices().await.context("list devices")?;
    if list.devices.is_empty() {
        println!("No paired devices.");
        return Ok(());
    }
    println!(
        "{:<12} {:<30} {:<10} {:<20} FINGERPRINT",
        "ID", "DEVICE", "STATUS", "LAST SEEN"
    );
    for d in list.devices {
        let last = if d.last_seen_at == 0 {
            "(never)".to_string()
        } else {
            chrono::DateTime::from_timestamp(d.last_seen_at, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| d.last_seen_at.to_string())
        };
        let fp_short = if d.fingerprint.len() > 16 {
            format!("{}…", &d.fingerprint[..16])
        } else {
            d.fingerprint.clone()
        };
        println!(
            "{:<12} {:<30} {:<10} {:<20} {}",
            d.id, d.device_label, d.status, last, fp_short
        );
    }
    Ok(())
}
